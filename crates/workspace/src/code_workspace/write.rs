use super::{open::read_workspace, registry};
use crate::Workspace;
use anyhow::{Context as _, Result};
use gpui::{App, AsyncWindowContext, Context, Task, WeakEntity, Window};
use project::{DirectoryLister, vscode_workspace};
use settings::SettingsContent;
use std::{path::PathBuf, time::Duration};
use util::ResultExt as _;

const CODE_WORKSPACE_EXTENSION: &str = "code-workspace";

impl Workspace {
    /// Asks where to save a workspace file for this window's folders, writes it
    /// and makes the window follow it.
    pub fn create_code_workspace(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let folders = self
            .root_paths(cx)
            .into_iter()
            .map(|path| path.to_path_buf())
            .collect::<Vec<_>>();
        let Some(first_folder) = folders.first() else {
            return Task::ready(Err(anyhow::anyhow!(
                "Add a folder to the project before saving a workspace"
            )));
        };
        let suggested_name = format!(
            "{}.{CODE_WORKSPACE_EXTENSION}",
            first_folder
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "workspace".to_string())
        );
        let fs = self.app_state.fs.clone();
        let lister = DirectoryLister::Local(self.project.clone(), fs.clone());
        let new_path = self.prompt_for_new_path(lister, Some(suggested_name), window, cx);
        cx.spawn_in(window, async move |this, cx| {
            let Some(path) = new_path
                .await
                .ok()
                .flatten()
                .and_then(|paths| paths.into_iter().next())
            else {
                return Ok(());
            };
            let path = if super::is_workspace_file(&path) {
                path
            } else {
                let mut path = path.into_os_string();
                path.push(".");
                path.push(CODE_WORKSPACE_EXTENSION);
                PathBuf::from(path)
            };
            let contents = vscode_workspace::new_workspace_file_contents(&path, &folders)?;
            fs.atomic_write(path.clone(), contents)
                .await
                .with_context(|| format!("Failed to write {}", path.display()))?;
            let path = fs.canonicalize(&path).await?;
            let configuration = read_workspace(path, fs.as_ref()).await?;
            this.update_in(cx, |this, window, cx| {
                this.attach_code_workspace(configuration, window, cx)
            })?
            .await
        })
    }

    /// Edits the settings stored in the attached workspace file. Updates are
    /// applied one at a time so quick successive edits don't overwrite each other.
    pub fn update_code_workspace_settings(
        &mut self,
        update: Box<dyn FnOnce(&mut SettingsContent, &App)>,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let fs = self.app_state.fs.clone();
        let workspace = self
            .code_workspace
            .as_mut()
            .context("This window has no saved workspace")?;
        let path = workspace.path.clone();
        let previous_update = workspace.settings_update.take();
        workspace.settings_update = Some(cx.spawn(async move |this, cx| {
            if let Some(previous_update) = previous_update {
                previous_update.await;
            }
            let result = async {
                let text = fs.load(&path).await?;
                let new_text = cx.update(|cx| {
                    settings::workspace_text_for_update(&path, &text, |settings| {
                        update(settings, cx)
                    })
                })?;
                if new_text != text {
                    fs.atomic_write(path.clone(), new_text).await?;
                }
                anyhow::Ok(())
            }
            .await;
            if let Err(error) = result {
                this.update(cx, |this, cx| {
                    this.show_error(
                        format!(
                            "Could not update workspace settings in {}: {error:#}",
                            path.display()
                        ),
                        cx,
                    );
                })
                .log_err();
            }
        }));
        Ok(())
    }

    /// Called whenever the project's folders change, so that folders added to or
    /// removed from a workspace window are saved in its workspace file.
    pub(crate) fn code_workspace_folders_changed(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.code_workspace.as_mut() else {
            return;
        };
        if workspace.applying > 0 {
            return;
        }
        workspace.sync_folders = Some(cx.spawn_in(window, async move |this, cx| {
            // Several folders are often added or removed in a row.
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
            if let Err(error) = write_code_workspace_folders(this.clone(), cx).await {
                this.update(cx, |this, cx| {
                    this.show_error(format!("Could not update workspace folders: {error:#}"), cx)
                })
                .log_err();
            }
        }));
    }
}

async fn write_code_workspace_folders(
    this: WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let Some((path, folders, fs)) = this.read_with(cx, |this, cx| {
        let workspace = this.code_workspace.as_ref()?;
        if workspace.applying > 0 {
            return None;
        }
        let folders = this
            .root_paths(cx)
            .into_iter()
            .map(|path| path.to_path_buf())
            .collect::<Vec<_>>();
        let unchanged = folders.len() == workspace.folders.len()
            && folders
                .iter()
                .all(|folder| workspace.folders.contains(folder));
        (!unchanged).then(|| (workspace.path.clone(), folders, this.app_state.fs.clone()))
    })?
    else {
        return Ok(());
    };
    let text = fs.load(&path).await?;
    let new_text = vscode_workspace::folders_text_for_update(&path, &text, &folders)?;
    if new_text != text {
        fs.atomic_write(path.clone(), new_text).await?;
    }
    this.update(cx, |this, cx| {
        if let Some(workspace) = this
            .code_workspace
            .as_mut()
            .filter(|workspace| workspace.path == path)
        {
            workspace.folders = folders.clone();
        }
        registry::record(path, folders, cx);
    })
}
