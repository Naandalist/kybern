//! Process-level checks of the machine network's transport (design §11.3): real `kybernd`
//! processes, each with its own `--data-dir`, on a fake tailnet (`KYBERN_TAILSCALE_FAKE`).
//!
//! The in-process scenarios (handshake, grants, relay, heartbeat) are in `src/network/tests.rs`;
//! these cover what only a started daemon shows: the startup wiring, the listener on the Tailscale
//! address, a daemon that already runs on the same computer, and the identity proof over a real
//! socket. They need `127.x.0.n` loopback aliases (Linux and Windows have them; on macOS
//! `sudo ifconfig lo0 alias 127.1.0.2`) and skip otherwise.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use ed25519_dalek::{Signature, VerifyingKey};
use kybern_client::{Client, Endpoint};
use kybern_protocol::NetworkClientGrantParams;
use kybern_protocol::methods::{DaemonInfoMethod, Empty, NetworkClientGrant, NetworkStatusMethod};
use serde_json::{Value, json};

static NEXT_BLOCK: AtomicU32 = AtomicU32::new(0);

/// The second octet of a loopback block no other test holds, and the listener on
/// `127.<block>.0.254` that holds it until dropped: nextest runs every test in its own process, so
/// a counter alone would give each the same block. The same lease as `lease_block` in
/// `src/network/harness.rs`, on blocks 200 and up, which the in-process clusters never use.
fn block() -> (u32, Option<std::net::TcpListener>) {
    const LEASE_PORT: u16 = 4169;
    const BLOCKS: u32 = 55;
    // A different start per process keeps stray daemons from an earlier run apart too.
    let start = std::process::id().wrapping_add(NEXT_BLOCK.fetch_add(1, Ordering::SeqCst));
    for offset in 0..BLOCKS {
        let block = 200 + start.wrapping_add(offset) % BLOCKS;
        match std::net::TcpListener::bind((format!("127.{block}.0.254").as_str(), LEASE_PORT)) {
            Ok(lease) => return (block, Some(lease)),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            // No `.254` alias (macOS): unleased, and skipped below unless `.2` is aliased.
            Err(_) => return (200 + start % BLOCKS, None),
        }
    }
    panic!("every loopback block from 200 is leased by another test")
}

fn aliases_available(block: u32) -> bool {
    std::net::TcpListener::bind((format!("127.{block}.0.2"), 0)).is_ok()
}

/// The `kybernd` binary of this build. Integration tests of the library cannot name it through
/// Cargo, so it is found next to the test executable and built with `cargo build -p kybern` when
/// it is missing.
fn kybernd() -> PathBuf {
    static BUILT: std::sync::Once = std::sync::Once::new();
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap();
    let name = if cfg!(windows) { "kybernd.exe" } else { "kybernd" };
    let path = dir.join(name);
    BUILT.call_once(|| {
        if !path.exists() {
            let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
                .args(["build", "-p", "kybern", "--bin", "kybernd"])
                .status()
                .expect("cargo build -p kybern");
            assert!(status.success(), "could not build kybernd");
        }
    });
    path
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("kybern-net-proc-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture(block: u32, names: &[(&str, u32)], me: &str, path: &Path) {
    let nodes: Vec<Value> = names
        .iter()
        .enumerate()
        .map(|(index, (name, host))| {
            json!({
                "name": name, "node_id": format!("n{name}"), "ip": format!("100.64.{block}.{}", index + 1),
                "listen": format!("127.{block}.0.{host}"), "login": "owner@example.com", "user_id": 1
            })
        })
        .collect();
    std::fs::write(path, serde_json::to_vec(&json!({ "self": me, "tailnet": "fake.ts.net", "nodes": nodes })).unwrap()).unwrap();
}

struct Daemon {
    dir: PathBuf,
    log: PathBuf,
    process: Child,
}

impl Daemon {
    /// Start `kybernd` on a loopback primary with the fake tailnet fixture and the network turned on
    /// (it is off by default). `network_port` is written to `settings.json` when given.
    fn start(scratch: &Scratch, name: &str, fixture: &Path, network_port: Option<u16>) -> Self {
        let dir = scratch.0.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let network = match network_port {
            Some(port) => json!({ "enabled": true, "port": port }),
            None => json!({ "enabled": true }),
        };
        std::fs::write(dir.join("settings.json"), json!({ "network": network }).to_string()).unwrap();
        let log = scratch.0.join(format!("{name}.log"));
        let file = std::fs::File::create(&log).unwrap();
        let process = Command::new(kybernd())
            .arg("--data-dir")
            .arg(&dir)
            .args(["--port", "0", "--bind", "127.0.0.1"])
            .env("KYBERN_TAILSCALE_FAKE", fixture)
            .env("RUST_LOG", "info,kybern=debug")
            // Plain text, so a needle naming a field (`transport="tcp"`) matches between the color codes.
            .env("NO_COLOR", "1")
            .env_remove("KYBERN_URL")
            .env_remove("KYBERN_TOKEN")
            .env_remove("KYBERN_ADVERTISE_URL")
            .stdin(Stdio::null())
            .stdout(file.try_clone().unwrap())
            .stderr(file)
            .spawn()
            .unwrap();
        Self { dir, log, process }
    }

    async fn client(&mut self) -> Client {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.process.try_wait().unwrap() {
                panic!("kybernd exited {status}: {}", std::fs::read_to_string(&self.log).unwrap_or_default());
            }
            if let (Ok(port), Ok(token)) =
                (std::fs::read_to_string(self.dir.join("daemon.port")), std::fs::read_to_string(self.dir.join("daemon.token")))
            {
                let endpoint = Endpoint { url: format!("ws://127.0.0.1:{}/ws", port.trim()), token: token.trim().to_owned() };
                if let Ok(client) = Client::connect(&endpoint).await {
                    return client;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "kybernd did not become ready: {}",
                std::fs::read_to_string(&self.log).unwrap_or_default()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    async fn wait_for_log(&self, needle: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !self.log().contains(needle) {
            assert!(tokio::time::Instant::now() < deadline, "log never said {needle:?}:\n{}", self.log());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// `GET /.well-known/kybern/machine` at `base` and check its proof against the key it names.
async fn well_known(base: &str) -> Value {
    let nonce = B64URL.encode(uuid::Uuid::new_v4().as_bytes());
    let authority = base.trim_start_matches("http://");
    let response = reqwest::Client::new().get(format!("{base}/.well-known/kybern/machine?nonce={nonce}")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let answer: Value = response.json().await.unwrap();
    let message = format!("kybern-wk-v1\0{nonce}{}{}{authority}", answer["machine_id"].as_str().unwrap(), answer["at"].as_str().unwrap());
    let key: [u8; 32] = B64.decode(answer["signing_key"].as_str().unwrap()).unwrap().try_into().unwrap();
    let signature: [u8; 64] = B64.decode(answer["proof"].as_str().unwrap()).unwrap().try_into().unwrap();
    VerifyingKey::from_bytes(&key)
        .unwrap()
        .verify_strict(message.as_bytes(), &Signature::from_bytes(&signature))
        .expect("the proof verifies against the daemon's own key");
    answer
}

#[tokio::test]
async fn a_started_daemon_lists_the_network_features_and_proves_its_identity_on_the_tailnet_address() {
    let (block, _lease) = block();
    if !aliases_available(block) {
        eprintln!("skipped: no 127.{block}.0.2 loopback alias");
        return;
    }
    let scratch = Scratch::new("identity");
    let path = scratch.0.join("fixture.json");
    fixture(block, &[("a", 2)], "a", &path);
    // A daemon on the default network port, 4173 on its Tailscale address.
    let mut a = Daemon::start(&scratch, "a", &path, None);
    let client = a.client().await;
    let info = client.call::<DaemonInfoMethod>(Default::default()).await.unwrap();
    assert!(info.features.iter().any(|feature| feature == "network.v1"), "{:?}", info.features);
    assert!(info.features.iter().any(|feature| feature == "relay.v1"), "{:?}", info.features);
    assert_eq!(info.machine_id, info.environment_id);
    a.wait_for_log("listening: machine network").await;

    let answer = well_known(&format!("http://127.{block}.0.2:4173")).await;
    assert_eq!(answer["machine_id"], info.environment_id);
    assert_eq!(answer["network_protocol"], 1);
    assert!(answer["features"].as_array().unwrap().iter().any(|feature| feature == "network.v1"));
    // The key in the answer is the key the daemon recorded in its own store, not one made up per request.
    let again = well_known(&format!("http://127.{block}.0.2:4173")).await;
    assert_eq!(again["signing_key"], answer["signing_key"]);
    assert!(a.dir.join("network").join("machine.key").exists());
}

#[tokio::test]
async fn the_network_listener_coexists_with_a_loopback_daemon_on_the_same_port_number() {
    let (block, _lease) = block();
    if !aliases_available(block) {
        eprintln!("skipped: no 127.{block}.0.2 loopback alias");
        return;
    }
    // The user's own daemon holds 127.0.0.1:<port>; the network port is the same number on the
    // Tailscale address. They are different sockets and must not collide or shadow each other.
    let loopback = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = loopback.local_addr().unwrap().port();
    let scratch = Scratch::new("coexist");
    let path = scratch.0.join("fixture.json");
    fixture(block, &[("a", 2)], "a", &path);
    // A stand-in for the user's daemon on 127.0.0.1:port.
    let legacy = std::thread::spawn(move || {
        use std::io::{Read, Write};
        for stream in loopback.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buffer = [0u8; 256];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 6\r\nconnection: close\r\n\r\nlegacy");
        }
    });
    let mut a = Daemon::start(&scratch, "a", &path, Some(port));
    let _client = a.client().await;
    a.wait_for_log("listening: machine network").await;
    assert!(a.log().contains(&format!("127.{block}.0.2:{port}")), "{}", a.log());
    let answer = well_known(&format!("http://127.{block}.0.2:{port}")).await;
    assert!(answer["machine_id"].is_string());
    // The loopback socket still answers as it did.
    let body = reqwest::get(format!("http://127.0.0.1:{port}/")).await.unwrap().text().await.unwrap();
    assert_eq!(body, "legacy");
    drop(legacy);
}

#[tokio::test]
async fn a_second_daemon_on_the_same_computer_says_why_it_is_not_on_the_network() {
    let (block, _lease) = block();
    if !aliases_available(block) {
        eprintln!("skipped: no 127.{block}.0.2 loopback alias");
        return;
    }
    let scratch = Scratch::new("shared-host");
    let first_fixture = scratch.0.join("first.json");
    let second_fixture = scratch.0.join("second.json");
    // Two daemons, one Tailscale node: both fixtures name the same listen address.
    fixture(block, &[("a", 2), ("b", 2)], "a", &first_fixture);
    fixture(block, &[("a", 2), ("b", 2)], "b", &second_fixture);
    let mut first = Daemon::start(&scratch, "first", &first_fixture, None);
    let first_client = first.client().await;
    first.wait_for_log("listening: machine network").await;
    let first_id = first_client.call::<DaemonInfoMethod>(Default::default()).await.unwrap().environment_id;

    let mut second = Daemon::start(&scratch, "second", &second_fixture, None);
    let _second_client = second.client().await;
    second.wait_for_log("Another Kybern daemon on this computer").await;
    assert!(second.log().contains(&first_id), "the message names the daemon that holds the address:\n{}", second.log());
    // The first one is unharmed and still answers as itself; the second keeps running without a listener.
    let answer = well_known(&format!("http://127.{block}.0.2:4173")).await;
    assert_eq!(answer["machine_id"], first_id);
}

/// The `kybern` command-line binary of this build, built like [`kybernd`].
fn kybern_cli() -> PathBuf {
    static BUILT: std::sync::Once = std::sync::Once::new();
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap();
    let path = dir.join(if cfg!(windows) { "kybern.exe" } else { "kybern" });
    BUILT.call_once(|| {
        if !path.exists() {
            let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
                .args(["build", "-p", "kybern", "--bin", "kybern"])
                .status()
                .expect("cargo build -p kybern");
            assert!(status.success(), "could not build kybern");
        }
    });
    path
}

/// Run `kybern --data-dir <dir> machines <args>` and return (success, stdout).
fn machines_cli(dir: &Path, args: &[&str]) -> (bool, String) {
    let output = Command::new(kybern_cli())
        .arg("--data-dir")
        .arg(dir)
        .arg("machines")
        .args(args)
        .env_remove("KYBERN_URL")
        .env_remove("KYBERN_TOKEN")
        .output()
        .unwrap();
    (output.status.success(), format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr)))
}

#[tokio::test]
async fn two_started_daemons_of_one_user_join_and_the_cli_lists_checks_and_removes_them() {
    let (block, _lease) = block();
    if !aliases_available(block) {
        eprintln!("skipped: no 127.{block}.0.2 loopback alias");
        return;
    }
    let scratch = Scratch::new("join");
    let path_a = scratch.0.join("a.json");
    let path_b = scratch.0.join("b.json");
    fixture(block, &[("a", 2), ("b", 3)], "a", &path_a);
    fixture(block, &[("a", 2), ("b", 3)], "b", &path_b);
    let mut a = Daemon::start(&scratch, "a", &path_a, None);
    let client_a = a.client().await;
    a.wait_for_log("listening: machine network").await;
    // The second daemon starts later; the first has to notice it without a long wait.
    let mut b = Daemon::start(&scratch, "b", &path_b, None);
    let client_b = b.client().await;
    b.wait_for_log("listening: machine network").await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let on_a = client_a.call::<NetworkStatusMethod>(Empty {}).await.unwrap();
        let on_b = client_b.call::<NetworkStatusMethod>(Empty {}).await.unwrap();
        if on_a.members.len() == 2
            && on_b.members.len() == 2
            && on_a.members.iter().all(|card| card.status == kybern_protocol::MachineStatus::Online)
        {
            assert_eq!(on_a.network_id, on_b.network_id);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the two daemons never joined:\na: {}\nb: {}",
            std::fs::read_to_string(&a.log).unwrap_or_default(),
            std::fs::read_to_string(&b.log).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // The command line shows the same, and its doctor finds nothing broken.
    let (ok, list) = machines_cli(&a.dir, &["list"]);
    assert!(ok, "{list}");
    assert!(list.contains("2 machines"), "{list}");
    let (ok, doctor) = machines_cli(&a.dir, &["doctor"]);
    assert!(ok, "the doctor reports no failure:\n{doctor}");
    assert!(doctor.contains("Tailscale is running") && doctor.contains("connected for"), "{doctor}");
    let (ok, status) = machines_cli(&a.dir, &["status"]);
    assert!(ok && status.contains("Tailscale:"), "{status}");

    // One pairing, a token for the other machine: it connects to b directly.
    let id_b = client_b.call::<DaemonInfoMethod>(Default::default()).await.unwrap().environment_id;
    let grant = client_a
        .call::<NetworkClientGrant>(NetworkClientGrantParams { machine_id: id_b.clone(), device_id: "app-1".into() })
        .await
        .unwrap();
    let port_b = std::fs::read_to_string(b.dir.join("daemon.port")).unwrap();
    let direct = Client::connect(&Endpoint { url: format!("ws://127.0.0.1:{}/ws", port_b.trim()), token: grant.token }).await.unwrap();
    assert_eq!(direct.call::<DaemonInfoMethod>(Default::default()).await.unwrap().environment_id, id_b);

    // A real dev server: raw TCP carries both HTTP and a WebSocket upgrade through the new path.
    {
        use axum::{Router, extract::ws::WebSocketUpgrade, routing::get};
        use futures::{SinkExt, StreamExt};
        use kybern_protocol::{
            PortsForwardParams, PortsStopParams,
            methods::{PortsForward, PortsStop},
        };
        use tokio_tungstenite::tungstenite::Message;
        let server = Router::new().route("/", get(|| async { "process forward" })).route(
            "/hmr",
            get(|ws: WebSocketUpgrade| async {
                ws.on_upgrade(|mut socket| async move {
                    while let Some(Ok(message)) = socket.recv().await {
                        if socket.send(message).await.is_err() {
                            break;
                        }
                    }
                })
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let dev = tokio::spawn(async move {
            axum::serve(listener, server).await.unwrap();
        });
        let forwarded = client_a
            .call::<PortsForward>(PortsForwardParams { machine: id_b.clone(), remote_port: port, local_port: None, https: false })
            .await
            .unwrap();
        assert!(forwarded.local_port > port, "the server occupies the same local port");
        let page = reqwest::get(format!("http://127.0.0.1:{}/", forwarded.local_port)).await.unwrap().text().await.unwrap();
        assert_eq!(page, "process forward");
        // Both daemons speak ports.direct.v2: the forward keeps ready sockets.
        a.wait_for_log("transport=\"tcp-ready\"").await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}/hmr", forwarded.local_port)).await.unwrap();
        ws.send(Message::Text("update".into())).await.unwrap();
        assert_eq!(ws.next().await.unwrap().unwrap(), Message::Text("update".into()));
        client_a.call::<PortsStop>(PortsStopParams { id: forwarded.id }).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(3), ws.next()).await.is_ok(), "stop must end the active socket");
        dev.abort();
    }

    // Removal from the command line reaches the other daemon.
    let (ok, removed) = machines_cli(&a.dir, &["remove", &id_b, "--yes"]);
    assert!(ok, "{removed}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let on_b = client_b.call::<NetworkStatusMethod>(Empty {}).await.unwrap();
        if on_b.removed {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "b never learned it was removed:\n{}",
            std::fs::read_to_string(&b.log).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let after = client_a.call::<NetworkStatusMethod>(Empty {}).await.unwrap();
    assert_eq!(after.members.len(), 1);
}
