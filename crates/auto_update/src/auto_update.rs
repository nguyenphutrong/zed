use anyhow::{Context as _, Result};
use client::Client;
use db::kvp::KeyValueStore;
use futures_lite::StreamExt;
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, Global, Task, TaskExt, Window, actions,
};
use http_client::{HttpClient, HttpClientWithUrl};
use paths::remote_servers_dir;
use release_channel::{AppCommitSha, ReleaseChannel};
use semver::Version;
use serde::{Deserialize, Serialize};
use settings::{RegisterSetting, Settings, SettingsStore};
use smol::fs::File;
use smol::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
};
use std::{
    env::{
        self,
        consts::{ARCH, OS},
    },
    ffi::OsStr,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};
use util::command::new_command;
use workspace::Workspace;

const SHOULD_SHOW_UPDATE_NOTIFICATION_KEY: &str = "auto-updater-should-show-updated-notification";

#[derive(Debug)]
struct MissingDependencyError(String);

impl std::fmt::Display for MissingDependencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for MissingDependencyError {}
const POLL_INTERVAL: Duration = Duration::from_secs(60 * 60);
const NIGHTLY_POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);
const REMOTE_SERVER_CACHE_LIMIT: usize = 5;
const REZED_GITHUB_RELEASES_URL: &str = "https://github.com/nguyenphutrong/rezed/releases";
const REZED_GITHUB_LATEST_RELEASE_API_URL: &str =
    "https://api.github.com/repos/nguyenphutrong/rezed/releases/latest";

#[cfg(target_os = "linux")]
fn linux_rsync_install_hint() -> &'static str {
    let os_release = match std::fs::read_to_string("/etc/os-release") {
        Ok(os_release) => os_release,
        Err(_) => return "Please install rsync using your package manager",
    };

    let mut distribution_ids = Vec::new();
    for line in os_release.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("ID=") {
            distribution_ids.push(value.trim_matches('"').to_ascii_lowercase());
        } else if let Some(value) = trimmed.strip_prefix("ID_LIKE=") {
            for id in value.trim_matches('"').split_whitespace() {
                distribution_ids.push(id.to_ascii_lowercase());
            }
        }
    }

    let package_manager_hint = if distribution_ids
        .iter()
        .any(|distribution_id| distribution_id == "arch")
    {
        Some("Install it with: sudo pacman -S rsync")
    } else if distribution_ids
        .iter()
        .any(|distribution_id| distribution_id == "debian" || distribution_id == "ubuntu")
    {
        Some("Install it with: sudo apt install rsync")
    } else if distribution_ids.iter().any(|distribution_id| {
        distribution_id == "fedora"
            || distribution_id == "rhel"
            || distribution_id == "centos"
            || distribution_id == "rocky"
            || distribution_id == "almalinux"
    }) {
        Some("Install it with: sudo dnf install rsync")
    } else if distribution_ids
        .iter()
        .any(|distribution_id| distribution_id == "nixos")
    {
        Some("Install pkgs.rsync from nixpkgs")
    } else {
        None
    };

    package_manager_hint.unwrap_or("Please install rsync using your package manager")
}

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
    Downloading {
        version: Version,
        /// Download progress as a fraction in the range `0.0..=1.0`, or `None`
        /// when the total download size is not yet known.
        progress: Option<f32>,
    },
    Installing {
        version: Version,
    },
    Updated {
        version: Version,
    },
    Errored {
        error: Arc<anyhow::Error>,
    },
}

impl PartialEq for AutoUpdateStatus {
    // `progress` is deliberately not compared: two `Downloading` statuses for
    // the same version are equal regardless of how far the download is.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (AutoUpdateStatus::Idle, AutoUpdateStatus::Idle) => true,
            (AutoUpdateStatus::Checking, AutoUpdateStatus::Checking) => true,
            (
                AutoUpdateStatus::Downloading { version: v1, .. },
                AutoUpdateStatus::Downloading { version: v2, .. },
            ) => v1 == v2,
            (
                AutoUpdateStatus::Installing { version: v1 },
                AutoUpdateStatus::Installing { version: v2 },
            ) => v1 == v2,
            (
                AutoUpdateStatus::Updated { version: v1 },
                AutoUpdateStatus::Updated { version: v2 },
            ) => v1 == v2,
            (AutoUpdateStatus::Errored { error: e1 }, AutoUpdateStatus::Errored { error: e2 }) => {
                e1.to_string() == e2.to_string()
            }
            _ => false,
        }
    }
}

impl AutoUpdateStatus {
    pub fn is_updated(&self) -> bool {
        matches!(self, Self::Updated { .. })
    }
}

pub struct AutoUpdater {
    status: AutoUpdateStatus,
    current_version: Version,
    client: Arc<Client>,
    pending_poll: Option<Task<Option<()>>>,
    quit_subscription: Option<gpui::Subscription>,
    update_check_type: UpdateCheckType,
    dismissed_status: Option<AutoUpdateStatus>,
    failed_install_version: Option<Version>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ReleaseAsset {
    pub version: String,
    pub url: String,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubReleaseAsset>,
}

#[derive(Deserialize)]
struct GithubReleaseAsset {
    name: String,
    browser_download_url: String,
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
    let url = match release_channel {
        ReleaseChannel::Stable | ReleaseChannel::Preview => {
            let auto_updater = AutoUpdater::get(cx)?;
            let auto_updater = auto_updater.read(cx);
            let mut current_version = auto_updater.current_version.clone();
            current_version.build = semver::BuildMetadata::EMPTY;
            format!("{REZED_GITHUB_RELEASES_URL}/tag/v{current_version}")
        }
        ReleaseChannel::Nightly => {
            "https://github.com/nguyenphutrong/rezed/commits/nightly/".to_string()
        }
        ReleaseChannel::Dev => "https://github.com/nguyenphutrong/rezed/commits/rezed/".to_string(),
    };
    Some(url)
}

pub fn view_release_notes(_: &ViewReleaseNotes, cx: &mut App) -> Option<()> {
    let url = release_notes_url(cx)?;
    cx.open_url(&url);
    None
}

#[cfg(target_os = "macos")]
const INSTALLER_DIR_PREFIX: &str = "rezed-auto-update";
#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
const INSTALLER_DIR_PREFIX: &str = "zed-auto-update";
#[cfg(target_os = "macos")]
const INSTALLER_MARKER_FILE: &str = ".rezed-auto-update";
#[cfg(target_os = "macos")]
const INSTALLER_MARKER_PREFIX: &str = "nguyenphutrong/rezed auto updater pid=";

#[cfg(target_os = "macos")]
const LEGACY_INSTALLER_DIR_PREFIX: &str = "zed-auto-update";
#[cfg(target_os = "macos")]
const UPDATE_LOCK_FILE: &str = "rezed-auto-update.lock";

#[cfg(not(target_os = "windows"))]
struct InstallerDir {
    path: PathBuf,
    temp_dir: Option<tempfile::TempDir>,
}

#[cfg(not(target_os = "windows"))]
impl InstallerDir {
    async fn new() -> Result<Self> {
        let temp_dir = tempfile::Builder::new()
            .prefix(INSTALLER_DIR_PREFIX)
            .tempdir()?;
        let path = temp_dir.path().to_owned();
        #[cfg(target_os = "macos")]
        fs::write(
            path.join(INSTALLER_MARKER_FILE),
            installer_marker_content(std::process::id()),
        )
        .await?;
        Ok(Self {
            path,
            temp_dir: Some(temp_dir),
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn keep_for_external_cleanup(&mut self) {
        if let Some(temp_dir) = self.temp_dir.take() {
            drop(temp_dir.keep());
        }
    }
}

#[cfg(not(target_os = "windows"))]
impl Drop for InstallerDir {
    fn drop(&mut self) {
        let Some(temp_dir) = self.temp_dir.take() else {
            return;
        };
        let path = temp_dir.path().to_owned();
        if let Err(error) = temp_dir.close() {
            log::error!(
                "failed to remove auto-update installer dir {}: {error}",
                path.display()
            );
        }
    }
}

#[cfg(target_os = "windows")]
struct InstallerDir(PathBuf);

#[cfg(target_os = "windows")]
impl InstallerDir {
    async fn new() -> Result<Self> {
        let installer_dir = std::env::current_exe()?
            .parent()
            .context("No parent dir for Zed.exe")?
            .join("updates");
        if smol::fs::metadata(&installer_dir).await.is_ok() {
            smol::fs::remove_dir_all(&installer_dir).await?;
        }
        smol::fs::create_dir(&installer_dir).await?;
        Ok(Self(installer_dir))
    }

    fn path(&self) -> &Path {
        self.0.as_path()
    }

    fn keep_for_external_cleanup(&mut self) {}
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

impl AutoUpdater {
    pub fn get(cx: &mut App) -> Option<Entity<Self>> {
        cx.default_global::<GlobalAutoUpdate>().0.clone()
    }

    fn new(current_version: Version, client: Arc<Client>, cx: &mut Context<Self>) -> Self {
        // On windows, executable files cannot be overwritten while they are
        // running, so we must wait to overwrite the application until quitting
        // or restarting. When quitting the app, we spawn the auto update helper
        // to finish the auto update process after Zed exits. When restarting
        // the app after an update, we use `set_restart_path` to run the auto
        // update helper instead of the app, so that it can overwrite the app
        // and then spawn the new binary.
        #[cfg(target_os = "windows")]
        let quit_subscription = Some(cx.on_app_quit(|_, _| finalize_auto_update_on_quit()));
        #[cfg(not(target_os = "windows"))]
        let quit_subscription = None;

        cx.on_app_restart(|this, _| {
            this.quit_subscription.take();
        })
        .detach();

        Self {
            status: AutoUpdateStatus::Idle,
            current_version,
            client,
            pending_poll: None,
            quit_subscription,
            update_check_type: UpdateCheckType::Automatic,
            dismissed_status: None,
            failed_install_version: None,
        }
    }

    pub fn start_polling(&self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let poll_interval =
            ReleaseChannel::try_global(cx).map_or(POLL_INTERVAL, |channel| match channel {
                ReleaseChannel::Nightly => NIGHTLY_POLL_INTERVAL,
                _ => POLL_INTERVAL,
            });

        cx.spawn(async move |this, cx| {
            if cfg!(target_os = "windows") {
                use util::ResultExt;

                cleanup_windows()
                    .await
                    .context("failed to cleanup old directories")
                    .log_err();
            }

            #[cfg(all(target_os = "macos", not(test)))]
            while !cx.background_spawn(cleanup_stale_installer_dirs()).await {
                cx.background_executor().timer(poll_interval).await;
            }

            #[cfg(all(not(target_os = "macos"), not(target_os = "windows"), not(test)))]
            cx.background_spawn(cleanup_stale_installer_dirs()).detach();

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
                    let is_missing_dependency =
                        error.downcast_ref::<MissingDependencyError>().is_some();
                    this.status = match this.update_check_type {
                        UpdateCheckType::Automatic if is_missing_dependency => {
                            log::warn!("auto-update: {}", error);
                            AutoUpdateStatus::Errored {
                                error: Arc::new(error),
                            }
                        }
                        // Be quiet if the check was automated (e.g. when offline)
                        UpdateCheckType::Automatic => {
                            log::info!("auto-update check failed: error:{:?}", error);
                            AutoUpdateStatus::Idle
                        }
                        UpdateCheckType::Manual => {
                            log::error!("auto-update failed: error:{:?}", error);
                            AutoUpdateStatus::Errored {
                                error: Arc::new(error),
                            }
                        }
                    };

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

    fn github_app_asset_name(os: &str, arch: &str) -> Result<String> {
        match os {
            "macos" => Ok(format!("Rezed-{arch}.dmg")),
            "linux" => Ok(format!("rezed-linux-{arch}.tar.gz")),
            "windows" => {
                anyhow::bail!("Rezed GitHub releases do not publish a Windows app update asset yet")
            }
            unsupported_os => anyhow::bail!("not supported: {unsupported_os}"),
        }
    }

    fn release_asset_from_github_release(
        release: GithubRelease,
        os: &str,
        arch: &str,
    ) -> Result<ReleaseAsset> {
        let version = release
            .tag_name
            .strip_prefix('v')
            .with_context(|| {
                format!(
                    "Rezed release tag {:?} must start with 'v'",
                    release.tag_name
                )
            })?
            .to_string();
        let asset_name = Self::github_app_asset_name(os, arch)?;
        let asset = release
            .assets
            .into_iter()
            .find(|asset| asset.name == asset_name)
            .with_context(|| {
                format!(
                    "Rezed release {} does not include app update asset {asset_name}",
                    release.tag_name
                )
            })?;

        Ok(ReleaseAsset {
            version,
            url: asset.browser_download_url,
        })
    }

    async fn get_github_app_release_asset(
        this: &Entity<Self>,
        os: &str,
        arch: &str,
        cx: &mut AsyncApp,
    ) -> Result<ReleaseAsset> {
        let http_client = this.read_with(cx, |this, _| this.client.http_client());
        let mut response = http_client
            .get(
                REZED_GITHUB_LATEST_RELEASE_API_URL,
                Default::default(),
                true,
            )
            .await?;
        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await?;

        anyhow::ensure!(
            response.status().is_success(),
            "failed to fetch Rezed GitHub release: {:?}",
            String::from_utf8_lossy(&body),
        );

        let release =
            serde_json::from_slice::<GithubRelease>(body.as_slice()).with_context(|| {
                format!(
                    "error deserializing Rezed GitHub release {:?}",
                    String::from_utf8_lossy(&body),
                )
            })?;
        Self::release_asset_from_github_release(release, os, arch)
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
        #[cfg(target_os = "macos")]
        let _update_lock = acquire_macos_update_lock()?;

        let (client, installed_version, previous_status, release_channel) =
            this.read_with(cx, |this, cx| {
                (
                    this.client.http_client(),
                    this.current_version.clone(),
                    this.status.clone(),
                    ReleaseChannel::try_global(cx).unwrap_or(ReleaseChannel::Stable),
                )
            });

        Self::check_dependencies()?;

        this.update(cx, |this, cx| {
            this.status = AutoUpdateStatus::Checking;
            log::info!("Auto Update: checking for updates");
            cx.notify();
        });

        let fetched_release_data = Self::get_github_app_release_asset(&this, OS, ARCH, cx).await?;
        let fetched_version = fetched_release_data.clone().version;
        let app_commit_sha = Ok(cx.update(|cx| AppCommitSha::try_global(cx).map(|sha| sha.full())));
        let newer_version = Self::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version,
            previous_status.clone(),
        )?;

        let Some(newer_version) = newer_version else {
            this.update(cx, |this, cx| {
                let status = match previous_status {
                    AutoUpdateStatus::Updated { .. } => previous_status,
                    _ => AutoUpdateStatus::Idle,
                };
                this.status = status;
                cx.notify();
            });
            return Ok(());
        };

        if this.read_with(cx, |this, _| {
            this.should_skip_automatic_install_retry(&newer_version)
        }) {
            log::warn!(
                "skipping automatic retry of Rezed {newer_version} after its installation failed"
            );
            this.update(cx, |this, cx| {
                this.status = AutoUpdateStatus::Idle;
                cx.notify();
            });
            return Ok(());
        }

        this.update(cx, |this, cx| {
            this.status = AutoUpdateStatus::Downloading {
                version: newer_version.clone(),
                progress: None,
            };
            cx.notify();
        });

        let installer_dir = InstallerDir::new()
            .await
            .context("Failed to create installer dir")?;
        let target_path = Self::target_path(&installer_dir).await?;
        let progress_entity = this.clone();
        let mut progress_cx = cx.clone();
        download_release(
            &target_path,
            fetched_release_data,
            client,
            move |progress| {
                progress_entity.update(&mut progress_cx, |this, cx| {
                    if let AutoUpdateStatus::Downloading {
                        progress: current_progress,
                        ..
                    } = &mut this.status
                    {
                        *current_progress = progress;
                        cx.notify();
                    }
                });
            },
        )
        .await
        .with_context(|| format!("Failed to download update to {}", target_path.display()))?;

        this.update(cx, |this, cx| {
            this.status = AutoUpdateStatus::Installing {
                version: newer_version.clone(),
            };
            cx.notify();
        });

        #[cfg(test)]
        let install_result = match cx
            .try_read_global::<tests::InstallOverride, _>(|g, _| g.0.clone())
            .map(|test_install| test_install(&target_path, cx))
        {
            Some(result) => result,
            None => return Ok(()),
        };

        #[cfg(not(test))]
        let install_result = {
            match cx.update(|cx| cx.app_path()) {
                Ok(running_app_path) => {
                    let channel = cx.update(|cx| ReleaseChannel::global(cx).dev_name());
                    cx.background_spawn(Self::install_release(
                        installer_dir,
                        target_path.clone(),
                        running_app_path,
                        channel,
                    ))
                    .await
                }
                Err(error) => Err(error),
            }
        };
        let new_binary_path = match install_result {
            Ok(new_binary_path) => new_binary_path,
            Err(error) => {
                this.update(cx, |this, _| {
                    this.failed_install_version = Some(newer_version.clone());
                });
                log::error!("auto-update installation of Rezed {newer_version} failed: {error:#}");
                return Err(error).with_context(|| {
                    format!("Failed to install update at: {}", target_path.display())
                });
            }
        };
        if let Some(new_binary_path) = new_binary_path {
            cx.update(|cx| cx.set_restart_path(new_binary_path));
        }

        this.update(cx, |this, cx| {
            this.failed_install_version = None;
            this.set_should_show_update_notification(true, cx)
                .detach_and_log_err(cx);
            this.status = AutoUpdateStatus::Updated {
                version: newer_version,
            };
            cx.notify();
        });
        Ok(())
    }

    fn should_skip_automatic_install_retry(&self, version: &Version) -> bool {
        self.update_check_type == UpdateCheckType::Automatic
            && self.failed_install_version.as_ref() == Some(version)
    }

    fn check_if_fetched_version_is_newer(
        release_channel: ReleaseChannel,
        app_commit_sha: Result<Option<String>>,
        installed_version: Version,
        fetched_version: String,
        status: AutoUpdateStatus,
    ) -> Result<Option<Version>> {
        let fetched_version = fetched_version.parse::<Version>()?;

        match release_channel {
            ReleaseChannel::Nightly => {
                let should_download = if let AutoUpdateStatus::Updated { version } = status {
                    fetched_version != version
                } else {
                    let fetched_sha = fetched_version.build.as_str().rsplit('.').next();
                    app_commit_sha
                        .ok()
                        .flatten()
                        .is_none_or(|sha| fetched_sha != Some(sha.as_str()))
                };
                Ok(should_download.then_some(fetched_version))
            }
            _ => {
                let current_version = if let AutoUpdateStatus::Updated { version } = status {
                    version
                } else {
                    installed_version
                };
                Ok(Self::check_if_fetched_version_is_newer_non_nightly(
                    current_version,
                    fetched_version,
                ))
            }
        }
    }

    fn check_dependencies() -> Result<()> {
        #[cfg(target_os = "linux")]
        if which::which("rsync").is_err() {
            let install_hint = linux_rsync_install_hint();
            return Err(MissingDependencyError(format!(
                "rsync is required for auto-updates but is not installed. {install_hint}"
            ))
            .into());
        }

        #[cfg(target_os = "macos")]
        anyhow::ensure!(
            which::which("rsync").is_ok(),
            "Could not auto-update because the required rsync utility was not found."
        );

        Ok(())
    }

    async fn target_path(installer_dir: &InstallerDir) -> Result<PathBuf> {
        let filename = match OS {
            "macos" => anyhow::Ok("Zed.dmg"),
            "linux" => Ok("zed.tar.gz"),
            "windows" => Ok("Zed.exe"),
            unsupported_os => anyhow::bail!("not supported: {unsupported_os}"),
        }?;

        Ok(installer_dir.path().join(filename))
    }

    #[cfg_attr(test, allow(dead_code))]
    async fn install_release(
        installer_dir: InstallerDir,
        target_path: PathBuf,
        running_app_path: PathBuf,
        channel: &str,
    ) -> Result<Option<PathBuf>> {
        match OS {
            "macos" => install_release_macos(installer_dir, &target_path, running_app_path).await,
            "linux" => {
                install_release_linux(&installer_dir, &target_path, channel, running_app_path).await
            }
            "windows" => install_release_windows(&target_path).await,
            unsupported_os => anyhow::bail!("not supported: {unsupported_os}"),
        }
    }

    fn check_if_fetched_version_is_newer_non_nightly(
        mut installed_version: Version,
        fetched_version: Version,
    ) -> Option<Version> {
        // Build metadata does not affect SemVer precedence and includes local build details.
        installed_version.build = semver::BuildMetadata::EMPTY;
        let mut fetched_version = fetched_version;
        fetched_version.build = semver::BuildMetadata::EMPTY;
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

async fn download_release(
    target_path: &Path,
    release: ReleaseAsset,
    client: Arc<HttpClientWithUrl>,
    mut on_progress: impl FnMut(Option<f32>),
) -> Result<()> {
    let mut target_file = File::create(&target_path).await?;

    let mut response = client.get(&release.url, Default::default(), true).await?;
    anyhow::ensure!(
        response.status().is_success(),
        "failed to download update: {:?}",
        response.status()
    );

    let total_bytes = response
        .headers()
        .get(http_client::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|total_bytes| *total_bytes > 0);

    let mut downloaded_bytes: u64 = 0;
    let mut last_reported_percent: Option<u8> = None;
    let mut buffer = [0u8; 8192];
    let body = response.body_mut();
    loop {
        let bytes_read = body.read(&mut buffer).await?;
        if bytes_read == 0 {
            break;
        }
        target_file.write_all(&buffer[..bytes_read]).await?;
        downloaded_bytes += bytes_read as u64;

        if let Some(total_bytes) = total_bytes {
            let fraction = (downloaded_bytes as f32 / total_bytes as f32).clamp(0.0, 1.0);
            // Only report when the whole-number percentage changes to avoid notifying the UI on every chunk.
            let percent = (fraction * 100.0) as u8;
            if last_reported_percent != Some(percent) {
                last_reported_percent = Some(percent);
                on_progress(Some(fraction));
            }
        }
    }
    target_file.flush().await?;
    if total_bytes.is_some() && last_reported_percent != Some(100) {
        on_progress(Some(1.0));
    }
    log::info!("downloaded update. path:{:?}", target_path);

    Ok(())
}

async fn install_release_linux(
    temp_dir: &InstallerDir,
    downloaded_tar_gz: &Path,
    channel: &str,
    running_app_path: PathBuf,
) -> Result<Option<PathBuf>> {
    let home_dir = PathBuf::from(env::var("HOME").context("no HOME env var set")?);

    let extracted = temp_dir.path().join("zed");
    fs::create_dir_all(&extracted)
        .await
        .context("failed to create directory into which to extract update")?;

    let mut cmd = new_command("tar");
    cmd.arg("-xzf")
        .arg(&downloaded_tar_gz)
        .arg("-C")
        .arg(&extracted);
    let output = cmd
        .output()
        .await
        .with_context(|| "failed to extract: {cmd}")?;

    anyhow::ensure!(
        output.status.success(),
        "failed to extract {:?} to {:?}: {:?}",
        downloaded_tar_gz,
        extracted,
        String::from_utf8_lossy(&output.stderr)
    );

    let suffix = if channel != "stable" {
        format!("-{}", channel)
    } else {
        String::default()
    };
    let app_folder_name = format!("zed{}.app", suffix);

    let from = extracted.join(&app_folder_name);
    let mut to = home_dir.join(".local");

    let expected_suffix = format!("{}/libexec/zed-editor", app_folder_name);

    if let Some(prefix) = running_app_path
        .to_str()
        .and_then(|str| str.strip_suffix(&expected_suffix))
    {
        to = PathBuf::from(prefix);
    }

    let mut cmd = new_command("rsync");
    cmd.args(["-av", "--delete"]).arg(&from).arg(&to);
    let output = cmd
        .output()
        .await
        .with_context(|| "failed to rsync: {cmd}")?;

    anyhow::ensure!(
        output.status.success(),
        "failed to copy Zed update from {:?} to {:?}: {:?}",
        from,
        to,
        String::from_utf8_lossy(&output.stderr)
    );

    Ok(Some(to.join(expected_suffix)))
}

#[cfg(target_os = "macos")]
fn acquire_macos_update_lock() -> Result<std::fs::File> {
    acquire_macos_update_lock_at(&env::temp_dir().join(UPDATE_LOCK_FILE))
}

#[cfg(target_os = "macos")]
fn acquire_macos_update_lock_at(path: &Path) -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("failed to open Rezed update lock at {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            anyhow::bail!("another Rezed update is already in progress")
        }
        Err(std::fs::TryLockError::Error(error)) => {
            Err(error).context("failed to acquire Rezed update lock")
        }
    }
}

const CANCELLED_MACOS_INSTALL_CLEANUP_SCRIPT: &str = concat!(
    "for delay in 0 1 2; do ",
    "/bin/sleep \"$delay\"; ",
    "if /usr/bin/hdiutil detach -force \"$1\"; then /bin/rm -rf \"$2\"; exit 0; fi; ",
    "done; ",
    "if [ ! -e \"$1\" ]; then /bin/rm -rf \"$2\"; exit 0; fi; ",
    "mount_device=$(/usr/bin/stat -f %d \"$1\" 2>/dev/null); ",
    "installer_device=$(/usr/bin/stat -f %d \"$2\" 2>/dev/null); ",
    "if [ -n \"$mount_device\" ] && [ \"$mount_device\" = \"$installer_device\" ]; then ",
    "/bin/rm -rf \"$2\"; exit 0; fi; ",
    "/usr/bin/logger -t Rezed \"failed to detach auto-update disk image at $1 during cancellation; retained $2 for startup recovery\"",
);

trait MacOsUpdateCommandRunner {
    async fn attach(&self, downloaded_dmg: &Path, mount_path: &Path) -> Result<()>;
    async fn copy_app(&self, mounted_app_path: &OsStr, running_app_path: &Path) -> Result<()>;
    async fn detach(&self, mount_path: &Path) -> Result<()>;
    fn detach_on_drop(&self, mount_path: &Path, installer_path: &Path) -> Result<()>;
    fn is_mounted(&self, mount_path: &Path) -> bool;
    async fn is_process_running(&self, process_id: u32) -> Result<bool>;
}

struct SystemMacOsUpdateCommandRunner;

impl MacOsUpdateCommandRunner for SystemMacOsUpdateCommandRunner {
    async fn attach(&self, downloaded_dmg: &Path, mount_path: &Path) -> Result<()> {
        let mut command = new_command("hdiutil");
        command
            .args(["attach", "-nobrowse"])
            .arg(downloaded_dmg)
            .arg("-mountpoint")
            .arg(mount_path)
            .kill_on_drop(true);
        let output = command
            .output()
            .await
            .with_context(|| format!("failed to mount disk image with {command:?}"))?;
        anyhow::ensure!(
            output.status.success(),
            "failed to mount disk image at {}: {}",
            mount_path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    async fn copy_app(&self, mounted_app_path: &OsStr, running_app_path: &Path) -> Result<()> {
        let mut command = new_command("rsync");
        command
            .args(["-av", "--delete", "--exclude", "Icon?"])
            .arg(mounted_app_path)
            .arg(running_app_path)
            .kill_on_drop(true);
        let output = command
            .output()
            .await
            .with_context(|| format!("failed to copy app with {command:?}"))?;
        anyhow::ensure!(
            output.status.success(),
            "failed to copy app to {}: {}",
            running_app_path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    async fn detach(&self, mount_path: &Path) -> Result<()> {
        let mut command = new_command("hdiutil");
        command.args(["detach", "-force"]).arg(mount_path);
        let output = command.output().await.with_context(|| {
            format!("failed to run hdiutil detach for {}", mount_path.display())
        })?;
        anyhow::ensure!(
            output.status.success(),
            "failed to detach disk image at {}: {}",
            mount_path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    fn detach_on_drop(&self, mount_path: &Path, installer_path: &Path) -> Result<()> {
        // This child must outlive the app's executors so cancellation during
        // shutdown still detaches the image before removing its temp dir.
        let mut command = new_command("/bin/sh");
        command
            .args([
                "-c",
                CANCELLED_MACOS_INSTALL_CLEANUP_SCRIPT,
                "rezed-auto-update-cleanup",
            ])
            .arg(mount_path)
            .arg(installer_path)
            .stdin(util::command::Stdio::null())
            .stdout(util::command::Stdio::null())
            .stderr(util::command::Stdio::null());
        command.spawn().with_context(|| {
            format!(
                "failed to start detach and cleanup for {} during cancellation",
                mount_path.display()
            )
        })?;
        Ok(())
    }

    fn is_mounted(&self, mount_path: &Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            let Some(parent_path) = mount_path.parent() else {
                return false;
            };
            let Ok(mount_metadata) = std::fs::metadata(mount_path) else {
                return false;
            };
            let Ok(parent_metadata) = std::fs::metadata(parent_path) else {
                return false;
            };
            mount_metadata.dev() != parent_metadata.dev()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    async fn is_process_running(&self, process_id: u32) -> Result<bool> {
        let process_id = process_id.to_string();
        let output = new_command("/bin/ps")
            .args(["-p", process_id.as_str(), "-o", "pid="])
            .output()
            .await
            .context("failed to check Rezed updater process")?;
        Ok(output.status.success() && !output.stdout.is_empty())
    }
}

struct MountedMacOsInstaller<'a, CommandRunner: MacOsUpdateCommandRunner> {
    _installer_dir: InstallerDir,
    mount_path: PathBuf,
    command_runner: &'a CommandRunner,
    needs_detach: bool,
}

impl<'a, CommandRunner: MacOsUpdateCommandRunner> MountedMacOsInstaller<'a, CommandRunner> {
    fn new(
        installer_dir: InstallerDir,
        mount_path: PathBuf,
        command_runner: &'a CommandRunner,
    ) -> Self {
        Self {
            _installer_dir: installer_dir,
            mount_path,
            command_runner,
            needs_detach: true,
        }
    }

    fn is_mounted(&self) -> bool {
        self.command_runner.is_mounted(&self.mount_path)
    }

    fn disarm(&mut self) {
        self.needs_detach = false;
    }

    async fn detach(mut self) -> Result<()> {
        let detach_result = self.command_runner.detach(&self.mount_path).await;
        if detach_result.is_ok() || !self.is_mounted() {
            self.needs_detach = false;
        }
        detach_result?;
        log::info!(
            "detached auto-update disk image at {}",
            self.mount_path.display()
        );
        Ok(())
    }
}

impl<CommandRunner: MacOsUpdateCommandRunner> Drop for MountedMacOsInstaller<'_, CommandRunner> {
    fn drop(&mut self) {
        if !self.needs_detach {
            return;
        }
        let installer_path = self._installer_dir.path().to_owned();
        self._installer_dir.keep_for_external_cleanup();
        match self
            .command_runner
            .detach_on_drop(&self.mount_path, &installer_path)
        {
            Ok(()) => log::info!(
                "started detaching auto-update disk image at {} and cleaning {} during cancellation",
                self.mount_path.display(),
                installer_path.display()
            ),
            Err(error) => log::error!(
                "failed to detach auto-update disk image at {} during cleanup: {error:#}",
                self.mount_path.display()
            ),
        }
    }
}

async fn install_release_macos(
    installer_dir: InstallerDir,
    downloaded_dmg: &Path,
    running_app_path: PathBuf,
) -> Result<Option<PathBuf>> {
    install_release_macos_with_runner(
        installer_dir,
        downloaded_dmg,
        running_app_path,
        &SystemMacOsUpdateCommandRunner,
    )
    .await
}

async fn install_release_macos_with_runner<CommandRunner: MacOsUpdateCommandRunner>(
    installer_dir: InstallerDir,
    downloaded_dmg: &Path,
    running_app_path: PathBuf,
    command_runner: &CommandRunner,
) -> Result<Option<PathBuf>> {
    let running_app_filename = running_app_path
        .file_name()
        .with_context(|| format!("invalid running app path {running_app_path:?}"))?;

    let mount_path = installer_dir.path().join("mount");
    let mut mounted_app_path: OsString = mount_path.join(running_app_filename).into();
    mounted_app_path.push("/");
    let mut mounted_installer =
        MountedMacOsInstaller::new(installer_dir, mount_path, command_runner);

    if let Err(error) = command_runner
        .attach(downloaded_dmg, &mounted_installer.mount_path)
        .await
    {
        if !mounted_installer.is_mounted() {
            mounted_installer.disarm();
            return Err(error);
        }
        return match mounted_installer.detach().await {
            Ok(()) => Err(error),
            Err(detach_error) => anyhow::bail!(
                "failed to attach disk image: {error:#}; additionally failed to detach its partial mount: {detach_error:#}"
            ),
        };
    }

    let copy_result = command_runner
        .copy_app(&mounted_app_path, &running_app_path)
        .await;
    let detach_result = mounted_installer.detach().await;

    match (copy_result, detach_result) {
        (Ok(()), Ok(())) => Ok(None),
        (Err(copy_error), Ok(())) => Err(copy_error),
        (Ok(()), Err(detach_error)) => Err(detach_error),
        (Err(copy_error), Err(detach_error)) => anyhow::bail!(
            "failed to copy app: {copy_error:#}; additionally failed to detach disk image: {detach_error:#}"
        ),
    }
}

#[cfg(target_os = "macos")]
async fn cleanup_macos_stale_installer_dirs_in<CommandRunner: MacOsUpdateCommandRunner>(
    temp_dir: &Path,
    stale_before: SystemTime,
    command_runner: &CommandRunner,
) -> Result<bool> {
    let mut entries = fs::read_dir(temp_dir).await?;
    let mut cleanup_succeeded = true;
    while let Some(entry) = entries.next().await {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                log::warn!("failed to read an installer dir entry: {error}");
                continue;
            }
        };
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let is_directory = entry
            .file_type()
            .await
            .ok()
            .is_some_and(|file_type| file_type.is_dir());
        if !is_directory {
            continue;
        }

        let marked_process_id = if file_name.starts_with(INSTALLER_DIR_PREFIX) {
            fs::read(path.join(INSTALLER_MARKER_FILE))
                .await
                .ok()
                .and_then(|contents| installer_marker_process_id(&contents))
        } else {
            None
        };
        if let Some(process_id) = marked_process_id {
            match command_runner.is_process_running(process_id).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => {
                    cleanup_succeeded = false;
                    log::warn!(
                        "failed to determine whether Rezed updater process {process_id} still owns {}: {error:#}",
                        path.display()
                    );
                    continue;
                }
            }
        }
        let legacy_mount_path = path.join("Rezed");
        let is_legacy_rezed_dir = marked_process_id.is_none()
            && file_name.starts_with(LEGACY_INSTALLER_DIR_PREFIX)
            && entry
                .metadata()
                .await
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .is_some_and(|modified| modified <= stale_before)
            && std::fs::symlink_metadata(path.join("Zed.dmg"))
                .is_ok_and(|metadata| metadata.is_file())
            && std::fs::symlink_metadata(legacy_mount_path.join("Rezed.app"))
                .is_ok_and(|metadata| metadata.is_dir())
            && command_runner.is_mounted(&legacy_mount_path);

        let mount_path = if marked_process_id.is_some() {
            let mount_path = path.join("mount");
            command_runner.is_mounted(&mount_path).then_some(mount_path)
        } else if is_legacy_rezed_dir {
            Some(legacy_mount_path)
        } else {
            continue;
        };

        if let Some(mount_path) = mount_path {
            match command_runner.detach(&mount_path).await {
                Ok(()) => log::info!(
                    "detached stale Rezed updater mount at {}",
                    mount_path.display()
                ),
                Err(error) if command_runner.is_mounted(&mount_path) => {
                    cleanup_succeeded = false;
                    log::error!(
                        "failed to detach stale Rezed updater mount at {}: {error:#}",
                        mount_path.display()
                    );
                    continue;
                }
                Err(error) => log::warn!(
                    "detach reported an error for stale Rezed updater mount at {}, but it is no longer mounted: {error:#}",
                    mount_path.display()
                ),
            }
        }

        if let Err(error) = fs::remove_dir_all(&path).await {
            cleanup_succeeded = false;
            log::warn!(
                "failed to remove stale Rezed installer dir {}: {error}",
                path.display()
            );
        } else {
            log::info!("removed stale Rezed installer dir {}", path.display());
        }
    }
    Ok(cleanup_succeeded)
}

#[cfg(target_os = "macos")]
fn installer_marker_content(process_id: u32) -> String {
    format!("{INSTALLER_MARKER_PREFIX}{process_id}\n")
}

#[cfg(target_os = "macos")]
fn installer_marker_process_id(contents: &[u8]) -> Option<u32> {
    std::str::from_utf8(contents)
        .ok()?
        .strip_prefix(INSTALLER_MARKER_PREFIX)?
        .strip_suffix('\n')?
        .parse()
        .ok()
}

#[cfg(any(rust_analyzer, all(not(target_os = "windows"), not(test))))]
async fn cleanup_stale_installer_dirs() -> bool {
    #[cfg(target_os = "macos")]
    {
        const STALE_INSTALLER_DIR_AGE: Duration = Duration::from_secs(24 * 60 * 60);
        let stale_before = SystemTime::now()
            .checked_sub(STALE_INSTALLER_DIR_AGE)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        match cleanup_macos_stale_installer_dirs_in(
            &env::temp_dir(),
            stale_before,
            &SystemMacOsUpdateCommandRunner,
        )
        .await
        {
            Ok(true) => return true,
            Ok(false) => {
                log::warn!(
                    "some Rezed installer artifacts could not be cleaned up; automatic updates will remain paused while cleanup is retried"
                );
                return false;
            }
            Err(error) => {
                log::warn!("failed to clean up stale Rezed installer dirs: {error:#}");
                return false;
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        const STALE_INSTALLER_DIR_AGE: Duration = Duration::from_secs(24 * 60 * 60);
        let temp_dir = env::temp_dir();
        let Ok(mut entries) = fs::read_dir(&temp_dir).await else {
            log::warn!("failed to read temp dir {temp_dir:?} while cleaning up installer dirs");
            return false;
        };
        while let Some(Ok(entry)) = entries.next().await {
            let path = entry.path();
            let is_stale_rezed_dir = entry
                .file_name()
                .to_string_lossy()
                .starts_with(INSTALLER_DIR_PREFIX)
                && entry.metadata().await.ok().is_some_and(|metadata| {
                    metadata.is_dir()
                        && metadata.modified().ok().is_some_and(|modified| {
                            SystemTime::now()
                                .duration_since(modified)
                                .is_ok_and(|age| age > STALE_INSTALLER_DIR_AGE)
                        })
                });
            if is_stale_rezed_dir {
                if let Err(error) = fs::remove_dir_all(&path).await {
                    log::warn!("failed to remove stale installer dir {path:?}: {error}");
                }
            }
        }
        true
    }
}

async fn cleanup_windows() -> Result<()> {
    let parent = std::env::current_exe()?
        .parent()
        .context("No parent dir for Zed.exe")?
        .to_owned();

    // keep in sync with crates/auto_update_helper/src/updater.rs
    _ = smol::fs::remove_dir(parent.join("updates")).await;
    _ = smol::fs::remove_dir(parent.join("install")).await;
    _ = smol::fs::remove_dir(parent.join("old")).await;

    Ok(())
}

async fn install_release_windows(downloaded_installer: &Path) -> Result<Option<PathBuf>> {
    let mut cmd = new_command(downloaded_installer);
    cmd.arg("/verysilent")
        .arg("/update=true")
        .arg("/MERGETASKS=!desktopicon");
    let output = cmd.output().await?;
    anyhow::ensure!(
        output.status.success(),
        "failed to start installer: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    // We return the path to the update helper program, because it will
    // perform the final steps of the update process, copying the new binary,
    // deleting the old one, and launching the new binary.
    let helper_path = std::env::current_exe()?
        .parent()
        .context("No parent dir for Zed.exe")?
        .join("tools")
        .join("auto_update_helper.exe");
    Ok(Some(helper_path))
}

pub async fn finalize_auto_update_on_quit() {
    let Some(installer_path) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.join("updates")))
    else {
        return;
    };

    // The installer will create a flag file after it finishes updating
    let flag_file = installer_path.join("versions.txt");
    if flag_file.exists()
        && let Some(helper) = installer_path
            .parent()
            .map(|p| p.join("tools").join("auto_update_helper.exe"))
    {
        let mut command = util::command::new_command(helper);
        command.arg("--launch");
        command.arg("false");
        if let Ok(mut cmd) = command.spawn() {
            _ = cmd.status().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use client::Client;
    use clock::FakeSystemClock;
    use futures::channel::oneshot;
    use gpui::TestAppContext;
    use http_client::{FakeHttpClient, Response};
    use settings::default_settings;
    #[cfg(target_os = "macos")]
    use std::collections::HashSet;
    use std::{
        rc::Rc,
        sync::{
            Arc,
            atomic::{self, AtomicBool},
        },
    };
    use tempfile::tempdir;

    #[ctor::ctor(unsafe)]
    fn init_logger() {
        zlog::init_test();
    }

    use super::*;

    pub(super) struct InstallOverride(pub Rc<dyn Fn(&Path, &AsyncApp) -> Result<Option<PathBuf>>>);
    impl Global for InstallOverride {}

    #[cfg(target_os = "macos")]
    #[derive(Debug, PartialEq)]
    enum MacOsCommandCall {
        Attach {
            downloaded_dmg: PathBuf,
            mount_path: PathBuf,
        },
        Copy {
            mounted_app_path: OsString,
            running_app_path: PathBuf,
        },
        Detach {
            mount_path: PathBuf,
        },
    }

    #[cfg(target_os = "macos")]
    #[derive(Default)]
    struct FakeMacOsUpdateCommandRunner {
        calls: parking_lot::Mutex<Vec<MacOsCommandCall>>,
        mounted_paths: parking_lot::Mutex<HashSet<PathBuf>>,
        running_processes: parking_lot::Mutex<HashSet<u32>>,
        fail_attach: AtomicBool,
        fail_copy: AtomicBool,
        fail_detach: AtomicBool,
    }

    #[cfg(target_os = "macos")]
    impl FakeMacOsUpdateCommandRunner {
        fn mark_mounted(&self, mount_path: &Path) {
            self.mounted_paths.lock().insert(mount_path.to_owned());
        }

        fn calls(&self) -> parking_lot::MutexGuard<'_, Vec<MacOsCommandCall>> {
            self.calls.lock()
        }

        fn detach_impl(&self, mount_path: &Path) -> Result<()> {
            self.calls.lock().push(MacOsCommandCall::Detach {
                mount_path: mount_path.to_owned(),
            });
            anyhow::ensure!(
                !self.fail_detach.load(atomic::Ordering::SeqCst),
                "fake detach failure"
            );
            self.mounted_paths.lock().remove(mount_path);
            match std::fs::remove_dir_all(mount_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            Ok(())
        }
    }

    #[cfg(target_os = "macos")]
    impl MacOsUpdateCommandRunner for FakeMacOsUpdateCommandRunner {
        async fn attach(&self, downloaded_dmg: &Path, mount_path: &Path) -> Result<()> {
            self.calls.lock().push(MacOsCommandCall::Attach {
                downloaded_dmg: downloaded_dmg.to_owned(),
                mount_path: mount_path.to_owned(),
            });
            std::fs::create_dir_all(mount_path.join("Rezed.app"))?;
            self.mark_mounted(mount_path);
            anyhow::ensure!(
                !self.fail_attach.load(atomic::Ordering::SeqCst),
                "fake attach failure"
            );
            Ok(())
        }

        async fn copy_app(&self, mounted_app_path: &OsStr, running_app_path: &Path) -> Result<()> {
            self.calls.lock().push(MacOsCommandCall::Copy {
                mounted_app_path: mounted_app_path.to_owned(),
                running_app_path: running_app_path.to_owned(),
            });
            anyhow::ensure!(
                !self.fail_copy.load(atomic::Ordering::SeqCst),
                "fake copy failure"
            );
            Ok(())
        }

        async fn detach(&self, mount_path: &Path) -> Result<()> {
            self.detach_impl(mount_path)
        }

        fn detach_on_drop(&self, mount_path: &Path, installer_path: &Path) -> Result<()> {
            self.detach_impl(mount_path)?;
            std::fs::remove_dir_all(installer_path)?;
            Ok(())
        }

        fn is_mounted(&self, mount_path: &Path) -> bool {
            self.mounted_paths.lock().contains(mount_path)
        }

        async fn is_process_running(&self, process_id: u32) -> Result<bool> {
            Ok(self.running_processes.lock().contains(&process_id))
        }
    }

    #[cfg(target_os = "macos")]
    fn write_installer_marker(path: &Path, process_id: u32) -> Result<()> {
        std::fs::write(
            path.join(INSTALLER_MARKER_FILE),
            installer_marker_content(process_id),
        )?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn test_installer_dir(parent: &Path) -> Result<InstallerDir> {
        let temp_dir = tempfile::Builder::new()
            .prefix(INSTALLER_DIR_PREFIX)
            .tempdir_in(parent)?;
        let path = temp_dir.path().to_owned();
        write_installer_marker(&path, std::process::id())?;
        Ok(InstallerDir {
            path,
            temp_dir: Some(temp_dir),
        })
    }

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
    async fn test_auto_update_downloads(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        zlog::init_test();
        let release_available = Arc::new(AtomicBool::new(false));

        let (dmg_tx, dmg_rx) = oneshot::channel::<String>();

        cx.update(|cx| {
            settings::init(cx);
            cx.set_global(db::AppDatabase::test_new());

            let current_version = semver::Version::new(0, 100, 0);
            release_channel::init_test(current_version, ReleaseChannel::Stable, cx);

            let clock = Arc::new(FakeSystemClock::new());
            let asset_name = AutoUpdater::github_app_asset_name(OS, ARCH).unwrap();
            let release_available = Arc::clone(&release_available);
            let dmg_rx = Arc::new(parking_lot::Mutex::new(Some(dmg_rx)));
            let fake_client_http = FakeHttpClient::create(move |req| {
                let asset_name = asset_name.clone();
                let release_available = release_available.load(atomic::Ordering::Relaxed);
                let dmg_rx = dmg_rx.clone();
                async move {
                if req.uri().path() == "/repos/nguyenphutrong/rezed/releases/latest" {
                    if release_available {
                        return Ok(Response::builder().status(200).body(format!(
                            r#"{{"tag_name":"v0.100.1","assets":[{{"name":"{asset_name}","browser_download_url":"https://test.example/new-download"}}]}}"#
                        ).into()).unwrap());
                    } else {
                        return Ok(Response::builder().status(200).body(format!(
                            r#"{{"tag_name":"v0.100.0","assets":[{{"name":"{asset_name}","browser_download_url":"https://test.example/old-download"}}]}}"#
                        ).into()).unwrap());
                    }
                } else if req.uri().path() == "/new-download" {
                    return Ok(Response::builder().status(200).body({
                        let dmg_rx = dmg_rx.lock().take().unwrap();
                        dmg_rx.await.unwrap().into()
                    }).unwrap());
                }
                Ok(Response::builder().status(404).body("".into()).unwrap())
                }
            });
            let client = Client::new(clock, fake_client_http, cx);
            crate::init(client, cx);
        });

        let auto_updater = cx.update(|cx| AutoUpdater::get(cx).expect("auto updater should exist"));

        cx.background_executor.run_until_parked();

        auto_updater.read_with(cx, |updater, _| {
            assert_eq!(updater.status(), AutoUpdateStatus::Idle);
            assert_eq!(updater.current_version(), semver::Version::new(0, 100, 0));
        });

        auto_updater.update(cx, |updater, cx| {
            let failed_version = semver::Version::new(0, 100, 1);
            updater.failed_install_version = Some(failed_version.clone());
            updater.pending_poll = Some(Task::ready(None));
            updater.poll(UpdateCheckType::Manual, cx);
            assert!(!updater.should_skip_automatic_install_retry(&failed_version));
            updater.pending_poll = None;
            updater.failed_install_version = None;
            updater.update_check_type = UpdateCheckType::Automatic;
        });

        release_available.store(true, atomic::Ordering::SeqCst);
        auto_updater.update(cx, |updater, _| {
            updater.failed_install_version = Some(semver::Version::new(0, 100, 1));
        });
        cx.background_executor.advance_clock(POLL_INTERVAL);
        cx.background_executor.run_until_parked();
        assert_eq!(
            auto_updater.read_with(cx, |updater, _| updater.status()),
            AutoUpdateStatus::Idle
        );

        auto_updater.update(cx, |updater, cx| {
            updater.poll(UpdateCheckType::Manual, cx);
        });

        loop {
            cx.background_executor.timer(Duration::from_millis(0)).await;
            cx.run_until_parked();
            let status = auto_updater.read_with(cx, |updater, _| updater.status());
            if !matches!(status, AutoUpdateStatus::Idle) {
                break;
            }
        }
        let status = auto_updater.read_with(cx, |updater, _| updater.status());
        assert_eq!(
            status,
            AutoUpdateStatus::Downloading {
                version: semver::Version::new(0, 100, 1),
                progress: None,
            }
        );

        auto_updater.update(cx, |updater, cx| {
            updater.poll(UpdateCheckType::Automatic, cx);
            updater.poll(UpdateCheckType::Manual, cx);
        });
        assert_eq!(
            auto_updater.read_with(cx, |updater, _| updater.update_check_type()),
            UpdateCheckType::Manual
        );
        assert!(!auto_updater.read_with(cx, |updater, _| {
            updater.should_skip_automatic_install_retry(&semver::Version::new(0, 100, 1))
        }));
        assert_eq!(
            auto_updater.read_with(cx, |updater, _| updater.status()),
            AutoUpdateStatus::Downloading {
                version: semver::Version::new(0, 100, 1),
                progress: None,
            }
        );

        dmg_tx.send("<fake-zed-update>".to_owned()).unwrap();

        let tmp_dir = Arc::new(tempdir().unwrap());

        cx.update(|cx| {
            let tmp_dir = tmp_dir.clone();
            cx.set_global(InstallOverride(Rc::new(move |target_path, _cx| {
                let tmp_dir = tmp_dir.clone();
                let dest_path = tmp_dir.path().join("zed");
                std::fs::copy(&target_path, &dest_path)?;
                Ok(Some(dest_path))
            })));
        });

        loop {
            cx.background_executor.timer(Duration::from_millis(0)).await;
            cx.run_until_parked();
            let status = auto_updater.read_with(cx, |updater, _| updater.status());
            if !matches!(status, AutoUpdateStatus::Downloading { .. }) {
                break;
            }
        }
        let status = auto_updater.read_with(cx, |updater, _| updater.status());
        assert_eq!(
            status,
            AutoUpdateStatus::Updated {
                version: semver::Version::new(0, 100, 1)
            }
        );
        let will_restart = cx.expect_restart();
        cx.update(|cx| cx.restart());
        let path = will_restart.await.unwrap().unwrap();
        assert_eq!(path, tmp_dir.path().join("zed"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "<fake-zed-update>");
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    async fn test_macos_install_uses_fixed_mountpoint_and_cleans_up(_cx: &mut TestAppContext) {
        let installer_root = tempdir().unwrap();
        let installer_dir = test_installer_dir(installer_root.path()).unwrap();
        let downloaded_dmg = installer_dir.path().join("Zed.dmg");
        std::fs::write(&downloaded_dmg, "fake dmg").unwrap();
        let running_app_root = tempdir().unwrap();
        let running_app_path = running_app_root.path().join("Rezed.app");
        std::fs::create_dir(&running_app_path).unwrap();
        let mount_path = installer_dir.path().join("mount");
        let mut mounted_app_path: OsString = mount_path.join("Rezed.app").into();
        mounted_app_path.push("/");
        let command_runner = FakeMacOsUpdateCommandRunner::default();

        install_release_macos_with_runner(
            installer_dir,
            &downloaded_dmg,
            running_app_path.clone(),
            &command_runner,
        )
        .await
        .unwrap();

        assert_eq!(
            command_runner.calls().as_slice(),
            [
                MacOsCommandCall::Attach {
                    downloaded_dmg,
                    mount_path: mount_path.clone(),
                },
                MacOsCommandCall::Copy {
                    mounted_app_path,
                    running_app_path,
                },
                MacOsCommandCall::Detach { mount_path },
            ]
        );
        assert_eq!(std::fs::read_dir(installer_root.path()).unwrap().count(), 0);
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    async fn test_macos_installer_dir_cleans_up_after_download_failure(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let installer_root = tempdir().unwrap();
        let installer_dir = test_installer_dir(installer_root.path()).unwrap();
        let target_path = installer_dir.path().join("Zed.dmg");
        let client = FakeHttpClient::create(|_| async move {
            Ok(Response::builder()
                .status(500)
                .body("failed".into())
                .unwrap())
        });

        let error = download_release(
            &target_path,
            ReleaseAsset {
                version: "1.0.0".to_string(),
                url: "https://test.example/download".to_string(),
            },
            client,
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("failed to download update"));

        drop(installer_dir);
        assert_eq!(std::fs::read_dir(installer_root.path()).unwrap().count(), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_update_lock_prevents_concurrent_attempts() {
        let temp_root = tempdir().unwrap();
        let lock_path = temp_root.path().join(UPDATE_LOCK_FILE);
        let first_lock = acquire_macos_update_lock_at(&lock_path).unwrap();

        let error = acquire_macos_update_lock_at(&lock_path).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("another Rezed update is already in progress")
        );

        drop(first_lock);
        acquire_macos_update_lock_at(&lock_path).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    async fn test_macos_install_detaches_and_cleans_up_after_copy_failures(
        _cx: &mut TestAppContext,
    ) {
        let installer_root = tempdir().unwrap();
        let running_app_root = tempdir().unwrap();
        let running_app_path = running_app_root.path().join("Rezed.app");
        std::fs::create_dir(&running_app_path).unwrap();
        let command_runner = FakeMacOsUpdateCommandRunner::default();
        command_runner
            .fail_copy
            .store(true, atomic::Ordering::SeqCst);

        for _ in 0..3 {
            let installer_dir = test_installer_dir(installer_root.path()).unwrap();
            let downloaded_dmg = installer_dir.path().join("Zed.dmg");
            std::fs::write(&downloaded_dmg, "fake dmg").unwrap();

            let error = install_release_macos_with_runner(
                installer_dir,
                &downloaded_dmg,
                running_app_path.clone(),
                &command_runner,
            )
            .await
            .unwrap_err();

            assert!(error.to_string().contains("fake copy failure"));
            assert_eq!(std::fs::read_dir(installer_root.path()).unwrap().count(), 0);
        }
        assert_eq!(
            command_runner
                .calls()
                .iter()
                .filter(|call| matches!(call, MacOsCommandCall::Detach { .. }))
                .count(),
            3
        );
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    async fn test_macos_install_detaches_partial_mount_after_attach_failure(
        _cx: &mut TestAppContext,
    ) {
        let installer_root = tempdir().unwrap();
        let installer_dir = test_installer_dir(installer_root.path()).unwrap();
        let downloaded_dmg = installer_dir.path().join("Zed.dmg");
        std::fs::write(&downloaded_dmg, "fake dmg").unwrap();
        let running_app_path = tempdir().unwrap().path().join("Rezed.app");
        let command_runner = FakeMacOsUpdateCommandRunner::default();
        command_runner
            .fail_attach
            .store(true, atomic::Ordering::SeqCst);

        let error = install_release_macos_with_runner(
            installer_dir,
            &downloaded_dmg,
            running_app_path,
            &command_runner,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("fake attach failure"));
        assert!(
            command_runner
                .calls()
                .iter()
                .any(|call| matches!(call, MacOsCommandCall::Detach { .. }))
        );
        assert_eq!(std::fs::read_dir(installer_root.path()).unwrap().count(), 0);
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    async fn test_macos_installer_detaches_when_cancelled(_cx: &mut TestAppContext) {
        let installer_root = tempdir().unwrap();
        let installer_dir = test_installer_dir(installer_root.path()).unwrap();
        let downloaded_dmg = installer_dir.path().join("Zed.dmg");
        std::fs::write(&downloaded_dmg, "fake dmg").unwrap();
        let mount_path = installer_dir.path().join("mount");
        let command_runner = FakeMacOsUpdateCommandRunner::default();
        command_runner
            .attach(&downloaded_dmg, &mount_path)
            .await
            .unwrap();

        drop(MountedMacOsInstaller::new(
            installer_dir,
            mount_path.clone(),
            &command_runner,
        ));

        assert!(
            command_runner.calls().iter().any(
                |call| matches!(call, MacOsCommandCall::Detach { mount_path: path } if path == &mount_path)
            )
        );
        assert_eq!(std::fs::read_dir(installer_root.path()).unwrap().count(), 0);
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    async fn test_stale_cleanup_only_removes_confirmed_rezed_installer_dirs(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let temp_root = tempdir().unwrap();
        let command_runner = FakeMacOsUpdateCommandRunner::default();

        let marked_dir = temp_root.path().join("rezed-auto-update-marked");
        let marked_mount = marked_dir.join("mount");
        std::fs::create_dir_all(marked_mount.join("Rezed.app")).unwrap();
        write_installer_marker(&marked_dir, 1000).unwrap();
        command_runner.mark_mounted(&marked_mount);

        let live_marked_dir = temp_root.path().join("rezed-auto-update-live");
        let live_marked_mount = live_marked_dir.join("mount");
        std::fs::create_dir_all(live_marked_mount.join("Rezed.app")).unwrap();
        write_installer_marker(&live_marked_dir, 1001).unwrap();
        command_runner.mark_mounted(&live_marked_mount);
        command_runner.running_processes.lock().insert(1001);

        let legacy_dir = temp_root.path().join("zed-auto-update-legacy");
        let legacy_mount = legacy_dir.join("Rezed");
        std::fs::create_dir_all(legacy_mount.join("Rezed.app")).unwrap();
        std::fs::write(legacy_dir.join("Zed.dmg"), "fake dmg").unwrap();
        command_runner.mark_mounted(&legacy_mount);

        let unrelated_dir = temp_root.path().join("zed-auto-update-unrelated");
        let unrelated_mount = unrelated_dir.join("Zed");
        std::fs::create_dir_all(unrelated_mount.join("Zed.app")).unwrap();
        std::fs::write(unrelated_dir.join("Zed.dmg"), "other app dmg").unwrap();
        command_runner.mark_mounted(&unrelated_mount);

        let invalid_marker_dir = temp_root.path().join("rezed-auto-update-invalid-marker");
        let invalid_marker_mount = invalid_marker_dir.join("mount");
        std::fs::create_dir_all(invalid_marker_mount.join("Rezed.app")).unwrap();
        std::fs::write(
            invalid_marker_dir.join(INSTALLER_MARKER_FILE),
            "another updater",
        )
        .unwrap();
        command_runner.mark_mounted(&invalid_marker_mount);

        cleanup_macos_stale_installer_dirs_in(
            temp_root.path(),
            SystemTime::UNIX_EPOCH,
            &command_runner,
        )
        .await
        .unwrap();
        assert!(!marked_dir.exists());
        assert!(live_marked_dir.exists());
        assert!(legacy_dir.exists());
        assert_eq!(
            command_runner.calls().as_slice(),
            [MacOsCommandCall::Detach {
                mount_path: marked_mount.clone()
            }]
        );

        cleanup_macos_stale_installer_dirs_in(
            temp_root.path(),
            SystemTime::now()
                .checked_add(Duration::from_secs(1))
                .unwrap(),
            &command_runner,
        )
        .await
        .unwrap();

        assert!(!marked_dir.exists());
        assert!(live_marked_dir.exists());
        assert!(!legacy_dir.exists());
        assert!(unrelated_dir.exists());
        assert!(invalid_marker_dir.exists());
        assert_eq!(
            command_runner
                .calls()
                .iter()
                .filter_map(|call| match call {
                    MacOsCommandCall::Detach { mount_path } => Some(mount_path.clone()),
                    _ => None,
                })
                .collect::<HashSet<_>>(),
            HashSet::from([marked_mount, legacy_mount])
        );
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    async fn test_stale_cleanup_retains_mount_when_detach_fails(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let temp_root = tempdir().unwrap();
        let command_runner = FakeMacOsUpdateCommandRunner::default();
        let marked_dir = temp_root.path().join("rezed-auto-update-marked");
        let marked_mount = marked_dir.join("mount");
        std::fs::create_dir_all(marked_mount.join("Rezed.app")).unwrap();
        write_installer_marker(&marked_dir, 1000).unwrap();
        command_runner.mark_mounted(&marked_mount);
        command_runner
            .fail_detach
            .store(true, atomic::Ordering::SeqCst);

        assert!(
            !cleanup_macos_stale_installer_dirs_in(
                temp_root.path(),
                SystemTime::now(),
                &command_runner
            )
            .await
            .unwrap()
        );
        assert!(marked_dir.exists());

        command_runner
            .fail_detach
            .store(false, atomic::Ordering::SeqCst);
        assert!(
            cleanup_macos_stale_installer_dirs_in(
                temp_root.path(),
                SystemTime::now(),
                &command_runner
            )
            .await
            .unwrap()
        );
        assert!(!marked_dir.exists());
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    async fn test_macos_install_retains_mount_until_failed_detach_can_be_recovered(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let installer_root = tempdir().unwrap();
        let installer_dir = test_installer_dir(installer_root.path()).unwrap();
        let installer_path = installer_dir.path().to_owned();
        let downloaded_dmg = installer_path.join("Zed.dmg");
        std::fs::write(&downloaded_dmg, "fake dmg").unwrap();
        let running_app_path = tempdir().unwrap().path().join("Rezed.app");
        let command_runner = FakeMacOsUpdateCommandRunner::default();
        command_runner
            .fail_detach
            .store(true, atomic::Ordering::SeqCst);

        let error = install_release_macos_with_runner(
            installer_dir,
            &downloaded_dmg,
            running_app_path,
            &command_runner,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("fake detach failure"));
        assert!(installer_path.exists());
        assert_eq!(
            command_runner
                .calls()
                .iter()
                .filter(|call| matches!(call, MacOsCommandCall::Detach { .. }))
                .count(),
            2
        );

        command_runner
            .fail_detach
            .store(false, atomic::Ordering::SeqCst);
        cleanup_macos_stale_installer_dirs_in(
            installer_root.path(),
            SystemTime::now()
                .checked_add(Duration::from_secs(1))
                .unwrap(),
            &command_runner,
        )
        .await
        .unwrap();
        assert!(!installer_path.exists());
    }

    #[gpui::test]
    async fn test_download_release_reports_progress(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();

        let body = vec![0u8; 20_000];
        let content_length = body.len();

        let client = FakeHttpClient::create(move |_req| {
            let body = body.clone();
            async move {
                Ok(Response::builder()
                    .status(200)
                    .header(
                        http_client::http::header::CONTENT_LENGTH,
                        body.len().to_string(),
                    )
                    .body(body.into())
                    .unwrap())
            }
        });

        let temp_dir = tempdir().unwrap();
        let target_path = temp_dir.path().join("zed-download");
        let release = ReleaseAsset {
            version: "1.0.0".to_string(),
            url: "https://test.example/download".to_string(),
        };

        let reported = Rc::new(std::cell::RefCell::new(Vec::<f32>::new()));
        download_release(&target_path, release, client, {
            let reported = reported.clone();
            move |fraction| {
                if let Some(fraction) = fraction {
                    reported.borrow_mut().push(fraction);
                }
            }
        })
        .await
        .unwrap();

        let reported = reported.borrow();
        assert!(
            reported.len() >= 2,
            "expected progress to be reported across multiple reads, got {reported:?}"
        );
        assert_eq!(
            reported.last().copied(),
            Some(1.0),
            "download should finish at 100%"
        );
        for fraction in reported.iter() {
            assert!(
                (0.0..=1.0).contains(fraction),
                "progress {fraction} out of range"
            );
        }
        for pair in reported.windows(2) {
            assert!(
                pair[0] <= pair[1],
                "progress must not decrease: {reported:?}"
            );
        }

        let downloaded_len = std::fs::metadata(&target_path).unwrap().len();
        assert_eq!(downloaded_len, content_length as u64);
    }

    #[gpui::test]
    async fn test_download_release_without_content_length_reports_no_progress(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();

        let body = vec![0u8; 20_000];
        let content_length = body.len();

        let client = FakeHttpClient::create(move |_req| {
            let body = body.clone();
            async move { Ok(Response::builder().status(200).body(body.into()).unwrap()) }
        });

        let temp_dir = tempdir().unwrap();
        let target_path = temp_dir.path().join("zed-download");
        let release = ReleaseAsset {
            version: "1.0.0".to_string(),
            url: "https://test.example/download".to_string(),
        };

        let reported = Rc::new(std::cell::RefCell::new(Vec::<Option<f32>>::new()));
        download_release(&target_path, release, client, {
            let reported = reported.clone();
            move |fraction| {
                reported.borrow_mut().push(fraction);
            }
        })
        .await
        .unwrap();

        assert!(
            reported.borrow().is_empty(),
            "progress should not be reported when the total size is unknown, got {:?}",
            reported.borrow()
        );

        let downloaded_len = std::fs::metadata(&target_path).unwrap().len();
        assert_eq!(downloaded_len, content_length as u64);
    }

    #[test]
    fn test_github_release_asset_selects_linux_asset() {
        let release_asset = AutoUpdater::release_asset_from_github_release(
            GithubRelease {
                tag_name: "v1.9.0-rezed.4".to_string(),
                assets: vec![GithubReleaseAsset {
                    name: "rezed-linux-x86_64.tar.gz".to_string(),
                    browser_download_url: "https://example.com/rezed-linux-x86_64.tar.gz"
                        .to_string(),
                }],
            },
            "linux",
            "x86_64",
        )
        .unwrap();

        assert_eq!(release_asset.version, "1.9.0-rezed.4");
        assert_eq!(
            release_asset.url,
            "https://example.com/rezed-linux-x86_64.tar.gz"
        );
    }

    #[test]
    fn test_github_release_asset_selects_macos_asset() {
        let release_asset = AutoUpdater::release_asset_from_github_release(
            GithubRelease {
                tag_name: "v1.9.0-rezed.4".to_string(),
                assets: vec![GithubReleaseAsset {
                    name: "Rezed-aarch64.dmg".to_string(),
                    browser_download_url: "https://example.com/Rezed-aarch64.dmg".to_string(),
                }],
            },
            "macos",
            "aarch64",
        )
        .unwrap();

        assert_eq!(release_asset.version, "1.9.0-rezed.4");
        assert_eq!(release_asset.url, "https://example.com/Rezed-aarch64.dmg");
    }

    #[test]
    fn test_github_release_asset_reports_missing_asset() {
        let error = AutoUpdater::release_asset_from_github_release(
            GithubRelease {
                tag_name: "v1.9.0-rezed.4".to_string(),
                assets: Vec::new(),
            },
            "linux",
            "x86_64",
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("does not include app update asset rezed-linux-x86_64.tar.gz")
        );
    }

    #[test]
    fn test_stable_does_not_update_when_fetched_version_is_not_higher() {
        let release_channel = ReleaseChannel::Stable;
        let app_commit_sha = Ok(Some("a".to_string()));
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Idle;
        let fetched_version = semver::Version::new(1, 0, 0);

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.to_string(),
            status,
        );

        assert_eq!(newer_version.unwrap(), None);
    }

    #[test]
    fn test_stable_does_update_when_fetched_version_is_higher() {
        let release_channel = ReleaseChannel::Stable;
        let app_commit_sha = Ok(Some("a".to_string()));
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Idle;
        let fetched_version = semver::Version::new(1, 0, 1);

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.to_string(),
            status,
        );

        assert_eq!(newer_version.unwrap(), Some(fetched_version));
    }

    #[test]
    fn test_stable_does_not_update_when_fetched_version_is_not_higher_than_cached() {
        let release_channel = ReleaseChannel::Stable;
        let app_commit_sha = Ok(Some("a".to_string()));
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Updated {
            version: semver::Version::new(1, 0, 1),
        };
        let fetched_version = semver::Version::new(1, 0, 1);

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.to_string(),
            status,
        );

        assert_eq!(newer_version.unwrap(), None);
    }

    #[test]
    fn test_stable_does_update_when_fetched_version_is_higher_than_cached() {
        let release_channel = ReleaseChannel::Stable;
        let app_commit_sha = Ok(Some("a".to_string()));
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Updated {
            version: semver::Version::new(1, 0, 1),
        };
        let fetched_version = semver::Version::new(1, 0, 2);

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.to_string(),
            status,
        );

        assert_eq!(newer_version.unwrap(), Some(fetched_version));
    }

    #[test]
    fn test_stable_rezed_prerelease_suffix_updates_when_higher() {
        let release_channel = ReleaseChannel::Stable;
        let app_commit_sha = Ok(Some("a".to_string()));
        let installed_version = "1.9.0-rezed.2".parse().unwrap();
        let status = AutoUpdateStatus::Idle;
        let fetched_version = "1.9.0-rezed.3".parse::<semver::Version>().unwrap();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.to_string(),
            status,
        );

        assert_eq!(newer_version.unwrap(), Some(fetched_version));
    }

    #[test]
    fn test_stable_rezed_prerelease_suffix_does_not_update_when_same() {
        let release_channel = ReleaseChannel::Stable;
        let app_commit_sha = Ok(Some("a".to_string()));
        let installed_version = "1.9.0-rezed.3".parse().unwrap();
        let status = AutoUpdateStatus::Idle;
        let fetched_version = "1.9.0-rezed.3".parse::<semver::Version>().unwrap();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.to_string(),
            status,
        );

        assert_eq!(newer_version.unwrap(), None);
    }

    #[test]
    fn test_nightly_does_not_update_when_fetched_sha_is_same() {
        let release_channel = ReleaseChannel::Nightly;
        let app_commit_sha = Ok(Some("a".to_string()));
        let mut installed_version = semver::Version::new(1, 0, 0);
        installed_version.build = semver::BuildMetadata::new("a").unwrap();
        let status = AutoUpdateStatus::Idle;
        let fetched_version = "1.0.0+a".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version,
            status,
        );

        assert_eq!(newer_version.unwrap(), None);
    }

    #[test]
    fn test_nightly_does_update_when_fetched_sha_is_not_same() {
        let release_channel = ReleaseChannel::Nightly;
        let app_commit_sha = Ok(Some("a".to_string()));
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Idle;
        let fetched_version = "1.0.0+b".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.clone(),
            status,
        );

        assert_eq!(
            newer_version.unwrap(),
            Some(fetched_version.parse().unwrap())
        );
    }

    #[test]
    fn test_nightly_does_not_update_when_fetched_version_is_same_as_cached() {
        let release_channel = ReleaseChannel::Nightly;
        let app_commit_sha = Ok(Some("a".to_string()));
        let mut installed_version = semver::Version::new(1, 0, 0);
        installed_version.build = semver::BuildMetadata::new("a").unwrap();
        let status = AutoUpdateStatus::Updated {
            version: "1.0.0+b".parse().unwrap(),
        };
        let fetched_version = "1.0.0+b".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version,
            status,
        );

        assert_eq!(newer_version.unwrap(), None);
    }

    #[test]
    fn test_nightly_does_update_when_fetched_sha_is_not_same_as_cached() {
        let release_channel = ReleaseChannel::Nightly;
        let app_commit_sha = Ok(Some("a".to_string()));
        let mut installed_version = semver::Version::new(1, 0, 0);
        installed_version.build = semver::BuildMetadata::new("a").unwrap();
        let status = AutoUpdateStatus::Updated {
            version: "1.0.0+b".parse().unwrap(),
        };
        let fetched_version = "1.0.0+c".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.clone(),
            status,
        );

        assert_eq!(
            newer_version.unwrap(),
            Some(fetched_version.parse().unwrap())
        );
    }

    #[test]
    fn test_nightly_does_not_redownload_after_updating_to_fetched_version() {
        let release_channel = ReleaseChannel::Nightly;
        let installed_version = semver::Version::new(1, 0, 0);
        let fetched_version = "1.0.0+nightly.b".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            Ok(Some("a".to_string())),
            installed_version.clone(),
            fetched_version.clone(),
            AutoUpdateStatus::Idle,
        )
        .unwrap()
        .expect("a newer nightly version should be available");

        let next_check = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            Ok(Some("a".to_string())),
            installed_version,
            fetched_version,
            AutoUpdateStatus::Updated {
                version: newer_version,
            },
        );

        assert_eq!(next_check.unwrap(), None);
    }

    #[test]
    fn test_nightly_does_update_when_installed_versions_sha_cannot_be_retrieved() {
        let release_channel = ReleaseChannel::Nightly;
        let app_commit_sha = Ok(None);
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Idle;
        let fetched_version = "1.0.0+a".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.clone(),
            status,
        );

        assert_eq!(
            newer_version.unwrap(),
            Some(fetched_version.parse().unwrap())
        );
    }

    #[test]
    fn test_nightly_does_not_update_when_cached_update_is_same_as_fetched_and_installed_versions_sha_cannot_be_retrieved()
     {
        let release_channel = ReleaseChannel::Nightly;
        let app_commit_sha = Ok(None);
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Updated {
            version: "1.0.0+b".parse().unwrap(),
        };
        let fetched_version = "1.0.0+b".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version,
            status,
        );

        assert_eq!(newer_version.unwrap(), None);
    }

    #[test]
    fn test_nightly_does_update_when_cached_update_is_not_same_as_fetched_and_installed_versions_sha_cannot_be_retrieved()
     {
        let release_channel = ReleaseChannel::Nightly;
        let app_commit_sha = Ok(None);
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Updated {
            version: "1.0.0+b".parse().unwrap(),
        };
        let fetched_version = "1.0.0+c".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.clone(),
            status,
        );

        assert_eq!(
            newer_version.unwrap(),
            Some(fetched_version.parse().unwrap())
        );
    }
}
