use super::{CodeWorkspace, registry};
use crate::Workspace;
use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use gpui::{Context, Entity, Task, Window};
use project::{Worktree, vscode_workspace::VsCodeWorkspace};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use util::ResultExt as _;

pub(crate) fn is_workspace_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("code-workspace"))
}

pub(super) async fn read_workspace(path: PathBuf, fs: &dyn fs::Fs) -> Result<VsCodeWorkspace> {
    let content = fs
        .load(&path)
        .await
        .with_context(|| format!("Failed to read workspace {}", path.display()))?;
    let mut workspace = VsCodeWorkspace::parse(path, &content)?;
    for folder in &mut workspace.folders {
        if let Some(metadata) = fs.metadata(&folder.path).await? {
            anyhow::ensure!(
                metadata.is_dir,
                "Workspace folder is not a directory: {}",
                folder.path.display()
            );
            folder.path = fs.canonicalize(&folder.path).await?;
        }
    }
    let mut paths = collections::HashSet::default();
    workspace
        .folders
        .retain(|folder| paths.insert(folder.path.clone()));
    Ok(workspace)
}

/// Replaces a `.code-workspace` path with the folders it lists, returning the
/// parsed workspace file alongside the paths to open.
pub(crate) async fn expand_workspace_paths(
    paths: Vec<PathBuf>,
    fs: &dyn fs::Fs,
) -> Result<(Vec<PathBuf>, Option<VsCodeWorkspace>)> {
    let mut configuration = None;
    let mut expanded_paths = Vec::with_capacity(paths.len());
    for path in paths {
        if !is_workspace_file(&path) {
            expanded_paths.push(path);
            continue;
        }
        anyhow::ensure!(
            configuration.is_none(),
            "Open one .code-workspace file at a time"
        );
        let path = fs
            .canonicalize(&path)
            .await
            .with_context(|| format!("Failed to read workspace {}", path.display()))?;
        let workspace = read_workspace(path, fs).await?;
        for folder in &workspace.folders {
            if fs.is_dir(&folder.path).await {
                expanded_paths.push(folder.path.clone());
            }
        }
        configuration = Some(workspace);
    }
    Ok((expanded_paths, configuration))
}

/// Compares folder lists as sets, resolving each path the way workspace files do.
pub(super) async fn same_folders(left: &[PathBuf], right: &[PathBuf], fs: &dyn fs::Fs) -> bool {
    async fn resolve(paths: &[PathBuf], fs: &dyn fs::Fs) -> collections::HashSet<PathBuf> {
        let mut resolved = collections::HashSet::default();
        for path in paths {
            resolved.insert(fs.canonicalize(path).await.unwrap_or_else(|_| path.clone()));
        }
        resolved
    }
    resolve(left, fs).await == resolve(right, fs).await
}

impl Workspace {
    /// Makes this window follow the given workspace file: its folders become the
    /// project's folders and its settings, tasks and launch configurations apply.
    pub(crate) fn attach_code_workspace(
        &mut self,
        configuration: VsCodeWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let path = configuration.path.clone();
        if self.code_workspace_path() != Some(path.as_path()) {
            let watcher = self.watch_code_workspace(path.clone(), window, cx);
            self.code_workspace = Some(CodeWorkspace {
                path,
                folders: Vec::new(),
                applying: 0,
                sync_folders: None,
                settings_update: None,
                _watcher: watcher,
            });
        }
        self.apply_code_workspace(configuration, window, cx)
    }

    /// Attaches the workspace file that was last opened with exactly this
    /// window's folders, if there is one.
    pub(crate) fn attach_matching_code_workspace(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if self.code_workspace.is_some() || !self.project.read(cx).is_local() {
            return Task::ready(Ok(()));
        }
        let folders = self
            .root_paths(cx)
            .into_iter()
            .map(|path| path.to_path_buf())
            .collect::<Vec<_>>();
        let Some(path) = registry::code_workspace_for_folders(&folders, cx) else {
            return Task::ready(Ok(()));
        };
        let fs = self.app_state.fs.clone();
        cx.spawn_in(window, async move |this, cx| {
            let configuration = match read_workspace(path.clone(), fs.as_ref()).await {
                Ok(configuration) => configuration,
                Err(error) => {
                    log::info!("Forgetting workspace {}: {error:#}", path.display());
                    this.update(cx, |_, cx| registry::forget(&path, cx))?;
                    return Ok(());
                }
            };
            let workspace_folders = configuration
                .folders
                .iter()
                .map(|folder| folder.path.clone())
                .collect::<Vec<_>>();
            if !same_folders(&workspace_folders, &folders, fs.as_ref()).await {
                return Ok(());
            }
            this.update_in(cx, |this, window, cx| {
                if this.code_workspace.is_some() {
                    return Task::ready(Ok(()));
                }
                this.attach_code_workspace(configuration, window, cx)
            })?
            .await
        })
    }

    fn watch_code_workspace(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let fs = self.app_state.fs.clone();
        cx.spawn_in(window, async move |this, cx| {
            let (events, _watcher) = fs.watch(&path, Duration::from_millis(100)).await;
            futures::pin_mut!(events);
            // The caller applies the current contents, so only later changes matter here.
            let mut previous_content = fs.load(&path).await.ok();
            while events.next().await.is_some() {
                match fs.load(&path).await {
                    Ok(content) if previous_content.as_ref() != Some(&content) => {
                        previous_content = Some(content);
                        let result = async {
                            let workspace = read_workspace(path.clone(), fs.as_ref()).await?;
                            this.update_in(cx, |this, window, cx| {
                                this.apply_code_workspace(workspace, window, cx)
                            })?
                            .await
                        }
                        .await;
                        if let Err(error) = result {
                            this.update(cx, |this, cx| {
                                this.show_error(
                                    format!(
                                        "Could not reload workspace {}: {error:#}",
                                        path.display()
                                    ),
                                    cx,
                                )
                            })
                            .log_err();
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        previous_content = None;
                        this.update(cx, |this, cx| {
                            this.project
                                .update(cx, |project, cx| project.set_vscode_workspace(None, cx));
                            this.show_error(
                                format!("Could not read workspace {}: {error:#}", path.display()),
                                cx,
                            );
                        })
                        .log_err();
                    }
                }
            }
        })
    }

    fn apply_code_workspace(
        &mut self,
        configuration: VsCodeWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if let Some(workspace) = self
            .code_workspace
            .as_mut()
            .filter(|workspace| workspace.path == configuration.path)
        {
            workspace.applying += 1;
        }
        let fs = self.app_state.fs.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let mut folders = Vec::new();
                let mut missing = Vec::new();
                for folder in &configuration.folders {
                    if !fs.is_dir(&folder.path).await {
                        missing.push(folder.path.display().to_string());
                        continue;
                    }
                    let worktree =
                        find_folder_worktree(&this, &folder.path, fs.as_ref(), cx).await?;
                    let worktree = match worktree {
                        Some(worktree) => worktree,
                        None => {
                            this.update(cx, |this, cx| {
                                this.project.update(cx, |project, cx| {
                                    project.create_worktree(&folder.path, true, cx)
                                })
                            })?
                            .await?
                        }
                    };
                    folders.push(
                        worktree.read_with(cx, |worktree, _| worktree.abs_path().to_path_buf()),
                    );
                }
                anyhow::Ok((folders, missing))
            }
            .await;

            this.update_in(cx, |this, window, cx| {
                let attached = this
                    .code_workspace
                    .as_mut()
                    .filter(|workspace| workspace.path == configuration.path);
                let Some(workspace) = attached else {
                    return result.map(|_| ());
                };
                workspace.applying = workspace.applying.saturating_sub(1);
                let (folders, missing) = result?;

                let previous_folders = std::mem::replace(&mut workspace.folders, folders.clone());
                let removed = this
                    .project
                    .read(cx)
                    .visible_worktrees(cx)
                    .filter_map(|worktree| {
                        let path = worktree.read(cx).abs_path();
                        (previous_folders
                            .iter()
                            .any(|folder| folder == path.as_ref())
                            && !folders.iter().any(|folder| folder == path.as_ref()))
                        .then_some(worktree.read(cx).id())
                    })
                    .collect::<Vec<_>>();
                if let Some(workspace) = this.code_workspace.as_mut() {
                    workspace.applying += 1;
                }
                this.project.update(cx, |project, cx| {
                    for id in removed {
                        project.remove_worktree(id, cx);
                    }
                });
                if let Some(workspace) = this.code_workspace.as_mut() {
                    workspace.applying -= 1;
                }

                registry::record(configuration.path.clone(), folders, cx);
                this.project.update(cx, |project, cx| {
                    project.set_vscode_workspace(Some(configuration), cx)
                });
                if !missing.is_empty() {
                    this.show_error(
                        format!(
                            "Workspace folders could not be found:\n{}",
                            missing.join("\n")
                        ),
                        cx,
                    );
                }
                this.serialize_workspace(window, cx);
                cx.notify();
                Ok(())
            })?
        })
    }
}

async fn find_folder_worktree(
    this: &gpui::WeakEntity<Workspace>,
    folder: &Path,
    fs: &dyn fs::Fs,
    cx: &mut gpui::AsyncWindowContext,
) -> Result<Option<Entity<Worktree>>> {
    let worktrees = this.read_with(cx, |this, cx| {
        this.project
            .read(cx)
            .worktrees(cx)
            .map(|worktree| (worktree.read(cx).abs_path().to_path_buf(), worktree))
            .collect::<Vec<_>>()
    })?;
    if let Some((_, worktree)) = worktrees.iter().find(|(path, _)| path == folder) {
        return Ok(Some(worktree.clone()));
    }
    for (path, worktree) in worktrees {
        if fs.canonicalize(&path).await.ok().as_deref() == Some(folder) {
            return Ok(Some(worktree));
        }
    }
    Ok(None)
}
