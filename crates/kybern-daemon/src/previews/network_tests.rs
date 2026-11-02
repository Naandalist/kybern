//! Server previews through authenticated client RPC, real machine sessions and the port relay.
//! Ephemeral ports on 127.0.0.1 keep this test runnable on macOS without extra loopback aliases.
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message as AxumMessage, WebSocketUpgrade};
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use kybern_client::{Client, Endpoint};
use kybern_protocol::{methods::*, *};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use uuid::Uuid;

use crate::network::{peers, session, tailscale::Fake, testutil::TempDir};

struct Node {
    state: crate::state::AppState,
    address: SocketAddr,
    server: tokio::task::JoinHandle<()>,
    dir: TempDir,
}

impl Node {
    async fn start(name: &str) -> Self {
        Self::start_tailnet(name, None).await
    }

    async fn start_tailnet(name: &str, peer: Option<&str>) -> Self {
        let dir = TempDir::new(name);
        let paths = crate::config::Paths::resolve(Some(dir.path().to_path_buf())).unwrap();
        let fixture = dir.path().join("no-tailnet.json");
        if let Some(peer) = peer {
            write_tailnet(&fixture, name, peer);
        }
        // No fake tailnet nodes: membership is enrolled explicitly as code-joined machines.
        // Only Tailscale discovery is replaced; identity proofs, signatures and sockets are real.
        let state = crate::state::AppState::initialize_with_drivers(
            &paths,
            Arc::new(Fake::new(fixture)),
            crate::network::Timing::default(),
            kybern_drivers::registry::DriverRegistry::default(),
        )
        .unwrap();
        let mut settings = state.settings.get();
        settings.network.enabled = true;
        state.settings.set(settings).unwrap();
        state.network.set_enabled(true);
        state.orchestrator.remote_start();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        *state.listen_addr.write().unwrap() = Some(address);
        state.port.store(address.port(), std::sync::atomic::Ordering::Relaxed);
        let app = Router::new()
            .merge(crate::http::routes())
            .merge(crate::network::routes())
            .route("/ws", get(crate::ws::upgrade))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
        });
        Self { state, address, server, dir }
    }

    fn id(&self) -> String {
        self.state.network.machine_id().unwrap()
    }

    async fn client(&self, scopes: &[Scope]) -> (Client, Uuid) {
        let token = crate::auth::generate();
        let id = Uuid::now_v7();
        self.state.store.token_insert(id, &crate::auth::hash(&token), "preview test device", scopes).unwrap();
        let client = Client::connect(&Endpoint { url: format!("ws://{}/ws", self.address), token }).await.unwrap();
        (client, id)
    }

    async fn link(&self, other: &Self) {
        let target = session::Target::at_url(
            other.id(),
            other.state.network.membership().unwrap().keys().signing_public_b64(),
            format!("http://{}", other.address),
        );
        let established =
            session::open(&self.state.network, self.state.network.membership().unwrap(), &target, &Scope::MACHINE_DEFAULT, None)
                .await
                .unwrap();
        let mut request = format!("ws://{}/ws", other.address).into_client_request().unwrap();
        request.headers_mut().insert("authorization", format!("Bearer {}", established.token).parse().unwrap());
        let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        peers::run_dialed(self.state.clone(), target, established, socket);
        crate::network::harness::eventually("preview machine session", Duration::from_secs(5), || {
            self.state.network.peers.link(&other.id()).is_some() && other.state.network.peers.link(&self.id()).is_some()
        })
        .await;
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.state.shutdown.cancel();
        self.state.network.ports.close_all();
        self.state.network.peers.close_all();
        self.server.abort();
    }
}

#[test]
fn server_preview_rpc_forwards_a_third_machine_without_a_primary_thread() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_stack_size(crate::RUNTIME_THREAD_STACK)
        .enable_all()
        .build()
        .unwrap()
        .block_on(run_server_preview());
}

async fn run_server_preview() {
    let primary = Node::start("preview-primary").await;
    let thread_host = Node::start("preview-thread-host").await;
    let server_host = Node::start("preview-server-host").await;
    let members = primary.state.network.membership().unwrap();
    let mut info = primary.state.network.self_info().await;
    info.name = "Primary".into();
    members.genesis("owner@example.test", "test", &info).unwrap();
    for (node, name) in [(&thread_host, "Thread host"), (&server_host, "Server host")] {
        let mut info = node.state.network.self_info().await;
        info.name = name.into();
        let descriptor = node.state.network.membership().unwrap().self_descriptor(&info);
        members.add(descriptor, JoinVia::Code).unwrap();
    }
    for node in [&thread_host, &server_host] {
        node.state.network.membership().unwrap().join(members.all_ops()).unwrap();
    }
    primary.link(&thread_host).await;
    primary.link(&server_host).await;

    // A selected conversation belongs to B; primary A deliberately has no local threads.
    let (on_thread_host, _) = thread_host.client(&Scope::ALL).await;
    let project = on_thread_host
        .call::<ProjectsAdd>(ProjectsAddParams {
            path: thread_host.dir.path().to_string_lossy().into_owned(),
            name: Some("Thread project".into()),
            space_id: None,
            kind: None,
            members: None,
        })
        .await
        .unwrap();
    let thread = on_thread_host
        .call::<ThreadsCreate>(ThreadsCreateParams {
            project_id: Some(project.id),
            provider: ProviderInstance::default_for(ProviderKind::ClaudeCode),
            model: None,
            effort: None,
            permission_mode: None,
            use_worktree: Some(false),
            base_branch: None,
            title: Some("Preview elsewhere".into()),
            message: None,
            thread_id: None,
            message_id: None,
            members: None,
            base_branches: None,
            pin_account: false,
        })
        .await
        .unwrap();
    assert!(primary.state.store.thread_get(thread.id).unwrap().is_none());
    let (client, device) = primary.client(&crate::access::paired_scopes()).await;
    assert!(client.call::<ThreadsList>(ThreadsListParams::default()).await.unwrap().threads.is_empty());

    // C serves its own identity, root-relative assets, and a Vite-style hot reload socket.
    let source = server_host.id();
    let page_source = source.clone();
    let socket_source = source.clone();
    let app = Router::new()
        .route(
            "/app",
            get(move || {
                let source = page_source.clone();
                async move { format!("<title>{source}</title><script src='/asset.js'></script>") }
            }),
        )
        .route("/asset.js", get(|| async { "window.sourceLoaded = true" }))
        .route(
            "/hmr",
            get(move |ws: WebSocketUpgrade| {
                let source = socket_source.clone();
                async move {
                    ws.protocols(["vite-hmr"]).on_upgrade(move |mut socket| async move {
                        while let Some(Ok(AxumMessage::Text(text))) = socket.recv().await {
                            if socket.send(AxumMessage::Text(format!("{source}:{text}").into())).await.is_err() {
                                break;
                            }
                        }
                    })
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_port = listener.local_addr().unwrap().port();
    let dev = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let forward = client
        .call::<PortsForward>(PortsForwardParams { machine: source.clone(), remote_port, local_port: None, https: false })
        .await
        .unwrap();
    assert_eq!(forward.machine_id, source);
    assert_eq!(forward.remote_port, remote_port);
    assert_ne!(forward.local_port, remote_port);
    let params = PreviewServerOpenParams { url: format!("http://localhost:{}/app?q=1#hash", forward.local_port), proxy: true };
    let opened = client.call::<PreviewServerOpen>(params.clone()).await.unwrap();
    assert_eq!(opened.target, PreviewTargetInfo::Server { url: params.url.clone(), port: forward.local_port });
    let ticket = opened.ticket.unwrap();
    let capability = primary.state.previews.lookup(&ticket).unwrap();
    assert_eq!(capability.thread_id, None);
    assert_eq!(capability.principal, Some(device));
    let base = format!("http://{}{}", primary.address, opened.path.unwrap().trim_end_matches('/'));
    let http = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap();
    let page = http.get(format!("{base}/app?q=1")).send().await.unwrap();
    assert_eq!(page.status(), 200);
    assert!(page.text().await.unwrap().contains(&source));
    assert_eq!(http.get(format!("{base}/asset.js")).send().await.unwrap().text().await.unwrap(), "window.sourceLoaded = true");
    let mut request = format!("{}/hmr", base.replace("http://", "ws://")).into_client_request().unwrap();
    request.headers_mut().insert("sec-websocket-protocol", "vite-hmr".parse().unwrap());
    let (mut socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(response.headers().get("sec-websocket-protocol").unwrap(), "vite-hmr");
    for update in ["update-1", "update-2"] {
        socket.send(Message::Text(update.into())).await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(5), socket.next()).await.unwrap().unwrap().unwrap();
        assert_eq!(reply, Message::Text(format!("{source}:{update}").into()));
    }
    assert!(primary.state.network.peers.link(&server_host.id()).unwrap().relays.len() > 0, "code-only peers must use the fallback stream");
    socket.close(None).await.unwrap();

    // Scope and address checks run at the actual client entry point.
    let (limited, _) = primary.client(&[Scope::OrchestrationRead]).await;
    let forbidden = limited.call::<PreviewServerOpen>(params.clone()).await.unwrap_err().to_string();
    assert!(forbidden.contains(&codes::FORBIDDEN.to_string()) && forbidden.contains("orchestration:operate"), "{forbidden}");
    for url in [
        format!("http://localhost:{}", primary.address.port()),
        "http://192.168.1.1:5173".into(),
        "https://example.test".into(),
        "file:///tmp/index.html".into(),
        "/tmp/index.html".into(),
    ] {
        assert!(client.call::<PreviewServerOpen>(PreviewServerOpenParams { url: url.clone(), proxy: true }).await.is_err(), "{url}");
    }
    assert!(
        client
            .call::<PortsForward>(PortsForwardParams {
                machine: source,
                remote_port: server_host.address.port(),
                local_port: None,
                https: false
            })
            .await
            .is_err()
    );
    let refused = client
        .call::<PreviewServerOpen>(PreviewServerOpenParams { url: format!("https://localhost:{remote_port}"), proxy: true })
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("HTTPS servers cannot be proxied"), "{refused}");

    client.call::<PreviewClose>(PreviewCloseParams { ticket }).await.unwrap();
    assert_eq!(http.get(format!("{base}/app")).send().await.unwrap().status(), 404);
    let opened = client.call::<PreviewServerOpen>(params).await.unwrap();
    let revoked = format!("http://{}{}app", primary.address, opened.path.unwrap());
    let (admin, _) = primary.client(&Scope::ALL).await;
    admin.call::<TokensRevoke>(TokensRevokeParams { token_id: device }).await.unwrap();
    assert_eq!(http.get(revoked).send().await.unwrap().status(), 404);
    assert!(primary.state.previews.lookup(&opened.ticket.unwrap()).is_none());
    admin.call::<PortsStop>(PortsStopParams { id: forward.id }).await.unwrap();
    dev.abort();
}

// Two loopback-only nodes exercise production TCP forwarding even on Macs without aliases.
fn write_tailnet(path: &std::path::Path, name: &str, peer: &str) {
    use crate::network::tailscale::{FakeFixture, FakeNode};
    let nodes = [peer, name]
        .into_iter()
        .enumerate()
        .map(|(i, name)| FakeNode {
            name: name.into(),
            node_id: format!("n{name}"),
            ip: format!("100.64.0.{}", i + 1).parse().unwrap(),
            listen: "127.0.0.1".parse().unwrap(),
            login: "owner@example.test".into(),
            user_id: 1,
            tags: vec![],
            shared_in: false,
            online: true,
        })
        .collect();
    let fixture = FakeFixture { self_name: name.into(), tailnet: "test".into(), nodes };
    std::fs::write(path, serde_json::to_vec(&fixture).unwrap()).unwrap();
}

async fn direct_pair() -> (Node, Node) {
    let a = Node::start_tailnet("a", Some("b")).await;
    let b = Node::start_tailnet("b", Some("a")).await;
    let members = a.state.network.membership().unwrap();
    members.genesis("owner@example.test", "test", &a.state.network.self_info().await).unwrap();
    let descriptor = b.state.network.membership().unwrap().self_descriptor(&b.state.network.self_info().await);
    members.add(descriptor, JoinVia::Code).unwrap();
    b.state.network.membership().unwrap().join(members.all_ops()).unwrap();
    a.link(&b).await;
    (a, b)
}

#[test]
fn direct_port_forward_authorizes_tokens_members_and_policy_and_carries_websockets() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_stack_size(crate::RUNTIME_THREAD_STACK)
        .enable_all()
        .build()
        .unwrap()
        .block_on(run_direct_ports());
}

async fn run_direct_ports() {
    use crate::network::ports::direct;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (a, b) = direct_pair().await;
    let app = Router::new().route("/", get(|| async { "hello direct" })).route(
        "/hmr",
        get(|ws: WebSocketUpgrade| async {
            ws.protocols(["vite-hmr"]).on_upgrade(|mut socket| async move {
                while let Some(Ok(message)) = socket.recv().await {
                    if socket.send(message).await.is_err() {
                        break;
                    }
                }
            })
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_port = listener.local_addr().unwrap().port();
    let dev = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let link = a.state.network.peers.link(&b.id()).unwrap();
    let id = Uuid::now_v7();
    let prepared =
        link.call::<PeerPortsPrepare>(PeerPortsPrepareParams { forward_id: id, port: remote_port, direct_version: None }).await.unwrap();
    let endpoint = prepared.direct.expect("a direct listener on the fake tailnet");
    let destination = direct::destination(&a.state.network, &b.id(), endpoint.port).await.unwrap();
    let bad = PeerPortsDirect { token: "0".repeat(64), port: endpoint.port, version: endpoint.version };
    assert!(direct::connect(&a.state.network, destination, &bad).await.is_none(), "invalid token accepted");
    // A node outside membership with a stolen per-forward token is still refused.
    write_tailnet(&b.dir.path().join("no-tailnet.json"), "b", "outsider");
    assert!(direct::connect(&a.state.network, destination, &endpoint).await.is_none(), "non-member accepted");
    write_tailnet(&b.dir.path().join("no-tailnet.json"), "b", "a");
    let mut raw = direct::connect(&a.state.network, destination, &endpoint).await.unwrap();
    raw.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut response = String::new();
    raw.read_to_string(&mut response).await.unwrap();
    assert!(response.contains("hello direct"));
    link.call::<PeerPortsRelease>(PeerPortsReleaseParams { forward_id: id }).await.unwrap();

    let (client, _) = a.client(&Scope::ALL).await;
    let forwarded =
        client.call::<PortsForward>(PortsForwardParams { machine: b.id(), remote_port, local_port: None, https: false }).await.unwrap();
    assert!(forwarded.local_port > remote_port, "the occupied remote port is followed by a higher free port");
    assert!(
        a.state.network.ports.lock().iter().find(|f| f.id == forwarded.id).unwrap().route_now().is_some(),
        "no negotiated direct route"
    );
    let page = reqwest::get(format!("http://127.0.0.1:{}/", forwarded.local_port)).await.unwrap().text().await.unwrap();
    assert_eq!(page, "hello direct");
    let mut request = format!("ws://127.0.0.1:{}/hmr", forwarded.local_port).into_client_request().unwrap();
    request.headers_mut().insert("sec-websocket-protocol", "vite-hmr".parse().unwrap());
    let (mut websocket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(response.headers()["sec-websocket-protocol"], "vite-hmr");
    websocket.send(Message::Text("update".into())).await.unwrap();
    assert_eq!(websocket.next().await.unwrap().unwrap(), Message::Text("update".into()));
    assert_eq!(a.state.network.peers.link(&b.id()).unwrap().relays.len(), 0, "the direct path used the WebSocket tunnel");
    assert_eq!(b.state.network.peers.link(&a.id()).unwrap().relays.len(), 0);
    // A third member gets a separate listener and token for the same remote service.
    // This loopback-only fixture attributes new handshakes to c; a's established socket stays live.
    let c = Node::start_tailnet("c", Some("b")).await;
    let members = a.state.network.membership().unwrap();
    let descriptor = c.state.network.membership().unwrap().self_descriptor(&c.state.network.self_info().await);
    let added = members.add(descriptor, JoinVia::Code).unwrap();
    b.state.network.apply_remote(Some(&a.id()), vec![added]).unwrap();
    c.state.network.membership().unwrap().join(members.all_ops()).unwrap();
    write_tailnet(&b.dir.path().join("no-tailnet.json"), "b", "c");
    c.link(&b).await;
    let (on_c, _) = c.client(&Scope::ALL).await;
    let second =
        on_c.call::<PortsForward>(PortsForwardParams { machine: b.id(), remote_port, local_port: None, https: false }).await.unwrap();
    let token_a = a.state.network.ports.lock().iter().find(|f| f.id == forwarded.id).unwrap().route_now().unwrap().endpoint.token.clone();
    let token_c = c.state.network.ports.lock().iter().find(|f| f.id == second.id).unwrap().route_now().unwrap().endpoint.token.clone();
    assert!(token_a != token_c, "each member must get its own token");
    let (mut other, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}/hmr", second.local_port)).await.unwrap();
    other.send(Message::Text("client-c".into())).await.unwrap();
    assert_eq!(other.next().await.unwrap().unwrap(), Message::Text("client-c".into()));
    websocket.send(Message::Text("still-a".into())).await.unwrap();
    assert_eq!(websocket.next().await.unwrap().unwrap(), Message::Text("still-a".into()));
    on_c.call::<PortsStop>(PortsStopParams { id: second.id }).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(3), other.next()).await.is_ok());
    websocket.send(Message::Text("a-after-c-stopped".into())).await.unwrap();
    assert_eq!(websocket.next().await.unwrap().unwrap(), Message::Text("a-after-c-stopped".into()));
    write_tailnet(&b.dir.path().join("no-tailnet.json"), "b", "a");

    // Narrowing policy must end the socket that is already open.
    let op = b.state.network.membership().unwrap().set(&format!("allow.{}.ports", b.id()), serde_json::json!(false)).unwrap();
    b.state.network.publish_ops(vec![op]);
    let end = tokio::time::timeout(Duration::from_secs(3), websocket.next()).await.expect("policy did not end TCP");
    assert!(end.is_none_or(|frame| frame.is_err() || matches!(frame, Ok(Message::Close(_)))));
    assert!(
        client.call::<PortsForward>(PortsForwardParams { machine: b.id(), remote_port: 1, local_port: None, https: false }).await.is_err()
    );
    client.call::<PortsStop>(PortsStopParams { id: forwarded.id }).await.unwrap();
    let op = b.state.network.membership().unwrap().set(&format!("allow.{}.ports", b.id()), serde_json::json!(true)).unwrap();
    b.state.network.publish_ops(vec![op]);
    let mut old = b.state.network.presence();
    old.version = "0.20.4".into();
    old.features.retain(|f| f != features::PORTS_DIRECT_V1 && f != features::PORTS_DIRECT_V2 && f != features::RELAY_FRAMES_V2);
    // a now sends JSON frames with base64 data and the 256 KiB window; b still sends a binary frames.
    a.state.network.peers.link(&b.id()).unwrap().set_peer_features(&old.features);
    a.state.network.peers.set_presence(&b.id(), old);
    let legacy =
        client.call::<PortsForward>(PortsForwardParams { machine: b.id(), remote_port, local_port: None, https: false }).await.unwrap();
    let (mut legacy_ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}/hmr", legacy.local_port)).await.unwrap();
    legacy_ws.send(Message::Text("legacy".into())).await.unwrap();
    assert_eq!(legacy_ws.next().await.unwrap().unwrap(), Message::Text("legacy".into()));
    assert_eq!(a.state.network.peers.link(&b.id()).unwrap().relays.len(), 1, "old peers must use stream transport");
    client.call::<PortsStop>(PortsStopParams { id: legacy.id }).await.unwrap();
    dev.abort();
}

/// Kept ignored because it holds scratch nodes open for the external browser benchmark.
#[test]
#[ignore = "run with KYBERN_PORT_BENCHMARK_ROOT and a scratch Vite server"]
fn port_forward_benchmark_fixture() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_stack_size(crate::RUNTIME_THREAD_STACK)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let root = std::path::PathBuf::from(std::env::var_os("KYBERN_PORT_BENCHMARK_ROOT").expect("scratch benchmark folder"));
            let remote_port = std::env::var("KYBERN_PORT_BENCHMARK_PORT").unwrap_or_else(|_| "58173".into()).parse().unwrap();
            let (a, b) = direct_pair().await;
            let (client, _) = a.client(&Scope::ALL).await;
            let started = std::time::Instant::now();
            let direct = client
                .call::<PortsForward>(PortsForwardParams { machine: b.id(), remote_port, local_port: None, https: false })
                .await
                .unwrap();
            let open_ms = started.elapsed().as_secs_f64() * 1000.0;
            let mut old = b.state.network.presence();
            old.features.retain(|f| f != features::PORTS_DIRECT_V1);
            a.state.network.peers.set_presence(&b.id(), old);
            let tunnel = client
                .call::<PortsForward>(PortsForwardParams { machine: b.id(), remote_port, local_port: Some(0), https: false })
                .await
                .unwrap();
            let info =
                serde_json::json!({ "direct": remote_port, "tcp": direct.local_port, "tunnel": tunnel.local_port, "open_ms": open_ms });
            std::fs::write(root.join("forwards.json"), serde_json::to_vec_pretty(&info).unwrap()).unwrap();
            println!("{info}");
            for _ in 0..1200 {
                if root.join("done").exists() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            panic!("benchmark did not signal completion within 10 minutes");
        });
}

#[test]
fn fast_mode_multiplexes_a_page_over_http2_and_keeps_websockets_and_events_working() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_stack_size(crate::RUNTIME_THREAD_STACK)
        .enable_all()
        .build()
        .unwrap()
        .block_on(run_fast_mode());
}

/// A TLS client that trusts exactly the forward's certificate, as Kybern's web views do.
fn pinned_tls(pem: &str, alpn: &[&[u8]]) -> Arc<rustls::ClientConfig> {
    use rustls::pki_types::{CertificateDer, pem::PemObject};
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from_pem_slice(pem.as_bytes()).unwrap()).unwrap();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Arc::new(config)
}

async fn run_fast_mode() {
    use http_body_util::{BodyExt, Empty};
    use hyper::body::Bytes;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (a, b) = direct_pair().await;
    let connections = Arc::new(AtomicUsize::new(0));
    let hosts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let app = Router::new()
        .route(
            "/module/{n}",
            get({
                let hosts = hosts.clone();
                move |axum::extract::Path(n): axum::extract::Path<u32>, headers: axum::http::HeaderMap| {
                    let hosts = hosts.clone();
                    async move {
                        hosts.lock().unwrap().push(headers.get("host").and_then(|h| h.to_str().ok()).unwrap_or_default().to_string());
                        format!("export default {n}")
                    }
                }
            }),
        )
        .route("/big.js", get(|| async { ([("content-type", "text/javascript")], "export const x = 1 // padding\n".repeat(200)) }))
        .route(
            "/events",
            get(|| async {
                let stream = futures::stream::iter((0..3).map(|i| Ok::<_, std::convert::Infallible>(format!("data: tick {i}\n\n"))));
                ([("content-type", "text/event-stream")], axum::body::Body::from_stream(stream))
            }),
        )
        .route(
            "/hmr",
            get(|ws: WebSocketUpgrade| async {
                ws.protocols(["vite-hmr"]).on_upgrade(|mut socket| async move {
                    while let Some(Ok(message)) = socket.recv().await {
                        if socket.send(message).await.is_err() {
                            break;
                        }
                    }
                })
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_port = listener.local_addr().unwrap().port();
    let counted = connections.clone();
    let dev = tokio::spawn(async move {
        let listener = axum::serve::ListenerExt::tap_io(listener, move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
        });
        axum::serve(listener, app).await.unwrap();
    });

    let (client, _) = a.client(&Scope::ALL).await;
    let plain =
        client.call::<PortsForward>(PortsForwardParams { machine: b.id(), remote_port, local_port: None, https: false }).await.unwrap();
    assert!(plain.https_available, "a current machine offers Fast mode");
    assert!(plain.https_port.is_none() && plain.https_certificate.is_none(), "Fast mode is opened only when asked for");
    assert!(plain.rtt_ms.is_some());
    let forward =
        client.call::<PortsForward>(PortsForwardParams { machine: b.id(), remote_port, local_port: None, https: true }).await.unwrap();
    assert_eq!(forward.id, plain.id, "the same forward gains an HTTPS listener");
    let port = forward.https_port.expect("an HTTPS listener");
    let pem = forward.https_certificate.clone().unwrap();
    assert_eq!(forward.https_certificate_sha256.as_deref().unwrap().len(), 64);
    // The plain listener keeps working beside it.
    assert_eq!(
        reqwest::get(format!("http://127.0.0.1:{}/module/1", forward.local_port)).await.unwrap().text().await.unwrap(),
        "export default 1"
    );

    // An untrusting client is refused: the certificate is in no trust store.
    let stranger = tokio_rustls::TlsConnector::from(Arc::new({
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth()
    }));
    let socket = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    assert!(stranger.connect("localhost".try_into().unwrap(), socket).await.is_err());

    // One HTTP/2 connection carries a page's worth of concurrent module requests.
    let connector = tokio_rustls::TlsConnector::from(pinned_tls(&pem, &[b"h2"]));
    let socket = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let tls = connector.connect("localhost".try_into().unwrap(), socket).await.unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    let (sender, connection) =
        hyper::client::conn::http2::handshake(hyper_util::rt::TokioExecutor::new(), hyper_util::rt::TokioIo::new(tls)).await.unwrap();
    tokio::spawn(connection);
    let burst = |sender: hyper::client::conn::http2::SendRequest<Empty<Bytes>>, count: u32| {
        futures::future::join_all((0..count).map(move |n| {
            let mut sender = sender.clone();
            async move {
                let request = hyper::Request::get(format!("https://localhost:{port}/module/{n}")).body(Empty::<Bytes>::new()).unwrap();
                let response = sender.send_request(request).await.unwrap();
                assert_eq!(response.version(), hyper::Version::HTTP_2);
                assert_eq!(response.status(), 200);
                String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
            }
        }))
    };
    hosts.lock().unwrap().clear();
    let bodies = burst(sender.clone(), 200).await;
    for (n, body) in bodies.iter().enumerate() {
        assert_eq!(body, &format!("export default {n}"));
    }
    // The next page load reuses the target's pooled connections to the dev server.
    let before = connections.load(Ordering::SeqCst);
    assert_eq!(burst(sender.clone(), 40).await.len(), 40);
    let opened = connections.load(Ordering::SeqCst) - before;
    assert_eq!(opened, 0, "the target pools its connections to the dev server ({opened} opened)");
    assert!(hosts.lock().unwrap().iter().all(|host| host == &format!("localhost:{port}")), "the dev server sees the browser's host");
    // Nothing went over the session's WebSocket.
    assert_eq!(a.state.network.peers.link(&b.id()).unwrap().relays.len(), 0);

    // A large module is gzipped on the target when the browser accepts it, and decodes to what the dev server sent; a
    // small one, or a request that does not accept gzip, passes as it is.
    let mut zipped = sender.clone();
    let request = |path: &str, gzip: bool| {
        let mut builder = hyper::Request::get(format!("https://localhost:{port}{path}"));
        if gzip {
            builder = builder.header("accept-encoding", "gzip, deflate, br");
        }
        builder.body(Empty::<Bytes>::new()).unwrap()
    };
    let response = zipped.send_request(request("/big.js", true)).await.unwrap();
    assert_eq!(response.headers()["content-encoding"], "gzip");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let mut text = String::new();
    std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&body[..]), &mut text).unwrap();
    assert_eq!(text, "export const x = 1 // padding\n".repeat(200));
    assert!(body.len() < text.len() / 4, "compressed {} of {}", body.len(), text.len());
    let plain = zipped.send_request(request("/big.js", false)).await.unwrap();
    assert!(!plain.headers().contains_key("content-encoding"));
    let small = zipped.send_request(request("/module/7", true)).await.unwrap();
    assert!(!small.headers().contains_key("content-encoding"));

    // Server-sent events stream through, never compressed.
    let mut events = sender.clone();
    let response = events.send_request(request("/events", true)).await.unwrap();
    assert!(!response.headers().contains_key("content-encoding"));
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let text = String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert_eq!(text, "data: tick 0\n\ndata: tick 1\n\ndata: tick 2\n\n");

    // Hot reload: a WebSocket on the same origin over HTTP/1.1, with its subprotocol.
    let mut request = format!("wss://localhost:{port}/hmr").into_client_request().unwrap();
    request.headers_mut().insert("sec-websocket-protocol", "vite-hmr".parse().unwrap());
    let socket = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (mut websocket, response) = tokio_tungstenite::client_async_tls_with_config(
        request,
        socket,
        None,
        Some(tokio_tungstenite::Connector::Rustls(pinned_tls(&pem, &[b"http/1.1"]))),
    )
    .await
    .unwrap();
    assert_eq!(response.headers()["sec-websocket-protocol"], "vite-hmr");
    websocket.send(Message::Text("update".into())).await.unwrap();
    assert_eq!(websocket.next().await.unwrap().unwrap(), Message::Text("update".into()));

    // The target's listener lives with the session it was prepared on. After a new session (a restart there, a renewed
    // connection), the forward prepares itself again: plain and HTTPS pages keep loading through the same local ports.
    let before = a.state.network.ports.lock().iter().find(|f| f.id == forward.id).unwrap().route_now().unwrap();
    a.state.network.peers.link(&b.id()).unwrap().close();
    tokio::time::sleep(Duration::from_millis(200)).await;
    a.link(&b).await;
    let page = reqwest::get(format!("http://127.0.0.1:{}/module/2", forward.local_port)).await.unwrap().text().await.unwrap();
    assert_eq!(page, "export default 2", "the plain listener works on the new session");
    let after = a.state.network.ports.lock().iter().find(|f| f.id == forward.id).unwrap().route_now().unwrap();
    assert!(!Arc::ptr_eq(&before, &after), "the forward was prepared again");
    assert_ne!(before.endpoint.port, after.endpoint.port);
    let socket = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let tls = connector.connect("localhost".try_into().unwrap(), socket).await.unwrap();
    let (mut again, connection) =
        hyper::client::conn::http2::handshake(hyper_util::rt::TokioExecutor::new(), hyper_util::rt::TokioIo::new(tls)).await.unwrap();
    tokio::spawn(connection);
    let response = again
        .send_request(hyper::Request::get(format!("https://localhost:{port}/module/3")).body(Empty::<Bytes>::new()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "HTTPS works on the new session");

    // Stopping the forward ends the HTTPS listener too.
    client.call::<PortsStop>(PortsStopParams { id: forward.id }).await.unwrap();
    let end = tokio::time::timeout(Duration::from_secs(3), websocket.next()).await.expect("the WebSocket did not end");
    assert!(end.is_none_or(|frame| frame.is_err() || matches!(frame, Ok(Message::Close(_)))));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
        assert!(tokio::time::Instant::now() < deadline, "the HTTPS listener closed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    dev.abort();
}

#[test]
fn a_member_at_its_stream_limit_is_refused_at_once_and_its_other_requests_keep_running() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_stack_size(crate::RUNTIME_THREAD_STACK)
        .enable_all()
        .build()
        .unwrap()
        .block_on(run_full_stream_limit());
}

async fn run_full_stream_limit() {
    use crate::network::relay::MAX_INBOUND_PER_MEMBER;
    let (a, b) = direct_pair().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_port = listener.local_addr().unwrap().port();
    let dev = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    // `a` already holds every stream it may hold on `b` (warm direct sockets and relayed connections).
    let key = format!("inbound:{}", a.id());
    let mut places: Vec<_> =
        (0..MAX_INBOUND_PER_MEMBER).map(|_| b.state.network.relays.acquire_within(&key, MAX_INBOUND_PER_MEMBER).unwrap()).collect();
    let link = a.state.network.peers.link(&b.id()).unwrap();
    // More opens than the session has request places (16). Each used to wait 10 s for a stream place while holding one,
    // and everything else on the session queued behind them.
    let started = std::time::Instant::now();
    let opens = (0..24u64).map(|n| {
        let link = link.clone();
        async move { link.call::<PeerPortsOpen>(PeerPortsOpenParams { stream_id: 1_000_001 + 2 * n, port: remote_port }).await }
    });
    let results = futures::future::join_all(opens).await;
    for result in &results {
        assert_eq!(result.as_ref().unwrap_err().code, codes::CONFLICT, "{result:?}");
    }
    let prepared =
        link.call::<PeerPortsPrepare>(PeerPortsPrepareParams { forward_id: Uuid::now_v7(), port: remote_port, direct_version: Some(2) });
    assert!(prepared.await.is_ok(), "another request of the session still runs");
    assert!(started.elapsed() < Duration::from_secs(3), "the session waited {:?}", started.elapsed());
    // A freed place is used by the next open.
    drop(places.pop());
    link.call::<PeerPortsOpen>(PeerPortsOpenParams { stream_id: 2_000_001, port: remote_port }).await.unwrap();
    drop(places);
    dev.abort();
}

#[test]
fn a_direct_listener_checks_a_bounded_number_of_connections_at_once() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_stack_size(crate::RUNTIME_THREAD_STACK)
        .enable_all()
        .build()
        .unwrap()
        .block_on(run_handshake_cap());
}

async fn run_handshake_cap() {
    use crate::network::ports::direct;
    use tokio::io::AsyncReadExt;
    let (a, b) = direct_pair().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_port = listener.local_addr().unwrap().port();
    let link = a.state.network.peers.link(&b.id()).unwrap();
    let prepared = link
        .call::<PeerPortsPrepare>(PeerPortsPrepareParams { forward_id: Uuid::now_v7(), port: remote_port, direct_version: Some(2) })
        .await
        .unwrap();
    let endpoint = prepared.direct.expect("a direct listener on the fake tailnet");
    let destination = direct::destination(&a.state.network, &b.id(), endpoint.port).await.unwrap();
    // Connections that never send a token hold every place under the cap (until the 3 s handshake limit).
    let mut idle = Vec::new();
    for _ in 0..32 {
        idle.push(tokio::net::TcpStream::connect(destination).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    // One more is dropped at once instead of waiting out a handshake.
    let mut over = tokio::net::TcpStream::connect(destination).await.unwrap();
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(2), over.read(&mut byte)).await.expect("dropped at once, not after a handshake");
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
    // Once they are gone, a real caller gets through.
    drop(idle);
    let mut ready = None;
    for _ in 0..40 {
        ready = direct::connect(&a.state.network, destination, &endpoint).await;
        if ready.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ready.is_some(), "an authorized connection after the idle ones closed");
}
