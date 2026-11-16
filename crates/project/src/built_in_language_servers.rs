use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result, bail, ensure};
use async_trait::async_trait;
use collections::{BTreeMap, HashMap, HashSet};
use fs::{Fs, RemoveOptions};
use gpui::{
    App, AppContext as _, BackgroundExecutor, Context, Entity, EventEmitter, Global, Task,
    TaskExt as _, WeakEntity,
};
use http_client::HttpClient;
use language::{
    BinaryStatus, CachedLspAdapter, LanguageRegistry, LanguageServerName, LanguageServerSourceKind,
    LspAdapter, LspAdapterDelegate, language_server_pinned_version,
    language_server_uninstalled_marker,
};
use lsp::{LanguageServerBinary, LanguageServerSelector};
use node_runtime::PackageManagerRuntime;
use semver::{Version, VersionReq};
use settings::WorktreeId;
use util::{ResultExt as _, rel_path::RelPath};

use crate::{Project, lsp_store::LspStore};

#[derive(Default)]
pub(crate) struct LocalLanguageServerStores(pub Vec<WeakEntity<LspStore>>);
impl Global for LocalLanguageServerStores {}

struct GlobalBuiltInLanguageServers(Entity<BuiltInLanguageServers>);
impl Global for GlobalBuiltInLanguageServers {}

#[derive(Default)]
pub struct BuiltInServerState {
    pub checked: bool,
    pub installed: bool,
    pub busy: bool,
    pub version: Option<String>,
    pub description: Option<String>,
    pub author: Option<String>,
    pub homepage: Option<String>,
    pub versions: Vec<Version>,
    pub metadata_error: Option<String>,
    pub error: Option<String>,
}

pub enum BuiltInServerEvent {
    OperationFailed(String),
}

enum BuiltInServerOperation {
    Install { version: Option<Version> },
    Uninstall,
}

pub struct BuiltInLanguageServers {
    registry: Arc<LanguageRegistry>,
    node: Option<PackageManagerRuntime>,
    delegate: Arc<dyn LspAdapterDelegate>,
    fs: Arc<dyn Fs>,
    servers: BTreeMap<LanguageServerName, BuiltInServerState>,
    refresh_task: Option<Task<()>>,
}

impl EventEmitter<BuiltInServerEvent> for BuiltInLanguageServers {}

impl BuiltInLanguageServers {
    pub fn global(project: &Project, cx: &mut App) -> Entity<Self> {
        if let Some(entity) = cx
            .try_global::<GlobalBuiltInLanguageServers>()
            .map(|global| global.0.clone())
        {
            entity.update(cx, |store, cx| {
                if store.node.is_none() && project.node_runtime().is_some() {
                    store.node = project.node_runtime().cloned();
                }
                store.refresh(cx);
            });
            return entity;
        }
        let registry = project.languages().clone();
        let node = project.node_runtime().cloned();
        let fs = project.fs().clone();
        let http_client = project.client().http_client();
        let entity = cx.new(|cx| {
            let delegate = Arc::new(ManagedServerDelegate {
                registry: registry.clone(),
                http_client,
                background_executor: cx.background_executor().clone(),
                status_source_id: cx.entity_id(),
                root: paths::languages_dir().clone(),
            });
            let mut this = Self {
                registry,
                node,
                delegate,
                fs,
                servers: BTreeMap::new(),
                refresh_task: None,
            };
            this.refresh(cx);
            this
        });
        cx.set_global(GlobalBuiltInLanguageServers(entity.clone()));
        entity
    }

    pub fn state(&self, name: &LanguageServerName) -> Option<&BuiltInServerState> {
        self.servers.get(name)
    }

    pub fn fetch_versions(
        &self,
        name: LanguageServerName,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<Version>>> {
        let adapter = self.registry.adapter_for_name(&name);
        let node = self.node.clone();
        cx.spawn(async move |this, cx| {
            let source = adapter
                .and_then(|adapter| adapter.adapter.server_source())
                .context("language server has no managed source")?;
            let LanguageServerSourceKind::PackageManager {
                package,
                version_requirement,
            } = source.kind
            else {
                bail!("this language server is not installed from a package registry");
            };
            let metadata = node
                .context("package manager is unavailable in this workspace")?
                .package_manager_package_info(package)
                .await?;
            let mut versions = metadata.versions.clone();
            if let Some(requirement) = version_requirement {
                let requirement = VersionReq::parse(requirement)?;
                versions.retain(|version| requirement.matches(version));
            }
            versions.sort_unstable_by(|left, right| right.cmp(left));
            versions.dedup();
            this.update(cx, |this, cx| {
                if let Some(state) = this.servers.get_mut(&name) {
                    state.description = metadata.description.clone();
                    state.author = metadata.author_info();
                    state.homepage = metadata.homepage.filter(|url| valid_homepage(url));
                    state.versions = versions.clone();
                    state.metadata_error = None;
                }
                cx.notify();
            })?;
            Ok(versions)
        })
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let adapters = self
            .registry
            .all_lsp_adapters()
            .into_iter()
            .filter(|adapter| adapter.adapter.server_source().is_some())
            .collect::<Vec<_>>();
        for adapter in &adapters {
            self.servers.entry(adapter.name.clone()).or_default();
        }
        let registry = self.registry.clone();
        let delegate = self.delegate.clone();
        let node = self.node.clone();
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let result: Result<()> = async {
                for adapter in &adapters {
                    let binary_cache = adapter.lock_binary().await;
                    let Some(directory) = registry.language_server_download_dir(&adapter.name)
                    else {
                        continue;
                    };
                    let installed = directory.is_dir()
                        && !language_server_uninstalled_marker(&directory).is_file()
                        && adapter
                            .adapter
                            .managed_cached_binary(directory.to_path_buf(), delegate.as_ref())
                            .await
                            .is_some();
                    let version = if let Some(node) = &node
                        && let Some(source) = adapter.adapter.server_source()
                        && let LanguageServerSourceKind::PackageManager { package, .. } =
                            source.kind
                    {
                        node.package_manager_installed_version(&directory, package)
                            .await
                            .log_err()
                            .flatten()
                            .map(|version| version.to_string())
                    } else {
                        None
                    };
                    this.update(cx, |this, cx| {
                        if let Some(state) = this.servers.get_mut(&adapter.name) {
                            if !state.busy {
                                state.checked = true;
                                state.installed = installed;
                                state.version = version;
                            }
                        }
                        cx.notify();
                    })?;
                    drop(binary_cache);
                }
                if let Some(node) = node {
                    let mut packages = BTreeMap::<&str, Vec<Arc<CachedLspAdapter>>>::new();
                    for adapter in adapters {
                        if let Some(source) = adapter.adapter.server_source()
                            && let LanguageServerSourceKind::PackageManager { package, .. } =
                                source.kind
                        {
                            packages.entry(package).or_default().push(adapter);
                        }
                    }
                    for (package, adapters) in packages {
                        let metadata = node.package_manager_package_info(package).await;
                        this.update(cx, |this, cx| {
                            for adapter in adapters {
                                let Some(state) = this.servers.get_mut(&adapter.name) else {
                                    continue;
                                };
                                match &metadata {
                                    Ok(metadata) => {
                                        state.metadata_error = None;
                                        state.description = metadata
                                            .description
                                            .clone()
                                            .filter(|value| !value.trim().is_empty());
                                        state.author = metadata.author_info();
                                        state.homepage = metadata
                                            .homepage
                                            .clone()
                                            .filter(|url| valid_homepage(url));
                                        state.versions = metadata.versions.clone();
                                        if let Some(source) = adapter.adapter.server_source()
                                            && let LanguageServerSourceKind::PackageManager {
                                                version_requirement: Some(requirement),
                                                ..
                                            } = source.kind
                                        {
                                            match VersionReq::parse(requirement) {
                                                Ok(requirement) => state
                                                    .versions
                                                    .retain(|version| requirement.matches(version)),
                                                Err(error) => {
                                                    state.versions.clear();
                                                    state.metadata_error = Some(error.to_string());
                                                }
                                            }
                                        }
                                        state
                                            .versions
                                            .sort_unstable_by(|left, right| right.cmp(left));
                                        state.versions.dedup();
                                    }
                                    Err(error) => {
                                        log::error!("Fetching metadata for {package}: {error:#}");
                                        state.metadata_error = Some(format!("{error:#}"));
                                    }
                                }
                            }
                            cx.notify();
                        })?;
                    }
                }
                anyhow::Ok(())
            }
            .await;
            result.log_err();
        }));
    }

    pub fn install(
        &mut self,
        name: LanguageServerName,
        version: Option<Version>,
        cx: &mut Context<Self>,
    ) {
        self.change_installation(name, BuiltInServerOperation::Install { version }, cx);
    }

    pub fn uninstall(&mut self, name: LanguageServerName, cx: &mut Context<Self>) {
        self.change_installation(name, BuiltInServerOperation::Uninstall, cx);
    }

    fn change_installation(
        &mut self,
        name: LanguageServerName,
        operation: BuiltInServerOperation,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.servers.get_mut(&name) else {
            return;
        };
        if state.busy {
            return;
        }
        state.busy = true;
        state.error = None;
        let registry = self.registry.clone();
        let delegate = self.delegate.clone();
        let node = self.node.clone();
        let fs = self.fs.clone();
        let stores = cx.default_global::<LocalLanguageServerStores>().0.clone();
        let installing = matches!(operation, BuiltInServerOperation::Install { .. });
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result: Result<Option<String>> = async {
                let adapter = registry.adapter_for_name(&name).context("language server is no longer registered")?;
                let source = adapter.adapter.server_source().context("this language server has no managed source")?;
                let directory = registry.language_server_download_dir(&name).context("no language server download directory")?;
                ensure!(directory.parent() == Some(paths::languages_dir().as_path()), "refusing to manage a language server outside Zed's language directory");
                ensure!(!fs.metadata(&directory).await?.is_some_and(|metadata| metadata.is_symlink), "refusing to manage a symlinked language server directory");
                fs.create_dir(paths::languages_dir()).await?;
                let uninstalled = language_server_uninstalled_marker(&directory);
                // Block new starts before stopping processes and replacing their managed files.
                fs.atomic_write(uninstalled.clone(), String::new()).await?;
                let shutdowns = cx.update(|cx| {
                    stores.iter().filter_map(WeakEntity::upgrade).map(|store| {
                        store.update(cx, |store, cx| store.stop_language_servers_for_buffers(
                            Vec::new(), HashSet::from_iter([LanguageServerSelector::Name(name.clone())]), cx))
                    }).collect::<Vec<_>>()
                });
                for shutdown in shutdowns { shutdown.await?; }
                let mut binary_cache = adapter.lock_binary().await;
                *binary_cache = None;
                let pinned = language_server_pinned_version(&directory);
                let installed_version = if let BuiltInServerOperation::Install { version } = &operation {
                    fs.create_dir(&directory).await?;
                    let binary = if let Some(version) = version {
                        let LanguageServerSourceKind::PackageManager { package, version_requirement } = source.kind else {
                            bail!("this language server does not support selecting a package version");
                        };
                        if let Some(requirement) = version_requirement {
                            ensure!(VersionReq::parse(requirement)?.matches(version), "this version is incompatible with the language server adapter");
                        }
                        let node = node.as_ref().context("package manager is unavailable in this workspace")?;
                        node.package_manager_install_packages(&directory, &[(package, &version.to_string())]).await?;
                        adapter.adapter.managed_cached_binary(directory.to_path_buf(), delegate.as_ref()).await.context("the selected package does not contain a supported language server executable")?
                    } else {
                        adapter.adapter.try_fetch_server_binary(&delegate, directory.to_path_buf(), false, cx).await?
                    };
                    ensure!(adapter.adapter.managed_cached_binary(directory.to_path_buf(), delegate.as_ref()).await.is_some(), "installation did not produce a supported language server executable");
                    if let Some(version) = version {
                        fs.atomic_write(pinned, version.to_string()).await?;
                    } else {
                        fs.remove_file(&pinned, RemoveOptions { ignore_if_not_exists: true, ..Default::default() }).await?;
                    }
                    fs.remove_file(&uninstalled, RemoveOptions { ignore_if_not_exists: true, ..Default::default() }).await?;
                    *binary_cache = Some((false, binary));
                    if let LanguageServerSourceKind::PackageManager { package, .. } = source.kind {
                        node.as_ref().context("package manager is unavailable")?.package_manager_installed_version(&directory, package).await?.map(|version| version.to_string())
                    } else { None }
                } else {
                    fs.remove_dir(&directory, RemoveOptions { recursive: true, ignore_if_not_exists: true }).await?;
                    None
                };
                drop(binary_cache);
                if installing {
                    cx.update(|cx| {
                        for store in stores.iter().filter_map(WeakEntity::upgrade) {
                            store.update(cx, |store, cx| store.restart_managed_language_server(name.clone(), cx));
                        }
                    });
                }
                Ok(installed_version)
            }.await;
            this.update(cx, |this, cx| {
                if let Some(state) = this.servers.get_mut(&name) {
                    state.busy = false;
                    match result {
                        Ok(version) => {
                            state.checked = true;
                            state.installed = installing;
                            state.version = version;
                        }
                        Err(error) => {
                            let message = format!("Failed to {} {name}: {error:#}", if installing { "install" } else { "uninstall" });
                            state.error = Some(message.clone());
                            cx.emit(BuiltInServerEvent::OperationFailed(message));
                        }
                    }
                }
                cx.notify();
            })?;
            anyhow::Ok(())
        }).detach_and_log_err(cx);
    }
}

fn valid_homepage(homepage: &str) -> bool {
    http_client::Url::parse(homepage)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
}

struct ManagedServerDelegate {
    registry: Arc<LanguageRegistry>,
    http_client: Arc<dyn HttpClient>,
    background_executor: BackgroundExecutor,
    status_source_id: gpui::EntityId,
    root: PathBuf,
}

#[async_trait]
impl LspAdapterDelegate for ManagedServerDelegate {
    fn show_notification(&self, message: &str, _: &mut App) {
        log::info!("{message}");
    }
    fn http_client(&self) -> Arc<dyn HttpClient> {
        self.http_client.clone()
    }
    fn background_executor(&self) -> &BackgroundExecutor {
        &self.background_executor
    }
    fn worktree_id(&self) -> WorktreeId {
        WorktreeId::from_proto(0)
    }
    fn worktree_root_path(&self) -> &Path {
        &self.root
    }
    fn resolve_relative_path(&self, path: PathBuf) -> PathBuf {
        self.root.join(path)
    }
    fn status_source_id(&self) -> gpui::EntityId {
        self.status_source_id
    }
    fn update_status(&self, name: LanguageServerName, status: BinaryStatus) {
        self.registry
            .update_lsp_binary_status_for_entity(self.status_source_id, name, status);
    }
    fn registered_lsp_adapters(&self) -> Vec<Arc<dyn LspAdapter>> {
        self.registry
            .all_lsp_adapters()
            .into_iter()
            .map(|adapter| adapter.adapter.clone())
            .collect()
    }
    async fn language_server_download_dir(&self, name: &LanguageServerName) -> Option<Arc<Path>> {
        let directory = self.registry.language_server_download_dir(name)?;
        smol::fs::create_dir_all(&directory).await.log_err()?;
        Some(directory)
    }
    async fn package_manager_installed_version(
        &self,
        _: &str,
    ) -> Result<Option<(PathBuf, Version)>> {
        Ok(None)
    }
    async fn which(&self, command: &OsStr) -> Option<PathBuf> {
        which::which(command).ok()
    }
    async fn shell_env(&self) -> HashMap<String, String> {
        std::env::vars().collect()
    }
    async fn read_text_file(&self, _: &RelPath) -> Result<String> {
        bail!("managed installation has no project files")
    }
    async fn try_exec(&self, binary: LanguageServerBinary) -> Result<()> {
        let output = util::command::new_command(binary.path)
            .args(binary.arguments)
            .envs(binary.env.unwrap_or_default())
            .output()
            .await?;
        ensure!(
            output.status.success(),
            "language server command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }
}
