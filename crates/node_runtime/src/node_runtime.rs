pub mod package_manager;

pub use package_manager::{
    PackageDistTags, PackageInfo, PackageManager, PackageManagerInstallEvent,
    PackageManagerInvocation, PackageManagerRuntime, VersionStrategy,
    add_installation_to_user_path, detect_package_manager, detect_package_manager_on_disk,
    find_installed_package, installation_in_user_path, node_command_env, project_tool_environment,
    read_package_executable, read_package_installed_version,
};

use anyhow::{Result, bail};
use futures::{FutureExt as _, channel::oneshot, future::Shared};
use semver::Version;
use smol::lock::Mutex;
use std::{
    env,
    ffi::{OsStr, OsString},
    fmt::Display,
    io,
    path::{Path, PathBuf},
    process::Output,
    sync::Arc,
};
use util::ResultExt;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NodeBinaryOptions {
    pub allow_path_lookup: bool,
}

#[derive(Clone)]
pub struct NodeRuntime(
    Arc<Mutex<NodeRuntimeState>>,
    watch::Receiver<Option<Arc<String>>>,
);

struct NodeRuntimeState {
    instance: Option<Box<dyn NodeRuntimeTrait>>,
    last_options: Option<NodeBinaryOptions>,
    options: watch::Receiver<Option<NodeBinaryOptions>>,
    shell_env_loaded: Shared<oneshot::Receiver<()>>,
    errors: watch::Sender<Option<Arc<String>>>,
    last_error: Option<Arc<String>>,
}

impl NodeRuntimeState {
    fn update_error(&mut self, error: Option<Arc<String>>) {
        if self.last_error != error {
            self.last_error = error.clone();
            self.errors.send(error).log_err();
        }
    }

    fn unavailable(&mut self, error_message: impl Into<String>) -> Box<dyn NodeRuntimeTrait> {
        let error_message = Arc::new(error_message.into());
        self.update_error(Some(error_message.clone()));
        Box::new(UnavailableNodeRuntime { error_message })
    }
}

impl NodeRuntime {
    pub fn new(
        shell_env_loaded: Option<oneshot::Receiver<()>>,
        options: watch::Receiver<Option<NodeBinaryOptions>>,
    ) -> Self {
        let (errors, receiver) = watch::channel(None);
        NodeRuntime(
            Arc::new(Mutex::new(NodeRuntimeState {
                instance: None,
                last_options: None,
                options,
                shell_env_loaded: shell_env_loaded.unwrap_or(oneshot::channel().1).shared(),
                errors,
                last_error: None,
            })),
            receiver,
        )
    }

    pub fn unavailable() -> Self {
        Self::new(None, watch::channel(Some(NodeBinaryOptions::default())).1)
    }

    pub fn errors(&self) -> watch::Receiver<Option<Arc<String>>> {
        self.1.clone()
    }

    async fn instance(&self) -> Box<dyn NodeRuntimeTrait> {
        let mut state = self.0.lock().await;

        let options = loop {
            if let Some(options) = state.options.borrow().as_ref() {
                break options.clone();
            }
            match state.options.changed().await {
                Ok(()) => {}
                // failure case not cached
                Err(err) => {
                    return state.unavailable(err.to_string());
                }
            }
        };

        if state.last_options.as_ref() != Some(&options) {
            state.instance.take();
        }
        if let Some(instance) = state.instance.as_ref() {
            return instance.boxed_clone();
        }

        let system_node_error = if options.allow_path_lookup {
            state.shell_env_loaded.clone().await.ok();
            match SystemNodeRuntime::detect().await {
                Ok(instance) => {
                    log::info!("using Node.js found on PATH: {:?}", instance);
                    state.update_error(None);
                    state.instance = Some(instance.boxed_clone());
                    state.last_options = Some(options);
                    return Box::new(instance) as Box<dyn NodeRuntimeTrait>;
                }
                Err(err) => Some(err),
            }
        } else {
            None
        };

        let instance = if let Some(system_node_error) = system_node_error {
            // failure case not cached, since it's cheap to check again
            return state.unavailable(format!(
                "This feature requires a supported Node.js installation. {system_node_error}. Install it manually and retry: https://nodejs.org/en/download"
            ));
        } else {
            // failure case is cached because it will always happen with these options
            state.unavailable("Node.js is unavailable. Install it manually and retry: https://nodejs.org/en/download")
        };

        state.instance = Some(instance.boxed_clone());
        state.last_options = Some(options);
        instance
    }

    pub async fn binary_path(&self) -> Result<PathBuf> {
        self.instance().await.binary_path()
    }
}

trait NodeRuntimeTrait: Send + Sync {
    fn boxed_clone(&self) -> Box<dyn NodeRuntimeTrait>;
    fn binary_path(&self) -> Result<PathBuf>;
}
/// A system Node.js executable whose version has been checked.
///
/// Discovery does not require npm, initialize a cache, or download a runtime.
#[derive(Debug, Clone)]
pub struct SystemNode {
    path: PathBuf,
    version: Version,
}

/// A failure to locate or validate a system Node.js executable.
#[derive(Debug, thiserror::Error)]
pub enum NodeDiscoveryError {
    #[error("Could not read the current directory while looking for Node.js: {0}")]
    CurrentDirectory(#[source] io::Error),
    #[error(
        "Node.js executable {path:?} was not found. Install Node.js {minimum} or newer.",
        minimum = SystemNode::MIN_VERSION,
    )]
    NotFound {
        path: PathBuf,
        #[source]
        source: which::Error,
    },
    #[error("Could not run Node.js at {path:?}: {source}. Check the Node.js installation.")]
    Probe {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "Node.js at {path:?} failed its version check ({status}). Check the Node.js installation.\nstdout: {stdout}\nstderr: {stderr}",
        status = .output.status,
        stdout = String::from_utf8_lossy(&.output.stdout),
        stderr = String::from_utf8_lossy(&.output.stderr),
    )]
    UnsuccessfulExit { path: PathBuf, output: Output },
    #[error(
        "Node.js at {path:?} reported an invalid version: {output:?}. Install Node.js {minimum} or newer.",
        minimum = SystemNode::MIN_VERSION,
    )]
    InvalidVersion {
        path: PathBuf,
        output: String,
        #[source]
        source: semver::Error,
    },
    #[error(
        "Node.js at {path:?} is too old ({version}). Install Node.js {minimum} or newer.",
        minimum = SystemNode::MIN_VERSION,
    )]
    TooOld { path: PathBuf, version: Version },
}

impl SystemNode {
    const MIN_VERSION: Version = Version::new(22, 0, 0);

    /// Finds and validates Node.js using an explicit executable or search path.
    ///
    /// An explicit executable takes precedence and never falls back to another
    /// candidate on failure. Otherwise the first executable named `node` in
    /// `search_path` is checked, preserving PATH order even if it is too old.
    /// `None` disables PATH lookup; it does not read the process's PATH.
    ///
    /// A configured basename is also looked up in `search_path`; use `./name`
    /// to select one in the current directory. Relative paths containing
    /// separators are resolved against that directory. On Windows, lookup
    /// honors PATHEXT, including batch-file wrappers.
    ///
    /// The caller must finish loading its login-shell environment before calling
    /// this method. The version probe inherits that environment, with PATH
    /// replaced by `search_path` when supplied. Discovery does not launch a login
    /// shell to obtain the environment.
    ///
    /// # Errors
    ///
    /// Returns a typed failure if lookup or execution fails, the version cannot
    /// be parsed, or the executable reports a version older than 22.0.0.
    pub async fn discover(
        configured_path: Option<PathBuf>,
        search_path: Option<OsString>,
    ) -> std::result::Result<Self, NodeDiscoveryError> {
        let path = smol::unblock({
            let search_path = search_path.clone();
            move || {
                find_node_path(
                    configured_path.as_deref(),
                    search_path.as_deref(),
                    Path::new("."),
                )
            }
        })
        .await?;
        let mut command = util::command::new_command(&path);
        command.arg("--version").kill_on_drop(true);
        if let Some(search_path) = search_path {
            command.env("PATH", search_path);
        }
        let output = command
            .output()
            .await
            .map_err(|source| NodeDiscoveryError::Probe {
                path: path.clone(),
                source,
            })?;
        let version = check_node_version(&path, output)?;
        Ok(Self { path, version })
    }

    /// Returns the absolute executable path without resolving symlinks.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the version reported by the executable.
    pub fn version(&self) -> &Version {
        &self.version
    }
}

fn find_node_path(
    configured_path: Option<&Path>,
    search_path: Option<&OsStr>,
    directory: &Path,
) -> std::result::Result<PathBuf, NodeDiscoveryError> {
    let path = configured_path.unwrap_or_else(|| Path::new("node"));
    let path = which::which_in(path, search_path, directory).map_err(|source| {
        NodeDiscoveryError::NotFound {
            path: path.to_path_buf(),
            source,
        }
    })?;
    // The cwd may have been deleted even when Node's absolute path still works.
    // Only relative results need it, but those must be anchored before launch
    // so the command cannot search PATH a second time.
    std::path::absolute(directory.join(path)).map_err(NodeDiscoveryError::CurrentDirectory)
}

fn check_node_version(
    path: &Path,
    output: Output,
) -> std::result::Result<Version, NodeDiscoveryError> {
    if !output.status.success() {
        return Err(NodeDiscoveryError::UnsuccessfulExit {
            path: path.to_path_buf(),
            output,
        });
    }
    let version_string = String::from_utf8_lossy(&output.stdout);
    let version =
        Version::parse(version_string.trim().trim_start_matches('v')).map_err(|source| {
            NodeDiscoveryError::InvalidVersion {
                path: path.to_path_buf(),
                output: version_string.into_owned(),
                source,
            }
        })?;
    if version < SystemNode::MIN_VERSION {
        return Err(NodeDiscoveryError::TooOld {
            path: path.to_path_buf(),
            version,
        });
    }
    Ok(version)
}

#[derive(Debug, Clone)]
pub struct SystemNodeRuntime {
    node: PathBuf,
}
impl SystemNodeRuntime {
    async fn new(node: PathBuf) -> Result<Self> {
        let node = SystemNode::discover(Some(node), env::var_os("PATH"))
            .await?
            .path;
        Ok(Self { node })
    }
    async fn detect() -> std::result::Result<Self, DetectError> {
        let node = which::which("node").map_err(DetectError::NotInPath)?;
        Self::new(node).await.map_err(DetectError::Other)
    }
}
enum DetectError {
    NotInPath(which::Error),
    Other(anyhow::Error),
}

impl Display for DetectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DetectError::NotInPath(err) => {
                write!(f, "system Node.js wasn't found on PATH: {}", err)
            }
            DetectError::Other(err) => {
                write!(f, "checking system Node.js failed with error: {}", err)
            }
        }
    }
}

impl NodeRuntimeTrait for SystemNodeRuntime {
    fn boxed_clone(&self) -> Box<dyn NodeRuntimeTrait> {
        Box::new(self.clone())
    }
    fn binary_path(&self) -> Result<PathBuf> {
        Ok(self.node.clone())
    }
}
#[derive(Clone)]
pub struct UnavailableNodeRuntime {
    error_message: Arc<String>,
}
impl NodeRuntimeTrait for UnavailableNodeRuntime {
    fn boxed_clone(&self) -> Box<dyn NodeRuntimeTrait> {
        Box::new(self.clone())
    }
    fn binary_path(&self) -> Result<PathBuf> {
        bail!("{}", self.error_message)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process::ExitStatus};
    #[test]
    fn test_node_lookup_distinguishes_basenames_and_relative_paths() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let binary_directory = directory.path().join("bin");
        fs::create_dir(&binary_directory)?;
        let filename = format!("node{}", env::consts::EXE_SUFFIX);
        let node = binary_directory.join(&filename);
        fs::write(&node, b"not executed by lookup tests")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&node, fs::Permissions::from_mode(0o755))?;
        }
        for (configured_path, search_path) in [
            (PathBuf::from(&filename), Some(binary_directory.as_os_str())),
            (Path::new("bin").join(&filename), None),
        ] {
            assert_eq!(
                find_node_path(Some(&configured_path), search_path, directory.path())?,
                node
            );
        }
        Ok(())
    }
    #[test]
    fn test_node_version_validation() -> Result<()> {
        let path = Path::new("node");
        for (stdout, expected) in [
            ("v22.0.0\n", Version::new(22, 0, 0)),
            (" \t22.1.0\r\n", Version::new(22, 1, 0)),
        ] {
            assert_eq!(check_node_version(path, node_output(stdout))?, expected);
        }
        for version in ["21.7.3", "22.0.0-rc.1"] {
            let error = check_node_version(path, node_output(&format!("v{version}\n")))
                .expect_err("unsupported version");
            assert!(
                matches!(&error, NodeDiscoveryError::TooOld { path: actual_path, version: actual_version }
                    if actual_path == path && actual_version == &Version::parse(version)?)
            );
            assert!(
                error
                    .to_string()
                    .contains("Install Node.js 22.0.0 or newer")
            );
        }
        let stdout = "not node\n";
        assert!(matches!(
            check_node_version(path, node_output(stdout)),
            Err(NodeDiscoveryError::InvalidVersion { path: actual_path, output, .. })
                if actual_path == path && output == stdout
        ));
        Ok(())
    }

    fn node_output(stdout: &str) -> Output {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt;
        Output {
            status: ExitStatus::from_raw(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }
}
