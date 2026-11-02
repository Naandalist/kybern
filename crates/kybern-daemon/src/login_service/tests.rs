use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use kybern_protocol::{BackgroundSettings, JoinVia, LoginServiceKind};

use super::*;
use crate::network::harness::{Cluster, NodeSpec, WAIT, eventually};

/// A manager that keeps its one "registration" in memory and records what it was asked.
#[derive(Default)]
struct Recording {
    registered: Mutex<Option<ServiceSpec>>,
    calls: Mutex<Vec<String>>,
    external: Option<String>,
    fail_install: bool,
}

impl Recording {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn registered(&self) -> bool {
        self.registered.lock().unwrap().is_some()
    }
}

impl ServiceManager for Recording {
    fn kind(&self) -> Option<LoginServiceKind> {
        Some(LoginServiceKind::TaskScheduler)
    }
    fn registration(&self, spec: &ServiceSpec) -> Result<Registration> {
        Ok(match &*self.registered.lock().unwrap() {
            None => Registration::Missing,
            Some(existing) if existing.exe == spec.exe => Registration::Current,
            Some(_) => Registration::Stale,
        })
    }
    fn install(&self, spec: &ServiceSpec) -> Result<()> {
        self.calls.lock().unwrap().push(format!("install {}", spec.exe.display()));
        if self.fail_install {
            anyhow::bail!("Access is denied.");
        }
        *self.registered.lock().unwrap() = Some(spec.clone());
        Ok(())
    }
    fn uninstall(&self, _spec: &ServiceSpec, running_here: bool) -> Result<()> {
        self.calls.lock().unwrap().push(format!("uninstall running_here={running_here}"));
        *self.registered.lock().unwrap() = None;
        Ok(())
    }
    fn external(&self, _data_dir: &Path) -> Option<String> {
        self.external.clone()
    }
}

fn spec(exe: &str) -> ServiceSpec {
    ServiceSpec { name: "kybernd".into(), exe: PathBuf::from(exe), data_dir: PathBuf::from("/d"), path_env: None }
}

const INSTALLED: &str = "/opt/Kybern/kybernd";

#[test]
fn the_default_data_folder_gets_the_plain_name_and_any_other_a_stable_hashed_one() {
    let default = std::env::temp_dir().join("kybern-login-name-default");
    let scratch = std::env::temp_dir().join("kybern-login-name-scratch");
    assert_eq!(service_name("kybernd", &default, Some(&default)), "kybernd");
    let hashed = service_name("kybernd", &scratch, Some(&default));
    assert!(hashed.starts_with("kybernd-") && hashed.len() == "kybernd-".len() + 8, "{hashed}");
    assert_eq!(hashed, service_name("kybernd", &scratch, Some(&default)), "the same folder, the same name");
    assert_ne!(hashed, service_name("kybernd", &std::env::temp_dir().join("other"), Some(&default)));
}

#[test]
fn a_binary_in_a_cargo_target_folder_is_a_development_build() {
    for exe in
        ["/src/kybern/target/debug/kybernd", r"D:\work\target\release\kybernd.exe", "/w/target/x86_64-unknown-linux-gnu/release/kybernd"]
    {
        assert!(is_development_build(Path::new(&exe.replace('\\', std::path::MAIN_SEPARATOR_STR))), "{exe}");
    }
    for exe in
        ["/Applications/Kybern.app/Contents/MacOS/kybernd", r"C:\Users\me\AppData\Local\Programs\Kybern\kybernd.exe", "/usr/bin/kybernd"]
    {
        assert!(!is_development_build(Path::new(&exe.replace('\\', std::path::MAIN_SEPARATOR_STR))), "{exe}");
    }
}

#[test]
fn apply_registers_when_wanted_and_removes_when_not() {
    let manager = Recording::default();
    let outcome = apply(&manager, &spec(INSTALLED), true, false, false);
    assert!(outcome.registered && outcome.error.is_none());
    assert_eq!(manager.calls(), ["install /opt/Kybern/kybernd"]);
    let again = apply(&manager, &spec(INSTALLED), true, false, false);
    assert!(again.registered);
    assert_eq!(manager.calls().len(), 1, "a current item is left as it is");
    // An update moved the binary: the item follows it.
    apply(&manager, &spec("/opt/Kybern2/kybernd"), true, false, false);
    assert_eq!(manager.calls().last().unwrap(), "install /opt/Kybern2/kybernd");
    // The daemon the item started is told not to stop itself while removing it.
    let removed = apply(&manager, &spec("/opt/Kybern2/kybernd"), false, true, false);
    assert!(!removed.registered && !manager.registered());
    assert_eq!(manager.calls().last().unwrap(), "uninstall running_here=true");
    let before = manager.calls().len();
    apply(&manager, &spec(INSTALLED), false, false, false);
    assert_eq!(manager.calls().len(), before, "nothing to remove");
}

#[test]
fn apply_leaves_an_external_service_and_reports_a_failure_with_its_reason() {
    let external = Recording {
        external: Some("The systemd user service kybernd.service already starts Kybern at login.".into()),
        ..Recording::default()
    };
    let outcome = apply(&external, &spec(INSTALLED), true, false, false);
    assert!(outcome.external.is_some() && !outcome.registered && external.calls().is_empty());

    let failing = Recording { fail_install: true, ..Recording::default() };
    let outcome = apply(&failing, &spec(INSTALLED), true, false, false);
    assert!(!outcome.registered);
    assert!(outcome.error.as_deref().is_some_and(|error| error.contains("Access is denied.")), "{outcome:?}");

    let unsupported = apply(&Inert, &spec(INSTALLED), true, false, false);
    assert!(!unsupported.registered && unsupported.error.is_none(), "nothing to say where nothing can be registered");
}

#[test]
fn an_external_service_replaces_ours_and_a_failure_is_tried_again() {
    let manager = Recording::default();
    apply(&manager, &spec(INSTALLED), true, false, false);
    assert!(manager.registered());
    let both = Recording {
        external: Some("The systemd user service kybernd.service already starts Kybern at login.".into()),
        ..Recording::default()
    };
    *both.registered.lock().unwrap() = Some(spec(INSTALLED));
    let outcome = apply(&both, &spec(INSTALLED), true, false, false);
    assert!(!both.registered() && !outcome.registered && outcome.external.is_some(), "two items would start two daemons");
    assert!(!outcome.retry);
    let failing = Recording { fail_install: true, ..Recording::default() };
    assert!(apply(&failing, &spec(INSTALLED), true, false, false).retry, "a refused registration is tried again at the next reconcile");
    assert!(!apply(&manager, &spec(INSTALLED), true, false, false).retry);
}

#[test]
fn the_item_starts_the_binary_an_update_put_in_place() {
    let dir = crate::network::testutil::TempDir::new("login-binary");
    let exe = dir.path().join("kybernd");
    std::fs::write(&exe, b"binary").unwrap();
    let deleted = PathBuf::from(format!("{} (deleted)", exe.display()));
    assert_eq!(installed_binary(deleted).unwrap(), exe, "Linux names a replaced binary `<path> (deleted)`");
    assert_eq!(installed_binary(exe.clone()).unwrap(), exe);
    assert!(installed_binary(dir.path().join("gone")).is_err(), "never register a binary that is not there");
}

#[test]
fn a_development_build_never_registers_itself() {
    let manager = Recording::default();
    let outcome = apply(&manager, &spec("/src/kybern/target/debug/kybernd"), true, false, false);
    assert!(!outcome.registered && manager.calls().is_empty());
    assert!(outcome.error.as_deref().is_some_and(|error| error.contains("development build")));
    // It may still remove what is there.
    *manager.registered.lock().unwrap() = Some(spec(INSTALLED));
    apply(&manager, &spec("/src/kybern/target/debug/kybernd"), false, false, false);
    assert!(!manager.registered());
}

/// The binary these tests run is a development build; the item is registered for the installed one.
fn installed_manager() -> Arc<Recording> {
    Arc::new(Recording::default())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn joining_registers_the_login_item_and_being_removed_or_turning_it_off_removes_it() {
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b")]) else { return };
    let a = cluster.start("a", true).await;
    let b = cluster.start("b", true).await;
    let (at_a, at_b) = (installed_manager(), installed_manager());
    a.state.login_service.set_manager(at_a.clone());
    b.state.login_service.set_manager(at_b.clone());
    // The test binary is a development build.
    a.state.login_service.allow_development_build();
    b.state.login_service.allow_development_build();
    tokio::spawn(run(a.state.clone()));
    tokio::spawn(run(b.state.clone()));
    // A lone machine is reachable by no one: nothing is registered.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!at_a.registered() && !at_b.registered());
    assert!(!wanted(&a.state));

    // `a`'s daemon was started by the desktop app, which saved its port; `b`'s never was.
    std::fs::write(a.state.paths.root.join("desktop.port"), "59931").unwrap();
    cluster.form(&a, &[&b], JoinVia::Auto).await;
    eventually("a registers once it is in a network", WAIT, || at_a.registered()).await;
    // The item's `--port 0` means the saved port: without one, it would start a daemon on a random port.
    eventually("b says why it has no login item", WAIT, || {
        b.state.login_service.status(&b.state).error.as_deref() == Some(NO_DESKTOP_PORT)
    })
    .await;
    assert!(wanted(&b.state) && !at_b.registered());
    std::fs::write(b.state.paths.root.join("desktop.port"), "59932").unwrap();
    set_enabled(&b.state, true).await.unwrap();
    assert!(at_b.registered(), "registered once the app saved its port");
    assert!(b.state.login_service.status(&b.state).error.is_none());
    let status = a.state.login_service.status(&a.state);
    assert!(status.enabled && status.wanted && status.registered && !status.running && status.kind.is_some());

    // The setting off removes it, on registers it again.
    set_enabled(&a.state, false).await.unwrap();
    assert!(!at_a.registered(), "removed before the answer");
    assert!(!a.state.login_service.status(&a.state).registered);
    set_enabled(&a.state, true).await.unwrap();
    assert!(at_a.registered());

    // Removed from the network: gone.
    crate::network::removal::remove(&a.state, &b.id(), false, &[]).await.unwrap();
    eventually("b removes its login item once it learns it was removed", WAIT, || !at_b.registered()).await;

    // Turning the network off: gone too.
    a.set_network_enabled(false);
    eventually("a removes its login item with the network off", WAIT, || !at_a.registered()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_idle_exit_spares_a_daemon_that_keeps_its_machine_reachable() {
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b")]) else { return };
    let a = cluster.start("a", false).await;
    let b = cluster.start("b", false).await;
    cluster.form(&a, &[&b], JoinVia::Auto).await;
    assert!(wanted(&a.state));
    let policy = BackgroundSettings { daemon_idle_exit_minutes: 1, ..BackgroundSettings::default() };
    let long_ago = chrono::Utc::now() - chrono::Duration::hours(2);
    *a.state.idle_since.write().unwrap() = Some(long_ago);
    crate::maintenance::sweep(&a.state, &policy, true).await.unwrap();
    assert!(!a.state.shutdown.is_cancelled(), "the network needs it");

    let mut settings = a.state.settings.get();
    settings.network.keep_reachable = false;
    a.state.settings.set(settings).unwrap();
    *a.state.idle_since.write().unwrap() = Some(long_ago);
    crate::maintenance::sweep(&a.state, &policy, true).await.unwrap();
    assert!(a.state.shutdown.is_cancelled(), "without the setting the idle exit applies as before");
}

#[test]
fn a_login_item_start_finds_a_daemon_that_already_serves_the_folder() {
    let root = crate::network::testutil::TempDir::new("login-served");
    assert!(!already_served(root.path()), "no endpoint files");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::fs::write(root.path().join("daemon.port"), port.to_string()).unwrap();
    assert!(!already_served(root.path()), "daemon.listen is written with the port file");
    std::fs::write(root.path().join("daemon.listen"), format!("127.0.0.1:{port}")).unwrap();
    // Something else took the port of a daemon that crashed: it accepts, but it is not Kybern.
    let silent = std::thread::spawn({
        let listener = listener.try_clone().unwrap();
        move || drop(listener.accept())
    });
    assert!(!already_served(root.path()), "a listener that does not answer /health");
    silent.join().unwrap();
    let daemon = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0u8; 512];
        let _ = stream.read(&mut request);
        stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok").unwrap();
    });
    assert!(already_served(root.path()));
    daemon.join().unwrap();
    assert!(!already_served(root.path()), "stale files of a daemon that is gone");

    // The desktop app is starting a daemon right now.
    let starting = crate::network::testutil::TempDir::new("login-starting");
    std::fs::write(starting.path().join(".gpuix-startup-lock"), "1:x").unwrap();
    assert!(already_served(starting.path()));
}

/// The real LaunchAgent round trip on this Mac: write, load, read back, unload, remove. It runs
/// `/usr/bin/true` under a throwaway label, so it never starts a daemon. Run it by hand:
/// `cargo test -p kybern-daemon login_service::tests::launch_agent_real -- --ignored`.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "touches this user's launchd domain"]
fn launch_agent_real_round_trip() {
    let manager = macos::LaunchAgent;
    let data = crate::network::testutil::TempDir::new("login-launchd");
    let spec = ServiceSpec {
        name: format!("dev.kybern.kybernd.login.selftest-{}", std::process::id()),
        exe: PathBuf::from("/usr/bin/true"),
        data_dir: data.path().to_path_buf(),
        path_env: Some("/usr/bin:/bin".into()),
    };
    let plist = PathBuf::from(std::env::var("HOME").unwrap()).join("Library/LaunchAgents").join(format!("{}.plist", spec.name));
    let label = format!("gui/{}/{}", unsafe { libc::getuid() }, spec.name);
    let loaded = || std::process::Command::new("/bin/launchctl").args(["print", &label]).output().unwrap().status.success();

    assert_eq!(manager.registration(&spec).unwrap(), Registration::Missing);
    manager.install(&spec).unwrap();
    let lint = std::process::Command::new("/usr/bin/plutil").arg("-lint").arg(&plist).output().unwrap();
    assert!(lint.status.success(), "plutil: {}", String::from_utf8_lossy(&lint.stdout));
    assert_eq!(manager.registration(&spec).unwrap(), Registration::Current);
    assert!(loaded(), "launchctl did not load {label}");

    manager.uninstall(&spec, false).unwrap();
    assert!(!plist.exists(), "the plist is still there");
    assert!(!loaded(), "{label} is still loaded");
    assert_eq!(manager.registration(&spec).unwrap(), Registration::Missing);
}
