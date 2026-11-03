//! Port forwarding between machines (ADE-86 M4 / U3, design §8.3).
//!
//! `ports.forward` binds `127.0.0.1:<local>` here, preferring the remote port then the next free port.
//! New peers authorize once with `peer.ports.prepare`; each connection carries raw bytes on its own
//! Tailscale TCP socket. Older or indirect peers retain the existing `peer.ports.open` relay streams.
//! Both paths require `machine:ports`, connect only to the target's loopback, and preserve half-close.
//!
//! * The target refuses the ports of its own kybernd (its listeners and the default port): a forward is
//!   never a way to reach another machine's daemon without a token.
//! * A forward with no connection for [`super::Timing::ports_idle`] (30 min) closes, and a daemon holds
//!   at most 16.
//! * A preview of a dev server on another machine (`kybern_preview_open { machine }`) starts a forward
//!   and shows `http://localhost:<local>`: the page, its assets and its hot-reload WebSocket are plain
//!   TCP to the browser, so nothing in the page changes.
//! * Speed (ADE-116): a forward to a `ports.direct.v2` machine keeps authorized sockets ready, so a browser connection
//!   starts without a round trip ([`direct::Route`]), and can also serve `https://localhost:<https_port>` over HTTP/2
//!   for Kybern's web views ([`fast`]), which multiplexes every request of a page over one connection to the target.
//! * Slow links (ADE-126): on such a forward the plain listener also parses HTTP when the browser speaks it ([`http`]), so
//!   every request travels over that one connection too: the target compresses text, this machine shows how far a slow
//!   page has loaded ([`load`]) and keeps responses for the next load ([`cache`]). When another member reaches the target
//!   much faster than the direct route, new connections go through it ([`hop`]).

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use kybern_protocol::methods::*;
use kybern_protocol::*;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::Network;
use super::peers::PeerLink;
use super::relay::{self, Chunk, Slot, Stream, StreamKind, frame_cost};
use crate::state::AppState;

pub mod cache;
pub mod direct;
pub mod fast;
pub mod hop;
pub mod http;
pub mod load;
pub mod tls;

const TARGET: &str = "kybern::network::ports";
const OPEN_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// A TCP read is cut into frames of this size.
const READ_CHUNK: usize = 16 * 1024;

#[cfg(test)]
#[path = "../previews/network_tests.rs"]
mod preview_rpc_tests;

// ---- this machine: the forwards ----

struct Forward {
    id: Uuid,
    machine_id: String,
    machine_name: String,
    remote_port: u16,
    local_port: u16,
    created_at: chrono::DateTime<chrono::Utc>,
    connections: AtomicU32,
    cancel: CancellationToken,
    /// The direct route, and the session it was prepared on: the target's listener for it lives as long as that session,
    /// so a new session (the target restarted, the connection was renewed) prepares it again ([`current_route`]).
    route: std::sync::Mutex<(Option<Arc<direct::Route>>, std::sync::Weak<PeerLink>)>,
    rerouting: tokio::sync::Mutex<()>,
    prepared: bool,
    rtt_ms: Option<u32>,
    /// The HTTPS (HTTP/2) listener, once asked for, and the certificate it presents.
    https: std::sync::Mutex<Option<(u16, Arc<tls::PreviewCert>)>>,
    /// Starting the HTTPS listener happens once.
    https_start: tokio::sync::Mutex<()>,
    tunnel: fast::Tunnel,
    /// The target revalidates batches and keeps the dev server's own tags (`ports.cache.v1`): responses are kept here.
    caching: bool,
    cache: cache::Cache,
    load: load::LoadMeter,
    /// The member new connections go through, and the direct route to return to when it stops answering.
    via: std::sync::Mutex<Option<(PortRouteVia, Arc<direct::Route>)>>,
    /// A route choice ([`hop::choose`]) runs.
    choosing: std::sync::atomic::AtomicBool,
    /// Opened for a preview (stopped by the app when the page's last tab closes) until someone asks for it by hand.
    preview: std::sync::atomic::AtomicBool,
}

impl Forward {
    fn route_now(&self) -> Option<Arc<direct::Route>> {
        self.route.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).0.clone()
    }

    fn caches(&self) -> bool {
        self.caching
    }

    fn via_now(&self) -> Option<PortRouteVia> {
        self.via.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref().map(|(via, _)| via.clone())
    }

    fn view(&self) -> PortForward {
        let https = self.https.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        PortForward {
            id: self.id,
            machine_id: self.machine_id.clone(),
            machine_name: self.machine_name.clone(),
            remote_port: self.remote_port,
            local_port: self.local_port,
            connections: self.connections.load(Ordering::Relaxed),
            created_at: self.created_at,
            rtt_ms: self.rtt_ms,
            https_available: self.route_now().is_some_and(|route| route.ready_sockets()),
            https_port: https.as_ref().map(|(port, _)| *port),
            https_certificate: https.as_ref().map(|(_, cert)| cert.pem.clone()),
            https_certificate_sha256: https.as_ref().map(|(_, cert)| cert.sha256.clone()),
            route_via: self.via_now(),
            loading: self.load.view(),
            origin: if self.preview.load(Ordering::Relaxed) { PortForwardOrigin::Preview } else { PortForwardOrigin::Manual },
        }
    }
}

/// The forwards this daemon holds.
#[derive(Default)]
pub struct Forwards {
    list: std::sync::Mutex<Vec<Arc<Forward>>>,
    pub direct: direct::Listeners,
    /// The forwards this machine relays for other members.
    pub hops: hop::Hops,
    /// Which route each target's forwards take.
    book: hop::Book,
    certs: std::sync::OnceLock<tls::CertStore>,
}

impl Forwards {
    pub fn new(data_dir: &std::path::Path) -> Self {
        let forwards = Self::default();
        let _ = forwards.certs.set(tls::CertStore::new(data_dir.join("network").join("preview-tls")));
        forwards
    }

    /// The certificate of the HTTPS (Fast) listeners, created on first use.
    pub fn certificate(&self) -> anyhow::Result<Arc<tls::PreviewCert>> {
        self.certs.get().ok_or_else(|| anyhow::anyhow!("no data directory for the preview certificate"))?.get()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Arc<Forward>>> {
        self.list.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn views(&self) -> Vec<PortForward> {
        self.lock().iter().map(|forward| forward.view()).collect()
    }

    fn remove(&self, id: Uuid) -> Option<Arc<Forward>> {
        let mut list = self.lock();
        let at = list.iter().position(|forward| forward.id == id)?;
        Some(list.remove(at))
    }

    /// Close every forward (the daemon is shutting down, or the network was turned off).
    pub fn close_all(&self) {
        self.direct.close_all();
        self.hops.close_all();
        for forward in self.lock().drain(..) {
            forward.cancel.cancel();
        }
    }

    /// Close the forwards that lead to a machine (it was removed).
    pub fn close_machine(&self, machine_id: &str) {
        self.lock().retain(|forward| {
            let keep = forward.machine_id != machine_id;
            if !keep {
                forward.cancel.cancel();
            }
            keep
        });
    }
}

fn announce(network: &Network) {
    let result = PortsListResult { forwards: network.ports.views() };
    network.notify_utility(PORTS_CHANGED_NOTIFICATION, serde_json::to_value(result).unwrap_or(Value::Null));
}

/// `ports.list`.
pub fn list(state: &AppState) -> PortsListResult {
    PortsListResult { forwards: state.network.ports.views() }
}

/// `ports.stop`.
pub fn stop(state: &AppState, id: Uuid) -> Result<PortsListResult, RpcError> {
    let forward = state.network.ports.remove(id).ok_or_else(|| RpcError::not_found("port forward"))?;
    forward.cancel.cancel();
    announce(&state.network);
    Ok(list(state))
}

/// `ports.forward`: bind a loopback port here that leads to a loopback port of `machine`. A second call
/// for the same machine and port returns the forward that exists.
pub async fn forward(state: &AppState, params: PortsForwardParams) -> Result<PortForward, RpcError> {
    let hub = state.orchestrator.remote_hub().ok_or_else(|| RpcError::new(codes::NOT_AVAILABLE, "the machine network is not available"))?;
    let target = hub.resolve(&params.machine).map_err(|message| RpcError::new(codes::NOT_FOUND, message))?;
    forward_to(
        &state.network,
        &state.shutdown,
        &target.id,
        &target.name,
        params.remote_port,
        params.local_port,
        params.https,
        params.origin,
    )
    .await
}

/// [`forward`] for a machine already resolved; the agent's preview tool comes through here too (`origin: Preview`).
/// A manual ask for a forward a preview opened makes it manual: it then stays when the preview closes.
#[allow(clippy::too_many_arguments)]
pub async fn forward_to(
    network: &Arc<Network>,
    shutdown: &CancellationToken,
    machine_id: &str,
    machine_name: &str,
    remote_port: u16,
    local_port: Option<u16>,
    https: bool,
    origin: PortForwardOrigin,
) -> Result<PortForward, RpcError> {
    if remote_port == 0 {
        return Err(RpcError::invalid_params("Name a port between 1 and 65535."));
    }
    let existing = network
        .ports
        .lock()
        .iter()
        .find(|forward| {
            forward.machine_id == machine_id
                && forward.remote_port == remote_port
                && local_port.is_none_or(|port| port == forward.local_port)
        })
        .cloned();
    if let Some(existing) = existing {
        if origin.is_manual() && existing.preview.swap(false, Ordering::Relaxed) {
            announce(network);
        }
        if https {
            start_https(network, &existing).await?;
        }
        return Ok(existing.view());
    }
    if network.ports.lock().len() >= MAX_PORT_FORWARDS {
        return Err(RpcError::new(codes::CONFLICT, format!("{MAX_PORT_FORWARDS} ports are forwarded already. Stop one first.")));
    }
    let link = network
        .peers
        .link(machine_id)
        .ok_or_else(|| RpcError::new(codes::NOT_AVAILABLE, format!("{machine_name} is offline, so its ports cannot be forwarded.")))?;
    let id = Uuid::now_v7();
    let asked = Instant::now();
    let cancel = CancellationToken::new();
    let (prepared, route) = prepare_route(network, &link, id, remote_port, &cancel).await?;
    // The route's round trip: a ready socket's TCP connect (which also leaves the first browser connection a socket
    // to use), else the authorizing call over the session.
    let mut rtt = asked.elapsed();
    if let Some(route) = &route
        && let Some(connect) = route.warm_one(network).await
    {
        rtt = connect;
    }
    let listener = match bind_local(remote_port, local_port).await {
        Ok(listener) => listener,
        Err(error) => {
            if prepared {
                let _ = link.notify(PeerPortsRelease::NAME, json!({"forward_id": id})).await;
            }
            return Err(error);
        }
    };
    let bound = listener.local_addr().map_err(RpcError::internal)?.port();
    let forward = Arc::new(Forward {
        id,
        machine_id: machine_id.to_string(),
        machine_name: machine_name.to_string(),
        remote_port,
        local_port: bound,
        created_at: chrono::Utc::now(),
        route: std::sync::Mutex::new((route, Arc::downgrade(&link))),
        rerouting: tokio::sync::Mutex::new(()),
        prepared,
        rtt_ms: Some(rtt.as_millis().min(u32::MAX as u128) as u32),
        https: std::sync::Mutex::new(None),
        https_start: tokio::sync::Mutex::new(()),
        tunnel: fast::Tunnel::default(),
        caching: network.peers.status(machine_id).presence.is_some_and(|p| p.features.iter().any(|f| f == features::PORTS_CACHE_V1)),
        cache: cache::Cache::default(),
        load: load::LoadMeter::default(),
        via: std::sync::Mutex::new(None),
        choosing: Default::default(),
        preview: std::sync::atomic::AtomicBool::new(origin == PortForwardOrigin::Preview),
        connections: AtomicU32::new(0),
        cancel,
    });
    let added = {
        let mut list = network.ports.lock();
        if list.len() >= MAX_PORT_FORWARDS {
            false
        } else {
            list.push(forward.clone());
            true
        }
    };
    if !added {
        if prepared {
            let _ = link.notify(PeerPortsRelease::NAME, json!({"forward_id": id})).await;
        }
        return Err(RpcError::new(codes::CONFLICT, format!("{MAX_PORT_FORWARDS} ports are forwarded already. Stop one first.")));
    }
    tokio::spawn(accept_loop(network.clone(), shutdown.clone(), forward.clone(), listener));
    if let Some(route) = forward.route_now() {
        tokio::spawn(route.keep_warm(network.clone()));
        // Only a route already chosen for this target is taken now; measuring waits for a slow load or its end.
        tokio::spawn(hop::use_booked(network.clone(), forward.clone()));
    }
    if https && let Err(error) = start_https(network, &forward).await {
        tracing::info!(target: TARGET, machine = machine_id, error = %error.message, "the forward has no HTTPS listener");
    }
    announce(network);
    tracing::info!(
        target: TARGET,
        machine = machine_id,
        remote_port,
        local_port = bound,
        rtt_ms = forward.rtt_ms,
        transport = match forward.route_now() { Some(route) if route.ready_sockets() => "tcp-ready", Some(_) => "tcp", None => "websocket" },
        "forwarding a port"
    );
    Ok(forward.view())
}

/// Authorize a forward on `link` (`peer.ports.prepare`) and resolve its direct route: `(prepared, route)`, where an older
/// machine has no prepare at all and a machine that cannot be reached directly has no route (the WebSocket streams carry
/// the forward then). The route's ready sockets are kept until `cancel`.
async fn prepare_route(
    network: &Arc<Network>,
    link: &Arc<PeerLink>,
    id: Uuid,
    remote_port: u16,
    cancel: &CancellationToken,
) -> Result<(bool, Option<Arc<direct::Route>>), RpcError> {
    let presence = network.peers.status(&link.machine_id).presence;
    if !presence.as_ref().is_none_or(|p| p.features.iter().any(|f| f == features::PORTS_DIRECT_V1)) {
        return Ok((false, None));
    }
    // Version 2 is asked for every time: a machine that does not know it ignores the field and answers version 1.
    let prepare = PeerPortsPrepareParams { forward_id: id, port: remote_port, direct_version: Some(2) };
    match link.call::<PeerPortsPrepare>(prepare).await {
        Ok(result) => {
            let route = match result.direct {
                Some(endpoint) => direct::destination(network, &link.machine_id, endpoint.port)
                    .await
                    .map(|addr| Arc::new(direct::Route::new(addr, endpoint, cancel.child_token()))),
                None => None,
            };
            if route.is_none() {
                let _ = link.notify(PeerPortsRelease::NAME, json!({"forward_id": id})).await;
            }
            Ok((true, route))
        }
        Err(error) if error.code == codes::METHOD_NOT_FOUND => Ok((false, None)),
        Err(error) => Err(error),
    }
}

/// The forward's direct route on the session open now. The target's listener for a route ends with the session it was
/// prepared on, so after a new session (the target restarted or the connection was renewed) the forward is prepared
/// again and the old route's ready sockets are dropped. `None` when the forward has no direct route.
async fn current_route(network: &Arc<Network>, forward: &Arc<Forward>) -> Option<Arc<direct::Route>> {
    let link = network.peers.link(&forward.machine_id)?;
    let fresh = |forward: &Forward| {
        let held = forward.route.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        held.1.upgrade().is_some_and(|prepared_on| Arc::ptr_eq(&prepared_on, &link)).then(|| held.0.clone())
    };
    if let Some(route) = fresh(forward) {
        return route;
    }
    if !forward.prepared {
        return None;
    }
    let _rerouting = forward.rerouting.lock().await;
    if let Some(route) = fresh(forward) {
        return route;
    }
    let route = match prepare_route(network, &link, forward.id, forward.remote_port, &forward.cancel).await {
        Ok((_, route)) => route,
        // The old session's listener is still being torn down there: keep the forward as it is and try again next time.
        Err(error) => {
            tracing::debug!(target: TARGET, machine = %forward.machine_id, error = %error.message, "could not prepare a forward again");
            return None;
        }
    };
    let old = std::mem::replace(
        &mut *forward.route.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
        (route.clone(), Arc::downgrade(&link)),
    );
    if let Some(old) = old.0 {
        old.stop();
    }
    // A member's route belonged to the old listener: start from the direct one and choose again.
    if let Some((via, direct)) = forward.via.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take() {
        direct.stop();
        hop::release(network, &forward.machine_id, &via.machine_id, forward.id);
    }
    if let Some(route) = &route {
        tokio::spawn(route.clone().keep_warm(network.clone()));
        tokio::spawn(hop::use_booked(network.clone(), forward.clone()));
    }
    tracing::info!(target: TARGET, machine = %forward.machine_id, remote_port = forward.remote_port, direct = route.is_some(), "prepared a forward again on a new session");
    route
}

/// New connections of `forward` go through a member's `route` from now on; `direct` is kept to return to.
fn take_route(network: &Arc<Network>, forward: &Arc<Forward>, direct: &Arc<direct::Route>, route: Arc<direct::Route>) {
    let Some(hop) = route.hop.clone() else { return };
    {
        let mut held = forward.route.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // The session changed while measuring: that route is gone with it.
        if !held.0.as_ref().is_some_and(|current| Arc::ptr_eq(current, direct)) {
            route.stop();
            hop::release(network, &forward.machine_id, &hop.machine_id, forward.id);
            return;
        }
        held.0 = Some(route.clone());
        // Under the route's lock, so a new session never sees the route without its member, or the reverse.
        *forward.via.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some((PortRouteVia { machine_id: hop.machine_id.clone(), machine_name: hop.machine_name.clone() }, direct.clone()));
    }
    tokio::spawn(route.keep_warm(network.clone()));
    announce(network);
    tracing::info!(target: TARGET, machine = %forward.machine_id, via = %hop.machine_name, "a forward now goes through another member");
}

/// A member's route stopped answering: back to the direct route, and that member is not chosen again for a while.
fn fall_back(network: &Arc<Network>, forward: &Arc<Forward>, failed: &Arc<direct::Route>) -> Option<Arc<direct::Route>> {
    let mut held = forward.route.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if !held.0.as_ref().is_some_and(|current| Arc::ptr_eq(current, failed)) {
        return held.0.clone();
    }
    let (via, direct) = forward.via.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take()?;
    held.0 = Some(direct.clone());
    drop(held);
    failed.stop();
    hop::forget(network, &forward.machine_id);
    hop::release(network, &forward.machine_id, &via.machine_id, forward.id);
    // The direct route's ready sockets were kept warm all along.
    announce(network);
    tracing::info!(target: TARGET, machine = %forward.machine_id, via = %via.machine_name, "a member's route stopped answering; back to the direct route");
    Some(direct)
}

/// Tell clients about a forward's load while it runs and [`load::looks_slow`], after [`load::ANNOUNCE_AFTER`] and at most
/// every [`load::ANNOUNCE_EVERY`], and once more when it stops showing. A slow load, or the end of a load, is also when
/// the forward's routes through other members are measured ([`hop::choose`]), so probes never compete with a fast page.
fn watch_load(network: &Arc<Network>, forward: &Arc<Forward>) {
    if !forward.load.start_ticking() {
        return;
    }
    let (network, forward) = (network.clone(), forward.clone());
    tokio::spawn(async move {
        let mut announced: Option<Instant> = None;
        let mut measured = false;
        loop {
            tokio::select! {
                _ = forward.cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
            let view = forward.load.view();
            let showing =
                view.as_ref().is_some_and(|load| load.elapsed_ms >= load::ANNOUNCE_AFTER.as_millis() as u64 && load::looks_slow(load));
            if showing {
                if announced.is_none_or(|at| at.elapsed() >= load::ANNOUNCE_EVERY) {
                    announce(&network);
                    announced = Some(Instant::now());
                }
                if !measured {
                    measured = true;
                    tokio::spawn(hop::choose(network.clone(), forward.clone()));
                }
            } else if announced.take().is_some() {
                // The load ended or no longer looks slow: say so once, so the notice goes away.
                announce(&network);
            }
            if view.is_none() && !forward.load.active() {
                break;
            }
        }
        forward.load.stop_ticking();
        if announced.is_some() {
            announce(&network);
        }
        if !measured && !forward.cancel.is_cancelled() {
            tokio::spawn(hop::choose(network.clone(), forward.clone()));
        }
        // A request that began while this task was ending starts it again.
        if forward.load.active() && !forward.cancel.is_cancelled() {
            watch_load(&network, &forward);
        }
    });
}

/// Open the HTTPS (HTTP/2) listener of a forward, once.
async fn start_https(network: &Arc<Network>, forward: &Arc<Forward>) -> Result<(), RpcError> {
    let _starting = forward.https_start.lock().await;
    if forward.https.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).is_some() {
        return Ok(());
    }
    let port = fast::start(network, forward).await?;
    let cert = network.ports.certificate().map_err(|error| RpcError::internal(format!("{error:#}")))?;
    *forward.https.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((port, cert));
    announce(network);
    tracing::info!(target: TARGET, machine = %forward.machine_id, remote_port = forward.remote_port, https_port = port, "serving a forward over HTTP/2");
    Ok(())
}

/// Start at the remote port, then try each higher port. An explicitly chosen port is exact.
async fn bind_local(remote: u16, local: Option<u16>) -> Result<TcpListener, RpcError> {
    bind_first_free(remote, local, |port| TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))).await
}

/// The port choice of `bind_local`, with the bind passed in so it can be tested without real ports:
/// the asked-for local port or nothing, else the remote port number or the first free one above it.
async fn bind_first_free<T, F, Fut>(remote: u16, local: Option<u16>, mut bind: F) -> Result<T, RpcError>
where
    F: FnMut(u16) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<T>>,
{
    let start = local.unwrap_or(remote);
    let end = if local.is_some() { start } else { u16::MAX };
    for port in start..=end {
        match bind(port).await {
            Ok(listener) => return Ok(listener),
            Err(error) if local.is_none() && error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => return Err(RpcError::new(codes::CONFLICT, format!("Could not use that local port: {error}"))),
        }
    }
    Err(RpcError::new(codes::CONFLICT, "No local port is free. Stop a forward and try again."))
}

/// What a preview target names on another machine: the port of its dev server, and the path to open.
pub fn parse_server_target(target: &str) -> Result<(u16, String), String> {
    let trimmed = target.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else if trimmed.chars().all(|c| c.is_ascii_digit()) && !trimmed.is_empty() {
        format!("http://localhost:{trimmed}")
    } else {
        format!("http://{trimmed}")
    };
    let url = reqwest::Url::parse(&with_scheme).map_err(|_| {
        "Give the dev server's address on that machine, such as http://localhost:5173. Files on another machine are not previewed here: send them with kybern files send first.".to_string()
    })?;
    let host = url.host_str().unwrap_or_default();
    if !matches!(url.scheme(), "http" | "https") || !(host == "localhost" || host == "127.0.0.1" || host == "[::1]" || host == "::1") {
        return Err("Only a dev server on that machine's own localhost can be previewed through it, such as http://localhost:5173.".into());
    }
    let port = url.port_or_known_default().ok_or_else(|| "Add the port of the dev server, such as http://localhost:5173.".to_string())?;
    let mut path = url.path().to_string();
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    if let Some(fragment) = url.fragment() {
        path.push('#');
        path.push_str(fragment);
    }
    Ok((port, path))
}

async fn accept_loop(network: Arc<Network>, shutdown: CancellationToken, forward: Arc<Forward>, listener: TcpListener) {
    let idle_limit = network.timing.ports_idle;
    let mut idle_since = Instant::now();
    let tick = (idle_limit / 4).clamp(Duration::from_millis(50), Duration::from_secs(30));
    loop {
        tokio::select! {
            _ = forward.cancel.cancelled() => break,
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                let Ok((socket, _)) = accepted else { continue };
                idle_since = Instant::now();
                let network = network.clone();
                let forward = forward.clone();
                tokio::spawn(async move { connection(network, forward, socket).await });
            }
            _ = tokio::time::sleep(tick) => {
                if forward.connections.load(Ordering::Relaxed) > 0 {
                    idle_since = Instant::now();
                } else if idle_since.elapsed() >= idle_limit {
                    tracing::info!(target: TARGET, machine = %forward.machine_id, port = forward.remote_port, "closing an idle port forward");
                    break;
                }
            }
        }
    }
    forward.cancel.cancel();
    if forward.prepared
        && let Some(link) = network.peers.link(&forward.machine_id)
    {
        let _ = link.notify(PeerPortsRelease::NAME, json!({"forward_id": forward.id})).await;
    }
    if let Some(via) = forward.via_now() {
        hop::release(&network, &forward.machine_id, &via.machine_id, forward.id);
    }
    network.ports.remove(forward.id);
    announce(&network);
}

struct Counted(Arc<Forward>);

impl Counted {
    fn new(forward: &Arc<Forward>) -> Self {
        forward.connections.fetch_add(1, Ordering::Relaxed);
        Self(forward.clone())
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn connection(network: Arc<Network>, forward: Arc<Forward>, mut socket: TcpStream) {
    let _count = Counted::new(&forward);
    let _ = socket.set_nodelay(true);
    let Some(link) = network.peers.link(&forward.machine_id) else { return };
    if let Some(route) = current_route(&network, &forward).await {
        // A browser speaking HTTP/1.x is served here, every request over the forward's one HTTP/2 connection: compressed,
        // counted and cached. Anything else stays a byte-for-byte socket.
        let opened = if route.ready_sockets() {
            match http::sniff(&network, &route, &socket).await {
                http::Sniffed::Http => return http::serve_plain(network, forward, socket).await,
                http::Sniffed::Raw(opened) => opened,
            }
        } else {
            None
        };
        // Over the limit, the browser's connection waits for a place rather than being closed.
        let key = format!("ports:{}", link.machine_id);
        let Some(_slot) = network.relays.acquire_wait(&key, relay::MAX_STREAMS_PER_CLIENT, direct::QUEUE_WAIT, &forward.cancel).await
        else {
            return;
        };
        let remote = match opened {
            Some(opened) => Some(opened),
            None => tokio::select! {
                _ = forward.cancel.cancelled() => return,
                remote = route.open(&network, direct::MODE_RAW) => remote,
            },
        };
        // A member's route that stopped answering: the direct route takes this connection and the next ones.
        let remote = match remote {
            None if route.hop.is_some() => match fall_back(&network, &forward, &route) {
                Some(direct) => direct.open(&network, direct::MODE_RAW).await,
                None => None,
            },
            remote => remote,
        };
        if let Some(mut remote) = remote {
            tokio::select! {
                _ = forward.cancel.cancelled() => {},
                _ = link.closed() => {},
                _ = tokio::io::copy_bidirectional(&mut socket, &mut remote) => {},
            }
            return;
        }
    }
    let (stream, incoming, slot) = match open_stream(&link, &network, forward.remote_port).await {
        Ok(opened) => opened,
        Err(error) => {
            tracing::debug!(target: TARGET, machine = %forward.machine_id, error = %error.message, "could not open a forwarded connection");
            return;
        }
    };
    pump(link, stream, incoming, socket, slot, forward.cancel.clone()).await;
}

/// Open a stream to a port of the machine on the other end of `link`.
async fn open_stream(link: &Arc<PeerLink>, network: &Network, port: u16) -> Result<(Arc<Stream>, mpsc::Receiver<Chunk>, Slot), RpcError> {
    let cancel = link.token();
    let slot = network
        .relays
        .acquire_wait(&format!("ports:{}", link.machine_id), relay::MAX_STREAMS_PER_CLIENT, direct::QUEUE_WAIT, &cancel)
        .await
        .ok_or_else(|| RpcError::new(codes::CONFLICT, "too many forwarded connections"))?;
    // The target refuses at once when this machine holds all its places there (it never waits while holding one of the
    // session's request places); the connection waits here instead, asking again as places may have freed up.
    let deadline = Instant::now() + direct::QUEUE_WAIT;
    let mut pause = OPEN_RETRY_FIRST;
    loop {
        let (stream, incoming) = link.relays.create(None, StreamKind::Ports, None).ok_or_else(|| RpcError::internal("stream id clash"))?;
        let open = PeerPortsOpenParams { stream_id: stream.id, port };
        let error = match tokio::time::timeout(OPEN_TIMEOUT, link.call::<PeerPortsOpen>(open)).await {
            Ok(Ok(_)) => return Ok((stream, incoming, slot)),
            Ok(Err(error)) => error,
            Err(_) => RpcError::internal("the machine did not open the connection in time"),
        };
        link.relays.remove(stream.id);
        if error.code != codes::CONFLICT || Instant::now() + pause >= deadline {
            return Err(error);
        }
        tokio::select! {
            _ = cancel.cancelled() => return Err(error),
            _ = tokio::time::sleep(pause) => {}
        }
        pause = (pause * 2).min(OPEN_RETRY_MAX);
    }
}

/// How long a connection waits before asking a busy target again, doubling up to [`OPEN_RETRY_MAX`].
const OPEN_RETRY_FIRST: Duration = Duration::from_millis(50);
const OPEN_RETRY_MAX: Duration = Duration::from_secs(1);

// ---- the target: peer.ports.open ----

/// The ports of this machine's own daemon, which a forward may never reach.
pub fn own_ports(state: &AppState) -> Vec<u16> {
    let mut ports = vec![DEFAULT_PORT, state.port.load(Ordering::Relaxed)];
    if let Some(addr) = *state.listen_addr.read().unwrap_or_else(|poisoned| poisoned.into_inner()) {
        ports.push(addr.port());
    }
    if let Some(port) = state.network.listening_port() {
        ports.push(port);
    }
    ports.push(state.settings.get().network.port);
    ports.retain(|port| *port != 0);
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// `peer.ports.open`: connect a stream to a loopback port of this machine. Over the stream limits it refuses at once with
/// `CONFLICT`: waiting here would hold one of the session's few request places, and every other request of the session
/// (presence, operations, relays) would queue behind it. The caller waits and asks again.
pub async fn handle_open(state: &AppState, link: &Arc<PeerLink>, params: PeerPortsOpenParams) -> Result<Value, RpcError> {
    validate_port(state, params.port)?;
    let slot = state
        .network
        .relays
        .acquire_within(&format!("inbound:{}", link.machine_id), relay::MAX_INBOUND_PER_MEMBER)
        .ok_or_else(|| RpcError::new(codes::CONFLICT, "too many forwarded connections on this machine"))?;
    let socket = connect_loopback(params.port).await?;
    let _ = socket.set_nodelay(true);
    let (stream, incoming) = link
        .relays
        .create(Some(params.stream_id), StreamKind::Ports, Some(Scope::MachinePorts))
        .ok_or_else(|| RpcError::invalid_params("that stream id is in use"))?;
    let link = link.clone();
    tokio::spawn(async move {
        let cancel = CancellationToken::new();
        pump(link, stream, incoming, socket, slot, cancel).await;
    });
    Ok(json!({}))
}

fn validate_port(state: &AppState, port: u16) -> Result<(), RpcError> {
    if port == 0 {
        return Err(RpcError::invalid_params("port must be 1-65535"));
    }
    if own_ports(state).contains(&port) {
        return Err(RpcError::new(codes::FORBIDDEN, format!("Port {port} belongs to Kybern itself and is not forwarded.")));
    }
    Ok(())
}

/// `127.0.0.1`, else `::1`: a dev server may listen on either.
async fn connect_loopback(port: u16) -> Result<TcpStream, RpcError> {
    let attempt = |addr: SocketAddr| async move { tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await };
    if let Ok(Ok(socket)) = attempt(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await {
        return Ok(socket);
    }
    if let Ok(Ok(socket)) = attempt(SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port))).await {
        return Ok(socket);
    }
    Err(RpcError::new(codes::NOT_FOUND, format!("Nothing is listening on port {port} of that machine.")))
}

// ---- the stream ----

async fn send_data(link: &PeerLink, stream: &Stream, bytes: &[u8], text: bool) -> bool {
    relay::send_frame(link, stream, bytes, text).await
}

async fn grant(link: &PeerLink, stream: &Stream, bytes: usize) {
    if bytes == 0 {
        return;
    }
    let bytes = frame_cost(bytes);
    let params =
        PeerRelayDataParams { stream_id: stream.id, data: String::new(), credit: Some(bytes.min(u32::MAX as usize) as u32), text: false };
    let _ = link.notify(PeerPortsData::NAME, serde_json::to_value(params).unwrap_or(Value::Null)).await;
}

/// After the stream's data, on the same lane: a close must not overtake bytes still queued.
async fn send_close(link: &PeerLink, stream_id: u64) {
    let params = PeerRelayCloseParams { stream_id, reason: None };
    let _ = link.notify_bulk(PeerPortsClose::NAME, serde_json::to_value(params).unwrap_or(Value::Null)).await;
}

/// Carry bytes between a TCP socket and a stream until either side ends. Reading and writing run apart, so a
/// transfer in one direction never waits on credit the other direction has to grant. An empty text frame
/// means "this side will send no more" (a half-close); the stream ends when both sides have.
async fn pump(
    link: Arc<PeerLink>,
    stream: Arc<Stream>,
    mut incoming: mpsc::Receiver<Chunk>,
    socket: TcpStream,
    slot: Slot,
    stop: CancellationToken,
) {
    let (mut reader, mut writer) = socket.into_split();
    let done = CancellationToken::new();
    let read_task = {
        let (link, stream, done, stop) = (link.clone(), stream.clone(), done.clone(), stop.clone());
        tokio::spawn(async move {
            let mut buffer = vec![0u8; READ_CHUNK];
            let finished_cleanly = loop {
                let read = tokio::select! {
                    read = reader.read(&mut buffer) => read,
                    _ = stream.cancel.cancelled() => break false,
                    _ = link.closed() => break false,
                    _ = stop.cancelled() => break false,
                    _ = done.cancelled() => break false,
                };
                match read {
                    Ok(0) => break true,
                    Ok(read) => {
                        if !send_data(&link, &stream, &buffer[..read], false).await {
                            break false;
                        }
                    }
                    Err(_) => break false,
                }
            };
            if finished_cleanly {
                // Tell the other side no more is coming; it may still answer.
                let _ = send_data(&link, &stream, &[], true).await;
            }
            finished_cleanly
        })
    };
    let mut remote_open = true;
    while remote_open {
        // Bytes already received are written before an end or a cancel is honoured.
        let chunk = tokio::select! {
            biased;
            chunk = incoming.recv() => chunk,
            _ = stream.cancel.cancelled() => None,
            _ = link.closed() => None,
            _ = stop.cancelled() => None,
            _ = done.cancelled() => None,
        };
        match chunk {
            Some(Chunk::Binary(bytes)) | Some(Chunk::Text(bytes)) if !bytes.is_empty() => {
                let size = bytes.len();
                if writer.write_all(&bytes).await.is_err() {
                    break;
                }
                grant(&link, &stream, size).await;
            }
            // Half-close from the other side: end our writing, keep reading until it ends.
            Some(Chunk::Text(_)) | Some(Chunk::Binary(_)) => {
                let _ = writer.shutdown().await;
                remote_open = false;
            }
            Some(Chunk::End) | None => break,
        }
    }
    if remote_open {
        // The stream ended abruptly: stop the reader too.
        done.cancel();
    }
    let reader_clean = read_task.await.unwrap_or(false);
    if remote_open || !reader_clean {
        let _ = writer.shutdown().await;
    }
    // The reader may still be waiting for the local side after a remote half-close; wait for it to finish
    // or for the stream to be cancelled, then close.
    send_close(&link, stream.id).await;
    link.relays.remove(stream.id);
    drop(slot);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_port_selection_preserves_free_ports_and_skips_occupied_ports() {
        // The choice itself, against a fixed set of taken ports: other tests binding ephemeral
        // ports cannot take a "free" one in between, as they could with real sockets.
        let taken = [5173u16, 5174, 5176, u16::MAX];
        let bind = |port: u16| {
            let tried = port;
            async move { if taken.contains(&tried) { Err(std::io::Error::from(std::io::ErrorKind::AddrInUse)) } else { Ok(tried) } }
        };
        assert_eq!(bind_first_free(3000, None, bind).await.unwrap(), 3000, "a free remote port number is kept locally");
        assert_eq!(bind_first_free(5173, None, bind).await.unwrap(), 5175, "an occupied one moves to the next free port");
        assert_eq!(bind_first_free(5176, None, bind).await.unwrap(), 5177);
        assert_eq!(bind_first_free(5173, Some(4000), bind).await.unwrap(), 4000, "an asked-for local port is used as is");
        assert_eq!(bind_first_free(5173, Some(5174), bind).await.unwrap_err().code, codes::CONFLICT, "and never replaced");
        assert_eq!(bind_first_free(u16::MAX, None, bind).await.unwrap_err().code, codes::CONFLICT);
        // Any error other than "in use" is reported, not skipped.
        let denied = |_port: u16| async { Err::<u16, _>(std::io::Error::from(std::io::ErrorKind::PermissionDenied)) };
        assert_eq!(bind_first_free(3000, None, denied).await.unwrap_err().code, codes::CONFLICT);

        // And with real sockets, on a port this test holds: an asked-for port is refused, and an
        // automatic choice moves above it.
        let reserved = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = reserved.local_addr().unwrap().port();
        assert!(bind_local(port, Some(port)).await.is_err());
        if port < u16::MAX {
            assert!(bind_local(port, None).await.unwrap().local_addr().unwrap().port() > port);
        }
    }

    #[test]
    fn a_preview_target_on_another_machine_is_a_localhost_dev_server() {
        assert_eq!(parse_server_target("http://localhost:5173").unwrap(), (5173, "/".to_string()));
        assert_eq!(parse_server_target("localhost:3000/app?x=1").unwrap(), (3000, "/app?x=1".to_string()));
        assert_eq!(parse_server_target("5173").unwrap(), (5173, "/".to_string()));
        assert_eq!(parse_server_target("http://127.0.0.1:8080/a#top").unwrap(), (8080, "/a#top".to_string()));
        for bad in ["https://example.com", "mockups/index.html", "javascript:alert(1)", "http://192.168.1.4:3000", ""] {
            assert!(parse_server_target(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_port_forward_view_reports_its_connections() {
        let forward = Forward {
            id: Uuid::now_v7(),
            machine_id: "m".into(),
            machine_name: "satyamas".into(),
            remote_port: 5173,
            local_port: 40000,
            created_at: chrono::Utc::now(),
            route: std::sync::Mutex::new((None, std::sync::Weak::new())),
            rerouting: tokio::sync::Mutex::new(()),
            prepared: false,
            rtt_ms: None,
            https: std::sync::Mutex::new(None),
            https_start: tokio::sync::Mutex::new(()),
            tunnel: fast::Tunnel::default(),
            caching: false,
            cache: cache::Cache::default(),
            load: load::LoadMeter::default(),
            via: std::sync::Mutex::new(None),
            choosing: Default::default(),
            preview: Default::default(),
            connections: AtomicU32::new(0),
            cancel: CancellationToken::new(),
        };
        let forward = Arc::new(forward);
        let guard = Counted::new(&forward);
        assert_eq!(forward.view().connections, 1);
        drop(guard);
        assert_eq!(forward.view().connections, 0);
    }
}
