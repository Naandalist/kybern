mod access;
mod account_identity;
mod account_login;
mod agent_guide;
mod antigravity_mcp;
mod app_tools;
mod artifacts;
mod askpass;
mod auth;
mod bounded_broadcast;
mod chips;
mod cli_login;
mod computer;
mod config;
mod cursor_setup;
mod discovery;
mod exposure;
mod files;
mod github;
mod github_actions;
mod github_review;
mod harness_updates;
mod http;
mod integrations;
mod lead_git;
mod login_service;
mod maintenance;
mod native_tools_mcp;
// Transport (N3) is wired in below; the joining flows and `network.*` handlers (N4) use the rest.
#[allow(dead_code)]
mod network;
mod orchestrator;
mod pick;
mod power;
mod previews;
mod project_map;
mod provider_accounts;
mod provider_assets;
#[cfg(test)]
mod remote_tests;
mod requests;
mod rpc;
#[cfg(test)]
mod test_support;
// S3: the encrypted secret store. Its RPC, push transport and outbox hooks arrive with N3/R1.
mod plugins;
#[allow(dead_code)]
mod secret_store;
mod secrets;
mod self_update;
mod session_import;
mod settings;
mod settings_share;
mod skills;
mod state;
mod terminal;
mod thread_image;
mod thread_projection;
mod thumbnail;
mod usage;
mod visuals;
mod windows_path;
mod ws;

use std::fs::OpenOptions;
use std::io::Write;
use std::net::SocketAddr;

use anyhow::{Result, anyhow};
use axum::Router;
use axum::routing::get;
use clap::Parser;
use kybern_protocol::PROTOCOL_VERSION;
use serde::Serialize;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "kybernd", version, about = "kybern daemon")]
struct Args {
    /// Port to listen on (loopback only unless --bind is given).
    #[arg(long, env = "KYBERN_PORT", default_value_t = kybern_protocol::DEFAULT_PORT)]
    port: u16,
    /// Bind address. Defaults to loopback; use a private interface for direct remote access.
    #[arg(long, env = "KYBERN_BIND", default_value = "127.0.0.1")]
    bind: String,
    /// Reachable public or private proxy WebSocket URL to include in invitations.
    #[arg(long, env = "KYBERN_ADVERTISE_URL")]
    advertise_url: Option<String>,
    /// Print a one-time pairing code and detected addresses, then keep serving.
    #[arg(long, conflicts_with = "print_token")]
    pair: bool,
    /// Data directory. Defaults to ~/.kybern.
    #[arg(long, env = "KYBERN_DATA_DIR")]
    data_dir: Option<std::path::PathBuf>,
    /// Print the bootstrap token and exit.
    #[arg(long)]
    print_token: bool,
    /// Private desktop bootstrap handshake. Not part of the public daemon CLI.
    #[arg(long, hide = true)]
    desktop_startup_id: Option<String>,
    /// Started at login by the item "Keep this machine reachable" registers (ADE-119). Exits at once
    /// when another daemon already serves the data folder. Not part of the public daemon CLI.
    #[arg(long, hide = true, conflicts_with_all = ["pair", "print_token", "desktop_startup_id"])]
    login_service: bool,
}

#[derive(Serialize)]
struct DesktopStartupAnnouncement {
    port: u16,
    protocol_version: u32,
    version: &'static str,
}

fn startup_announcement_filename(id: &str) -> Result<String> {
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_') {
        return Err(anyhow!("invalid desktop startup id"));
    }
    Ok(format!(".desktop-startup-{id}.json"))
}

fn write_startup_announcement(paths: &config::Paths, id: &str, port: u16) -> Result<()> {
    let path = paths.root.join(startup_announcement_filename(id)?);
    let mut file = OpenOptions::new().write(true).create_new(true).open(&path)?;
    serde_json::to_writer(
        &mut file,
        &DesktopStartupAnnouncement { port, protocol_version: PROTOCOL_VERSION, version: env!("CARGO_PKG_VERSION") },
    )?;
    file.flush()?;
    tracing::debug!(path = %path.display(), port, "published desktop startup endpoint");
    Ok(())
}

fn desktop_listen_port(requested: u16, desktop_managed: bool, root: &std::path::Path) -> Result<u16> {
    if requested != 0 || !desktop_managed {
        return Ok(requested);
    }
    // Self-update restarts inherit the original --port 0 argument. Retain the
    // paired address even when no desktop process is around to select it.
    let path = root.join("desktop.port");
    match std::fs::read_to_string(&path) {
        Ok(value) => value
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| anyhow!("Invalid saved desktop port at {}; correct it before restarting Kybern", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

/// The login item passes `--port 0`, meaning the port the desktop app saved. Without that file it would bind a random
/// port that no client knows, and a second daemon could end up serving the same data folder.
fn login_item_may_listen(login_service: bool, port: u16) -> bool {
    !login_service || port != 0
}

/// Stack for each runtime thread. Unoptimized builds give every arm of an
/// `rpc` dispatch match its own stack slots (the whole match was a poll frame of
/// about 1.4 MB on macOS and larger with MSVC, now split into parts), so a
/// preview snapshot on top of it overflowed Tokio's 2 MiB default and aborted
/// the daemon on Windows. This is address space; pages are committed only as
/// they are touched.
pub(crate) const RUNTIME_THREAD_STACK: usize = 8 * 1024 * 1024;

fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(RUNTIME_THREAD_STACK).build()
}

/// Run the daemon with the process arguments. `kybernd` is a thin wrapper around this.
/// Windows: a daemon hosted in a console (a scheduled task's `run.cmd`, a terminal) is ended a few
/// seconds after that console closes or the system shuts down, with no other trace. Shut down
/// cleanly instead, and say why in the log. Never resolves elsewhere.
async fn console_closed() {
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_close, ctrl_shutdown};
        let (Ok(mut close), Ok(mut shutdown)) = (ctrl_close(), ctrl_shutdown()) else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = close.recv() => tracing::warn!("the console hosting the daemon closed"),
            _ = shutdown.recv() => tracing::warn!("Windows is shutting down"),
        }
    }
    #[cfg(not(windows))]
    std::future::pending::<()>().await
}

/// `daemon.log`, opened for appending, for a daemon the Windows login item started.
fn login_service_log(login_service: bool, root: &std::path::Path) -> Option<std::fs::File> {
    if !(cfg!(windows) && login_service) {
        return None;
    }
    OpenOptions::new().create(true).append(true).open(root.join("daemon.log")).ok()
}

pub fn run() -> Result<()> {
    runtime()?.block_on(serve())
}

async fn serve() -> Result<()> {
    let args = Args::parse();
    let paths = config::Paths::resolve(args.data_dir.clone())?;
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,kybern=debug"));
    // Windows' login item runs the daemon under a headless console, where its output goes nowhere:
    // it logs to `daemon.log` itself, as the app's own launch does. launchd and systemd keep the output.
    match login_service_log(args.login_service, &paths.root) {
        Some(file) => tracing_subscriber::fmt()
            .with_env_filter(filter())
            .with_target(true)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init(),
        None => tracing_subscriber::fmt().with_env_filter(filter()).with_target(true).init(),
    }

    widen_path();
    // Before the state opens: a second daemon would recover the first one's turns.
    if args.login_service && login_service::already_served(&paths.root) {
        tracing::info!(data_dir = %paths.root.display(), "another daemon already serves this data folder; the login item has nothing to start");
        return Ok(());
    }
    if !login_item_may_listen(args.login_service, desktop_listen_port(args.port, args.login_service, &paths.root)?) {
        tracing::warn!(
            data_dir = %paths.root.display(),
            "the login item has no saved desktop port (desktop.port) and would listen on a random port beside the daemon the app starts; it starts nothing. Open Kybern on this machine once to save the port."
        );
        return Ok(());
    }
    let state = state::AppState::initialize(&paths)?;

    if args.print_token {
        state.orchestrator.recover_after_restart().await?;
        println!("{}", state.bootstrap_token);
        return Ok(());
    }
    state.login_service.use_platform().await;
    if args.login_service {
        state.login_service.mark_launched();
    }

    // The login item runs the app's own binary on the port the app chose (`desktop.port`), so it is
    // desktop-managed too: the app updates and replaces it.
    let desktop_managed = args.desktop_startup_id.is_some() || args.login_service;
    let requested_port = desktop_listen_port(args.port, desktop_managed, &paths.root)?;
    let requested_addr = SocketAddr::new(args.bind.parse()?, requested_port);
    let listener = tokio::net::TcpListener::bind(requested_addr).await?;
    let addr = listener.local_addr()?;
    state.port.store(addr.port(), std::sync::atomic::Ordering::Relaxed);
    *state.listen_addr.write().unwrap() = Some(addr);
    state.native_tools.set_endpoint(addr);
    if let Some(url) = args.advertise_url {
        *state.advertised_urls.write().unwrap() = vec![kybern_client::address::normalize(&url)?];
    }
    if let Some(startup_id) = &args.desktop_startup_id {
        std::fs::write(paths.root.join("desktop.port"), addr.port().to_string())?;
        write_startup_announcement(&paths, startup_id, addr.port())?;
    }
    state.desktop_managed.store(desktop_managed, std::sync::atomic::Ordering::SeqCst);
    state.process_owned.store(true, std::sync::atomic::Ordering::SeqCst);

    // Recovery may need to close interrupted turns and capture final snapshots.
    // Keep the port reserved while it runs, but do not serve requests or publish
    // daemon.port to general clients until the state is consistent.
    state.orchestrator.recover_after_restart().await?;

    let orphans = account_login::orphaned_staging(&state);
    if !orphans.is_empty() {
        tokio::spawn(account_login::remove_orphans(state.clone(), orphans));
    }
    let update_worker = tokio::spawn(harness_updates::run(state.clone()));
    let self_update_worker = tokio::spawn(self_update::run(state.clone()));
    let maintenance_worker = tokio::spawn(maintenance::run(state.clone(), !args.pair));
    let notes_worker = tokio::spawn(purge_expired_notes(state.clone()));
    let queue_state = state.clone();
    let queue_worker = tokio::spawn(async move {
        // Follow-ups dispatch when their thread goes idle. The orchestrator
        // wakes this worker on every event that can make that happen; the
        // timer is only a safety net, short while work is pending and long
        // otherwise so an idle daemon does not poll the store.
        loop {
            let waiting = match queue_state.orchestrator.drain_queues().await {
                Ok(waiting) => waiting,
                Err(error) => {
                    tracing::error!(%error, "could not dispatch queued follow-up");
                    true
                }
            };
            let fallback = if waiting { std::time::Duration::from_secs(2) } else { std::time::Duration::from_secs(60) };
            tokio::select! {
                _ = queue_state.shutdown.cancelled() => break,
                _ = queue_state.orchestrator.queue_changed() => {}
                _ = tokio::time::sleep(fallback) => {}
            }
        }
    });

    let app = Router::new()
        .merge(http::routes())
        .merge(native_tools_mcp::routes())
        .merge(network::routes())
        .route("/ws", get(ws::upgrade))
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(state.clone());

    tracing::info!(%addr, data_dir = %paths.root.display(), "kybernd listening");
    std::fs::write(paths.root.join("daemon.listen"), addr.to_string())?;
    std::fs::write(&paths.port_file, addr.port().to_string())?;

    state.exposure.attach(app.clone()).await;
    if state.settings.get().access.tailscale
        && let Err(error) = state.exposure.set_tailscale(addr, true).await
    {
        tracing::warn!(%error, "could not listen on Tailscale; pairing stays loopback-only until it is enabled again");
    }

    // Discovery, the network listener and the sessions with other members. It opens the Tailscale
    // listener itself once the network is on and Tailscale is running (design §3.1).
    let network_worker = tokio::spawn(network::peers::run(state.clone()));
    // Delivery of work queued for machines that were offline, and the followers of agents that run
    // on other machines (they pick up where a restart left them).
    let outbox_worker = tokio::spawn(network::outbox::run(state.clone()));
    state.orchestrator.remote_start();
    // Which repository each project is, for matching checkouts across machines (design §4.4).
    let repo_identity_worker = tokio::spawn(network::repo_identity::run(state.clone()));
    // "Keep all machines on the same version": follows the newest version another member runs.
    let follow_updates_worker = tokio::spawn(network::updates::run(state.clone()));
    // The login broker's watch (rotation, refresh ahead of expiry) and every member's catch-up after a move.
    let login_broker_worker = tokio::spawn(network::broker::run(state.clone()));
    // The universal clipboard: idle unless this machine's owner turned on "Share clipboard automatically".
    let clipboard_worker = tokio::spawn(network::clipboard::run(state.clone()));
    // "Keep this machine reachable when Kybern is closed": the login item follows membership and settings.
    let login_service_worker = tokio::spawn({
        let state = state.clone();
        async move {
            state.login_service.migrate(&state.paths.root).await;
            login_service::run(state).await;
        }
    });

    if args.pair {
        let endpoints = crate::access::endpoints(&state).await;
        let (code, expires_at) = state.pairing.create(None)?;
        let pairing = kybern_protocol::methods::PairingCreateResult { code, expires_at, endpoints };
        print!("{}", kybern_client::pairing::PairingReport::new(&pairing, &state.environment_id)?.render());
        std::io::stdout().flush()?;
    }

    let shutdown_token = state.shutdown.clone();
    let shutdown_orchestrator = state.orchestrator.clone();
    let shutdown = async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("Ctrl+C received"),
            _ = console_closed() => {}
            _ = shutdown_token.cancelled() => {}
        }
        // Signal-driven shutdown must also wake maintenance/update workers
        // before we await them and release the endpoint files.
        shutdown_token.cancel();
        // The same signal reaches the agents; their failures from here on are this stop.
        shutdown_orchestrator.begin_shutdown();
        tracing::info!("shutting down");
    };
    // The askpass endpoint checks which process holds the connection (`askpass::connection_owned_by`).
    // TCP_NODELAY: previews, relayed pages and session frames are small, latency-bound writes.
    use axum::serve::ListenerExt as _;
    let listener = listener.tap_io(|socket| {
        let _ = socket.set_nodelay(true);
    });
    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).with_graceful_shutdown(shutdown).await?;
    queue_worker.abort();
    notes_worker.abort();
    let _ = network_worker.await;
    outbox_worker.abort();
    repo_identity_worker.abort();
    follow_updates_worker.abort();
    login_broker_worker.abort();
    clipboard_worker.abort();
    login_service_worker.abort();
    let _ = maintenance_worker.await;
    let _ = update_worker.await;
    let _ = self_update_worker.await;
    state.orchestrator.shutdown().await;
    state.terminals.shutdown().await;
    release_endpoint_files(&paths, addr);
    if state.restart_pending.load(std::sync::atomic::Ordering::SeqCst) {
        self_update::restart(&paths.root.join("daemon.log"));
    }
    Ok(())
}

/// Notes and tasks deleted more than 30 days ago are removed for good, once at start and then hourly.
async fn purge_expired_notes(state: state::AppState) {
    loop {
        match state.orchestrator.purge_expired_notes() {
            Ok(0) => {}
            Ok(purged) => tracing::info!(purged, "removed notes deleted more than 30 days ago"),
            Err(error) => tracing::warn!(%error, "could not remove expired notes"),
        }
        match state.orchestrator.purge_expired_tasks() {
            Ok(0) => {}
            Ok(purged) => tracing::info!(purged, "removed tasks deleted more than 30 days ago"),
            Err(error) => tracing::warn!(%error, "could not remove expired tasks"),
        }
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            _ = tokio::time::sleep(std::time::Duration::from_secs(60 * 60)) => {}
        }
    }
}

/// Agent CLIs usually live in per-user directories that a service manager or a
/// GUI launcher leaves off PATH. Append the usual ones so `claude`, `codex` and
/// friends resolve without a binary override, keeping the inherited order first.
/// On Windows the registry's Path comes next, so an app started with an old or
/// thin Path still finds what a new terminal finds (`windows_path`).
fn widen_path() {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut entries: Vec<std::path::PathBuf> = std::env::split_paths(&current).collect();
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(std::path::PathBuf::from);
    let mut candidates: Vec<std::path::PathBuf> = windows_path::registry_path();
    if let Some(home) = &home {
        candidates.extend([".local/bin", ".cargo/bin", ".bun/bin"].map(|relative| home.join(relative)));
        let nvm = std::env::var_os("NVM_DIR").map(std::path::PathBuf::from).unwrap_or_else(|| home.join(".nvm"));
        if let Ok(node) = std::fs::read_link(nvm.join("current")) {
            candidates.push(node.join("bin"));
        }
        candidates.extend(nvm_default_bin(&nvm));
    }
    if cfg!(windows) {
        candidates.extend(windows_path::tool_dirs(|name| std::env::var_os(name)));
    } else {
        candidates.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(std::path::PathBuf::from));
    }
    let mut seen: std::collections::HashSet<String> = entries.iter().map(|entry| windows_path::dedupe_key(entry)).collect();
    for candidate in candidates {
        if candidate.is_dir() && seen.insert(windows_path::dedupe_key(&candidate)) {
            entries.push(candidate);
        }
    }
    if let Ok(joined) = std::env::join_paths(entries) {
        // SAFETY: called once at startup before any thread is spawned.
        unsafe { std::env::set_var("PATH", joined) };
    }
}

/// The bin directory of nvm's default Node. `nvm.sh` adds it to PATH only in
/// an interactive shell, so a daemon started without the user's shell PATH
/// (a slow login shell at boot, launchd, a GUI launcher) never sees `codex`
/// and other global npm installs. Follows alias chains such as `lts/*` →
/// `lts/krypton` → `v24.15.0`; a partial version such as `24` picks the newest
/// matching install, and an alias that names no install picks the newest one.
fn nvm_default_bin(nvm: &std::path::Path) -> Option<std::path::PathBuf> {
    fn parse(version: &str) -> Option<Vec<u64>> {
        version.trim().trim_start_matches('v').split('.').map(|part| part.parse().ok()).collect()
    }
    let mut installed: Vec<(Vec<u64>, std::path::PathBuf)> = std::fs::read_dir(nvm.join("versions/node"))
        .ok()?
        .flatten()
        .filter_map(|entry| Some((parse(entry.file_name().to_str()?)?, entry.path().join("bin"))))
        .filter(|(_, bin)| bin.is_dir())
        .collect();
    installed.sort_by(|a, b| b.0.cmp(&a.0));
    let mut alias = "default".to_string();
    for _ in 0..8 {
        match std::fs::read_to_string(nvm.join("alias").join(&alias)) {
            Ok(target) => alias = target.trim().to_string(),
            Err(_) => break,
        }
    }
    if alias == "system" {
        return None;
    }
    let wanted = parse(&alias);
    installed
        .iter()
        .find(|(version, _)| wanted.as_ref().is_some_and(|wanted| version.starts_with(wanted)))
        .or_else(|| installed.first())
        .map(|(_, bin)| bin.clone())
}

/// Remove the port and listen files on exit, but only while they still name
/// this daemon. A newer daemon may have taken over the data directory; deleting
/// its files would strand every client and make the desktop spawn yet another.
fn release_endpoint_files(paths: &config::Paths, addr: SocketAddr) {
    let listen = paths.root.join("daemon.listen");
    for (path, expected) in [(&paths.port_file, addr.port().to_string()), (&listen, addr.to_string())] {
        if std::fs::read_to_string(path).is_ok_and(|contents| owns_endpoint_file(&contents, &expected)) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn owns_endpoint_file(contents: &str, expected: &str) -> bool {
    contents.trim() == expected
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::nvm_default_bin;
    use super::{desktop_listen_port, login_item_may_listen, owns_endpoint_file, runtime, startup_announcement_filename};

    /// A spawned task (every RPC runs in one) has room for more than Tokio's 2 MiB default.
    #[test]
    fn runtime_tasks_have_room_for_the_unoptimized_dispatch_frame() {
        // 48 frames of 64 KiB: 3 MiB of stack.
        fn deep(frames: usize) -> usize {
            let frame = [0u8; 64 * 1024];
            let frame = std::hint::black_box(&frame);
            if frames == 0 { frame.len() } else { frame[0] as usize + deep(frames - 1) }
        }
        let used = runtime().unwrap().block_on(async { tokio::spawn(async { deep(47) }).await.unwrap() });
        assert_eq!(used, 64 * 1024);
    }

    #[test]
    fn desktop_self_restart_reuses_the_port_but_explicit_and_scratch_ports_stay_explicit() {
        let root = std::env::temp_dir().join(format!("kybern-restart-port-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(desktop_listen_port(0, true, &root).unwrap(), 0);
        std::fs::write(root.join("desktop.port"), "59930").unwrap();
        assert_eq!(desktop_listen_port(0, true, &root).unwrap(), 59930);
        assert_eq!(desktop_listen_port(4219, true, &root).unwrap(), 4219);
        assert_eq!(desktop_listen_port(0, false, &root).unwrap(), 0);
        // The login item (`--login-service --port 0`) starts nothing without a saved port.
        assert!(login_item_may_listen(true, 59930) && login_item_may_listen(false, 0));
        let unsaved = root.join("unsaved");
        std::fs::create_dir_all(&unsaved).unwrap();
        assert!(!login_item_may_listen(true, desktop_listen_port(0, true, &unsaved).unwrap()));
        std::fs::write(root.join("desktop.port"), "broken").unwrap();
        assert!(desktop_listen_port(0, true, &root).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn endpoint_files_are_only_released_by_the_daemon_they_name() {
        assert!(owns_endpoint_file("4199\n", "4199"));
        assert!(!owns_endpoint_file("4200", "4199"));
        assert!(!owns_endpoint_file("", "4199"));
    }

    #[test]
    fn startup_announcement_filename_is_scoped_to_the_data_directory() {
        assert_eq!(startup_announcement_filename("123-456-0").unwrap(), ".desktop-startup-123-456-0.json");
    }

    #[test]
    fn startup_announcement_id_rejects_path_traversal() {
        assert!(startup_announcement_filename("../daemon.token").is_err());
        assert!(startup_announcement_filename("nested/file").is_err());
        assert!(startup_announcement_filename("").is_err());
    }

    // nvm-sh uses Unix-only alias filenames (including `lts/*`).
    #[cfg(unix)]
    #[test]
    fn nvm_default_bin_follows_aliases_to_an_installed_node() {
        let nvm = std::env::temp_dir().join(format!("kybern-nvm-{}", uuid::Uuid::new_v4()));
        for version in ["v18.20.8", "v22.7.0", "v24.15.0", "v24.9.1"] {
            std::fs::create_dir_all(nvm.join("versions/node").join(version).join("bin")).unwrap();
        }
        let bin = |version: &str| Some(nvm.join("versions/node").join(version).join("bin"));
        let alias = |name: &str, target: &str| {
            let path = nvm.join("alias").join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("{target}\n")).unwrap();
        };

        assert_eq!(nvm_default_bin(&nvm), bin("v24.15.0"), "no default alias picks the newest install");
        alias("default", "lts/*");
        alias("lts/*", "lts/krypton");
        alias("lts/krypton", "v24.15.0");
        assert_eq!(nvm_default_bin(&nvm), bin("v24.15.0"));
        alias("default", "22");
        assert_eq!(nvm_default_bin(&nvm), bin("v22.7.0"));
        alias("default", "v24");
        assert_eq!(nvm_default_bin(&nvm), bin("v24.15.0"), "a partial version picks the newest match");
        alias("default", "20");
        assert_eq!(nvm_default_bin(&nvm), bin("v24.15.0"), "an uninstalled version falls back to the newest");
        alias("default", "system");
        assert_eq!(nvm_default_bin(&nvm), None);
        assert_eq!(nvm_default_bin(&nvm.join("missing")), None);
        std::fs::remove_dir_all(&nvm).unwrap();
    }
}
