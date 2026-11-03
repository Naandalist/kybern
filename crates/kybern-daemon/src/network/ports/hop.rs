//! Port routes through another member (ADE-126, `ports.route.v1`).
//!
//! A tailnet's direct path between two machines can be far slower than a path through a third: measured on 2026-11-03,
//! instabox reached a Mac in Bali at 86 KB/s directly, while instabox → kdi-vps ran at 778 KB/s and kdi-vps → the Mac at
//! about 2.5 MB/s. Tailscale keeps a direct path once it has one, so Kybern picks the route itself.
//!
//! * **The member (M)**, `peer.ports.hop`: binds a listener on its Tailscale address for one forward of the caller (V)
//!   and a random token. Each connection sends the token; M checks it and that the connection comes from V's node
//!   (`whois`), connects the target's (T's) forward listener (resolved from membership, never from V) and answers one
//!   byte, then copies bytes. M needs `machine:relay` from V's grants and policy, as for any relay.
//! * **The target (T)**, `peer.ports.via`: V's forward may also arrive from M's node, with a token T issued for that route
//!   only (V gets it over its own session with T; the forward's own token never passes through M). T checks it on every
//!   connection, with V's ports permission and M's: M must be a current, unpaused member with a session that may use T's
//!   ports itself. Dropping M (`drop`), or M losing any of that, ends every connection let in through it, ready ones too.
//! * **End to end**: M sees the route token as it passes, so the token alone does not prove who answers. V sends a fresh
//!   nonce after it, and T answers with an Ed25519 signature, made with its membership key, over the nonce, the forward,
//!   its listener port and the three machine ids ([`direct::hop_proof`]). V verifies it against T's key in membership
//!   before it sends a byte of the page. M cannot answer in T's place or replay an answer. It still carries the bytes,
//!   which are not encrypted beyond WireGuard on each leg, so it could read or change them after that: M is a machine of
//!   the user's own network that was granted `machine:relay`, and the route is chosen only among such members. Pinned
//!   TLS to T would close that too, at the cost of a TLS layer on every relayed socket; it is not done.
//! * **The viewing machine (V)**, [`choose`]: when a forward's load looks slow, or after a load ends, V measures the direct
//!   route first (mode `P`, at most [`direct::PROBE_LIMIT`] and about [`PROBE_TIME`]). A direct route at [`GOOD_ENOUGH`]
//!   or faster is kept without asking any member. Otherwise each candidate member is measured too, twice, one at a time.
//!   A member is used only when both rounds show at least [`GAIN`] times the direct speed. New connections (and the
//!   forward's next HTTP/2 connection) take the new route; open ones finish where they are. The choice is kept per
//!   target for [`BOOK_FOR`], so the next forward to it takes the route at once ([`use_booked`]) and nothing is measured
//!   again until then. A route through a member that stops answering falls back to the direct one.
//! * Candidates are members with a session open now that list `ports.route.v1` (and so does the target), and that are not
//!   paused. Older members are never asked.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kybern_protocol::methods::*;
use kybern_protocol::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::Forward;
use super::direct::{self, ACK, HANDSHAKE_TIMEOUT, Hop, MAX_HANDSHAKES, Route};
use crate::network::Network;
use crate::network::peers::{self, PeerLink};
use crate::network::tailscale;
use crate::state::AppState;

const TARGET: &str = "kybern::network::ports";
/// How long one probe reads.
pub const PROBE_TIME: Duration = Duration::from_millis(if cfg!(test) { 150 } else { 2500 });
/// How much faster a member's route must be, in every probe round, to be used.
pub const GAIN: f64 = 2.0;
/// Below this a route is not worth taking whatever the ratio (bytes per second).
const MIN_RATE: u64 = 64 * 1024;
/// A direct route at least this fast is kept without measuring any member (bytes per second).
pub const GOOD_ENOUGH: u64 = 1_000_000;
const ROUNDS: usize = 2;
/// Members tried at most.
const MAX_CANDIDATES: usize = 3;
/// A choice for a target is reused for this long.
pub const BOOK_FOR: Duration = Duration::from_secs(15 * 60);

// ---- the member: peer.ports.hop ----

struct Relaying {
    caller: Arc<PeerLink>,
    forward_id: Uuid,
    cancel: CancellationToken,
    /// Connections relayed so far.
    relayed: std::sync::atomic::AtomicU64,
}

/// The hops this machine relays.
#[derive(Default)]
pub struct Hops(Mutex<Vec<Arc<Relaying>>>);

impl Hops {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Arc<Relaying>>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn close_all(&self) {
        for hop in self.lock().drain(..) {
            hop.cancel.cancel();
        }
    }

    pub fn release(&self, machine: &str, id: Uuid) {
        self.lock().retain(|hop| {
            let keep = hop.caller.machine_id != machine || hop.forward_id != id;
            if !keep {
                hop.cancel.cancel();
            }
            keep
        });
    }

    /// Connections relayed so far by the hops open now.
    pub fn relayed(&self) -> u64 {
        self.lock().iter().map(|hop| hop.relayed.load(std::sync::atomic::Ordering::Relaxed)).sum()
    }

    pub fn close_unpermitted(&self, network: &Network) {
        self.lock().retain(|hop| {
            let keep = relay_allowed(network, &hop.caller);
            if !keep {
                hop.cancel.cancel();
            }
            keep
        });
    }
}

fn relay_allowed(network: &Network, link: &PeerLink) -> bool {
    network.is_enabled()
        && !link.is_closed()
        && link.grant_id.is_none_or(|id| network.grants.is_active(id))
        && peers::effective_scopes(network, link).contains(&Scope::MachineRelay)
}

/// A current member that is not paused, and its Tailscale node.
pub(super) fn live_member_node(network: &Network, machine: &str) -> Option<String> {
    let membership = network.membership()?;
    let projection = membership.projection();
    let entry = projection.entry(machine)?;
    if entry.removed.is_some() || entry.paused {
        return None;
    }
    entry.descriptor.ts_node_id.clone()
}

/// `peer.ports.hop`.
pub async fn handle_hop(state: &AppState, link: &Arc<PeerLink>, params: PeerPortsHopParams) -> Result<PeerPortsHopResult, RpcError> {
    let network = &state.network;
    if !relay_allowed(network, link) {
        return Err(RpcError::new(codes::FORBIDDEN, "This machine does not relay for that machine."));
    }
    if params.target == link.machine_id || network.machine_id().as_deref() == Some(params.target.as_str()) || params.port == 0 {
        return Err(RpcError::invalid_params("A hop leads to a third machine's forward listener."));
    }
    let caller_node = live_member_node(network, &link.machine_id)
        .ok_or_else(|| RpcError::new(codes::NOT_AVAILABLE, "no Tailscale node for the caller"))?;
    if live_member_node(network, &params.target).is_none() {
        return Err(RpcError::new(codes::NOT_FOUND, "That machine is not a current member."));
    }
    // Only a listener the target prepared for the caller's forward, and only when the caller named this machine for it:
    // the target confirms both, so a hop is never a way to reach any other port of the target from here.
    let target = network
        .peers
        .link(&params.target)
        .ok_or_else(|| RpcError::new(codes::NOT_AVAILABLE, "This machine has no session to that machine."))?;
    let check = PeerPortsHopCheckParams { caller: link.machine_id.clone(), forward_id: params.forward_id, port: params.port };
    if let Err(error) = target.call::<PeerPortsHopCheck>(check).await {
        return Err(RpcError::new(codes::FORBIDDEN, format!("That machine did not confirm the forward: {}", error.message)));
    }
    let destination = direct::destination(network, &params.target, params.port)
        .await
        .ok_or_else(|| RpcError::new(codes::NOT_AVAILABLE, "This machine cannot reach that machine over the tailnet."))?;
    let ip = network
        .tailscale
        .listen_ip()
        .await
        .filter(|ip| tailscale::plain_http_allowed(*ip, network.tailscale.is_fake()))
        .ok_or_else(|| RpcError::new(codes::NOT_AVAILABLE, "This machine has no tailnet address to relay on."))?;
    let listener = TcpListener::bind(SocketAddr::new(ip, 0)).await.map_err(RpcError::internal)?;
    let port = listener.local_addr().map_err(RpcError::internal)?.port();
    let token = crate::auth::hash(&crate::auth::generate());
    let token_hash = crate::auth::hash(&token);
    let hop = Arc::new(Relaying {
        caller: link.clone(),
        forward_id: params.forward_id,
        cancel: CancellationToken::new(),
        relayed: Default::default(),
    });
    {
        let mut held = network.ports.hops.lock();
        held.retain(|hop| !hop.cancel.is_cancelled() && !hop.caller.is_closed());
        if held.iter().filter(|hop| hop.caller.machine_id == link.machine_id).count() >= MAX_PORT_FORWARDS {
            return Err(RpcError::new(codes::CONFLICT, "Too many relayed forwards for that machine."));
        }
        if let Some(old) = held.iter().find(|old| old.caller.machine_id == link.machine_id && old.forward_id == params.forward_id) {
            old.cancel.cancel();
        }
        held.retain(|old| !(old.caller.machine_id == link.machine_id && old.forward_id == params.forward_id));
        held.push(hop.clone());
    }
    let state = state.clone();
    tokio::spawn(async move {
        let handshakes = Arc::new(tokio::sync::Semaphore::new(MAX_HANDSHAKES));
        let verified: Arc<Mutex<HashMap<IpAddr, Instant>>> = Arc::default();
        let active = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let idle_limit = state.network.timing.ports_idle;
        let tick = (idle_limit / 4).clamp(Duration::from_millis(50), Duration::from_secs(30));
        let mut idle_since = Instant::now();
        loop {
            tokio::select! {
                _ = hop.cancel.cancelled() => break,
                _ = hop.caller.closed() => break,
                _ = state.shutdown.cancelled() => break,
                _ = tokio::time::sleep(tick) => {
                    if !relay_allowed(&state.network, &hop.caller) { break; }
                    if active.load(std::sync::atomic::Ordering::Relaxed) > 0 { idle_since = Instant::now(); }
                    else if idle_since.elapsed() >= idle_limit { break; }
                }
                accepted = listener.accept() => {
                    let Ok((socket, source)) = accepted else { continue };
                    let Ok(handshake) = handshakes.clone().try_acquire_owned() else { continue };
                    let _ = socket.set_nodelay(true);
                    idle_since = Instant::now();
                    let (state, hop, caller_node, token_hash, verified, active) =
                        (state.clone(), hop.clone(), caller_node.clone(), token_hash.clone(), verified.clone(), active.clone());
                    tokio::spawn(async move {
                        active.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        relay_one(&state, &hop, socket, source.ip(), &caller_node, &token_hash, destination, &verified, handshake).await;
                        active.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    });
                }
            }
        }
        hop.cancel.cancel();
        state.network.ports.hops.lock().retain(|held| !Arc::ptr_eq(held, &hop));
    });
    tracing::info!(target: TARGET, caller = %link.machine_id, target_machine = %params.target, "relaying a forward for another machine");
    Ok(PeerPortsHopResult { port, token })
}

#[allow(clippy::too_many_arguments)]
async fn relay_one(
    state: &AppState,
    hop: &Relaying,
    mut socket: TcpStream,
    source: IpAddr,
    caller_node: &str,
    token_hash: &str,
    destination: SocketAddr,
    verified: &Mutex<HashMap<IpAddr, Instant>>,
    handshake: tokio::sync::OwnedSemaphorePermit,
) {
    let network = &state.network;
    let authorize = async {
        let mut token = [0u8; 64];
        socket.read_exact(&mut token).await.ok()?;
        if crate::auth::hash(std::str::from_utf8(&token).ok()?) != token_hash {
            return None;
        }
        let known =
            verified.lock().unwrap_or_else(|p| p.into_inner()).get(&source).is_some_and(|at| at.elapsed() < Duration::from_secs(600));
        if !known {
            let who = network.tailscale.whois(source).await.ok()?;
            if who.node_id != caller_node {
                return None;
            }
            verified.lock().unwrap_or_else(|p| p.into_inner()).insert(source, Instant::now());
        }
        relay_allowed(network, &hop.caller).then_some(())
    };
    let authorized = tokio::select! {
        _ = hop.cancel.cancelled() => return,
        result = tokio::time::timeout(HANDSHAKE_TIMEOUT, authorize) => matches!(result, Ok(Some(()))),
    };
    if !authorized {
        return;
    }
    drop(handshake);
    let key = format!("hop:{}", hop.caller.machine_id);
    // Shorter than the caller's handshake wait, so a busy member is refused rather than taken for a dead one.
    let Some(_slot) =
        network.relays.acquire_wait(&key, crate::network::relay::MAX_INBOUND_PER_MEMBER, HANDSHAKE_TIMEOUT, &hop.cancel).await
    else {
        return;
    };
    let Ok(Some(mut upstream)) = tokio::time::timeout(HANDSHAKE_TIMEOUT, direct::connect_from_tailnet(network, destination)).await else {
        return;
    };
    if socket.write_all(&[ACK]).await.is_err() {
        return;
    }
    hop.relayed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    tokio::select! {
        _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream) => {},
        _ = hop.cancel.cancelled() => { let _ = socket.set_zero_linger(); },
        _ = hop.caller.closed() => { let _ = socket.set_zero_linger(); },
    }
}

// ---- the target: peer.ports.via and peer.ports.hop_check ----

/// `peer.ports.via`: the caller names `via` to relay its forward, and gets the token for that route. `via` must be a
/// current, unpaused member that may use this machine's ports itself, so it can reach nothing through the route that
/// it could not reach on its own.
pub fn handle_via(state: &AppState, link: &Arc<PeerLink>, params: PeerPortsViaParams) -> Result<PeerPortsViaResult, RpcError> {
    let network = &state.network;
    if params.drop {
        network.ports.direct.drop_via(&link.machine_id, params.forward_id, &params.via);
        return Ok(PeerPortsViaResult { token: None });
    }
    if !direct::authorized(network, link) {
        return Err(RpcError::new(codes::FORBIDDEN, "This machine no longer permits port forwarding."));
    }
    if params.via == link.machine_id || network.machine_id().as_deref() == Some(params.via.as_str()) {
        return Err(RpcError::invalid_params("A forward is relayed by a third member."));
    }
    let node =
        live_member_node(network, &params.via).ok_or_else(|| RpcError::new(codes::NOT_FOUND, "That machine is not a current member."))?;
    if !network.peers.link(&params.via).is_some_and(|via| direct::authorized(network, &via)) {
        return Err(RpcError::new(codes::FORBIDDEN, "That machine may not use this machine's ports."));
    }
    let token = network
        .ports
        .direct
        .allow_via(&link.machine_id, params.forward_id, &params.via, node)
        .ok_or_else(|| RpcError::new(codes::NOT_FOUND, "That forward is not open on this machine."))?;
    Ok(PeerPortsViaResult { token: Some(token) })
}

/// `peer.ports.hop_check`, from the member asked to relay: `port` is `caller`'s forward listener here, and `caller`
/// named that member for it.
pub fn handle_hop_check(state: &AppState, link: &Arc<PeerLink>, params: PeerPortsHopCheckParams) -> Result<Empty, RpcError> {
    let network = &state.network;
    if !direct::authorized(network, link)
        || !network.ports.direct.relays_through(&params.caller, params.forward_id, params.port, &link.machine_id)
    {
        return Err(RpcError::new(codes::FORBIDDEN, "That forward does not go through this machine."));
    }
    Ok(Empty {})
}

// ---- the viewing machine: choosing a route ----

/// What was chosen for a target, until when.
#[derive(Clone)]
pub struct Choice {
    until: Instant,
    via: Option<String>,
}

/// Route choices per target.
#[derive(Default)]
pub struct Book(Mutex<HashMap<String, Choice>>);

impl Book {
    pub(super) fn get(&self, target: &str) -> Option<Choice> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).get(target).filter(|choice| Instant::now() < choice.until).cloned()
    }

    pub(super) fn set(&self, target: &str, via: Option<String>) {
        self.set_for(target, via, BOOK_FOR);
    }

    fn set_for(&self, target: &str, via: Option<String>, keep: Duration) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).insert(target.to_string(), Choice { until: Instant::now() + keep, via });
    }
}

fn lists(network: &Network, machine: &str, feature: &str) -> bool {
    network.peers.status(machine).presence.is_some_and(|presence| presence.features.iter().any(|f| f == feature))
}

/// Members that could relay to `target`: a session open now, `ports.route.v1`, not paused.
fn candidates(network: &Network, target: &str) -> Vec<(String, String)> {
    let me = network.machine_id();
    let Some(membership) = network.membership() else { return Vec::new() };
    let projection = membership.projection();
    network
        .peers
        .links()
        .into_iter()
        .filter(|link| link.machine_id != target && Some(&link.machine_id) != me.as_ref() && !link.is_closed())
        .filter(|link| lists(network, &link.machine_id, features::PORTS_ROUTE_V1))
        .filter(|link| live_member_node(network, &link.machine_id).is_some())
        .filter_map(|link| {
            let name = projection.entry(&link.machine_id)?.descriptor.name.clone();
            Some((link.machine_id.clone(), name))
        })
        .take(MAX_CANDIDATES)
        .collect()
}

/// Set up a route for `forward` through `via`: the target gives it its own token, the member relays it. Whatever was
/// set up is taken back when a step fails.
pub(super) async fn through(network: &Arc<Network>, forward: &Forward, direct: &Route, via: &str, via_name: &str) -> Option<Arc<Route>> {
    let target = network.peers.link(&forward.machine_id)?;
    let member = network.peers.link(via)?;
    let token = match target.call::<PeerPortsVia>(PeerPortsViaParams { forward_id: forward.id, via: via.to_string(), drop: false }).await {
        Ok(PeerPortsViaResult { token: Some(token) }) => token,
        Ok(_) => return None,
        Err(error) => {
            tracing::debug!(target: TARGET, via, error = %error.message, "the target did not accept a relayed route");
            return None;
        }
    };
    let params = PeerPortsHopParams { forward_id: forward.id, target: forward.machine_id.clone(), port: direct.endpoint.port };
    let hop = match member.call::<PeerPortsHop>(params).await {
        Ok(hop) => hop,
        Err(error) => {
            tracing::debug!(target: TARGET, via, error = %error.message, "a member did not relay a route");
            drop_via(network, &forward.machine_id, via, forward.id);
            return None;
        }
    };
    let Some(address) = direct::destination(network, via, hop.port).await else {
        release(network, &forward.machine_id, via, forward.id);
        return None;
    };
    let membership = network.membership()?;
    let target_key = membership.projection().entry(&forward.machine_id)?.descriptor.signing_key.clone();
    let proof = direct::HopProof { caller: network.machine_id()?, target: forward.machine_id.clone(), target_key, forward_id: forward.id };
    let hop = Hop { machine_id: via.to_string(), machine_name: via_name.to_string(), address, token: hop.token, proof };
    let endpoint = PeerPortsDirect { token, ..direct.endpoint.clone() };
    Some(Arc::new(Route::through(direct.address, endpoint, Some(hop), forward.cancel.child_token())))
}

/// The target stops accepting `via` for the forward.
fn drop_via(network: &Network, target: &str, via: &str, forward_id: Uuid) {
    if let Some(link) = network.peers.link(target) {
        let params = PeerPortsViaParams { forward_id, via: via.to_string(), drop: true };
        tokio::spawn(async move {
            let _ = link.call::<PeerPortsVia>(params).await;
        });
    }
}

/// A member's route of a forward is given up: the member stops relaying it and the target stops accepting it.
pub(super) fn release(network: &Network, target: &str, via: &str, forward_id: Uuid) {
    if let Some(link) = network.peers.link(via) {
        tokio::spawn(async move {
            let _ = link.notify(PeerPortsRelease::NAME, json!({ "forward_id": forward_id })).await;
        });
    }
    drop_via(network, target, via, forward_id);
}

fn give_up(network: &Network, forward: &Forward, routes: &[Arc<Route>]) {
    for route in routes {
        route.stop();
        if let Some(hop) = &route.hop {
            release(network, &forward.machine_id, &hop.machine_id, forward.id);
        }
    }
}

/// The direct route's speed while the page may be loading over it too: the probe shares the link with the page, so the
/// page's own bytes in that time count as what the link carried.
async fn direct_rate(network: &Network, forward: &Forward, direct: &Route) -> u64 {
    let (before, started) = (forward.load.total_received(), Instant::now());
    let probed = direct.probe(network, PROBE_TIME).await.unwrap_or(0);
    let page = (forward.load.total_received() - before) as f64 / started.elapsed().as_secs_f64().max(0.05);
    probed + page as u64
}

/// Take the route already chosen for this forward's target, if one is booked: what a new forward does at once. Nothing
/// is measured here.
pub(super) async fn use_booked(network: Arc<Network>, forward: Arc<Forward>) {
    if forward.choosing.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    if let Some(direct) = direct_route(&network, &forward) {
        take_booked(&network, &forward, &direct).await;
    }
    forward.choosing.store(false, std::sync::atomic::Ordering::Release);
}

/// Measure the routes of a forward that runs on a direct ready-socket route, and take a member's route when it is
/// [`GAIN`] times faster in every round. Runs in the background, once at a time per forward, after a slow-load signal or
/// the end of a load; the forward works on the direct route meanwhile. A booked choice is used without measuring.
pub(super) async fn choose(network: Arc<Network>, forward: Arc<Forward>) {
    if forward.choosing.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    choose_once(&network, &forward).await;
    forward.choosing.store(false, std::sync::atomic::Ordering::Release);
}

/// The forward's direct ready-socket route, when its target can relay routes at all.
fn direct_route(network: &Network, forward: &Forward) -> Option<Arc<Route>> {
    let direct = forward.route_now().filter(|route| route.ready_sockets() && route.hop.is_none())?;
    lists(network, &forward.machine_id, features::PORTS_ROUTE_V1).then_some(direct)
}

/// Use the booked choice for the target. True when there is one (taken, or the direct route), so nothing is measured.
async fn take_booked(network: &Arc<Network>, forward: &Arc<Forward>, direct: &Arc<Route>) -> bool {
    let Some(choice) = network.ports.book.get(&forward.machine_id) else { return false };
    let Some(via) = choice.via else { return true };
    let name = network.membership().and_then(|m| m.projection().entry(&via).map(|e| e.descriptor.name.clone())).unwrap_or_default();
    match through(network, forward, direct, &via, &name).await {
        Some(route) => {
            super::take_route(network, forward, direct, route);
            true
        }
        None => false,
    }
}

/// Whether the members are worth measuring after the direct route's first probe.
pub fn worth_trying_members(direct: u64) -> bool {
    direct < GOOD_ENOUGH
}

async fn choose_once(network: &Arc<Network>, forward: &Arc<Forward>) {
    let Some(direct) = direct_route(network, forward) else { return };
    if take_booked(network, forward, &direct).await {
        return;
    }
    let candidates = candidates(network, &forward.machine_id);
    if candidates.is_empty() {
        return;
    }
    // The direct route first: when it is fast enough, no member is asked and nothing else is probed.
    let first = direct_rate(network, forward, &direct).await;
    if !worth_trying_members(first) {
        tracing::info!(target: TARGET, machine = %forward.machine_id, direct = first, "a forward's direct route is fast enough");
        network.ports.book.set(&forward.machine_id, None);
        return;
    }
    let mut routes = Vec::new();
    for (via, name) in &candidates {
        if let Some(route) = through(network, forward, &direct, via, name).await {
            routes.push(route);
        }
    }
    if routes.is_empty() {
        network.ports.book.set(&forward.machine_id, None);
        return;
    }
    // Each round measures the direct route, then each member's, one at a time so they do not share the link.
    let mut direct_rates = vec![first];
    let mut rates = vec![Vec::new(); routes.len()];
    for round in 0..ROUNDS {
        if forward.cancel.is_cancelled() {
            give_up(network, forward, &routes);
            return;
        }
        if round > 0 {
            direct_rates.push(direct_rate(network, forward, &direct).await);
        }
        for (i, route) in routes.iter().enumerate() {
            rates[i].push(route.probe(network, PROBE_TIME).await.unwrap_or(0));
        }
    }
    let best = pick(&direct_rates, &rates);
    tracing::info!(
        target: TARGET,
        machine = %forward.machine_id,
        direct = ?direct_rates,
        members = ?routes.iter().zip(&rates).map(|(route, rates)| (route.hop.as_ref().map(|hop| hop.machine_name.clone()), rates.clone())).collect::<Vec<_>>(),
        chosen = ?best.map(|i| routes[i].hop.as_ref().map(|hop| hop.machine_name.clone())),
        "measured a forward's routes"
    );
    let chosen = best.map(|i| routes.swap_remove(i));
    give_up(network, forward, &routes);
    let Some(route) = chosen else {
        network.ports.book.set(&forward.machine_id, None);
        return;
    };
    network.ports.book.set(&forward.machine_id, route.hop.as_ref().map(|hop| hop.machine_id.clone()));
    super::take_route(network, forward, &direct, route);
}
/// The member route to take: the one whose slowest round is fastest, among those at least [`GAIN`] times the direct
/// route in every round and above [`MIN_RATE`].
pub fn pick(direct: &[u64], members: &[Vec<u64>]) -> Option<usize> {
    members
        .iter()
        .enumerate()
        .filter(|(_, rates)| {
            rates.len() == direct.len()
                && !rates.is_empty()
                && rates.iter().zip(direct).all(|(rate, direct)| *rate >= MIN_RATE && *rate as f64 >= GAIN * (*direct).max(1) as f64)
        })
        .max_by_key(|(_, rates)| rates.iter().min().copied().unwrap_or(0))
        .map(|(i, _)| i)
}

/// A member's route stopped answering: the direct route is used for this target for a few minutes, then measured again.
pub fn forget(network: &Network, target: &str) {
    network.ports.book.set_for(target, None, Duration::from_secs(120));
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_member_route_is_taken_only_when_it_is_much_faster_every_time() {
        let kb = 1024;
        // instabox → Mac directly about 90 KB/s; through kdi-vps about 700 KB/s.
        assert_eq!(pick(&[90 * kb, 95 * kb], &[vec![700 * kb, 650 * kb]]), Some(0));
        assert_eq!(pick(&[90 * kb, 95 * kb], &[vec![700 * kb, 150 * kb]]), None, "one slow round is not sustained");
        assert_eq!(pick(&[400 * kb, 400 * kb], &[vec![700 * kb, 750 * kb]]), None, "less than twice as fast");
        assert_eq!(pick(&[0, 0], &[vec![30 * kb, 30 * kb]]), None, "too slow to be worth it whatever the ratio");
        assert_eq!(pick(&[0, 0], &[vec![100 * kb, 100 * kb]]), Some(0), "a direct route that carries nothing");
        assert_eq!(pick(&[90 * kb, 90 * kb], &[vec![500 * kb, 600 * kb], vec![900 * kb, 800 * kb]]), Some(1), "the faster member");
        assert_eq!(pick(&[90 * kb], &[vec![]]), None);
    }

    #[test]
    fn a_fast_direct_route_is_kept_without_asking_any_member() {
        assert!(!worth_trying_members(2_500_000), "2.5 MB/s");
        assert!(!worth_trying_members(GOOD_ENOUGH));
        assert!(worth_trying_members(86_000), "86 KB/s");
    }

    #[test]
    fn a_probe_sends_at_most_its_byte_cap() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let (mut sender, mut receiver) = tokio::io::duplex(64 * 1024);
            let sending = tokio::spawn(async move { direct::send_probe(&mut sender).await });
            let mut received = Vec::new();
            receiver.read_to_end(&mut received).await.unwrap();
            sending.await.unwrap();
            assert!(received.len() <= direct::PROBE_LIMIT + 64 * 1024, "{} bytes", received.len());
            assert!(received.len() >= direct::PROBE_LIMIT, "a fast link gets the whole probe");
        });
    }

    #[test]
    fn only_the_target_can_prove_a_relayed_route() {
        let target = crate::network::keys::MachineKeys::generate();
        let member = crate::network::keys::MachineKeys::generate();
        let forward = Uuid::now_v7();
        let message = direct::hop_proof(&[7; direct::NONCE_LEN], forward, 4000, "v", "m", "t");
        let key = target.signing_public_b64();
        let verify = |message: &[u8], signature: &[u8]| crate::network::keys::verify_signature(&key, message, signature).is_ok();
        assert!(verify(&message, &target.sign(&message)));
        assert!(!verify(&message, &member.sign(&message)), "the member signs in the target's place");
        let replayed = direct::hop_proof(&[8; direct::NONCE_LEN], forward, 4000, "v", "m", "t");
        assert!(!verify(&replayed, &target.sign(&message)), "an answer to another nonce");
        let other_port = direct::hop_proof(&[7; direct::NONCE_LEN], forward, 4001, "v", "m", "t");
        assert!(!verify(&other_port, &target.sign(&message)), "an answer for another listener");
        let other_member = direct::hop_proof(&[7; direct::NONCE_LEN], forward, 4000, "v", "x", "t");
        assert!(!verify(&other_member, &target.sign(&message)), "an answer for a route through another member");
    }
}
