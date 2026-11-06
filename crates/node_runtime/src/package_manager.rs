mod command;
mod detection;
mod environment;
mod executable;
mod install;
mod installed_packages;
mod registry;
mod user_path;

pub use command::PackageManagerInvocation;
pub use detection::{detect_package_manager, detect_package_manager_on_disk};
pub use environment::{node_command_env, project_tool_environment};
pub use install::PackageManagerInstallEvent;
pub use installed_packages::{
    find_installed_package, read_package_executable, read_package_installed_version,
};
pub use registry::{PackageDistTags, PackageInfo, VersionStrategy};
pub use user_path::{add_installation_to_user_path, installation_in_user_path};

use crate::NodeRuntime;
use anyhow::{Context as _, Result, bail, ensure};
use command::{ToolCommand, run_tool_command};
use environment::without_corepack_downloads;
pub(crate) use environment::{path_variable, set_path_variable};
use executable::{
    AvailablePackageManager, PackageManagerUnavailable, append_package_manager_fallbacks,
    find_package_manager,
};
use http_client::HttpClient;
use semver::{Version, VersionReq};
use serde_json::Value;
use smol::{fs, lock::Mutex};
use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
    process::Output,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use util::ResultExt as _;

type InstallationEvents = futures::channel::mpsc::UnboundedSender<PackageManagerInstallEvent>;

#[derive(Clone)]
pub struct PackageManagerRuntime {
    node_runtime: NodeRuntime,
    http: Arc<dyn HttpClient>,
    state: Arc<Mutex<PackageManagerState>>,
    errors: watch::Receiver<Option<Arc<String>>>,
    project_directory: Option<Arc<Path>>,
    environment: Option<Arc<HashMap<String, String>>>,
    installation_events: Option<InstallationEvents>,
    installation_lock: Arc<Mutex<()>>,
    /// Counts cancelled selection prompts. Requests that queued behind a prompt
    /// the user cancelled are cancelled with it instead of prompting again.
    cancelled_selections: Arc<AtomicU64>,
}

struct PackageManagerState {
    preference: watch::Receiver<Option<PackageManager>>,
    errors: watch::Sender<Option<Arc<String>>>,
    last_error: Option<Arc<String>>,
}

/// What a resolved package manager is used for, which decides how it is chosen.
#[derive(Clone, Copy)]
enum Purpose {
    /// A project task: an explicit or declared manager must be used as is.
    Task(Option<PackageManager>),
    /// Installing agents and language servers: any installed manager will do under `auto`.
    Tool,
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
            environment: None,
            installation_events: None,
            installation_lock: Arc::default(),
            cancelled_selections: Arc::default(),
        }
    }

    pub fn unavailable() -> Self {
        Self::new(
            NodeRuntime::unavailable(),
            Arc::new(http_client::BlockedHttpClient),
            watch::channel(Some(PackageManager::Auto)).1,
        )
    }

    pub fn with_installation_events(mut self, events: InstallationEvents) -> Self {
        self.installation_events = Some(events);
        self
    }

    pub fn node_runtime(&self) -> &NodeRuntime {
        &self.node_runtime
    }

    pub fn errors(&self) -> watch::Receiver<Option<Arc<String>>> {
        self.errors.clone()
    }

    pub fn for_project(&self, directory: impl AsRef<Path>) -> Self {
        let mut runtime = self.clone();
        runtime.project_directory = Some(Arc::from(directory.as_ref()));
        runtime
    }

    /// Uses `environment` to resolve and run package managers.
    pub fn with_environment(&self, environment: HashMap<String, String>) -> Self {
        let mut runtime = self.clone();
        runtime.environment = Some(Arc::new(environment));
        runtime
    }

    /// The absolute path of a supported Node.js, for processes Zed starts itself.
    pub async fn binary_path(&self) -> Result<PathBuf> {
        self.node_runtime.binary_path().await
    }

    pub async fn package_manager_preference(&self) -> Result<PackageManager> {
        let mut state = self.state.lock().await;
        loop {
            if let Some(preference) = *state.preference.borrow() {
                return Ok(preference);
            }
            state.preference.changed().await?;
        }
    }

    fn base_environment(&self) -> HashMap<String, String> {
        self.environment
            .as_deref()
            .cloned()
            .unwrap_or_else(|| env::vars().collect())
    }
}

impl PackageManagerRuntime {
    /// Finds the manager for `purpose`, installing it when it is missing.
    ///
    /// With a target (a concrete setting, or a task's declared manager) only that manager is
    /// accepted: it is installed through any installed manager, or downloaded when none is
    /// installed. Without one, tools use any installed manager and download pnpm when none is
    /// installed, while tasks ask which manager to download.
    async fn select_package_manager(
        &self,
        purpose: Purpose,
        directory: &Path,
        environment: &mut HashMap<String, String>,
    ) -> Result<AvailablePackageManager> {
        let target = match purpose {
            Purpose::Task(target) => target,
            Purpose::Tool => Some(self.package_manager_preference().await?)
                .filter(|preference| *preference != PackageManager::Auto),
        };
        let candidates = target.map_or_else(
            || PackageManager::INSTALLABLE.to_vec(),
            |manager| vec![manager],
        );
        if let Ok(available) = find_package_manager(&candidates, directory, environment).await {
            return Ok(available);
        }
        let Some(events) = &self.installation_events else {
            if matches!(purpose, Purpose::Task(_)) {
                return find_package_manager(&candidates, directory, environment).await;
            }
            // Without an installation channel nothing may be installed, so any manager will do.
            let mut fallbacks = candidates;
            append_package_manager_fallbacks(&mut fallbacks);
            let available = find_package_manager(&fallbacks, directory, environment).await?;
            log::warn!(
                "selected package manager {} is unavailable; using {} in {}",
                target.unwrap_or_default().name(),
                available.manager.name(),
                directory.display()
            );
            return Ok(available);
        };
        let cancelled_selections = self.cancelled_selections.load(Ordering::SeqCst);
        let _guard = self.installation_lock.lock().await;
        *environment = project_tool_environment(directory, environment.clone()).await?;
        // Another request may have installed a manager while this one waited for the lock.
        if let Ok(available) = find_package_manager(&candidates, directory, environment).await {
            return Ok(available);
        }
        let installer = find_package_manager(&PackageManager::INSTALLABLE, directory, environment)
            .await
            .ok();
        let manager = match (target, purpose) {
            (Some(manager), _) => manager,
            (None, Purpose::Tool) => PackageManager::Pnpm,
            (None, Purpose::Task(_)) => {
                if self.cancelled_selections.load(Ordering::SeqCst) != cancelled_selections {
                    bail!("package manager installation cancelled");
                }
                let (response, selection) = futures::channel::oneshot::channel();
                events
                    .unbounded_send(PackageManagerInstallEvent::Select { response })
                    .context("package manager installation prompt is unavailable")?;
                let selected = selection
                    .await
                    .context("package manager installation prompt closed")?;
                match selected {
                    Some(manager) => manager,
                    None => {
                        self.cancelled_selections.fetch_add(1, Ordering::SeqCst);
                        bail!("package manager installation cancelled");
                    }
                }
            }
        };
        // Without a target an installed manager would already have been used above.
        let installer = installer.filter(|_| target.is_some());
        self.install_package_manager(events, manager, installer, directory, environment)
            .await
    }

    async fn install_package_manager(
        &self,
        events: &InstallationEvents,
        manager: PackageManager,
        installer: Option<AvailablePackageManager>,
        directory: &Path,
        environment: &mut HashMap<String, String>,
    ) -> Result<AvailablePackageManager> {
        ensure!(
            manager != PackageManager::Auto,
            "select a concrete package manager to install"
        );
        let installed_via_manager =
            installer.is_some() || matches!(manager, PackageManager::Npm | PackageManager::Yarn);
        let directories = if let Some(installer) = installer {
            install::install_with_manager(
                installer.manager,
                installer.major_version,
                &installer.path,
                manager,
                directory,
                environment,
            )
            .await?
        } else {
            install::install_standalone(
                self.http.as_ref(),
                &self.node_runtime,
                manager,
                directory,
                environment,
            )
            .await?
        };
        let mut paths = directories;
        if let Some(path) = path_variable(environment) {
            paths.extend(env::split_paths(path));
        }
        set_path_variable(
            environment,
            env::join_paths(paths)?.to_string_lossy().into_owned(),
        );
        *environment = project_tool_environment(directory, environment.clone()).await?;
        let available = find_package_manager(&[manager], directory, environment)
            .await
            .context("installation finished but the package manager is not usable")?;
        let installed_directory = available
            .path
            .parent()
            .context("installed package manager has no parent directory")?
            .to_path_buf();
        install::register_directory(&installed_directory).await?;
        events
            .unbounded_send(PackageManagerInstallEvent::Installed {
                manager,
                directory: installed_directory,
                installed_via_manager,
            })
            .log_err();
        Ok(available)
    }
}

impl PackageManagerRuntime {
    /// Resolves the manager for a project task and translates its arguments.
    ///
    /// Under `auto` the project's declared manager is required; when nothing is declared any
    /// installed manager is used, and the user is asked which one to download when none is.
    pub async fn task_command(
        &self,
        directory: &Path,
        arguments: &[String],
        environment: HashMap<String, String>,
        preference: PackageManager,
        sources: &[PathBuf],
    ) -> Result<PackageManagerInvocation> {
        let declared = if preference == PackageManager::Auto {
            detection::cached_project_package_manager(directory, sources).await?
        } else {
            preference
        };
        let target = Some(declared).filter(|manager| *manager != PackageManager::Auto);
        let mut probe_environment = without_corepack_downloads(
            project_tool_environment(directory, environment.clone()).await?,
        );
        let available = self
            .select_package_manager(Purpose::Task(target), directory, &mut probe_environment)
            .await
            .with_context(|| match target {
                Some(manager) => format!(
                    "This task requires {}. Install it manually and retry, or change the Package Manager setting. https://pnpm.io/installation",
                    manager.name()
                ),
                None => "Could not prepare a package manager for this task".into(),
            })?;
        let manager = available.manager;
        let (operation, arguments) = arguments
            .split_first()
            .context("missing package manager task operation")?;
        let mut args = match operation.as_str() {
            "run" => vec!["run".into()],
            "exec" => match manager {
                PackageManager::Bun => vec!["run".into()],
                PackageManager::Npm => vec!["exec".into(), "--".into()],
                _ => vec!["exec".into()],
            },
            _ => bail!("Unsupported package manager task operation: {operation}"),
        };
        let arguments = match arguments.split_first() {
            Some((delimiter, rest)) if operation == "exec" && delimiter == "--" => rest,
            _ => arguments,
        };
        args.extend_from_slice(arguments);
        log::debug!("Running {} task in {}", manager.name(), directory.display());
        // A manager installed during selection lives in a registered directory, which
        // `project_tool_environment` puts on `PATH` for the receiving shell as well.
        Ok(PackageManagerInvocation {
            package_manager: manager,
            program: manager.name().into(),
            args,
            env: project_tool_environment(directory, environment).await?,
            current_dir: directory.to_path_buf(),
        })
    }

    pub(crate) async fn package_manager_command(
        &self,
        directory: Option<&Path>,
        subcommand: &str,
        arguments: &[&str],
    ) -> Result<ToolCommand> {
        let result = self
            .build_tool_command(directory, subcommand, arguments)
            .await;
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

    async fn build_tool_command(
        &self,
        directory: Option<&Path>,
        subcommand: &str,
        arguments: &[&str],
    ) -> Result<ToolCommand> {
        let runtime_directory = paths::data_dir().join("package_manager");
        let directory = directory
            .map(Path::to_path_buf)
            .unwrap_or_else(|| runtime_directory.join("package-manager-work"));
        fs::create_dir_all(&directory)
            .await
            .with_context(|| format!("creating tool directory {}", directory.display()))?;
        let mut environment = without_corepack_downloads(
            project_tool_environment(
                self.project_directory.as_deref().unwrap_or(&directory),
                self.base_environment(),
            )
            .await?,
        );
        let proxy = self.http.proxy();
        if let Some(proxy) = command::proxy_argument(proxy) {
            environment.insert("HTTP_PROXY".into(), proxy.clone());
            environment.insert("HTTPS_PROXY".into(), proxy.clone());
            environment.insert("npm_config_proxy".into(), proxy);
        }
        let available = self
            .select_package_manager(Purpose::Tool, &directory, &mut environment)
            .await?;

        if matches!(subcommand, "install" | "info") {
            command::create_tool_manifest(&directory).await?;
            if available.manager == PackageManager::Yarn && available.major_version >= 2 {
                command::create_if_missing(&directory.join("yarn.lock"), "").await?;
            }
        }
        let cache = runtime_directory.join("cache");
        let args = command::tool_arguments(
            available.manager,
            available.major_version,
            subcommand,
            arguments,
            &directory,
            &cache,
            proxy,
        )?;
        command::apply_tool_environment(
            available.manager,
            available.major_version,
            &cache,
            &mut environment,
        );
        Ok(ToolCommand {
            package_manager: available.manager,
            executable: available.path,
            args,
            env: environment,
            current_dir: directory,
        })
    }

    pub async fn run_package_manager_subcommand(
        &self,
        directory: Option<&Path>,
        subcommand: &str,
        arguments: &[&str],
    ) -> Result<Output> {
        let command = self
            .package_manager_command(directory, subcommand, arguments)
            .await?;
        run_tool_command(command).await
    }

    async fn npm_config_before(&self) -> Result<Option<String>> {
        // `npm config get before` renders Date values for display. The JSON config output keeps
        // the computed cutoff in the same ISO format used by `npm info --json` release times.
        let output = self
            .run_package_manager_subcommand(None, "config", &["list", "--json"])
            .await?;
        registry::parse_npm_config_before(&output.stdout)
    }
}

impl PackageManagerRuntime {
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
        let output = run_tool_command(command).await?;

        let info: PackageInfo = registry::deserialize_package_info_from_response(&output.stdout)
            .map_err(|e| {
                anyhow::anyhow!(
                    "failed to parse {} package info response: {e}\nstdout: {}",
                    manager.name(),
                    String::from_utf8_lossy(&output.stdout)
                )
            })?;
        let before = if manager == PackageManager::Npm {
            self.npm_config_before()
                .await
                .context("getting npm before config")
                .log_err()
                .flatten()
        } else {
            None
        };
        let latest_dist_tag = info.dist_tags.latest.clone();
        let selected_version =
            registry::select_package_version(name, info, before.as_deref(), version_requirement)?;
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
        run_tool_command(command).await?;
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
        let should_install =
            registry::should_install_package_version(&installed_version, version_strategy);
        log::debug!(
            "package cache check package={package_name:?} installed={installed_version} strategy={version_strategy_label} should_install={should_install}"
        );
        should_install
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum PackageManager {
    #[default]
    Auto,
    Pnpm,
    Vp,
    Bun,
    Npm,
    Yarn,
}

impl PackageManager {
    pub const INSTALLABLE: [Self; 5] = [Self::Pnpm, Self::Vp, Self::Bun, Self::Npm, Self::Yarn];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Pnpm => "pnpm",
            Self::Vp => "vp",
            Self::Bun => "bun",
            Self::Npm => "npm",
            Self::Yarn => "yarn",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.split('@').next()? {
            "pnpm" => Some(Self::Pnpm),
            "vp" | "vite-plus" => Some(Self::Vp),
            "bun" => Some(Self::Bun),
            "npm" => Some(Self::Npm),
            "yarn" => Some(Self::Yarn),
            _ => None,
        }
    }

    pub fn from_package_json(package: &Value) -> Option<Self> {
        package
            .get("packageManager")
            .and_then(Value::as_str)
            .and_then(Self::from_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let (first_manager, second_manager) = futures::try_join!(
                detect_package_manager_on_disk(&first),
                detect_package_manager_on_disk(&second)
            )?;
            assert_eq!(first_manager, PackageManager::Pnpm);
            assert_eq!(second_manager, PackageManager::Bun);
            assert_eq!(
                runtime.package_manager_preference().await?,
                PackageManager::Auto
            );
            sender.send(Some(PackageManager::Yarn))?;
            assert_eq!(
                runtime
                    .for_project(&first)
                    .package_manager_preference()
                    .await?,
                PackageManager::Yarn
            );
            Ok(())
        })
    }
}
