//! Updating the desktop app that manages this daemon (ADE-125).
//!
//! A daemon the desktop app started (or the login item started from the app's own binary) cannot replace
//! itself: its binary is part of the installed app. When this machine is asked to update ("Update all
//! machines", "Keep all machines on the same version", `daemon_update.run`), it updates the app instead:
//!
//! * **The app is open.** Each window attaches over its local connection (`app_updater.attach`). Once
//!   nothing is running here, the daemon sends `app_update.requested` to one attached window. The window
//!   installs the release with the app's own updater (its update track's feed, its signature check), waits
//!   again until nothing is running, shows a short notice and restarts the app, whose new daemon replaces
//!   this one. It reports how far it got with `app_updater.report`.
//! * **Only the daemon runs** (the app is closed; the login item started it). Once nothing is running, the
//!   daemon reads the same feed for its own update track, verifies the download with the same key and
//!   replaces the installed app: the `.app` bundle on macOS (staged next to it, then swapped), the per-user
//!   install on Windows (its installer, silently), the AppImage on Linux (renamed over). It then restarts on
//!   the new binary, at the same path, so the login item still starts the right one, and the app opens on
//!   the new version next time.
//! * **A deb or rpm install** needs root, so it is never replaced; the answer says what to run.
//!
//! An update never interrupts work: it waits while a turn, an approval, background work or a queued
//! message is in flight, the same rule as the daemon's own updater. Older daemons answer "Updates with the
//! app" (`features::APP_UPDATE_V1` marks the ones that do this), and an app too old to attach is never
//! replaced under itself: on macOS the daemon waits while a copy of the app runs from the bundle.

pub mod install;
#[cfg(test)]
mod tests;

use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use kybern_protocol::methods::{AppUpdatePhase, AppUpdaterReportParams};
use kybern_protocol::{DaemonUpdate, DaemonUpdateStatus as Status};
use uuid::Uuid;

use crate::self_update::{self, CURRENT_VERSION};
use crate::state::AppState;
pub use install::{DesktopInstall, Detected, PackageKind};

const TARGET: &str = "kybern::app_update";
/// How often a pending update looks again whether work finished.
const WAIT_EVERY: Duration = Duration::from_secs(10);
/// How long after the window that had an update went away the daemon waits before installing it itself: the window
/// may have quit to restart on the new version, whose daemon then replaces this one.
const RELEASE_GRACE: Duration = Duration::from_secs(120);

/// One window of the app on this machine that can take update requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attached {
    pub connection: Uuid,
    pub version: String,
    pub can_install: bool,
    pub reason: Option<String>,
}

/// An update this machine was asked for and has not finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// The version asked for, when the request named one. The installer still reads its own feed.
    pub version: Option<String>,
    /// Nobody asked for it on any machine: this machine follows another one ("Keep all machines on the same
    /// version"). Such an update also waits while a terminal is open, since a restart ends its processes.
    pub automatic: bool,
    /// The window it was handed to, if one has it.
    pub handed_to: Option<Uuid>,
    /// When that window went away without saying how it ended. It may be the app quitting to restart on the new
    /// version, so the daemon does not install the same release itself right after.
    pub released_at: Option<std::time::Instant>,
}

/// What the attached window is told: install now.
#[derive(Debug, Clone)]
pub struct Notice {
    pub connection: Uuid,
    pub version: Option<String>,
    pub automatic: bool,
}

#[derive(Default)]
struct Inner {
    /// In the order the windows attached; the first one able to install takes requests.
    attached: Vec<Attached>,
    pending: Option<Pending>,
}

pub struct AppUpdates {
    inner: Mutex<Inner>,
    notices: tokio::sync::broadcast::Sender<Notice>,
    wake: tokio::sync::Notify,
    worker: tokio::sync::Mutex<()>,
    /// What the tests install instead of the test binary's own folder.
    #[cfg(test)]
    pub(crate) test_install: Mutex<Option<Detected>>,
    /// Work is running, as far as a test is concerned.
    #[cfg(test)]
    pub(crate) test_busy: std::sync::atomic::AtomicBool,
    /// A terminal is open, as far as a test is concerned.
    #[cfg(test)]
    pub(crate) test_terminals: std::sync::atomic::AtomicBool,
}

impl Default for AppUpdates {
    fn default() -> Self {
        Self {
            inner: Mutex::default(),
            notices: tokio::sync::broadcast::channel(8).0,
            wake: tokio::sync::Notify::new(),
            worker: tokio::sync::Mutex::new(()),
            #[cfg(test)]
            test_install: Mutex::new(None),
            #[cfg(test)]
            test_busy: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            test_terminals: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl AppUpdates {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A window attached (again). A pending update is looked at once more, so a window that opens while the
    /// daemon waits takes it.
    pub fn attach(&self, window: Attached) {
        {
            let mut inner = self.inner();
            inner.attached.retain(|existing| existing.connection != window.connection);
            tracing::debug!(target: TARGET, connection = %window.connection, version = %window.version, can_install = window.can_install, "a window attached as the app's updater");
            inner.attached.push(window);
        }
        self.wake.notify_one();
    }

    /// A connection closed. If it had the update, the next window (or the daemon) takes it over.
    pub fn detach(&self, connection: Uuid) {
        let mut inner = self.inner();
        let before = inner.attached.len();
        inner.attached.retain(|existing| existing.connection != connection);
        if let Some(pending) = inner.pending.as_mut()
            && pending.handed_to == Some(connection)
        {
            pending.handed_to = None;
            pending.released_at = Some(std::time::Instant::now());
        }
        if inner.attached.len() != before {
            drop(inner);
            self.wake.notify_one();
        }
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Notice> {
        self.notices.subscribe()
    }

    /// The window that takes requests: the first attached one.
    pub fn window(&self) -> Option<Attached> {
        self.inner().attached.first().cloned()
    }

    pub fn pending(&self) -> Option<Pending> {
        self.inner().pending.clone()
    }

    /// Queue an update. A second request while one waits keeps the higher version, and a request someone made
    /// makes the pending update no longer automatic.
    pub fn request(&self, version: Option<String>, automatic: bool) {
        {
            let mut inner = self.inner();
            let previous = inner.pending.take();
            let automatic = automatic && previous.as_ref().is_none_or(|pending| pending.automatic);
            let version = match (previous.and_then(|pending| pending.version), version) {
                (Some(a), Some(b)) => Some(if self_update::newer(&b, &a) { b } else { a }),
                (a, b) => b.or(a),
            };
            inner.pending = Some(Pending { version, automatic, handed_to: None, released_at: None });
        }
        self.wake.notify_one();
    }

    fn finish(&self) {
        self.inner().pending = None;
    }

    /// Hand the pending update to `connection`, once. Returns the notice to send when it was not handed over yet.
    fn hand_to(&self, connection: Uuid) -> Option<Notice> {
        let mut inner = self.inner();
        let pending = inner.pending.as_mut()?;
        if pending.handed_to == Some(connection) {
            return None;
        }
        pending.handed_to = Some(connection);
        Some(Notice { connection, version: pending.version.clone(), automatic: pending.automatic })
    }
}

/// Who installs an update on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// An open window, with the app's own updater.
    App(Attached),
    /// The daemon, while the app is closed.
    Daemon(DesktopInstall),
    /// A deb or rpm package: only the user, as root.
    Package(PackageKind),
    /// Nothing here can; the words say why.
    Manual(String),
}

/// The rule: a system package is never replaced from here; an open window takes it when it can install; with no
/// window the daemon replaces the installed app, when there is one.
pub fn route(window: Option<&Attached>, detected: &Detected) -> Route {
    if let Detected::Install(DesktopInstall::Package(kind)) = detected {
        return Route::Package(*kind);
    }
    if let Some(window) = window {
        return if window.can_install {
            Route::App(window.clone())
        } else {
            Route::Manual(window.reason.clone().unwrap_or_else(|| install::NOT_INSTALLED.to_string()))
        };
    }
    match detected {
        Detected::Install(install) => Route::Daemon(install.clone()),
        Detected::NotInstalled(reason) => Route::Manual(reason.clone()),
    }
}

/// How this daemon's binary was installed.
pub fn detect(state: &AppState) -> Detected {
    #[cfg(test)]
    {
        state.app_updates.test_install.lock().unwrap().clone().unwrap_or_else(|| Detected::NotInstalled(install::DEVELOPMENT.into()))
    }
    #[cfg(not(test))]
    {
        let _ = state;
        match std::env::current_exe() {
            Ok(exe) => install::detect(
                &crate::login_service::installed_binary(exe.clone()).unwrap_or(exe),
                std::env::consts::OS,
                &install::Probe::REAL,
            ),
            Err(error) => Detected::NotInstalled(format!("Kybern could not find its own program: {error}")),
        }
    }
}

/// This machine's route right now.
pub fn current_route(state: &AppState) -> Route {
    route(state.app_updates.window().as_ref(), &detect(state))
}

/// Why this desktop-managed machine cannot start an update for `version`, or `None` when it can.
pub fn blocked(state: &AppState, version: Option<&str>) -> Option<String> {
    match current_route(state) {
        Route::App(_) | Route::Daemon(_) => None,
        Route::Package(kind) => Some(install::package_message(kind, version, std::env::consts::ARCH)),
        Route::Manual(reason) => Some(reason),
    }
}

/// Queue an update of the app; [`run`] carries it out once nothing is running. An `automatic` one (following another
/// machine) also waits for this machine's terminals to close.
pub fn request(state: &AppState, version: Option<String>, automatic: bool) {
    state.app_updates.request(version, automatic);
    note(state, Status::Waiting, "Waiting for running work to finish".into());
}

/// What the attached window said about the update it was handed.
pub fn report(state: &AppState, connection: Uuid, report: AppUpdaterReportParams) {
    let handed = state.app_updates.pending().is_some_and(|pending| pending.handed_to == Some(connection));
    let version = report.version.clone().unwrap_or_else(|| "the new version".into());
    tracing::info!(target: TARGET, %connection, phase = ?report.phase, message = ?report.message, "the app reported on its update");
    match report.phase {
        AppUpdatePhase::Installing => note(state, Status::Updating, format!("Installing {version}…")),
        AppUpdatePhase::Waiting => note(state, Status::Waiting, format!("{version} is ready. Waiting for running work to finish")),
        AppUpdatePhase::Restarting => {
            // The app installed it and restarts; its new daemon replaces this one. Nothing is left to do here.
            if handed {
                state.app_updates.finish();
            }
            note(state, Status::Restarting, format!("Restarting on {version}"));
        }
        AppUpdatePhase::Current => {
            if handed {
                state.app_updates.finish();
            }
            note(state, Status::Current, "Up to date".into());
        }
        AppUpdatePhase::Failed => {
            if handed {
                state.app_updates.finish();
            }
            note(state, Status::Failed, report.message.unwrap_or_else(|| "The update did not install. Try again.".into()));
        }
    }
}

/// Record progress where `daemon_update.status` and the machines page read it.
fn note(state: &AppState, status: Status, message: String) {
    let mut record: DaemonUpdate = state.daemon_updates.get();
    record.status = status;
    record.message = message;
    record.checked_at = Some(chrono::Utc::now());
    state.daemon_updates.record(&state.store, record);
}

/// What one pass over a pending update did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Nothing is pending.
    Idle,
    /// Work is running, or the app is open but has not attached: look again later.
    Wait,
    /// Handed to this window.
    Handed(Uuid),
    /// The window already has it.
    WithWindow,
    /// The daemon installed this version and restarts.
    Installed(String),
    /// Nothing newer on the feed, or it cannot be done here; the request is closed.
    Closed,
}

/// One look at a pending update.
pub async fn tick(state: &AppState) -> Step {
    let Ok(_worker) = state.app_updates.worker.try_lock() else { return Step::Wait };
    let Some(pending) = state.app_updates.pending() else { return Step::Idle };
    if busy(state, pending.automatic).await {
        note(state, Status::Waiting, "Waiting for running work to finish".into());
        return Step::Wait;
    }
    match current_route(state) {
        Route::App(window) => match state.app_updates.hand_to(window.connection) {
            Some(notice) => {
                tracing::info!(target: TARGET, connection = %window.connection, version = ?notice.version, "asking the open app to update");
                let _ = state.app_updates.notices.send(notice);
                note(state, Status::Updating, "Updating Kybern on this machine…".into());
                Step::Handed(window.connection)
            }
            None => Step::WithWindow,
        },
        Route::Daemon(_) if pending.released_at.is_some_and(|at| at.elapsed() < RELEASE_GRACE) => Step::Wait,
        Route::Daemon(target) => install_here(state, &target, &pending).await,
        Route::Package(kind) => {
            note(state, Status::Unsupported, install::package_message(kind, pending.version.as_deref(), std::env::consts::ARCH));
            state.app_updates.finish();
            Step::Closed
        }
        Route::Manual(reason) => {
            note(state, Status::Unsupported, reason);
            state.app_updates.finish();
            Step::Closed
        }
    }
}

/// Anything a restart would interrupt: a turn, an approval, background work, a queued message, and for an `automatic`
/// update an open terminal (a dev server the user started). A failed read of the activity counts as busy: better late
/// than in the middle of a turn.
async fn busy(state: &AppState, automatic: bool) -> bool {
    #[cfg(test)]
    if state.app_updates.test_busy.load(Ordering::SeqCst) {
        return true;
    }
    #[cfg(test)]
    if automatic && state.app_updates.test_terminals.load(Ordering::SeqCst) {
        return true;
    }
    self_update::busy(state, automatic).await.unwrap_or(true)
}

/// The daemon replaces the installed app itself, then restarts on the new binary.
async fn install_here(state: &AppState, target: &DesktopInstall, pending: &Pending) -> Step {
    let wanted = pending.version.as_deref();
    if install::app_running(target) {
        note(state, Status::Waiting, "Waiting for Kybern to close on this machine".into());
        return Step::Wait;
    }
    let track = state.settings.get().update_track;
    note(state, Status::Updating, "Checking the release feed…".into());
    let prepared = match install::prepare(target, track, CURRENT_VERSION).await {
        Ok(Some(prepared)) => prepared,
        Ok(None) => {
            tracing::info!(target: TARGET, ?wanted, "the app's feed has nothing newer for this machine's update track");
            note(state, Status::Current, "Up to date".into());
            state.app_updates.finish();
            return Step::Closed;
        }
        Err(error) => return failed(state, error),
    };
    // The download took a while: never swap the app under work that started meanwhile, nor under a window that opened.
    if busy(state, pending.automatic).await || install::app_running(target) || state.app_updates.window().is_some() {
        note(state, Status::Waiting, format!("{} is ready. Waiting for running work to finish", prepared.version));
        return Step::Wait;
    }
    note(state, Status::Updating, format!("Installing {}…", prepared.version));
    let version = prepared.version.clone();
    let applied = tokio::select! {
        applied = install::apply(target, prepared) => applied,
        _ = state.shutdown.cancelled() => Err(anyhow::anyhow!("The update was interrupted by a shutdown. Try again.")),
    };
    match applied {
        Ok(Some(exe)) => {
            tracing::info!(target: TARGET, %version, exe = %exe.display(), "installed the app's update; restarting on it");
            note(state, Status::Restarting, format!("Restarting on {version}"));
            state.app_updates.finish();
            *state.restart_exe.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(exe);
            state.restart_pending.store(true, Ordering::SeqCst);
            state.shutdown.cancel();
            Step::Installed(version)
        }
        Ok(None) => {
            note(state, Status::Waiting, format!("Kybern {version} is installed. This machine restarts on it when Kybern opens"));
            state.app_updates.finish();
            Step::Installed(version)
        }
        Err(error) => failed(state, error),
    }
}

fn failed(state: &AppState, error: anyhow::Error) -> Step {
    tracing::warn!(target: TARGET, error = format!("{error:#}"), "the app's update failed");
    note(state, Status::Failed, format!("{error:#}"));
    state.app_updates.finish();
    Step::Closed
}

/// The worker: carries out a pending update once nothing is running. It sleeps until a request or a window
/// arrives, and looks again every few seconds only while an update waits.
pub async fn run(state: AppState) {
    loop {
        let waiting = state.app_updates.pending().is_some();
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            _ = state.app_updates.wake.notified() => {}
            _ = tokio::time::sleep(WAIT_EVERY), if waiting => {}
        }
        if !state.desktop_managed.load(Ordering::SeqCst) {
            continue;
        }
        tick(&state).await;
    }
}

/// Remove what earlier daemon-side installs left in the temp folder; best effort.
pub fn cleanup() -> Result<()> {
    install::remove_stale_staging(&std::env::temp_dir(), std::time::SystemTime::now());
    Ok(())
}
