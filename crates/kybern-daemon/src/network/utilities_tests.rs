//! The machine utilities (ADE-86 M4, design §8, §11.2 U1/U3/U4) on the multi-daemon harness: files that
//! resume and stay inside their root, a dev server previewed through the relay with its WebSocket, ports
//! forwarded between machines, and a terminal on another machine. Real daemons, real sockets.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message as AxumMessage, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use futures::{SinkExt, StreamExt};
use kybern_client::{Client, Endpoint};
use kybern_protocol::methods::*;
use kybern_protocol::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use uuid::Uuid;

use super::harness::*;
use super::testutil::TempDir;

async fn pair() -> Option<(Cluster, Node, Node)> {
    let cluster = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b")])?;
    let a = cluster.start("a", true).await;
    let b = cluster.start("b", true).await;
    cluster.form(&a, &[&b], JoinVia::Auto).await;
    eventually("a and b linked", WAIT, || linked(&a, &b)).await;
    Some((cluster, a, b))
}

async fn admin(node: &Node) -> Client {
    Client::connect(&Endpoint { url: node.ws_url(), token: node.device_token(&crate::access::paired_scopes()) }).await.unwrap()
}

async fn add_project(node: &Node, path: &std::path::Path, name: &str) -> Project {
    let client = admin(node).await;
    client
        .call::<ProjectsAdd>(ProjectsAddParams {
            path: path.to_string_lossy().into_owned(),
            name: Some(name.into()),
            space_id: None,
            kind: None,
            members: None,
        })
        .await
        .unwrap()
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len as u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect()
}

fn canonical(dir: &TempDir) -> std::path::PathBuf {
    std::fs::canonicalize(dir.path()).unwrap()
}

// ---- U1: files ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_resumes_by_offset_after_a_disconnect_and_lands_only_once_every_checksum_matches() {
    let Some((_cluster, a, b)) = pair().await else { return };
    let work = TempDir::new("push-resume");
    let root = canonical(&work);
    let project_dir = root.join("dest");
    std::fs::create_dir_all(&project_dir).unwrap();
    add_project(&b, &project_dir, "dest").await;

    let bytes = pattern(9 * 1024 * 1024 + 123);
    let entry = PushEntry { executable: false, path: "big.bin".into(), size: bytes.len() as u64, sha256: sha(&bytes) };
    let transfer_id = Uuid::now_v7();
    let begin = PeerFilesBeginParams {
        transfer_id,
        name: "big.bin".into(),
        is_folder: false,
        dest: PushDest::Project { project: "dest".into(), subdir: Some("incoming/here".into()) },
        entries: vec![entry.clone()],
    };
    let link = a.state.network.peers.link(&b.id()).unwrap();
    let began = link.call::<PeerFilesBegin>(begin.clone()).await.unwrap();
    assert_eq!(began.offsets, [0]);
    let chunk = |offset: usize, len: usize| PeerFilesChunkParams {
        transfer_id,
        index: 0,
        offset: offset as u64,
        data: B64.encode(&bytes[offset..offset + len]),
    };
    let first = 4 * 1024 * 1024;
    assert_eq!(link.call::<PeerFilesChunk>(chunk(0, first)).await.unwrap().offset, first as u64);

    // The session drops between chunks; a new one resumes where the target left off.
    link.close();
    eventually("a fresh link", WAIT, || a.state.network.peers.link(&b.id()).is_some_and(|fresh| !Arc::ptr_eq(&fresh, &link))).await;
    let link = a.state.network.peers.link(&b.id()).unwrap();
    let again = link.call::<PeerFilesBegin>(begin.clone()).await.unwrap();
    assert_eq!(again.offsets, [first as u64], "the staged bytes survive the disconnect");
    // A chunk from the wrong offset is refused with the offset to use.
    let wrong = link.call::<PeerFilesChunk>(chunk(0, 100)).await.unwrap_err();
    assert_eq!(wrong.code, codes::CONFLICT);
    assert_eq!(wrong.data.as_ref().and_then(|data| data["offset"].as_u64()), Some(first as u64));
    // Nothing is in the project yet.
    assert!(!project_dir.join("incoming").exists(), "a half-sent file never appears in a project");
    let premature = link.call::<PeerFilesFinish>(PeerFilesTransferParams { transfer_id }).await.unwrap_err();
    assert_eq!(premature.code, codes::CONFLICT);

    assert_eq!(link.call::<PeerFilesChunk>(chunk(first, first)).await.unwrap().offset, (2 * first) as u64);
    let rest = bytes.len() - 2 * first;
    assert_eq!(link.call::<PeerFilesChunk>(chunk(2 * first, rest)).await.unwrap().offset, bytes.len() as u64);
    let done = link.call::<PeerFilesFinish>(PeerFilesTransferParams { transfer_id }).await.unwrap();
    let landed = project_dir.join("incoming/here/big.bin");
    assert_eq!(std::path::PathBuf::from(&done.dest_path), landed);
    assert_eq!(std::fs::read(&landed).unwrap(), bytes);
    assert_eq!((done.files, done.bytes), (1, bytes.len() as u64));
    // Asking again after it finished answers the same and sends nothing.
    let repeat = link.call::<PeerFilesBegin>(begin).await.unwrap();
    assert!(repeat.complete && repeat.dest_path.as_deref() == Some(done.dest_path.as_str()));
    assert!(std::fs::read_dir(project_dir.join("incoming/here")).unwrap().count() == 1, "no staging leftovers in the project");

    // A file that arrives damaged is not placed.
    let bad_id = Uuid::now_v7();
    let bad = PeerFilesBeginParams {
        transfer_id: bad_id,
        name: "bad.txt".into(),
        is_folder: false,
        dest: PushDest::Project { project: "dest".into(), subdir: None },
        entries: vec![PushEntry { executable: false, path: "bad.txt".into(), size: 5, sha256: sha(b"right") }],
    };
    link.call::<PeerFilesBegin>(bad).await.unwrap();
    link.call::<PeerFilesChunk>(PeerFilesChunkParams { transfer_id: bad_id, index: 0, offset: 0, data: B64.encode(b"wrong") })
        .await
        .unwrap();
    let damaged = link.call::<PeerFilesFinish>(PeerFilesTransferParams { transfer_id: bad_id }).await.unwrap_err();
    assert_eq!(damaged.code, codes::CONFLICT, "{damaged:?}");
    assert!(!project_dir.join("bad.txt").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_destination_root_is_enforced_and_nothing_is_overwritten() {
    let Some((_cluster, a, b)) = pair().await else { return };
    let work = TempDir::new("push-root");
    let root = canonical(&work);
    let project_dir = root.join("dest");
    let outside = root.join("outside");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    add_project(&b, &project_dir, "dest").await;
    let link = a.state.network.peers.link(&b.id()).unwrap();
    let begin = |dest: PushDest, entries: Vec<PushEntry>, is_folder: bool, name: &str| PeerFilesBeginParams {
        transfer_id: Uuid::now_v7(),
        name: name.into(),
        is_folder,
        dest,
        entries,
    };
    let one = |path: &str| PushEntry { executable: false, path: path.into(), size: 1, sha256: sha(b"x") };
    let project = |subdir: Option<&str>| PushDest::Project { project: "dest".into(), subdir: subdir.map(str::to_string) };

    // Climbing out, absolute paths, .git, credential folders and env files are all refused.
    for subdir in ["../outside", "/etc", "a/../../outside", ".git/hooks", ".ssh"] {
        let refused = link.call::<PeerFilesBegin>(begin(project(Some(subdir)), vec![one("a.txt")], false, "a.txt")).await.unwrap_err();
        assert!(matches!(refused.code, codes::INVALID_PARAMS | codes::FORBIDDEN), "{subdir}: {refused:?}");
    }
    for path in ["../escape.txt", "/etc/passwd", ".git/config", ".env", "ok/../../x"] {
        let refused = link.call::<PeerFilesBegin>(begin(project(None), vec![one(path)], true, "folder")).await.unwrap_err();
        assert!(matches!(refused.code, codes::INVALID_PARAMS | codes::FORBIDDEN), "{path}: {refused:?}");
    }
    let unknown = link
        .call::<PeerFilesBegin>(begin(PushDest::Project { project: "nope".into(), subdir: None }, vec![one("a.txt")], false, "a.txt"))
        .await
        .unwrap_err();
    assert_eq!(unknown.code, codes::NOT_FOUND);

    // A symlink inside the project that points outside it cannot be a destination.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, project_dir.join("sneaky")).unwrap();
        let refused =
            link.call::<PeerFilesBegin>(begin(project(Some("sneaky/deeper")), vec![one("a.txt")], false, "a.txt")).await.unwrap_err();
        assert_eq!(refused.code, codes::FORBIDDEN, "{refused:?}");
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0, "nothing was written through the link");
    }

    // A name that is taken becomes `name (2)`; the original is untouched.
    std::fs::write(project_dir.join("notes.txt"), "mine").unwrap();
    let id = Uuid::now_v7();
    let announce = PeerFilesBeginParams {
        transfer_id: id,
        ..begin(
            project(None),
            vec![PushEntry { executable: false, path: "notes.txt".into(), size: 5, sha256: sha(b"yours") }],
            false,
            "notes.txt",
        )
    };
    link.call::<PeerFilesBegin>(announce).await.unwrap();
    link.call::<PeerFilesChunk>(PeerFilesChunkParams { transfer_id: id, index: 0, offset: 0, data: B64.encode(b"yours") }).await.unwrap();
    let done = link.call::<PeerFilesFinish>(PeerFilesTransferParams { transfer_id: id }).await.unwrap();
    assert_eq!(std::fs::read_to_string(project_dir.join("notes.txt")).unwrap(), "mine");
    assert_eq!(std::fs::read_to_string(project_dir.join("notes (2).txt")).unwrap(), "yours");
    assert!(done.dest_path.ends_with("notes (2).txt"));

    // Another machine's staged send cannot be finished by a third caller: the manifest names its sender.
    // (With two machines the check is the same code path: the id belongs to the machine that began it.)
    let missing = link.call::<PeerFilesFinish>(PeerFilesTransferParams { transfer_id: Uuid::now_v7() }).await.unwrap_err();
    assert_eq!(missing.code, codes::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_is_capped_at_two_gib_on_both_machines() {
    let Some((_cluster, a, b)) = pair().await else { return };
    let work = TempDir::new("push-cap");
    let root = canonical(&work);
    let project_dir = root.join("src");
    std::fs::create_dir_all(&project_dir).unwrap();
    // A sparse file over the cap costs no disk.
    let huge = std::fs::File::create(project_dir.join("huge.bin")).unwrap();
    huge.set_len(MAX_PUSH_BYTES + 1).unwrap();
    let project = add_project(&a, &project_dir, "src").await;
    let client = admin(&a).await;
    let refused = client
        .call::<FilesSend>(FilesSendParams { project_id: project.id, path: "huge.bin".into(), machine: b.id(), dest: None })
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("2 GiB"), "{refused}");
    // A folder is counted as a whole.
    std::fs::remove_file(project_dir.join("huge.bin")).unwrap();
    std::fs::create_dir(project_dir.join("many")).unwrap();
    for name in ["one", "two"] {
        let file = std::fs::File::create(project_dir.join("many").join(name)).unwrap();
        file.set_len(MAX_PUSH_BYTES / 2 + 1).unwrap();
    }
    let refused = client
        .call::<FilesSend>(FilesSendParams { project_id: project.id, path: "many".into(), machine: b.id(), dest: None })
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("2 GiB"), "{refused}");

    // The target enforces it too, whatever the sender claims.
    let link = a.state.network.peers.link(&b.id()).unwrap();
    let claim = |size: u64| PeerFilesBeginParams {
        transfer_id: Uuid::now_v7(),
        name: "x.bin".into(),
        is_folder: false,
        dest: PushDest::Downloads,
        entries: vec![PushEntry { executable: false, path: "x.bin".into(), size, sha256: sha(b"x") }],
    };
    assert_eq!(link.call::<PeerFilesBegin>(claim(MAX_PUSH_BYTES + 1)).await.unwrap_err().code, codes::INVALID_PARAMS);
    // And a chunk cannot run past the size it announced.
    let work_b = TempDir::new("push-cap-dest");
    let dest = canonical(&work_b).join("p");
    std::fs::create_dir_all(&dest).unwrap();
    add_project(&b, &dest, "p").await;
    let id = Uuid::now_v7();
    let small = PeerFilesBeginParams {
        transfer_id: id,
        name: "s.bin".into(),
        is_folder: false,
        dest: PushDest::Project { project: "p".into(), subdir: None },
        entries: vec![PushEntry { executable: false, path: "s.bin".into(), size: 3, sha256: sha(b"abc") }],
    };
    link.call::<PeerFilesBegin>(small).await.unwrap();
    let over = link.call::<PeerFilesChunk>(PeerFilesChunkParams { transfer_id: id, index: 0, offset: 0, data: B64.encode(b"abcd") }).await;
    assert_eq!(over.unwrap_err().code, codes::INVALID_PARAMS);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_sent_to_an_offline_machine_waits_in_the_outbox_and_lands_when_it_returns() {
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b")]) else { return };
    let a = cluster.start("a", true).await;
    // Register b and its destination without opening peer sessions. Stopping an already linked
    // node leaves in-flight handshakes, so a transient Sending state while they close is valid;
    // this scenario exercises an offline destination, not shutdown detection.
    let mut b = cluster.start("b", false).await;
    cluster.form(&a, &[&b], JoinVia::Auto).await;
    let work = TempDir::new("push-offline");
    let root = canonical(&work);
    let src = root.join("src");
    let dst = root.join("dst");
    std::fs::create_dir_all(src.join("app/sub")).unwrap();
    std::fs::create_dir_all(src.join("app/.git")).unwrap();
    std::fs::create_dir_all(&dst).unwrap();
    let body = pattern(5 * 1024 * 1024 + 7);
    std::fs::write(src.join("app/data.bin"), &body).unwrap();
    std::fs::write(src.join("app/sub/readme.md"), "hello").unwrap();
    std::fs::write(src.join("app/empty.txt"), "").unwrap();
    std::fs::write(src.join("app/.git/config"), "[core]").unwrap();
    std::fs::write(src.join("app/.env"), "TOKEN=1").unwrap();
    let project = add_project(&a, &src, "src").await;
    add_project(&b, &dst, "dst").await;
    let b_id = b.id();

    // b is registered but offline; no session has ever opened to it.
    b.stop();
    assert!(a.state.network.peers.link(&b_id).is_none());
    let client = admin(&a).await;
    let sent = client
        .call::<FilesSend>(FilesSendParams {
            project_id: project.id,
            path: "app".into(),
            machine: b_id.clone(),
            dest: Some(PushDest::Project { project: "dst".into(), subdir: Some("received".into()) }),
        })
        .await
        .unwrap();
    assert_eq!((sent.files, sent.bytes), (3, body.len() as u64 + 5), "the folder's files, minus .git and .env");
    assert_eq!(sent.item.kind, OutboxKind::FilesPush);
    assert!(sent.skipped >= 2);
    // Held back, not delivered: a machine that is away is not attempted, and the item stays queued.
    let state_of = || a.state.store.outbox_get(sent.item.id).unwrap().unwrap().state;
    eventually("the item waits for b", WAIT, || state_of() == OutboxState::Queued).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(state_of(), OutboxState::Queued, "nothing is delivered to an offline machine");
    assert_eq!(a.state.store.outbox_get(sent.item.id).unwrap().unwrap().attempts, 0, "no session means no delivery attempt");
    assert!(!dst.join("received").exists(), "no files arrive before b comes online");

    // b comes back (same data directory): the item is delivered on its own.
    drop(b);
    let b = cluster.start("b", true).await;
    let mut received = b.state.network.subscribe_utility();
    eventually("the item is delivered", Duration::from_secs(40), || {
        a.state.store.outbox_get(sent.item.id).unwrap().unwrap().state == OutboxState::Delivered
    })
    .await;
    let landed = dst.join("received/app");
    assert_eq!(std::fs::read(landed.join("data.bin")).unwrap(), body);
    assert_eq!(std::fs::read_to_string(landed.join("sub/readme.md")).unwrap(), "hello");
    assert_eq!(std::fs::read(landed.join("empty.txt")).unwrap().len(), 0);
    assert!(!landed.join(".git").exists() && !landed.join(".env").exists(), "secrets and .git never travel");
    // The receiving machine's clients are told.
    let mut notice = None;
    while let Ok((method, params)) = received.try_recv() {
        if method == FILES_RECEIVED_NOTIFICATION {
            notice = Some(serde_json::from_value::<FilesReceived>(params).unwrap());
        }
    }
    let notice = notice.expect("a files.received notification");
    assert_eq!((notice.files, notice.name.as_str(), notice.from_machine.as_str()), (3, "app", a.id().as_str()));
    assert!(!notice.from_name.is_empty());
    assert!(std::fs::read_dir(&dst).unwrap().flatten().all(|entry| !entry.file_name().to_string_lossy().starts_with(".kybern-recv")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_machine_that_turned_files_off_refuses_a_send() {
    let Some((_cluster, a, b)) = pair().await else { return };
    let work = TempDir::new("push-scope");
    let root = canonical(&work);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/f.txt"), "hello").unwrap();
    let project = add_project(&a, &root.join("src"), "src").await;
    b.membership().set(&format!("allow.{}.files", b.id()), json!(false)).unwrap();
    let link = a.state.network.peers.link(&b.id()).unwrap();
    let refused = link
        .call::<PeerFilesBegin>(PeerFilesBeginParams {
            transfer_id: Uuid::now_v7(),
            name: "f.txt".into(),
            is_folder: false,
            dest: PushDest::Downloads,
            entries: vec![PushEntry { executable: false, path: "f.txt".into(), size: 5, sha256: sha(b"hello") }],
        })
        .await
        .unwrap_err();
    assert_eq!(refused.code, codes::FORBIDDEN);
    // Through the outbox the same refusal ends the item with a message that says what to turn on.
    let client = admin(&a).await;
    let sent = client
        .call::<FilesSend>(FilesSendParams { project_id: project.id, path: "f.txt".into(), machine: b.id(), dest: None })
        .await
        .unwrap();
    eventually("the send fails with a reason", WAIT, || {
        a.state.store.outbox_get(sent.item.id).unwrap().unwrap().state == OutboxState::Failed
    })
    .await;
    let failed = a.state.store.outbox_get(sent.item.id).unwrap().unwrap();
    assert!(failed.last_error.unwrap().contains("Files"), "names the setting to turn on");
}

// ---- U3: ports and previews ----

/// A dev server: pages at `/`, a Vite-style hot-reload socket at `/hmr` that echoes, with the
/// `vite-hmr` subprotocol, `/headers`, which answers with the request headers it got, and `/set-cookie`.
async fn dev_server() -> u16 {
    let app = Router::new()
        .route("/", get(|| async { "<html>dev</html>" }))
        .route(
            "/headers",
            get(|headers: axum::http::HeaderMap| async move {
                let seen: Vec<(String, String)> = headers
                    .iter()
                    .map(|(name, value)| (name.as_str().to_string(), value.to_str().unwrap_or_default().to_string()))
                    .collect();
                axum::Json(seen)
            }),
        )
        .route(
            "/set-cookie",
            get(|| async {
                (
                    axum::response::AppendHeaders([
                        (axum::http::header::SET_COOKIE, "sid=1; Path=/; HttpOnly"),
                        (axum::http::header::SET_COOKIE, "pref=2; Domain=localhost; Path=/api; SameSite=Lax"),
                    ]),
                    "ok",
                )
            }),
        )
        .route(
            "/hmr",
            get(|ws: WebSocketUpgrade| async move {
                ws.protocols(["vite-hmr"])
                    .on_upgrade(|mut socket| async move {
                        while let Some(Ok(message)) = socket.recv().await {
                            if let AxumMessage::Text(text) = message {
                                let _ = socket.send(AxumMessage::Text(format!("echo:{text}").into())).await;
                            }
                        }
                    })
                    .into_response()
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preview_of_a_dev_server_on_another_machine_loads_through_the_relay_with_its_websocket() {
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b"), NodeSpec::owner("c")]) else { return };
    let a = cluster.start("a", true).await;
    let b = cluster.start("b", true).await;
    let c = cluster.start("c", true).await;
    cluster.form(&a, &[&b, &c], JoinVia::Auto).await;
    eventually("linked", WAIT, || linked(&a, &b)).await;
    // The dev server runs on b. The ticket is what `previews.open` would hand b's client.
    let port = dev_server().await;
    let ticket = b.state.previews.mint(crate::previews::tickets::TicketKind::Proxy { port }, Uuid::now_v7(), None);
    // The client of `a` reaches b through a: relay credential for a, no credential needed for the ticket route.
    let token_at_a = a.device_token(&Scope::CLIENT);
    let base = format!("{}/relay/{}/http/preview-proxy/{ticket}", a.http_url(), b.id());
    let page = reqwest::Client::new()
        .get(format!("{base}/"))
        .header(super::relay::RELAY_AUTH_HEADER, format!("Bearer {token_at_a}"))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    assert_eq!(page.text().await.unwrap(), "<html>dev</html>");

    // Hot reload: the page's WebSocket goes through the same route, subprotocol and all.
    let url = format!("ws://{}/relay/{}/http/preview-proxy/{ticket}/hmr", a.primary, b.id());
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert(super::relay::RELAY_AUTH_HEADER, format!("Bearer {token_at_a}").parse().unwrap());
    request.headers_mut().insert("sec-websocket-protocol", "vite-hmr".parse().unwrap());
    let (mut socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(response.headers().get("sec-websocket-protocol").map(|value| value.to_str().unwrap()), Some("vite-hmr"));
    for round in 0..3 {
        socket.send(Message::Text(format!("update {round}").into())).await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(10), socket.next()).await.expect("no reply").unwrap().unwrap();
        assert_eq!(reply, Message::Text(format!("echo:update {round}").into()));
    }
    socket.close(None).await.unwrap();

    // A ticket that b does not know, and a relay credential that a does not know, both fail.
    let bad = reqwest::Client::new()
        .get(format!("{}/relay/{}/http/preview-proxy/nope/", a.http_url(), b.id()))
        .header(super::relay::RELAY_AUTH_HEADER, format!("Bearer {token_at_a}"))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 404);
    let anonymous = reqwest::Client::new().get(format!("{base}/")).send().await.unwrap();
    assert_eq!(anonymous.status(), 401);
    eventually("streams are released", WAIT, || a.state.network.relays.open_streams() == 0 && b.state.network.relays.open_streams() == 0)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_page_ticket_loads_a_relayed_preview_without_the_relay_header_and_stays_in_its_page() {
    let Some((_cluster, a, b)) = pair().await else { return };
    let port = dev_server().await;
    let ticket = b.state.previews.mint(crate::previews::tickets::TicketKind::Proxy { port }, Uuid::now_v7(), None);
    assert!(a.state.network.features().iter().any(|feature| feature == features::RELAY_PAGE_V1));
    // The phone's client talks to a with its own device token and asks a for a page of b.
    let client = admin(&a).await;
    let page = client
        .call::<NetworkRelayPageOpen>(NetworkRelayPageOpenParams { machine_id: b.id(), path: format!("/preview-proxy/{ticket}/") })
        .await
        .unwrap();
    let root = format!("/relay/{}/page/{}", b.id(), page.ticket);
    assert_eq!(page.path, format!("{root}/preview-proxy/{ticket}/"));
    assert_eq!(page.scope, format!("{root}/preview-proxy/{ticket}/"));

    // No relay header, no Authorization: the ticket in the URL is the capability.
    let http = reqwest::Client::new();
    let loaded = http.get(format!("{}{}", a.http_url(), page.path)).send().await.unwrap();
    assert_eq!(loaded.status(), 200);
    assert_eq!(loaded.text().await.unwrap(), "<html>dev</html>");

    // Kybern credentials (Authorization, the relay header, a `kyb_` cookie), the page headers only the daemons set,
    // and the ticket in the Referer never reach the dev server; the page's own cookie does.
    let token = a.device_token(&Scope::CLIENT);
    assert!(token.starts_with("kyb_"), "{token}");
    let seen: Vec<(String, String)> = http
        .get(format!("{}{root}/preview-proxy/{ticket}/headers", a.http_url()))
        .header("authorization", format!("Bearer {token}"))
        .header(super::relay::RELAY_AUTH_HEADER, format!("Bearer {token}"))
        .header("cookie", format!("kybern={token}; theme=dark"))
        .header("referer", format!("{}{root}/preview-proxy/{ticket}/", a.http_url()))
        .header(super::relay_pages::PREFIX_HEADER, "/relay/x/page/y")
        .header(super::relay_pages::LOCAL_MARK_HEADER, "forged")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let header = |name: &str| seen.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
    assert_eq!(header("authorization"), None, "{seen:?}");
    assert_eq!(header(super::relay::RELAY_AUTH_HEADER), None, "{seen:?}");
    assert_eq!(header(super::relay_pages::PREFIX_HEADER), None, "{seen:?}");
    assert_eq!(header(super::relay_pages::LOCAL_MARK_HEADER), None, "{seen:?}");
    assert_eq!(header("cookie").as_deref(), Some("theme=dark"), "{seen:?}");
    assert!(seen.iter().all(|(_, value)| !value.contains(&token) && !value.contains(&page.ticket)), "{seen:?}");

    // Hot reload: the WebSocket under the subtree opens, subprotocol included.
    let url = format!("ws://{}{root}/preview-proxy/{ticket}/hmr", a.primary);
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert("sec-websocket-protocol", "vite-hmr".parse().unwrap());
    let (mut socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(response.headers().get("sec-websocket-protocol").map(|value| value.to_str().unwrap()), Some("vite-hmr"));
    socket.send(Message::Text("update".into())).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(10), socket.next()).await.expect("no reply").unwrap().unwrap();
    assert_eq!(reply, Message::Text("echo:update".into()));
    socket.close(None).await.unwrap();

    // Outside the page: a longer id, another ticket, other roots, the API, and dot segments. The paths are sent as
    // written (an HTTP client would normalize the dot segments before they left).
    let address = a.primary.to_string();
    for outside in [
        format!("/preview-proxy/{ticket}x/"),
        "/preview-proxy/other/".to_string(),
        format!("/preview-files/{ticket}/"),
        "/ws".to_string(),
        "/api/threads".to_string(),
        format!("/preview-proxy/{ticket}/../other/"),
        format!("/preview-proxy/{ticket}/%2e%2e/other/"),
        format!("/preview-proxy/{ticket}/%2e%2e%2fother/"),
        format!("/preview-proxy/{ticket}/%2E%2E/%2E%2E/preview-proxy/other/"),
        format!("/preview-proxy/{ticket}/..%2f..%2fapi/threads"),
    ] {
        assert_eq!(raw_status(&address, &format!("{root}{outside}")).await, 403, "{outside} is outside the page");
    }
    // Encoded twice, it is one plain segment inside the page: the dev server gets it as written and has no such page.
    let twice = raw_status(&address, &format!("{root}/preview-proxy/{ticket}/%252e%252e%252fother/")).await;
    assert!(twice == 404 || twice == 403, "a doubly encoded dot segment stays inside the page ({twice})");
    let outside = format!("ws://{}{root}/ws", a.primary).into_client_request().unwrap();
    match tokio_tungstenite::connect_async(outside).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => assert_eq!(response.status(), 403),
        other => panic!("expected a refusal, got {other:?}"),
    }

    // An unknown ticket, and a ticket used for another machine.
    let unknown =
        http.get(format!("{}/relay/{}/page/{}/preview-proxy/{ticket}/", a.http_url(), b.id(), Uuid::new_v4())).send().await.unwrap();
    assert_eq!(unknown.status(), 404);
    let wrong_machine =
        http.get(format!("{}/relay/{}/page/{}/preview-proxy/{ticket}/", a.http_url(), Uuid::now_v7(), page.ticket)).send().await.unwrap();
    assert_eq!(wrong_machine.status(), 404);

    // Only the minting device closes a page; closing twice is fine; a closed page is gone.
    let stranger = admin(&a).await;
    stranger.call::<NetworkRelayPageClose>(NetworkRelayPageCloseParams { ticket: page.ticket.clone() }).await.unwrap();
    assert_eq!(http.get(format!("{}{}", a.http_url(), page.path)).send().await.unwrap().status(), 200, "another device cannot close it");
    client.call::<NetworkRelayPageClose>(NetworkRelayPageCloseParams { ticket: page.ticket.clone() }).await.unwrap();
    client.call::<NetworkRelayPageClose>(NetworkRelayPageCloseParams { ticket: page.ticket.clone() }).await.unwrap();
    assert_eq!(http.get(format!("{}{}", a.http_url(), page.path)).send().await.unwrap().status(), 404);
    eventually("streams are released", WAIT, || a.state.network.relays.open_streams() == 0 && b.state.network.relays.open_streams() == 0)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn documents_refuse_subpaths_and_sockets_the_mint_refuses_bad_requests_and_a_revoked_device_loses_its_pages() {
    let Some((_cluster, a, b)) = pair().await else { return };
    let phone = a.device_token(&crate::access::paired_scopes());
    let client = Client::connect(&Endpoint { url: a.ws_url(), token: phone.clone() }).await.unwrap();
    let http = reqwest::Client::new();

    // A visual frame is a document: its own path (with a query) is carried to b, nothing below it, and no sockets.
    let frame = client
        .call::<NetworkRelayPageOpen>(NetworkRelayPageOpenParams { machine_id: b.id(), path: "/visual-frame/nope?theme=dark".into() })
        .await
        .unwrap();
    let root = format!("/relay/{}/page/{}", b.id(), frame.ticket);
    assert_eq!(frame.path, format!("{root}/visual-frame/nope?theme=dark"));
    assert_eq!(frame.scope, format!("{root}/visual-frame/nope"));
    let at_b = http.get(format!("{}{}", a.http_url(), frame.path)).send().await.unwrap();
    assert_ne!(at_b.status(), 403, "the document is carried; b answers for it");
    for outside in ["/visual-frame/nope/sub", "/visual-frame/nopex", "/artifact-preview/nope"] {
        let refused = http.get(format!("{}{root}{outside}", a.http_url())).send().await.unwrap();
        assert_eq!(refused.status(), 403, "{outside}");
    }
    let socket = format!("ws://{}{root}/visual-frame/nope", a.primary).into_client_request().unwrap();
    match tokio_tungstenite::connect_async(socket).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => assert_eq!(response.status(), 403),
        other => panic!("expected a refusal, got {other:?}"),
    }
    let artifact = client
        .call::<NetworkRelayPageOpen>(NetworkRelayPageOpenParams { machine_id: b.id(), path: "/artifact-preview/abc".into() })
        .await
        .unwrap();
    assert_ne!(http.get(format!("{}{}", a.http_url(), artifact.path)).send().await.unwrap().status(), 403);

    // Refusals at the mint.
    let refused = |path: &'static str, machine: String| {
        let client = &client;
        async move {
            client
                .call::<NetworkRelayPageOpen>(NetworkRelayPageOpenParams { machine_id: machine, path: path.into() })
                .await
                .unwrap_err()
                .to_string()
        }
    };
    assert!(
        refused("/preview-proxy/abc/", a.id()).await.ends_with(&format!("(code {})", codes::INVALID_PARAMS)),
        "a machine is no relay to itself"
    );
    assert!(
        refused("/preview-proxy/abc/", Uuid::now_v7().to_string()).await.ends_with(&format!("(code {})", codes::NOT_FOUND)),
        "not a member"
    );
    for bad in ["/ws", "/preview-proxy/abc/../x", "/preview-proxy/abc/%2e%2e/x", "/preview-proxy/a\\b/", "/visual-frame/abc/x"] {
        assert!(refused(bad, b.id()).await.ends_with(&format!("(code {})", codes::INVALID_PARAMS)), "{bad}");
    }

    // Revoking the device ends its pages at once.
    a.state.store.token_revoke(token_id_of(&a, &phone)).unwrap();
    assert_eq!(http.get(format!("{}{}", a.http_url(), artifact.path)).send().await.unwrap().status(), 401);
    assert_eq!(http.get(format!("{}{}", a.http_url(), frame.path)).send().await.unwrap().status(), 401);

    // With the network off on the relay member, a new page is refused too.
    network_set(&a, false).await;
    let off = admin(&a).await;
    let error = off
        .call::<NetworkRelayPageOpen>(NetworkRelayPageOpenParams { machine_id: b.id(), path: "/preview-proxy/abc/".into() })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.ends_with(&format!("(code {})", codes::NOT_FOUND)), "{error}");
}

/// Turn the network on or off the way a user does (its setting), and wait until the node acts on it. Flipping only the
/// in-memory switch would race the worker, which sets it back from the setting on its next pass.
async fn network_set(node: &Node, enabled: bool) {
    node.set_network_enabled(enabled);
    eventually("the network follows its setting", WAIT, || node.state.network.is_enabled() == enabled).await;
}

fn token_id_of(node: &Node, token: &str) -> Uuid {
    crate::auth::authenticate(&node.state.store, token).unwrap().unwrap().token_id
}

/// The status of a `GET` for `path` sent exactly as written, without the normalization an HTTP client applies.
async fn raw_status(address: &str, path: &str) -> u16 {
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
    socket.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(20), socket.read_to_end(&mut response)).await;
    let text = String::from_utf8_lossy(&response);
    text.split(' ').nth(1).and_then(|code| code.parse().ok()).unwrap_or_else(|| panic!("no status for {path}: {text}"))
}

/// Whether `socket` ends (a close frame, the end of the stream or an error) within `wait`.
async fn ends_within<S>(socket: &mut S, wait: Duration) -> bool
where
    S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match tokio::time::timeout_at(deadline, socket.next()).await {
            Err(_) => return false,
            Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(Message::Close(_)))) => return true,
            Ok(Some(Ok(_))) => continue,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_page_has_its_own_stream_cap_and_its_streams_end_with_its_ticket() {
    let Some((_cluster, a, b)) = pair().await else { return };
    let port = dev_server().await;
    let ticket = b.state.previews.mint(crate::previews::tickets::TicketKind::Proxy { port }, Uuid::now_v7(), None);
    let phone = a.device_token(&crate::access::paired_scopes());
    let client = Client::connect(&Endpoint { url: a.ws_url(), token: phone.clone() }).await.unwrap();
    let open = || async {
        client
            .call::<NetworkRelayPageOpen>(NetworkRelayPageOpenParams { machine_id: b.id(), path: format!("/preview-proxy/{ticket}/") })
            .await
            .unwrap()
    };
    let socket_of = |page: &NetworkRelayPageOpenResult| format!("ws://{}{}hmr", a.primary, page.path);

    // A page holds at most MAX_STREAMS_PER_PAGE streams; the next is refused, and the device's own relayed
    // connections are not counted against it.
    let page = open().await;
    let mut sockets = Vec::new();
    for _ in 0..super::relay_pages::MAX_STREAMS_PER_PAGE {
        let (socket, _) = tokio_tungstenite::connect_async(socket_of(&page)).await.unwrap();
        sockets.push(socket);
    }
    match tokio_tungstenite::connect_async(socket_of(&page)).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => assert_eq!(response.status(), 429),
        other => panic!("expected the page's cap, got {other:?}"),
    }
    let mut own = format!("ws://{}/relay/{}/http/preview-proxy/{ticket}/hmr", a.primary, b.id()).into_client_request().unwrap();
    own.headers_mut().insert(super::relay::RELAY_AUTH_HEADER, format!("Bearer {phone}").parse().unwrap());
    let (mut own, _) = tokio_tungstenite::connect_async(own).await.expect("the device keeps its own streams");
    own.send(Message::Text("still here".into())).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(10), own.next()).await.expect("no reply").unwrap().unwrap();
    assert_eq!(reply, Message::Text("echo:still here".into()));

    // Closing the page ends every stream it holds, not only new requests.
    client.call::<NetworkRelayPageClose>(NetworkRelayPageCloseParams { ticket: page.ticket.clone() }).await.unwrap();
    for socket in &mut sockets {
        assert!(ends_within(socket, Duration::from_secs(10)).await, "a page socket outlived its closed ticket");
    }
    own.send(Message::Text("after".into())).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(10), own.next()).await.expect("no reply").unwrap().unwrap();
    assert_eq!(reply, Message::Text("echo:after".into()), "the device's own socket is not the page's");
    own.close(None).await.unwrap();

    // Revoking the device (access.tokens.revoke) ends its pages' open streams too.
    let page = open().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(socket_of(&page)).await.unwrap();
    let admin = Client::connect(&Endpoint { url: a.ws_url(), token: a.device_token(&Scope::ALL) }).await.unwrap();
    admin.call::<TokensRevoke>(TokensRevokeParams { token_id: token_id_of(&a, &phone) }).await.unwrap();
    assert!(ends_within(&mut socket, Duration::from_secs(10)).await, "a page socket outlived its revoked device");
    assert_eq!(reqwest::get(format!("{}{}", a.http_url(), page.path)).await.unwrap().status(), 404);
    eventually("streams are released", WAIT, || a.state.network.relays.open_streams() == 0 && b.state.network.relays.open_streams() == 0)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_page_keeps_its_cookies_and_stops_when_the_network_is_off_or_its_machine_is_gone() {
    let Some((_cluster, a, mut b)) = pair().await else { return };
    let port = dev_server().await;
    let ticket = b.state.previews.mint(crate::previews::tickets::TicketKind::Proxy { port }, Uuid::now_v7(), None);
    let client = admin(&a).await;
    let page = client
        .call::<NetworkRelayPageOpen>(NetworkRelayPageOpenParams { machine_id: b.id(), path: format!("/preview-proxy/{ticket}/") })
        .await
        .unwrap();
    let scope = page.scope.trim_end_matches('/').to_string();

    // A dev server's cookies stay on the page that set it: their path moves under the page's address and Domain goes.
    let response = reqwest::get(format!("{}{}set-cookie", a.http_url(), page.path)).await.unwrap();
    assert_eq!(response.status(), 200);
    let cookies: Vec<String> =
        response.headers().get_all(axum::http::header::SET_COOKIE).iter().map(|value| value.to_str().unwrap().to_string()).collect();
    assert_eq!(cookies, [format!("sid=1; HttpOnly; Path={scope}/"), format!("pref=2; SameSite=Lax; Path={scope}/api")]);

    // With the network off on the relay member the page stops answering; with its machine gone, too.
    network_set(&a, false).await;
    assert_eq!(reqwest::get(format!("{}{}", a.http_url(), page.path)).await.unwrap().status(), 404);
    network_set(&a, true).await;
    b.stop();
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let status = reqwest::get(format!("{}{}", a.http_url(), page.path)).await.unwrap().status();
        if status == 502 || status == 404 {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the page still answers with {status} after its machine stopped");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preview_files_page_through_the_relay_allows_its_relative_scripts_and_a_forged_prefix_is_ignored() {
    let Some((_cluster, a, b)) = pair().await else { return };
    let dir = TempDir::new("relay-page-files");
    std::fs::write(dir.path().join("index.html"), "<script src=\"app.js\"></script>").unwrap();
    std::fs::write(dir.path().join("app.js"), "console.log('hi')").unwrap();
    let root = dir.path().canonicalize().unwrap();
    let ticket = b.state.previews.mint(crate::previews::tickets::TicketKind::Files { root }, Uuid::now_v7(), None);
    let client = admin(&a).await;
    let page = client
        .call::<NetworkRelayPageOpen>(NetworkRelayPageOpenParams {
            machine_id: b.id(),
            path: format!("/preview-files/{ticket}/index.html"),
        })
        .await
        .unwrap();
    let loaded = reqwest::get(format!("{}{}", a.http_url(), page.path)).await.unwrap();
    assert_eq!(loaded.status(), 200);
    let policy = loaded.headers()[axum::http::header::CONTENT_SECURITY_POLICY].to_str().unwrap().to_string();
    // The policy's base is the address the web view loaded, so `app.js` (relative) is allowed.
    let base = format!("http://{}{}", a.primary, page.scope);
    assert!(policy.contains(&format!("default-src {base} ")), "{policy}");
    let script = reqwest::get(format!("{base}app.js")).await.unwrap();
    assert_eq!(script.status(), 200);
    assert_eq!(script.text().await.unwrap(), "console.log('hi')");

    // Straight to b, a client cannot choose the prefix: it carries no relay mark.
    let direct = reqwest::Client::new()
        .get(format!("{}/preview-files/{ticket}/index.html", b.http_url()))
        .header(super::relay_pages::PREFIX_HEADER, "/relay/x/page/y")
        .send()
        .await
        .unwrap();
    let policy = direct.headers()[axum::http::header::CONTENT_SECURITY_POLICY].to_str().unwrap().to_string();
    assert!(policy.contains(&format!("default-src http://{}/preview-files/{ticket}/ ", b.primary)), "{policy}");
    assert!(!policy.contains("/relay/x/page/y"), "{policy}");
}

async fn echo_server() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let (mut reader, mut writer) = socket.split();
                let _ = tokio::io::copy(&mut reader, &mut writer).await;
            });
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forwarded_port_carries_tcp_both_ways_refuses_kybernd_ports_and_closes_when_idle() {
    let Some((_cluster, a, b)) = pair().await else { return };
    let remote_port = echo_server().await;
    let client = admin(&a).await;
    let forwarded =
        client.call::<PortsForward>(PortsForwardParams { machine: b.id(), remote_port, local_port: None, https: false }).await.unwrap();
    assert_eq!((forwarded.remote_port, forwarded.machine_id.as_str()), (remote_port, b.id().as_str()));
    // The same request returns the forward that exists.
    let again =
        client.call::<PortsForward>(PortsForwardParams { machine: b.id(), remote_port, local_port: None, https: false }).await.unwrap();
    assert_eq!(again.id, forwarded.id);
    assert_eq!(client.call::<PortsList>(Empty {}).await.unwrap().forwards.len(), 1);
    // The agent's preview tool asks the hub for a forward and gets the same one.
    let hub = a.state.orchestrator.remote_hub().unwrap().clone();
    assert_eq!(hub.forward_port(&b.id(), remote_port).await.unwrap(), forwarded.local_port);

    // A megabyte each way, written and read at once, passes byte for byte (the window is 256 KiB).
    let payload = pattern(1_300_000);
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", forwarded.local_port)).await.unwrap();
    let (mut reader, mut writer) = socket.split();
    let expected = payload.clone();
    let write = async {
        writer.write_all(&payload).await.unwrap();
        writer.shutdown().await.unwrap(); // a half-close: the reply still arrives
    };
    let read = async {
        let mut got = Vec::new();
        tokio::time::timeout(Duration::from_secs(30), reader.read_to_end(&mut got)).await.expect("the echo never ended").unwrap();
        got
    };
    let ((), got) = tokio::join!(write, read);
    assert_eq!(got.len(), expected.len());
    assert!(got == expected, "the bytes survived both directions");
    eventually("the connection is released", WAIT, || {
        a.state.network.relays.open_streams() == 0 && b.state.network.relays.open_streams() == 0 && client_connections(&a) == 0
    })
    .await;

    // kybernd's own ports are never reachable: its listeners and the default port.
    let own = [b.primary.port(), b.state.network.listening_port().unwrap(), DEFAULT_PORT];
    for port in own {
        let refused =
            client.call::<PortsForward>(PortsForwardParams { machine: b.id(), remote_port: port, local_port: None, https: false }).await;
        let error = refused.unwrap_err().to_string();
        assert!(error.contains("belongs to Kybern"), "port {port}: {error}");
    }
    // Direct peer call, too: no forward is needed to be refused.
    let link = a.state.network.peers.link(&b.id()).unwrap();
    let refused = link.call::<PeerPortsOpen>(PeerPortsOpenParams { stream_id: 999, port: b.primary.port() }).await.unwrap_err();
    assert_eq!(refused.code, codes::FORBIDDEN);
    assert_eq!(client.call::<PortsList>(Empty {}).await.unwrap().forwards.len(), 1, "a refused forward leaves nothing behind");

    // With no connection for the idle time, the forward closes and its port stops answering.
    eventually("the idle forward closes", WAIT, || a.state.network.ports.views().is_empty()).await;
    assert!(tokio::net::TcpStream::connect(("127.0.0.1", forwarded.local_port)).await.is_err());

    // A forward is stopped by hand, too; and a machine that turned ports off refuses it.
    let second =
        client.call::<PortsForward>(PortsForwardParams { machine: b.id(), remote_port, local_port: None, https: false }).await.unwrap();
    let after = client.call::<PortsStop>(PortsStopParams { id: second.id }).await.unwrap();
    assert!(after.forwards.is_empty());
    b.membership().set(&format!("allow.{}.ports", b.id()), json!(false)).unwrap();
    let refused = client.call::<PortsForward>(PortsForwardParams { machine: b.id(), remote_port, local_port: None, https: false }).await;
    assert!(refused.is_err(), "{refused:?}");
}

/// One forwarded connection to b's echo server, proven open by an echo.
async fn open_echo_connection(a: &Node, b: &Node, client: &Client, remote_port: u16) -> tokio::net::TcpStream {
    let forwarded =
        client.call::<PortsForward>(PortsForwardParams { machine: b.id(), remote_port, local_port: None, https: false }).await.unwrap();
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", forwarded.local_port)).await.unwrap();
    socket.write_all(b"ping").await.unwrap();
    let mut got = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(10), socket.read_exact(&mut got)).await.expect("no echo").unwrap();
    assert_eq!(&got, b"ping");
    assert_eq!(client_connections(a), 1);
    socket
}

/// The connection ends: a read sees the end, or an error, and nothing more is echoed.
async fn assert_connection_ends(socket: &mut tokio::net::TcpStream) {
    let _ = socket.write_all(b"more").await;
    let mut rest = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(10), socket.read_to_end(&mut rest)).await;
    assert!(read.is_ok(), "the forwarded connection stayed open");
    assert!(rest.is_empty(), "nothing passes once the capability is gone: {rest:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_open_forwarded_connection_ends_when_the_machine_is_paused_or_ports_are_turned_off() {
    let Some((_cluster, a, b)) = pair().await else { return };
    let remote_port = echo_server().await;
    let client = admin(&a).await;

    // b turns ports off for everyone: the connection a already has ends.
    let mut socket = open_echo_connection(&a, &b, &client, remote_port).await;
    let off = b.membership().set(&format!("allow.{}.ports", b.id()), json!(false)).unwrap();
    b.state.network.publish_ops(vec![off]);
    assert_connection_ends(&mut socket).await;
    eventually("b ends the stream it served", WAIT, || b.state.network.relays.open_streams() == 0).await;

    // Ports on again, then a is paused (written on b, as Settings → Machines does): the open connection ends too.
    let on = b.membership().set(&format!("allow.{}.ports", b.id()), json!(true)).unwrap();
    b.state.network.publish_ops(vec![on]);
    let mut socket = open_echo_connection(&a, &b, &client, remote_port).await;
    let pause = b.membership().set(&format!("machine.{}.paused", a.id()), json!(true)).unwrap();
    b.state.network.publish_ops(vec![pause]);
    assert_connection_ends(&mut socket).await;
    eventually("b ends the stream it served", WAIT, || b.state.network.relays.open_streams() == 0).await;
}

fn client_connections(node: &Node) -> u32 {
    node.state.network.ports.views().iter().map(|forward| forward.connections).sum()
}

// ---- U4: terminals ----

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn relayed(via: &Node, target: &Node, token_at_via: &str, token_at_target: &str) -> Socket {
    let mut request = format!("ws://{}/relay/{}/ws", via.primary, target.id()).into_client_request().unwrap();
    request.headers_mut().insert(super::relay::RELAY_AUTH_HEADER, format!("Bearer {token_at_via}").parse().unwrap());
    request.headers_mut().insert("authorization", format!("Bearer {token_at_target}").parse().unwrap());
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

async fn call(socket: &mut Socket, id: i64, method: &str, params: Value) -> Value {
    socket.send(Message::Text(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string().into())).await.unwrap();
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), socket.next()).await.expect("no answer").unwrap().unwrap();
        if let Message::Text(text) = message {
            let value: Value = serde_json::from_str(text.as_str()).unwrap();
            if value["id"] == id {
                return value;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminal_on_another_machine_runs_there_and_streams_back_through_the_relay() {
    if cfg!(windows) {
        return;
    }
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b"), NodeSpec::owner("c")]) else { return };
    let a = cluster.start("a", true).await;
    let b = cluster.start("b", true).await;
    let c = cluster.start("c", true).await;
    cluster.form(&a, &[&b, &c], JoinVia::Auto).await;
    eventually("linked", WAIT, || linked(&a, &b)).await;
    let work = TempDir::new("terminal-remote");
    let cwd = canonical(&work);
    std::fs::write(cwd.join("only-on-b.txt"), "b-file").unwrap();
    // The phone or the Mac pairs with a, holds a token for b through a, and opens b's terminal.
    let token_at_a = a.device_token(&Scope::CLIENT);
    let token_at_b = b.device_token(&Scope::CLIENT);
    let mut socket = relayed(&a, &b, &token_at_a, &token_at_b).await;
    let terminal_id = Uuid::now_v7();
    let created = call(
        &mut socket,
        1,
        "terminals.create",
        json!({"terminal_id": terminal_id, "cwd": cwd.to_string_lossy(), "command": ["sh"], "cols": 80, "rows": 24}),
    )
    .await;
    assert!(created["error"].is_null(), "{created}");
    assert_eq!(created["result"]["cwd"], cwd.to_string_lossy().as_ref());
    // The terminal exists on b and not on a.
    assert_eq!(b.state.terminals.list(None).len(), 1);
    assert_eq!(a.state.terminals.list(None).len(), 0);

    assert!(call(&mut socket, 2, "terminals.subscribe", json!({"terminal_id": terminal_id, "replay": true})).await["error"].is_null());
    let command = "echo kybern-$((20+22)); cat only-on-b.txt\n";
    // Sent without waiting for its answer here: the output can arrive before it, and `call` would skip it.
    let input = json!({"jsonrpc": "2.0", "id": 3, "method": "terminals.input", "params": {"terminal_id": terminal_id, "data": B64.encode(command)}});
    socket.send(Message::Text(input.to_string().into())).await.unwrap();
    let mut output = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !(output.contains("kybern-42") && output.contains("b-file")) {
        let message =
            tokio::time::timeout_at(deadline, socket.next()).await.unwrap_or_else(|_| panic!("no output: {output:?}")).unwrap().unwrap();
        if let Message::Text(text) = message {
            let value: Value = serde_json::from_str(text.as_str()).unwrap();
            if value["method"] == TERMINAL_OUTPUT_NOTIFICATION
                && let Some(data) = value["params"]["data"].as_str()
            {
                output.push_str(&String::from_utf8_lossy(&B64.decode(data).unwrap()));
            }
        }
    }
    // The device token for a opens nothing on b without b's own credential: a cannot vouch for the client.
    let forged = {
        let mut request = format!("ws://{}/relay/{}/ws", a.primary, b.id()).into_client_request().unwrap();
        request.headers_mut().insert(super::relay::RELAY_AUTH_HEADER, format!("Bearer {token_at_a}").parse().unwrap());
        request.headers_mut().insert("authorization", "Bearer kyb_forged".parse().unwrap());
        tokio_tungstenite::connect_async(request).await
    };
    assert!(forged.is_err());
    // A read-only device cannot start one.
    let read_only = b.device_token(&[Scope::OrchestrationRead]);
    let mut limited = relayed(&a, &b, &token_at_a, &read_only).await;
    let denied = call(&mut limited, 1, "terminals.create", json!({"cwd": cwd.to_string_lossy(), "command": ["sh"]})).await;
    assert!(denied["error"].is_object(), "{denied}");
    call(&mut socket, 4, "terminals.close", json!({"terminal_id": terminal_id})).await;
}

/// Keystrokes sent without waiting for an answer (a paste, an IME burst, a fast typist on a slow link) reach the pty in
/// order and none is refused: the daemon runs a connection's `terminals.input` as one stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_input_sent_without_waiting_arrives_in_order_and_is_never_refused() {
    if cfg!(windows) {
        return;
    }
    let Some((_cluster, a, b)) = pair().await else { return };
    let work = TempDir::new("terminal-input-order");
    let cwd = canonical(&work);
    let token_at_a = a.device_token(&Scope::CLIENT);
    let token_at_b = b.device_token(&Scope::CLIENT);
    let mut socket = relayed(&a, &b, &token_at_a, &token_at_b).await;
    let terminal_id = Uuid::now_v7();
    // `cat` copies what the tty hands it into a file; echo is off so the file is exactly the input.
    let created = call(
        &mut socket,
        1,
        "terminals.create",
        json!({"terminal_id": terminal_id, "cwd": cwd.to_string_lossy(), "command": ["sh", "-c", "stty -echo; cat > typed.txt"], "cols": 80, "rows": 24}),
    )
    .await;
    assert!(created["error"].is_null(), "{created}");
    // 600 requests of one or two bytes, far more than the 16 a connection may have in flight.
    let mut expected = String::new();
    let mut frames = Vec::new();
    for i in 0..600u32 {
        let piece = if i % 40 == 39 { format!("{:03}\n", i % 1000) } else { format!("{:x}", i % 16) };
        expected.push_str(&piece);
        frames.push(piece);
    }
    expected.push('\n');
    for (n, piece) in frames.iter().enumerate() {
        let request = json!({"jsonrpc": "2.0", "id": 100 + n as i64, "method": "terminals.input", "params": {"terminal_id": terminal_id, "data": B64.encode(piece)}});
        socket.send(Message::Text(request.to_string().into())).await.unwrap();
    }
    let last = json!({"jsonrpc": "2.0", "id": 9000, "method": "terminals.input", "params": {"terminal_id": terminal_id, "data": B64.encode("\n\u{4}")}});
    socket.send(Message::Text(last.to_string().into())).await.unwrap();
    let mut answered = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while answered < frames.len() + 1 {
        let message = tokio::time::timeout_at(deadline, socket.next()).await.expect("answers stopped").unwrap().unwrap();
        if let Message::Text(text) = message {
            let value: Value = serde_json::from_str(text.as_str()).unwrap();
            if value["id"].is_i64() && value["id"].as_i64().unwrap() >= 100 {
                assert!(value["error"].is_null(), "an input request was refused: {value}");
                answered += 1;
            }
        }
    }
    let typed = work.path().join("typed.txt");
    eventually("the file holds the input", WAIT, || std::fs::read_to_string(&typed).is_ok_and(|text| text.len() >= expected.len())).await;
    assert_eq!(std::fs::read_to_string(&typed).unwrap(), expected);
    call(&mut socket, 2, "terminals.close", json!({"terminal_id": terminal_id})).await;
}

/// The Browser lists the servers of another machine from that machine's own daemon, and opening one forwards its port here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_servers_of_another_machine_are_listed_there_and_open_through_a_forward() {
    if cfg!(windows) {
        return;
    }
    let Some((_cluster, a, b)) = pair().await else { return };
    // The scan skips the daemon's own process, so the dev server is a child process.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let work = TempDir::new("remote-dev-server");
    std::fs::write(work.path().join("index.html"), "<html><title>served on b</title></html>").unwrap();
    let Ok(mut server) = std::process::Command::new("python3")
        .args(["-m", "http.server", &port.to_string(), "--bind", "127.0.0.1"])
        .current_dir(work.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return;
    };
    eventually("the server listens", WAIT, || std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()).await;
    let result = async {
        // b's own daemon names it; a's does not (the Browser asks the machine the user picked).
        let on_b = admin(&b).await;
        let mut listed = false;
        for _ in 0..20 {
            let result = on_b.call::<PreviewServersList>(PreviewServersListParams { thread_id: None }).await.unwrap();
            if result.servers.iter().any(|server| server.port == port) {
                listed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert!(listed, "the server was not listed on the machine it runs on");
        // Opening it: a forward on a's machine, whose local address serves b's page.
        let on_a = admin(&a).await;
        let forwarded = on_a
            .call::<PortsForward>(PortsForwardParams { machine: b.id(), remote_port: port, local_port: None, https: false })
            .await
            .unwrap();
        let page = reqwest::get(format!("http://127.0.0.1:{}/", forwarded.local_port)).await.unwrap();
        assert_eq!(page.status(), 200);
        assert!(page.text().await.unwrap().contains("served on b"));
        // A machine that is not in the network is a plain error, not a forward to nowhere.
        let unknown = on_a
            .call::<PortsForward>(PortsForwardParams {
                machine: "no-such-machine".into(),
                remote_port: port,
                local_port: None,
                https: false,
            })
            .await;
        assert!(unknown.is_err());
        on_a.call::<PortsStop>(PortsStopParams { id: forwarded.id }).await.unwrap();
    }
    .await;
    let _ = server.kill();
    let _ = server.wait();
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_tools_on_another_machine_need_the_terminals_capability() {
    let Some((_cluster, a, b)) = pair().await else { return };
    let work = TempDir::new("terminal-scope");
    let dir = canonical(&work).join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    add_project(&b, &dir, "proj").await;
    let link = a.state.network.peers.link(&b.id()).unwrap();
    let call = |tool: &str, args: Value| {
        let origin = Origin {
            machine_id: a.id(),
            machine_name: "a".into(),
            thread_id: Some(Uuid::now_v7()),
            operation_id: Uuid::now_v7(),
            kind: OriginKind::ToolCall,
            permission_cap: PermissionMode::FullAccess,
            hops: vec![a.id()],
        };
        let params = PeerToolCallParams {
            tool: tool.into(),
            args,
            origin,
            thread_title: "check".into(),
            repo: None,
            project: Some("proj".into()),
            deadline_ms: None,
        };
        let link = link.clone();
        async move { link.call::<PeerToolCall>(params).await }
    };
    let listed = call("kybern_list_terminals", json!({})).await;
    assert!(listed.is_ok(), "terminals are on by default: {listed:?}");
    b.membership().set(&format!("allow.{}.terminals", b.id()), json!(false)).unwrap();
    let list = call("kybern_list_terminals", json!({})).await.unwrap_err();
    assert!(list.message.contains("terminals"), "{list:?}");
    let run = call("kybern_run_in_terminal", json!({"command": "echo no", "reason": "scope"})).await.unwrap_err();
    assert!(run.message.contains("terminals"), "{run:?}");
    assert_eq!(b.state.terminals.list(None).len(), 0, "nothing ran on b");
    // Other forwarded reads still work.
    assert!(call("kybern_threads_search", json!({})).await.is_ok());
}

// ---- U2: clipboard ----

/// The opt-in registers a member published are known to the others.
async fn opt_in(node: &Node, capability: &str, on: bool) {
    let op = node.membership().set(&format!("allow.{}.{capability}", node.id()), json!(on)).unwrap();
    node.state.network.publish_ops(vec![op]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_clipboard_is_sent_and_pasted_between_machines_and_a_server_keeps_it_in_a_buffer() {
    let Some((_cluster, a, b)) = pair().await else { return };
    a.state.network.clipboard.use_memory();
    b.state.network.clipboard.use_buffer_only();
    let client = admin(&a).await;
    // Send to b (a machine with no desktop): it keeps the text in its Kybern buffer.
    let sent = client.call::<ClipboardSet>(ClipboardSetParams { machine: Some(b.id()), text: "from the mac".into() }).await.unwrap();
    assert_eq!((sent.stored, sent.bytes), (ClipboardStore::Buffer, 12));
    // Paste from b: the buffer answers, and so does b's own client.
    let pasted = client.call::<ClipboardGet>(ClipboardGetParams { machine: Some(b.id()) }).await.unwrap();
    assert_eq!((pasted.text.as_str(), pasted.source), ("from the mac", ClipboardStore::Buffer));
    let own = admin(&b).await.call::<ClipboardGet>(ClipboardGetParams::default()).await.unwrap();
    assert_eq!(own.text, "from the mac");
    // Without a machine it is this one: a's clipboard.
    client.call::<ClipboardSet>(ClipboardSetParams { machine: None, text: "local".into() }).await.unwrap();
    let local = client.call::<ClipboardGet>(ClipboardGetParams::default()).await.unwrap();
    assert_eq!((local.text.as_str(), local.source), ("local", ClipboardStore::System));
    // A megabyte and a byte is too much, and a machine that turned Clipboard off refuses.
    let big = client.call::<ClipboardSet>(ClipboardSetParams { machine: Some(b.id()), text: "x".repeat(MAX_CLIPBOARD_BYTES + 1) }).await;
    assert!(big.is_err());
    b.membership().set(&format!("allow.{}.clipboard", b.id()), json!(false)).unwrap();
    let refused = client.call::<ClipboardGet>(ClipboardGetParams { machine: Some(b.id()) }).await.unwrap_err();
    assert!(refused.to_string().contains("Clipboard"), "{refused}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_universal_clipboard_is_off_until_both_machines_opt_in_and_it_never_shares_a_secret() {
    use super::clipboard::tick;
    use super::clipboard_filter::Markers;
    let Some((_cluster, a, b)) = pair().await else { return };
    a.state.network.clipboard.use_memory();
    b.state.network.clipboard.use_memory();
    let status = |node: &Node| crate::network::clipboard::status(&node.state);
    assert!(!status(&a).auto && !status(&b).auto, "off by default");

    // Off: nothing is shared, and nothing is even read.
    a.state.network.clipboard.set("copied on a").await.unwrap();
    a.state.network.clipboard.use_memory();
    tick(&a.state).await;
    assert!(b.state.network.clipboard.memory_for_test().as_deref().unwrap_or("").is_empty(), "nothing arrived");

    // Only a opted in: still nothing, since no member has.
    opt_in(&a, "clipboard_auto", true).await;
    eventually("a is opted in", WAIT, || status(&a).auto).await;
    tick(&a.state).await;
    assert!(b.state.network.clipboard.memory_for_test().as_deref().unwrap_or("").is_empty());

    // Both opted in: a change reaches b, sealed, and b does not send it back.
    opt_in(&b, "clipboard_auto", true).await;
    eventually("a sees b opted in", WAIT, || a.membership().projection().allowed(&b.id()).get("clipboard_auto") == Some(&true)).await;
    eventually("b sees a opted in", WAIT, || b.membership().projection().allowed(&a.id()).get("clipboard_auto") == Some(&true)).await;
    a.state.network.clipboard.set("a sentence copied on a").await.unwrap();
    // `set` marks it as applied; a real copy is a change the watcher has not seen.
    *a.state.network.clipboard.memory_for_test() = Some("a sentence copied on a".into());
    a.state.network.clipboard.forget_applied();
    tick(&a.state).await;
    eventually("b has the text", WAIT, || b.state.network.clipboard.memory_for_test().as_deref() == Some("a sentence copied on a")).await;
    tick(&b.state).await;
    assert_eq!(a.state.network.clipboard.memory_for_test().as_deref(), Some("a sentence copied on a"), "no echo");
    assert_eq!(a.state.network.clipboard.skips.total(), 0);

    // A secret is skipped by shape, by entropy and by the source's own marker; the skips are counted.
    for secret in [
        "ghp_0123456789abcdefghijklmnopqrstuvwxyz",
        "Zx9Qm4Vb7Lp2Rt8Ys6Nc3Hd5Jf1Kg0W",
        "-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----",
    ] {
        *a.state.network.clipboard.memory_for_test() = Some(secret.into());
        tick(&a.state).await;
    }
    *a.state.network.clipboard.memory_for_test() = Some("an ordinary sentence from a password manager".into());
    a.state.network.clipboard.set_memory_markers(Markers { concealed: true, transient: false });
    tick(&a.state).await;
    assert_eq!(a.state.network.clipboard.skips.total(), 4);
    assert_eq!(status(&a).skipped, 4);
    assert_eq!(b.state.network.clipboard.memory_for_test().as_deref(), Some("a sentence copied on a"), "nothing secret crossed");

    // Opting out stops it at once.
    a.state.network.clipboard.set_memory_markers(Markers::default());
    *a.state.network.clipboard.memory_for_test() = Some("another ordinary sentence".into());
    opt_in(&b, "clipboard_auto", false).await;
    eventually("a sees b opted out", WAIT, || a.membership().projection().allowed(&b.id()).get("clipboard_auto") == Some(&false)).await;
    tick(&a.state).await;
    assert_eq!(b.state.network.clipboard.memory_for_test().as_deref(), Some("a sentence copied on a"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_machines_forward_one_server_independently_and_old_peers_keep_the_stream_transport() {
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b"), NodeSpec::owner("c")]) else {
        return;
    };
    let a = cluster.start("a", true).await;
    let b = cluster.start("b", true).await;
    let c = cluster.start("c", true).await;
    cluster.form(&a, &[&b, &c], JoinVia::Auto).await;
    eventually("both clients have the server presence", WAIT, || {
        linked(&a, &b) && linked(&c, &b) && [&a, &c].iter().all(|node| node.state.network.peers.status(&b.id()).presence.is_some())
    })
    .await;
    let port = echo_server().await;
    let ca = admin(&a).await;
    let cc = admin(&c).await;
    let mut first = open_echo_connection(&a, &b, &ca, port).await;
    let mut second = open_echo_connection(&c, &b, &cc, port).await;
    assert_eq!(b.state.network.peers.link(&a.id()).unwrap().relays.len(), 0, "raw TCP must bypass the multiplexed streams");
    let forward = ca.call::<PortsList>(Empty {}).await.unwrap().forwards.remove(0);
    ca.call::<PortsStop>(PortsStopParams { id: forward.id }).await.unwrap();
    assert_connection_ends(&mut first).await;
    second.write_all(b"live").await.unwrap();
    let mut reply = [0; 4];
    second.read_exact(&mut reply).await.unwrap();
    assert_eq!(&reply, b"live");
    // A 0.20.x presence lacks ports.direct.v1. The unchanged stream path must still work.
    let mut old = b.state.network.presence();
    old.version = "0.20.4".into();
    old.features.retain(|f| f != features::PORTS_DIRECT_V1);
    a.state.network.peers.set_presence(&b.id(), old);
    let _legacy = open_echo_connection(&a, &b, &ca, port).await;
    assert_eq!(a.state.network.relays.open_streams(), 1);
    assert_eq!(b.state.network.peers.link(&a.id()).unwrap().relays.len(), 1);
}
