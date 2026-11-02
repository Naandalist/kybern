//! "Keep this machine reachable when Kybern is closed" (ADE-119).
//!
//! While this machine is a member of a network with the network on, and `settings.network.keep_reachable`
//! is on (the default), the daemon registers a per-user login item that starts it at login, so the
//! other members reach it with no app window open. It removes the item when the machine leaves or is
//! removed, the network is turned off, or the setting is. Per-user only: never an administrator,
//! root or system service.
//!
//! * **Windows:** a Task Scheduler task at this user's logon (`\Kybern\kybernd`), run through
//!   `conhost.exe --headless` so no console window opens.
//! * **macOS:** a LaunchAgent, `~/Library/LaunchAgents/dev.kybern.kybernd.login.plist`.
//! * **Linux:** a `systemd --user` unit, `kybern-login.service`, or an XDG autostart entry where there is
//!   no user systemd.
//!
//! The item starts the binary of the daemon that registered it, which is the one the desktop app
//! installed and starts itself. Every daemon start checks the item again, so after an update moves
//! the binary the new daemon points the item at itself. A development build (a binary in a Cargo
//! `target` folder) never registers itself: it would replace the installed daemon at the next login.
//!
//! A data folder other than `~/.kybern` gets items of its own, named with a hash of the folder, so a
//! scratch daemon never touches the user's real one. The names differ from what the remote installers
//! write (`dev.kybern.daemon`, `kybernd.service`); when one of those already starts a daemon for this
//! data folder, it is reported and left alone.
//!
//! The daemon an item starts gets `--login-service`: it exits at once when another daemon already
//! serves the data folder (the app may have started one first), and it counts as desktop-managed, so
//! the app's superseded-binary restart keeps working. Each item restarts the daemon only after a
//! crash, never after a clean exit, so the app replacing it with a newer binary is not raced.
//!
//! All OS work goes through [`ServiceManager`]: the platform one is installed by `kybernd` itself
//! ([`LoginService::use_platform`]); every other state, the harness and the tests included, has one
//! that registers nothing.

mod files;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Result;
use kybern_protocol::{KeepReachableStatus, LoginServiceKind};
use sha2::Digest;

use crate::state::AppState;

const TARGET: &str = "kybern::login_service";

/// Set to `1` to let a development build register itself (a real check from a checkout).
pub const ANY_BINARY_ENV: &str = "KYBERN_LOGIN_SERVICE_ANY_BINARY";

/// One login item: what it is called and what it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSpec {
    /// The task name, launchd label or unit name, without a folder or extension.
    pub name: String,
    pub exe: PathBuf,
    pub data_dir: PathBuf,
    /// The PATH the daemon starts with (macOS and Linux): this daemon's own, which the app set from
    /// the login shell. Windows reads PATH from the registry itself.
    pub path_env: Option<String>,
}

impl ServiceSpec {
    /// The daemon's arguments. `--port 0` keeps the port the app chose before (`desktop.port`).
    pub fn args(&self) -> Vec<String> {
        vec!["--data-dir".into(), self.data_dir.to_string_lossy().into_owned(), "--port".into(), "0".into(), "--login-service".into()]
    }
}

/// What is registered under a spec's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registration {
    Missing,
    /// It runs this spec's binary for this spec's data folder.
    Current,
    /// It exists but runs another binary.
    Stale,
}

/// The OS seam: Task Scheduler, launchd, systemd, or nothing.
pub trait ServiceManager: Send + Sync {
    /// `None` where Kybern cannot register a login item.
    fn kind(&self) -> Option<LoginServiceKind>;
    fn registration(&self, spec: &ServiceSpec) -> Result<Registration>;
    /// Register or replace the item. It takes effect at the next login; nothing is started now.
    fn install(&self, spec: &ServiceSpec) -> Result<()>;
    /// Remove the item. `running_here`: this very process was started by it, and must not be stopped
    /// while it answers the request that removes it.
    fn uninstall(&self, spec: &ServiceSpec, running_here: bool) -> Result<()>;
    /// Another service that already starts a daemon for this data folder, described for the user.
    fn external(&self, _data_dir: &Path) -> Option<String> {
        None
    }
    /// Problems with services that start this daemon, for the doctor: `(id, title, next step)`.
    fn warnings(&self, _data_dir: &Path) -> Vec<(String, String, String)> {
        Vec::new()
    }
    /// Bring the services older versions wrote for `data_dir` up to date at start. Returns what was changed.
    fn migrate(&self, _data_dir: &Path) -> Vec<String> {
        Vec::new()
    }
}

/// A manager that registers nothing: the default outside a real `kybernd`.
struct Inert;

impl ServiceManager for Inert {
    fn kind(&self) -> Option<LoginServiceKind> {
        None
    }
    fn registration(&self, _spec: &ServiceSpec) -> Result<Registration> {
        Ok(Registration::Missing)
    }
    fn install(&self, _spec: &ServiceSpec) -> Result<()> {
        Ok(())
    }
    fn uninstall(&self, _spec: &ServiceSpec, _running_here: bool) -> Result<()> {
        Ok(())
    }
}

/// The platform's manager.
fn platform() -> Arc<dyn ServiceManager> {
    #[cfg(windows)]
    return Arc::new(windows::TaskScheduler);
    #[cfg(target_os = "macos")]
    return Arc::new(macos::LaunchAgent);
    #[cfg(target_os = "linux")]
    return Arc::new(linux::Systemd::detect());
    #[allow(unreachable_code)]
    Arc::new(Inert)
}

/// The item's name for a data folder: the plain name for `~/.kybern`, a hashed one for any other.
pub fn service_name(base: &str, data_dir: &Path, default_dir: Option<&Path>) -> String {
    let key = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if default_dir.is_some_and(|default| key(default) == key(data_dir)) {
        return base.to_string();
    }
    let digest = sha2::Sha256::digest(key(data_dir).to_string_lossy().as_bytes());
    let hash: String = digest.iter().take(4).map(|byte| format!("{byte:02x}")).collect();
    format!("{base}-{hash}")
}

/// A binary in a Cargo `target` folder (`target/debug`, `target/<triple>/release`, …).
pub fn is_development_build(exe: &Path) -> bool {
    let names: Vec<String> =
        exe.ancestors().skip(1).filter_map(|dir| dir.file_name()).map(|name| name.to_string_lossy().to_lowercase()).collect();
    names.iter().enumerate().any(|(index, name)| {
        (name == "debug" || name == "release")
            && (names.get(index + 1).is_some_and(|parent| parent == "target")
                || names.get(index + 2).is_some_and(|grandparent| grandparent == "target"))
    })
}

#[derive(Debug, Clone, Default)]
struct Applied {
    /// The decision the item last matched, `None` until the first reconcile.
    wanted: Option<bool>,
    registered: bool,
    external: Option<String>,
    error: Option<String>,
    /// The OS refused a read, registration or removal: the next reconcile tries again even when the
    /// decision did not change.
    retry: bool,
}

pub struct LoginService {
    manager: RwLock<Arc<dyn ServiceManager>>,
    /// This process was started by the login item (`kybernd --login-service`).
    launched: AtomicBool,
    /// A development build may register itself ([`ANY_BINARY_ENV`], or a test).
    allow_development_build: AtomicBool,
    applied: Mutex<Applied>,
    /// One reconcile at a time: the worker and `network.keep_reachable.set` may race.
    busy: tokio::sync::Mutex<()>,
}

impl Default for LoginService {
    fn default() -> Self {
        Self {
            manager: RwLock::new(Arc::new(Inert)),
            launched: AtomicBool::new(false),
            allow_development_build: AtomicBool::new(std::env::var(ANY_BINARY_ENV).as_deref() == Ok("1")),
            applied: Mutex::default(),
            busy: Default::default(),
        }
    }
}

impl LoginService {
    /// Register real login items from now on. Only `kybernd` itself calls this, and not when it plays a
    /// tailnet from a fixture (the process tests), unless [`ANY_BINARY_ENV`] asks for a real check.
    /// Detecting the platform's manager may run a command (`systemctl --user`), so it runs off the runtime.
    pub async fn use_platform(&self) {
        let fixture = std::env::var_os("KYBERN_TAILSCALE_FAKE").is_some();
        if !fixture || std::env::var(ANY_BINARY_ENV).as_deref() == Ok("1") {
            match tokio::task::spawn_blocking(platform).await {
                Ok(manager) => self.set_manager(manager),
                Err(error) => tracing::warn!(target: TARGET, %error, "could not detect how this system starts programs at login"),
            }
        }
    }

    /// Update the services older versions wrote to start this data folder's daemon (Linux: the OOM
    /// policy), off the async runtime.
    pub async fn migrate(&self, data_dir: &Path) {
        let manager = self.manager();
        let data_dir = data_dir.to_path_buf();
        if let Ok(changed) = tokio::task::spawn_blocking(move || manager.migrate(&data_dir)).await
            && !changed.is_empty()
        {
            tracing::info!(target: TARGET, ?changed, "updated the services that start this daemon");
        }
    }

    pub fn set_manager(&self, manager: Arc<dyn ServiceManager>) {
        *self.manager.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = manager;
        *self.applied.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Applied::default();
    }

    fn manager(&self) -> Arc<dyn ServiceManager> {
        self.manager.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    pub fn mark_launched(&self) {
        self.launched.store(true, Ordering::SeqCst);
    }

    pub fn launched(&self) -> bool {
        self.launched.load(Ordering::SeqCst)
    }

    /// Let this daemon register itself although it is a development build.
    #[cfg(test)]
    pub fn allow_development_build(&self) {
        self.allow_development_build.store(true, Ordering::SeqCst);
    }

    fn applied(&self) -> std::sync::MutexGuard<'_, Applied> {
        self.applied.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// What `network.status` reports.
    pub fn status(&self, state: &AppState) -> KeepReachableStatus {
        let applied = self.applied().clone();
        let warnings = self.warnings(state).into_iter().map(|(_, title, next)| format!("{title}. {next}")).collect();
        KeepReachableStatus {
            warnings,
            enabled: state.settings.get().network.keep_reachable,
            wanted: wanted(state),
            kind: self.manager().kind(),
            registered: applied.registered,
            running: self.launched(),
            external: applied.external,
            error: applied.error,
        }
    }

    /// Problems the doctor reports about services that start this daemon.
    pub fn warnings(&self, state: &AppState) -> Vec<(String, String, String)> {
        self.manager().warnings(&state.paths.root)
    }

    /// Bring the login item in line with [`wanted`]. Without `force`, nothing happens unless the decision
    /// changed since the last reconcile.
    pub async fn reconcile(&self, state: &AppState, force: bool) {
        let _busy = self.busy.lock().await;
        let wanted = wanted(state);
        if !force && !self.applied().retry && self.applied().wanted == Some(wanted) {
            return;
        }
        let manager = self.manager();
        let running_here = self.launched();
        let allow_dev = self.allow_development_build.load(Ordering::SeqCst);
        let outcome = match spec_for(state) {
            // The item starts `--port 0`, the port the app saved; without one it would listen where no client looks.
            Ok(_) if wanted && manager.kind().is_some() && !has_desktop_port(&state.paths.root) => {
                Applied { error: Some(NO_DESKTOP_PORT.to_string()), retry: true, ..Applied::default() }
            }
            Err(error) if manager.kind().is_some() => {
                Applied { error: Some(format!("Kybern could not check its login item: {error:#}")), retry: true, ..Applied::default() }
            }
            Err(_) => Applied::default(),
            Ok(spec) => tokio::task::spawn_blocking(move || apply(manager.as_ref(), &spec, wanted, running_here, allow_dev))
                .await
                .unwrap_or_else(|error| Applied {
                    error: Some(format!("Kybern could not check its login item: {error}")),
                    retry: true,
                    ..Applied::default()
                }),
        };
        if let Some(error) = &outcome.error {
            tracing::warn!(target: TARGET, %error, wanted, "the login item is not as it should be");
        }
        *self.applied() = Applied { wanted: Some(wanted), ..outcome };
    }
}

/// Why the item is not registered yet on a machine whose daemon the desktop app never started.
pub const NO_DESKTOP_PORT: &str = "Kybern registers itself to start at login once the desktop app has chosen this machine's daemon port. Open Kybern on this machine once, then try again.";

/// The desktop app saved the port its daemon listens on (`desktop.port`), which the item's `--port 0` means.
pub fn has_desktop_port(root: &Path) -> bool {
    std::fs::read_to_string(root.join("desktop.port")).ok().and_then(|text| text.trim().parse::<u16>().ok()).is_some_and(|port| port != 0)
}

/// Whether the login item should exist: the setting is on, the network is on, and this machine is a
/// member of a network it was not removed from. This is also what keeps the idle exit away.
pub fn wanted(state: &AppState) -> bool {
    let network = state.settings.get().network;
    network.enabled
        && network.keep_reachable
        && state.network.membership().is_some_and(|membership| membership.self_is_member() && !membership.self_removed())
}

/// The item for this daemon: its own binary and data folder, both absolute (a login item runs from
/// another working directory).
fn spec_for(state: &AppState) -> Result<ServiceSpec> {
    let data_dir = std::path::absolute(&state.paths.root)?;
    let default_dir = crate::config::default_data_dir();
    let exe = installed_binary(std::env::current_exe()?)?;
    Ok(ServiceSpec {
        name: service_name(BASE_NAME, &data_dir, default_dir.as_deref()),
        exe,
        data_dir,
        path_env: std::env::var("PATH").ok().filter(|path| !path.is_empty()),
    })
}

/// The path that starts this binary at the next login. On Linux a binary an update replaced reports
/// itself as `<path> (deleted)`; the new one at `<path>` is what the item should start.
pub fn installed_binary(exe: PathBuf) -> Result<PathBuf> {
    let text = exe.to_string_lossy();
    let exe = match text.strip_suffix(" (deleted)") {
        Some(path) => PathBuf::from(path),
        None => exe,
    };
    if !exe.is_file() {
        anyhow::bail!("this daemon's binary is gone from {}; open Kybern again so the new one registers itself", exe.display());
    }
    Ok(exe)
}

/// The item's name for `~/.kybern` on this platform.
#[cfg(windows)]
const BASE_NAME: &str = "kybernd";
#[cfg(target_os = "macos")]
const BASE_NAME: &str = "dev.kybern.kybernd.login";
#[cfg(not(any(windows, target_os = "macos")))]
const BASE_NAME: &str = "kybern-login";

/// The OS work of one reconcile, off the async runtime.
fn apply(manager: &dyn ServiceManager, spec: &ServiceSpec, wanted: bool, running_here: bool, allow_dev: bool) -> Applied {
    if manager.kind().is_none() {
        return Applied::default();
    }
    let registration = match manager.registration(spec) {
        Ok(registration) => registration,
        Err(error) => {
            return Applied { error: Some(format!("Kybern could not read its login item: {error:#}")), retry: true, ..Applied::default() };
        }
    };
    // Another service starts this data folder's daemon: ours would start a second one at login.
    if let Some(external) = manager.external(&spec.data_dir) {
        let removed = registration == Registration::Missing || manager.uninstall(spec, running_here).is_ok();
        return Applied { external: Some(external), registered: !removed, retry: !removed, ..Applied::default() };
    }
    let result = match (wanted, registration) {
        (true, Registration::Current) => Ok(true),
        (true, existing) if is_development_build(&spec.exe) && !allow_dev => {
            tracing::info!(target: TARGET, exe = %spec.exe.display(), "a development build does not register itself to start at login");
            return Applied {
                registered: existing != Registration::Missing,
                error: Some(format!(
                    "This kybernd is a development build, so it does not register itself to start at login. Run the installed Kybern, or set {ANY_BINARY_ENV}=1."
                )),
                ..Applied::default()
            };
        }
        (true, _) => manager.install(spec).map(|()| {
            tracing::info!(target: TARGET, name = %spec.name, exe = %spec.exe.display(), "registered the login item");
            true
        }),
        (false, Registration::Missing) => Ok(false),
        (false, _) => manager.uninstall(spec, running_here).map(|()| {
            tracing::info!(target: TARGET, name = %spec.name, "removed the login item");
            false
        }),
    };
    match result {
        Ok(registered) => Applied { registered, ..Applied::default() },
        Err(error) if wanted => Applied {
            registered: registration == Registration::Stale,
            error: Some(format!("Kybern could not register itself to start at login: {error:#}")),
            retry: true,
            ..Applied::default()
        },
        Err(error) => Applied {
            registered: true,
            error: Some(format!("Kybern could not remove its login item: {error:#}")),
            retry: true,
            ..Applied::default()
        },
    }
}

/// Keeps the login item in line with membership and settings for as long as the daemon runs.
pub async fn run(state: AppState) {
    let mut settings = state.settings.subscribe();
    let mut changes = state.network.subscribe_changes();
    state.login_service.reconcile(&state, true).await;
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            changed = settings.recv() => if matches!(changed, Err(tokio::sync::broadcast::error::RecvError::Closed)) { break },
            changed = changes.recv() => if matches!(changed, Err(tokio::sync::broadcast::error::RecvError::Closed)) { break },
            // Membership can change without a notification (a removal applied while no client watched).
            _ = tokio::time::sleep(Duration::from_secs(60)) => {}
        }
        state.login_service.reconcile(&state, false).await;
    }
}

/// `network.keep_reachable.set`: save the setting and register or remove the item before answering.
pub async fn set_enabled(state: &AppState, enabled: bool) -> Result<()> {
    let mut settings = state.settings.get();
    settings.network.keep_reachable = enabled;
    state.settings.set(settings)?;
    state.login_service.reconcile(state, true).await;
    Ok(())
}

/// `kybernd --login-service` at login: another daemon already serves this data folder (the app was
/// opened first, or a previous login's daemon is still up), or the desktop app is starting one right
/// now. A daemon serves when both endpoint files name it and its `/health` answers; the app is starting
/// one while its startup lock or announcement is fresh (it holds them for at most 15 s).
pub fn already_served(root: &Path) -> bool {
    let fresh = |name: &str| {
        std::fs::metadata(root.join(name))
            .and_then(|meta| meta.modified())
            .is_ok_and(|at| at.elapsed().is_ok_and(|age| age < Duration::from_secs(15)))
    };
    if fresh(".gpuix-startup-lock") || fresh(".gpuix-starting.json") {
        return true;
    }
    let Some(port) = std::fs::read_to_string(root.join("daemon.port")).ok().and_then(|text| text.trim().parse::<u16>().ok()) else {
        return false;
    };
    port != 0 && root.join("daemon.listen").exists() && answers_health(port)
}

/// Whether a Kybern daemon answers `GET /health` on this loopback port, not merely something listening.
fn answers_health(port: u16) -> bool {
    use std::io::{Read, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else { return false };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    if stream.write_all(b"GET /health HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n").is_err() {
        return false;
    }
    let mut answer = String::new();
    let _ = stream.take(4096).read_to_string(&mut answer);
    answer.starts_with("HTTP/1.") && answer.split_whitespace().nth(1) == Some("200") && answer.trim_end().ends_with("ok")
}

#[cfg(test)]
mod tests;
