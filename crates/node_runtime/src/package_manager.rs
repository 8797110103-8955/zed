use anyhow::{Context as _, Result, bail, ensure};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::HashMap,
    env,
    future::Future,
    io,
    path::{Path, PathBuf},
    process::Output,
};

use crate::NodeRuntime;
use chrono::{DateTime, Utc};
use futures::AsyncReadExt as _;
use http_client::{Host, HttpClient, Url};
use semver::{Version, VersionReq};
use smol::{fs, lock::Mutex};
use std::{
    ffi::OsString,
    net::{IpAddr, Ipv4Addr},
    sync::{Arc, LazyLock},
    time::SystemTime,
};
use util::ResultExt as _;

#[derive(Clone, Debug)]
pub struct PackageManagerCommand {
    pub package_manager: PackageManager,
    pub path: PathBuf,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub current_dir: PathBuf,
}

pub enum VersionStrategy<'a> {
    Pin(&'a Version),
    Latest(&'a Version),
}

#[derive(Clone)]
pub struct PackageManagerRuntime {
    node_runtime: NodeRuntime,
    http: Arc<dyn HttpClient>,
    state: Arc<Mutex<PackageManagerState>>,
    errors: watch::Receiver<Option<Arc<String>>>,
    project_directory: Option<Arc<Path>>,
}

struct PackageManagerState {
    preference: watch::Receiver<Option<PackageManager>>,
    errors: watch::Sender<Option<Arc<String>>>,
    last_error: Option<Arc<String>>,
}

static PACKAGE_MANAGER_CACHE: LazyLock<Mutex<PackageManagerCache>> =
    LazyLock::new(|| Mutex::new(PackageManagerCache::default()));

#[derive(Default)]
struct PackageManagerCache {
    last_detected: Option<PackageManager>,
    projects: HashMap<PathBuf, CachedProjectPackageManager>,
    executables: HashMap<(PackageManager, PathBuf, OsString), CachedPackageManagerExecutable>,
}

#[derive(Clone)]
struct CachedProjectPackageManager {
    manager: PackageManager,
    sources: Vec<PathBuf>,
    files: Vec<(PathBuf, Option<PackageManagerFileStamp>)>,
}

#[derive(Clone, PartialEq, Eq)]
struct PackageManagerFileStamp {
    modified: SystemTime,
    length: u64,
    is_directory: bool,
}

struct CachedPackageManagerExecutable {
    executable: AvailablePackageManager,
    stamp: PackageManagerFileStamp,
}

async fn package_manager_file_stamp(path: &Path) -> Result<Option<PackageManagerFileStamp>> {
    match fs::metadata(path).await {
        Ok(metadata) => Ok(Some(PackageManagerFileStamp {
            modified: if metadata.is_dir() {
                SystemTime::UNIX_EPOCH
            } else {
                metadata.modified()?
            },
            length: if metadata.is_dir() { 0 } else { metadata.len() },
            is_directory: metadata.is_dir(),
        })),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

#[derive(Debug, thiserror::Error)]
#[error(
    "No usable system package manager was found (preferred: {preferred}). Install pnpm and retry, or install npm, Yarn, Bun, or Vite+ and select it in Package Manager settings. {details}"
)]
struct PackageManagerUnavailable {
    preferred: &'static str,
    details: String,
}

impl PackageManagerRuntime {
    pub fn new(
        node_runtime: NodeRuntime,
        http: Arc<dyn HttpClient>,
        preference: watch::Receiver<Option<PackageManager>>,
    ) -> Self {
        let (sender, errors) = watch::channel(None);
        Self {
            node_runtime,
            http,
            state: Arc::new(Mutex::new(PackageManagerState {
                preference,
                errors: sender,
                last_error: None,
            })),
            errors,
            project_directory: None,
        }
    }

    pub fn unavailable() -> Self {
        Self::new(
            NodeRuntime::unavailable(),
            Arc::new(http_client::BlockedHttpClient),
            watch::channel(Some(PackageManager::Auto)).1,
        )
    }

    pub fn node_runtime(&self) -> &NodeRuntime {
        &self.node_runtime
    }

    pub async fn binary_path(&self) -> Result<PathBuf> {
        self.node_runtime.binary_path().await
    }

    pub fn errors(&self) -> watch::Receiver<Option<Arc<String>>> {
        self.errors.clone()
    }

    pub fn for_project(&self, directory: impl AsRef<Path>) -> Self {
        let mut runtime = self.clone();
        runtime.project_directory = Some(Arc::from(directory.as_ref()));
        runtime
    }

    pub async fn package_manager_command(
        &self,
        directory: Option<&Path>,
        subcommand: &str,
        arguments: &[&str],
    ) -> Result<PackageManagerCommand> {
        let result = self.build_command(directory, subcommand, arguments).await;
        let error = match &result {
            Err(error) if error.is::<PackageManagerUnavailable>() => {
                Some(Arc::new(error.to_string()))
            }
            Ok(_) => None,
            Err(_) => return result,
        };
        let mut state = self.state.lock().await;
        if error != state.last_error {
            state.last_error = error.clone();
            state.errors.send(error).log_err();
        }
        result
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum PackageManager {
    #[default]
    Auto,
    Npm,
    Pnpm,
    Yarn,
    Bun,
    Vp,
}

impl PackageManager {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
            Self::Bun => "bun",
            Self::Vp => "vp",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.split('@').next()? {
            "npm" => Some(Self::Npm),
            "pnpm" => Some(Self::Pnpm),
            "yarn" => Some(Self::Yarn),
            "bun" => Some(Self::Bun),
            "vp" | "vite-plus" => Some(Self::Vp),
            _ => None,
        }
    }

    pub fn from_package_json(package: &Value) -> Option<Self> {
        // Vite+ still declares its underlying package manager, so its dependency is
        // needed to distinguish a vp project from an ordinary pnpm/npm project.
        if ["dependencies", "devDependencies"].iter().any(|key| {
            package
                .get(key)
                .and_then(Value::as_object)
                .is_some_and(|dependencies| dependencies.contains_key("vite-plus"))
        }) {
            return Some(Self::Vp);
        }
        package
            .get("packageManager")
            .and_then(Value::as_str)
            .and_then(Self::from_name)
            .or_else(|| {
                package
                    .pointer("/devEngines/packageManager")
                    .and_then(Self::from_dev_engine)
            })
    }

    fn from_dev_engine(value: &Value) -> Option<Self> {
        match value {
            Value::Array(managers) => managers.iter().find_map(Self::from_dev_engine),
            Value::Object(manager) => manager
                .get("name")
                .and_then(Value::as_str)
                .and_then(Self::from_name),
            _ => None,
        }
    }
}

pub async fn detect_package_manager<ReadFile, ReadFuture, IsFile, IsFileFuture>(
    directory: &Path,
    read_file: ReadFile,
    is_file: IsFile,
) -> Result<PackageManager>
where
    ReadFile: Fn(std::path::PathBuf) -> ReadFuture,
    ReadFuture: Future<Output = Result<Option<String>>>,
    IsFile: Fn(std::path::PathBuf) -> IsFileFuture,
    IsFileFuture: Future<Output = bool>,
{
    for directory in directory.ancestors() {
        let manifest_path = directory.join("package.json");
        if let Some(contents) = read_file(manifest_path.clone()).await? {
            let package: Value = serde_json::from_str(&contents)
                .with_context(|| format!("reading {}", manifest_path.display()))?;
            if let Some(manager) = PackageManager::from_package_json(&package) {
                return Ok(manager);
            }
        }
        for (name, manager) in [
            ("pnpm-workspace.yaml", PackageManager::Pnpm),
            ("pnpm-lock.yaml", PackageManager::Pnpm),
            ("yarn.lock", PackageManager::Yarn),
            (".yarnrc.yml", PackageManager::Yarn),
            ("package-lock.json", PackageManager::Npm),
            ("npm-shrinkwrap.json", PackageManager::Npm),
            ("bun.lock", PackageManager::Bun),
            ("bun.lockb", PackageManager::Bun),
            (".pnpmfile.cjs", PackageManager::Pnpm),
            ("bunfig.toml", PackageManager::Bun),
        ] {
            if is_file(directory.join(name)).await {
                return Ok(manager);
            }
        }
        if is_file(directory.join(".git")).await {
            break;
        }
    }
    Ok(PackageManager::Pnpm)
}

pub async fn detect_package_manager_on_disk(directory: &Path) -> Result<PackageManager> {
    cached_project_package_manager(directory, &[]).await
}

async fn cached_project_package_manager(
    directory: &Path,
    sources: &[PathBuf],
) -> Result<PackageManager> {
    let directory = if smol::fs::metadata(directory)
        .await
        .with_context(|| format!("reading project directory {}", directory.display()))?
        .is_file()
    {
        directory
            .parent()
            .context("project file has no parent directory")?
    } else {
        directory
    };
    // The global cache lock is held only for lookups and updates, not file
    // system scans, so detection for unrelated projects can run concurrently.
    let previous = PACKAGE_MANAGER_CACHE
        .lock()
        .await
        .projects
        .get(directory)
        .filter(|previous| previous.sources == sources)
        .cloned();
    if let Some(previous) = previous {
        let mut unchanged = true;
        for (path, stamp) in &previous.files {
            if package_manager_file_stamp(path).await?.as_ref() != stamp.as_ref() {
                unchanged = false;
                break;
            }
        }
        if unchanged {
            return Ok(previous.manager);
        }
    }

    let mut files = Vec::new();
    for directory in directory.ancestors() {
        for name in [
            "package.json",
            "pnpm-workspace.yaml",
            "pnpm-lock.yaml",
            "yarn.lock",
            ".yarnrc.yml",
            "package-lock.json",
            "npm-shrinkwrap.json",
            "bun.lock",
            "bun.lockb",
            ".pnpmfile.cjs",
            "bunfig.toml",
            ".vscode/settings.json",
            ".vscode/tasks.json",
            ".vscode/launch.json",
            ".zed/settings.json",
            ".git",
        ] {
            let path = directory.join(name);
            let stamp = package_manager_file_stamp(&path).await?;
            files.push((path, stamp));
        }
        if fs::metadata(directory.join(".git")).await.is_ok() {
            break;
        }
    }
    for path in sources {
        files.push((path.clone(), package_manager_file_stamp(path).await?));
    }
    let manager = detect_package_manager(
        directory,
        |path| async move {
            match smol::fs::read_to_string(&path).await {
                Ok(contents) => Ok(Some(contents)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
            }
        },
        |path| async move { smol::fs::metadata(path).await.is_ok() },
    )
    .await?;
    let mut unchanged = true;
    for (path, stamp) in &files {
        if package_manager_file_stamp(path).await?.as_ref() != stamp.as_ref() {
            unchanged = false;
            break;
        }
    }
    let mut cache = PACKAGE_MANAGER_CACHE.lock().await;
    cache
        .executables
        .retain(|(_, cached_directory, _), _| cached_directory != directory);
    if unchanged {
        cache.projects.insert(
            directory.to_path_buf(),
            CachedProjectPackageManager {
                manager,
                sources: sources.to_vec(),
                files,
            },
        );
    }
    Ok(manager)
}

#[derive(Clone)]
struct AvailablePackageManager {
    manager: PackageManager,
    path: PathBuf,
    major_version: u64,
}

impl PackageManagerRuntime {
    pub async fn package_manager_preference(&self) -> Result<PackageManager> {
        Ok({
            let mut state = self.state.lock().await;
            loop {
                if let Some(options) = state.preference.borrow().as_ref() {
                    break *options;
                }
                state.preference.changed().await?;
            }
        })
    }

    pub async fn task_command(
        &self,
        directory: &Path,
        arguments: &[String],
        environment: HashMap<String, String>,
        preference: PackageManager,
        sources: &[PathBuf],
    ) -> Result<PackageManagerCommand> {
        let manager = if preference == PackageManager::Auto {
            cached_project_package_manager(directory, sources).await?
        } else {
            preference
        };
        let mut probe_environment = environment.clone();
        probe_environment.entry("PATH".into()).or_insert_with(|| {
            env::var_os("PATH")
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
        probe_environment.insert("COREPACK_ENABLE_NETWORK".into(), "0".into());
        probe_environment.insert("COREPACK_ENABLE_AUTO_PIN".into(), "0".into());
        let available = find_package_manager(&[manager], directory, &probe_environment)
            .await
            .with_context(|| format!(
                "This task requires {}. Install {} and retry, or change the Package Manager setting.",
                manager.name(), manager.name()
            ))?;
        let (operation, arguments) = arguments
            .split_first()
            .context("missing package manager task operation")?;
        let mut args = match operation.as_str() {
            "run" => vec!["run".into()],
            "exec" => match manager {
                PackageManager::Npm => vec!["exec".into(), "--".into()],
                PackageManager::Bun => vec!["run".into()],
                _ => vec!["exec".into()],
            },
            _ => bail!("Unsupported package manager task operation: {operation}"),
        };
        let arguments = match arguments.split_first() {
            Some((delimiter, rest)) if operation == "exec" && delimiter == "--" => rest,
            _ => arguments,
        };
        args.extend_from_slice(arguments);
        if preference == PackageManager::Auto {
            let mut cache = PACKAGE_MANAGER_CACHE.lock().await;
            cache.last_detected.get_or_insert(manager);
        }
        log::debug!("Running {} task in {}", manager.name(), directory.display());
        Ok(PackageManagerCommand {
            package_manager: manager,
            path: available.path,
            args,
            env: environment,
            current_dir: directory.to_path_buf(),
        })
    }

    pub async fn preferred_package_manager(&self) -> Result<PackageManager> {
        let preference = self.package_manager_preference().await?;
        if preference != PackageManager::Auto {
            return Ok(preference);
        }
        let directory = match &self.project_directory {
            Some(directory) => directory.to_path_buf(),
            None => env::current_dir().context("finding package manager detection directory")?,
        };
        detect_package_manager_on_disk(&directory).await
    }

    async fn build_command(
        &self,
        directory: Option<&Path>,
        subcommand: &str,
        arguments: &[&str],
    ) -> Result<PackageManagerCommand> {
        let node = self.binary_path().await?;
        let preference = self.package_manager_preference().await?;
        let preferred = if preference == PackageManager::Auto {
            PackageManager::Pnpm
        } else {
            preference
        };
        let runtime_directory = paths::data_dir().join("package_manager");
        let directory = directory
            .map(Path::to_path_buf)
            .unwrap_or_else(|| runtime_directory.join("package-manager-work"));
        smol::fs::create_dir_all(&directory)
            .await
            .with_context(|| format!("creating tool directory {}", directory.display()))?;
        let mut command_env = package_manager_command_env(&node);
        // Corepack must not download a missing manager while probing system tools.
        command_env.insert("COREPACK_ENABLE_NETWORK".into(), "0".into());
        command_env.insert("COREPACK_ENABLE_AUTO_PIN".into(), "0".into());
        let mut candidates = vec![preferred];
        if preference == PackageManager::Auto {
            let cache = PACKAGE_MANAGER_CACHE.lock().await;
            if let Some(manager) = cache.last_detected {
                if !candidates.contains(&manager) {
                    candidates.push(manager);
                }
            }
        }
        append_package_manager_fallbacks(&mut candidates);
        let available = find_package_manager(&candidates, &directory, &command_env).await?;
        if preference == PackageManager::Auto {
            remember_package_manager(available.manager).await;
        }
        if available.manager != preferred {
            log::warn!(
                "preferred package manager {} is unavailable; using {} for tool installation in {}",
                preferred.name(),
                available.manager.name(),
                directory.display()
            );
        }

        if matches!(subcommand, "install" | "info") {
            create_tool_manifest(&directory).await?;
        }
        let cache = runtime_directory.join("cache");
        let http = self.http.clone();
        if let Some(proxy) = proxy_argument(http.proxy()) {
            command_env.insert("HTTP_PROXY".into(), proxy.clone());
            command_env.insert("HTTPS_PROXY".into(), proxy.clone());
            command_env.insert("npm_config_proxy".into(), proxy);
        }
        let args = if available.manager == PackageManager::Npm {
            Vec::new()
        } else {
            package_manager_arguments(
                available.manager,
                available.major_version,
                subcommand,
                arguments,
            )?
        };
        if available.manager == PackageManager::Npm {
            return Ok(PackageManagerCommand {
                args: build_npm_command_args(
                    None,
                    Some(&directory),
                    &cache.join("npm"),
                    None,
                    None,
                    http.proxy(),
                    subcommand,
                    arguments,
                ),
                path: available.path,
                env: command_env,
                current_dir: directory,
                package_manager: available.manager,
            });
        }
        match available.manager {
            PackageManager::Pnpm | PackageManager::Vp => {
                command_env.insert("npm_config_node_linker".into(), "hoisted".into());
                command_env.insert(
                    "npm_config_store_dir".into(),
                    cache.join("pnpm").to_string_lossy().into_owned(),
                );
                command_env.insert(
                    "npm_config_manage_package_manager_versions".into(),
                    "false".into(),
                );
                command_env.insert("npm_config_package_manager_strict".into(), "false".into());
                if available.manager == PackageManager::Vp {
                    command_env.insert("YARN_NODE_LINKER".into(), "node-modules".into());
                    command_env.insert("YARN_ENABLE_IMMUTABLE_INSTALLS".into(), "false".into());
                }
            }
            PackageManager::Yarn => {
                command_env.insert("YARN_IGNORE_PATH".into(), "1".into());
                command_env.insert(
                    "YARN_CACHE_FOLDER".into(),
                    cache.join("yarn").to_string_lossy().into_owned(),
                );
                if available.major_version >= 2 {
                    // Adapters resolve executables in node_modules; PnP cannot be used here.
                    command_env.insert("YARN_NODE_LINKER".into(), "node-modules".into());
                    command_env.insert("YARN_ENABLE_IMMUTABLE_INSTALLS".into(), "false".into());
                    command_env.insert("YARN_ENABLE_GLOBAL_CACHE".into(), "false".into());
                    if matches!(subcommand, "install" | "info") {
                        create_if_missing(&directory.join("yarn.lock"), "").await?;
                    }
                }
            }
            PackageManager::Bun => {
                command_env.insert(
                    "BUN_INSTALL_CACHE_DIR".into(),
                    cache.join("bun").to_string_lossy().into_owned(),
                );
            }
            PackageManager::Auto | PackageManager::Npm => bail!("invalid resolved package manager"),
        }
        Ok(PackageManagerCommand {
            path: available.path,
            args,
            env: command_env,
            current_dir: directory,
            package_manager: available.manager,
        })
    }
}

impl PackageManagerRuntime {
    pub async fn run_package_manager_subcommand(
        &self,
        directory: Option<&Path>,
        subcommand: &str,
        args: &[&str],
    ) -> Result<Output> {
        let command = self
            .package_manager_command(directory, subcommand, args)
            .await?;
        run_package_manager_command(command).await
    }

    pub async fn package_manager_installed_version(
        &self,
        local_package_directory: &Path,
        name: &str,
    ) -> Result<Option<Version>> {
        read_package_installed_version(local_package_directory.join("node_modules"), name).await
    }

    pub async fn package_manager_latest_version(&self, name: &str) -> Result<Version> {
        self.package_manager_latest_version_with_requirement(name, None)
            .await
    }

    pub async fn package_manager_latest_version_with_requirement(
        &self,
        name: &str,
        version_requirement: Option<&VersionReq>,
    ) -> Result<Version> {
        let command = self
            .package_manager_command(
                None,
                "info",
                &[
                    name,
                    "--json",
                    "--fetch-retry-mintimeout",
                    "2000",
                    "--fetch-retry-maxtimeout",
                    "5000",
                    "--fetch-timeout",
                    "5000",
                ],
            )
            .await?;
        let manager = command.package_manager;
        let output = run_package_manager_command(command).await?;

        let info: PackageInfo =
            deserialize_package_info_from_response(&output.stdout).map_err(|e| {
                anyhow::anyhow!(
                    "failed to parse {} package info response: {e}\nstdout: {}",
                    manager.name(),
                    String::from_utf8_lossy(&output.stdout)
                )
            })?;
        let before = if manager == PackageManager::Npm {
            npm_config_before(self)
                .await
                .context("getting npm before config")
                .log_err()
                .flatten()
        } else {
            None
        };
        let latest_dist_tag = info.dist_tags.latest.clone();
        let selected_version =
            select_package_version(name, info, before.as_deref(), version_requirement)?;
        log::debug!(
            "{} selected package version package={name:?} version_requirement={version_requirement:?} before={before:?} dist_tag_latest={latest_dist_tag:?} selected={selected_version}",
            manager.name()
        );
        Ok(selected_version)
    }

    pub async fn package_manager_install_packages(
        &self,
        directory: &Path,
        packages: &[(&str, &str)],
    ) -> Result<()> {
        if packages.is_empty() {
            return Ok(());
        }

        let packages: Vec<_> = packages
            .iter()
            .map(|(name, version)| format!("{name}@{version}"))
            .collect();

        let arguments: Vec<_> = packages
            .iter()
            .map(|package| package.as_str())
            .chain([
                "--no-package-lock",
                "--save-exact",
                "--fetch-retry-mintimeout",
                "2000",
                "--fetch-retry-maxtimeout",
                "5000",
                "--fetch-timeout",
                "5000",
            ])
            .collect();

        let command = self
            .package_manager_command(Some(directory), "install", &arguments)
            .await?;
        log::debug!(
            "{} installing packages directory={} packages={packages:?}",
            command.package_manager.name(),
            directory.display()
        );
        run_package_manager_command(command).await?;
        Ok(())
    }

    pub async fn package_manager_install_latest_packages(
        &self,
        directory: &Path,
        package_names: &[&str],
    ) -> Result<()> {
        // Resolve dist-tags through the installer so its release-age policies still apply.
        let packages = package_names
            .iter()
            .map(|package_name| (*package_name, "latest"))
            .collect::<Vec<_>>();
        self.package_manager_install_packages(directory, &packages)
            .await
    }

    pub async fn should_install_package(
        &self,
        package_name: &str,
        local_executable_path: &Path,
        local_package_directory: &Path,
        version_strategy: VersionStrategy<'_>,
    ) -> bool {
        // In the case of the local system not having the package installed,
        // or in the instances where we fail to parse package.json data,
        // we attempt to install the package.
        if fs::metadata(local_executable_path).await.is_err() {
            log::debug!(
                "package cache miss package={package_name:?} reason=missing-executable executable={}",
                local_executable_path.display()
            );
            return true;
        }

        let Some(installed_version) = self
            .package_manager_installed_version(local_package_directory, package_name)
            .await
            .log_err()
            .flatten()
        else {
            log::debug!(
                "package cache miss package={package_name:?} reason=missing-installed-version package_dir={}",
                local_package_directory.display()
            );
            return true;
        };

        let version_strategy_label = match &version_strategy {
            VersionStrategy::Pin(version) => format!("pin:{version}"),
            VersionStrategy::Latest(version) => format!("latest:{version}"),
        };
        let should_install = should_install_package_version(&installed_version, version_strategy);
        log::debug!(
            "package cache check package={package_name:?} installed={installed_version} strategy={version_strategy_label} should_install={should_install}"
        );
        should_install
    }
}

async fn remember_package_manager(manager: PackageManager) {
    let mut cache = PACKAGE_MANAGER_CACHE.lock().await;
    if cache.last_detected != Some(manager) {
        cache.last_detected = Some(manager);
    }
}

#[cfg(test)]
async fn find_available_package_manager(
    preferred: PackageManager,
    directory: &Path,
    command_env: &HashMap<String, String>,
) -> Result<AvailablePackageManager> {
    let mut candidates = vec![preferred];
    append_package_manager_fallbacks(&mut candidates);
    find_package_manager(&candidates, directory, command_env).await
}

fn append_package_manager_fallbacks(candidates: &mut Vec<PackageManager>) {
    for manager in [
        PackageManager::Pnpm,
        PackageManager::Npm,
        PackageManager::Yarn,
        PackageManager::Bun,
        PackageManager::Vp,
    ] {
        if !candidates.contains(&manager) {
            candidates.push(manager);
        }
    }
}

async fn find_package_manager(
    candidates: &[PackageManager],
    directory: &Path,
    command_env: &HashMap<String, String>,
) -> Result<AvailablePackageManager> {
    let search_path = command_env
        .get("PATH")
        .map(OsString::from)
        .or_else(|| env::var_os("PATH"));
    let mut failures = Vec::new();
    for &manager in candidates {
        let executable = manager.name();
        let path = smol::unblock({
            let directory = directory.to_path_buf();
            let search_path = search_path.clone();
            move || -> Result<PathBuf> {
                let path = which::which_in(executable, search_path, &directory)?;
                Ok(std::path::absolute(directory.join(path))?)
            }
        })
        .await;
        let Ok(path) = path else { continue };
        let Some(stamp) = package_manager_file_stamp(&path).await? else {
            continue;
        };
        let cache_key = (
            manager,
            directory.to_path_buf(),
            search_path.clone().unwrap_or_default(),
        );
        {
            let cache = PACKAGE_MANAGER_CACHE.lock().await;
            if let Some(previous) = cache.executables.get(&cache_key)
                && previous.executable.path == path
                && previous.stamp == stamp
            {
                return Ok(previous.executable.clone());
            }
        }
        let mut command = util::command::new_command(&path);
        command
            .arg("--version")
            .envs(command_env)
            .kill_on_drop(true)
            .current_dir(directory);
        if manager == PackageManager::Vp {
            // Recent Vite+ releases otherwise install their runtime even for --version.
            command.env("VP_SELF_SETUP_SUPPORT_CHECK", "1");
        }
        // node_runtime has no GPUI executor, and this only bounds a real child process.
        #[allow(clippy::disallowed_methods)]
        let timeout = smol::Timer::after(std::time::Duration::from_secs(5));
        let output = futures::future::select(Box::pin(command.output()), Box::pin(timeout)).await;
        match output {
            futures::future::Either::Left((Ok(output), _)) if output.status.success() => {
                let major_version = if manager == PackageManager::Vp
                    && output.stdout.trim_ascii() == b"vite-plus-self-setup-v1"
                {
                    Some(0)
                } else {
                    String::from_utf8_lossy(&output.stdout)
                        .split_whitespace()
                        .find_map(|value| {
                            semver::Version::parse(value.trim_start_matches('v')).ok()
                        })
                        .map(|version| version.major)
                };
                if let Some(major_version) = major_version {
                    let executable = AvailablePackageManager {
                        manager,
                        path,
                        major_version,
                    };
                    PACKAGE_MANAGER_CACHE.lock().await.executables.insert(
                        cache_key,
                        CachedPackageManagerExecutable {
                            executable: executable.clone(),
                            stamp,
                        },
                    );
                    return Ok(executable);
                }
                failures.push(format!("{} returned an invalid version", manager.name()));
            }
            futures::future::Either::Left((Ok(output), _)) => failures.push(format!(
                "{}: {}",
                manager.name(),
                String::from_utf8_lossy(&output.stderr).trim()
            )),
            futures::future::Either::Left((Err(error), _)) => {
                failures.push(format!("{}: {error}", manager.name()))
            }
            futures::future::Either::Right(_) => {
                failures.push(format!("{} did not respond", manager.name()))
            }
        }
    }
    Err(PackageManagerUnavailable {
        preferred: candidates
            .first()
            .copied()
            .unwrap_or(PackageManager::Pnpm)
            .name(),
        details: failures.join("; "),
    }
    .into())
}

fn package_manager_arguments(
    manager: PackageManager,
    major: u64,
    subcommand: &str,
    arguments: &[&str],
) -> Result<Vec<String>> {
    let mut result: Vec<String> = match (manager, subcommand) {
        (PackageManager::Yarn, "info") if major >= 2 => vec!["npm".into(), "info".into()],
        (PackageManager::Pnpm, "info") => vec!["view".into()],
        (_, "info") => vec!["info".into()],
        (PackageManager::Pnpm, "install") => vec![
            "--ignore-workspace".into(),
            if arguments.iter().any(|argument| !argument.starts_with('-')) {
                "add"
            } else {
                "install"
            }
            .into(),
        ],
        (_, "install") => vec![
            if arguments.iter().any(|argument| !argument.starts_with('-')) {
                "add"
            } else {
                "install"
            }
            .into(),
        ],
        (_, "run-script" | "run") => vec!["run".into()],
        _ => bail!(
            "{} does not support tool operation {subcommand}",
            manager.name()
        ),
    };
    let mut arguments = arguments.iter().copied();
    while let Some(argument) = arguments.next() {
        match argument {
            "--no-package-lock" => {}
            "--fetch-retry-mintimeout" | "--fetch-retry-maxtimeout" | "--fetch-timeout" => {
                arguments.next();
            }
            "--save-exact" => result.push(
                if matches!(manager, PackageManager::Pnpm | PackageManager::Vp) {
                    "--save-exact"
                } else {
                    "--exact"
                }
                .into(),
            ),
            _ => result.push(argument.into()),
        }
    }
    if manager == PackageManager::Yarn && major < 2 {
        result.push("--non-interactive".into());
        if subcommand == "install" {
            result.push("--ignore-workspace-root-check".into());
        }
    }
    Ok(result)
}

async fn create_tool_manifest(directory: &Path) -> Result<()> {
    create_if_missing(&directory.join("package.json"), "{\"private\":true}\n").await
}

async fn create_if_missing(path: &Path, contents: &str) -> Result<()> {
    let path = path.to_path_buf();
    let contents = contents.to_owned();
    smol::unblock(move || {
        use std::io::Write as _;
        let parent = path
            .parent()
            .context("tool configuration has no parent directory")?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(contents.as_bytes())?;
        // Publish complete contents so concurrent installers never observe an empty manifest.
        match temporary.persist_noclobber(&path) {
            Ok(_) => Ok(()),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error.error).with_context(|| format!("creating {}", path.display())),
        }
    })
    .await
}

pub(crate) async fn run_package_manager_command(
    invocation: PackageManagerCommand,
) -> Result<Output> {
    log::debug!(
        "executing {} in {}",
        invocation.package_manager.name(),
        invocation.current_dir.display()
    );
    let mut command = util::command::new_command(&invocation.path);
    command
        .args(&invocation.args)
        .envs(&invocation.env)
        .current_dir(&invocation.current_dir);
    let output = command
        .output()
        .await
        .with_context(|| format!("starting {}", invocation.path.display()))?;
    ensure!(
        output.status.success(),
        "{} failed in {}:\nstdout: {}\nstderr: {}",
        invocation.package_manager.name(),
        invocation.current_dir.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

fn should_install_package_version(
    installed_version: &Version,
    version_strategy: VersionStrategy<'_>,
) -> bool {
    match version_strategy {
        VersionStrategy::Pin(pinned_version) => installed_version != pinned_version,
        VersionStrategy::Latest(latest_version) => installed_version < latest_version,
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PackageInfo {
    #[serde(default)]
    dist_tags: PackageDistTags,
    versions: Vec<Version>,
    #[serde(default, deserialize_with = "deserialize_package_info_time")]
    time: HashMap<String, String>,
}

/// Parse PackageInfo from npm info --json output, handling both v11 and >= v12 formats.
fn deserialize_package_info_from_response(data: &[u8]) -> Result<PackageInfo, serde_json::Error> {
    let value: serde_json::Value = match serde_json::from_slice(data) {
        Ok(value) => value,
        Err(error) => {
            // Yarn Classic emits one JSON record per line, including progress records.
            let Some(value) = data
                .split(|byte| *byte == b'\n')
                .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
                .find(|value| {
                    value.get("type").and_then(serde_json::Value::as_str) == Some("inspect")
                })
            else {
                return Err(error);
            };
            value
        }
    };
    if value.get("type").and_then(serde_json::Value::as_str) == Some("inspect") {
        return PackageInfo::deserialize(&value["data"]);
    }

    // npm >= 12 returns an array with one object: [ { ... } ]
    if let serde_json::Value::Array(arr) = &value {
        if arr.len() == 1 {
            return PackageInfo::deserialize(&arr[0]);
        }
    }

    // npm <= v11 returns a bare JSON object: { ... }
    PackageInfo::deserialize(value)
}

#[derive(Debug, Deserialize, Default)]
pub struct PackageDistTags {
    latest: Option<Version>,
}

// Some registries put non-string values in the `time` map: JFrog Artifactory emits
// `"unpublished": null`, and npm itself reports `unpublished` as an object when a
// package has had versions unpublished. Only version keys map to the RFC 3339 strings
// we read, so keep the string entries and drop the rest rather than failing to parse
// the entire `npm info` response (which would block language server installation).
fn deserialize_package_info_time<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let entries = HashMap::<String, serde_json::Value>::deserialize(deserializer)?;
    Ok(entries
        .into_iter()
        .filter_map(|(key, value)| match value {
            serde_json::Value::String(value) => Some((key, value)),
            _ => None,
        })
        .collect())
}

#[derive(Debug, Deserialize)]
struct NpmConfig {
    #[serde(default)]
    before: Option<String>,
}

async fn npm_config_before(runtime: &PackageManagerRuntime) -> Result<Option<String>> {
    // `npm config get before` renders Date values for display. The JSON config output keeps the
    // computed cutoff in the same ISO format used by `npm info --json` release times.
    let output = runtime
        .run_package_manager_subcommand(None, "config", &["list", "--json"])
        .await?;
    let config: NpmConfig = serde_json::from_slice(&output.stdout)?;
    Ok(config
        .before
        .filter(|before| !before.trim().is_empty() && before != "null"))
}

fn select_package_version(
    package_name: &str,
    mut info: PackageInfo,
    before: Option<&str>,
    version_requirement: Option<&VersionReq>,
) -> Result<Version> {
    if let Some(version_requirement) = version_requirement {
        info.versions
            .retain(|version| version_requirement.matches(version));
        info.versions.sort();
        info.dist_tags.latest = info
            .dist_tags
            .latest
            .take()
            .filter(|version| version_requirement.matches(version));
    }

    if let Some(before) = before
        && !info.time.is_empty()
    {
        let before_timestamp = DateTime::parse_from_rfc3339(before)
            .with_context(|| format!("parsing npm before config timestamp {before:?}"))?
            .with_timezone(&Utc);
        let latest_version = info.dist_tags.latest.as_ref();

        if let Some(version) = latest_version
            && package_version_was_published_before(version, &info.time, &before_timestamp)?
        {
            return Ok(version.clone());
        }

        for version in info.versions.iter().rev() {
            if is_allowed_package_version_before(
                version,
                latest_version,
                &info.time,
                &before_timestamp,
                version_requirement.is_some(),
            )? {
                return Ok(version.clone());
            }
        }

        bail!("no version found for package {package_name} before {before}");
    }

    info.dist_tags
        .latest
        .or_else(|| info.versions.pop())
        .with_context(|| format!("no version found for package {package_name}"))
}

fn is_allowed_package_version_before(
    version: &Version,
    latest_version: Option<&Version>,
    published_at_by_version: &HashMap<String, String>,
    before: &DateTime<Utc>,
    allow_prereleases: bool,
) -> Result<bool> {
    if (!allow_prereleases && !version.pre.is_empty())
        || latest_version.is_some_and(|latest_version| version > latest_version)
    {
        return Ok(false);
    }

    package_version_was_published_before(version, published_at_by_version, before)
}

fn package_version_was_published_before(
    version: &Version,
    published_at_by_version: &HashMap<String, String>,
    before: &DateTime<Utc>,
) -> Result<bool> {
    let Some(published_at) = published_at_by_version.get(&version.to_string()) else {
        return Ok(false);
    };
    let published_at = DateTime::parse_from_rfc3339(published_at)
        .with_context(|| format!("parsing package release timestamp for version {version}"))?
        .with_timezone(&Utc);
    Ok(&published_at <= before)
}

pub async fn read_package_installed_version(
    node_module_directory: PathBuf,
    name: &str,
) -> Result<Option<Version>> {
    let package_json_path = node_module_directory.join(name).join("package.json");

    let mut file = match fs::File::open(package_json_path).await {
        Ok(file) => file,
        Err(err) => {
            if err.kind() == io::ErrorKind::NotFound {
                return Ok(None);
            }

            Err(err)?
        }
    };

    #[derive(Deserialize)]
    struct PackageJson {
        version: Version,
    }

    let mut contents = String::new();
    file.read_to_string(&mut contents).await?;
    let package_json: PackageJson = serde_json::from_str(&contents)?;
    Ok(Some(package_json.version))
}

pub async fn read_package_executable(
    node_module_directory: PathBuf,
    name: &str,
) -> Result<PathBuf> {
    let package_directory = node_module_directory.join(name);

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Bin {
        Path(String),
        Named(HashMap<String, String>),
    }

    #[derive(Deserialize)]
    struct PackageJson {
        bin: Option<Bin>,
    }

    let package_json_path = package_directory.join("package.json");
    let mut file = fs::File::open(&package_json_path)
        .await
        .with_context(|| format!("opening {}", package_json_path.display()))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents).await?;
    let package_json: PackageJson = serde_json::from_str(&contents)
        .with_context(|| format!("parsing {}", package_json_path.display()))?;

    let relative_path = match package_json.bin {
        Some(Bin::Path(path)) => path,
        Some(Bin::Named(bins)) => {
            let unscoped_name = name.rsplit('/').next().unwrap_or(name);
            let path = if bins.len() == 1 {
                bins.values().next()
            } else {
                bins.get(unscoped_name)
            };
            path.with_context(|| {
                format!("package {name} declares no executable named {unscoped_name}")
            })?
            .clone()
        }
        None => bail!("package {name} declares no executable"),
    };

    Ok(package_directory.join(relative_path))
}

fn proxy_argument(proxy: Option<&Url>) -> Option<String> {
    let mut proxy = proxy.cloned()?;
    // Map proxy settings from `http://localhost:10809` to `http://127.0.0.1:10809`
    // NodeRuntime without environment information can not parse `localhost`
    // correctly.
    // TODO: map to `[::1]` if we are using ipv6
    if matches!(proxy.host(), Some(Host::Domain(domain)) if domain.eq_ignore_ascii_case("localhost"))
    {
        // When localhost is a valid Host, so is `127.0.0.1`
        if proxy.set_ip_host(IpAddr::V4(Ipv4Addr::LOCALHOST)).is_err() {
            log::warn!("could not normalize localhost in package manager proxy URL");
        }
    }

    Some(proxy.as_str().to_string())
}

fn build_npm_command_args(
    entrypoint: Option<&Path>,
    prefix_dir: Option<&Path>,
    cache_dir: &Path,
    user_config: Option<&Path>,
    global_config: Option<&Path>,
    proxy: Option<&Url>,
    subcommand: &str,
    args: &[&str],
) -> Vec<String> {
    let mut command_args = Vec::new();
    if let Some(entrypoint) = entrypoint {
        command_args.push(entrypoint.to_string_lossy().into_owned());
    }
    if let Some(prefix_dir) = prefix_dir {
        command_args.push("--prefix".into());
        command_args.push(prefix_dir.to_string_lossy().into_owned());
    }
    command_args.push(subcommand.to_string());
    command_args.push(format!("--cache={}", cache_dir.display()));
    if let Some(user_config) = user_config {
        command_args.push("--userconfig".into());
        command_args.push(user_config.to_string_lossy().into_owned());
    }
    if let Some(global_config) = global_config {
        command_args.push("--globalconfig".into());
        command_args.push(global_config.to_string_lossy().into_owned());
    }
    if let Some(proxy_arg) = proxy_argument(proxy) {
        command_args.push("--proxy".into());
        command_args.push(proxy_arg);
    }
    command_args.extend(args.into_iter().map(|a| a.to_string()));
    command_args
}

pub fn package_manager_command_env(node_binary: &Path) -> HashMap<String, String> {
    let mut command_env = HashMap::new();
    let env_path = path_with_node_binary_prepended(node_binary).unwrap_or_default();
    command_env.insert("PATH".into(), env_path.to_string_lossy().into_owned());

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
fn path_with_node_binary_prepended(node_binary: &Path) -> Option<OsString> {
    let existing_path = env::var_os("PATH");
    let node_bin_dir = node_binary.parent().map(|dir| dir.as_os_str());
    match (existing_path, node_bin_dir) {
        (Some(existing_path), Some(node_bin_dir)) => {
            if let Ok(joined) = env::join_paths(
                [PathBuf::from(node_bin_dir)]
                    .into_iter()
                    .chain(env::split_paths(&existing_path)),
            ) {
                Some(joined)
            } else {
                Some(existing_path)
            }
        }
        (Some(existing_path), None) => Some(existing_path),
        (None, Some(node_bin_dir)) => Some(node_bin_dir.to_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NodeBinaryOptions;
    use std::sync::Arc;

    #[test]
    fn publishes_complete_manifests_without_overwriting_existing_dependencies() -> Result<()> {
        smol::block_on(async {
            let directory = tempfile::tempdir()?;
            futures::future::try_join_all((0..12).map(|_| async {
                create_tool_manifest(directory.path()).await?;
                let contents =
                    smol::fs::read_to_string(directory.path().join("package.json")).await?;
                let manifest: serde_json::Value = serde_json::from_str(&contents)?;
                assert_eq!(
                    manifest.get("private"),
                    Some(&serde_json::Value::Bool(true))
                );
                anyhow::Ok(())
            }))
            .await?;
            let contents = r#"{"private":true,"dependencies":{"existing":"1.0.0"}}"#;
            std::fs::write(directory.path().join("package.json"), contents)?;
            create_tool_manifest(directory.path()).await?;
            assert_eq!(
                std::fs::read_to_string(directory.path().join("package.json"))?,
                contents
            );
            Ok(())
        })
    }

    #[test]
    fn detects_workspace_managers_and_stops_at_repository_boundaries() -> Result<()> {
        smol::block_on(async {
            let directory = tempfile::tempdir()?;
            let root = directory.path().join("workspace");
            let nested = root.join("packages/app/src");
            std::fs::create_dir_all(&nested)?;
            std::fs::create_dir(root.join(".git"))?;
            std::fs::write(
                directory.path().join("package.json"),
                r#"{"packageManager":"yarn@4"}"#,
            )?;
            assert_eq!(
                detect_package_manager_on_disk(&nested).await?,
                PackageManager::Pnpm
            );
            std::fs::write(root.join("pnpm-workspace.yaml"), "packages: [packages/*]")?;
            assert_eq!(
                detect_package_manager_on_disk(&nested).await?,
                PackageManager::Pnpm
            );
            std::fs::write(root.join("package.json"), r#"{"packageManager":"bun@1"}"#)?;
            assert_eq!(
                detect_package_manager_on_disk(&nested).await?,
                PackageManager::Bun
            );
            std::fs::write(
                root.join("packages/app/package.json"),
                r#"{"devEngines":{"packageManager":[{"name":"unknown"},{"name":"yarn"}]}}"#,
            )?;
            assert_eq!(
                detect_package_manager_on_disk(&nested).await?,
                PackageManager::Yarn
            );
            Ok(())
        })
    }

    #[test]
    fn detects_vite_plus_without_replacing_its_underlying_manager() -> Result<()> {
        let package = serde_json::json!({"packageManager":"pnpm@11.1.3", "devDependencies":{"vite-plus":"1.4.0"}});
        assert_eq!(
            PackageManager::from_package_json(&package),
            Some(PackageManager::Vp)
        );
        assert_eq!(package["packageManager"], "pnpm@11.1.3");
        assert_eq!(
            PackageManager::from_package_json(&serde_json::json!({"devDependencies":{"vite":"8"}})),
            None
        );
        Ok(())
    }

    #[test]
    fn detects_supported_lockfiles() -> Result<()> {
        smol::block_on(async {
            for (file, expected) in [
                ("pnpm-lock.yaml", PackageManager::Pnpm),
                ("yarn.lock", PackageManager::Yarn),
                (".yarnrc.yml", PackageManager::Yarn),
                ("package-lock.json", PackageManager::Npm),
                ("bun.lock", PackageManager::Bun),
                ("bun.lockb", PackageManager::Bun),
            ] {
                let directory = tempfile::tempdir()?;
                std::fs::create_dir(directory.path().join(".git"))?;
                std::fs::write(directory.path().join(file), "")?;
                assert_eq!(
                    detect_package_manager_on_disk(directory.path()).await?,
                    expected
                );
            }
            Ok(())
        })
    }

    #[test]
    fn reports_invalid_project_configuration() -> Result<()> {
        smol::block_on(async {
            let directory = tempfile::tempdir()?;
            std::fs::write(directory.path().join("package.json"), "{")?;
            let error = detect_package_manager_on_disk(directory.path())
                .await
                .expect_err("invalid manifest must be reported");
            assert!(error.to_string().contains("package.json"));
            Ok(())
        })
    }

    #[test]
    fn detects_manager_when_the_worktree_root_is_a_file() -> Result<()> {
        smol::block_on(async {
            let directory = tempfile::tempdir()?;
            std::fs::create_dir(directory.path().join(".git"))?;
            std::fs::write(directory.path().join("pnpm-lock.yaml"), "")?;
            let file = directory.path().join("example.ts");
            std::fs::write(&file, "")?;
            assert_eq!(
                detect_package_manager_on_disk(&file).await?,
                PackageManager::Pnpm
            );
            Ok(())
        })
    }

    #[test]
    fn keeps_project_detection_separate_and_applies_setting_changes() -> Result<()> {
        smol::block_on(async {
            let directory = tempfile::tempdir()?;
            let first = directory.path().join("first");
            let second = directory.path().join("second");
            for path in [&first, &second] {
                std::fs::create_dir_all(path.join(".git"))?;
            }
            std::fs::write(first.join("pnpm-lock.yaml"), "")?;
            std::fs::write(second.join("bun.lock"), "")?;
            let (mut sender, options) = watch::channel(Some(PackageManager::Auto));
            let runtime = PackageManagerRuntime::new(
                NodeRuntime::unavailable(),
                Arc::new(http_client::BlockedHttpClient),
                options,
            );
            let first = runtime.for_project(first.as_path());
            let second = runtime.for_project(second.as_path());
            let (first_manager, second_manager) = futures::try_join!(
                first.preferred_package_manager(),
                second.preferred_package_manager()
            )?;
            assert_eq!(first_manager, PackageManager::Pnpm);
            assert_eq!(second_manager, PackageManager::Bun);
            sender.send(Some(PackageManager::Yarn))?;
            assert_eq!(
                first.preferred_package_manager().await?,
                PackageManager::Yarn
            );
            assert_eq!(
                second.preferred_package_manager().await?,
                PackageManager::Yarn
            );
            Ok(())
        })
    }

    fn fake_manager(directory: &Path, manager: PackageManager, usable: bool) -> Result<()> {
        let path = directory.join(if cfg!(windows) {
            format!("{}.cmd", manager.name())
        } else {
            manager.name().into()
        });
        let source = if cfg!(windows) {
            format!(
                "@echo off\r\necho 4.1.0\r\nexit /b {}\r\n",
                if usable { 0 } else { 1 }
            )
        } else {
            format!(
                "#!/bin/sh\necho 4.1.0\nexit {}\n",
                if usable { 0 } else { 1 }
            )
        };
        std::fs::write(&path, source)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    }

    #[test]
    fn prefers_detected_manager_and_falls_back_only_when_unavailable() -> Result<()> {
        smol::block_on(async {
            let directory = tempfile::tempdir()?;
            let environment = HashMap::from([(
                "PATH".into(),
                directory.path().to_string_lossy().into_owned(),
            )]);
            fake_manager(directory.path(), PackageManager::Pnpm, true)?;
            assert_eq!(
                find_available_package_manager(
                    PackageManager::Yarn,
                    directory.path(),
                    &environment
                )
                .await?
                .manager,
                PackageManager::Pnpm
            );
            fake_manager(directory.path(), PackageManager::Npm, true)?;
            assert_eq!(
                find_available_package_manager(
                    PackageManager::Pnpm,
                    directory.path(),
                    &environment
                )
                .await?
                .manager,
                PackageManager::Pnpm
            );
            fake_manager(directory.path(), PackageManager::Yarn, false)?;
            assert_eq!(
                find_available_package_manager(
                    PackageManager::Yarn,
                    directory.path(),
                    &environment
                )
                .await?
                .manager,
                PackageManager::Pnpm
            );
            Ok(())
        })
    }

    #[test]
    fn probes_vite_plus_without_triggering_first_run_setup() -> Result<()> {
        smol::block_on(async {
            let directory = tempfile::tempdir()?;
            let path = directory
                .path()
                .join(if cfg!(windows) { "vp.cmd" } else { "vp" });
            let source = if cfg!(windows) {
                "@echo off\r\nif defined VP_SELF_SETUP_SUPPORT_CHECK (\r\necho vite-plus-self-setup-v1\r\nexit /b 0\r\n)\r\necho setup>setup-triggered\r\nexit /b 1\r\n"
            } else {
                "#!/bin/sh\nif [ -n \"$VP_SELF_SETUP_SUPPORT_CHECK\" ]; then\necho vite-plus-self-setup-v1\nexit 0\nfi\necho setup > setup-triggered\nexit 1\n"
            };
            std::fs::write(&path, source)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
            }
            let environment = HashMap::from([(
                "PATH".into(),
                directory.path().to_string_lossy().into_owned(),
            )]);
            assert_eq!(
                find_available_package_manager(PackageManager::Vp, directory.path(), &environment,)
                    .await?
                    .manager,
                PackageManager::Vp
            );
            assert!(!directory.path().join("setup-triggered").exists());
            Ok(())
        })
    }

    #[test]
    fn reports_missing_managers_and_does_not_hide_install_failures() -> Result<()> {
        smol::block_on(async {
            let directory = tempfile::tempdir()?;
            let environment = HashMap::from([(
                "PATH".into(),
                directory.path().to_string_lossy().into_owned(),
            )]);
            let error =
                find_available_package_manager(PackageManager::Vp, directory.path(), &environment)
                    .await
                    .err()
                    .context("no manager is installed")?;
            assert!(error.to_string().contains("preferred: vp"));
            fake_manager(directory.path(), PackageManager::Pnpm, false)?;
            let executable = directory
                .path()
                .join(if cfg!(windows) { "pnpm.cmd" } else { "pnpm" });
            let error = run_package_manager_command(PackageManagerCommand {
                package_manager: PackageManager::Pnpm,
                path: executable,
                args: vec!["install".into()],
                env: environment,
                current_dir: directory.path().into(),
            })
            .await
            .expect_err("install failure must propagate");
            assert!(error.to_string().contains("pnpm failed"));
            Ok(())
        })
    }

    #[test]
    fn publishes_missing_manager_errors_and_clears_them_after_installation() -> Result<()> {
        const CHILD: &str = "ZED_TEST_MISSING_PACKAGE_MANAGER";
        if env::var_os(CHILD).is_some() {
            return smol::block_on(async {
                let directory = env::current_dir()?;
                paths::set_custom_data_dir(&directory.join("data").to_string_lossy());
                let node_runtime = NodeRuntime::new(
                    None,
                    watch::channel(Some(NodeBinaryOptions {
                        allow_path_lookup: true,
                    }))
                    .1,
                );
                let runtime = PackageManagerRuntime::new(
                    node_runtime,
                    Arc::new(http_client::BlockedHttpClient),
                    watch::channel(Some(PackageManager::Auto)).1,
                )
                .for_project(&directory);
                let mut errors = runtime.errors();
                assert!(errors.borrow().is_none());
                let error = runtime
                    .package_manager_command(None, "info", &[])
                    .await
                    .expect_err("missing package managers must be reported");
                assert!(error.to_string().contains("Install pnpm"));
                assert_eq!(errors.borrow().as_deref(), Some(&error.to_string()));
                fake_manager(&directory, PackageManager::Pnpm, true)?;
                let command = runtime.package_manager_command(None, "info", &[]).await?;
                assert_eq!(command.package_manager, PackageManager::Pnpm);
                assert!(errors.borrow().is_none());
                Ok(())
            });
        }
        let directory = tempfile::tempdir()?;
        let node = directory
            .path()
            .join(if cfg!(windows) { "node.cmd" } else { "node" });
        std::fs::write(
            &node,
            if cfg!(windows) {
                "@echo off\r\necho v26.13.0\r\n"
            } else {
                "#!/bin/sh\necho v26.13.0\n"
            },
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(node, std::fs::Permissions::from_mode(0o755))?;
        }
        let output = smol::block_on(
            util::command::new_command(env::current_exe()?)
                .args([
                    "--exact",
                    "package_manager::tests::publishes_missing_manager_errors_and_clears_them_after_installation",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("PATH", directory.path())
                .env("PATHEXT", ".CMD;.EXE")
                .current_dir(directory.path())
                .output(),
        )?;
        ensure!(
            output.status.success(),
            "missing-manager notification failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires system Node.js, a package manager, and registry access"]
    fn installs_and_queries_tools_with_system_package_manager() -> Result<()> {
        const CHILD: &str = "ZED_TEST_PACKAGE_MANAGER_CHILD";
        if env::var_os(CHILD).is_none() {
            let output = smol::block_on(
                util::command::new_command(env::current_exe()?)
                    .args([
                        "--exact",
                        "package_manager::tests::installs_and_queries_tools_with_system_package_manager",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env(CHILD, "1")
                    .output(),
            )?;
            ensure!(
                output.status.success(),
                "tool installation failed:\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return Ok(());
        }
        smol::block_on(async {
            let manager = PackageManager::from_name(
                &env::var("ZED_TEST_PACKAGE_MANAGER").unwrap_or_else(|_| "pnpm".into()),
            )
            .context("unsupported test package manager")?;
            which::which(manager.name()).context("test package manager must be installed")?;
            which::which("node").context("test requires system Node.js")?;
            let directory = tempfile::tempdir()?;
            let data = directory.path().join("data");
            let project = directory.path().join("project");
            let package = directory.path().join("package");
            let tools = directory.path().join("tools");
            for path in [&data, &project, &package] {
                std::fs::create_dir_all(path)?;
            }
            std::fs::create_dir(project.join(".git"))?;
            let manifest = serde_json::to_string(
                &serde_json::json!({"packageManager": format!("{}@1.0.0", manager.name())}),
            )?;
            std::fs::write(project.join("package.json"), &manifest)?;
            std::fs::write(
                package.join("package.json"),
                r#"{"name":"zed-tool-install-test","version":"1.2.3","bin":"cli.js"}"#,
            )?;
            std::fs::write(package.join("cli.js"), "console.log('installed');\n")?;
            paths::set_custom_data_dir(&data.to_string_lossy());
            let node_runtime = NodeRuntime::new(
                None,
                watch::channel(Some(NodeBinaryOptions {
                    allow_path_lookup: true,
                }))
                .1,
            );
            let runtime = PackageManagerRuntime::new(
                node_runtime,
                Arc::new(http_client::BlockedHttpClient),
                watch::channel(Some(PackageManager::Auto)).1,
            )
            .for_project(project.as_path());
            assert_eq!(runtime.preferred_package_manager().await?, manager);
            assert_eq!(
                runtime
                    .package_manager_command(None, "info", &["is-number", "--json"])
                    .await?
                    .package_manager,
                manager
            );
            let version = format!("file:{}", package.to_string_lossy().replace('\\', "/"));
            runtime
                .package_manager_install_packages(&tools, &[("zed-tool-install-test", &version)])
                .await?;
            assert_eq!(
                runtime
                    .package_manager_installed_version(&tools, "zed-tool-install-test")
                    .await?,
                Some(semver::Version::new(1, 2, 3))
            );
            let executable =
                crate::read_package_executable(tools.join("node_modules"), "zed-tool-install-test")
                    .await?;
            assert!(executable.is_file());
            assert!(
                runtime
                    .package_manager_latest_version("is-number")
                    .await?
                    .major
                    > 0
            );
            assert_eq!(
                std::fs::read_to_string(project.join("package.json"))?,
                manifest
            );
            assert!(!project.join("node_modules").exists());
            Ok(())
        })
    }
    #[test]
    fn test_proxy_argument_map_localhost_proxy() {
        const CASES: [(&str, &str); 4] = [
            // Map localhost to 127.0.0.1
            ("http://localhost:9090/", "http://127.0.0.1:9090/"),
            ("https://google.com/", "https://google.com/"),
            (
                "http://username:password@proxy.thing.com:8080/",
                "http://username:password@proxy.thing.com:8080/",
            ),
            // Test when localhost is contained within a different part of the URL
            (
                "http://username:localhost@localhost:8080/",
                "http://username:localhost@127.0.0.1:8080/",
            ),
        ];

        for (proxy, mapped_proxy) in CASES {
            let proxy = Url::parse(proxy).unwrap();
            let proxy = proxy_argument(Some(&proxy)).expect("Proxy was not passed correctly");
            assert_eq!(
                proxy, mapped_proxy,
                "Incorrectly mapped localhost to 127.0.0.1"
            );
        }
    }

    #[test]
    fn test_build_npm_command_args_inserts_prefix_before_subcommand() {
        let args = build_npm_command_args(
            None,
            Some(Path::new("/tmp/zed-prefix")),
            Path::new("/tmp/cache"),
            None,
            None,
            None,
            "exec",
            &["--yes", "--", "agent-package"],
        );

        assert_eq!(
            args,
            vec![
                "--prefix".to_string(),
                "/tmp/zed-prefix".to_string(),
                "exec".to_string(),
                "--cache=/tmp/cache".to_string(),
                "--yes".to_string(),
                "--".to_string(),
                "agent-package".to_string(),
            ]
        );
    }

    #[test]
    fn test_build_npm_command_args_keeps_entrypoint_before_prefix() {
        let args = build_npm_command_args(
            Some(Path::new("/tmp/npm-cli.js")),
            Some(Path::new("/tmp/zed-prefix")),
            Path::new("/tmp/cache"),
            None,
            None,
            None,
            "exec",
            &["--yes"],
        );

        assert_eq!(
            args,
            vec![
                "/tmp/npm-cli.js".to_string(),
                "--prefix".to_string(),
                "/tmp/zed-prefix".to_string(),
                "exec".to_string(),
                "--cache=/tmp/cache".to_string(),
                "--yes".to_string(),
            ]
        );
    }

    #[test]
    fn test_latest_version_strategy_accepts_newer_installed_versions() -> Result<()> {
        let target_version = Version::parse("2.0.0")?;

        assert!(!should_install_package_version(
            &Version::parse("2.0.0")?,
            VersionStrategy::Latest(&target_version)
        ));
        assert!(should_install_package_version(
            &Version::parse("1.0.0")?,
            VersionStrategy::Latest(&target_version)
        ));
        assert!(!should_install_package_version(
            &Version::parse("3.0.0")?,
            VersionStrategy::Latest(&target_version)
        ));

        Ok(())
    }

    #[test]
    fn test_select_package_version_uses_dist_tag_without_before() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "3.0.0" },
                "versions": ["1.0.0", "2.0.0", "3.0.0"],
                "time": {
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0": "2024-02-01T00:00:00.000Z",
                    "3.0.0": "2024-03-01T00:00:00.000Z"
                }
            }"#,
        )?;

        assert_eq!(
            select_package_version("test-package", info, None, None)?,
            Version::parse("3.0.0")?
        );
        Ok(())
    }

    #[test]
    fn test_npm_info_skips_non_string_time_entries() -> Result<()> {
        // Registries such as JFrog Artifactory include `"unpublished": null` in `time`;
        // parsing must tolerate this rather than rejecting the whole response.
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "2.0.0" },
                "versions": ["1.0.0", "2.0.0"],
                "time": {
                    "unpublished": null,
                    "created": "2024-01-01T00:00:00.000Z",
                    "modified": "2024-02-01T00:00:00.000Z",
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0": "2024-02-01T00:00:00.000Z"
                }
            }"#,
        )?;

        assert_eq!(
            select_package_version("test-package", info, Some("2024-02-15T00:00:00.000Z"), None)?,
            Version::parse("2.0.0")?
        );
        Ok(())
    }

    #[test]
    fn test_select_package_version_uses_latest_before_npm_before_config() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "3.0.0" },
                "versions": ["1.0.0", "2.0.0", "3.0.0"],
                "time": {
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0": "2024-02-01T00:00:00.000Z",
                    "3.0.0": "2024-03-01T00:00:00.000Z"
                }
            }"#,
        )?;

        assert_eq!(
            select_package_version("test-package", info, Some("2024-02-15T00:00:00.000Z"), None)?,
            Version::parse("2.0.0")?
        );
        Ok(())
    }

    #[test]
    fn test_select_package_version_keeps_allowed_latest_dist_tag() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "2.0.0" },
                "versions": ["1.0.0", "2.0.0", "3.0.0"],
                "time": {
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0": "2024-02-01T00:00:00.000Z",
                    "3.0.0": "2024-03-01T00:00:00.000Z"
                }
            }"#,
        )?;

        assert_eq!(
            select_package_version("test-package", info, Some("2024-02-15T00:00:00.000Z"), None)?,
            Version::parse("2.0.0")?
        );
        Ok(())
    }

    #[test]
    fn test_select_package_version_keeps_allowed_prerelease_latest_dist_tag() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "2.0.0-beta.1" },
                "versions": ["1.0.0", "2.0.0-beta.1"],
                "time": {
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0-beta.1": "2024-02-01T00:00:00.000Z"
                }
            }"#,
        )?;

        assert_eq!(
            select_package_version("test-package", info, Some("2024-02-15T00:00:00.000Z"), None)?,
            Version::parse("2.0.0-beta.1")?
        );
        Ok(())
    }

    #[test]
    fn test_select_package_version_ignores_prereleases_before_cutoff() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "2.0.0" },
                "versions": ["1.0.0", "2.0.0-beta.1", "2.0.0"],
                "time": {
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0-beta.1": "2024-02-01T00:00:00.000Z",
                    "2.0.0": "2024-03-01T00:00:00.000Z"
                }
            }"#,
        )?;

        assert_eq!(
            select_package_version("test-package", info, Some("2024-02-15T00:00:00.000Z"), None)?,
            Version::parse("1.0.0")?
        );
        Ok(())
    }

    #[test]
    fn test_select_package_version_ignores_versions_above_latest_dist_tag() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "2.0.0" },
                "versions": ["1.0.0", "2.0.0", "3.0.0"],
                "time": {
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0": "2024-03-01T00:00:00.000Z",
                    "3.0.0": "2024-02-01T00:00:00.000Z"
                }
            }"#,
        )?;

        assert_eq!(
            select_package_version("test-package", info, Some("2024-02-15T00:00:00.000Z"), None)?,
            Version::parse("1.0.0")?
        );
        Ok(())
    }

    #[test]
    fn test_select_package_version_errors_when_no_version_matches_before() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "2.0.0" },
                "versions": ["1.0.0", "2.0.0"],
                "time": {
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0": "2024-02-01T00:00:00.000Z"
                }
            }"#,
        )?;

        let Err(error) =
            select_package_version("test-package", info, Some("2023-12-01T00:00:00.000Z"), None)
        else {
            bail!("expected cutoff to reject all package versions");
        };
        assert_eq!(
            error.to_string(),
            "no version found for package test-package before 2023-12-01T00:00:00.000Z"
        );
        Ok(())
    }

    #[test]
    fn test_select_package_version_selects_latest_matching_requirement() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "7.0.0" },
                "versions": ["6.0.3", "7.0.0", "5.9.3", "6.0.2"]
            }"#,
        )?;
        let version_requirement = VersionReq::parse("^6")?;

        assert_eq!(
            select_package_version("test-package", info, None, Some(&version_requirement))?,
            Version::parse("6.0.3")?
        );
        Ok(())
    }

    #[test]
    fn test_select_package_version_applies_before_to_matching_versions() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "7.0.0" },
                "versions": ["6.0.3", "7.0.0", "6.0.2"],
                "time": {
                    "6.0.2": "2024-02-01T00:00:00.000Z",
                    "6.0.3": "2024-03-01T00:00:00.000Z",
                    "7.0.0": "2024-04-01T00:00:00.000Z"
                }
            }"#,
        )?;
        let version_requirement = VersionReq::parse("^6")?;

        assert_eq!(
            select_package_version(
                "test-package",
                info,
                Some("2024-02-15T00:00:00.000Z"),
                Some(&version_requirement),
            )?,
            Version::parse("6.0.2")?
        );
        Ok(())
    }

    #[test]
    fn test_select_package_version_allows_requested_prerelease_before_cutoff() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "7.0.0" },
                "versions": ["7.1.0-beta.1", "7.1.0-beta.2", "7.0.0"],
                "time": {
                    "7.0.0": "2024-01-01T00:00:00.000Z",
                    "7.1.0-beta.1": "2024-02-01T00:00:00.000Z",
                    "7.1.0-beta.2": "2024-03-01T00:00:00.000Z"
                }
            }"#,
        )?;
        let version_requirement = VersionReq::parse(">=7.1.0-beta.1, <7.1.0")?;

        assert_eq!(
            select_package_version(
                "test-package",
                info,
                Some("2024-02-15T00:00:00.000Z"),
                Some(&version_requirement),
            )?,
            Version::parse("7.1.0-beta.1")?
        );
        Ok(())
    }

    #[test]
    fn test_select_package_version_errors_without_matching_version() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "7.0.0" },
                "versions": ["5.9.3", "7.0.0"]
            }"#,
        )?;
        let version_requirement = VersionReq::parse("^6")?;

        let error = select_package_version("test-package", info, None, Some(&version_requirement))
            .expect_err("expected version requirement to reject all package versions");
        assert_eq!(
            error.to_string(),
            "no version found for package test-package"
        );
        Ok(())
    }

    #[test]
    fn test_pinned_version_strategy_replaces_different_installed_version() -> Result<()> {
        let pinned_version = Version::parse("6.0.3")?;

        assert!(!should_install_package_version(
            &pinned_version,
            VersionStrategy::Pin(&pinned_version)
        ));
        assert!(should_install_package_version(
            &Version::parse("7.0.0")?,
            VersionStrategy::Pin(&pinned_version)
        ));
        Ok(())
    }

    #[test]
    fn test_deserialize_npm_info_npm11_format() -> Result<()> {
        let json = r#"{
            "dist-tags": { "latest": "3.0.0" },
            "versions": ["1.0.0", "2.0.0", "3.0.0"]
        }"#;

        let info = deserialize_package_info_from_response(json.as_bytes())?;
        assert_eq!(info.dist_tags.latest, Some(Version::parse("3.0.0")?));
        assert_eq!(
            info.versions,
            vec![
                Version::parse("1.0.0")?,
                Version::parse("2.0.0")?,
                Version::parse("3.0.0")?
            ]
        );
        Ok(())
    }

    #[test]
    fn test_deserialize_npm_v12_format() -> Result<()> {
        let json = r#"[
            {
                "dist-tags": { "latest": "3.0.0" },
                "versions": ["1.0.0", "2.0.0", "3.0.0"]
            }
        ]"#;

        let info = deserialize_package_info_from_response(json.as_bytes())?;
        assert_eq!(info.dist_tags.latest, Some(Version::parse("3.0.0")?));
        assert_eq!(
            info.versions,
            vec![
                Version::parse("1.0.0")?,
                Version::parse("2.0.0")?,
                Version::parse("3.0.0")?
            ]
        );
        Ok(())
    }
}
