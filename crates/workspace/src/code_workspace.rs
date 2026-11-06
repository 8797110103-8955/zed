use crate::Workspace;
use anyhow::{Context as _, Result};
use futures::StreamExt;
use gpui::{Context, Task, Window};
use project::vscode_workspace::VsCodeWorkspace;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use util::ResultExt;

#[derive(Clone)]
pub(super) struct CodeWorkspaceState {
    pub path: PathBuf,
    pub folders: Vec<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AppState, MultiWorkspace, OpenMode, OpenOptions, WorkspaceLocation, open_paths};
    use fs::{FakeFs, Fs, RemoveOptions};
    use gpui::TestAppContext;
    use project::Project;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    #[gpui::test]
    async fn test_code_workspace_open_settings_watch_and_restore(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let manifest = json!({
            "folders": [{"path": "a", "name": "Backend"}, {"path": "b"}],
            "settings": {"editor.tabSize": 2},
            "tasks": {"tasks": [{"label": "workspace build", "type": "shell", "command": "echo"}]}
        });
        fs.insert_tree(path!("/projects"), json!({
            "demo.code-workspace": manifest.to_string(),
            "a": {".vscode": {"settings.json": "{\"editor.tabSize\":3}"}, ".zed": {"settings.json": "{\"tab_size\":8}"}},
            "b": {".vscode": {"settings.json": "{\"editor.tabSize\":4}"}},
            "c": {}
        })).await;
        let project = Project::test(fs.clone(), [], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let app_state = window
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace().read(cx).app_state.clone()
            })
            .expect("test window");
        let original_user_settings =
            cx.read(|cx| cx.global::<SettingsStore>().raw_user_settings().cloned());
        let manifest_path = PathBuf::from(path!("/projects/demo.code-workspace"));
        let result = cx
            .update(|cx| {
                open_paths(
                    std::slice::from_ref(&manifest_path),
                    app_state.clone(),
                    OpenOptions {
                        requesting_window: Some(window),
                        ..Default::default()
                    },
                    cx,
                )
            })
            .await
            .expect("open workspace file");
        cx.run_until_parked();
        let workspace = result.workspace;
        let reopened = cx
            .update(|cx| {
                open_paths(
                    std::slice::from_ref(&manifest_path),
                    app_state.clone(),
                    OpenOptions::default(),
                    cx,
                )
            })
            .await
            .expect("reopen active workspace");
        assert_eq!(reopened.workspace.entity_id(), workspace.entity_id());
        let project = workspace.read_with(cx, |workspace, _| workspace.project.clone());
        let tab_size = |directory: &str, cx: &gpui::TestAppContext| -> u32 {
            project.read_with(cx, |project, cx| {
                let (worktree, _) = project
                    .find_worktree(Path::new(directory), cx)
                    .expect("workspace folder");
                cx.global::<SettingsStore>()
                    .local_settings(worktree.read(cx).id())
                    .find(|(path, _)| path.is_empty())
                    .and_then(|(_, content)| content.all_languages.defaults.tab_size)
                    .expect("local tab size")
                    .get()
            })
        };
        assert_eq!(tab_size(path!("/projects/a"), cx), 8);
        assert_eq!(tab_size(path!("/projects/b"), cx), 4);
        project.read_with(cx, |project, cx| {
            let (worktree, _) = project
                .find_worktree(Path::new(path!("/projects/a")), cx)
                .expect("a folder");
            assert_eq!(
                project
                    .vscode_worktree_name(worktree.read(cx).id(), cx)
                    .as_deref(),
                Some("Backend")
            );
        });
        let tasks = project
            .read_with(cx, |project, cx| {
                let (worktree, _) = project
                    .find_worktree(Path::new(path!("/projects/a")), cx)
                    .expect("a folder");
                project
                    .task_store()
                    .read(cx)
                    .task_inventory()
                    .expect("task inventory")
                    .read(cx)
                    .list_tasks(None, None, Some(worktree.read(cx).id()), cx)
            })
            .await;
        assert!(
            tasks
                .iter()
                .any(|(_, task)| task.label == "workspace build")
        );
        fs.remove_file(
            Path::new(path!("/projects/a/.zed/settings.json")),
            RemoveOptions::default(),
        )
        .await
        .expect("remove Zed override");
        cx.run_until_parked();
        assert_eq!(tab_size(path!("/projects/a"), cx), 3);
        fs.remove_file(
            Path::new(path!("/projects/b/.vscode/settings.json")),
            RemoveOptions::default(),
        )
        .await
        .expect("remove VS Code override");
        cx.run_until_parked();
        assert_eq!(tab_size(path!("/projects/b"), cx), 2);
        fs.insert_file(
            path!("/projects/demo.code-workspace"),
            json!({
                "folders": [{"path": "a", "name": "Server"}, {"path": "c"}],
                "settings": {"editor.tabSize": 5}
            })
            .to_string()
            .into_bytes(),
        )
        .await;
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
        assert_eq!(tab_size(path!("/projects/a"), cx), 3);
        assert_eq!(tab_size(path!("/projects/c"), cx), 5);
        project.read_with(cx, |project, cx| {
            assert!(
                project
                    .find_worktree(Path::new(path!("/projects/b")), cx)
                    .is_none()
            );
        });
        workspace.read_with(cx, |workspace, cx| {
            let WorkspaceLocation::Location(_, paths) = workspace.workspace_location(cx) else {
                panic!("local workspace")
            };
            assert_eq!(paths.paths(), std::slice::from_ref(&manifest_path));
        });
        assert_eq!(
            cx.read(|cx| cx.global::<SettingsStore>().raw_user_settings().cloned()),
            original_user_settings
        );
        result
            .window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.serialize_workspace_internal(window, cx)
                })
            })
            .expect("serialize workspace")
            .await;
        let database = cx.read(|cx| crate::WorkspaceDb::global(cx));
        let recent = database
            .recent_project_workspaces_ungrouped(fs.as_ref())
            .await
            .expect("recent workspaces");
        assert!(
            recent
                .iter()
                .any(|workspace| workspace.paths.paths() == std::slice::from_ref(&manifest_path))
        );
        let restored = cx
            .update(|cx| {
                crate::Workspace::new_local(
                    vec![manifest_path],
                    app_state,
                    None,
                    None,
                    None,
                    OpenMode::NewWindow,
                    cx,
                )
            })
            .await
            .expect("restore workspace");
        cx.run_until_parked();
        restored.workspace.read_with(cx, |workspace, cx| {
            let paths = workspace.root_paths(cx);
            assert_eq!(paths.len(), 2);
            assert!(
                paths
                    .iter()
                    .any(|path| path.as_ref() == Path::new(path!("/projects/c")))
            );
            assert!(workspace.code_workspace.is_some());
        });
    }

    #[gpui::test]
    async fn test_code_workspace_keeps_nested_roots(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/projects"), json!({
            "nested.code-workspace": json!({"folders": [{"path": "parent"}, {"path": "parent/child", "name": "Child"}], "settings": {"editor.tabSize": 2}}).to_string(),
            "parent": {"child": {"file.txt": ""}}
        })).await;
        let project = Project::test(fs.clone(), [], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let app_state = window
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace().read(cx).app_state.clone()
            })
            .expect("test window");
        let result = cx
            .update(|cx| {
                open_paths(
                    &[path!("/projects/nested.code-workspace").into()],
                    app_state,
                    OpenOptions {
                        requesting_window: Some(window),
                        ..Default::default()
                    },
                    cx,
                )
            })
            .await
            .expect("nested workspace");
        cx.run_until_parked();
        result.workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.root_paths(cx).len(), 2);
            let project = workspace.project.read(cx);
            let child = project
                .visible_worktrees(cx)
                .find(|worktree| {
                    worktree.read(cx).abs_path().as_ref()
                        == Path::new(path!("/projects/parent/child"))
                })
                .expect("independent child root");
            assert_eq!(
                project
                    .vscode_worktree_name(child.read(cx).id(), cx)
                    .as_deref(),
                Some("Child")
            );
        });
    }

    #[gpui::test]
    async fn test_recent_project_opens_in_requesting_empty_window(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/projects"),
            json!({
                "demo.code-workspace": json!({"folders": [{"path": "a"}, {"path": "b"}]}).to_string(),
                "a": {}, "b": {}
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [], cx).await;
        let original_window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let app_state = original_window
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace().read(cx).app_state.clone()
            })
            .expect("original window");
        let manifest_path = PathBuf::from(path!("/projects/demo.code-workspace"));
        let original = cx
            .update(|cx| {
                open_paths(
                    std::slice::from_ref(&manifest_path),
                    app_state,
                    OpenOptions {
                        requesting_window: Some(original_window),
                        ..Default::default()
                    },
                    cx,
                )
            })
            .await
            .expect("original project");

        for paths in [
            vec![manifest_path],
            vec![
                PathBuf::from(path!("/projects/a")),
                PathBuf::from(path!("/projects/b")),
            ],
        ] {
            let project = Project::test(fs.clone(), [], cx).await;
            let new_window =
                cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
            let opened = new_window
                .update(cx, |multi_workspace, window, cx| {
                    multi_workspace
                        .workspace()
                        .clone()
                        .update(cx, |workspace, cx| {
                            workspace.open_workspace_for_paths(
                                OpenMode::NewWindow,
                                paths,
                                window,
                                cx,
                            )
                        })
                })
                .expect("new window")
                .await
                .expect("open recent project");
            assert_ne!(opened.entity_id(), original.workspace.entity_id());
            new_window
                .read_with(cx, |multi_workspace, _| {
                    assert_eq!(multi_workspace.workspace().entity_id(), opened.entity_id());
                })
                .expect("project stays in new window");
            original_window
                .read_with(cx, |multi_workspace, _| {
                    assert_eq!(
                        multi_workspace.workspace().entity_id(),
                        original.workspace.entity_id()
                    );
                })
                .expect("original project stays open");
        }
        assert_eq!(cx.read(|cx| cx.windows().len()), 3);
    }

    #[gpui::test]
    async fn test_code_workspace_invalid_file_does_not_open_a_project(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);
        let app_state = cx.update(AppState::test);
        let result = cx
            .update(|cx| {
                open_paths(
                    &[PathBuf::from(path!("/missing.code-workspace"))],
                    app_state,
                    OpenOptions::default(),
                    cx,
                )
            })
            .await;
        assert!(result.is_err());
        assert!(cx.read(|cx| cx.windows().is_empty()));
    }
}

pub(super) fn is_workspace_file(path: &Path) -> bool {
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

impl Workspace {
    pub(super) fn watch_code_workspace(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fs = self.app_state.fs.clone();
        self.code_workspace_watcher = Some(cx.spawn_in(window, async move |this, cx| {
            let (events, _watcher) = fs.watch(&path, Duration::from_millis(100)).await;
            futures::pin_mut!(events);
            let mut previous_content = None;
            loop {
                match fs.load(&path).await {
                    Ok(content) if previous_content.as_ref() != Some(&content) => {
                        previous_content = Some(content.clone());
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
                if events.next().await.is_none() {
                    break;
                }
            }
        }));
    }

    pub(super) fn apply_code_workspace(
        &mut self,
        configuration: VsCodeWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let fs = self.app_state.fs.clone();
        cx.spawn_in(window, async move |this, cx| {
            let mut folders = Vec::new();
            let mut missing = Vec::new();
            for folder in &configuration.folders {
                if fs.is_dir(&folder.path).await {
                    this.update(cx, |this, cx| {
                        this.project.update(cx, |project, cx| {
                            let existing = project.worktrees(cx).find(|worktree| {
                                worktree.read(cx).abs_path().as_ref() == folder.path
                            });
                            if let Some(worktree) = existing {
                                Task::ready(Ok(worktree))
                            } else {
                                project.create_worktree(&folder.path, true, cx)
                            }
                        })
                    })?
                    .await?;
                    folders.push(folder.path.clone());
                } else {
                    missing.push(folder.path.display().to_string());
                }
            }
            let state = CodeWorkspaceState {
                path: configuration.path.clone(),
                folders,
            };
            this.update_in(cx, |this, window, cx| {
                if let Some(previous) = this.code_workspace.take() {
                    let removed = this
                        .project
                        .read(cx)
                        .visible_worktrees(cx)
                        .filter_map(|worktree| {
                            let path = worktree.read(cx).abs_path();
                            (previous
                                .folders
                                .iter()
                                .any(|previous| previous == path.as_ref())
                                && !state.folders.iter().any(|folder| folder == path.as_ref()))
                            .then_some(worktree.read(cx).id())
                        })
                        .collect::<Vec<_>>();
                    this.project.update(cx, |project, cx| {
                        for id in removed {
                            project.remove_worktree(id, cx);
                        }
                    });
                }
                this.code_workspace = Some(state);
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
            })?;
            Ok(())
        })
    }
}
