//! The daemon's own updater. It reads the GitHub release feed, downloads the
//! archive for this platform, verifies the published checksum, swaps the
//! binary next to the running one and restarts under the service manager.
//! It runs on the maintenance cadence when `auto_update_daemon` is on, or on
//! request from a client, and only while no turn, approval, background task or
//! queued message is in flight. A daemon the desktop app started never replaces
//! itself: that binary belongs to the installed app, which `app_update` updates instead.
//!
//! `update_track` picks the feed: Stable reads the latest GitHub release, Dev
//! also reads the dev feed (a rolling `dev-feed` prerelease whose
//! `latest-dev.json` every `vX.Y.Z-dev.N` release replaces) and takes the newer
//! of the two. Versions are ordered as semver, so `0.21.0-dev.2` sits between
//! `0.20.x` and `0.21.0`, and a daemon never moves to an older version: after
//! Dev → Stable it keeps its dev build until a stable release is newer.
use crate::state::AppState;
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use kybern_protocol::process::NoWindow;
use kybern_protocol::{DaemonUpdate, DaemonUpdateStatus as Status, UpdateTrack};
use kybern_store::Store;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const REPO: &str = "Tresnanda/kybern";
const META_KEY: &str = "daemon_update";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Exit status after a successful swap under systemd, so `Restart=on-failure`
/// starts the new binary.
pub const RESTART_EXIT_CODE: i32 = 75;

pub struct DaemonUpdates {
    record: Mutex<DaemonUpdate>,
    requested: AtomicBool,
    worker: tokio::sync::Mutex<()>,
    /// A test build is a development build, which never replaces itself; the machine-network tests
    /// set this to see a request get as far as being queued.
    #[cfg(test)]
    pub(crate) allow_development: AtomicBool,
    /// Loopback feeds a test serves in place of GitHub's.
    #[cfg(test)]
    pub(crate) test_feeds: Mutex<Option<Feeds>>,
}

impl DaemonUpdates {
    pub fn new(store: &Store) -> Result<Self> {
        let mut record =
            store.meta_get(META_KEY)?.and_then(|value| serde_json::from_str::<DaemonUpdate>(&value).ok()).unwrap_or_else(|| DaemonUpdate {
                status: Status::NotChecked,
                message: "Not checked yet".into(),
                current_version: CURRENT_VERSION.into(),
                latest_version: None,
                checked_at: None,
            });
        if record.current_version != CURRENT_VERSION {
            // The previous binary wrote this record; we are the update it installed.
            record = DaemonUpdate {
                status: Status::Current,
                message: format!("Updated from {}", record.current_version),
                current_version: CURRENT_VERSION.into(),
                latest_version: Some(CURRENT_VERSION.into()),
                checked_at: Some(Utc::now()),
            };
        } else if matches!(record.status, Status::Checking | Status::Waiting | Status::Updating | Status::Restarting) {
            record.status = Status::Failed;
            record.message = "The daemon restarted during an update. Retry the update.".into();
            record.checked_at = Some(Utc::now());
        }
        Ok(Self {
            record: Mutex::new(record),
            requested: AtomicBool::new(false),
            worker: tokio::sync::Mutex::new(()),
            #[cfg(test)]
            allow_development: AtomicBool::new(false),
            #[cfg(test)]
            test_feeds: Mutex::new(None),
        })
    }

    pub fn get(&self) -> DaemonUpdate {
        self.record.lock().unwrap().clone()
    }

    fn save(&self, store: &Store, record: DaemonUpdate) {
        if let Err(error) = store.meta_set(META_KEY, &serde_json::to_string(&record).expect("serializable record")) {
            tracing::error!(%error, "could not persist daemon update status");
        }
        *self.record.lock().unwrap() = record;
    }

    /// Record progress made elsewhere: the desktop app's update (`app_update`).
    pub(crate) fn record(&self, store: &Store, record: DaemonUpdate) {
        self.save(store, record);
    }

    /// Queue an install; the worker picks it up on its next pass.
    pub fn request(&self, store: &Store) -> DaemonUpdate {
        let mut record = self.get();
        if matches!(record.status, Status::Updating | Status::Restarting) {
            return record;
        }
        self.requested.store(true, Ordering::SeqCst);
        record.status = Status::Waiting;
        record.message = "Waiting for running work to finish".into();
        self.save(store, record.clone());
        record
    }
}

// ---- release feed ----------------------------------------------------------

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(format!("kybernd/{CURRENT_VERSION}"))
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(15))
        .build()?)
}

/// Where each track's newest release is announced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Feeds {
    /// dist's manifest of the latest (non-prerelease) GitHub release; its `announcement_tag` names it.
    pub stable: String,
    /// `latest-dev.json` on the rolling `dev-feed` prerelease (`.github/workflows/dev-feed.yml`); its `version` names it.
    pub dev: String,
}

impl Feeds {
    #[cfg(not(test))]
    fn github() -> Self {
        Self {
            stable: format!("https://github.com/{REPO}/releases/latest/download/dist-manifest.json"),
            dev: format!("https://github.com/{REPO}/releases/download/dev-feed/latest-dev.json"),
        }
    }
}

fn feeds(state: &AppState) -> Option<Feeds> {
    #[cfg(test)]
    {
        // Tests never reach for the network.
        state.daemon_updates.test_feeds.lock().unwrap().clone()
    }
    #[cfg(not(test))]
    {
        let _ = state;
        Some(Feeds::github())
    }
}

async fn read_json(client: &reqwest::Client, url: &str) -> Result<serde_json::Value> {
    client.get(url).send().await?.error_for_status()?.json().await.context("read the release manifest")
}

async fn stable_version(client: &reqwest::Client, feeds: &Feeds) -> Result<String> {
    let manifest = read_json(client, &feeds.stable).await?;
    let tag = manifest["announcement_tag"].as_str().context("the release manifest has no version")?;
    Ok(tag.trim_start_matches('v').to_string())
}

async fn dev_version(client: &reqwest::Client, feeds: &Feeds) -> Result<String> {
    let manifest = read_json(client, &feeds.dev).await?;
    let version = manifest["version"].as_str().context("the dev feed has no version")?;
    Ok(version.trim_start_matches('v').to_string())
}

/// The newest release `track` offers, from what each feed answered. Dev also takes stable releases, so a
/// stable version that overtakes the dev line reaches both tracks, and a missing dev feed (before the first
/// dev release) only matters when the stable one fails too.
pub(crate) fn track_latest(track: UpdateTrack, stable: Result<String>, dev: Option<Result<String>>) -> Result<String> {
    match (track, dev) {
        (UpdateTrack::Stable, _) | (UpdateTrack::Dev, None) => stable,
        (UpdateTrack::Dev, Some(dev)) => match (stable, dev) {
            (Ok(stable), Ok(dev)) => Ok(if newer(&dev, &stable) { dev } else { stable }),
            (Ok(version), Err(_)) | (Err(_), Ok(version)) => Ok(version),
            (Err(error), Err(_)) => Err(error),
        },
    }
}

async fn latest_version(client: &reqwest::Client, feeds: &Feeds, track: UpdateTrack) -> Result<String> {
    let stable = stable_version(client, feeds).await;
    let dev = match track {
        UpdateTrack::Stable => None,
        UpdateTrack::Dev => Some(dev_version(client, feeds).await),
    };
    track_latest(track, stable, dev)
}

/// The newest version this daemon's update track offers, when it can be read.
pub async fn feed_latest(state: &AppState) -> Option<String> {
    let feeds = feeds(state)?;
    latest_version(&http().ok()?, &feeds, state.settings.get().update_track).await.ok()
}

fn parse_version(version: &str) -> Option<semver::Version> {
    semver::Version::parse(version.trim().trim_start_matches('v')).ok()
}

/// Whether `latest` is a newer release than `current`, in semver precedence: `0.2.0-dev.10` is newer
/// than `0.2.0-dev.9` and older than `0.2.0`.
pub fn newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(a), Some(b)) => a.cmp_precedence(&b).is_gt(),
        _ => false,
    }
}

/// Whether a machine on `track` takes `version`: Stable never takes a prerelease.
pub fn on_track(track: UpdateTrack, version: &str) -> bool {
    track == UpdateTrack::Dev || parse_version(version).is_some_and(|version| version.pre.is_empty())
}

fn archive_name() -> Result<String> {
    let arch = match std::env::consts::ARCH {
        arch @ ("x86_64" | "aarch64") => arch,
        other => bail!("No published build for {other}. Install from source instead."),
    };
    let (os, extension) = match std::env::consts::OS {
        "macos" => ("apple-darwin", "tar.xz"),
        "linux" => ("unknown-linux-musl", "tar.xz"),
        "windows" if arch == "x86_64" => ("pc-windows-msvc", "zip"),
        other => bail!("No published build for {other} {arch}. Install from source instead."),
    };
    Ok(format!("kybern-{arch}-{os}.{extension}"))
}

// ---- install -----------------------------------------------------------------

/// Why this daemon must not replace itself, if it must not.
pub(crate) fn unsupported(state: &AppState) -> Option<String> {
    if state.desktop_managed.load(Ordering::SeqCst) {
        return Some("Managed by the Kybern desktop app on this machine; updating the app updates it.".into());
    }
    #[cfg(test)]
    let development = !state.daemon_updates.allow_development.load(Ordering::SeqCst);
    #[cfg(not(test))]
    let development = true;
    if cfg!(debug_assertions) && development {
        return Some("Development build; rebuild it instead.".into());
    }
    let exe = std::env::current_exe().ok()?;
    if exe.components().any(|part| part.as_os_str().to_string_lossy().ends_with(".app")) {
        return Some("Runs from an app bundle; update the app instead.".into());
    }
    None
}

fn find_binary(root: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|file| file == name) {
                return Some(path);
            }
        }
    }
    None
}

/// Atomically put `source` in place of `dest`, which may be the running binary.
fn replace_binary(source: &Path, dest: &Path) -> Result<()> {
    let dir = dest.parent().context("binary has no parent directory")?;
    let staged = dir.join(format!(".{}.new", dest.file_name().unwrap_or_default().to_string_lossy()));
    std::fs::copy(source, &staged).with_context(|| format!("write {}", staged.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(windows)]
    {
        // A running executable cannot be overwritten, but it can be renamed away.
        let old = dir.join(format!(".{}.old", dest.file_name().unwrap_or_default().to_string_lossy()));
        let _ = std::fs::remove_file(&old);
        if dest.exists() {
            std::fs::rename(dest, &old)?;
        }
    }
    std::fs::rename(&staged, dest).with_context(|| format!("replace {}", dest.display()))?;
    Ok(())
}

async fn install(client: &reqwest::Client, version: &str) -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let dir = exe.parent().context("binary has no parent directory")?.to_path_buf();
    let probe = dir.join(".kybern-update-probe");
    std::fs::write(&probe, b"").with_context(|| format!("{} is not writable by this user", dir.display()))?;
    let _ = std::fs::remove_file(&probe);

    let name = archive_name()?;
    let base = format!("https://github.com/{REPO}/releases/download/v{version}");
    let archive =
        client.get(format!("{base}/{name}")).send().await?.error_for_status()?.bytes().await.context("download the release archive")?;
    let published =
        client.get(format!("{base}/{name}.sha256")).send().await?.error_for_status()?.text().await.context("download the checksum")?;
    let expected = published.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    let actual = Sha256::digest(&archive).iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    if expected.is_empty() || expected != actual {
        bail!("The downloaded archive does not match its published checksum. Try again later.");
    }

    let stage = std::env::temp_dir().join(format!("kybern-update-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&stage)?;
    let result = async {
        let archive_path = stage.join(&name);
        std::fs::write(&archive_path, &archive)?;
        let output = tokio::process::Command::new("tar")
            .no_window()
            .arg("-xf")
            .arg(&archive_path)
            .arg("-C")
            .arg(&stage)
            .output()
            .await
            .context("run tar; install tar and xz on this machine")?;
        if !output.status.success() {
            bail!("Unable to unpack the release archive: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        let daemon_name = exe.file_name().context("binary name")?.to_string_lossy().to_string();
        let new_daemon = find_binary(&stage, &daemon_name).with_context(|| format!("the archive has no {daemon_name}"))?;
        replace_binary(&new_daemon, &exe)?;
        // The CLI ships in the same archive; keep it in step when it sits next to us.
        let cli_name = if cfg!(windows) { "kybern.exe" } else { "kybern" };
        if dir.join(cli_name).exists()
            && let Some(new_cli) = find_binary(&stage, cli_name)
        {
            replace_binary(&new_cli, &dir.join(cli_name))?;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let _ = std::fs::remove_dir_all(&stage);
    result
}

// ---- scheduling --------------------------------------------------------------

fn due(record: &DaemonUpdate) -> bool {
    record.checked_at.is_none_or(|at| Utc::now().signed_duration_since(at).num_hours() >= 24)
}

/// Anything a restart would interrupt. Automatic updates also wait for open
/// terminals; a requested update treats those as the user's call.
pub(crate) async fn busy(state: &AppState, automatic: bool) -> Result<bool> {
    let activity = crate::maintenance::activity(state).await?;
    Ok(activity.running_threads > 0
        || activity.live_sessions != activity.idle_sessions
        || activity.queued_messages > 0
        || (automatic && activity.terminals > 0))
}

/// Ask the feed for the newest version and record whether it is newer.
pub async fn check(state: &AppState) -> DaemonUpdate {
    let mut record = state.daemon_updates.get();
    if let Some(reason) = unsupported(state) {
        record.status = Status::Unsupported;
        record.message = reason;
        record.checked_at = Some(Utc::now());
        state.daemon_updates.save(&state.store, record.clone());
        return record;
    }
    record.status = Status::Checking;
    record.message = "Checking the release feed…".into();
    state.daemon_updates.save(&state.store, record.clone());
    let track = state.settings.get().update_track;
    let latest = async { latest_version(&http()?, &feeds(state).context("no release feed in tests")?, track).await }.await;
    record.checked_at = Some(Utc::now());
    match latest {
        Ok(latest) => {
            if newer(&latest, CURRENT_VERSION) {
                record.status = Status::Available;
                record.message = format!("{latest} is available");
            } else {
                record.status = Status::Current;
                record.message = if on_track(track, CURRENT_VERSION) {
                    "Up to date".into()
                } else {
                    // Dev → Stable never downgrades: the dev build stays until a stable release passes it.
                    format!("Up to date. Stays on {CURRENT_VERSION} until a stable release is newer")
                };
            }
            record.latest_version = Some(latest);
        }
        Err(error) => {
            record.status = Status::Failed;
            record.message = format!("Unable to check the release feed: {error:#}");
        }
    }
    state.daemon_updates.save(&state.store, record.clone());
    record
}

pub async fn tick(state: AppState) {
    let Ok(_worker) = state.daemon_updates.worker.try_lock() else { return };
    let manual = state.daemon_updates.requested.load(Ordering::SeqCst);
    let settings = state.settings.get();
    let record = state.daemon_updates.get();
    if !manual && (!settings.auto_update_daemon || !due(&record) || state.saving_power() || unsupported(&state).is_some()) {
        return;
    }
    let finish = |record: DaemonUpdate| {
        state.daemon_updates.save(&state.store, record);
        state.daemon_updates.requested.store(false, Ordering::SeqCst);
    };
    let mut record = check(&state).await;
    let Some(latest) = record.latest_version.clone().filter(|_| record.status == Status::Available) else {
        finish(record);
        return;
    };
    if !manual && !state.settings.get().auto_update_daemon {
        finish(record);
        return;
    }
    match busy(&state, !manual).await {
        Ok(false) => {}
        Ok(true) => {
            record.status = Status::Waiting;
            record.message = format!("{latest} is ready. Waiting for running work to finish");
            state.daemon_updates.save(&state.store, record);
            if !manual {
                state.daemon_updates.requested.store(true, Ordering::SeqCst);
            }
            return;
        }
        Err(error) => {
            record.status = Status::Failed;
            record.message = format!("{error:#}");
            finish(record);
            return;
        }
    }
    record.status = Status::Updating;
    record.message = format!("Installing {latest}…");
    state.daemon_updates.save(&state.store, record.clone());
    let result = tokio::select! {
        result = async { install(&http()?, &latest).await } => result,
        _ = state.shutdown.cancelled() => Err(anyhow!("Update interrupted by daemon shutdown. Retry the update.")),
    };
    match result {
        Ok(()) => {
            record.status = Status::Restarting;
            record.message = format!("Restarting on {latest}");
            finish(record);
            tracing::info!(version = %latest, "daemon updated; restarting");
            state.restart_pending.store(true, Ordering::SeqCst);
            state.shutdown.cancel();
        }
        Err(error) => {
            record.status = Status::Failed;
            record.message = format!("{error:#}");
            finish(record);
        }
    }
}

pub async fn run(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            _ = interval.tick() => tick(state.clone()).await,
        }
    }
}

/// Hand over to the freshly installed binary after the graceful shutdown.
/// Under systemd the unit restarts us on a non-zero exit; anywhere else we
/// start the replacement ourselves with the same arguments and log file.
pub fn restart(log_file: &Path, exe: Option<PathBuf>) -> ! {
    if std::env::var_os("INVOCATION_ID").is_some() {
        std::process::exit(RESTART_EXIT_CODE);
    }
    // The installed path, not what this process reports for itself after the swap: Linux names a replaced binary
    // `<path> (deleted)`. An app update may name the new daemon itself.
    let exe = exe.unwrap_or_else(|| {
        let exe = std::env::current_exe().expect("current executable");
        crate::login_service::installed_binary(exe.clone()).unwrap_or(exe)
    });
    let mut command = std::process::Command::new(exe).no_window();
    command.args(std::env::args_os().skip(1)).stdin(std::process::Stdio::null());
    match std::fs::OpenOptions::new().append(true).create(true).open(log_file) {
        Ok(log) => {
            let err = log.try_clone().expect("clone log handle");
            command.stdout(log).stderr(err);
        }
        Err(_) => {
            command.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    match command.spawn() {
        Ok(_) => std::process::exit(0),
        Err(error) => {
            tracing::error!(%error, "could not start the updated daemon");
            std::process::exit(RESTART_EXIT_CODE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_release_versions() {
        assert!(newer("0.1.4", "0.1.3"));
        assert!(newer("v1.0.0", "0.9.9"));
        assert!(!newer("0.1.3", "0.1.3"));
        assert!(!newer("0.1.3-rc.1", "0.1.3"));
        assert!(!newer("garbage", "0.1.3"));
    }

    #[test]
    fn orders_dev_builds_between_releases() {
        assert!(newer("0.21.0-dev.1", "0.20.9"));
        assert!(newer("0.21.0", "0.21.0-dev.9"));
        assert!(newer("0.21.0-dev.10", "0.21.0-dev.9"), "numeric prerelease parts compare as numbers");
        assert!(!newer("0.21.0-dev.9", "0.21.0-dev.10"));
        assert!(!newer("0.21.0-dev.3", "0.21.0-dev.3"));
        assert!(!newer("0.20.9", "0.21.0-dev.1"), "a stable release older than the dev build is not an update");
        assert!(!newer("0.21.0-dev.1", "0.21.0"));
        assert!(newer("v0.22.0-dev.1", "0.21.0"));
    }

    #[test]
    fn a_track_takes_its_releases() {
        assert!(on_track(UpdateTrack::Stable, "0.21.0"));
        assert!(!on_track(UpdateTrack::Stable, "0.21.0-dev.2"));
        assert!(on_track(UpdateTrack::Dev, "0.21.0-dev.2"));
        assert!(on_track(UpdateTrack::Dev, "0.21.0"));
        assert!(!on_track(UpdateTrack::Stable, "garbage"));
    }

    #[test]
    fn dev_takes_the_newer_feed_and_stable_reads_only_its_own() {
        let ok = |version: &str| Ok::<_, anyhow::Error>(version.to_string());
        let gone = || Err::<String, _>(anyhow!("404"));
        // Stable never looks at the dev feed.
        assert_eq!(track_latest(UpdateTrack::Stable, ok("0.20.1"), Some(ok("0.21.0-dev.2"))).unwrap(), "0.20.1");
        assert_eq!(track_latest(UpdateTrack::Stable, ok("0.20.1"), None).unwrap(), "0.20.1");
        // Dev takes whichever is newer: the dev build, or a stable release that overtook it.
        assert_eq!(track_latest(UpdateTrack::Dev, ok("0.20.1"), Some(ok("0.21.0-dev.2"))).unwrap(), "0.21.0-dev.2");
        assert_eq!(track_latest(UpdateTrack::Dev, ok("0.21.0"), Some(ok("0.21.0-dev.2"))).unwrap(), "0.21.0");
        // Before the first dev release the dev feed is missing; the stable feed still answers.
        assert_eq!(track_latest(UpdateTrack::Dev, ok("0.20.1"), Some(gone())).unwrap(), "0.20.1");
        assert_eq!(track_latest(UpdateTrack::Dev, gone(), Some(ok("0.21.0-dev.2"))).unwrap(), "0.21.0-dev.2");
        assert!(track_latest(UpdateTrack::Dev, gone(), Some(gone())).is_err());
        assert!(track_latest(UpdateTrack::Stable, gone(), Some(ok("0.21.0-dev.2"))).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_daemon_checks_the_feed_of_its_own_update_track() {
        use axum::{Json, Router, routing::get};
        let current = semver::Version::parse(CURRENT_VERSION).unwrap();
        let dev = format!("{}.{}.0-dev.1", current.major, current.minor + 1);
        let stable_body = serde_json::json!({ "announcement_tag": format!("v{CURRENT_VERSION}") });
        let dev_body = serde_json::json!({ "version": dev, "platforms": {} });
        let app = Router::new()
            .route("/dist-manifest.json", get(move || async move { Json(stable_body) }))
            .route("/latest-dev.json", get(move || async move { Json(dev_body) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let root = std::env::temp_dir().join(format!("kybern-track-test-{}", uuid::Uuid::now_v7()));
        let state = AppState::initialize(&crate::config::Paths::resolve(Some(root.clone())).unwrap()).unwrap();
        state.daemon_updates.allow_development.store(true, Ordering::SeqCst);
        let feeds = |dev: &str| Feeds { stable: format!("{base}/dist-manifest.json"), dev: format!("{base}/{dev}") };
        *state.daemon_updates.test_feeds.lock().unwrap() = Some(feeds("latest-dev.json"));
        let set_track = |track| {
            let mut settings = state.settings.get();
            settings.update_track = track;
            state.settings.set(settings).unwrap();
        };

        // A new install is on Stable and does not see the dev build.
        assert_eq!(state.settings.get().update_track, UpdateTrack::Stable);
        let record = check(&state).await;
        assert_eq!((record.status, record.latest_version.as_deref()), (Status::Current, Some(CURRENT_VERSION)), "{record:?}");

        set_track(UpdateTrack::Dev);
        let record = check(&state).await;
        assert_eq!((record.status, record.latest_version.as_deref()), (Status::Available, Some(dev.as_str())), "{record:?}");
        assert_eq!(feed_latest(&state).await.as_deref(), Some(dev.as_str()), "update_all aims at the dev build too");

        // Before the first dev release the dev feed is missing; Dev still reads the stable one.
        *state.daemon_updates.test_feeds.lock().unwrap() = Some(feeds("missing.json"));
        let record = check(&state).await;
        assert_eq!((record.status, record.latest_version.as_deref()), (Status::Current, Some(CURRENT_VERSION)), "{record:?}");

        set_track(UpdateTrack::Stable);
        *state.daemon_updates.test_feeds.lock().unwrap() = Some(feeds("latest-dev.json"));
        assert_eq!(check(&state).await.status, Status::Current);
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn names_the_archive_for_this_platform() {
        let name = archive_name().unwrap();
        assert!(name.starts_with("kybern-"));
        assert!(name.ends_with(".tar.xz") || name.ends_with(".zip"));
    }

    #[test]
    fn replaces_a_binary_in_place() {
        let dir = std::env::temp_dir().join(format!("kybern-replace-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("kybernd");
        let source = dir.join("incoming");
        std::fs::write(&dest, b"old").unwrap();
        std::fs::write(&source, b"new").unwrap();
        replace_binary(&source, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        assert!(!dir.join(".kybernd.new").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
