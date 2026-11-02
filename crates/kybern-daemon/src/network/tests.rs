//! Transport scenarios on the in-process multi-daemon harness (design §11.1 N3 acceptance, §11.3):
//! discovery, the identity check, the handshake, grants at use time, heartbeat and backoff, the
//! network listener and the relay.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{Method, Uri};
use axum::response::IntoResponse;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use futures::{SinkExt, StreamExt};
use kybern_protocol::methods::*;
use kybern_protocol::*;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::harness::*;
use super::keys::MachineKeys;
use super::peers::{self, Direction, FrameSink, PeerLink};
use super::session::{self, SessionError, SessionRequest, Target};
use super::well_known::{self, Expected, ProbeError};
use super::{Timing, ops};

const WS: tokio_tungstenite::tungstenite::http::HeaderName = tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;

fn three() -> [NodeSpec; 3] {
    [NodeSpec::owner("a"), NodeSpec::owner("b"), NodeSpec::owner("c")]
}

/// `a` founds the network and adds `b` and `c`, all with their daemons and workers running.
async fn trio(cluster: &Cluster) -> (Node, Node, Node) {
    let a = cluster.start("a", true).await;
    let b = cluster.start("b", true).await;
    let c = cluster.start("c", true).await;
    cluster.form(&a, &[&b, &c], JoinVia::Auto).await;
    (a, b, c)
}

async fn all_linked(nodes: &[&Node]) {
    for (index, first) in nodes.iter().enumerate() {
        for second in &nodes[index + 1..] {
            eventually(&format!("{} and {} linked", first.name, second.name), WAIT, || linked(first, second)).await;
        }
    }
}

/// Make the node's network listener ready without the worker.
async fn listen(node: &Node) {
    crate::exposure::apply_network(&node.state, true).await;
    assert!(node.state.network.listening_port().is_some(), "{} did not listen: {:?}", node.name, node.state.network.listen_info());
}

fn target_for(cluster: &Cluster, node: &Node, port: u16) -> Target {
    Target {
        machine_id: node.id(),
        signing_key: node.membership().keys().signing_public_b64(),
        ts_ip: cluster.fake(&node.name).ip,
        port,
        url: None,
    }
}

// ---- discovery and the listener ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn members_find_each_other_link_and_converge_on_new_operations() {
    let Some(cluster) = Cluster::new(&three()) else { return };
    let (a, b, c) = trio(&cluster).await;
    all_linked(&[&a, &b, &c]).await;
    for node in [&a, &b, &c] {
        assert_eq!(node.state.network.listening_port(), Some(4173), "{} listens on the network port", node.name);
    }

    // The member with the smaller id dialed each pair.
    for (x, y) in [(&a, &b), (&a, &c), (&b, &c)] {
        let (dialer, listener) = if x.id() < y.id() { (x, y) } else { (y, x) };
        assert_eq!(dialer.state.network.peers.link(&listener.id()).unwrap().direction, Direction::Dialed);
        assert_eq!(listener.state.network.peers.link(&dialer.id()).unwrap().direction, Direction::Accepted);
    }

    // An operation written on one member spreads to the others (peer.ops.push, re-gossiped once).
    let op = b.membership().set("follow_updates", json!(true)).unwrap();
    b.state.network.publish_ops(vec![op]);
    eventually("the register reaches everyone", WAIT, || {
        a.membership().projection().follow_updates() && c.membership().projection().follow_updates()
    })
    .await;
    assert_eq!(a.membership().digest(), b.membership().digest());
    assert_eq!(b.membership().digest(), c.membership().digest());

    // A sync on open also repairs a member that missed operations: c is told while its links are down.
    c.state.network.peers.close_all();
    let missed = a.membership().set("follow_updates", json!(false)).unwrap();
    let _ = missed;
    eventually("c catches up when its link returns", WAIT, || !c.membership().projection().follow_updates() && linked(&a, &c)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_members_on_the_tailnet_are_found_with_the_identity_whois_reports() {
    let specs = [
        NodeSpec::owner("a"),
        NodeSpec::owner("b"),
        NodeSpec::owner("c"),
        NodeSpec::tagged("v"),
        NodeSpec::shared_in("s"),
        NodeSpec::other_user("o"),
    ];
    let Some(mut cluster) = Cluster::new(&specs) else { return };
    let a = cluster.start("a", true).await;
    let b = cluster.start("b", true).await;
    let others =
        [cluster.start("c", true).await, cluster.start("v", true).await, cluster.start("s", true).await, cluster.start("o", true).await];
    cluster.form(&a, &[&b], JoinVia::Auto).await;
    eventually("everyone not a member is found", WAIT, || {
        let found = a.state.network.peers.found();
        ["c", "v", "s", "o"].iter().all(|name| found.iter().any(|entry| entry.node.name() == *name))
            && found.iter().all(|entry| entry.node.name() != "b")
    })
    .await;

    let found = a.state.network.peers.found();
    let entry = |name: &str| found.iter().find(|entry| entry.node.name() == name).unwrap_or_else(|| panic!("{name} not found"));
    assert!(found.iter().all(|entry| entry.node.name() != "b"), "a member is never listed as found");
    let owner = entry("c");
    assert_eq!(owner.descriptor.machine_id, others[0].id());
    assert_eq!(owner.port, 4173);
    assert!(owner.whois.as_ref().unwrap().is_owner(OWNER), "the same user, untagged: auto-join material");
    assert!(entry("v").whois.as_ref().unwrap().tagged(), "a tagged node is a one-click Add");
    assert!(entry("s").whois.as_ref().unwrap().shared_in, "a shared-in node needs a code");
    assert!(!entry("o").whois.as_ref().unwrap().is_owner(OWNER), "another user's node is not the owner's");

    // A node that leaves the tailnet drops out of the list.
    cluster.set_online("c", false);
    a.state.network.wake();
    eventually("c is no longer found", WAIT, || a.state.network.peers.found().iter().all(|entry| entry.node.name() != "c")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_network_listener_binds_the_tailscale_address_beside_the_primary_and_follows_the_setting() {
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a")]) else { return };
    let a = cluster.start("a", true).await;
    eventually("the listener is up", WAIT, || a.state.network.listening_port() == Some(4173)).await;
    let network_addr = a.state.network.listen_info().addr.unwrap();
    assert_eq!(network_addr.ip(), a.listen_ip);
    assert_ne!(network_addr, a.primary, "it never takes the primary listener's address");
    // Both serve the same routes.
    for base in [a.http_url(), a.network_base(4173)] {
        let response = reqwest::get(format!("{base}/health")).await.unwrap();
        assert_eq!(response.text().await.unwrap(), "ok");
    }
    let exposure = crate::access::exposure(&a.state).await;
    assert!(exposure.listeners.contains(&network_addr.to_string()));

    // Off: the listener closes and the session endpoint is gone.
    a.set_network_enabled(false);
    eventually("the listener is closed", WAIT, || a.state.network.listening_port().is_none()).await;
    assert!(reqwest::get(format!("{}/health", a.network_base(4173))).await.is_err());
    // On again: it comes back.
    a.set_network_enabled(true);
    eventually("the listener returns", WAIT, || a.state.network.listening_port() == Some(4173)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_port_moves_the_listener_to_the_next_one_and_peers_still_find_it() {
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b")]) else { return };
    // Something that is not Kybern holds 4173 on b's address.
    let squatter = std::net::TcpListener::bind((cluster.listen_ip("b"), 4173)).unwrap();
    let a = cluster.start("a", true).await;
    let b = cluster.start("b", true).await;
    eventually("b listens on the next port", WAIT, || b.state.network.listening_port() == Some(4174)).await;
    assert_eq!(b.state.network.self_info().await.port, Some(4174), "the descriptor publishes the port it got");
    eventually("a finds b on 4174", WAIT, || a.state.network.peers.found().iter().any(|entry| entry.port == 4174)).await;
    drop(squatter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_daemons_on_one_computer_fail_clearly_and_the_first_is_unharmed() {
    let Some(mut cluster) = Cluster::new(&[NodeSpec::owner("x"), NodeSpec::owner("y")]) else { return };
    cluster.share_host("y", "x");
    let x = cluster.start("x", true).await;
    eventually("x listens", WAIT, || x.state.network.listening_port() == Some(4173)).await;
    let y = cluster.start("y", true).await;
    eventually("y reports why it cannot listen", WAIT, || y.state.network.listen_info().error.is_some()).await;
    let info = y.state.network.listen_info();
    assert!(info.addr.is_none());
    let error = info.error.unwrap();
    assert!(error.contains("Another Kybern daemon on this computer"), "unclear error: {error}");
    assert!(error.contains(&x.id()), "the error names the machine that holds the port: {error}");
    // x still answers, as itself.
    let answer = well_known::probe(&x.state.network.http, &x.network_base(4173), None, Duration::from_secs(3), Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(answer.machine_id, x.id());
    assert_eq!(x.state.network.listening_port(), Some(4173));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_port_taken_is_an_error_that_says_so() {
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a")]) else { return };
    let squatters: Vec<_> = (4173..=4179).map(|port| std::net::TcpListener::bind((cluster.listen_ip("a"), port)).unwrap()).collect();
    let a = cluster.start("a", true).await;
    eventually("the error is reported", WAIT, || a.state.network.listen_info().error.is_some()).await;
    let error = a.state.network.listen_info().error.unwrap();
    assert!(error.contains("4173") && error.contains("4179") && error.contains("in use"), "{error}");
    drop(squatters);
}

// ---- the well-known endpoint ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_well_known_endpoint_proves_the_machine_and_limits_its_callers() {
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b")]) else { return };
    let a = cluster.start("a", false).await;
    let b = cluster.start("b", false).await;
    listen(&a).await;
    let base = a.network_base(4173);
    let key = a.membership().keys().signing_public_b64();
    let expected = Expected { machine_id: &a.id(), signing_key: &key };
    let answer =
        well_known::probe(&b.state.network.http, &base, Some(expected), Duration::from_secs(3), Duration::from_secs(60)).await.unwrap();
    assert_eq!(answer.name, crate::rpc::hostname());
    assert_eq!(answer.network_protocol, NETWORK_PROTOCOL);
    assert!(answer.features.iter().any(|feature| feature == features::NETWORK_V1));
    assert!(answer.features.iter().any(|feature| feature == features::RELAY_V1));
    assert_eq!(answer.sealing_key, a.membership().keys().sealing_public_b64());

    // A different expected key is refused before the proof is even looked at.
    let other = MachineKeys::generate().signing_public_b64();
    let wrong = Expected { machine_id: &a.id(), signing_key: &other };
    let refused = well_known::probe(&b.state.network.http, &base, Some(wrong), Duration::from_secs(3), Duration::from_secs(60)).await;
    assert_eq!(refused.unwrap_err(), ProbeError::WrongMachine);

    // The nonce is required, and the answer cannot be reused for another nonce.
    let missing = b.state.network.http.get(format!("{base}{}", well_known::PATH)).send().await.unwrap();
    assert_eq!(missing.status(), 400);
    let nonce = well_known::new_nonce();
    let first: well_known::WellKnown =
        b.state.network.http.get(format!("{base}{}?nonce={nonce}", well_known::PATH)).send().await.unwrap().json().await.unwrap();
    assert_eq!(
        well_known::verify(&first, &well_known::new_nonce(), well_known::authority(&base), None, Duration::from_secs(60)),
        Err(ProbeError::BadProof)
    );

    // 30 a minute per source address; b's source is a non-loopback address, so it is limited.
    let mut limited = false;
    for _ in 0..40 {
        let response =
            b.state.network.http.get(format!("{base}{}?nonce={}", well_known::PATH, well_known::new_nonce())).send().await.unwrap();
        limited |= response.status() == 429;
    }
    assert!(limited, "the 31st request in a minute is refused");
}

// ---- the handshake ----

fn signed_request(from: &Node, to: &Node, scopes: &[Scope]) -> SessionRequest {
    let mut request = SessionRequest {
        from: from.id(),
        to: to.id(),
        nonce_a: well_known::new_nonce(),
        at: well_known::now_rfc3339(),
        scopes: scopes.iter().map(|scope| scope.as_str().to_string()).collect(),
        renews: None,
        sig: String::new(),
    };
    request.sig = B64.encode(from.membership().keys().sign(&session::request_message(&request)));
    request
}

async fn post_session(from: &Node, to: &Node, request: &SessionRequest) -> reqwest::Response {
    from.state.network.http.post(format!("{}{}", to.network_base(4173), session::SESSION_PATH)).json(request).send().await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_handshake_refuses_replays_forgeries_strangers_and_stale_requests() {
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b"), NodeSpec::owner("c")]) else { return };
    let a = cluster.start("a", false).await;
    let b = cluster.start("b", false).await;
    let c = cluster.start("c", false).await;
    cluster.form(&a, &[&b], JoinVia::Auto).await;
    listen(&a).await;

    // A genuine request is granted: peer, plus what a's policy allows.
    let request = signed_request(&b, &a, &Scope::MACHINE_DEFAULT);
    let granted = post_session(&b, &a, &request).await;
    assert_eq!(granted.status(), 200);
    let granted: session::SessionResponse = granted.json().await.unwrap();
    assert!(granted.grant.as_deref().is_some_and(|grant| grant.starts_with("kyb_m_")));
    assert!(granted.scopes.contains(&"machine:peer".to_string()) && granted.scopes.contains(&"machine:relay".to_string()));
    assert!(!granted.scopes.contains(&"machine:clipboard_auto".to_string()), "the universal clipboard is opt-in");

    // The same bytes again are a replay.
    let replay = post_session(&b, &a, &request).await;
    assert_eq!(replay.status(), 401);
    assert!(replay.text().await.unwrap().contains("already used"));

    // A request for another machine, a bad signature, a stale time and a non-member are all refused.
    let mut wrong_target = signed_request(&b, &a, &[Scope::MachinePeer]);
    wrong_target.to = c.id();
    wrong_target.sig = B64.encode(b.membership().keys().sign(&session::request_message(&wrong_target)));
    assert_eq!(post_session(&b, &a, &wrong_target).await.status(), 400);

    let mut forged = signed_request(&b, &a, &[Scope::MachinePeer]);
    forged.sig = B64.encode(MachineKeys::generate().sign(&session::request_message(&forged)));
    assert_eq!(post_session(&b, &a, &forged).await.status(), 401);

    let mut tampered = signed_request(&b, &a, &[Scope::MachinePeer]);
    tampered.scopes.push("machine:files".into());
    assert_eq!(post_session(&b, &a, &tampered).await.status(), 401, "the scopes are signed");

    let mut stale = signed_request(&b, &a, &[Scope::MachinePeer]);
    stale.at = (chrono::Utc::now() - chrono::Duration::seconds(300)).to_rfc3339();
    stale.sig = B64.encode(b.membership().keys().sign(&session::request_message(&stale)));
    assert_eq!(post_session(&b, &a, &stale).await.status(), 401);

    // c is not a member: its signature is good for its own key, but a has never heard of it.
    let stranger = signed_request(&c, &a, &[Scope::MachinePeer]);
    assert_eq!(post_session(&c, &a, &stranger).await.status(), 403);

    // A member's request from an address that is not its tailnet node is refused too (b signs,
    // but the call comes from a plain client on 127.0.0.1).
    let from_elsewhere = signed_request(&b, &a, &[Scope::MachinePeer]);
    let response = reqwest::Client::new()
        .post(format!("{}{}", a.network_base(4173), session::SESSION_PATH))
        .json(&from_elsewhere)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    // c claiming to be b is caught by the signature before the address is even considered.
    let mut impersonation = signed_request(&c, &a, &[Scope::MachinePeer]);
    impersonation.from = b.id();
    assert_eq!(post_session(&c, &a, &impersonation).await.status(), 401);
}

/// A server that answers the well-known check as a machine with the wrong key and records
/// everything it is sent.
async fn impostor(
    ip: std::net::IpAddr,
    claims: &str,
    claimed_key: Option<String>,
) -> (Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    #[derive(Clone)]
    struct Impostor {
        seen: Arc<Mutex<Vec<String>>>,
        keys: Arc<MachineKeys>,
        claims: String,
        claimed_key: Option<String>,
    }
    async fn any(State(me): State<Impostor>, method: Method, uri: Uri, headers: axum::http::HeaderMap) -> axum::response::Response {
        me.seen.lock().unwrap().push(format!("{method} {}", uri.path()));
        if headers.contains_key("authorization") {
            me.seen.lock().unwrap().push("AUTHORIZATION HEADER".into());
        }
        if uri.path() != well_known::PATH {
            return (axum::http::StatusCode::NOT_FOUND, "no").into_response();
        }
        let query: std::collections::HashMap<String, String> =
            uri.query().unwrap_or_default().split('&').filter_map(|pair| pair.split_once('=')).map(|(k, v)| (k.into(), v.into())).collect();
        let nonce = query.get("nonce").cloned().unwrap_or_default();
        let host = headers.get("host").and_then(|h| h.to_str().ok()).unwrap_or_default().to_string();
        let at = well_known::now_rfc3339();
        let proof = B64.encode(me.keys.sign(&well_known::proof_message(&nonce, &me.claims, &at, &host)));
        // `Connection: close`: an aborted impostor's kept-alive socket would otherwise answer the
        // next probe in its place (aborting `serve` does not end its open connections).
        let close = [(axum::http::header::CONNECTION, "close")];
        (
            close,
            axum::Json(json!({
                "machine_id": me.claims, "name": "impostor", "os": "linux", "arch": "x86_64", "version": "0", "network_protocol": 1,
                "features": [], "signing_key": me.claimed_key.clone().unwrap_or_else(|| me.keys.signing_public_b64()),
                "sealing_key": me.keys.sealing_public_b64(), "network_id": null, "ts_name": null, "at": at, "proof": proof
            })),
        )
            .into_response()
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    let me = Impostor { seen: seen.clone(), keys: Arc::new(MachineKeys::generate()), claims: claims.to_string(), claimed_key };
    let app = Router::new().fallback(any).with_state(me);
    // The previous impostor on this address may still be closing its socket.
    let mut attempts = 0;
    let listener = loop {
        match tokio::net::TcpListener::bind((ip, 4173)).await {
            Ok(listener) => break listener,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse && attempts < 100 => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("could not bind the impostor: {error}"),
        }
    };
    (seen, tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_key_is_refused_before_any_credential_is_sent() {
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b")]) else { return };
    let a = cluster.start("a", false).await;
    let b = cluster.start("b", false).await;
    cluster.form(&a, &[&b], JoinVia::Auto).await;
    let target = target_for(&cluster, &b, 4173);
    let scopes = Scope::MACHINE_DEFAULT;

    // Another machine's key behind b's id and address.
    let (seen, server) = impostor(cluster.listen_ip("b"), &b.id(), None).await;
    let error = session::open(&a.state.network, a.membership(), &target, &scopes, None).await.unwrap_err();
    assert!(matches!(error, SessionError::Identity(ProbeError::WrongMachine)), "{error}");
    assert_eq!(seen.lock().unwrap().clone(), [format!("GET {}", well_known::PATH)], "nothing but the identity check was sent");
    server.abort();
    let _ = server.await;

    // b's real public key, but a proof only the impostor could make.
    let (seen, server) = impostor(cluster.listen_ip("b"), &b.id(), Some(b.membership().keys().signing_public_b64())).await;
    let error = session::open(&a.state.network, a.membership(), &target, &scopes, None).await.unwrap_err();
    assert!(matches!(error, SessionError::Identity(ProbeError::BadProof)), "{error}");
    assert_eq!(seen.lock().unwrap().clone(), [format!("GET {}", well_known::PATH)]);
    assert!(!seen.lock().unwrap().iter().any(|line| line.contains("AUTHORIZATION")));
    server.abort();
    let _ = server.await;

    // And a daemon that dials never gets as far as a socket: the peer manager records the failure.
    let (_seen, server) = impostor(cluster.listen_ip("b"), &b.id(), None).await;
    let (dialer, _) = if a.id() < b.id() { (&a, &b) } else { (&b, &a) };
    if dialer.name == "a" {
        dialer.pass().await;
        let status = dialer.state.network.peers.status(&b.id());
        assert!(!status.online);
        assert!(status.last_error.is_some_and(|error| error.contains("different machine")));
    }
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_renews_in_place_for_the_same_machine_only() {
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b"), NodeSpec::owner("c")]) else { return };
    let a = cluster.start("a", false).await;
    let b = cluster.start("b", false).await;
    let c = cluster.start("c", false).await;
    cluster.form(&a, &[&b, &c], JoinVia::Auto).await;
    listen(&a).await;
    let target = target_for(&cluster, &a, 4173);
    let opened = session::open(&b.state.network, b.membership(), &target, &Scope::MACHINE_DEFAULT, None).await.unwrap();
    assert!(!opened.token.is_empty());
    let before = a.state.network.grants.held().into_iter().find(|grant| grant.id.to_string() == opened.grant_id).unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let renewed = session::open(&b.state.network, b.membership(), &target, &Scope::MACHINE_DEFAULT, Some(&opened.grant_id)).await.unwrap();
    assert_eq!(renewed.grant_id, opened.grant_id);
    assert!(renewed.token.is_empty(), "a renewal keeps the holder's token");
    let after = a.state.network.grants.held().into_iter().find(|grant| grant.id.to_string() == opened.grant_id).unwrap();
    assert!(after.expires_at > before.expires_at, "the expiry moved");
    // Another member cannot renew b's grant.
    let stolen = session::open(&c.state.network, c.membership(), &target, &Scope::MACHINE_DEFAULT, Some(&opened.grant_id)).await;
    assert!(matches!(stolen, Err(SessionError::Refused { status: 401, .. })), "{stolen:?}");
}

// ---- grants at use time ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_machine_loses_its_session_and_cannot_open_another() {
    let Some(cluster) = Cluster::new(&three()) else { return };
    let (a, b, c) = trio(&cluster).await;
    all_linked(&[&a, &b, &c]).await;

    // b's grant is live on a (or a's on b); b is removed on c and the removal gossips.
    let removal = c.membership().remove(&b.id(), RemoveReason::User).unwrap();
    c.state.network.publish_ops(vec![removal]);
    c.state.network.drop_machine(&b.id());
    // The links can go before the removal reaches a (b closes its own on learning of it), so wait
    // for the removal itself, then for what it takes with it.
    eventually("the removal reaches a", WAIT, || !a.membership().is_member(&b.id())).await;
    eventually("a closes b's session and drops its grants", WAIT, || {
        a.state.network.peers.link(&b.id()).is_none() && a.state.network.grants.held().iter().all(|grant| grant.machine_id != b.id())
    })
    .await;
    eventually("c closes b's session", WAIT, || c.state.network.peers.link(&b.id()).is_none()).await;
    // A new request from b is refused.
    listen(&a).await;
    let request = signed_request(&b, &a, &[Scope::MachinePeer]);
    assert_eq!(post_session(&b, &a, &request).await.status(), 403);
    // And the removed machine is not dialed again.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(a.state.network.peers.link(&b.id()).is_none() && c.state.network.peers.link(&b.id()).is_none());
}

/// A session that was opening when `remove` pushed the operation is closed by `drop_machine` (through
/// `removed_while_opening`) before the push reaches the removed machine. The members refuse it every
/// session after that, so the closing session is the last place it can learn it was removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_machine_learns_it_was_removed_from_the_session_being_closed() {
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b")]) else { return };
    let (a, b) = (cluster.start("a", true).await, cluster.start("b", true).await);
    cluster.form(&a, &[&b], JoinVia::Auto).await;
    eventually("linked", WAIT, || linked(&a, &b)).await;

    // The removal is written on a and never pushed: only the closing session can carry it.
    a.membership().remove(&b.id(), RemoveReason::User).unwrap();
    a.state.network.drop_machine(&b.id());
    assert!(a.state.network.peers.link(&b.id()).is_none(), "the link leaves the table at once");
    eventually("b learns it was removed", WAIT, || b.membership().self_removed()).await;
    eventually("the session with b ends", WAIT, || b.state.network.peers.link(&a.id()).is_none()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_session_is_refused_at_use_time_after_removal_or_pause() {
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b")]) else { return };
    let (a, b) = (cluster.start("a", true).await, cluster.start("b", true).await);
    cluster.form(&a, &[&b], JoinVia::Auto).await;
    eventually("linked", WAIT, || linked(&a, &b)).await;
    let (dialer, listener) = if a.id() < b.id() { (&a, &b) } else { (&b, &a) };
    let link = dialer.state.network.peers.link(&listener.id()).unwrap();
    let sync = || PeerOpsSyncParams { digest: String::new(), ids: vec![] };

    // The session works before.
    link.call::<PeerOpsSync>(sync()).await.unwrap();
    // Paused (a register the listener's owner sees): only machine:peer is left, so the relay is cut
    // while op sync still works. The pause is written on the listener and kept there, not gossiped,
    // so the dialer does not drop the session on its own.
    let pause = listener.membership().set(&format!("machine.{}.paused", dialer.id()), json!(true)).unwrap();
    let _ = pause;
    link.call::<PeerOpsSync>(sync()).await.unwrap();
    let relay = PeerRelayOpenParams { stream_id: 1, kind: RelayStreamKind::Http, path: "/health".into(), method: None, headers: vec![] };
    let refused = link.call::<PeerRelayOpen>(relay).await.unwrap_err();
    assert_eq!(refused.code, codes::FORBIDDEN, "{refused:?}");

    // Removed (kept on the listener only, with no drop_machine): every call is refused. The
    // listener's periodic membership check may close the socket before this request reaches it.
    // Then the call fails as a closed session, possibly before this side has noticed the close
    // (the send fails first), so wait for the close itself rather than expect it already.
    let removal = listener.membership().remove(&dialer.id(), RemoveReason::User).unwrap();
    let _ = removal;
    let refused = link.call::<PeerOpsSync>(sync()).await.unwrap_err();
    if refused.code != codes::FORBIDDEN {
        assert_eq!(refused.code, codes::INTERNAL_ERROR, "removal must forbid the call or close its session: {refused:?}");
        tokio::time::timeout(WAIT, link.closed())
            .await
            .unwrap_or_else(|_| panic!("a call refused as closed closes its session: {refused:?}"));
    }
    // Even if the request won that race and got FORBIDDEN, the next beat closes the socket.
    eventually("the listener closes the socket", WAIT, || link.is_closed() || listener.state.network.peers.link(&dialer.id()).is_none())
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_capability_turned_off_by_its_owner_is_refused_while_the_rest_still_work() {
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b")]) else { return };
    let (a, b) = (cluster.start("a", true).await, cluster.start("b", true).await);
    cluster.form(&a, &[&b], JoinVia::Auto).await;
    eventually("linked", WAIT, || linked(&a, &b)).await;
    let link = b.state.network.peers.link(&a.id()).unwrap();
    // a turns relaying off for everyone: allow.<a>.relay is a register only a can write.
    a.membership().set(&format!("allow.{}.relay", a.id()), json!(false)).unwrap();
    let open = PeerRelayOpenParams { stream_id: 7, kind: RelayStreamKind::Http, path: "/health".into(), method: None, headers: vec![] };
    let refused = link.call::<PeerRelayOpen>(open).await.unwrap_err();
    assert_eq!(refused.code, codes::FORBIDDEN);
    link.call::<PeerOpsSync>(PeerOpsSyncParams { digest: String::new(), ids: vec![] }).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forged_operations_close_the_senders_session() {
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b")]) else { return };
    let (a, b) = (cluster.start("a", true).await, cluster.start("b", true).await);
    cluster.form(&a, &[&b], JoinVia::Auto).await;
    eventually("linked", WAIT, || linked(&a, &b)).await;
    let link = b.state.network.peers.link(&a.id()).unwrap();
    // An operation signed by a key the network has never added.
    let intruder = MachineKeys::generate();
    let network_id = a.membership().network_id().unwrap();
    let body =
        ops::encode_body(&network_id, chrono::Utc::now(), 99, &OpKind::Set { key: "follow_updates".into(), value: json!(true) }).unwrap();
    let forged = ops::seal(&intruder, &uuid::Uuid::now_v7().to_string(), body);
    let _ = link.call::<PeerOpsPush>(PeerOpsPushParams { ops: vec![forged] }).await;
    eventually("a closes the session of the sender", WAIT, || {
        a.state.network.peers.link(&b.id()).is_none()
            || a.state.network.peers.link(&b.id()).unwrap().opened_at.elapsed() < Duration::from_millis(50)
    })
    .await;
    assert!(!a.membership().projection().follow_updates(), "the forged register did not apply");
}

// ---- heartbeat and backoff ----

struct NullSink;

#[async_trait::async_trait]
impl FrameSink for NullSink {
    async fn send(&self, _frame: Value) -> bool {
        true
    }
}

#[tokio::test]
async fn a_link_that_hears_nothing_for_three_pings_is_closed_and_a_talkative_one_is_not() {
    let timing = Timing::fast();
    let silent = PeerLink::new("silent".into(), Direction::Dialed, None, vec![], Arc::new(NullSink));
    let talking = PeerLink::new("talking".into(), Direction::Dialed, None, vec![], Arc::new(NullSink));
    let (tx, mut pings) = tokio::sync::mpsc::channel(8);
    peers::spawn_heartbeat(silent.clone(), timing.clone(), tx.clone());
    peers::spawn_heartbeat(talking.clone(), timing.clone(), tx);
    // Real time: the idle clock is std's. Twenty beats of 150 ms is five times `dead_after`.
    for _ in 0..20 {
        tokio::time::sleep(timing.ping_every / 2).await;
        talking.touch();
        tokio::time::sleep(timing.ping_every / 2).await;
    }
    assert!(silent.is_closed(), "no frame for {:?}", timing.dead_after);
    assert!(!talking.is_closed());
    assert!(pings.try_recv().is_ok(), "pings were sent while the links lived");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quiet_sessions_stay_up_on_pings_alone() {
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b")]) else { return };
    let (a, b) = (cluster.start("a", true).await, cluster.start("b", true).await);
    cluster.form(&a, &[&b], JoinVia::Auto).await;
    eventually("linked", WAIT, || linked(&a, &b)).await;
    let opened = a.state.network.peers.links().into_iter().map(|link| link.opened_at).max().unwrap();
    // Several times dead_after (600 ms) with no traffic but pings: the same link is still there.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(linked(&a, &b));
    assert_eq!(a.state.network.peers.links().into_iter().map(|link| link.opened_at).max().unwrap(), opened, "it was never reconnected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_that_goes_away_is_retried_with_growing_waits_and_found_again() {
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b")]) else { return };
    let (a, b) = (cluster.start("a", true).await, cluster.start("b", true).await);
    cluster.form(&a, &[&b], JoinVia::Auto).await;
    eventually("linked", WAIT, || linked(&a, &b)).await;
    let (dialer, other) = if a.id() < b.id() { (&a, &b) } else { (&b, &a) };

    // The other side turns the network off: its listener closes and its sessions end.
    other.set_network_enabled(false);
    eventually("the session ends", WAIT, || dialer.state.network.peers.link(&other.id()).is_none()).await;
    other.state.exposure.clear_network().await;
    // Reject TCP connections immediately while the peer is unavailable. Windows
    // otherwise spends seconds retrying each of the seven closed discovery ports;
    // this scenario measures peer retries, not the operating system's SYN policy.
    let mut rejectors = tokio::task::JoinSet::new();
    for port in peers::PORT_RANGE {
        let listener = tokio::net::TcpListener::bind(std::net::SocketAddr::new(other.listen_ip, port)).await.unwrap();
        rejectors.spawn(async move {
            while let Ok((connection, _)) = listener.accept().await {
                drop(connection);
            }
        });
    }
    eventually("failed attempts pile up", WAIT, || dialer.state.network.peers.status(&other.id()).failures >= 3).await;
    let status = dialer.state.network.peers.status(&other.id());
    assert!(!status.online && status.last_error.is_some(), "{status:?}");
    // Back on: the dialer finds it again within the capped backoff.
    rejectors.abort_all();
    while rejectors.join_next().await.is_some() {}
    other.set_network_enabled(true);
    eventually("linked again", WAIT, || linked(&a, &b)).await;
}

// ---- the relay ----

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn relay_socket(
    via: &Node,
    target: &Node,
    relay_auth: Option<&str>,
    target_token: Option<&str>,
) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
    let mut request = format!("ws://{}/relay/{}/ws", via.primary, target.id()).into_client_request().unwrap();
    if let Some(token) = relay_auth {
        request.headers_mut().insert(relay_header(), format!("Bearer {token}").parse().unwrap());
    }
    if let Some(token) = target_token {
        request.headers_mut().insert(WS.clone(), format!("Bearer {token}").parse().unwrap());
    }
    tokio_tungstenite::connect_async(request).await.map(|(socket, _)| socket)
}

fn relay_header() -> tokio_tungstenite::tungstenite::http::HeaderName {
    tokio_tungstenite::tungstenite::http::HeaderName::from_static(super::relay::RELAY_AUTH_HEADER)
}

async fn rpc(socket: &mut Socket, id: i64, method: &str, params: Value) -> Value {
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
async fn a_client_reaches_b_through_a_and_b_sees_the_device_not_the_relay() {
    let Some(cluster) = Cluster::new(&three()) else { return };
    let (a, b, c) = trio(&cluster).await;
    all_linked(&[&a, &b, &c]).await;
    // The client is paired with a, and holds a token for b that it got through a (design §3.4).
    let token_at_a = a.device_token(&Scope::CLIENT);
    let token_at_b = b.device_token(&Scope::CLIENT);

    let mut socket = relay_socket(&a, &b, Some(&token_at_a), Some(&token_at_b)).await.unwrap();
    let info = rpc(&mut socket, 1, "daemon.info", json!({})).await;
    let result = &info["result"];
    assert_eq!(result["environment_id"], b.id(), "the answer is b's, not a's");
    let scopes: Vec<String> = serde_json::from_value(result["scopes"].clone()).unwrap();
    let mut expected: Vec<String> =
        Scope::CLIENT.iter().map(|scope| serde_json::to_value(scope).unwrap().as_str().unwrap().to_string()).collect();
    expected.sort();
    let mut scopes = scopes;
    scopes.sort();
    assert_eq!(scopes, expected, "b sees the client's device token, never a machine grant");
    assert!(scopes.iter().all(|scope| !scope.starts_with("machine_")));
    assert_eq!(b.state.connections.load(std::sync::atomic::Ordering::SeqCst), 1, "b counts one client connection");
    assert_eq!(a.state.connections.load(std::sync::atomic::Ordering::SeqCst), 0, "a is only a pipe");

    // A device scope check applies on b: a read-only token cannot do what it may not.
    let read_only = b.device_token(&[Scope::OrchestrationRead]);
    let mut limited = relay_socket(&a, &b, Some(&token_at_a), Some(&read_only)).await.unwrap();
    let denied = rpc(&mut limited, 2, "daemon.shutdown", json!({})).await;
    assert!(denied["error"].is_object(), "{denied}");

    // Several messages in a row come back in order, and a large one passes whole.
    for id in 10..20 {
        assert_eq!(rpc(&mut socket, id, "daemon.info", json!({})).await["id"], id);
    }
    socket.close(None).await.unwrap();
    limited.close(None).await.unwrap();
    eventually("the stream is released", WAIT, || a.state.network.relays.open_streams() == 0 && b.state.network.relays.open_streams() == 0)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_relay_checks_both_credentials_and_the_route() {
    let Some(cluster) = Cluster::new(&three()) else { return };
    let (a, b, c) = trio(&cluster).await;
    all_linked(&[&a, &b, &c]).await;
    let token_at_a = a.device_token(&Scope::CLIENT);
    let token_at_b = b.device_token(&Scope::CLIENT);
    let status = |error: tokio_tungstenite::tungstenite::Error| match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => response.status().as_u16(),
        other => panic!("not an HTTP refusal: {other}"),
    };
    // No credential for the relay member.
    assert_eq!(status(relay_socket(&a, &b, None, Some(&token_at_b)).await.err().unwrap()), 401);
    // A token a does not know.
    assert_eq!(status(relay_socket(&a, &b, Some("kyb_nope"), Some(&token_at_b)).await.err().unwrap()), 401);
    // A token without orchestration:read.
    let no_read = a.device_token(&[Scope::NetworkRead]);
    assert_eq!(status(relay_socket(&a, &b, Some(&no_read), Some(&token_at_b)).await.err().unwrap()), 403);
    // Relay accepted, but b rejects the credential it was sent: end to end, a cannot vouch for the client.
    assert_eq!(status(relay_socket(&a, &b, Some(&token_at_a), Some("kyb_forged")).await.err().unwrap()), 401);
    assert_eq!(status(relay_socket(&a, &b, Some(&token_at_a), None).await.err().unwrap()), 401);
    // A token for a is no good on b, even though a vouched for the connection.
    assert_eq!(status(relay_socket(&a, &b, Some(&token_at_a), Some(&token_at_a)).await.err().unwrap()), 401);
    // Not a member, and the member itself.
    let mut request = format!("ws://{}/relay/{}/ws", a.primary, uuid::Uuid::now_v7()).into_client_request().unwrap();
    request.headers_mut().insert(relay_header(), format!("Bearer {token_at_a}").parse().unwrap());
    assert_eq!(status(tokio_tungstenite::connect_async(request).await.err().unwrap()), 404);
    assert_eq!(status(relay_socket(&a, &a, Some(&token_at_a), Some(&token_at_a)).await.err().unwrap()), 400);
    // No session with the target: 502.
    b.state.network.peers.close_all();
    a.state.network.peers.close(&b.id());
    a.state.network.drop_machine(&b.id());
    let gone = {
        let cut = a.membership().set(&format!("machine.{}.paused", b.id()), json!(true)).unwrap();
        a.state.network.publish_ops(vec![cut]);
        relay_socket(&a, &b, Some(&token_at_a), Some(&token_at_b)).await
    };
    assert!([502, 404].contains(&status(gone.err().unwrap())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_relay_carries_the_identity_check_and_large_bodies_with_flow_control() {
    let Some(cluster) = Cluster::new(&three()) else { return };
    let (a, b, c) = trio(&cluster).await;
    all_linked(&[&a, &b, &c]).await;
    let token_at_a = a.device_token(&Scope::CLIENT);
    let token_at_b = b.device_token(&Scope::CLIENT);
    let client = reqwest::Client::new();
    let base = format!("{}/relay/{}/http", a.http_url(), b.id());

    // The well-known proof covers the Host the client asked for (a's address), and b's key.
    let nonce = well_known::new_nonce();
    let response = client
        .get(format!("{base}{}?nonce={nonce}", well_known::PATH))
        .header(super::relay::RELAY_AUTH_HEADER, format!("Bearer {token_at_a}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let answer: well_known::WellKnown = response.json().await.unwrap();
    let key = b.membership().keys().signing_public_b64();
    well_known::verify(
        &answer,
        &nonce,
        &a.primary.to_string(),
        Some(Expected { machine_id: &b.id(), signing_key: &key }),
        Duration::from_secs(60),
    )
    .unwrap();
    assert_ne!(answer.machine_id, a.id());

    // A body larger than the 256 KiB window goes up and comes back byte for byte.
    let payload: Vec<u8> = (0..700_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    let operator = b.device_token(&[Scope::OrchestrationOperate, Scope::OrchestrationRead]);
    let uploaded = client
        .post(format!("{base}/assets"))
        .header(super::relay::RELAY_AUTH_HEADER, format!("Bearer {token_at_a}"))
        .header("authorization", format!("Bearer {operator}"))
        .header("x-kybern-filename", "blob.bin")
        .header("content-type", "application/octet-stream")
        .body(payload.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(uploaded.status(), 200, "{}", uploaded.text().await.unwrap_or_default());
    let info: Value = uploaded.json().await.unwrap();
    assert_eq!(info["size"], payload.len());
    let id = info["id"].as_str().unwrap();
    let fetched = client
        .get(format!("{base}/assets/{id}"))
        .header(super::relay::RELAY_AUTH_HEADER, format!("Bearer {token_at_a}"))
        .header("authorization", format!("Bearer {token_at_b}"))
        .send()
        .await
        .unwrap();
    assert_eq!(fetched.status(), 200);
    let fetched = fetched.bytes().await.unwrap();
    assert_eq!(fetched.len(), payload.len(), "every byte came back");
    let first_difference = fetched.iter().zip(&payload).position(|(got, sent)| got != sent);
    assert_eq!(first_difference, None, "the bytes survived both directions");
    // It is on b, not on a.
    assert!(a.state.store.asset_get(id.parse().unwrap()).unwrap().is_none());
    assert!(b.state.store.asset_get(id.parse().unwrap()).unwrap().is_some());

    // The relay does not reach the session handshake or another relay.
    for path in ["network/session", format!("relay/{}/ws", c.id()).as_str()] {
        let blocked = client
            .get(format!("{base}/{path}"))
            .header(super::relay::RELAY_AUTH_HEADER, format!("Bearer {token_at_a}"))
            .send()
            .await
            .unwrap();
        assert_eq!(blocked.status(), 403, "{path}");
    }
    eventually("streams are released", WAIT, || a.state.network.relays.open_streams() == 0 && b.state.network.relays.open_streams() == 0)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_session_to_the_same_member_replaces_the_first() {
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b")]) else { return };
    let (a, b) = (cluster.start("a", true).await, cluster.start("b", true).await);
    cluster.form(&a, &[&b], JoinVia::Auto).await;
    eventually("linked", WAIT, || linked(&a, &b)).await;
    let (dialer, listener) = if a.id() < b.id() { (&a, &b) } else { (&b, &a) };
    let first = dialer.state.network.peers.link(&listener.id()).unwrap();
    // Force a reconnect: the dialer closes its end; the pair comes back as one link, not two.
    first.close();
    eventually("a fresh link", WAIT, || dialer.state.network.peers.link(&listener.id()).is_some_and(|link| !Arc::ptr_eq(&link, &first)))
        .await;
    eventually("the pair is linked once", WAIT, || linked(&a, &b)).await;
    assert_eq!(dialer.state.network.peers.links().len(), 1);
    assert_eq!(listener.state.network.peers.links().len(), 1);
}

#[allow(dead_code)]
fn _unused(_: &PeerLink) {}

// ---- a real tailnet ----

/// Not a test: it seeds two data directories so two scratch `kybernd` on a real tailnet are
/// already members of one network, until the joining flows (N4) can do it. Run it by hand:
///
/// ```text
/// KYBERN_SMOKE_DIRS=/path/a:/path/b KYBERN_SMOKE_NODES=<node id>:<dns name>,<node id>:<dns name> \
///   KYBERN_SMOKE_OWNER=<tailscale login> KYBERN_SMOKE_TAILNET=<tailnet> \
///   cargo test -p kybern-daemon --lib seed_two_data_directories -- --ignored --nocapture
/// ```
///
/// The node id is `Self.ID` and the name `Self.DNSName` from `tailscale status --json` on each
/// machine. The first directory founds the network and adds the second. Each directory also gets a
/// `settings.json` with the network port `KYBERN_SMOKE_PORT` (default 4175, free beside a daemon on
/// 4173). It prints the machine ids.
#[test]
#[ignore = "seeds scratch data directories for a manual run on a real tailnet"]
fn seed_two_data_directories_for_a_real_tailnet() {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
    let dirs: Vec<String> = var("KYBERN_SMOKE_DIRS").split(':').map(str::to_string).collect();
    let nodes: Vec<(String, String)> = var("KYBERN_SMOKE_NODES")
        .split(',')
        .map(|pair| pair.split_once(':').map(|(id, name)| (id.to_string(), name.trim_end_matches('.').to_string())).expect("node id:name"))
        .collect();
    assert_eq!((dirs.len(), nodes.len()), (2, 2), "two directories and two nodes");
    let port: u16 = std::env::var("KYBERN_SMOKE_PORT").ok().and_then(|port| port.parse().ok()).unwrap_or(4175);
    let machines: Vec<_> = dirs
        .iter()
        .zip(&nodes)
        .map(|(dir, (node_id, ts_name))| {
            std::fs::create_dir_all(dir).unwrap();
            let paths = crate::config::Paths::resolve(Some(dir.into())).unwrap();
            let store = kybern_store::Store::open(&paths.db).unwrap();
            let id = uuid::Uuid::now_v7().to_string();
            store.meta_set("environment_id", &id).unwrap();
            let membership = super::start(std::path::Path::new(dir), &store, &id).unwrap();
            std::fs::write(&paths.settings, json!({ "network": { "enabled": true, "port": port } }).to_string()).unwrap();
            let info = super::SelfInfo {
                name: ts_name.split('.').next().unwrap().to_string(),
                os: "linux".into(),
                ts_node_id: Some(node_id.clone()),
                ts_name: Some(ts_name.clone()),
                port: (port != DEFAULT_PORT).then_some(port),
            };
            (membership, info, id)
        })
        .collect();
    machines[0].0.genesis(&var("KYBERN_SMOKE_OWNER"), &var("KYBERN_SMOKE_TAILNET"), &machines[0].1).unwrap();
    machines[0].0.add(machines[1].0.self_descriptor(&machines[1].1), JoinVia::Auto).unwrap();
    machines[1].0.join(machines[0].0.all_ops()).unwrap();
    for ((_, _, id), dir) in machines.iter().zip(&dirs) {
        println!("seeded {dir}: machine {id}");
    }
}
