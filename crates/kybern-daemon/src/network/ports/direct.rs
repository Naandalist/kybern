//! Separate TCP sockets for port bytes. Authorization stays on the authenticated peer session.
//!
//! A forward's caller dials the target's per-forward listener on its Tailscale address and sends the 64-byte token; the
//! target checks it and who the connection comes from (`tailscale whois`), then answers one byte.
//!
//! * Version 1 (`ports.direct.v1`): the target connects to the dev server before it answers, and the socket carries raw
//!   bytes from then on. Each browser connection costs those two round trips.
//! * Version 2 (`ports.direct.v2`, asked for in `peer.ports.prepare`): the target answers as soon as the caller is
//!   authorized, and the socket then waits for a mode byte. The caller keeps a few such sockets ready ([`WarmPool`]), so a
//!   browser connection costs no round trip before its first bytes. Mode `R` connects the dev server and carries raw
//!   bytes; mode `H` carries one HTTP/2 connection whose requests the target sends over pooled HTTP/1.1 connections to the
//!   dev server (the Fast mode of a preview, [`super::fast`]).
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Empty, combinators::BoxBody};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use kybern_protocol::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::super::{
    Network,
    peers::{self, PeerLink},
    tailscale,
};
use crate::state::AppState;

const TARGET: &str = "kybern::network::ports";
pub(super) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a source address stays verified for one forward once `whois` named the member's node. Tailscale addresses
/// belong to a node for its lifetime; the per-forward token is checked on every connection regardless.
const VERIFIED_FOR: Duration = Duration::from_secs(10 * 60);
/// An authorized version 2 socket waits this long for its mode byte.
const WARM_WAIT: Duration = Duration::from_secs(60);
/// The caller discards a ready socket older than this, well before the target gives up on it.
const WARM_KEEP: Duration = Duration::from_secs(45);
/// Ready sockets a forward keeps: what a browser opens at once for one origin, less the one it reuses.
const WARM_SOCKETS: usize = 4;
/// A forward keeps sockets ready for this long after its last connection.
const WARM_WHILE_ACTIVE: Duration = Duration::from_secs(10 * 60);
/// Connections one forward's listener checks at once (token, `whois`, at most [`HANDSHAKE_TIMEOUT`] each); more are
/// dropped unanswered.
pub(super) const MAX_HANDSHAKES: usize = 32;
/// How long a connection waits for a place under the stream limits before it is refused.
pub(super) const QUEUE_WAIT: Duration = Duration::from_secs(10);

pub const MODE_RAW: u8 = b'R';
pub const MODE_HTTP2: u8 = b'H';
/// A timed throughput probe (`ports.route.v1`): the target sends filler bytes until the caller closes the socket, for at
/// most [`PROBE_LIMIT`] bytes or [`PROBE_TIME`].
pub const MODE_PROBE: u8 = b'P';
pub(super) const ACK: u8 = 1;
/// What one probe sends at most, and for how long: enough to tell a fast route (2 MiB in under a second at 2.5 MB/s)
/// without spending much of a slow or metered link on it.
pub(super) const PROBE_LIMIT: usize = 2 * 1024 * 1024;
const PROBE_TIME: Duration = Duration::from_secs(5);
/// What a caller sends after a relayed route's token: a fresh nonce the target signs, so the caller knows the target
/// answers, not the member in between.
pub(super) const NONCE_LEN: usize = 32;
const SIGNATURE_LEN: usize = 64;

struct Serving {
    id: Uuid,
    link: Arc<PeerLink>,
    cancel: CancellationToken,
    version: u8,
    /// The listener's port.
    port: u16,
    /// Source addresses `whois` already named, with the node it named, and when.
    verified: Mutex<HashMap<IpAddr, (String, Instant)>>,
    /// The members this forward's connections may also be relayed through (`peer.ports.via`).
    vias: Mutex<Vec<Via>>,
}

/// A member allowed to relay one forward: its node, and the hash of the token that works only from there.
struct Via {
    machine: String,
    node: String,
    token_hash: String,
    /// Ends every connection let in with this token: the member is dropped, no longer permitted, or its session ends.
    cancel: CancellationToken,
}

/// Which credential a connection presented.
enum Credential {
    /// The forward's own token, from the caller's node.
    Own,
    /// A relaying member's token, from that member's node: the connection lasts while the member may relay it.
    Via { machine: String, cancel: CancellationToken },
}

/// Members one forward may name at once; the oldest goes first.
const MAX_VIAS: usize = 4;

#[derive(Default)]
pub struct Listeners(Mutex<Vec<Arc<Serving>>>);

impl Listeners {
    pub fn close_all(&self) {
        for serving in self.0.lock().unwrap_or_else(|p| p.into_inner()).drain(..) {
            serving.cancel.cancel();
        }
    }

    pub fn release(&self, machine: &str, id: Uuid) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).retain(|s| {
            let keep = s.link.machine_id != machine || s.id != id;
            if !keep {
                s.cancel.cancel();
            }
            keep
        });
    }

    fn serving(&self, machine: &str, id: Uuid) -> Option<Arc<Serving>> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|s| s.link.machine_id == machine && s.id == id && !s.cancel.is_cancelled())
            .cloned()
    }

    /// Let `machine`'s forward `id` also arrive relayed by member `via` (node `node`): the token for that route, which
    /// works only from `node`. `None` when no such forward is open here.
    pub fn allow_via(&self, machine: &str, id: Uuid, via: &str, node: String) -> Option<String> {
        let serving = self.serving(machine, id)?;
        let token = crate::auth::hash(&crate::auth::generate());
        let mut vias = serving.vias.lock().unwrap_or_else(|p| p.into_inner());
        vias.retain(|held| {
            let keep = held.machine != via;
            if !keep {
                held.cancel.cancel();
            }
            keep
        });
        if vias.len() >= MAX_VIAS {
            vias.remove(0).cancel.cancel();
        }
        let cancel = serving.cancel.child_token();
        vias.push(Via { machine: via.to_string(), node, token_hash: crate::auth::hash(&token), cancel });
        Some(token)
    }

    /// Take back what [`Self::allow_via`] gave `via`, and end the connections it relays: open ones and ready ones alike.
    pub fn drop_via(&self, machine: &str, id: Uuid, via: &str) {
        if let Some(serving) = self.serving(machine, id) {
            serving.vias.lock().unwrap_or_else(|p| p.into_inner()).retain(|held| {
                let keep = held.machine != via;
                if !keep {
                    held.cancel.cancel();
                }
                keep
            });
        }
    }

    /// Whether `port` is the listener of `machine`'s forward `id` and `via` may relay it.
    pub fn relays_through(&self, machine: &str, id: Uuid, port: u16, via: &str) -> bool {
        self.serving(machine, id).is_some_and(|serving| {
            serving.port == port && serving.vias.lock().unwrap_or_else(|p| p.into_inner()).iter().any(|held| held.machine == via)
        })
    }

    /// End the forwards whose caller may no longer use this machine's ports, and the relayed connections of members that
    /// may no longer relay them (removed, paused, without ports permission, or with no session).
    pub fn close_unpermitted(&self, network: &Network) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).retain(|s| {
            let keep = authorized(network, &s.link);
            if !keep {
                s.cancel.cancel();
            }
            s.vias.lock().unwrap_or_else(|p| p.into_inner()).retain(|via| {
                let keep = keep && member_may_relay(network, &via.machine);
                if !keep {
                    tracing::info!(target: TARGET, caller = %s.link.machine_id, via = %via.machine, "ended a forward's connections through a member that may no longer relay them");
                    via.cancel.cancel();
                }
                keep
            });
            keep
        });
    }
}

/// A member may relay a forward here while it is a current, unpaused member with a session that may use this machine's
/// ports itself.
fn member_may_relay(network: &Network, machine: &str) -> bool {
    super::hop::live_member_node(network, machine).is_some() && network.peers.link(machine).is_some_and(|link| authorized(network, &link))
}

/// What the target of a relayed connection signs: the caller's nonce, bound to that forward, its listener and the three
/// machines, so a member in between can neither answer in the target's place nor replay an answer.
pub(super) fn hop_proof(nonce: &[u8], forward_id: Uuid, port: u16, caller: &str, via: &str, target: &str) -> Vec<u8> {
    let mut message = b"kybern ports hop v1\0".to_vec();
    message.extend_from_slice(nonce);
    message.extend_from_slice(forward_id.as_bytes());
    message.extend_from_slice(&port.to_be_bytes());
    for machine in [caller, via, target] {
        message.extend_from_slice(machine.as_bytes());
        message.push(0);
    }
    message
}

pub(super) fn authorized(network: &Network, link: &PeerLink) -> bool {
    network.is_enabled()
        && !link.is_closed()
        && link.grant_id.is_none_or(|id| network.grants.is_active(id))
        && peers::effective_scopes(network, link).contains(&Scope::MachinePorts)
}

/// A destination is always obtained from the peer's recorded Tailscale node, never an RPC URL.
pub async fn destination(network: &Network, machine: &str, port: u16) -> Option<SocketAddr> {
    let node = network.membership()?.projection().entry(machine)?.descriptor.ts_node_id.clone()?;
    let status = network.tailscale.status().await.ok()?;
    let peer = status.peer(&node)?;
    if !status.running || !peer.online || network.tailscale.listen_ip().await.is_none() {
        return None;
    }
    let ip = peer.ip()?;
    if !tailscale::plain_http_allowed(ip, network.tailscale.is_fake()) {
        return None;
    }
    Some(SocketAddr::new(network.tailscale.dial_target(ip), port))
}

pub async fn prepare(state: &AppState, link: &Arc<PeerLink>, request: PeerPortsPrepareParams) -> Result<PeerPortsPrepareResult, RpcError> {
    super::validate_port(state, request.port)?;
    let network = &state.network;
    if !authorized(network, link) {
        return Err(RpcError::new(codes::FORBIDDEN, "This machine no longer permits port forwarding."));
    }
    let node = network.membership().and_then(|m| m.projection().entry(&link.machine_id).and_then(|e| e.descriptor.ts_node_id.clone()));
    let Some(node) = node else {
        return Ok(PeerPortsPrepareResult { direct: None });
    };
    let Some(ip) = network.tailscale.listen_ip().await.filter(|ip| tailscale::plain_http_allowed(*ip, network.tailscale.is_fake())) else {
        return Ok(PeerPortsPrepareResult { direct: None });
    };
    let listener = match TcpListener::bind(SocketAddr::new(ip, 0)).await {
        Ok(listener) => listener,
        Err(_) => return Ok(PeerPortsPrepareResult { direct: None }),
    };
    let port = listener.local_addr().map_err(RpcError::internal)?.port();
    let token = crate::auth::hash(&crate::auth::generate());
    let token_hash = crate::auth::hash(&token);
    let version = if request.direct_version.unwrap_or(1) >= 2 { 2 } else { 1 };
    let serving = Arc::new(Serving {
        id: request.forward_id,
        link: link.clone(),
        cancel: CancellationToken::new(),
        version,
        port,
        verified: Mutex::default(),
        vias: Mutex::default(),
    });
    {
        let mut held = network.ports.direct.0.lock().unwrap_or_else(|p| p.into_inner());
        held.retain(|s| !s.cancel.is_cancelled() && !s.link.is_closed());
        if held.len() >= MAX_PORT_FORWARDS * 16 || held.iter().filter(|s| s.link.machine_id == link.machine_id).count() >= MAX_PORT_FORWARDS
        {
            return Err(RpcError::new(codes::CONFLICT, "Too many direct port forwards. Stop one first."));
        }
        if held.iter().any(|s| s.link.machine_id == link.machine_id && s.id == request.forward_id) {
            return Err(RpcError::new(codes::CONFLICT, "That forward is already open."));
        }
        held.push(serving.clone());
    }
    let state = state.clone();
    tokio::spawn(async move {
        let mut idle_since = Instant::now();
        let active = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let handshakes = Arc::new(tokio::sync::Semaphore::new(MAX_HANDSHAKES));
        let idle_limit = state.network.timing.ports_idle;
        let tick = (idle_limit / 4).clamp(Duration::from_millis(50), Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = serving.cancel.cancelled() => break,
                _ = serving.link.closed() => break,
                _ = state.shutdown.cancelled() => break,
                _ = tokio::time::sleep(tick) => {
                    if !authorized(&state.network, &serving.link) { break; }
                    if active.load(std::sync::atomic::Ordering::Relaxed) > 0 { idle_since = Instant::now(); }
                    else if idle_since.elapsed() >= idle_limit { break; }
                }
                accepted = listener.accept() => {
                    let Ok((socket, source)) = accepted else { continue; };
                    // Anyone on the tailnet can connect before the token is checked: over the cap, a connection is dropped.
                    let Ok(handshake) = handshakes.clone().try_acquire_owned() else {
                        tracing::debug!(target: TARGET, %source, "dropped a direct connection: too many handshakes in flight");
                        continue;
                    };
                    let _ = socket.set_nodelay(true);
                    idle_since = Instant::now();
                    let (state, serving, node, token_hash, active) = (state.clone(), serving.clone(), node.clone(), token_hash.clone(), active.clone());
                    tokio::spawn(async move {
                        active.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        transfer(&state, &serving, socket, source.ip(), &node, &token_hash, request.port, handshake).await;
                        active.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    });
                }
            }
        }
        serving.cancel.cancel();
        state.network.ports.direct.0.lock().unwrap_or_else(|p| p.into_inner()).retain(|held| !Arc::ptr_eq(held, &serving));
    });
    Ok(PeerPortsPrepareResult { direct: Some(PeerPortsDirect { port, token, version: (version == 2).then_some(2) }) })
}

/// The connection comes from `node`: an address `whois` named as that node recently for this forward, or `whois` now.
async fn verify_source(state: &AppState, serving: &Serving, source: IpAddr, node: &str) -> bool {
    let known = serving
        .verified
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&source)
        .is_some_and(|(named, at)| named == node && at.elapsed() < VERIFIED_FOR);
    if known {
        return true;
    }
    let Ok(who) = state.network.tailscale.whois(source).await else { return false };
    if who.node_id != node {
        return false;
    }
    serving.verified.lock().unwrap_or_else(|p| p.into_inner()).insert(source, (who.node_id, Instant::now()));
    true
}

/// Which credential a connection presented: the forward's own token, which works only from the caller's node, or a
/// relaying member's, which works only from that member's node while it may use this machine's ports itself.
async fn verify_token(
    state: &AppState,
    serving: &Serving,
    source: IpAddr,
    node: &str,
    token_hash: &str,
    presented: &str,
) -> Option<Credential> {
    let presented = crate::auth::hash(presented);
    if presented == token_hash {
        return verify_source(state, serving, source, node).await.then_some(Credential::Own);
    }
    let via = serving
        .vias
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .find(|via| via.token_hash == presented && !via.cancel.is_cancelled())
        .map(|via| (via.machine.clone(), via.node.clone(), via.cancel.clone()));
    let (machine, via_node, cancel) = via?;
    (member_may_relay(&state.network, &machine) && verify_source(state, serving, source, &via_node).await)
        .then_some(Credential::Via { machine, cancel })
}

/// Resolves when a connection must end: the forward or the caller's session ended, or, for a relayed connection, the
/// member was dropped, lost its permission or its session.
async fn connection_ended(serving: &Serving, relayed: Option<&(CancellationToken, Option<Arc<PeerLink>>)>) {
    let via = async {
        match relayed {
            Some((cancel, Some(member))) => tokio::select! {
                _ = cancel.cancelled() => {},
                _ = member.closed() => {},
            },
            Some((cancel, None)) => cancel.cancelled().await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        _ = serving.cancel.cancelled() => {},
        _ = serving.link.closed() => {},
        _ = via => {},
    }
}

/// One connection to a forward's listener. `handshake` is its place among the connections not yet authorized
/// ([`MAX_HANDSHAKES`]), given back once the token and source are checked.
#[allow(clippy::too_many_arguments)]
async fn transfer(
    state: &AppState,
    serving: &Serving,
    mut socket: TcpStream,
    source: IpAddr,
    node: &str,
    token_hash: &str,
    port: u16,
    handshake: tokio::sync::OwnedSemaphorePermit,
) {
    let link = &serving.link;
    let handshaking = connection_ended(serving, None);
    tokio::pin!(handshaking);
    let authorize = async {
        let mut token = [0u8; 64];
        socket.read_exact(&mut token).await.ok()?;
        let token = std::str::from_utf8(&token).ok()?;
        let credential = verify_token(state, serving, source, node, token_hash, token).await?;
        if !authorized(&state.network, link) || super::own_ports(state).contains(&port) {
            return None;
        }
        // Through a member, the caller proves nothing more but asks this machine to prove itself: it sends a nonce, and
        // reads nothing of the page until this machine's signature over it checks out.
        let mut proof = None;
        if let Credential::Via { machine, .. } = &credential {
            if serving.version < 2 {
                return None;
            }
            let mut nonce = [0u8; NONCE_LEN];
            socket.read_exact(&mut nonce).await.ok()?;
            let membership = state.network.membership()?;
            let me = state.network.machine_id()?;
            let message = hop_proof(&nonce, serving.id, serving.port, &link.machine_id, machine, &me);
            proof = Some(membership.keys().sign(&message));
        }
        Some((credential, proof))
    };
    let (credential, proof) = tokio::select! {
        _ = &mut handshaking => return,
        result = tokio::time::timeout(HANDSHAKE_TIMEOUT, authorize) => match result {
            Ok(Some(authorized)) => authorized,
            _ => return,
        },
    };
    drop(handshake);
    let relayed = match credential {
        Credential::Own => None,
        Credential::Via { machine, cancel } => Some((cancel, state.network.peers.link(&machine))),
    };
    let ended = connection_ended(serving, relayed.as_ref());
    tokio::pin!(ended);
    let mode = if serving.version >= 2 {
        let mut answer = vec![ACK];
        answer.extend_from_slice(proof.as_deref().unwrap_or_default());
        if socket.write_all(&answer).await.is_err() {
            return;
        }
        // A ready socket: it waits for the caller to use it.
        let mut mode = [0u8; 1];
        let read = tokio::select! {
            _ = &mut ended => return,
            read = tokio::time::timeout(WARM_WAIT, socket.read_exact(&mut mode)) => read,
        };
        match read {
            Ok(Ok(_)) => mode[0],
            _ => return,
        }
    } else {
        MODE_RAW
    };
    // A socket in use takes a place under the stream limits; over them it waits for one instead of being dropped. A ready
    // socket that waits for its mode does not count.
    let key = format!("inbound:{}", link.machine_id);
    let Some(_slot) =
        state.network.relays.acquire_wait(&key, crate::network::relay::MAX_INBOUND_PER_MEMBER, QUEUE_WAIT, &serving.cancel).await
    else {
        tracing::debug!(target: TARGET, machine = %link.machine_id, "refused a direct connection: no place under the stream limit");
        return;
    };
    match mode {
        MODE_RAW => {
            let upstream = tokio::select! {
                _ = &mut ended => return,
                upstream = super::connect_loopback(port) => upstream,
            };
            let Ok(mut upstream) = upstream else { return };
            let _ = upstream.set_nodelay(true);
            if serving.version < 2 && socket.write_all(&[ACK]).await.is_err() {
                return;
            }
            tokio::select! {
                _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream) => {},
                // A reset, not an end: a plain FIN reads as a half-close, and the client would keep its side open.
                _ = &mut ended => { let _ = socket.set_zero_linger(); },
            }
        }
        MODE_HTTP2 => {
            tokio::select! {
                _ = serve_http2(socket, port) => {},
                _ = &mut ended => {},
            }
        }
        MODE_PROBE => {
            tokio::select! {
                _ = send_probe(&mut socket) => {},
                _ = &mut ended => {},
            }
        }
        _ => {}
    }
}

// ---- mode H: one HTTP/2 connection, pooled HTTP/1.1 to the dev server ----

pub(super) type ProxyBody = BoxBody<Bytes, hyper::Error>;

pub(super) fn empty_body() -> ProxyBody {
    Empty::<Bytes>::new().map_err(|never| match never {}).boxed()
}

pub(super) fn status_response(status: StatusCode) -> Response<ProxyBody> {
    let mut response = Response::new(empty_body());
    *response.status_mut() = status;
    response
}

/// Headers that belong to one hop (RFC 9110 §7.6.1), and those HTTP/2 forbids.
pub(super) fn strip_hop_headers(headers: &mut hyper::HeaderMap) {
    let named: Vec<String> = headers
        .get_all(hyper::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect();
    for name in named {
        headers.remove(name.as_str());
    }
    for name in ["connection", "keep-alive", "proxy-connection", "transfer-encoding", "upgrade", "te", "trailer", "proxy-authorization"] {
        headers.remove(name);
    }
}

/// The address the dev server answers on: `127.0.0.1`, else `::1` (a dev server may listen on either).
async fn loopback_address(port: u16) -> Option<SocketAddr> {
    let socket = super::connect_loopback(port).await.ok()?;
    socket.peer_addr().ok()
}

async fn serve_http2(socket: TcpStream, port: u16) {
    let Some(upstream) = loopback_address(port).await else {
        tracing::debug!(target: TARGET, port, "nothing listens on the forwarded port; the HTTP/2 connection ends");
        return;
    };
    let mut connector = hyper_util::client::legacy::connect::HttpConnector::new();
    connector.set_nodelay(true);
    connector.set_connect_timeout(Some(Duration::from_secs(3)));
    let client: hyper_util::client::legacy::Client<_, Incoming> = hyper_util::client::legacy::Client::builder(TokioExecutor::new())
        .pool_timer(TokioTimer::new())
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(64)
        .build(connector);
    let service = hyper::service::service_fn(move |request: Request<Incoming>| {
        let client = client.clone();
        async move {
            // Only the viewing machine speaks on this connection, and it never passes a page's own `x-kybern-*` headers on.
            if request.headers().contains_key(super::cache::REVALIDATE_HEADER) {
                return Ok::<_, Infallible>(super::cache::serve_revalidate(upstream, request).await);
            }
            Ok::<_, Infallible>(to_dev_server(&client, upstream, request).await)
        }
    });
    let mut builder = hyper::server::conn::http2::Builder::new(TokioExecutor::new());
    builder
        .timer(TokioTimer::new())
        .adaptive_window(true)
        .max_concurrent_streams(1024)
        .max_frame_size(64 * 1024)
        .max_send_buf_size(1024 * 1024)
        .keep_alive_interval(Some(Duration::from_secs(20)))
        .keep_alive_timeout(Duration::from_secs(20));
    if let Err(error) = builder.serve_connection(TokioIo::new(socket), service).await {
        tracing::debug!(target: TARGET, ?error, "the HTTP/2 connection of a forward ended");
    }
}

pub(super) type DevClient = hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, Incoming>;

/// Removes the `x-kybern-*` headers of a request and says whether the viewing machine keeps a cache of this forward's
/// responses (`x-kybern-cache`): it then wants the dev server's own `ETag` beside a weakened one.
pub(super) fn take_kybern_headers(headers: &mut hyper::HeaderMap) -> bool {
    let caching = headers.contains_key(super::cache::CACHE_HEADER);
    let named: Vec<hyper::header::HeaderName> = headers.keys().filter(|name| name.as_str().starts_with("x-kybern-")).cloned().collect();
    for name in named {
        headers.remove(name);
    }
    caching
}

/// The `Host` a request to the dev server carries: the address the browser used (`localhost:<port>`), so host checks
/// such as Vite's `allowedHosts` see what they would see locally.
pub(super) fn set_dev_host(headers: &mut hyper::HeaderMap, authority: Option<&str>) {
    if !headers.contains_key(hyper::header::HOST)
        && let Some(host) = authority.and_then(|host| hyper::header::HeaderValue::from_str(host).ok())
    {
        headers.insert(hyper::header::HOST, host);
    }
    if !headers.contains_key("x-forwarded-proto") {
        headers.insert("x-forwarded-proto", hyper::header::HeaderValue::from_static("https"));
    }
}

/// One request of the HTTP/2 connection, sent to the dev server over HTTP/1.1.
async fn to_dev_server(client: &DevClient, upstream: SocketAddr, request: Request<Incoming>) -> Response<ProxyBody> {
    let (mut parts, body) = request.into_parts();
    let authority = parts.uri.authority().map(|authority| authority.to_string());
    let path = parts.uri.path_and_query().map(|pq| pq.as_str().to_string()).unwrap_or_else(|| "/".into());
    let Ok(uri) = format!("http://{upstream}{path}").parse::<hyper::Uri>() else {
        return status_response(StatusCode::BAD_REQUEST);
    };
    strip_hop_headers(&mut parts.headers);
    let caching = take_kybern_headers(&mut parts.headers);
    set_dev_host(&mut parts.headers, authority.as_deref());
    parts.uri = uri;
    parts.version = hyper::Version::HTTP_11;
    let gzip_ok = accepts_gzip(&parts.headers) && parts.method != hyper::Method::HEAD;
    match client.request(Request::from_parts(parts, body)).await {
        Ok(response) => {
            let (mut parts, body) = response.into_parts();
            strip_hop_headers(&mut parts.headers);
            parts.version = hyper::Version::HTTP_2;
            if gzip_ok && compressible(&parts) {
                mark_gzipped(&mut parts.headers, caching);
                return Response::from_parts(parts, GzipBody::new(body).boxed());
            }
            Response::from_parts(parts, body.boxed())
        }
        Err(error) => {
            tracing::debug!(target: TARGET, ?error, "the dev server did not answer a forwarded request");
            status_response(StatusCode::BAD_GATEWAY)
        }
    }
}

// ---- compression ----
//
// Dev servers send their modules uncompressed: on the loopback that costs nothing, across machines the bytes are the slow
// part (a Vite page of a few hundred modules is about 1.5 MB, mostly inline source maps, and a relayed tailnet path carries
// a few hundred KB/s). The target gzips what the browser accepts, at the fastest level and flushed per chunk so streamed
// responses keep streaming; the browser decompresses as from any server. Event streams and encoded bodies pass as they are.

/// The request lists `gzip` in `Accept-Encoding` with a quality above zero.
pub(super) fn accepts_gzip(headers: &hyper::HeaderMap) -> bool {
    headers.get_all(hyper::header::ACCEPT_ENCODING).iter().filter_map(|value| value.to_str().ok()).flat_map(|value| value.split(',')).any(
        |coding| {
            let mut parts = coding.split(';');
            let named = parts.next().is_some_and(|name| name.trim().eq_ignore_ascii_case("gzip"));
            let quality = parts
                .filter_map(|param| param.trim().strip_prefix("q=").or_else(|| param.trim().strip_prefix("Q=")))
                .find_map(|q| q.trim().parse::<f32>().ok())
                .unwrap_or(1.0);
            named && quality > 0.0
        },
    )
}

/// Only a whole `200` body is gzipped: a `206` or any `Content-Range` describes the uncompressed bytes, and
/// `Cache-Control: no-transform` forbids it (RFC 9111 §5.2.2.6).
pub(super) fn compressible(parts: &hyper::http::response::Parts) -> bool {
    if parts.headers.contains_key(hyper::header::CONTENT_ENCODING)
        || parts.status != StatusCode::OK
        || parts.headers.contains_key(hyper::header::CONTENT_RANGE)
    {
        return false;
    }
    let no_transform = parts
        .headers
        .get_all(hyper::header::CACHE_CONTROL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|directive| directive.trim().eq_ignore_ascii_case("no-transform"));
    if no_transform {
        return false;
    }
    if parts
        .headers
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n < 512)
    {
        return false;
    }
    let kind = parts.headers.get(hyper::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default().to_ascii_lowercase();
    let kind = kind.split(';').next().unwrap_or_default().trim();
    kind != "text/event-stream"
        && (kind.starts_with("text/")
            || matches!(
                kind,
                "application/javascript"
                    | "application/json"
                    | "application/xml"
                    | "application/wasm"
                    | "image/svg+xml"
                    | "application/manifest+json"
            ))
}

/// The headers of a response whose body is gzipped on the way: no length, the encoding, `Vary`, and a weak `ETag`, since
/// the bytes are no longer the ones a strong tag names (RFC 9110 §8.8.3). The browser's `If-None-Match` still matches the
/// dev server's tag, which compares weakly. With `keep_original` (the viewing machine caches this forward's responses) the
/// dev server's own tag travels beside it in [`super::cache::ORIGINAL_ETAG_HEADER`], so its revalidations send exactly that.
pub(super) fn mark_gzipped(headers: &mut hyper::HeaderMap, keep_original: bool) {
    headers.remove(hyper::header::CONTENT_LENGTH);
    headers.insert(hyper::header::CONTENT_ENCODING, hyper::header::HeaderValue::from_static("gzip"));
    headers.append(hyper::header::VARY, hyper::header::HeaderValue::from_static("accept-encoding"));
    let strong = headers.get(hyper::header::ETAG).filter(|tag| !tag.as_bytes().starts_with(b"W/")).cloned();
    if let Some(strong) = strong
        && let Ok(weak) = hyper::header::HeaderValue::from_bytes(&[b"W/", strong.as_bytes()].concat())
    {
        headers.insert(hyper::header::ETAG, weak);
        if keep_original {
            headers.insert(super::cache::ORIGINAL_ETAG_HEADER, strong);
        }
    }
}

/// A whole body gzipped at once (a batched revalidation sends complete bodies).
pub(super) fn gzip_all(bytes: &[u8]) -> Option<Vec<u8>> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::with_capacity(bytes.len() / 4), flate2::Compression::fast());
    encoder.write_all(bytes).ok()?;
    encoder.finish().ok()
}

/// A response body gzipped as it streams.
struct GzipBody {
    inner: Incoming,
    encoder: Option<flate2::write::GzEncoder<Vec<u8>>>,
}

impl GzipBody {
    fn new(inner: Incoming) -> Self {
        Self { inner, encoder: Some(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast())) }
    }
}

impl hyper::body::Body for GzipBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, hyper::Error>>> {
        use std::io::Write;
        use std::task::Poll;
        loop {
            if self.encoder.is_none() {
                return Poll::Ready(None);
            }
            match std::pin::Pin::new(&mut self.inner).poll_frame(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Some(Err(error))),
                Poll::Ready(Some(Ok(frame))) => {
                    let Ok(data) = frame.into_data() else { continue };
                    let encoder = self.encoder.as_mut().expect("checked above");
                    if encoder.write_all(&data).is_err() || encoder.flush().is_err() {
                        self.encoder = None;
                        return Poll::Ready(None);
                    }
                    let out = std::mem::take(encoder.get_mut());
                    if !out.is_empty() {
                        return Poll::Ready(Some(Ok(hyper::body::Frame::data(Bytes::from(out)))));
                    }
                }
                Poll::Ready(None) => {
                    let finished = self.encoder.take().and_then(|encoder| encoder.finish().ok()).unwrap_or_default();
                    return Poll::Ready((!finished.is_empty()).then(|| Ok(hyper::body::Frame::data(Bytes::from(finished)))));
                }
            }
        }
    }
}

/// Mode `P`: filler bytes until the caller has measured enough and closes, within [`PROBE_LIMIT`] and [`PROBE_TIME`].
pub(super) async fn send_probe<S: tokio::io::AsyncWrite + Unpin>(socket: &mut S) {
    let filler = vec![0x5au8; 64 * 1024];
    let deadline = Instant::now() + PROBE_TIME;
    let mut sent = 0;
    while sent < PROBE_LIMIT && Instant::now() < deadline {
        match tokio::time::timeout_at(deadline.into(), socket.write_all(&filler)).await {
            Ok(Ok(())) => sent += filler.len(),
            _ => break,
        }
    }
    let _ = socket.shutdown().await;
}

// ---- the caller ----

/// A member that relays a route's connections (`peer.ports.hop`): its listener and token, and what the target behind it
/// must prove on every connection.
#[derive(Clone, Debug)]
pub struct Hop {
    pub machine_id: String,
    pub machine_name: String,
    pub address: SocketAddr,
    pub token: String,
    pub proof: HopProof,
}

/// Who must answer at the far end of a relayed route: the target's signing key from membership, and what binds its
/// signature to this route.
#[derive(Clone, Debug)]
pub struct HopProof {
    pub caller: String,
    pub target: String,
    pub target_key: String,
    pub forward_id: Uuid,
}

/// A TCP connection from this machine's Tailscale address (the fake binds a loopback source).
pub(super) async fn connect_from_tailnet(network: &Network, address: SocketAddr) -> Option<TcpStream> {
    let socket = if address.is_ipv4() { TcpSocket::new_v4() } else { TcpSocket::new_v6() }.ok()?;
    if let Some(source) = network.tailscale.dial_source() {
        socket.bind(SocketAddr::new(source, 0)).ok()?;
    }
    let socket = socket.connect(address).await.ok()?;
    socket.set_nodelay(true).ok()?;
    Some(socket)
}

/// Dial a forward's listener (through `hop` when given) and authorize: for version 2 the socket is then ready and waits for
/// a mode. Through a hop, the member checks its own token, connects the target and answers one byte. The caller then
/// sends the route's token and a fresh nonce, and the target answers with its signature over the nonce, the forward, its
/// listener and the three machines ([`hop_proof`]). The socket is used only when that signature verifies against the
/// target's key in membership: a member cannot answer in the target's place. (It still carries the bytes, unencrypted
/// beyond WireGuard on each leg, so it could read or change them; a member is a machine of the user's own network that
/// was granted `machine:relay`.)
async fn dial(network: &Network, address: SocketAddr, endpoint: &PeerPortsDirect, hop: Option<&Hop>) -> Option<(TcpStream, Duration)> {
    let connect = async {
        if endpoint.token.len() != 64 || hop.is_some_and(|hop| hop.token.len() != 64) {
            return None;
        }
        let started = Instant::now();
        let mut socket = connect_from_tailnet(network, hop.map_or(address, |hop| hop.address)).await?;
        let round_trip = started.elapsed();
        let mut hello = endpoint.token.as_bytes().to_vec();
        let mut nonce = [0u8; NONCE_LEN];
        if let Some(hop) = hop {
            socket.write_all(hop.token.as_bytes()).await.ok()?;
            if socket.read_u8().await.ok()? != ACK || endpoint.version.unwrap_or(1) < 2 {
                return None;
            }
            rand::RngExt::fill(&mut rand::rng(), &mut nonce);
            hello.extend_from_slice(&nonce);
        }
        socket.write_all(&hello).await.ok()?;
        if endpoint.version.unwrap_or(1) >= 2 && socket.read_u8().await.ok()? != ACK {
            return None;
        }
        if let Some(hop) = hop {
            let mut signature = [0u8; SIGNATURE_LEN];
            socket.read_exact(&mut signature).await.ok()?;
            let proof = &hop.proof;
            let message = hop_proof(&nonce, proof.forward_id, endpoint.port, &proof.caller, &hop.machine_id, &proof.target);
            if crate::network::keys::verify_signature(&proof.target_key, &message, &signature).is_err() {
                tracing::warn!(target: TARGET, via = %hop.machine_name, "a relayed route did not prove it reaches the target; not using it");
                return None;
            }
        }
        Some((socket, round_trip))
    };
    tokio::time::timeout(HANDSHAKE_TIMEOUT * if hop.is_some() { 2 } else { 1 }, connect).await.ok().flatten()
}

/// A socket that carries raw bytes to the forwarded port.
pub async fn connect(network: &Network, address: SocketAddr, endpoint: &PeerPortsDirect) -> Option<TcpStream> {
    let (mut socket, _) = dial(network, address, endpoint, None).await?;
    if endpoint.version.unwrap_or(1) >= 2 {
        socket.write_all(&[MODE_RAW]).await.ok()?;
    } else if tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.read_u8()).await.ok()?.ok()? != ACK {
        return None;
    }
    Some(socket)
}

/// The direct route of one forward, with the sockets it keeps ready (version 2).
pub struct Route {
    pub address: SocketAddr,
    pub endpoint: PeerPortsDirect,
    /// The member this route's connections travel through, if any.
    pub hop: Option<Hop>,
    warm: Mutex<VecDeque<(Instant, TcpStream)>>,
    wanted: Notify,
    last_used: Mutex<Instant>,
    /// Ends the ready sockets: the forward stopped, or a new session replaced this route.
    cancel: CancellationToken,
}

impl Route {
    pub fn new(address: SocketAddr, endpoint: PeerPortsDirect, cancel: CancellationToken) -> Self {
        Self::through(address, endpoint, None, cancel)
    }

    pub fn through(address: SocketAddr, endpoint: PeerPortsDirect, hop: Option<Hop>, cancel: CancellationToken) -> Self {
        Self { address, endpoint, hop, warm: Mutex::default(), wanted: Notify::new(), last_used: Mutex::new(Instant::now()), cancel }
    }

    /// Download speed to this machine over this route, in bytes per second: a fresh socket in mode `P`, read for `time`
    /// after the first byte arrives (the first round trip and TCP's first windows are left out as far as they fit in it).
    /// Only for a target that lists `ports.route.v1`.
    pub async fn probe(&self, network: &Network, time: Duration) -> Option<u64> {
        let (mut socket, _) = dial(network, self.address, &self.endpoint, self.hop.as_ref()).await?;
        socket.write_all(&[MODE_PROBE]).await.ok()?;
        let mut buffer = vec![0u8; 64 * 1024];
        let first = tokio::time::timeout(HANDSHAKE_TIMEOUT * 2, socket.read(&mut buffer)).await.ok()?.ok()?;
        if first == 0 {
            return None;
        }
        let started = Instant::now();
        let deadline = started + time;
        let mut bytes = 0u64;
        while let Ok(Ok(read)) = tokio::time::timeout_at(deadline.into(), socket.read(&mut buffer)).await {
            if read == 0 {
                break;
            }
            bytes += read as u64;
            if bytes + first as u64 >= PROBE_LIMIT as u64 {
                break;
            }
        }
        let elapsed = started.elapsed().as_secs_f64().max(0.05);
        Some((bytes as f64 / elapsed) as u64)
    }

    /// This route is replaced: drop its ready sockets.
    pub fn stop(&self) {
        self.cancel.cancel();
        self.warm.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    pub fn ready_sockets(&self) -> bool {
        self.endpoint.version.unwrap_or(1) >= 2
    }

    /// A ready socket that is still open, if one is kept.
    fn take_ready(&self) -> Option<TcpStream> {
        let mut warm = self.warm.lock().unwrap_or_else(|p| p.into_inner());
        while let Some((at, socket)) = warm.pop_front() {
            if at.elapsed() >= WARM_KEEP {
                continue;
            }
            // A ready socket has nothing to read; a closed one reads an end.
            let mut probe = [0u8; 1];
            match socket.try_read(&mut probe) {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Some(socket),
                _ => continue,
            }
        }
        None
    }

    /// A socket for `mode`: a ready one when kept, else a new one. The pool refills behind it.
    pub async fn open(&self, network: &Network, mode: u8) -> Option<TcpStream> {
        *self.last_used.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
        if !self.ready_sockets() {
            return if mode == MODE_RAW && self.hop.is_none() { connect(network, self.address, &self.endpoint).await } else { None };
        }
        let socket = match self.take_ready() {
            Some(socket) => Some(socket),
            None => dial(network, self.address, &self.endpoint, self.hop.as_ref()).await.map(|(socket, _)| socket),
        };
        self.wanted.notify_one();
        let mut socket = socket?;
        socket.write_all(&[mode]).await.ok()?;
        Some(socket)
    }

    /// Dial one ready socket now and keep it; its connect time is the route's round trip.
    pub async fn warm_one(&self, network: &Network) -> Option<Duration> {
        if !self.ready_sockets() {
            return None;
        }
        let (socket, round_trip) = dial(network, self.address, &self.endpoint, self.hop.as_ref()).await?;
        self.warm.lock().unwrap_or_else(|p| p.into_inner()).push_back((Instant::now(), socket));
        Some(round_trip)
    }

    /// Keep [`WARM_SOCKETS`] ready while the forward is in use, replacing those that grow old, until `cancel`.
    pub async fn keep_warm(self: Arc<Self>, network: Arc<Network>) {
        if !self.ready_sockets() {
            return;
        }
        let cancel = self.cancel.clone();
        loop {
            let (count, oldest) = {
                let mut warm = self.warm.lock().unwrap_or_else(|p| p.into_inner());
                warm.retain(|(at, _)| at.elapsed() < WARM_KEEP);
                (warm.len(), warm.front().map(|(at, _)| *at))
            };
            let active = self.last_used.lock().unwrap_or_else(|p| p.into_inner()).elapsed() < WARM_WHILE_ACTIVE;
            if active && count < WARM_SOCKETS {
                if self.warm_one(&network).await.is_none() {
                    // The target is not answering; try again a little later rather than spin.
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                    }
                }
                continue;
            }
            if !active {
                self.warm.lock().unwrap_or_else(|p| p.into_inner()).clear();
            }
            let wait = oldest.map(|at| WARM_KEEP.saturating_sub(at.elapsed())).unwrap_or(WARM_KEEP).max(Duration::from_millis(200));
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = self.wanted.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
        self.warm.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: u16, headers: &[(&str, &str)]) -> hyper::http::response::Parts {
        let mut builder = Response::builder().status(status);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(()).unwrap().into_parts().0
    }

    #[test]
    fn only_a_whole_ok_body_the_server_lets_be_transformed_is_gzipped() {
        let js = ("content-type", "application/javascript");
        assert!(compressible(&response(200, &[js])));
        assert!(!compressible(&response(206, &[js, ("content-range", "bytes 0-99/5000")])), "a range of the plain bytes");
        assert!(!compressible(&response(200, &[js, ("content-range", "bytes 0-4999/5000")])));
        assert!(!compressible(&response(200, &[js, ("cache-control", "public, No-Transform")])));
        assert!(!compressible(&response(203, &[js])));
        assert!(!compressible(&response(200, &[js, ("content-encoding", "br")])));
        assert!(!compressible(&response(200, &[("content-type", "text/event-stream")])));
        assert!(!compressible(&response(200, &[js, ("content-length", "100")])));
        assert!(compressible(&response(200, &[("content-type", "text/css; charset=utf-8"), ("cache-control", "no-cache")])));
    }

    #[test]
    fn gzip_is_used_only_when_the_browser_accepts_it() {
        let accepts = |value: &str| {
            let mut headers = hyper::HeaderMap::new();
            headers.insert(hyper::header::ACCEPT_ENCODING, value.parse().unwrap());
            accepts_gzip(&headers)
        };
        assert!(accepts("gzip, deflate, br"));
        assert!(accepts("br;q=1.0, GZIP;q=0.5"));
        assert!(!accepts("gzip;q=0"));
        assert!(!accepts("gzip; q=0.000"));
        assert!(!accepts("br, identity"));
        assert!(!accepts_gzip(&hyper::HeaderMap::new()));
    }

    #[test]
    fn a_gzipped_body_gets_a_weak_etag_and_no_length() {
        let mut parts = response(200, &[("etag", "\"abc\""), ("content-length", "4000")]);
        mark_gzipped(&mut parts.headers, false);
        assert_eq!(parts.headers[hyper::header::ETAG], "W/\"abc\"");
        assert!(!parts.headers.contains_key(hyper::header::CONTENT_LENGTH));
        assert_eq!(parts.headers[hyper::header::CONTENT_ENCODING], "gzip");
        assert!(!parts.headers.contains_key(super::super::cache::ORIGINAL_ETAG_HEADER), "only a caching viewer asks for it");
        let mut weak = response(200, &[("etag", "W/\"abc\"")]);
        mark_gzipped(&mut weak.headers, true);
        assert_eq!(weak.headers[hyper::header::ETAG], "W/\"abc\"", "a weak tag stays as it is");
        assert!(!weak.headers.contains_key(super::super::cache::ORIGINAL_ETAG_HEADER));
        let mut cached = response(200, &[("etag", "\"abc\"")]);
        mark_gzipped(&mut cached.headers, true);
        assert_eq!(cached.headers[super::super::cache::ORIGINAL_ETAG_HEADER], "\"abc\"", "the dev server's own tag, for revalidation");
    }
}
