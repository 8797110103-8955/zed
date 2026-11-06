use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use serde_json::Value;
use settings::{ProjectSettingsContent, VsCodeSettings, parse_json_with_comments};
use std::path::{Path, PathBuf};
use task::{DebugTaskFile, TaskTemplates, VsCodeDebugTaskFile, VsCodeTaskFile};
use util::paths::SanitizedPath;

#[derive(Clone, Debug)]
pub struct VsCodeWorkspace {
    pub path: PathBuf,
    pub folders: Vec<VsCodeWorkspaceFolder>,
    pub settings: ProjectSettingsContent,
    pub tasks: Option<Value>,
    pub launch: Option<Value>,
}

#[derive(Clone, Debug)]
pub struct VsCodeWorkspaceFolder {
    pub path: PathBuf,
    pub name: Option<String>,
}

impl VsCodeWorkspace {
    pub fn parse(path: PathBuf, content: &str) -> Result<Self> {
        #[derive(Deserialize)]
        struct WorkspaceFile {
            folders: Vec<Folder>,
            #[serde(default = "empty_object")]
            settings: Value,
            fork_zed: Option<Value>,
            tasks: Option<Value>,
            launch: Option<Value>,
        }
        #[derive(Deserialize)]
        struct Folder {
            path: Option<String>,
            uri: Option<String>,
            name: Option<String>,
        }
        let file: WorkspaceFile = parse_json_with_comments(content)
            .with_context(|| format!("Invalid workspace file {}", path.display()))?;
        let parent = path
            .parent()
            .context("Workspace file has no parent directory")?;
        let mut folders = Vec::new();
        for folder in file.folders {
            let folder_path = resolve_folder_path(parent, folder.path, folder.uri)?;
            if !folders
                .iter()
                .any(|folder: &VsCodeWorkspaceFolder| folder.path == folder_path)
            {
                folders.push(VsCodeWorkspaceFolder {
                    path: folder_path,
                    name: folder.name,
                });
            }
        }
        let settings =
            settings::workspace_project_settings(&path, &file.settings, file.fork_zed.as_ref())?;
        Ok(Self {
            path,
            folders,
            settings,
            tasks: file.tasks,
            launch: file.launch,
        })
    }

    pub fn tasks_for_folder(&self, folder: &Path) -> Result<Option<String>> {
        self.tasks
            .as_ref()
            .map(|tasks| {
                let tasks = self.resolve_variables(tasks.clone(), folder);
                let tasks: VsCodeTaskFile = serde_json::from_value(tasks)?;
                Ok(serde_json::to_string(&TaskTemplates::try_from(tasks)?)?)
            })
            .transpose()
    }

    pub fn launch_for_folder(&self, folder: &Path) -> Result<Option<String>> {
        self.launch
            .as_ref()
            .map(|launch| {
                let launch = self.resolve_variables(launch.clone(), folder);
                let launch: VsCodeDebugTaskFile = serde_json::from_value(launch)?;
                Ok(serde_json::to_string(&DebugTaskFile::try_from(launch)?)?)
            })
            .transpose()
    }

    fn resolve_variables(&self, mut value: Value, folder: &Path) -> Value {
        fn visit(value: &mut Value, replacements: &[(String, String)]) {
            match value {
                Value::String(text) => {
                    for (variable, replacement) in replacements {
                        *text = text.replace(variable, replacement);
                    }
                }
                Value::Array(values) => values
                    .iter_mut()
                    .for_each(|value| visit(value, replacements)),
                Value::Object(values) => values
                    .values_mut()
                    .for_each(|value| visit(value, replacements)),
                _ => {}
            }
        }
        let mut replacements = vec![(
            "${workspaceFolder}".into(),
            folder.to_string_lossy().into_owned(),
        )];
        if let Some(parent) = self.path.parent() {
            replacements.push((
                "${workspaceFileDirname}".into(),
                parent.to_string_lossy().into_owned(),
            ));
        }
        for folder in &self.folders {
            if let Some(name) = folder
                .name
                .as_deref()
                .or_else(|| folder.path.file_name()?.to_str())
            {
                replacements.push((
                    format!("${{workspaceFolder:{name}}}"),
                    folder.path.to_string_lossy().into_owned(),
                ));
            }
        }
        visit(&mut value, &replacements);
        value
    }
}

fn resolve_folder_path(
    parent: &Path,
    path: Option<String>,
    uri: Option<String>,
) -> Result<PathBuf> {
    let folder_path = match (path, uri) {
        (Some(folder_path), None) => parent.join(folder_path),
        (None, Some(uri)) => url::Url::parse(&uri)?
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("Unsupported workspace folder URI: {uri}"))?,
        _ => bail!("Each workspace folder must specify either path or a local file URI"),
    };
    Ok(util::paths::normalize_path(
        SanitizedPath::new(&folder_path).as_path(),
    ))
}

/// Rewrites the `folders` list of the workspace file at `path` to contain exactly
/// `folders`, keeping the existing entries (and their names) of folders that remain.
pub fn folders_text_for_update(path: &Path, text: &str, folders: &[PathBuf]) -> Result<String> {
    let document: Value = parse_json_with_comments(text)
        .with_context(|| format!("Invalid workspace file {}", path.display()))?;
    let parent = path
        .parent()
        .context("Workspace file has no parent directory")?;
    let existing = document
        .get("folders")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            let field = |name: &str| entry.get(name).and_then(Value::as_str).map(str::to_owned);
            let folder_path = resolve_folder_path(parent, field("path"), field("uri")).ok()?;
            Some((folder_path, entry))
        })
        .collect::<Vec<_>>();
    let entries = folders
        .iter()
        .map(|folder| {
            let folder =
                util::paths::normalize_path(SanitizedPath::new(folder.as_path()).as_path());
            existing
                .iter()
                .find(|(existing_path, _)| *existing_path == folder)
                .map(|(_, entry)| entry.clone())
                .unwrap_or_else(|| {
                    let folder_path = match folder.strip_prefix(parent) {
                        Ok(relative) if !relative.as_os_str().is_empty() => relative
                            .components()
                            .map(|component| component.as_os_str().to_string_lossy())
                            .collect::<Vec<_>>()
                            .join("/"),
                        Ok(_) => ".".to_string(),
                        Err(_) => folder.to_string_lossy().into_owned(),
                    };
                    serde_json::json!({ "path": folder_path })
                })
        })
        .collect::<Vec<_>>();
    let mut text = text.to_string();
    let (range, replacement) = settings::replace_value_in_json_text(
        &text,
        &["folders"],
        settings::infer_json_indent_size(&text),
        Some(&Value::Array(entries)),
        None,
    );
    text.replace_range(range, &replacement);
    Ok(text)
}

/// The contents of a new workspace file listing `folders`.
pub fn new_workspace_file_contents(path: &Path, folders: &[PathBuf]) -> Result<String> {
    folders_text_for_update(
        path,
        "{\n  \"folders\": [],\n  \"settings\": {}\n}\n",
        folders,
    )
}

pub(crate) fn empty_object() -> Value {
    Value::Object(Default::default())
}

pub(crate) fn convert_settings(path: &Path, value: Value) -> Result<Value> {
    Ok(serde_json::to_value(
        VsCodeSettings::from_json(path.into(), value)?.project_settings_content(),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use util::path;

    #[test]
    fn test_code_workspace_paths_and_json_comments() -> Result<()> {
        let workspace = VsCodeWorkspace::parse(
            path!("/projects/demo.code-workspace").into(),
            r#"{
            // Paths are relative to the workspace file.
            "folders": [{"path": "server", "name": "Server"}, {"path": "server"}, {"path": "client"},],
            "settings": {"editor.tabSize": 3},
        }"#,
        )?;
        assert_eq!(workspace.folders.len(), 2);
        assert_eq!(
            workspace.folders.first().context("server folder")?.path,
            Path::new(path!("/projects/server"))
        );
        assert_eq!(
            workspace
                .folders
                .first()
                .and_then(|folder| folder.name.as_deref()),
            Some("Server")
        );
        assert_eq!(
            workspace.settings.all_languages.defaults.tab_size,
            std::num::NonZeroU32::new(3)
        );
        Ok(())
    }

    #[test]
    fn test_code_workspace_rejects_invalid_settings_and_remote_uris() {
        for content in [
            r#"{"folders": [], "settings": []}"#,
            r#"{"folders": [{"uri": "vscode-remote://ssh-remote+host/project"}]}"#,
            r#"{"folders": [{"name": "missing path"}]}"#,
        ] {
            assert!(
                VsCodeWorkspace::parse(path!("/projects/demo.code-workspace").into(), content)
                    .is_err()
            );
        }
    }

    #[test]
    fn test_code_workspace_tasks_and_launch_resolve_named_folders() -> Result<()> {
        let content = json!({
            "folders": [{"path": "server"}, {"path": "client", "name": "Client"}],
            "tasks": {"version": "2.0.0", "tasks": [{"label": "build", "type": "shell", "command": "echo", "args": ["${workspaceFolder:Client}"], "options": {"cwd": "${workspaceFolder}"}}]},
            "launch": {"configurations": [{"name": "debug", "type": "lldb", "request": "launch", "program": "${workspaceFolder:Client}/app", "cwd": "${workspaceFileDirname}"}]}
        });
        let workspace = VsCodeWorkspace::parse(
            path!("/projects/demo.code-workspace").into(),
            &content.to_string(),
        )?;
        let folder = Path::new(path!("/projects/server"));
        let tasks: Value = serde_json::from_str(
            &workspace
                .tasks_for_folder(folder)?
                .context("workspace tasks")?,
        )?;
        assert_eq!(
            tasks.pointer("/0/cwd"),
            Some(&json!(folder.to_string_lossy()))
        );
        assert_eq!(
            tasks.pointer("/0/args/0"),
            Some(&json!(path!("/projects/client")))
        );
        let launch = workspace
            .launch_for_folder(folder)?
            .context("workspace launch")?;
        assert!(!launch.contains("${workspaceFolder"));
        assert!(!launch.contains("${workspaceFileDirname}"));
        let launch: Value = serde_json::from_str(&launch)?;
        assert_eq!(launch.as_array().map(Vec::len), Some(1));
        Ok(())
    }

    #[test]
    fn test_folders_text_for_update_keeps_existing_entries() -> Result<()> {
        let path = Path::new(path!("/projects/demo.code-workspace"));
        let text = r#"{
    // folders
    "folders": [
        { "path": "a", "name": "Backend" },
        { "path": "b" }
    ],
    "settings": { "editor.tabSize": 2 }
}"#;
        let new_text = folders_text_for_update(
            path,
            text,
            &[
                PathBuf::from(path!("/projects/a")),
                PathBuf::from(path!("/projects/nested/c")),
                PathBuf::from(path!("/elsewhere/d")),
            ],
        )?;
        let document: Value = parse_json_with_comments(&new_text)?;
        assert_eq!(
            document["folders"],
            json!([
                { "path": "a", "name": "Backend" },
                { "path": "nested/c" },
                { "path": path!("/elsewhere/d") }
            ])
        );
        assert_eq!(document["settings"], json!({ "editor.tabSize": 2 }));
        assert!(new_text.contains("// folders"));

        let workspace = VsCodeWorkspace::parse(path.into(), &new_text)?;
        assert_eq!(workspace.folders.len(), 3);
        assert_eq!(workspace.folders[0].name.as_deref(), Some("Backend"));
        Ok(())
    }

    #[test]
    fn test_new_workspace_file_contents() -> Result<()> {
        let path = Path::new(path!("/projects/demo.code-workspace"));
        let text = new_workspace_file_contents(
            path,
            &[
                PathBuf::from(path!("/projects/a")),
                PathBuf::from(path!("/projects/b")),
            ],
        )?;
        let workspace = VsCodeWorkspace::parse(path.into(), &text)?;
        assert_eq!(
            workspace
                .folders
                .iter()
                .map(|folder| folder.path.clone())
                .collect::<Vec<_>>(),
            vec![
                PathBuf::from(path!("/projects/a")),
                PathBuf::from(path!("/projects/b"))
            ]
        );
        Ok(())
    }

    #[test]
    fn test_code_workspace_settings_merge_preserves_explicit_overrides() {
        let mut settings =
            json!({"tab_size": 2, "languages": {"Rust": {"tab_size": 4, "hard_tabs": true}}});
        util::merge_non_null_json_value_into(
            json!({"tab_size": null, "languages": {"Rust": {"hard_tabs": false}}}),
            &mut settings,
        );
        assert_eq!(
            settings,
            json!({"tab_size": 2, "languages": {"Rust": {"tab_size": 4, "hard_tabs": false}}})
        );
    }
}
