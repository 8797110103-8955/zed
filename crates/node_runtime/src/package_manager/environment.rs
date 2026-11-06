use anyhow::{Context as _, Result};
use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    path::{Path, PathBuf},
};
use util::ResultExt as _;

/// Returns the search path, whose name is case-insensitive on Windows (`Path` or `PATH`).
pub(crate) fn path_variable(environment: &HashMap<String, String>) -> Option<&str> {
    environment
        .iter()
        .find(|(name, _)| is_path_variable(name))
        .map(|(_, value)| value.as_str())
}

/// Replaces the search path, dropping differently cased duplicates that would otherwise reach
/// child processes in arbitrary order on Windows.
pub(crate) fn set_path_variable(environment: &mut HashMap<String, String>, value: String) {
    environment.retain(|name, _| !is_path_variable(name));
    environment.insert("PATH".into(), value);
}

fn is_path_variable(name: &str) -> bool {
    if cfg!(windows) {
        name.eq_ignore_ascii_case("PATH")
    } else {
        name == "PATH"
    }
}

/// Keeps Corepack from downloading a missing manager while Zed probes or runs system tools.
pub(super) fn without_corepack_downloads(
    mut environment: HashMap<String, String>,
) -> HashMap<String, String> {
    environment.insert("COREPACK_ENABLE_NETWORK".into(), "0".into());
    environment.insert("COREPACK_ENABLE_AUTO_PIN".into(), "0".into());
    environment
}

pub(super) async fn project_tool_directories(directory: &Path) -> Result<Vec<PathBuf>> {
    let directory = if super::detection::package_manager_file_stamp(directory)
        .await?
        .is_some_and(|metadata| !metadata.is_directory)
    {
        directory
            .parent()
            .context("project file has no parent directory")?
    } else {
        directory
    };
    let mut search_root = directory;
    for ancestor in directory.ancestors() {
        if super::detection::package_manager_file_stamp(&ancestor.join(".git"))
            .await?
            .is_some()
        {
            search_root = ancestor;
            break;
        }
    }
    let mut paths = Vec::new();
    for ancestor in directory.ancestors() {
        paths.push(ancestor.join("node_modules/.bin"));
        if ancestor == search_root {
            break;
        }
    }
    Ok(paths)
}

pub async fn project_tool_environment(
    directory: &Path,
    mut environment: HashMap<String, String>,
) -> Result<HashMap<String, String>> {
    let mut paths = project_tool_directories(directory).await?;
    for installed in super::install::installed_directories()
        .await
        .log_err()
        .unwrap_or_default()
    {
        if !paths.contains(&installed) {
            paths.push(installed);
        }
    }
    let system_path = path_variable(&environment)
        .map(OsString::from)
        .or_else(|| env::var_os("PATH"));
    if let Some(system_path) = system_path {
        for path in env::split_paths(&system_path) {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    set_path_variable(
        &mut environment,
        env::join_paths(paths)?.to_string_lossy().into_owned(),
    );
    Ok(environment)
}

/// Variables Node.js child processes need beyond the caller's environment.
///
/// `PATH` is intentionally absent so the caller's project environment is preserved.
pub fn node_command_env() -> HashMap<String, String> {
    let mut command_env = HashMap::new();
    if let Ok(node_ca_certs) = env::var(NODE_CA_CERTS_ENV_VAR) {
        if !node_ca_certs.is_empty() {
            command_env.insert(NODE_CA_CERTS_ENV_VAR.to_string(), node_ca_certs);
        }
    }

    #[cfg(windows)]
    {
        if let Some(val) = env::var("SYSTEMROOT")
            .context("Missing environment variable: SYSTEMROOT!")
            .log_err()
        {
            command_env.insert("SYSTEMROOT".into(), val);
        }
        if let Some(val) = env::var("ComSpec")
            .context("Missing environment variable: ComSpec!")
            .log_err()
        {
            command_env.insert("ComSpec".into(), val);
        }
    }

    command_env
}

const NODE_CA_CERTS_ENV_VAR: &str = "NODE_EXTRA_CA_CERTS";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_variable_is_replaced_regardless_of_case() {
        let mut environment = HashMap::from([
            ("Path".to_string(), "old".to_string()),
            ("OTHER".to_string(), "kept".to_string()),
        ]);
        set_path_variable(&mut environment, "new".into());
        assert_eq!(path_variable(&environment), Some("new"));
        assert_eq!(environment.get("OTHER").map(String::as_str), Some("kept"));
        assert_eq!(
            environment.len(),
            if cfg!(windows) { 2 } else { 3 },
            "differently cased PATH entries are only duplicates on Windows"
        );
    }
}
