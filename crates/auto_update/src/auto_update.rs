use anyhow::{Context as _, Result};
use client::Client;
use db::kvp::KeyValueStore;
use futures_lite::StreamExt;
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Global, Task, Window, actions,
};
use http_client::{HttpClient, HttpClientWithUrl};
use paths::remote_servers_dir;
use release_channel::{AppCommitSha, ReleaseChannel};
use semver::Version;
use serde::{Deserialize, Serialize};
use settings::{RegisterSetting, Settings, SettingsStore};
use smol::fs::File;
use smol::io::AsyncReadExt;
use std::{
    env::{
        self,
        consts::{ARCH, OS},
    },
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};
use workspace::Workspace;

const SHOULD_SHOW_UPDATE_NOTIFICATION_KEY: &str = "auto-updater-should-show-updated-notification";

const POLL_INTERVAL: Duration = Duration::from_secs(60 * 60);
const NIGHTLY_POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);
const REMOTE_SERVER_CACHE_LIMIT: usize = 5;

actions!(
    auto_update,
    [
        /// Checks for available updates.
        Check,
        /// Dismisses the update error message.
        DismissMessage,
        /// Opens the release notes for the current version in a browser.
        ViewReleaseNotes,
    ]
);

#[derive(Serialize, Debug)]
pub struct AssetQuery<'a> {
    asset: &'a str,
    os: &'a str,
    arch: &'a str,
    metrics_id: Option<&'a str>,
    system_id: Option<&'a str>,
    is_staff: Option<bool>,
}

#[derive(Clone, Debug)]
pub enum AutoUpdateStatus {
    Idle,
    Checking,
    UpdateAvailable {
        version: Version,
    },
    Errored {
        error: Arc<anyhow::Error>,
    },
}

impl PartialEq for AutoUpdateStatus {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (AutoUpdateStatus::Idle, AutoUpdateStatus::Idle) => true,
            (AutoUpdateStatus::Checking, AutoUpdateStatus::Checking) => true,
            (
                AutoUpdateStatus::UpdateAvailable { version: v1 },
                AutoUpdateStatus::UpdateAvailable { version: v2 },
            ) => v1 == v2,
            (AutoUpdateStatus::Errored { error: e1 }, AutoUpdateStatus::Errored { error: e2 }) => {
                e1.to_string() == e2.to_string()
            }
            _ => false,
        }
    }
}

impl AutoUpdateStatus {
    pub fn is_update_available(&self) -> bool {
        matches!(self, Self::UpdateAvailable { .. })
    }
}

pub enum AutoUpdateEvent {
    /// A manual check received a release response and found no newer version.
    UpToDate,
}

pub struct AutoUpdater {
    status: AutoUpdateStatus,
    current_version: Version,
    client: Arc<Client>,
    pending_poll: Option<Task<Option<()>>>,
    update_check_type: UpdateCheckType,
    _wake_subscription: gpui::Subscription,
    dismissed_status: Option<AutoUpdateStatus>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ReleaseAsset {
    pub version: String,
    pub url: String,
}

#[derive(Clone, Copy, Debug, RegisterSetting)]
struct AutoUpdateSetting(bool);

/// Whether or not to automatically check for updates.
///
/// Default: true
impl Settings for AutoUpdateSetting {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self(content.auto_update.unwrap())
    }
}

#[derive(Default)]
struct GlobalAutoUpdate(Option<Entity<AutoUpdater>>);

impl Global for GlobalAutoUpdate {}

pub fn init(client: Arc<Client>, cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|_, action, window, cx| check(action, window, cx));

        workspace.register_action(|_, action, _, cx| {
            view_release_notes(action, cx);
        });
    })
    .detach();

    let version = release_channel::AppVersion::global(cx);
    let auto_updater = cx.new(|cx| {
        let updater = AutoUpdater::new(version, client, cx);

        let poll_for_updates = ReleaseChannel::try_global(cx)
            .map(|channel| channel.poll_for_updates())
            .unwrap_or(false);

        if option_env!("ZED_UPDATE_EXPLANATION").is_none()
            && env::var("ZED_UPDATE_EXPLANATION").is_err()
            && poll_for_updates
        {
            let mut update_subscription = AutoUpdateSetting::get_global(cx)
                .0
                .then(|| updater.start_polling(cx));

            cx.observe_global::<SettingsStore>(move |updater: &mut AutoUpdater, cx| {
                if AutoUpdateSetting::get_global(cx).0 {
                    if update_subscription.is_none() {
                        update_subscription = Some(updater.start_polling(cx))
                    }
                } else {
                    update_subscription.take();
                }
            })
            .detach();
        }

        updater
    });
    cx.set_global(GlobalAutoUpdate(Some(auto_updater)));
}

pub fn check(_: &Check, window: &mut Window, cx: &mut App) {
    if let Some(message) = option_env!("ZED_UPDATE_EXPLANATION")
        .map(ToOwned::to_owned)
        .or_else(|| env::var("ZED_UPDATE_EXPLANATION").ok())
    {
        drop(window.prompt(
            gpui::PromptLevel::Info,
            "Zed was installed via a package manager.",
            Some(&message),
            &["OK"],
            cx,
        ));
        return;
    }

    if !ReleaseChannel::try_global(cx)
        .map(|channel| channel.poll_for_updates())
        .unwrap_or(false)
    {
        return;
    }

    if let Some(updater) = AutoUpdater::get(cx) {
        updater.update(cx, |updater, cx| updater.poll(UpdateCheckType::Manual, cx));
    } else {
        drop(window.prompt(
            gpui::PromptLevel::Info,
            "Could not check for updates",
            Some("Auto-updates disabled for non-bundled app."),
            &["OK"],
            cx,
        ));
    }
}

pub fn release_notes_url(cx: &mut App) -> Option<String> {
    let release_channel = ReleaseChannel::try_global(cx)?;
    let auto_updater = AutoUpdater::get(cx)?;
    let auto_updater = auto_updater.read(cx);
    Some(release_page_url(
        release_channel,
        auto_updater.current_version.clone(),
        &auto_updater.client,
    ))
}

/// Returns the release page for the update reported by the last check, if any.
pub fn available_update_url(cx: &mut App) -> Option<String> {
    let release_channel = ReleaseChannel::try_global(cx)?;
    let auto_updater = AutoUpdater::get(cx)?;
    let auto_updater = auto_updater.read(cx);
    let AutoUpdateStatus::UpdateAvailable { version } = &auto_updater.status else {
        return None;
    };
    Some(release_page_url(
        release_channel,
        version.clone(),
        &auto_updater.client,
    ))
}

fn release_page_url(release_channel: ReleaseChannel, mut version: Version, client: &Client) -> String {
    match release_channel {
        ReleaseChannel::Stable | ReleaseChannel::Preview => {
            version.pre = semver::Prerelease::EMPTY;
            version.build = semver::BuildMetadata::EMPTY;
            let release_channel = release_channel.dev_name();
            client
                .http_client()
                .build_url(&format!("/releases/{release_channel}/{version}"))
        }
        ReleaseChannel::Nightly => {
            "https://github.com/zed-industries/zed/commits/nightly/".to_string()
        }
        ReleaseChannel::Dev => "https://github.com/zed-industries/zed/commits/main/".to_string(),
    }
}

pub fn view_release_notes(_: &ViewReleaseNotes, cx: &mut App) -> Option<()> {
    let url = release_notes_url(cx)?;
    cx.open_url(&url);
    None
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UpdateCheckType {
    Automatic,
    Manual,
}

impl UpdateCheckType {
    pub fn is_manual(self) -> bool {
        self == Self::Manual
    }
}

impl EventEmitter<AutoUpdateEvent> for AutoUpdater {}

impl AutoUpdater {
    pub fn get(cx: &mut App) -> Option<Entity<Self>> {
        cx.default_global::<GlobalAutoUpdate>().0.clone()
    }

    fn new(current_version: Version, client: Arc<Client>, cx: &mut Context<Self>) -> Self {
        // A download or check that was in flight when the machine went to sleep
        // is almost certainly riding a TCP connection that silently died during
        // suspend, so it would otherwise appear to stall indefinitely.
        let wake_subscription = cx.on_system_wake({
            let this = cx.entity().downgrade();
            move |cx| {
                this.update(cx, |this, cx| this.restart_after_wake(cx)).ok();
            }
        });

        Self {
            status: AutoUpdateStatus::Idle,
            current_version,
            client,
            pending_poll: None,
            update_check_type: UpdateCheckType::Automatic,
            _wake_subscription: wake_subscription,
            dismissed_status: None,
        }
    }

    fn restart_after_wake(&mut self, cx: &mut Context<Self>) {
        if self.pending_poll.is_none() {
            return;
        }

        let check_type = self.update_check_type;
        self.pending_poll.take();
        self.poll(check_type, cx);
    }

    pub fn start_polling(&self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let poll_interval =
            ReleaseChannel::try_global(cx).map_or(POLL_INTERVAL, |channel| match channel {
                ReleaseChannel::Nightly => NIGHTLY_POLL_INTERVAL,
                _ => POLL_INTERVAL,
            });

        cx.spawn(async move |this, cx| {
            loop {
                this.update(cx, |this, cx| this.poll(UpdateCheckType::Automatic, cx))?;
                cx.background_executor().timer(poll_interval).await;
            }
        })
    }

    pub fn update_check_type(&self) -> UpdateCheckType {
        self.update_check_type
    }

    pub fn poll(&mut self, check_type: UpdateCheckType, cx: &mut Context<Self>) {
        if check_type.is_manual() {
            self.dismissed_status = None;
        }
        if self.pending_poll.is_some() {
            if self.update_check_type == UpdateCheckType::Automatic {
                self.update_check_type = check_type;
                cx.notify();
            }
            return;
        }
        self.update_check_type = check_type;

        cx.notify();

        self.pending_poll = Some(cx.spawn(async move |this, cx| {
            let result = Self::update(this.upgrade()?, cx).await;
            this.update(cx, |this, cx| {
                this.pending_poll = None;
                if let Err(error) = result {
                    match check_type {
                        // Be quiet if the check was automated (e.g. when offline), and keep
                        // showing an update found by an earlier check.
                        UpdateCheckType::Automatic => {
                            log::info!("auto-update check failed: error:{:?}", error);
                            if this.status == AutoUpdateStatus::Checking {
                                this.status = AutoUpdateStatus::Idle;
                            }
                        }
                        UpdateCheckType::Manual => {
                            log::error!("auto-update failed: error:{:?}", error);
                            this.status = AutoUpdateStatus::Errored {
                                error: Arc::new(error),
                            };
                        }
                    }

                    cx.notify();
                }
            })
            .ok()
        }));
    }

    pub fn current_version(&self) -> Version {
        self.current_version.clone()
    }

    pub fn status(&self) -> AutoUpdateStatus {
        self.status.clone()
    }

    pub fn dismissed_status(&self) -> Option<AutoUpdateStatus> {
        self.dismissed_status.clone()
    }

    pub fn dismiss_status(&mut self, status: AutoUpdateStatus, cx: &mut Context<Self>) {
        self.dismissed_status = Some(status);
        cx.notify();
    }

    pub fn dismiss(&mut self, cx: &mut Context<Self>) -> bool {
        if let AutoUpdateStatus::Idle = self.status {
            return false;
        }
        self.status = AutoUpdateStatus::Idle;
        cx.notify();
        true
    }

    // If you are packaging Zed and need to override the place it downloads SSH remotes from,
    // you can override this function. You should also update get_remote_server_release_url to return
    // Ok(None).
    pub async fn download_remote_server_release(
        release_channel: ReleaseChannel,
        version: Option<Version>,
        os: &str,
        arch: &str,
        set_status: impl Fn(&str, &mut AsyncApp) + Send + 'static,
        cx: &mut AsyncApp,
    ) -> Result<PathBuf> {
        let this = cx.update(|cx| {
            cx.default_global::<GlobalAutoUpdate>()
                .0
                .clone()
                .context("auto-update not initialized")
        })?;

        set_status("Fetching remote server release", cx);
        let release = Self::get_release_asset(
            &this,
            release_channel,
            version,
            "zed-remote-server",
            os,
            arch,
            cx,
        )
        .await?;

        let servers_dir = paths::remote_servers_dir();
        let channel_dir = servers_dir.join(release_channel.dev_name());
        let platform_dir = channel_dir.join(format!("{}-{}", os, arch));
        let version_path = platform_dir.join(format!("{}.gz", release.version));
        smol::fs::create_dir_all(&platform_dir).await.ok();

        let client = this.read_with(cx, |this, _| this.client.http_client());

        if smol::fs::metadata(&version_path).await.is_err() {
            log::info!(
                "downloading zed-remote-server {os} {arch} version {}",
                release.version
            );
            set_status("Downloading remote server", cx);
            download_remote_server_binary(&version_path, release, client).await?;
        }

        if let Err(error) =
            cleanup_remote_server_cache(&platform_dir, &version_path, REMOTE_SERVER_CACHE_LIMIT)
                .await
        {
            log::warn!(
                "Failed to clean up remote server cache in {:?}: {error:#}",
                platform_dir
            );
        }

        Ok(version_path)
    }

    pub async fn get_remote_server_release_url(
        channel: ReleaseChannel,
        version: Option<Version>,
        os: &str,
        arch: &str,
        cx: &mut AsyncApp,
    ) -> Result<Option<String>> {
        let this = cx.update(|cx| {
            cx.default_global::<GlobalAutoUpdate>()
                .0
                .clone()
                .context("auto-update not initialized")
        })?;

        let release =
            Self::get_release_asset(&this, channel, version, "zed-remote-server", os, arch, cx)
                .await?;

        Ok(Some(release.url))
    }

    async fn get_release_asset(
        this: &Entity<Self>,
        release_channel: ReleaseChannel,
        version: Option<Version>,
        asset: &str,
        os: &str,
        arch: &str,
        cx: &mut AsyncApp,
    ) -> Result<ReleaseAsset> {
        let client = this.read_with(cx, |this, _| this.client.clone());

        let (system_id, metrics_id, is_staff) = if client.telemetry().metrics_enabled() {
            (
                client.telemetry().system_id(),
                client.telemetry().metrics_id(),
                client.telemetry().is_staff(),
            )
        } else {
            (None, None, None)
        };

        let version = if let Some(mut version) = version {
            version.pre = semver::Prerelease::EMPTY;
            version.build = semver::BuildMetadata::EMPTY;
            version.to_string()
        } else {
            "latest".to_string()
        };
        let http_client = client.http_client();

        let path = format!("/releases/{}/{}/asset", release_channel.dev_name(), version,);
        let url = http_client.build_zed_cloud_url_with_query(
            &path,
            AssetQuery {
                os,
                arch,
                asset,
                metrics_id: metrics_id.as_deref(),
                system_id: system_id.as_deref(),
                is_staff,
            },
        )?;

        let mut response = http_client
            .get(url.as_str(), Default::default(), true)
            .await?;
        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await?;

        anyhow::ensure!(
            response.status().is_success(),
            "failed to fetch release: {:?}",
            String::from_utf8_lossy(&body),
        );

        serde_json::from_slice(body.as_slice()).with_context(|| {
            format!(
                "error deserializing release {:?}",
                String::from_utf8_lossy(&body),
            )
        })
    }

    async fn update(this: Entity<Self>, cx: &mut AsyncApp) -> Result<()> {
        let (installed_version, release_channel) = this.update(cx, |this, cx| {
            // Automatic checks keep the current status so an available update stays visible.
            if this.update_check_type.is_manual() {
                this.status = AutoUpdateStatus::Checking;
                cx.notify();
            }
            log::info!("Auto Update: checking for updates");
            (
                this.current_version.clone(),
                match ReleaseChannel::try_global(cx) {
                    Some(ReleaseChannel::Dev) | None => ReleaseChannel::Stable,
                    Some(channel) => channel,
                },
            )
        });

        let fetched_release_data =
            Self::get_release_asset(&this, release_channel, None, "zed", OS, ARCH, cx).await?;
        let app_commit_sha = Ok(cx.update(|cx| AppCommitSha::try_global(cx).map(|sha| sha.full())));
        let newer_version = Self::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_release_data.version,
        )?;

        this.update(cx, |this, cx| {
            this.status = match newer_version {
                Some(version) => AutoUpdateStatus::UpdateAvailable { version },
                None => {
                    if this.update_check_type.is_manual() {
                        cx.emit(AutoUpdateEvent::UpToDate);
                    }
                    AutoUpdateStatus::Idle
                }
            };
            cx.notify();
        });
        Ok(())
    }

    fn check_if_fetched_version_is_newer(
        release_channel: ReleaseChannel,
        app_commit_sha: Result<Option<String>>,
        installed_version: Version,
        fetched_version: String,
    ) -> Result<Option<Version>> {
        let fetched_version = fetched_version.parse::<Version>()?;

        match release_channel {
            ReleaseChannel::Nightly => {
                let fetched_sha = fetched_version.build.as_str().rsplit('.').next();
                let is_newer = app_commit_sha
                    .ok()
                    .flatten()
                    .is_none_or(|sha| fetched_sha != Some(sha.as_str()));
                Ok(is_newer.then_some(fetched_version))
            }
            _ => Ok(Self::check_if_fetched_version_is_newer_non_nightly(
                installed_version,
                fetched_version,
            )),
        }
    }

    fn check_if_fetched_version_is_newer_non_nightly(
        mut installed_version: Version,
        fetched_version: Version,
    ) -> Option<Version> {
        // For non-nightly releases, ignore build and pre-release fields as they're not provided by our endpoints right now.
        installed_version.pre = semver::Prerelease::EMPTY;
        installed_version.build = semver::BuildMetadata::EMPTY;
        (fetched_version > installed_version).then_some(fetched_version)
    }

    pub fn set_should_show_update_notification(
        &self,
        should_show: bool,
        cx: &App,
    ) -> Task<Result<()>> {
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            if should_show {
                kvp.write_kvp(
                    SHOULD_SHOW_UPDATE_NOTIFICATION_KEY.to_string(),
                    "".to_string(),
                )
                .await?;
            } else {
                kvp.delete_kvp(SHOULD_SHOW_UPDATE_NOTIFICATION_KEY.to_string())
                    .await?;
            }
            Ok(())
        })
    }

    pub fn should_show_update_notification(&self, cx: &App) -> Task<Result<bool>> {
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            Ok(kvp.read_kvp(SHOULD_SHOW_UPDATE_NOTIFICATION_KEY)?.is_some())
        })
    }
}

async fn download_remote_server_binary(
    target_path: &PathBuf,
    release: ReleaseAsset,
    client: Arc<HttpClientWithUrl>,
) -> Result<()> {
    let temp = tempfile::Builder::new().tempfile_in(remote_servers_dir())?;
    let mut temp_file = File::create(&temp).await?;

    let mut response = client.get(&release.url, Default::default(), true).await?;
    anyhow::ensure!(
        response.status().is_success(),
        "failed to download remote server release: {:?}",
        response.status()
    );
    smol::io::copy(response.body_mut(), &mut temp_file).await?;
    smol::fs::rename(&temp, &target_path).await?;

    Ok(())
}

async fn cleanup_remote_server_cache(
    platform_dir: &Path,
    keep_path: &Path,
    limit: usize,
) -> Result<()> {
    if limit == 0 {
        return Ok(());
    }

    let mut entries = smol::fs::read_dir(platform_dir).await?;
    let now = SystemTime::now();
    let mut candidates = Vec::new();

    while let Some(entry) = entries.next().await {
        let entry = entry?;
        let path = entry.path();
        if path.extension() != Some(OsStr::new("gz")) {
            continue;
        }

        let mtime = if path == keep_path {
            now
        } else {
            smol::fs::metadata(&path)
                .await
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH)
        };

        candidates.push((path, mtime));
    }

    if candidates.len() <= limit {
        return Ok(());
    }

    candidates.sort_by(|(path_a, time_a), (path_b, time_b)| {
        time_b.cmp(time_a).then_with(|| path_a.cmp(path_b))
    });

    for (index, (path, _)) in candidates.into_iter().enumerate() {
        if index < limit || path == keep_path {
            continue;
        }

        if let Err(error) = smol::fs::remove_file(&path).await {
            log::warn!(
                "Failed to remove old remote server archive {:?}: {}",
                path,
                error
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use client::Client;
    use clock::FakeSystemClock;
    use gpui::TestAppContext;
    use http_client::{FakeHttpClient, Response};
    use settings::default_settings;
    use std::sync::{
        Arc,
        atomic::{self, AtomicBool, AtomicUsize},
    };

    #[ctor::ctor(unsafe)]
    fn init_logger() {
        zlog::init_test();
    }

    use super::*;

    #[gpui::test]
    fn test_auto_update_defaults_to_true(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut store = SettingsStore::new(cx, &settings::default_settings());
            store
                .set_default_settings(&default_settings(), cx)
                .expect("Unable to set default settings");
            store
                .set_user_settings("{}", cx)
                .expect("Unable to set user settings");
            cx.set_global(store);
            assert!(AutoUpdateSetting::get_global(cx).0);
        });
    }

    #[gpui::test]
    async fn test_auto_update_only_reports_available_version(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let release_available = Arc::new(AtomicBool::new(false));
        let request_count = Arc::new(AtomicUsize::new(0));
        cx.add_window(|_, _| gpui::Empty);
        cx.update(|cx| {
            settings::init(cx);
            release_channel::init_test(Version::new(0, 100, 0), ReleaseChannel::Stable, cx);
            let client = FakeHttpClient::create({
                let release_available = release_available.clone();
                let request_count = request_count.clone();
                move |request| {
                    let release_available = release_available.load(atomic::Ordering::SeqCst);
                    request_count.fetch_add(1, atomic::Ordering::SeqCst);
                    async move {
                        assert_eq!(request.method(), http_client::Method::GET);
                        assert_eq!(
                            request.uri().path(),
                            "/releases/stable/latest/asset",
                            "the updater must only fetch release metadata"
                        );
                        let version = if release_available { "0.100.1" } else { "0.100.0" };
                        Ok(Response::builder().status(200).body(
                            serde_json::json!({"version": version, "url": "https://test.example/must-not-download"})
                                .to_string()
                                .into(),
                        )?)
                    }
                }
            });
            crate::init(Client::new(Arc::new(FakeSystemClock::new()), client, cx), cx);
        });
        let updater = cx.update(|cx| AutoUpdater::get(cx).expect("updater"));
        let available = AutoUpdateStatus::UpdateAvailable {
            version: Version::new(0, 100, 1),
        };

        cx.condition(&updater, |updater, _| {
            request_count.load(atomic::Ordering::SeqCst) >= 1 && updater.pending_poll.is_none()
        })
        .await;
        assert_eq!(updater.read_with(cx, |updater, _| updater.status()), AutoUpdateStatus::Idle);

        release_available.store(true, atomic::Ordering::SeqCst);
        cx.background_executor.advance_clock(POLL_INTERVAL);
        cx.condition(&updater, |updater, _| {
            updater.status().is_update_available() && updater.pending_poll.is_none()
        })
        .await;
        assert_eq!(updater.read_with(cx, |updater, _| updater.status()), available);
        assert!(!cx.has_pending_prompt(), "updates must not open a modal prompt");
        assert_eq!(
            updater.read_with(cx, |updater, _| updater.current_version()),
            Version::new(0, 100, 0)
        );

        updater.update(cx, |updater, cx| updater.dismiss_status(available.clone(), cx));
        cx.background_executor.advance_clock(POLL_INTERVAL);
        cx.condition(&updater, |updater, _| {
            request_count.load(atomic::Ordering::SeqCst) >= 3 && updater.pending_poll.is_none()
        })
        .await;
        updater.read_with(cx, |updater, _| {
            assert_eq!(updater.status(), available);
            assert_eq!(
                updater.dismissed_status(),
                Some(available.clone()),
                "automatic checks should keep the same version dismissed"
            );
        });

        updater.update(cx, |updater, cx| updater.poll(UpdateCheckType::Manual, cx));
        cx.condition(&updater, |updater, _| updater.pending_poll.is_none())
            .await;
        updater.read_with(cx, |updater, _| {
            assert_eq!(updater.status(), available);
            assert_eq!(
                updater.dismissed_status(),
                None,
                "manual checks should show the available version again"
            );
        });
    }

    #[gpui::test]
    fn test_dev_builds_do_not_poll_for_updates(cx: &mut TestAppContext) {
        let request_count = Arc::new(AtomicUsize::new(0));
        cx.update(|cx| {
            settings::init(cx);
            release_channel::init_test(Version::new(0, 100, 0), ReleaseChannel::Dev, cx);
            let client = FakeHttpClient::create({
                let request_count = request_count.clone();
                move |_| {
                    request_count.fetch_add(1, atomic::Ordering::SeqCst);
                    async move { Ok(Response::builder().status(500).body("".into())?) }
                }
            });
            crate::init(Client::new(Arc::new(FakeSystemClock::new()), client, cx), cx);
        });
        cx.background_executor.advance_clock(POLL_INTERVAL);
        cx.run_until_parked();
        assert_eq!(request_count.load(atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn test_stable_does_not_update_when_fetched_version_is_not_higher() {
        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            ReleaseChannel::Stable,
            Ok(Some("a".to_string())),
            Version::new(1, 0, 0),
            "1.0.0".to_string(),
        );

        assert_eq!(newer_version.unwrap(), None);
    }

    #[test]
    fn test_stable_does_update_when_fetched_version_is_higher() {
        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            ReleaseChannel::Stable,
            Ok(Some("a".to_string())),
            Version::new(1, 0, 0),
            "1.0.1".to_string(),
        );

        assert_eq!(newer_version.unwrap(), Some(Version::new(1, 0, 1)));
    }

    #[test]
    fn test_nightly_does_not_update_when_fetched_sha_is_same() {
        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            ReleaseChannel::Nightly,
            Ok(Some("a".to_string())),
            Version::new(1, 0, 0),
            "1.0.0+a".to_string(),
        );

        assert_eq!(newer_version.unwrap(), None);
    }

    #[test]
    fn test_nightly_does_update_when_fetched_sha_is_not_same() {
        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            ReleaseChannel::Nightly,
            Ok(Some("a".to_string())),
            Version::new(1, 0, 0),
            "1.0.0+b".to_string(),
        );

        assert_eq!(newer_version.unwrap(), Some("1.0.0+b".parse().unwrap()));
    }

    #[test]
    fn test_nightly_does_update_when_installed_versions_sha_cannot_be_retrieved() {
        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            ReleaseChannel::Nightly,
            Ok(None),
            Version::new(1, 0, 0),
            "1.0.0+a".to_string(),
        );

        assert_eq!(newer_version.unwrap(), Some("1.0.0+a".parse().unwrap()));
    }
}
