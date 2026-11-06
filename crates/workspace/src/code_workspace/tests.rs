use super::*;
use crate::{AppState, MultiWorkspace, OpenMode, OpenOptions, open_paths};
use fs::{FakeFs, Fs as _, RemoveOptions};
use gpui::TestAppContext;
use project::Project;
use serde_json::json;
use settings::{Settings as _, SettingsLocation, SettingsStore};
use std::time::Duration;
use util::{path, rel_path::RelPath};

fn init_test(cx: &mut TestAppContext) {
    crate::tests::init_test(cx);
    cx.update(init);
}

fn effective_tab_size(project: &gpui::Entity<Project>, folder: &str, cx: &TestAppContext) -> u32 {
    project.read_with(cx, |project, cx| {
        let (worktree, _) = project
            .find_worktree(Path::new(folder), cx)
            .expect("workspace folder");
        let location = SettingsLocation {
            worktree_id: worktree.read(cx).id(),
            path: RelPath::empty(),
        };
        language::language_settings::AllLanguageSettings::get(Some(location), cx)
            .defaults
            .tab_size
            .get()
    })
}

#[gpui::test]
async fn test_code_workspace_open_settings_watch_and_restore(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let manifest = json!({
        "folders": [{"path": "a", "name": "Backend"}, {"path": "b"}],
        "settings": {"editor.tabSize": 2},
        "tasks": {"tasks": [{"label": "workspace build", "type": "shell", "command": "echo"}]}
    });
    fs.insert_tree(
        path!("/projects"),
        json!({
            "demo.code-workspace": manifest.to_string(),
            "a": {".vscode": {"settings.json": "{\"editor.tabSize\":3}"}, ".zed": {"settings.json": "{\"tab_size\":8}"}},
            "b": {".vscode": {"settings.json": "{\"editor.tabSize\":4}"}},
            "c": {}
        }),
    )
    .await;
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
    workspace.read_with(cx, |workspace, _| {
        assert_eq!(
            workspace.code_workspace_path(),
            Some(manifest_path.as_path())
        );
        assert_eq!(
            workspace.code_workspace_name().as_deref(),
            Some("demo.code-workspace")
        );
    });

    let project = workspace.read_with(cx, |workspace, _| workspace.project.clone());
    assert_eq!(effective_tab_size(&project, path!("/projects/a"), cx), 8);
    assert_eq!(effective_tab_size(&project, path!("/projects/b"), cx), 4);
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
        let store = cx.global::<SettingsStore>();
        let workspace_file = store
            .workspace_settings_file(worktree.read(cx).id())
            .expect("workspace settings");
        let tab_size = store
            .get_content_for_file(workspace_file)
            .and_then(|content| content.project.all_languages.defaults.tab_size)
            .expect("workspace tab size");
        assert_eq!(tab_size.get(), 2);
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
    assert_eq!(effective_tab_size(&project, path!("/projects/a"), cx), 3);
    fs.remove_file(
        Path::new(path!("/projects/b/.vscode/settings.json")),
        RemoveOptions::default(),
    )
    .await
    .expect("remove VS Code override");
    cx.run_until_parked();
    assert_eq!(effective_tab_size(&project, path!("/projects/b"), cx), 2);

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
    assert_eq!(effective_tab_size(&project, path!("/projects/a"), cx), 3);
    assert_eq!(effective_tab_size(&project, path!("/projects/c"), cx), 5);
    project.read_with(cx, |project, cx| {
        assert!(
            project
                .find_worktree(Path::new(path!("/projects/b")), cx)
                .is_none()
        );
    });
    // Reloading the file must not write the folders back to it.
    let manifest_text = fs.load(&manifest_path).await.expect("manifest");
    assert!(manifest_text.contains("Server"));
    assert!(!manifest_text.contains("\"b\""));

    workspace.read_with(cx, |workspace, cx| {
        let crate::WorkspaceLocation::Location(_, paths) = workspace.workspace_location(cx) else {
            panic!("local workspace")
        };
        assert!(
            paths.paths().iter().all(|path| !is_workspace_file(path)),
            "the database only stores folders"
        );
        let folders = paths.paths().to_vec();
        assert_eq!(
            code_workspace_display_name(&folders, cx).as_deref(),
            Some("demo.code-workspace")
        );
    });
    assert_eq!(
        cx.read(|cx| cx.global::<SettingsStore>().raw_user_settings().cloned()),
        original_user_settings
    );

    let restored = cx
        .update(|cx| {
            crate::Workspace::new_local(
                vec![
                    PathBuf::from(path!("/projects/a")),
                    PathBuf::from(path!("/projects/c")),
                ],
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
    restored.workspace.read_with(cx, |workspace, _| {
        assert_eq!(
            workspace.code_workspace_path(),
            Some(manifest_path.as_path())
        );
    });
}

#[gpui::test]
async fn test_code_workspace_writes_folder_changes(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/projects"),
        json!({
            "demo.code-workspace": "{\n  // keep\n  \"folders\": [{ \"path\": \"a\", \"name\": \"A\" }, { \"path\": \"b\" }]\n}",
            "a": {}, "b": {}, "c": {}
        }),
    )
    .await;
    let project = Project::test(fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let app_state = window
        .read_with(cx, |multi_workspace, cx| {
            multi_workspace.workspace().read(cx).app_state.clone()
        })
        .expect("test window");
    let manifest_path = PathBuf::from(path!("/projects/demo.code-workspace"));
    let result = cx
        .update(|cx| {
            open_paths(
                std::slice::from_ref(&manifest_path),
                app_state,
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
    let project = result
        .workspace
        .read_with(cx, |workspace, _| workspace.project.clone());

    project
        .update(cx, |project, cx| {
            project.create_worktree(path!("/projects/c"), true, cx)
        })
        .await
        .expect("add folder");
    let b = project.read_with(cx, |project, cx| {
        project
            .find_worktree(Path::new(path!("/projects/b")), cx)
            .expect("b folder")
            .0
            .read(cx)
            .id()
    });
    project.update(cx, |project, cx| project.remove_worktree(b, cx));
    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();

    let text = fs.load(&manifest_path).await.expect("manifest");
    let document: serde_json::Value =
        settings::parse_json_with_comments(&text).expect("valid manifest");
    assert_eq!(
        document["folders"],
        json!([{ "path": "a", "name": "A" }, { "path": "c" }])
    );
    assert!(text.contains("// keep"));
    result.workspace.read_with(cx, |workspace, _| {
        assert_eq!(
            workspace.code_workspace_path(),
            Some(manifest_path.as_path())
        );
    });
    project.read_with(cx, |project, cx| {
        assert_eq!(project.visible_worktrees(cx).count(), 2);
    });
}

#[gpui::test]
async fn test_code_workspace_settings_update(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/projects"),
        json!({
            "demo.code-workspace": json!({
                "folders": [{"path": "a"}, {"path": "b"}],
                "settings": {"editor.tabSize": 2, "python.analysis.typeCheckingMode": "strict"},
                "extensions": {"recommendations": ["rust-lang.rust-analyzer"]}
            }).to_string(),
            "a": {}, "b": {}
        }),
    )
    .await;
    let project = Project::test(fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let app_state = window
        .read_with(cx, |multi_workspace, cx| {
            multi_workspace.workspace().read(cx).app_state.clone()
        })
        .expect("test window");
    let manifest_path = PathBuf::from(path!("/projects/demo.code-workspace"));
    let result = cx
        .update(|cx| {
            open_paths(
                std::slice::from_ref(&manifest_path),
                app_state,
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
    {
        let project = result
            .workspace
            .read_with(cx, |workspace, _| workspace.project.clone());
        assert_eq!(effective_tab_size(&project, path!("/projects/b"), cx), 2);
    }

    result.workspace.update(cx, |workspace, cx| {
        workspace
            .update_code_workspace_settings(
                Box::new(|settings, _| {
                    settings.project.all_languages.defaults.tab_size = std::num::NonZeroU32::new(6);
                }),
                cx,
            )
            .expect("attached workspace");
        workspace
            .update_code_workspace_settings(
                Box::new(|settings, _| {
                    settings.project.context_server_timeout = Some(9);
                }),
                cx,
            )
            .expect("attached workspace");
    });
    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();

    let text = fs.load(&manifest_path).await.expect("manifest");
    let document: serde_json::Value =
        settings::parse_json_with_comments(&text).expect("valid manifest");
    // The VS Code settings are only read; Zed writes its own settings to `fork_zed`.
    assert_eq!(document["settings"]["editor.tabSize"], json!(2));
    assert_eq!(document["fork_zed"]["tab_size"], json!(6));
    assert_eq!(document["fork_zed"]["context_server_timeout"], json!(9));
    assert_eq!(
        document["settings"]["python.analysis.typeCheckingMode"],
        json!("strict")
    );
    assert_eq!(
        document["extensions"]["recommendations"],
        json!(["rust-lang.rust-analyzer"])
    );
    let project = result
        .workspace
        .read_with(cx, |workspace, _| workspace.project.clone());
    assert_eq!(effective_tab_size(&project, path!("/projects/b"), cx), 6);
}

#[gpui::test]
async fn test_code_workspace_keeps_nested_roots(cx: &mut TestAppContext) {
    init_test(cx);
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
                worktree.read(cx).abs_path().as_ref() == Path::new(path!("/projects/parent/child"))
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
    init_test(cx);
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
    let original_window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
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
        let new_window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let opened = new_window
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace
                    .workspace()
                    .clone()
                    .update(cx, |workspace, cx| {
                        workspace.open_workspace_for_paths(OpenMode::NewWindow, paths, window, cx)
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
    init_test(cx);
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
