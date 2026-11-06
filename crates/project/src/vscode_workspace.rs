use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use serde_json::Value;
use settings::{VsCodeSettings, parse_json_with_comments};
use std::path::{Path, PathBuf};
use task::{DebugTaskFile, TaskTemplates, VsCodeDebugTaskFile, VsCodeTaskFile};
use util::paths::SanitizedPath;

#[derive(Clone, Debug)]
pub struct VsCodeWorkspace {
    pub path: PathBuf,
    pub folders: Vec<VsCodeWorkspaceFolder>,
    pub settings: Value,
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
            let folder_path = match (folder.path, folder.uri) {
                (Some(folder_path), None) => parent.join(folder_path),
                (None, Some(uri)) => url::Url::parse(&uri)?
                    .to_file_path()
                    .map_err(|_| anyhow::anyhow!("Unsupported workspace folder URI: {uri}"))?,
                _ => bail!("Each workspace folder must specify either path or a local file URI"),
            };
            let folder_path =
                util::paths::normalize_path(SanitizedPath::new(&folder_path).as_path());
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
        let settings = convert_settings(&path, file.settings)?;
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
        assert_eq!(workspace.settings.get("tab_size"), Some(&json!(3)));
        assert!(workspace.settings.get("base_keymap").is_none());
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
