//! The peer manager (design §3.1 to §3.3): discovery ticks, the dial rule, heartbeat and backoff,
//! and the one JSON-RPC link per pair of members.
//!
//! * **Discovery.** Every tick (60 s while a client shows Machines, else 5 min, and once on start
//!   and after any session drops) the worker reads `tailscale status`, probes online peers that are
//!   not members for the well-known endpoint and records the answers as the *found* list.
//! * **Dial rule.** For every pair the member with the smaller machine id dials; the other waits.
//!   A dial verifies the target's well-known proof against the key in the membership set, opens a
//!   session ([`super::session`]) and connects a WebSocket with the grant.
//! * **One link per pair, used both ways.** [`PeerLink`] sends and answers requests on one
//!   socket. The listening side wraps its connection's outbox; the dialing side owns the socket.
//! * **Heartbeat and backoff.** A WebSocket ping every 30 s; a link silent for three missed pings
//!   is dead. Reconnects back off 2, 4, 8 … 300 s and start over when the tailnet's status changes,
//!   when this machine wakes from sleep, or when a member that was away says it is back
//!   ([`super::wake_watch`]).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt};
use kybern_protocol::methods::*;
use kybern_protocol::*;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::relay::{LinkRelays, StreamKind};
use super::session::{self, Target};
use super::tailscale::{TsNode, TsStatus, Whois};
use super::well_known::{self, WellKnown};
use super::{ApplyOutcome, MembershipError, Network, TARGET};
use crate::state::AppState;

const SESSION_TARGET: &str = "kybern::network::session";
const DISCOVERY_TARGET: &str = "kybern::network::discovery";
/// The ports a peer's network listener may hold (§3.1): the default and the next six.
pub const PORT_RANGE: std::ops::RangeInclusive<u16> = DEFAULT_PORT..=DEFAULT_PORT + 6;
const MAX_CONCURRENT_PROBES: usize = 8;
/// How long the session with a removed machine stays open for it to take the operations that removed it.
const FAREWELL_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_INBOUND_REQUESTS: usize = 16;
/// Requests that wait for one of the [`MAX_INBOUND_REQUESTS`] places instead of being refused.
const MAX_QUEUED_REQUESTS: usize = 256;

// ---- the link ----

/// Where a link's frames go: the listener's connection outbox, or the dialer's socket writer.
#[async_trait]
pub trait FrameSink: Send + Sync {
    /// Queue one JSON frame; `false` once the socket is gone.
    async fn send(&self, frame: Value) -> bool;

    /// Queue one frame of stream data (relay and port bytes; binary between machines that list `relay.frames.v2`). It
    /// waits for room in the session's [`BulkLane`] as long as the route needs and never ends the session for being
    /// slow; `false` once the socket is gone. A sink without a lane sends JSON through [`FrameSink::send`] and refuses
    /// binary frames.
    async fn send_bulk(&self, frame: Outgoing) -> bool {
        match frame {
            Outgoing::Text(text) => match serde_json::from_str(&text) {
                Ok(value) => self.send(value).await,
                Err(_) => false,
            },
            Outgoing::Binary(_) => false,
        }
    }
}

/// A frame for a session socket.
pub enum Outgoing {
    Text(String),
    Binary(Vec<u8>),
}

impl Outgoing {
    fn size(&self) -> usize {
        match self {
            Outgoing::Text(text) => text.len(),
            Outgoing::Binary(bytes) => bytes.len(),
        }
    }
}

/// Stream data one session socket queues at most, in bytes, apart from its control frames. A stream waits here for room
/// (the route is slow, not broken), so no timeout applies, and the writer sends control frames first ([`next_frame`]):
/// requests, answers and credit grants never queue behind data.
pub const BULK_BYTES: usize = 4 * 1024 * 1024;
const BULK_FRAMES: usize = 1024;

/// The data lane of one session socket.
pub struct BulkLane {
    sender: mpsc::Sender<BulkFrame>,
    budget: Arc<tokio::sync::Semaphore>,
    closed: CancellationToken,
}

/// A queued data frame. Its share of the lane's budget is held until the writer has sent it.
pub struct BulkFrame {
    message: Outgoing,
    permit: tokio::sync::OwnedSemaphorePermit,
}

impl BulkFrame {
    /// The message, and the budget it holds: drop that once the message is on the socket.
    pub fn into_parts(self) -> (Outgoing, tokio::sync::OwnedSemaphorePermit) {
        (self.message, self.permit)
    }
}

impl BulkLane {
    /// A lane whose waits end once `closed` fires, and the receiver its writer drains.
    pub fn new(closed: CancellationToken) -> (Self, mpsc::Receiver<BulkFrame>) {
        let (sender, receiver) = mpsc::channel(BULK_FRAMES);
        (Self { sender, budget: Arc::new(tokio::sync::Semaphore::new(BULK_BYTES)), closed }, receiver)
    }

    /// Queue a frame, waiting for room. `false` once the socket is gone.
    pub async fn send(&self, message: Outgoing) -> bool {
        let size = message.size().clamp(1, BULK_BYTES) as u32;
        let queue = async {
            let permit = self.budget.clone().acquire_many_owned(size).await.ok()?;
            self.sender.send(BulkFrame { message, permit }).await.ok()
        };
        tokio::select! {
            _ = self.closed.cancelled() => false,
            queued = queue => queued.is_some(),
        }
    }
}

/// The next frame for a session socket: a control frame when one waits, else stream data, so a backlog of data on a slow
/// route never delays a request, an answer or a credit grant. `None` once the control channel is closed.
pub async fn next_frame<C>(control: &mut mpsc::Receiver<C>, bulk: &mut mpsc::Receiver<BulkFrame>) -> Option<Result<C, BulkFrame>> {
    tokio::select! {
        biased;
        frame = control.recv() => frame.map(Ok),
        Some(frame) = bulk.recv() => Some(Err(frame)),
    }
}

struct ChannelSink {
    control: mpsc::Sender<Outgoing>,
    bulk: BulkLane,
}

#[async_trait]
impl FrameSink for ChannelSink {
    async fn send(&self, frame: Value) -> bool {
        match serde_json::to_string(&frame) {
            Ok(text) => self.control.send(Outgoing::Text(text)).await.is_ok(),
            Err(_) => false,
        }
    }

    async fn send_bulk(&self, frame: Outgoing) -> bool {
        self.bulk.send(frame).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// This machine opened the session.
    Dialed,
    /// The other machine opened it to this one.
    Accepted,
}

/// One session with one member, requests and notifications flowing both ways.
pub struct PeerLink {
    pub machine_id: String,
    pub direction: Direction,
    /// The grant that authenticated an accepted link.
    pub grant_id: Option<Uuid>,
    /// What the session allows: the scopes the target granted us (dialed) or we granted (accepted).
    pub scopes: Vec<Scope>,
    pub opened_at: Instant,
    sink: Arc<dyn FrameSink>,
    pending: Mutex<HashMap<i64, oneshot::Sender<RpcResponse>>>,
    next_id: AtomicI64,
    closed: CancellationToken,
    last_frame: Mutex<Instant>,
    inbound: Arc<tokio::sync::Semaphore>,
    /// Requests waiting for an `inbound` place.
    queued: Arc<std::sync::atomic::AtomicUsize>,
    /// The other machine reads binary relay frames (it listed `relay.frames.v2`).
    binary: std::sync::atomic::AtomicBool,
    /// The machine is no longer a member and is being told so before the link closes ([`Network::say_farewell`]).
    farewell: std::sync::atomic::AtomicBool,
    pub relays: LinkRelays,
}

impl PeerLink {
    pub fn new(
        machine_id: String,
        direction: Direction,
        grant_id: Option<Uuid>,
        scopes: Vec<Scope>,
        sink: Arc<dyn FrameSink>,
    ) -> Arc<Self> {
        Arc::new(Self {
            machine_id,
            direction,
            grant_id,
            scopes,
            opened_at: Instant::now(),
            sink,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            closed: CancellationToken::new(),
            last_frame: Mutex::new(Instant::now()),
            inbound: Arc::new(tokio::sync::Semaphore::new(MAX_INBOUND_REQUESTS)),
            queued: Arc::default(),
            binary: std::sync::atomic::AtomicBool::new(false),
            farewell: std::sync::atomic::AtomicBool::new(false),
            relays: LinkRelays::new(direction),
        })
    }

    fn pending(&self) -> MutexGuard<'_, HashMap<i64, oneshot::Sender<RpcResponse>>> {
        self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Send a request and wait for its answer.
    pub async fn call_raw(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending().insert(id, tx);
        let request = RpcRequest::new(RpcId::Number(id), method, params);
        if !self.sink.send(serde_json::to_value(&request).unwrap_or(Value::Null)).await {
            self.pending().remove(&id);
            return Err(RpcError::internal("the session is closed"));
        }
        let answer = tokio::select! {
            answer = rx => answer.map_err(|_| RpcError::internal("the session closed before it answered"))?,
            _ = self.closed.cancelled() => { self.pending().remove(&id); return Err(RpcError::internal("the session is closed")); }
            _ = tokio::time::sleep(Duration::from_secs(60)) => { self.pending().remove(&id); return Err(RpcError::internal("the machine did not answer in time")); }
        };
        match (answer.result, answer.error) {
            (_, Some(error)) => Err(error),
            (Some(result), None) => Ok(result),
            (None, None) => Ok(Value::Null),
        }
    }

    pub async fn call<M: Method>(&self, params: M::Params) -> Result<M::Result, RpcError> {
        let value = self.call_raw(M::NAME, serde_json::to_value(params).map_err(RpcError::internal)?).await?;
        serde_json::from_value(value).map_err(RpcError::internal)
    }

    /// What the other machine listed in its presence: binary frames and the larger stream window.
    pub fn set_peer_features(&self, features: &[String]) {
        let v2 = features.iter().any(|feature| feature == features::RELAY_FRAMES_V2);
        self.binary.store(v2, Ordering::Relaxed);
        self.relays.set_large_window(v2);
    }

    pub fn binary_frames(&self) -> bool {
        self.binary.load(Ordering::Relaxed)
    }

    /// Send a binary data frame on the session's data lane. `false` when the session is gone.
    pub async fn send_binary(&self, frame: Vec<u8>) -> bool {
        self.sink.send_bulk(Outgoing::Binary(frame)).await
    }

    /// Send a data notification (`peer.relay.data` / `peer.ports.data` with base64 data, for a machine that reads no
    /// binary frames) on the session's data lane. `false` when the session is gone.
    pub async fn notify_bulk(&self, method: &str, params: Value) -> bool {
        match serde_json::to_string(&RpcNotification::new(method, params)) {
            Ok(text) => self.sink.send_bulk(Outgoing::Text(text)).await,
            Err(_) => false,
        }
    }

    /// Send a notification (no answer expected). `false` when the session is gone.
    pub async fn notify(&self, method: &str, params: Value) -> bool {
        let note = RpcNotification::new(method, params);
        self.sink.send(serde_json::to_value(&note).unwrap_or(Value::Null)).await
    }

    async fn respond(&self, response: RpcResponse) {
        let _ = self.sink.send(serde_json::to_value(&response).unwrap_or(Value::Null)).await;
    }

    /// Hand an answer to the request waiting for it.
    pub fn deliver(&self, response: RpcResponse) {
        if let RpcId::Number(id) = response.id
            && let Some(tx) = self.pending().remove(&id)
        {
            let _ = tx.send(response);
        }
    }

    /// Any frame, pong included, proves the other side is alive.
    pub fn touch(&self) {
        *self.last_frame.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Instant::now();
    }

    pub fn idle_for(&self) -> Duration {
        self.last_frame.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).elapsed()
    }

    pub fn close(&self) {
        self.closed.cancel();
        self.relays.close_all();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.is_cancelled()
    }

    /// Whether the link is telling a removed machine of its removal; it closes itself once done.
    pub fn saying_farewell(&self) -> bool {
        self.farewell.load(Ordering::SeqCst)
    }

    /// Resolves when the link is closed.
    pub async fn closed(&self) {
        self.closed.cancelled().await;
    }

    pub fn token(&self) -> CancellationToken {
        self.closed.clone()
    }
}

// ---- the table ----

/// What the peer manager knows about one member.
#[derive(Debug, Clone, Default)]
pub struct PeerStatus {
    pub online: bool,
    pub direction: Option<Direction>,
    pub scopes: Vec<Scope>,
    pub since: Option<Instant>,
    pub last_seen: Option<DateTime<Utc>>,
    pub presence: Option<PeerPresence>,
    pub last_error: Option<String>,
    /// Consecutive failed attempts; the next wait is `backoff_min * 2^failures`, capped.
    pub failures: u32,
}

/// A machine that answered the well-known endpoint and is not a member.
#[derive(Debug, Clone)]
pub struct FoundEntry {
    pub node: TsNode,
    pub whois: Option<Whois>,
    pub descriptor: WellKnown,
    pub port: u16,
    pub probed_at: Instant,
}

#[derive(Default)]
struct PeersInner {
    links: HashMap<String, Arc<PeerLink>>,
    status: HashMap<String, PeerStatus>,
    next_attempt: HashMap<String, Instant>,
    found: HashMap<String, FoundEntry>,
    /// Nodes probed recently, answered or not.
    probed: HashMap<String, Instant>,
    /// The tailnet's online set when the worker last looked.
    signature: String,
    /// The capabilities a policy widening already renewed an accepted session for, per machine.
    widened: HashMap<String, Vec<Scope>>,
    /// When a node last nudged this machine, so one chatty node cannot wake the worker without end.
    nudges: HashMap<String, Instant>,
    /// Nodes that nudged this machine and did not answer yet: when, and how many times they were retried.
    awaited: HashMap<String, (Instant, u32)>,
    /// When this machine last nudged a member that dials it.
    nudged_out: HashMap<String, Instant>,
    /// Cancelled to give up every dial in flight: they started on a network that changed since.
    attempts: CancellationToken,
    /// When the worker's pass that runs now started.
    pass_started: Option<Instant>,
}

#[derive(Default)]
pub struct Peers {
    inner: Mutex<PeersInner>,
}

impl Peers {
    fn lock(&self) -> MutexGuard<'_, PeersInner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The open link to a member.
    pub fn link(&self, machine_id: &str) -> Option<Arc<PeerLink>> {
        self.lock().links.get(machine_id).filter(|link| !link.is_closed()).cloned()
    }

    pub fn links(&self) -> Vec<Arc<PeerLink>> {
        self.lock().links.values().filter(|link| !link.is_closed()).cloned().collect()
    }

    pub fn status(&self, machine_id: &str) -> PeerStatus {
        let inner = self.lock();
        let mut status = inner.status.get(machine_id).cloned().unwrap_or_default();
        let link = inner.links.get(machine_id).filter(|link| !link.is_closed());
        status.online = link.is_some();
        status.direction = link.map(|link| link.direction);
        status.scopes = link.map(|link| link.scopes.clone()).unwrap_or_default();
        status.since = link.map(|link| link.opened_at);
        status
    }

    /// Machines found on the tailnet that are not members, newest answers only.
    pub fn found(&self) -> Vec<FoundEntry> {
        self.lock().found.values().cloned().collect()
    }

    /// Register a link. A second link to the same member replaces the first, which is closed.
    pub fn register(&self, link: Arc<PeerLink>) {
        let old = {
            let mut inner = self.lock();
            let inner = &mut *inner;
            let status = inner.status.entry(link.machine_id.clone()).or_default();
            status.last_seen = Some(Utc::now());
            status.last_error = None;
            // The machine is here: whatever backoff it earned while it was away is over.
            if link.direction == Direction::Accepted {
                status.failures = 0;
                inner.next_attempt.remove(&link.machine_id);
            }
            inner.links.insert(link.machine_id.clone(), link)
        };
        if let Some(old) = old {
            old.close();
        }
    }

    /// Forget a link that ended. Only the link itself is removed; a newer one stays.
    pub fn unregister(&self, link: &Arc<PeerLink>, error: Option<String>) {
        let mut inner = self.lock();
        if inner.links.get(&link.machine_id).is_some_and(|current| Arc::ptr_eq(current, link)) {
            inner.links.remove(&link.machine_id);
            let lived = link.opened_at.elapsed();
            let status = inner.status.entry(link.machine_id.clone()).or_default();
            status.last_seen = Some(Utc::now());
            status.presence = None;
            if error.is_some() {
                status.last_error = error;
            }
            // A link that survived a couple of heartbeats was a good one: backoff starts over.
            if lived > Duration::from_secs(10) {
                status.failures = 0;
            }
        }
    }

    /// A member with an open link was seen now. `last_seen` moves at most once per
    /// [`SEEN_REFRESH`], so it stays meaningful and costs nothing per heartbeat; returns the new time
    /// when it moved, for the caller to store. A machine without a link is not touched.
    pub fn refresh_seen(&self, machine_id: &str) -> Option<DateTime<Utc>> {
        let mut inner = self.lock();
        if !inner.links.get(machine_id).is_some_and(|link| !link.is_closed()) {
            return None;
        }
        let now = Utc::now();
        let status = inner.status.entry(machine_id.to_string()).or_default();
        if status.last_seen.is_some_and(|seen| now.signed_duration_since(seen) < SEEN_REFRESH) {
            return None;
        }
        status.last_seen = Some(now);
        Some(now)
    }

    pub fn set_presence(&self, machine_id: &str, presence: PeerPresence) {
        let mut inner = self.lock();
        let status = inner.status.entry(machine_id.to_string()).or_default();
        status.presence = Some(presence);
        status.last_seen = Some(Utc::now());
    }

    /// Close the link to a member, if any (removal, a tampered operation, the network turned off).
    pub fn close(&self, machine_id: &str) {
        if let Some(link) = self.take(machine_id) {
            link.close();
        }
    }

    /// Remove a machine's link from the table without closing it.
    fn take(&self, machine_id: &str) -> Option<Arc<PeerLink>> {
        self.lock().links.remove(machine_id)
    }

    pub fn close_all(&self) {
        let links: Vec<_> = self.lock().links.drain().map(|(_, link)| link).collect();
        for link in links {
            link.close();
        }
    }

    /// Push operations to every connected member except `except`, in the background (§2.3).
    pub fn gossip(&self, ops: Vec<OpEnvelope>, except: Option<&str>) {
        if ops.is_empty() {
            return;
        }
        for link in self.links() {
            if Some(link.machine_id.as_str()) == except {
                continue;
            }
            let ops = ops.clone();
            tokio::spawn(async move {
                if let Err(error) = link.call::<PeerOpsPush>(PeerOpsPushParams { ops }).await {
                    tracing::debug!(target: SESSION_TARGET, machine = %link.machine_id, %error.message, "could not push operations");
                }
            });
        }
    }

    fn fail(&self, machine_id: &str, timing: &super::Timing, error: String) {
        let mut inner = self.lock();
        let status = inner.status.entry(machine_id.to_string()).or_default();
        status.failures = status.failures.saturating_add(1);
        status.last_error = Some(error);
        let wait = backoff(timing, status.failures);
        inner.next_attempt.insert(machine_id.to_string(), Instant::now() + wait);
    }

    /// A failed attempt at a member Tailscale lists offline: the usual backoff, but never sooner than
    /// `offline_retry`, since most of the time it really is off.
    fn fail_slow(&self, machine_id: &str, timing: &super::Timing, error: String) {
        let mut inner = self.lock();
        let status = inner.status.entry(machine_id.to_string()).or_default();
        status.failures = status.failures.saturating_add(1);
        status.last_error = Some(error);
        let wait = backoff(timing, status.failures).max(timing.offline_retry);
        inner.next_attempt.insert(machine_id.to_string(), Instant::now() + wait);
    }

    /// How long until a member that failed is dialed again, and how many attempts failed: `None` when it is due.
    fn waiting(&self, machine_id: &str) -> Option<(Duration, u32)> {
        let inner = self.lock();
        let at = inner.next_attempt.get(machine_id)?;
        let left = at.saturating_duration_since(Instant::now());
        (!left.is_zero()).then(|| (left, inner.status.get(machine_id).map_or(0, |status| status.failures)))
    }

    /// What gives up the dials that start now.
    fn attempts(&self) -> CancellationToken {
        self.lock().attempts.clone()
    }

    /// Give up every dial in flight: it started on a network that changed (a wake, a tailnet change, the switch). The
    /// pass waiting for it goes on at once, and the next one dials again. True when a pass was running.
    pub(super) fn give_up_attempts(&self) -> bool {
        let mut inner = self.lock();
        std::mem::take(&mut inner.attempts).cancel();
        inner.pass_started.is_some()
    }

    /// How long the worker's current pass has run, if one runs.
    pub(super) fn pass_running_for(&self) -> Option<Duration> {
        self.lock().pass_started.map(|at| at.elapsed())
    }

    fn set_pass_started(&self, at: Option<Instant>) {
        self.lock().pass_started = at;
    }

    fn due(&self, machine_id: &str) -> bool {
        self.lock().next_attempt.get(machine_id).is_none_or(|at| Instant::now() >= *at)
    }

    /// When the soonest scheduled attempt is due.
    fn soonest_retry(&self) -> Option<Duration> {
        let now = Instant::now();
        self.lock().next_attempt.values().filter(|at| **at > now).map(|at| *at - now).min()
    }

    /// Keep a fresh answer for a node, or forget the node when it no longer answers: the next pass
    /// looks at it again.
    pub(super) fn replace_found(&self, node_id: &str, entry: Option<FoundEntry>) {
        let mut inner = self.lock();
        match entry {
            Some(entry) => {
                inner.found.insert(node_id.to_string(), entry);
            }
            None => {
                inner.found.remove(node_id);
                inner.probed.remove(node_id);
            }
        }
    }

    /// Let the next discovery pass look at one node again (or at every node, with `None`).
    pub(super) fn forget_probes(&self, node_id: Option<&str>) {
        let mut inner = self.lock();
        match node_id {
            Some(node_id) => {
                inner.probed.remove(node_id);
            }
            None => inner.probed.clear(),
        }
    }

    /// Whether a nudge from `key` is news: one per `every` for each, so a member's session checks
    /// do not hide the nudge of a new machine.
    pub(super) fn nudge_due(&self, key: &str, every: Duration) -> bool {
        let mut inner = self.lock();
        let now = Instant::now();
        if inner.nudges.get(key).is_some_and(|at| now.duration_since(*at) < every) {
            return false;
        }
        if inner.nudges.len() >= 256 {
            inner.nudges.retain(|_, at| now.duration_since(*at) < Duration::from_secs(60));
        }
        inner.nudges.insert(key.to_string(), now);
        true
    }

    /// A node nudged us and is about to be probed: if it does not answer yet (its listener may still be
    /// coming up), the probe is tried again a few times.
    pub(super) fn expect_node(&self, node_id: &str) {
        self.lock().awaited.insert(node_id.to_string(), (Instant::now(), 0));
    }

    /// The node answered, or was given up on.
    fn node_answered(&self, node_id: &str) {
        self.lock().awaited.remove(node_id);
    }

    /// The node a nudge announced did not answer: the wait before the next probe, `None` after a few tries.
    fn retry_awaited(&self, node_id: &str, step: Duration) -> Option<Duration> {
        let mut inner = self.lock();
        let (since, tries) = *inner.awaited.get(node_id)?;
        if since.elapsed() > Duration::from_secs(30) || tries >= 3 {
            inner.awaited.remove(node_id);
            return None;
        }
        inner.awaited.insert(node_id.to_string(), (since, tries + 1));
        Some(step.saturating_mul(tries + 1))
    }

    /// A member is back: its failed attempts no longer count and the next pass dials it.
    pub(super) fn reset_pair(&self, machine_id: &str) {
        let mut inner = self.lock();
        inner.next_attempt.remove(machine_id);
        if let Some(status) = inner.status.get_mut(machine_id) {
            status.failures = 0;
        }
    }

    /// Whether this machine should nudge a member that dials it: not while it is linked, and once per `every`.
    fn nudge_out_due(&self, machine_id: &str, every: Duration) -> bool {
        let mut inner = self.lock();
        if inner.links.get(machine_id).is_some_and(|link| !link.is_closed()) {
            return false;
        }
        let now = Instant::now();
        if inner.nudged_out.get(machine_id).is_some_and(|at| now.duration_since(*at) < every) {
            return false;
        }
        inner.nudged_out.insert(machine_id.to_string(), now);
        true
    }

    /// Record the capabilities this machine now allows `machine_id` beyond its session's grant. `true`
    /// when that is news: the session is then reopened, so the grant carries them.
    fn widening_is_new(&self, machine_id: &str, widened: &[Scope]) -> bool {
        let mut inner = self.lock();
        if widened.is_empty() {
            inner.widened.remove(machine_id);
            return false;
        }
        let seen = inner.widened.entry(machine_id.to_string()).or_default();
        if widened.iter().all(|scope| seen.contains(scope)) {
            return false;
        }
        *seen = widened.to_vec();
        true
    }

    fn reset_backoff(&self) {
        let mut inner = self.lock();
        inner.next_attempt.clear();
        for status in inner.status.values_mut() {
            status.failures = 0;
        }
    }

    /// This machine woke from sleep: every wait its failed attempts earned while it slept is over,
    /// the members that dial it are told it is back at once, and the tailnet is looked at afresh.
    pub(super) fn after_resume(&self) {
        self.reset_backoff();
        let mut inner = self.lock();
        inner.nudged_out.clear();
        inner.probed.clear();
        inner.awaited.clear();
        // A dial that began before the sleep runs on a network that is gone; a timer does not run while the machine
        // sleeps, so it would otherwise hold the pass for its whole deadline after the wake.
        std::mem::take(&mut inner.attempts).cancel();
    }

    /// Whether `signature` (of a fresh Tailscale status) differs from the one the last pass saw.
    pub(super) fn signature_changed(&self, signature: &str) -> bool {
        let inner = self.lock();
        !inner.signature.is_empty() && inner.signature != signature
    }

    /// Whether some member other than `me` that is not paused has no open link.
    pub(super) fn any_unlinked(&self, members: &[String]) -> bool {
        let inner = self.lock();
        members.iter().any(|id| inner.links.get(id).is_none_or(|link| link.is_closed()))
    }
}

/// `backoff_min * 2^(failures - 1)`, capped: 2, 4, 8 … 300 s with the defaults.
pub fn backoff(timing: &super::Timing, failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(16);
    timing.backoff_min.saturating_mul(1u32 << exponent).min(timing.backoff_max)
}

// ---- operations arriving from a peer ----

impl Network {
    /// Take operations a peer sent, close its session if they were forged, drop removed machines,
    /// and re-gossip what was new (§2.3, §2.5).
    pub fn apply_remote(&self, from: Option<&str>, ops: Vec<OpEnvelope>) -> Result<ApplyOutcome, MembershipError> {
        let membership = self.membership().ok_or(MembershipError::NoNetwork)?;
        let outcome = membership.apply_ops(ops)?;
        self.after_apply(from, &outcome);
        Ok(outcome)
    }

    /// The consequences of an applied batch. N4 calls it for operations it applied itself, with
    /// `from: None`.
    /// End the streams this machine serves for members that lost the capability: paused, removed, or a register
    /// that turned relaying or ports off. Opening is refused at use time; this ends what was already open.
    pub(crate) fn close_unpermitted_streams(&self) {
        self.ports.direct.close_unpermitted(self);
        self.ports.hops.close_unpermitted(self);
        for link in self.peers.links() {
            let ended = link.relays.close_unpermitted(&effective_scopes(self, &link));
            if ended > 0 {
                tracing::info!(target: SESSION_TARGET, machine = %link.machine_id, ended, "ended streams a member may no longer use");
            }
        }
    }

    pub fn after_apply(&self, from: Option<&str>, outcome: &ApplyOutcome) {
        if outcome.tampered() {
            tracing::warn!(target: TARGET, peer = ?from, "a peer sent a membership operation that does not verify; closing its session");
            if let Some(from) = from {
                self.peers.close(from);
            }
        }
        for machine in &outcome.removed {
            self.drop_machine(machine);
        }
        if outcome.self_removed {
            tracing::warn!(target: TARGET, "this machine was removed from its network");
            self.forget_network_state();
        }
        if outcome.changed() {
            self.revoke_revoked_devices();
            self.peers.gossip(outcome.accepted.clone(), from);
            self.close_unpermitted_streams();
            self.notify_changed();
            self.wake();
        }
    }

    /// Close every session with a machine and revoke every grant it holds (§2.5).
    pub fn drop_machine(&self, machine_id: &str) {
        if let Some(link) = self.peers.take(machine_id) {
            self.say_farewell(link);
        }
        let revoked = self.grants.revoke_machine(machine_id);
        if !revoked.is_empty() {
            tracing::info!(target: SESSION_TARGET, machine = machine_id, grants = revoked.len(), "revoked the grants of a removed machine");
        }
        // The client tokens it asked this machine to mint die with it (§2.5).
        self.revoke_tokens_issued_via(machine_id);
    }

    /// End the session with a machine that is no longer a member, telling it first. The removed machine learns of its
    /// removal only from a member: once its sessions close, every member refuses it a new one. `remove` pushes the
    /// operation, but a session that was opening at that moment closes here before the push can arrive, so the
    /// operations go once more on the link being closed, and the accepting side's membership check hands a removed
    /// machine's socket here instead of closing it. Its streams end now and its requests are refused (a removed machine
    /// has no scopes); it closes once the machine answers, or after [`FAREWELL_TIMEOUT`]. Saying it again on the same
    /// link does nothing. Only the operations that removed it go, with their signers' chain
    /// ([`super::Membership::removal_proof`]); a machine that was never a member is told nothing.
    pub fn say_farewell(&self, link: Arc<PeerLink>) {
        if link.farewell.swap(true, Ordering::SeqCst) {
            return;
        }
        let ops = self
            .membership()
            .filter(|membership| !membership.is_member(&link.machine_id))
            .map(|membership| membership.removal_proof(&link.machine_id))
            .unwrap_or_default();
        let runtime = tokio::runtime::Handle::try_current();
        let (Ok(runtime), false, false) = (runtime, ops.is_empty(), link.is_closed()) else {
            link.close();
            return;
        };
        link.relays.close_all();
        runtime.spawn(async move {
            let told = tokio::time::timeout(FAREWELL_TIMEOUT, link.call::<PeerOpsPush>(PeerOpsPushParams { ops })).await;
            if !matches!(told, Ok(Ok(_))) {
                // It closes its own sessions on applying the removal, often before it answers.
                tracing::debug!(target: SESSION_TARGET, machine = %link.machine_id, "no answer to the removal on the closing session");
            }
            link.close();
        });
    }

    /// The presence this machine pushes (§4.1): version and features now; harnesses, load and
    /// battery come with the packages that own them.
    pub fn presence(&self) -> PeerPresence {
        PeerPresence {
            version: env!("CARGO_PKG_VERSION").to_string(),
            network_protocol: NETWORK_PROTOCOL,
            features: self.features(),
            harnesses: Vec::new(),
            load: None,
            battery: None,
            at: Utc::now(),
        }
    }
}

// ---- requests arriving on a link ----

fn parse<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(if params.is_null() { serde_json::json!({}) } else { params }).map_err(RpcError::invalid_params)
}

fn ok<T: serde::Serialize>(value: T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(RpcError::internal)
}

/// Answer one `peer.*` request. The caller has already checked the method's scope.
pub async fn dispatch(state: &AppState, link: &Arc<PeerLink>, method: &str, params: Value) -> Result<Value, RpcError> {
    let network = &state.network;
    match method {
        PeerSpacesSync::NAME => ok(super::spaces::serve(state, link, parse(params)?).await?),
        PeerSpaceLogo::NAME => ok(super::spaces::serve_logo(state, parse(params)?)?),
        PeerOpsSync::NAME => {
            let theirs: PeerOpsSyncParams = parse(params)?;
            let membership = network.membership().ok_or_else(|| RpcError::new(codes::NOT_AVAILABLE, "no network"))?;
            ok(membership.sync_respond(&theirs))
        }
        PeerOpsPush::NAME => {
            let push: PeerOpsPushParams = parse(params)?;
            let outcome = network
                .apply_remote(Some(&link.machine_id), push.ops)
                .map_err(|error| RpcError::new(codes::CONFLICT, error.to_string()))?;
            ok(PeerOpsPushResult { accepted: outcome.accepted.len() as u32, rejected: outcome.rejected.len() as u32 })
        }
        PeerPresenceMethod::NAME => {
            let presence: PeerPresence = parse(params)?;
            link.set_peer_features(&presence.features);
            network.peers.set_presence(&link.machine_id, presence);
            network.notify_changed();
            ok(Empty {})
        }
        PeerRelayOpen::NAME => super::relay::handle_open(state, link, parse(params)?).await,
        PeerRelayData::NAME => super::relay::handle_data(link, parse(params)?, StreamKind::Relay).await,
        PeerRelayClose::NAME => super::relay::handle_close(link, parse(params)?, StreamKind::Relay),
        // Remote delegation (R1): the child, its state, a stop, and the commits a worktree starts from.
        PeerDelegate::NAME
        | PeerDelegationGet::NAME
        | PeerDelegationCancel::NAME
        | PeerGitBundle::NAME
        | PeerGitImport::NAME
        | PeerThreadCreate::NAME
        | PeerThreadSend::NAME
        | PeerFork::NAME
        | PeerMovePrepare::NAME
        | PeerMoveCommit::NAME
        | PeerMoveGet::NAME
        | PeerMoveAbort::NAME => state.orchestrator.peer_dispatch(&link.machine_id, method, params).await,
        // Adding a project to this machine from another one.
        PeerProvisionProbeMethod::NAME
        | PeerProvisionPrepare::NAME
        | PeerProvisionChunk::NAME
        | PeerProvisionImport::NAME
        | PeerProvisionFinish::NAME => super::provision::serve(state, link, method, params).await,
        // Forwarded agent tools (R2): the tool runs as an inbound thread; its scopes beyond `machine:tools` are checked there.
        PeerToolCall::NAME => {
            let granted = effective_scopes(network, link);
            let result = state.orchestrator.serve_tool_call(&link.machine_id, &granted, parse(params)?).await?;
            ok(result)
        }
        // Files pushed to this machine (U1) and ports forwarded from it (U3).
        PeerFilesBegin::NAME | PeerFilesChunk::NAME | PeerFilesFinish::NAME | PeerFilesAbort::NAME => {
            super::files_push::serve(state, link, method, params).await
        }
        // User skills: this machine's inventory, and a copy of one of them to other machines.
        PeerPluginsInventory::NAME => ok(crate::plugins::inventory(state).await?),
        PeerPluginsCopy::NAME => ok(super::plugin_sync::serve_copy(state, parse(params)?).await?),
        PeerPluginsInstall::NAME => ok(crate::plugins::install(state, &link.machine_id, parse(params)?).await?),
        PeerSkillsInventory::NAME => ok(super::skill_sync::inventory(state).await?),
        PeerSkillsCopy::NAME => ok(super::skill_sync::serve_copy(state, &link.machine_id, parse(params)?).await?),
        // The clipboard (U2): an explicit text, a read, and a change of the universal clipboard.
        PeerClipboardSet::NAME => ok(super::clipboard::peer_set(state, parse(params)?).await?),
        PeerClipboardGet::NAME => ok(super::clipboard::peer_get(state).await?),
        PeerClipboardAuto::NAME => {
            super::clipboard::peer_auto(state, link, parse(params)?).await?;
            ok(Empty {})
        }
        PeerPortsPrepare::NAME => ok(super::ports::direct::prepare(state, link, parse(params)?).await?),
        PeerPortsVia::NAME => ok(super::ports::hop::handle_via(state, link, parse(params)?)?),
        PeerPortsHop::NAME => ok(super::ports::hop::handle_hop(state, link, parse(params)?).await?),
        PeerPortsHopCheck::NAME => ok(super::ports::hop::handle_hop_check(state, link, parse(params)?)?),
        PeerPortsRelease::NAME => {
            let request: PeerPortsReleaseParams = parse(params)?;
            network.ports.direct.release(&link.machine_id, request.forward_id);
            network.ports.hops.release(&link.machine_id, request.forward_id);
            ok(Empty {})
        }
        PeerPortsOpen::NAME => super::ports::handle_open(state, link, parse(params)?).await,
        PeerPortsData::NAME => super::relay::handle_data(link, parse(params)?, StreamKind::Ports).await,
        PeerPortsClose::NAME => super::relay::handle_close(link, parse(params)?, StreamKind::Ports),
        PeerThreadArchiveInbound::NAME => {
            let archive: PeerThreadArchiveInboundParams = parse(params)?;
            state.orchestrator.archive_inbound_threads(&link.machine_id, archive.thread_id).await?;
            ok(Empty {})
        }
        // Shared settings (S1): the shared fields only; the target refuses any path that is not shared.
        PeerSettingsGet::NAME => ok(crate::settings_share::peer_get(state)),
        PeerSettingsPatch::NAME => ok(crate::settings_share::apply_patch(state, parse(params)?)),
        // Federated notes and tasks (S2): read-only, so `machine:tools` is all it needs.
        PeerFederatedRead::NAME => ok(super::federated::serve_read(state, parse(params)?).await?),
        // The login broker (S4) and keys pushed by another member. `machine:logins` is checked by the caller; the broker
        // checks for itself that this machine is the one the register names.
        PeerLoginBorrow::NAME
        | PeerLoginPush::NAME
        | PeerLoginRelease::NAME
        | PeerLoginStatusMethod::NAME
        | PeerLoginReleaseAll::NAME
        | PeerLoginUsage::NAME => serve_login(state, link, method, params).await,
        PeerSecretsPut::NAME => {
            super::broker::serve_secrets_put(state, &link.machine_id, parse(params)?)?;
            ok(Empty {})
        }
        PeerUpdateStart::NAME => {
            let request: PeerUpdateStartParams = parse(params)?;
            tracing::info!(target: SESSION_TARGET, machine = %link.machine_id, "a member asked this machine to update");
            ok(super::updates::start_self(state, request.version.as_deref()).await)
        }
        PeerClientTokenMint::NAME => ok(super::client_grant::mint_for_peer(state, link, parse(params)?)?),
        PeerDeviceRevoke::NAME => {
            let revoke: PeerDeviceRevokeParams = parse(params)?;
            let count = super::client_grant::revoke_device_local(state, &revoke.device_id);
            tracing::info!(target: SESSION_TARGET, machine = %link.machine_id, count, "revoked a device on a member's request");
            ok(Empty {})
        }
        _ => Err(RpcError::method_not_found(method)),
    }
}

/// The `peer.login.*` methods. Only the machine the register names serves a borrow, a usage read or a
/// release-all; only the broker may push a rotated credential, so a push from any other member is refused.
async fn serve_login(state: &AppState, link: &Arc<PeerLink>, method: &str, params: Value) -> Result<Value, RpcError> {
    let broker =
        state.orchestrator.login_broker().cloned().ok_or_else(|| RpcError::new(codes::NOT_AVAILABLE, "no login broker on this machine"))?;
    match method {
        PeerLoginBorrow::NAME => ok(broker.serve_borrow(state, &link.machine_id, parse(params)?).await?),
        PeerLoginRelease::NAME => {
            broker.serve_release(&link.machine_id, parse(params)?)?;
            ok(Empty {})
        }
        PeerLoginStatusMethod::NAME => ok(broker.serve_status(state).await),
        PeerLoginUsage::NAME => ok(broker.serve_usage(state, parse(params)?).await?),
        PeerLoginReleaseAll::NAME => {
            // Honoured only once this machine is no longer the broker (checked there), so a member cannot sign out a working one.
            ok(broker.serve_release_all(state, parse(params)?).await)
        }
        _ => {
            let (kind, instance) = broker.apply_push(link, parse(params)?)?;
            state.orchestrator.login_rotated(kind, &instance).await;
            ok(Empty {})
        }
    }
}

/// What a peer may use right now: for a dialed link, this machine's policy for it; for an accepted
/// one, the grant's scopes narrowed by the same policy, so a register turned off after the grant
/// was issued takes effect at once. A removed or paused member keeps `machine:peer` only if paused.
pub fn effective_scopes(network: &Network, link: &PeerLink) -> Vec<Scope> {
    let policy = network.policy_scopes(&link.machine_id);
    match link.direction {
        Direction::Dialed => policy,
        // The universal clipboard is governed by this machine's own opt-in alone, so one made after the session opened
        // applies at once, as every other register does.
        Direction::Accepted => {
            policy.iter().copied().filter(|scope| *scope == Scope::MachineClipboardAuto || link.scopes.contains(scope)).collect()
        }
    }
}

/// How long a request may wait for one of a session's [`MAX_INBOUND_REQUESTS`] places: no longer than its caller waits
/// for the answer. A stream open is given up after 15 s ([`super::relay`], [`super::ports`]); any other call after 60 s.
fn queue_deadline(method: &str) -> Duration {
    if [PeerRelayOpen::NAME, PeerPortsOpen::NAME].contains(&method) { Duration::from_secs(15) } else { Duration::from_secs(60) }
}

/// Handle one text frame from a peer: an answer to our request, a request, or a notification.
pub async fn inbound(state: &AppState, link: &Arc<PeerLink>, text: &str) {
    link.touch();
    let Ok(value) = serde_json::from_str::<Value>(text) else { return };
    if value.get("method").is_none() {
        if let Ok(response) = serde_json::from_value::<RpcResponse>(value) {
            link.deliver(response);
        }
        return;
    }
    let id = value.get("id").and_then(|id| serde_json::from_value::<RpcId>(id.clone()).ok());
    let method = value["method"].as_str().unwrap_or_default().to_string();
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    // Relay data is hot and ordered: it is handled inline. Other requests run on their own task,
    // bounded per link.
    let ordered = [PeerRelayData::NAME, PeerRelayClose::NAME, PeerPortsData::NAME, PeerPortsClose::NAME].contains(&method.as_str());
    let state = state.clone();
    let link = link.clone();
    let busy_link = link.clone();
    let busy_id = id.clone();
    let method_name = method.clone();
    let inbound = link.inbound.clone();
    let work = async move {
        let result = match scope_for(&method) {
            Some(Some(scope)) if method.starts_with("peer.") => {
                if effective_scopes(&state.network, &link).contains(&scope) {
                    dispatch(&state, &link, &method, params).await
                } else {
                    Err(RpcError::forbidden(scope.as_str()))
                }
            }
            _ => Err(RpcError::method_not_found(&method)),
        };
        if let Some(id) = id {
            link.respond(match result {
                Ok(value) => RpcResponse::ok(id, value),
                Err(error) => RpcResponse::err(id, error),
            })
            .await;
        }
    };
    if ordered {
        work.await;
    } else if let Ok(permit) = inbound.clone().try_acquire_owned() {
        tokio::spawn(async move {
            let _permit = permit;
            work.await;
        });
    } else if busy_link.queued.fetch_add(1, Ordering::SeqCst) < MAX_QUEUED_REQUESTS {
        // A burst (a page opening many relayed requests at once) waits its turn instead of failing, but only while its
        // caller still waits for the answer: a stream opened after that would have no reader.
        let queued = busy_link.queued.clone();
        let deadline = tokio::time::Instant::now() + queue_deadline(&method_name);
        tokio::spawn(async move {
            let permit = tokio::time::timeout_at(deadline, inbound.acquire_owned()).await;
            queued.fetch_sub(1, Ordering::SeqCst);
            match permit {
                Ok(Ok(_permit)) => work.await,
                Ok(Err(_)) => {}
                Err(_) => {
                    tracing::debug!(target: SESSION_TARGET, machine = %busy_link.machine_id, method = %method_name, "dropped a request that waited longer than its caller");
                    if let Some(id) = busy_id {
                        busy_link.respond(RpcResponse::err(id, RpcError::internal("too many concurrent requests on this session"))).await;
                    }
                }
            }
        });
    } else {
        busy_link.queued.fetch_sub(1, Ordering::SeqCst);
        if let Some(id) = busy_id {
            busy_link.respond(RpcResponse::err(id, RpcError::internal("too many concurrent requests on this session"))).await;
        }
    }
}

/// Handle one binary frame from a peer: relay or port data (`relay.frames.v2`), under the same capability as its JSON
/// notification.
pub fn inbound_binary(state: &AppState, link: &Arc<PeerLink>, frame: &[u8]) {
    link.touch();
    let Some((kind, stream_id, text, bytes)) = super::relay::decode_frame(frame) else { return };
    let method = match kind {
        StreamKind::Relay => PeerRelayData::NAME,
        StreamKind::Ports => PeerPortsData::NAME,
    };
    let Some(Some(scope)) = scope_for(method) else { return };
    if effective_scopes(&state.network, link).contains(&scope) {
        super::relay::handle_binary(link, kind, stream_id, text, bytes);
    }
}

/// After a link opens: compare membership and exchange what each side lacks, then say who we are.
async fn after_open(state: &AppState, link: &Arc<PeerLink>, dialer: bool) {
    let network = &state.network;
    let Some(membership) = network.membership() else { return };
    if dialer {
        match link.call::<PeerOpsSync>(membership.sync_request()).await {
            Ok(theirs) => {
                if !theirs.ops.is_empty()
                    && let Err(error) = network.apply_remote(Some(&link.machine_id), theirs.ops)
                {
                    tracing::warn!(target: SESSION_TARGET, machine = %link.machine_id, %error, "could not apply the peer's operations");
                }
                let lacking = membership.ops_since(&theirs.ids);
                if !lacking.is_empty() {
                    let _ = link.call::<PeerOpsPush>(PeerOpsPushParams { ops: lacking }).await;
                }
            }
            Err(error) => tracing::warn!(target: SESSION_TARGET, machine = %link.machine_id, %error.message, "operation sync failed"),
        }
    }
    if dialer {
        super::spaces::sync(state, link, true).await;
    }
    let _ = link.notify(PeerPresenceMethod::NAME, serde_json::to_value(network.presence()).unwrap_or(Value::Null)).await;
}

/// A removal can land while a session is being opened, after `drop_machine` closed the links that
/// existed. Looked at once the link is registered, so either the removal finds the link or the
/// link finds the removal: a removed machine never keeps a session. The link was just registered, so
/// `drop_machine` ends it (or the link that replaced it already did) after telling the machine.
fn removed_while_opening(state: &AppState, link: &Arc<PeerLink>) -> bool {
    if !state.network.membership().is_some_and(|membership| !membership.is_member(&link.machine_id)) {
        return false;
    }
    state.network.drop_machine(&link.machine_id);
    true
}

/// A session the other machine opened to us: register it and send our side of the opening.
/// Called by the WebSocket layer once the grant has authenticated the upgrade.
pub fn accept(state: &AppState, link: Arc<PeerLink>) {
    state.network.peers.register(link.clone());
    if removed_while_opening(state, &link) {
        return;
    }
    state.network.notify_changed();
    tracing::info!(target: SESSION_TARGET, machine = %link.machine_id, "accepted a session");
    let state = state.clone();
    tokio::spawn(async move { after_open(&state, &link, false).await });
}

/// An accepted link ended.
pub fn accepted_ended(state: &AppState, link: &Arc<PeerLink>) {
    link.close();
    state.network.peers.unregister(link, None);
    state.network.notify_changed();
    // A session that dropped is looked at once (§3.1).
    state.network.wake();
}

// ---- dialing ----

fn candidate_ports(descriptor_port: Option<u16>) -> Vec<u16> {
    match descriptor_port {
        Some(port) => vec![port],
        None => PORT_RANGE.collect(),
    }
}

pub(super) async fn tcp_connect(network: &Network, addr: SocketAddr) -> std::io::Result<tokio::net::TcpStream> {
    let socket = if addr.is_ipv4() { tokio::net::TcpSocket::new_v4()? } else { tokio::net::TcpSocket::new_v6()? };
    if let Some(source) = network.tailscale.dial_source()
        && source.is_ipv4() == addr.is_ipv4()
    {
        socket.bind(SocketAddr::new(source, 0))?;
    }
    let stream = tokio::time::timeout(Duration::from_secs(10), socket.connect(addr)).await.map_err(|_| std::io::ErrorKind::TimedOut)??;
    // Session frames are small and latency-bound (requests, credit grants): Nagle would hold them for an ACK.
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

/// How long a session socket's WebSocket upgrade may wait for its answer. A path that dies after the request left (the
/// machine slept, the NAT mapping went away) never answers, and the socket has no keepalive to notice.
pub(super) fn upgrade_timeout(timing: &super::Timing) -> Duration {
    timing.dial_deadline.min(Duration::from_secs(15))
}

/// Open the WebSocket of an established session.
async fn connect_socket(
    network: &Network,
    target: &Target,
    token: &str,
) -> Result<tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>, String> {
    let ip = network.tailscale.dial_target(target.ts_ip);
    let addr = SocketAddr::new(ip, target.port);
    let stream = tcp_connect(network, addr).await.map_err(|error| format!("could not connect the session socket: {error}"))?;
    let mut request = target.ws_url(network).into_client_request().map_err(|error| error.to_string())?;
    let header = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|error| error.to_string())?;
    request.headers_mut().insert(AUTHORIZATION, header);
    let wait = upgrade_timeout(&network.timing);
    let (socket, _) = tokio::time::timeout(wait, tokio_tungstenite::client_async(request, stream))
        .await
        .map_err(|_| format!("the session socket did not answer within {} s", wait.as_secs_f32()))?
        .map_err(|error| format!("the session socket was refused: {error}"))?;
    Ok(socket)
}

/// How one dial of the pass ended.
enum Attempt {
    Done(Result<(), String>),
    /// Given up because the network changed while it ran: not a failure of the member, tried again at once.
    GivenUp,
}

/// Run one dial within the timing's deadline, giving it up when `attempts` is cancelled.
async fn bounded(attempts: &CancellationToken, deadline: Duration, dial: impl std::future::Future<Output = Result<(), String>>) -> Attempt {
    tokio::select! {
        result = tokio::time::timeout(deadline, dial) => Attempt::Done(
            result.unwrap_or_else(|_| Err(format!("no answer within {} s; the attempt was given up", deadline.as_secs_f32())))
        ),
        _ = attempts.cancelled() => Attempt::GivenUp,
    }
}

/// Dial a member: verify it, open a session, connect and run the link until it ends. Returns when
/// the link is registered (the run continues on its own tasks) or the attempt failed.
/// Dial a member that Tailscale lists offline. A node that lost contact with Tailscale's coordination
/// server is listed offline by its peers yet still passes traffic through DERP relays, so the one cheap
/// well-known probe decides whether a session is worth opening.
async fn dial_listed_offline(state: &AppState, member: &str, node: &TsNode, url: Option<String>) -> Result<(), String> {
    if probe_node(state, node.clone()).await.is_none() {
        // The address it published, if any, is the other way to reach it.
        if let Some(url) = url {
            return super::dial_out::dial(state, member, &url).await;
        }
        return Err("Tailscale lists it offline and nothing answered".into());
    }
    tracing::info!(target: SESSION_TARGET, machine = %member, "Tailscale lists this machine offline, but it answers; dialing it");
    dial(state, member, node).await
}

async fn dial(state: &AppState, member: &str, node: &TsNode) -> Result<(), String> {
    let network = &state.network;
    let membership = network.membership().ok_or("the network is unavailable")?;
    let projection = membership.projection();
    let entry = projection.entry(member).filter(|entry| entry.removed.is_none()).ok_or("not a member")?;
    let ip = node.ip().ok_or("the node has no tailnet address")?;
    let wanted: Vec<Scope> = Scope::MACHINE.to_vec();
    let mut last = String::from("no listener answered");
    for port in candidate_ports(entry.descriptor.port) {
        let target =
            Target { machine_id: member.to_string(), signing_key: entry.descriptor.signing_key.clone(), ts_ip: ip, port, url: None };
        match session::open(network, membership, &target, &wanted, None).await {
            Ok(established) => {
                let socket = connect_socket(network, &target, &established.token).await?;
                run_dialed(state.clone(), target, established, socket);
                return Ok(());
            }
            // Nothing listens on this port: try the next one the peer may have taken (§3.1).
            Err(session::SessionError::Identity(well_known::ProbeError::Unreachable(reason))) => last = reason,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err(last)
}

/// How often an online member's `last_seen` moves while its session is up.
const SEEN_REFRESH: chrono::Duration = chrono::Duration::seconds(60);

/// A member with a live session was seen now: its `last_seen` moves (at most once per [`SEEN_REFRESH`]) and is stored.
pub(super) fn touch_seen(state: &AppState, machine_id: &str) {
    if let Some(seen) = state.network.peers.refresh_seen(machine_id) {
        let _ = state.store.network_machine_touch(machine_id, seen);
    }
}

/// Ping every `ping_every`; a link that has heard nothing for `dead_after` (three missed pings) is
/// closed, which the reader turns into an offline peer and a reconnect with backoff. `alive` runs on
/// every beat of a live link (it keeps the member's `last_seen` current).
pub(super) fn spawn_heartbeat(link: Arc<PeerLink>, timing: super::Timing, ping: mpsc::Sender<()>, alive: impl Fn() + Send + 'static) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = link.closed() => break,
                _ = tokio::time::sleep(timing.ping_every) => {}
            }
            if link.idle_for() > timing.dead_after {
                tracing::warn!(target: SESSION_TARGET, machine = %link.machine_id, "the peer missed its heartbeats; closing the session");
                link.close();
                break;
            }
            alive();
            let _ = ping.try_send(());
        }
    });
}

pub(super) fn run_dialed<S>(
    state: AppState,
    target: Target,
    established: session::Established,
    socket: tokio_tungstenite::WebSocketStream<S>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Outgoing>(256);
    let lane_closed = CancellationToken::new();
    let (bulk, mut bulk_rx) = BulkLane::new(lane_closed.clone());
    let link = PeerLink::new(
        target.machine_id.clone(),
        Direction::Dialed,
        None,
        established.scopes.clone(),
        Arc::new(ChannelSink { control: out_tx, bulk }),
    );
    let timing = state.network.timing.clone();
    let (ping_tx, mut ping_rx) = mpsc::channel::<()>(1);

    // Writer: pings, control frames, then stream data.
    let writer_link = link.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = writer_link.closed() => break,
                ping = ping_rx.recv() => match ping {
                    Some(()) => if sink.send(Message::Ping(Vec::new().into())).await.is_err() { break },
                    None => break,
                },
                frame = next_frame(&mut out_rx, &mut bulk_rx) => {
                    let (message, _held) = match frame {
                        Some(Ok(message)) => (message, None),
                        Some(Err(data)) => {
                            let (message, held) = data.into_parts();
                            (message, Some(held))
                        }
                        None => break,
                    };
                    let message = match message {
                        Outgoing::Text(text) => Message::Text(text.into()),
                        Outgoing::Binary(bytes) => Message::Binary(bytes.into()),
                    };
                    if sink.send(message).await.is_err() { break }
                }
            }
        }
        lane_closed.cancel();
        let _ = sink.close().await;
        writer_link.close();
    });

    // Heartbeat: a ping every interval; three missed ones and the link is dead.
    let seen_state = state.clone();
    let seen_id = link.machine_id.clone();
    spawn_heartbeat(link.clone(), timing.clone(), ping_tx, move || touch_seen(&seen_state, &seen_id));

    // Renewal: a new signed request names the grant, so the socket keeps its authority (§3.3).
    let renew_state = state.clone();
    let renew_link = link.clone();
    let renew_target = target.clone();
    let grant_id = established.grant_id.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = renew_link.closed() => break,
                _ = tokio::time::sleep(renew_state.network.timing.renew_after) => {}
            }
            let Some(membership) = renew_state.network.membership() else { break };
            let wanted = Scope::MACHINE.to_vec();
            if let Err(error) = session::open(&renew_state.network, membership, &renew_target, &wanted, Some(&grant_id)).await {
                tracing::warn!(target: SESSION_TARGET, machine = %renew_link.machine_id, %error, "could not renew the session; reconnecting");
                renew_link.close();
                break;
            }
        }
    });

    // Reader: answers, requests and pongs. When it ends the link is over.
    let reader_state = state.clone();
    let reader_link = link.clone();
    tokio::spawn(async move {
        let mut error = None;
        loop {
            tokio::select! {
                _ = reader_link.closed() => break,
                frame = stream.next() => match frame {
                    Some(Ok(Message::Text(text))) => inbound(&reader_state, &reader_link, text.as_str()).await,
                    Some(Ok(Message::Binary(frame))) => inbound_binary(&reader_state, &reader_link, &frame),
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => reader_link.touch(),
                    Some(Err(failure)) => { error = Some(failure.to_string()); break }
                }
            }
        }
        reader_link.close();
        reader_state.network.peers.unregister(&reader_link, error);
        reader_state.network.notify_changed();
        // A session that dropped is looked at once.
        reader_state.network.wake();
    });

    state.network.peers.register(link.clone());
    if removed_while_opening(&state, &link) {
        return;
    }
    state.network.notify_changed();
    tracing::info!(target: SESSION_TARGET, machine = %link.machine_id, "opened a session");
    tokio::spawn(async move { after_open(&state, &link, true).await });
}

// ---- the worker ----

/// A fingerprint of who is online, to notice that the tailnet changed.
pub(super) fn signature(status: &TsStatus) -> String {
    let mut online: Vec<&str> = status.peers.iter().filter(|peer| peer.online).map(|peer| peer.node_id.as_str()).collect();
    online.sort_unstable();
    format!("{}|{}", status.running, online.join(","))
}

/// How soon a network listener that failed to open is tried again.
const LISTEN_RETRY: Duration = Duration::from_secs(10);

/// The longest one pass of the worker may take, dials, discovery and joining together. Each step is bounded on its own;
/// this is the last line, so nothing that hangs can stop the worker for good.
const PASS_LIMIT: Duration = Duration::from_secs(180);
/// The longest the worker waits on the shared spaces of the links after a pass.
const SPACES_LIMIT: Duration = Duration::from_secs(60);

/// The background worker: runs a pass, then waits for the next tick, a retry that is due, or a
/// wake-up. It ends with the daemon.
pub async fn run(state: AppState) {
    let network = state.network.clone();
    // Sleep and tailnet changes cut the backoff short; it ends with the daemon too.
    tokio::spawn(super::wake_watch::run(state.clone()));
    loop {
        network.peers.set_pass_started(Some(Instant::now()));
        let retry = match tokio::time::timeout(PASS_LIMIT, pass(&state)).await {
            Ok(retry) => retry,
            Err(_) => {
                tracing::warn!(target: SESSION_TARGET, limit_s = PASS_LIMIT.as_secs(), "a pass of the machine network ran past its limit; giving it up and starting over");
                network.peers.give_up_attempts();
                Some(network.timing.backoff_min)
            }
        };
        network.peers.set_pass_started(None);
        let spaces = async {
            super::spaces::reconcile(&state).await;
            for link in network.peers.links() {
                super::spaces::sync(&state, &link, false).await;
            }
        };
        if tokio::time::timeout(SPACES_LIMIT, spaces).await.is_err() {
            tracing::warn!(target: SESSION_TARGET, "syncing spaces with the other machines ran past its limit; going on");
        }
        let tick = if network.is_watching() { network.timing.tick_watching } else { network.timing.tick_idle };
        let mut wait = retry.map_or(tick, |retry| retry.min(tick)).max(Duration::from_millis(20));
        // A listener that could not open (a daemon started at boot before Tailscale was up) is tried
        // again soon, not on the idle tick: until it opens, no other machine can reach this one.
        if network.listen_info().error.is_some() {
            wait = wait.min(LISTEN_RETRY);
        }
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            _ = tokio::time::sleep(wait) => {}
            _ = network.woken() => {}
        }
    }
    state.network.peers.close_all();
}

/// One pass of the worker. Returns when the next scheduled retry is due.
pub async fn pass(state: &AppState) -> Option<Duration> {
    let network = &state.network;
    let enabled = state.settings.get().network.enabled;
    network.set_enabled(enabled);
    crate::exposure::apply_network(state, enabled).await;
    let membership = network.membership()?;
    if !enabled {
        network.peers.close_all();
        return None;
    }
    // A machine off the tailnet (or with Tailscale stopped) has no status, but it still dials the
    // members that publish a URL (§3.5).
    let raw_status = network.tailscale.status().await.ok();
    let status = raw_status.clone().filter(|status| status.running);
    // Tailscale that cannot reach its coordination server reports itself offline (`running` is false)
    // while the backend still passes traffic through DERP; its peer list is still good for dialing.
    let dial_status = raw_status.filter(|status| status.running || status.backend_state == "Running");
    if let Some(status) = &status {
        let sig = signature(status);
        let changed = {
            let mut inner = network.peers.lock();
            std::mem::replace(&mut inner.signature, sig.clone()) != sig
        };
        if changed {
            // A tailnet change is the moment an unreachable peer may have become reachable.
            network.peers.reset_backoff();
            network.notify_changed();
        }
    }

    let me = membership.machine_id();
    if membership.self_is_member() {
        reopen_widened_sessions(network);
        super::dial_out::publish_url(state).await;
        let projection = membership.projection();
        let own_url = super::dial_out::url_of(&projection, &me);
        let mut dials = Vec::new();
        let mut url_dials = Vec::new();
        for (id, entry) in projection.members() {
            if *id == me || network.peers.link(id).is_some() {
                continue;
            }
            // A paused member gets no session.
            if entry.paused {
                tracing::debug!(target: SESSION_TARGET, machine = %id, "not dialing: the member is paused");
                continue;
            }
            if !network.peers.due(id) {
                if let Some((left, failures)) = network.peers.waiting(id) {
                    tracing::debug!(target: SESSION_TARGET, machine = %id, failures, wait_s = left.as_secs_f32(), "not dialing yet: waiting after failed attempts");
                }
                continue;
            }
            let node = dial_status.as_ref().zip(entry.descriptor.ts_node_id.as_deref()).and_then(|(status, node_id)| status.peer(node_id));
            if let Some(node) = node.filter(|node| node.ip().is_some()) {
                // The member with the smaller id dials (§3.3). Tailscale may list a member offline that
                // still answers (it lost its coordination server and passes traffic through DERP), so an
                // offline member is dialed too, after a cheap probe and on a slower schedule.
                let url =
                    super::dial_out::url_of(&projection, id).filter(|_| !node.online && (own_url.is_none() || me.as_str() < id.as_str()));
                if me.as_str() < id.as_str() {
                    dials.push((id.clone(), node.clone(), url));
                } else if let Some(url) = url {
                    // A larger id that cannot see the other online dials the address it published.
                    url_dials.push((id.clone(), url));
                } else {
                    nudge_dialer(state, id, node);
                }
                continue;
            }
            // Not on the tailnet from here. A member that published a URL can be dialed there; of two
            // that both can be reached, the smaller id dials, and one that cannot be reached always does.
            if let Some(url) = super::dial_out::url_of(&projection, id).filter(|_| own_url.is_none() || me.as_str() < id.as_str()) {
                url_dials.push((id.clone(), url));
            } else if me.as_str() < id.as_str() {
                network.peers.fail_quiet(id, "not online on the tailnet");
            }
        }
        // Every dial is bounded by the deadline, and all of them are given up when the network changes meanwhile, so one
        // that never answers cannot hold the worker (and every other member with it).
        let attempts = network.peers.attempts();
        let deadline = network.timing.dial_deadline;
        let results = futures::stream::iter(dials)
            .map(|(id, node, url)| {
                let attempts = attempts.clone();
                async move {
                    let dialing = async {
                        if node.online { dial(state, &id, &node).await } else { dial_listed_offline(state, &id, &node, url).await }
                    };
                    let result = bounded(&attempts, deadline, dialing).await;
                    (id, !node.online, result)
                }
            })
            .buffer_unordered(MAX_CONCURRENT_PROBES)
            .collect::<Vec<_>>()
            .await;
        let url_results = futures::stream::iter(url_dials)
            .map(|(id, url)| {
                let attempts = attempts.clone();
                async move {
                    let result = bounded(&attempts, deadline, super::dial_out::dial(state, &id, &url)).await;
                    (id, false, result)
                }
            })
            .buffer_unordered(MAX_CONCURRENT_PROBES)
            .collect::<Vec<_>>()
            .await;
        for (id, listed_offline, attempt) in results.into_iter().chain(url_results) {
            let result = match attempt {
                Attempt::Done(result) => result,
                Attempt::GivenUp => {
                    tracing::info!(target: SESSION_TARGET, machine = %id, "gave up a dial: the network changed while it ran; dialing again now");
                    network.wake();
                    continue;
                }
            };
            match result {
                Ok(()) if listed_offline => {
                    tracing::info!(target: SESSION_TARGET, machine = %id, "Tailscale lists this machine offline, but it answered; opened a session");
                }
                Ok(()) => {}
                Err(error) => {
                    tracing::debug!(target: SESSION_TARGET, machine = %id, %error, listed_offline, "could not open a session");
                    if listed_offline {
                        network.peers.fail_slow(&id, &network.timing, error);
                    } else {
                        network.peers.fail(&id, &network.timing, error);
                    }
                }
            }
        }
    }

    if let Some(status) = &status {
        discover(state, status).await;
        super::join::auto_join(state, status).await;
    }
    network.peers.soonest_retry()
}

/// A session this machine accepted carries the capabilities its policy allowed when the grant was
/// issued. When the policy later widens (a capability turned on, a pause lifted), the session is
/// reopened: the dialer reconnects at once and its new grant carries them. Narrowing needs nothing,
/// since [`effective_scopes`] applies the policy at use time.
fn reopen_widened_sessions(network: &Network) {
    for link in network.peers.links() {
        if link.direction != Direction::Accepted {
            continue;
        }
        // The clipboard opt-in is checked against this machine's policy alone, never the grant.
        let widened: Vec<Scope> = network
            .policy_scopes(&link.machine_id)
            .into_iter()
            .filter(|scope| *scope != Scope::MachineClipboardAuto && !link.scopes.contains(scope))
            .collect();
        if !network.peers.widening_is_new(&link.machine_id, &widened) {
            continue;
        }
        tracing::info!(target: SESSION_TARGET, machine = %link.machine_id, "this machine's policy widened; reopening the session");
        network.drop_session(&link.machine_id);
    }
}

/// A member with a larger id dials this machine, not the other way round. On its way back (a restart)
/// this machine tells it so, instead of waiting for the wait its failed attempts earned: a well-known
/// check, which the member takes as news (`Network::nudge_discovery`).
fn nudge_dialer(state: &AppState, member: &str, node: &TsNode) {
    let network = &state.network;
    let every = if node.online { Duration::from_secs(10).min(network.timing.tick_watching) } else { network.timing.offline_retry };
    if !network.peers.nudge_out_due(member, every) {
        return;
    }
    let state = state.clone();
    let node = node.clone();
    tokio::spawn(async move {
        probe_node(&state, node).await;
    });
}

impl Peers {
    /// Note why a member cannot be dialed without counting it as a failed attempt: the status
    /// change that brings it back resets the schedule anyway.
    fn fail_quiet(&self, machine_id: &str, reason: &str) {
        let mut inner = self.lock();
        inner.status.entry(machine_id.to_string()).or_default().last_error = Some(reason.to_string());
    }
}

/// Probe online peers that are not members for the well-known endpoint (§3.1).
async fn discover(state: &AppState, status: &TsStatus) {
    let network = &state.network;
    let Some(membership) = network.membership() else { return };
    let projection = membership.projection();
    let ttl = network.timing.found_ttl;
    let candidates: Vec<TsNode> = {
        let mut inner = network.peers.lock();
        // Members are never "found"; nodes that left the tailnet are dropped. A node whose machine
        // was removed is found again (a rejoined machine has a new key); only a click adds it (§2.6).
        let known: Vec<String> = projection.members().filter_map(|(_, entry)| entry.descriptor.ts_node_id.clone()).collect();
        inner.found.retain(|node_id, _| !known.contains(node_id) && status.peer(node_id).is_some_and(|peer| peer.online));
        // What answered as a machine that was removed is stale: that node may have rejoined with a new
        // identity (§2.6), so it is looked at again now.
        let stale: Vec<String> = inner
            .found
            .iter()
            .filter(|(_, entry)| projection.is_removed(&entry.descriptor.machine_id))
            .map(|(node, _)| node.clone())
            .collect();
        for node in stale {
            inner.found.remove(&node);
            inner.probed.remove(&node);
        }
        // A node that answered is not probed again for `ttl`; one that did not (Kybern may simply not be
        // running there yet) is looked at again on the next tick.
        let unanswered_ttl = ttl.min(network.timing.tick_watching);
        let answered: Vec<String> = inner.found.keys().cloned().collect();
        inner.probed.retain(|node_id, at| at.elapsed() < if answered.contains(node_id) { ttl } else { unanswered_ttl });
        status
            .peers
            .iter()
            .filter(|peer| peer.online && peer.ip().is_some() && !peer.node_id.is_empty())
            .filter(|peer| !known.contains(&peer.node_id))
            .filter(|peer| !inner.probed.contains_key(&peer.node_id))
            .cloned()
            .collect()
    };
    if candidates.is_empty() {
        return;
    }
    {
        let mut inner = network.peers.lock();
        for peer in &candidates {
            inner.probed.insert(peer.node_id.clone(), Instant::now());
        }
    }
    let candidate_ids: Vec<String> = candidates.iter().map(|node| node.node_id.clone()).collect();
    let found = futures::stream::iter(candidates)
        .map(|node| async move { probe_node(state, node).await })
        .buffer_unordered(MAX_CONCURRENT_PROBES)
        .collect::<Vec<_>>()
        .await;
    // A node that nudged us and does not answer yet (its listener may still be coming up) is probed again
    // in a moment, not at the next tick.
    for node_id in candidate_ids {
        if found.iter().flatten().any(|entry| entry.node.node_id == node_id) {
            network.peers.node_answered(&node_id);
        } else if let Some(wait) = network.peers.retry_awaited(&node_id, network.timing.backoff_min) {
            let network = state.network.clone();
            tokio::spawn(async move {
                tokio::time::sleep(wait).await;
                network.peers.forget_probes(Some(&node_id));
                network.wake();
            });
        }
    }
    let found: Vec<FoundEntry> = found.into_iter().flatten().collect();
    tell_removed_machines(state, &found);
    let mut changed = false;
    {
        let mut inner = network.peers.lock();
        for entry in found {
            tracing::debug!(target: DISCOVERY_TARGET, machine = %entry.descriptor.machine_id, node = %entry.node.node_id, "found a machine on the tailnet");
            changed |= inner.found.insert(entry.node.node_id.clone(), entry).is_none();
        }
    }
    if changed {
        network.notify_changed();
    }
}

/// A machine that was removed while it was offline, with the larger id, never dials: it nudges the member that should
/// dial it, and members do not dial removed machines. When a removed machine nudged this one (so it does not know)
/// and discovery then finds it answering with the very key the set recorded for it (its well-known proof verified),
/// this member dials it once to say farewell (ADE-120): the removed machine still takes it for a member and grants
/// the session, and the link that opens is handed to [`Network::say_farewell`] by `removed_while_opening`, which
/// pushes the operations that removed it. Once per nudge, and at most once per
/// [`super::Timing::removal_notice_every`] for each machine; a removed machine that knows nudges no one.
fn tell_removed_machines(state: &AppState, found: &[FoundEntry]) {
    let network = &state.network;
    let Some(membership) = network.membership().filter(|membership| membership.self_is_member()) else { return };
    let projection = membership.projection();
    for entry in found {
        let answered = &entry.descriptor;
        let removed = projection
            .entry(&answered.machine_id)
            .is_some_and(|recorded| recorded.removed.is_some() && recorded.descriptor.signing_key == answered.signing_key);
        if !removed
            || network.peers.link(&answered.machine_id).is_some()
            || !network.take_farewell_wanted(&answered.machine_id)
            || !network.removal_notice_due(&answered.machine_id)
        {
            continue;
        }
        let state = state.clone();
        let entry = entry.clone();
        tokio::spawn(async move {
            let machine = entry.descriptor.machine_id.clone();
            match farewell_dial(&state, &entry).await {
                Ok(()) => tracing::info!(target: SESSION_TARGET, %machine, "told a removed machine that it was removed"),
                Err(error) => {
                    tracing::debug!(target: SESSION_TARGET, %machine, %error, "could not tell a removed machine that it was removed")
                }
            }
        });
    }
}

/// Open a session to a removed machine only to close it with the operations that removed it.
async fn farewell_dial(state: &AppState, found: &FoundEntry) -> Result<(), String> {
    let network = &state.network;
    let membership = network.membership().ok_or("the network is unavailable")?;
    let projection = membership.projection();
    let machine_id = found.descriptor.machine_id.as_str();
    let entry = projection.entry(machine_id).filter(|entry| entry.removed.is_some()).ok_or("not a removed machine")?;
    let ip = found.node.ip().ok_or("the node has no tailnet address")?;
    let target = Target {
        machine_id: machine_id.to_string(),
        signing_key: entry.descriptor.signing_key.clone(),
        ts_ip: ip,
        port: found.port,
        url: None,
    };
    let established = session::open(network, membership, &target, &[Scope::MachinePeer], None).await.map_err(|error| error.to_string())?;
    let socket = connect_socket(network, &target, &established.token).await?;
    // Registered, found removed, told and closed: `run_dialed` never treats it as a member's session.
    run_dialed(state.clone(), target, established, socket);
    Ok(())
}

pub(super) async fn probe_node(state: &AppState, node: TsNode) -> Option<FoundEntry> {
    let network = &state.network;
    let ip = node.ip()?;
    let dial_ip = network.tailscale.dial_target(ip);
    // A node that drops packets (a firewall, a phone asleep) costs one timeout, not one per port: the
    // ports are tried together and the first answer wins.
    let timeout = if dial_ip.is_loopback() { Duration::from_secs(3) } else { Duration::from_secs(6) };
    let mut probes = futures::stream::iter(PORT_RANGE)
        .map(|port| async move {
            well_known::probe(&network.http, &well_known::base_url(dial_ip, port), None, timeout, network.timing.clock_window)
                .await
                .map(|descriptor| (port, descriptor))
        })
        .buffer_unordered(PORT_RANGE.count());
    while let Some(result) = probes.next().await {
        // Nothing there, or not Kybern: wait for the others.
        if let Ok((port, descriptor)) = result {
            let whois = network.tailscale.whois(ip).await.ok();
            return Some(FoundEntry { node, whois, descriptor, port, probed_at: Instant::now() });
        }
    }
    None
}

/// The address a probe or session actually uses for a tailnet peer, for the doctor and the tests.
pub fn dial_address(network: &Network, ts_ip: IpAddr, port: u16) -> SocketAddr {
    SocketAddr::new(network.tailscale.dial_target(ts_ip), port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::Timing;

    #[test]
    fn a_member_listed_offline_waits_at_least_the_offline_retry_after_a_failed_attempt() {
        let timing = Timing::default();
        let peers = Peers::default();
        peers.fail_slow("m", &timing, "nothing answered".into());
        assert!(!peers.due("m"));
        let wait = peers.soonest_retry().unwrap();
        assert!(wait > Duration::from_secs(25) && wait <= timing.offline_retry, "{wait:?}");
        // The plain schedule would have retried after 2 s.
        peers.fail("n", &timing, "nothing answered".into());
        assert!(peers.soonest_retry().unwrap() <= Duration::from_secs(2));
    }

    #[test]
    fn a_dial_is_bounded_by_its_deadline_and_given_up_when_the_network_changes() {
        tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(async {
            let attempts = CancellationToken::new();
            let hung = std::future::pending::<Result<(), String>>();
            let started = Instant::now();
            let Attempt::Done(Err(error)) = bounded(&attempts, Duration::from_millis(100), hung).await else { panic!("a failure") };
            assert!(error.contains("no answer"), "{error}");
            assert!(started.elapsed() < Duration::from_secs(2));

            let peers = Peers::default();
            let attempts = peers.attempts();
            let waiting = tokio::spawn(async move { bounded(&attempts, Duration::from_secs(60), std::future::pending()).await });
            tokio::task::yield_now().await;
            peers.after_resume();
            let given_up = tokio::time::timeout(Duration::from_secs(2), waiting).await.expect("given up at once").unwrap();
            assert!(matches!(given_up, Attempt::GivenUp), "a wake is not a failure of the member");
            assert!(!peers.attempts().is_cancelled(), "the next pass dials with a fresh token");
        });
    }

    #[test]
    fn a_session_socket_that_never_answers_its_upgrade_is_given_up() {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
            let dir = crate::network::testutil::TempDir::new("hung-upgrade");
            let store = kybern_store::Store::open_in_memory().unwrap();
            let tailscale = Arc::new(crate::network::tailscale::Fake::new(dir.path().join("no-tailnet.json")));
            let network = Network::new(dir.path(), &store, "00000000-0000-7000-8000-000000000001", tailscale, Timing::fast());
            // Accepts the connection and reads the upgrade request, then never answers.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let holder = tokio::spawn(async move {
                let mut held = Vec::new();
                while let Ok((socket, _)) = listener.accept().await {
                    held.push(socket);
                }
            });
            let target =
                Target { machine_id: "m".into(), signing_key: String::new(), ts_ip: "127.0.0.1".parse().unwrap(), port, url: None };
            let started = Instant::now();
            let error = tokio::time::timeout(Duration::from_secs(10), connect_socket(&network, &target, "token"))
                .await
                .expect("the upgrade is bounded")
                .expect_err("nothing answered");
            assert!(error.contains("did not answer"), "{error}");
            assert!(started.elapsed() <= upgrade_timeout(&Timing::fast()) + Duration::from_secs(2));
            holder.abort();
        });
    }

    struct NoSink;

    #[async_trait::async_trait]
    impl FrameSink for NoSink {
        async fn send(&self, _frame: Value) -> bool {
            true
        }
    }

    // ADE-121: a member that stays connected is not "last seen" when its session opened.
    #[test]
    fn last_seen_moves_about_once_a_minute_while_a_link_is_up_and_never_without_one() {
        let peers = Peers::default();
        assert_eq!(peers.refresh_seen("m"), None, "no link, no sighting");
        let link = PeerLink::new("m".into(), Direction::Dialed, None, vec![], Arc::new(NoSink));
        peers.register(link.clone());
        let opened = peers.status("m").last_seen.unwrap();
        assert_eq!(peers.refresh_seen("m"), None, "seen moments ago");
        // The session has been up for a while.
        peers.lock().status.get_mut("m").unwrap().last_seen = Some(opened - chrono::Duration::minutes(5));
        let seen = peers.refresh_seen("m").expect("it moves");
        assert!(seen >= opened);
        assert_eq!(peers.status("m").last_seen, Some(seen));
        assert_eq!(peers.refresh_seen("m"), None, "not again within the minute");
        link.close();
        peers.lock().status.get_mut("m").unwrap().last_seen = Some(opened - chrono::Duration::minutes(5));
        assert_eq!(peers.refresh_seen("m"), None, "a closed link is no sighting");
    }

    #[test]
    fn backoff_doubles_from_two_seconds_to_five_minutes() {
        let timing = Timing::default();
        let waits: Vec<u64> = (1..=10).map(|failures| backoff(&timing, failures).as_secs()).collect();
        assert_eq!(waits, [2, 4, 8, 16, 32, 64, 128, 256, 300, 300]);
        assert_eq!(backoff(&timing, 0).as_secs(), 2);
        assert_eq!(backoff(&timing, u32::MAX).as_secs(), 300);
    }

    #[test]
    fn a_nudge_is_news_once_per_node_and_a_members_does_not_hide_a_new_machines() {
        let peers = Peers::default();
        let every = Duration::from_secs(10);
        assert!(peers.nudge_due("member:n1", every));
        assert!(!peers.nudge_due("member:n1", every), "the same node again within the wait");
        assert!(peers.nudge_due("n2", every), "another node is news");
        assert!(peers.nudge_due("n3", Duration::ZERO) && peers.nudge_due("n3", Duration::ZERO));
    }

    #[test]
    fn a_nudging_node_that_does_not_answer_is_probed_again_a_few_times() {
        let peers = Peers::default();
        let step = Duration::from_secs(2);
        assert_eq!(peers.retry_awaited("n1", step), None, "a node that never nudged is left to the tick");
        peers.expect_node("n1");
        assert_eq!(peers.retry_awaited("n1", step), Some(step));
        assert_eq!(peers.retry_awaited("n1", step), Some(step * 2));
        assert_eq!(peers.retry_awaited("n1", step), Some(step * 3));
        assert_eq!(peers.retry_awaited("n1", step), None, "then it waits for the tick");
        peers.expect_node("n2");
        peers.node_answered("n2");
        assert_eq!(peers.retry_awaited("n2", step), None);
    }

    #[test]
    fn a_widening_renews_a_session_once_for_the_same_capabilities() {
        let peers = Peers::default();
        assert!(!peers.widening_is_new("m", &[]));
        assert!(peers.widening_is_new("m", &[Scope::MachineFiles]));
        assert!(!peers.widening_is_new("m", &[Scope::MachineFiles]), "a dialer that does not ask for it is not redialed in a loop");
        assert!(peers.widening_is_new("m", &[Scope::MachineFiles, Scope::MachinePorts]));
        assert!(!peers.widening_is_new("m", &[]), "the grant caught up");
        assert!(peers.widening_is_new("m", &[Scope::MachineFiles]), "and a later widening is news again");
    }

    #[test]
    fn a_member_that_is_back_loses_the_wait_its_failed_attempts_earned() {
        let peers = Peers::default();
        let timing = crate::network::Timing::default();
        peers.fail("m", &timing, "refused".into());
        peers.fail("m", &timing, "refused".into());
        assert!(!peers.due("m") && peers.status("m").failures == 2);
        peers.reset_pair("m");
        assert!(peers.due("m") && peers.status("m").failures == 0);
    }

    #[test]
    fn waking_up_ends_every_wait_and_lets_this_machine_nudge_its_dialers_again() {
        let peers = Peers::default();
        let timing = crate::network::Timing::default();
        for _ in 0..8 {
            peers.fail("m", &timing, "refused".into());
        }
        peers.fail_slow("n", &timing, "asleep".into());
        assert!(!peers.due("m") && !peers.due("n"), "long waits were earned while it slept");
        assert!(peers.soonest_retry().unwrap() > Duration::from_secs(25));
        assert!(peers.nudge_out_due("d", Duration::from_secs(30)));
        assert!(!peers.nudge_out_due("d", Duration::from_secs(30)));
        peers.after_resume();
        assert!(peers.due("m") && peers.due("n") && peers.status("m").failures == 0);
        assert!(peers.nudge_out_due("d", Duration::from_secs(30)), "the member that dials this one hears at once");
    }

    #[test]
    fn a_queued_request_waits_no_longer_than_its_caller() {
        assert_eq!(queue_deadline(PeerPortsOpen::NAME), Duration::from_secs(15));
        assert_eq!(queue_deadline(PeerRelayOpen::NAME), Duration::from_secs(15));
        assert_eq!(queue_deadline(PeerPresenceMethod::NAME), Duration::from_secs(60));
    }

    #[test]
    fn a_signature_change_is_news_only_after_a_pass_saw_one() {
        let peers = Peers::default();
        assert!(!peers.signature_changed("true|a,b"), "no pass yet: the pass itself looks");
        peers.lock().signature = "true|a".into();
        assert!(!peers.signature_changed("true|a"));
        assert!(peers.signature_changed("true|a,b"));
        assert!(peers.any_unlinked(&["m".into()]));
        assert!(!peers.any_unlinked(&[]));
    }

    #[test]
    fn a_peers_ports_are_the_descriptors_or_the_default_range() {
        assert_eq!(candidate_ports(Some(4180)), vec![4180]);
        assert_eq!(candidate_ports(None), vec![4173, 4174, 4175, 4176, 4177, 4178, 4179]);
    }
}
