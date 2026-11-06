//! Settings stored in a VS Code `.code-workspace` file.
//!
//! The VS Code `settings` object is only read, through the VS Code settings
//! import. Zed writes its own settings, in Zed's format, to a top-level
//! `fork_zed` object, which takes precedence when reading. VS Code's workspace
//! schema doesn't restrict top-level keys, and VS Code keeps unknown ones when it
//! rewrites a workspace file.

use crate::{
    ParseStatus, ProjectSettingsContent, SettingsContent, VsCodeSettings, fallible_options,
    infer_json_indent_size, parse_json_with_comments, replace_value_in_json_text,
    update_value_in_json_text,
};
use anyhow::{Context as _, Result, anyhow};
use serde_json::{Map, Value};
use settings_content::merge_from::MergeFrom as _;
use std::path::Path;

const FORK_ZED_KEY: &str = "fork_zed";

/// The Zed project settings of a workspace file: its VS Code `settings`,
/// overridden by its `fork_zed` settings.
pub fn workspace_project_settings(
    path: &Path,
    settings: &Value,
    fork_zed: Option<&Value>,
) -> Result<ProjectSettingsContent> {
    anyhow::ensure!(
        settings.is_object(),
        "Workspace settings must be a JSON object"
    );
    let mut content =
        VsCodeSettings::from_json(path.into(), settings.clone())?.project_settings_content();
    if let Some(fork_zed) = fork_zed {
        content.merge_from(&parse_fork_zed(fork_zed)?);
    }
    Ok(content)
}

/// Applies `update` to the `fork_zed` settings of the workspace file whose
/// contents are `text`, returning the new file contents.
pub fn workspace_text_for_update(
    path: &Path,
    text: &str,
    update: impl FnOnce(&mut SettingsContent),
) -> Result<String> {
    let document: Value = parse_json_with_comments(text)
        .with_context(|| format!("Invalid workspace file {}", path.display()))?;
    anyhow::ensure!(
        document.is_object(),
        "Invalid workspace file {}",
        path.display()
    );
    let raw_fork_zed = document.get(FORK_ZED_KEY);
    let old_content = SettingsContent {
        project: raw_fork_zed
            .map(parse_fork_zed)
            .transpose()?
            .unwrap_or_default(),
        ..Default::default()
    };
    let mut new_content = old_content.clone();
    update(&mut new_content);
    let old_value = serde_json::to_value(&old_content)?;
    let new_value = serde_json::to_value(&new_content)?;
    if old_value == new_value {
        return Ok(text.to_string());
    }

    let mut text = text.to_string();
    let tab_size = infer_json_indent_size(&text);
    if raw_fork_zed.is_none() {
        let (range, replacement) = replace_value_in_json_text(
            &text,
            &[FORK_ZED_KEY],
            tab_size,
            Some(&Value::Object(Map::new())),
            None,
        );
        text.replace_range(range, &replacement);
    }
    // The same diff `SettingsStore` applies to settings files, scoped to `fork_zed`.
    let mut key_path = vec![FORK_ZED_KEY];
    update_value_in_json_text(
        &mut text,
        &mut key_path,
        tab_size,
        &old_value,
        &new_value,
        &mut Vec::new(),
    );

    let document: Value = parse_json_with_comments(&text)?;
    if document
        .get(FORK_ZED_KEY)
        .and_then(Value::as_object)
        .is_some_and(Map::is_empty)
    {
        let (range, replacement) =
            replace_value_in_json_text(&text, &[FORK_ZED_KEY], tab_size, None, None);
        text.replace_range(range, &replacement);
    }
    Ok(text)
}

fn parse_fork_zed(value: &Value) -> Result<ProjectSettingsContent> {
    anyhow::ensure!(
        value.is_object(),
        "The `{FORK_ZED_KEY}` settings of a workspace file must be a JSON object"
    );
    let text = serde_json::to_string(value)?;
    let (settings, status) = fallible_options::parse_json::<ProjectSettingsContent>(&text);
    if let ParseStatus::Failed { error } = status {
        return Err(anyhow!(
            "Invalid `{FORK_ZED_KEY}` settings in workspace file: {error}"
        ));
    }
    settings.with_context(|| format!("Invalid `{FORK_ZED_KEY}` settings in workspace file"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FormatOnSave, LineEndingSetting};
    use serde_json::json;
    use std::{num::NonZeroU32, path::PathBuf};

    fn path() -> PathBuf {
        PathBuf::from("/projects/demo.code-workspace")
    }

    fn update(text: &str, update: impl FnOnce(&mut SettingsContent)) -> String {
        workspace_text_for_update(&path(), text, update).unwrap()
    }

    fn read(text: &str) -> ProjectSettingsContent {
        let document: Value = parse_json_with_comments(text).unwrap();
        let settings = document
            .get("settings")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        workspace_project_settings(&path(), &settings, document.get(FORK_ZED_KEY)).unwrap()
    }

    fn document(text: &str) -> Value {
        parse_json_with_comments(text).unwrap()
    }

    #[test]
    fn test_reads_vscode_settings_and_fork_zed_overrides() {
        let settings = read(
            r#"{
                "folders": [],
                "settings": {
                    "editor.tabSize": 2,
                    "editor.formatOnSave": true,
                    "files.eol": "\n"
                },
                "fork_zed": { "tab_size": 8, "line_ending": "enforce_lf" }
            }"#,
        );
        assert_eq!(settings.all_languages.defaults.tab_size, NonZeroU32::new(8));
        assert_eq!(
            settings.all_languages.defaults.format_on_save,
            Some(FormatOnSave::On)
        );
        assert_eq!(
            settings.all_languages.defaults.line_ending,
            Some(LineEndingSetting::EnforceLf)
        );
    }

    #[test]
    fn test_writes_only_to_fork_zed() {
        let text = r#"{
    // keep this comment
    "folders": [{ "path": "a" }],
    "settings": {
        "editor.tabSize": 2,
        "workbench.colorTheme": "Dark+"
    },
    "extensions": { "recommendations": ["rust-lang.rust-analyzer"] }
}"#;
        let new_text = update(text, |settings| {
            settings.project.all_languages.defaults.tab_size = NonZeroU32::new(4);
            settings.project.all_languages.defaults.format_on_save =
                Some(FormatOnSave::Modifications);
        });
        assert!(new_text.contains("// keep this comment"));
        let document = document(&new_text);
        assert_eq!(
            document["settings"],
            json!({ "editor.tabSize": 2, "workbench.colorTheme": "Dark+" })
        );
        assert_eq!(
            document["extensions"],
            json!({ "recommendations": ["rust-lang.rust-analyzer"] })
        );
        assert_eq!(
            document[FORK_ZED_KEY],
            json!({ "tab_size": 4, "format_on_save": "modifications" })
        );
        let settings = read(&new_text);
        assert_eq!(settings.all_languages.defaults.tab_size, NonZeroU32::new(4));
        assert_eq!(
            settings.all_languages.defaults.format_on_save,
            Some(FormatOnSave::Modifications)
        );
    }

    #[test]
    fn test_creates_fork_zed_with_only_the_changed_settings() {
        let new_text = update(r#"{ "folders": [] }"#, |settings| {
            settings.project.context_server_timeout = Some(30);
        });
        assert_eq!(
            document(&new_text),
            json!({ "folders": [], "fork_zed": { "context_server_timeout": 30 } })
        );
    }

    #[test]
    fn test_removing_settings_only_removes_their_keys() {
        let text = r#"{
    "folders": [],
    "settings": { "editor.tabSize": 2 },
    "fork_zed": { "tab_size": 4, "context_server_timeout": 5, "unknown_setting": 1 }
}"#;
        let new_text = update(text, |settings| {
            settings.project.all_languages.defaults.tab_size = None;
            settings.project.context_server_timeout = None;
        });
        let document = document(&new_text);
        assert_eq!(document["settings"], json!({ "editor.tabSize": 2 }));
        assert_eq!(document[FORK_ZED_KEY], json!({ "unknown_setting": 1 }));
    }

    #[test]
    fn test_empty_fork_zed_is_removed() {
        let text = r#"{
    "folders": [],
    "settings": { "editor.tabSize": 2 },
    "fork_zed": { "context_server_timeout": 5 }
}"#;
        let new_text = update(text, |settings| {
            settings.project.context_server_timeout = None;
        });
        assert_eq!(
            document(&new_text),
            json!({ "folders": [], "settings": { "editor.tabSize": 2 } })
        );
    }

    #[test]
    fn test_unrelated_update_does_not_touch_file() {
        let text = r#"{
    "folders": [],
    "settings": { "editor.formatOnSaveMode": "file" },
    "fork_zed": { "tab_size": 8, "unknown_setting": 1 }
}"#;
        assert_eq!(update(text, |_| {}), text);
    }
}
