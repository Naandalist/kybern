//! The relay (design §3.4): a client that cannot reach machine B directly reaches it through member
//! M, over the session M already holds with B.
//!
//! * `GET /relay/{machine_id}/ws` upgrades to a WebSocket and `ANY /relay/{machine_id}/http/{*path}`
//!   carries one HTTP request (previews, assets, uploads, the well-known check).
//! * Between two machines that list `relay.frames.v2`, data frames are binary WebSocket messages (see
//!   [`encode_frame`]) and a stream's window is [`RELAY_WINDOW_V2`]; credit grants and closes stay JSON.
//! * M authenticates the caller with a device token that has `orchestration:read`, sent in
//!   `X-Kybern-Relay-Auth: Bearer …` (the `Authorization` header belongs to B, see below). It then
//!   opens a **stream** on its session with B: `peer.relay.open`, then `peer.relay.data` frames in
//!   both directions and `peer.relay.close`. Frames are JSON notifications carrying base64 data; a
//!   per-stream credit window of 256 KiB keeps one transfer from starving the session.
//! * The caller's own `Authorization`, ticket and `Host` travel end to end. B opens a local
//!   connection to its own listener with them, so B authenticates the **client's device token** and
//!   sees the device as the principal, never M. M strips its own header.
//! * Limits: 32 streams per client, 256 per daemon, and a stream with no traffic for 30 s closes,
//!   except the main `/ws` stream, which follows the session's heartbeat.
//!
//! For an HTTP stream the first text frame from B is the response head, `{"status":200,"headers":[[k,v]]}`;
//! binary frames after it are the body, and an empty text frame from M ends the request body.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Path, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use futures::{SinkExt, StreamExt};
use kybern_protocol::methods::{Method, PeerRelayClose, PeerRelayData, PeerRelayOpen};
use kybern_protocol::*;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName as TName, HeaderValue as TValue};
use tokio_util::sync::CancellationToken;

use super::peers::{Direction, PeerLink};
use super::relay_pages::{self, MAX_STREAMS_PER_PAGE, PageGrant};
use crate::auth::Principal;
use crate::state::AppState;

const TARGET: &str = "kybern::network::relay";
/// The credit window of a stream in bytes (§3.4).
pub const WINDOW: i64 = 256 * 1024;
/// What a frame costs of the window at least. The window counts bytes but the receiving queue counts frames, so a burst of
/// one-byte frames (keystrokes, a chatty socket) would fill the queue long before the bytes ran out and end the stream as an
/// overrun. With this floor the window holds at most `WINDOW / MIN_FRAME_COST` (128) frames, half of the queue.
pub(super) const MIN_FRAME_COST: usize = 2048;
/// The part of the window a frame of `len` bytes takes, and the credit its receiver returns.
pub(super) const fn frame_cost(len: usize) -> usize {
    if len < MIN_FRAME_COST { MIN_FRAME_COST } else { len }
}
pub const MAX_STREAMS_PER_CLIENT: usize = 32;
/// Streams one member machine may hold open on this one (`inbound:<machine>`): it relays for several devices and
/// pages at once, each under its own limit there, so its total here is larger than any one of theirs.
pub const MAX_INBOUND_PER_MEMBER: usize = 128;
pub const MAX_STREAMS_PER_DAEMON: usize = 256;
/// Request bodies and response bodies are cut into pieces of this size.
const CHUNK: usize = 16 * 1024;
const OPEN_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a request waits for a place under the stream limits before it is refused with 429.
const QUEUE_WAIT: Duration = Duration::from_secs(10);
pub const RELAY_AUTH_HEADER: &str = "x-kybern-relay-auth";

// ---- streams ----

/// A frame for the local consumer of a stream.
#[derive(Debug)]
pub enum Chunk {
    Text(Vec<u8>),
    Binary(Vec<u8>),
    /// The other side closed the stream.
    End,
}

/// Bytes a sender may still put on a stream. A message larger than the window is allowed once the
/// balance is positive and leaves it in debt, so a WebSocket message is never split.
pub(super) struct Credit {
    balance: AtomicI64,
    changed: Notify,
}

impl Credit {
    fn new(window: i64) -> Self {
        Self { balance: AtomicI64::new(window), changed: Notify::new() }
    }

    pub(super) fn add(&self, bytes: i64) {
        self.balance.fetch_add(bytes, Ordering::SeqCst);
        self.changed.notify_waiters();
    }

    pub(super) async fn take(&self, bytes: usize, cancel: &CancellationToken) -> bool {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            // Register interest before checking, so a grant between check and wait is not lost.
            notified.as_mut().enable();
            if self.balance.load(Ordering::SeqCst) > 0 {
                self.balance.fetch_sub(bytes as i64, Ordering::SeqCst);
                return true;
            }
            tokio::select! {
                _ = cancel.cancelled() => return false,
                _ = notified => {}
            }
        }
    }
}

/// What a stream carries. Its frames travel under that kind's method names only, so a frame sent under the other
/// kind's names (`peer.ports.data` for a relay stream, which `machine:peer` alone allows) is not delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Relay,
    Ports,
}

/// One relayed stream, as the side that owns its local end sees it.
pub struct Stream {
    pub id: u64,
    kind: StreamKind,
    /// On the machine that serves the stream (it took `peer.relay.open` or `peer.ports.open`), the capability the
    /// other side needs to keep it: when the member is paused or the capability is turned off, the stream ends.
    serves: Option<Scope>,
    incoming: mpsc::Sender<Chunk>,
    pub(super) credit: Credit,
    pub(super) cancel: CancellationToken,
    activity: Mutex<Instant>,
}

impl Stream {
    pub(super) fn touch(&self) {
        *self.activity.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.activity.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).elapsed()
    }
}

/// The streams of one session. Ids are odd for streams the dialer opens and even for the other
/// direction, so the two roles of one session never collide.
pub struct LinkRelays {
    streams: Mutex<HashMap<u64, Arc<Stream>>>,
    next: AtomicU64,
    /// The window a new stream may send into: what the other machine's queue holds ([`WINDOW`], or
    /// [`RELAY_WINDOW_V2`] once it listed `relay.frames.v2`).
    window: AtomicI64,
}

/// Frames a stream's receiving queue holds: enough for the larger window at the smallest frame cost, twice over.
const QUEUE_FRAMES: usize = (RELAY_WINDOW_V2 as usize / MIN_FRAME_COST) * 2;

/// What all the large windows of one session's streams add up to before they shrink: a few streams each get the full
/// [`RELAY_WINDOW_V2`] (a fast route needs it), and many share this, so what a session may have in flight, and the other
/// machine may have to queue, grows slowly with the stream count instead of by 4 MiB a stream.
pub(super) const LINK_WINDOW_BUDGET: i64 = 16 * 1024 * 1024;

/// The window of a new stream when `open` streams already run on its session: `largest` (what the other machine reads)
/// shared out of [`LINK_WINDOW_BUDGET`], never below the original [`WINDOW`].
pub(super) fn stream_window(largest: i64, open: usize) -> i64 {
    if largest <= WINDOW {
        return largest;
    }
    (LINK_WINDOW_BUDGET / (open as i64 + 1)).clamp(WINDOW, largest)
}

impl LinkRelays {
    pub fn new(direction: Direction) -> Self {
        Self {
            streams: Mutex::new(HashMap::new()),
            next: AtomicU64::new(if direction == Direction::Dialed { 1 } else { 2 }),
            window: AtomicI64::new(WINDOW),
        }
    }

    /// The other machine takes the larger window (it listed `relay.frames.v2`).
    pub fn set_large_window(&self, large: bool) {
        self.window.store(if large { RELAY_WINDOW_V2 } else { WINDOW }, Ordering::Relaxed);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Arc<Stream>>> {
        self.streams.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A new stream with a fresh id, and the receiver for the frames the other side sends it. `serves` is the
    /// capability the other side needs for a stream this machine serves; `None` for one it opened.
    pub(super) fn create(&self, id: Option<u64>, kind: StreamKind, serves: Option<Scope>) -> Option<(Arc<Stream>, mpsc::Receiver<Chunk>)> {
        let id = id.unwrap_or_else(|| self.next.fetch_add(2, Ordering::Relaxed));
        let mut streams = self.lock();
        if streams.contains_key(&id) {
            return None;
        }
        let (tx, rx) = mpsc::channel(QUEUE_FRAMES);
        let stream = Arc::new(Stream {
            id,
            kind,
            serves,
            incoming: tx,
            credit: Credit::new(stream_window(self.window.load(Ordering::Relaxed), streams.len())),
            cancel: CancellationToken::new(),
            activity: Mutex::new(Instant::now()),
        });
        streams.insert(id, stream.clone());
        Some((stream, rx))
    }

    fn get(&self, id: u64, kind: StreamKind) -> Option<Arc<Stream>> {
        self.lock().get(&id).filter(|stream| stream.kind == kind).cloned()
    }

    pub(super) fn remove(&self, id: u64) {
        if let Some(stream) = self.lock().remove(&id) {
            stream.cancel.cancel();
        }
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// End the streams this machine serves whose capability the other side no longer has (paused, or turned off).
    /// Returns how many ended.
    pub fn close_unpermitted(&self, scopes: &[Scope]) -> usize {
        let ended: Vec<_> = {
            let mut streams = self.lock();
            let ids: Vec<u64> = streams
                .values()
                .filter(|stream| stream.serves.is_some_and(|scope| !scopes.contains(&scope)))
                .map(|stream| stream.id)
                .collect();
            ids.into_iter().filter_map(|id| streams.remove(&id)).collect()
        };
        for stream in &ended {
            stream.cancel.cancel();
            let _ = stream.incoming.try_send(Chunk::End);
        }
        ended.len()
    }

    /// The session ended: every stream on it ends.
    pub fn close_all(&self) {
        let streams: Vec<_> = self.lock().drain().map(|(_, stream)| stream).collect();
        for stream in streams {
            stream.cancel.cancel();
            let _ = stream.incoming.try_send(Chunk::End);
        }
    }
}

impl Default for LinkRelays {
    fn default() -> Self {
        Self::new(Direction::Dialed)
    }
}

/// Counts open streams against the limits.
#[derive(Default)]
pub struct RelayHub {
    total: Arc<AtomicUsize>,
    per_client: Arc<Mutex<HashMap<String, usize>>>,
    /// Woken whenever a place frees up, for [`RelayHub::acquire_wait`].
    freed: Arc<Notify>,
}

/// Holds one stream's place under the limits until dropped.
pub struct Slot {
    total: Arc<AtomicUsize>,
    per_client: Arc<Mutex<HashMap<String, usize>>>,
    freed: Arc<Notify>,
    client: String,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.total.fetch_sub(1, Ordering::SeqCst);
        {
            let mut clients = self.per_client.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(count) = clients.get_mut(&self.client) {
                *count -= 1;
                if *count == 0 {
                    clients.remove(&self.client);
                }
            }
        }
        self.freed.notify_waiters();
    }
}

impl RelayHub {
    /// A place for a stream of `client`, or `None` over a limit.
    pub fn acquire(&self, client: &str) -> Option<Slot> {
        self.acquire_within(client, MAX_STREAMS_PER_CLIENT)
    }

    /// A place for a stream of `client` under its own `limit` (a page ticket's, say), or `None` over it.
    pub fn acquire_within(&self, client: &str, limit: usize) -> Option<Slot> {
        let mut clients = self.per_client.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.total.load(Ordering::SeqCst) >= MAX_STREAMS_PER_DAEMON || clients.get(client).copied().unwrap_or(0) >= limit {
            return None;
        }
        self.total.fetch_add(1, Ordering::SeqCst);
        *clients.entry(client.to_string()).or_default() += 1;
        Some(Slot { total: self.total.clone(), per_client: self.per_client.clone(), freed: self.freed.clone(), client: client.to_string() })
    }

    /// [`RelayHub::acquire_within`], waiting up to `wait` for a place to free up instead of refusing at once: a browser
    /// that opens a burst of connections sees them queue, not fail. `None` after the wait or once `cancel` fires.
    pub async fn acquire_wait(&self, client: &str, limit: usize, wait: Duration, cancel: &CancellationToken) -> Option<Slot> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let freed = self.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            if let Some(slot) = self.acquire_within(client, limit) {
                return Some(slot);
            }
            tokio::select! {
                _ = freed => {}
                _ = cancel.cancelled() => return None,
                _ = tokio::time::sleep_until(deadline) => return self.acquire_within(client, limit),
            }
        }
    }

    pub fn open_streams(&self) -> usize {
        self.total.load(Ordering::SeqCst)
    }
}

// ---- data frames ----

/// Send `bytes` on a stream, waiting for credit. `false` when the stream or the session ended.
async fn send_data(link: &PeerLink, stream: &Stream, bytes: &[u8], text: bool) -> bool {
    send_frame(link, stream, bytes, text).await
}

/// A data frame of either kind of stream, after its credit: a binary message when the other machine reads them, else a
/// `peer.relay.data` / `peer.ports.data` notification with base64 data. Both go on the session's data lane, which waits
/// for room on a slow route instead of failing ([`super::peers::BulkLane`]); the wait ends with the stream.
pub(super) async fn send_frame(link: &PeerLink, stream: &Stream, bytes: &[u8], text: bool) -> bool {
    if !stream.credit.take(frame_cost(bytes.len()), &stream.cancel).await {
        return false;
    }
    stream.touch();
    let send = async {
        if link.binary_frames() {
            return link.send_binary(encode_frame(stream.kind, stream.id, text, bytes)).await;
        }
        let params = PeerRelayDataParams { stream_id: stream.id, data: B64.encode(bytes), credit: None, text };
        let method = match stream.kind {
            StreamKind::Relay => PeerRelayData::NAME,
            StreamKind::Ports => kybern_protocol::methods::PeerPortsData::NAME,
        };
        link.notify_bulk(method, serde_json::to_value(params).unwrap_or(Value::Null)).await
    };
    tokio::select! {
        sent = send => sent,
        _ = stream.cancel.cancelled() => false,
    }
}

const FRAME_RELAY: u8 = 0;
const FRAME_PORTS: u8 = 1;
const FRAME_TEXT: u8 = 1;
const FRAME_HEADER: usize = 10;

/// A binary data frame: the stream kind (0 relay, 1 ports), flags (bit 0: a text frame), the stream id (u64, big
/// endian), then the bytes.
pub(super) fn encode_frame(kind: StreamKind, stream_id: u64, text: bool, bytes: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(FRAME_HEADER + bytes.len());
    frame.push(match kind {
        StreamKind::Relay => FRAME_RELAY,
        StreamKind::Ports => FRAME_PORTS,
    });
    frame.push(if text { FRAME_TEXT } else { 0 });
    frame.extend_from_slice(&stream_id.to_be_bytes());
    frame.extend_from_slice(bytes);
    frame
}

/// The parts of a binary data frame, or `None` for one this machine cannot read.
pub(super) fn decode_frame(frame: &[u8]) -> Option<(StreamKind, u64, bool, &[u8])> {
    if frame.len() < FRAME_HEADER {
        return None;
    }
    let kind = match frame[0] {
        FRAME_RELAY => StreamKind::Relay,
        FRAME_PORTS => StreamKind::Ports,
        _ => return None,
    };
    let stream_id = u64::from_be_bytes(frame[2..FRAME_HEADER].try_into().ok()?);
    Some((kind, stream_id, frame[1] & FRAME_TEXT != 0, &frame[FRAME_HEADER..]))
}

/// Send `bytes` cut into pieces (HTTP bodies); WebSocket messages go whole through [`send_data`].
async fn send_chunks(link: &PeerLink, stream: &Stream, bytes: &[u8], text: bool) -> bool {
    for piece in bytes.chunks(CHUNK) {
        if !send_data(link, stream, piece, text).await {
            return false;
        }
    }
    true
}

/// Tell the other side it may send `bytes` more.
async fn grant(link: &PeerLink, stream: &Stream, bytes: usize) {
    let bytes = frame_cost(bytes);
    let params =
        PeerRelayDataParams { stream_id: stream.id, data: String::new(), credit: Some(bytes.min(u32::MAX as usize) as u32), text: false };
    let _ = link.notify(PeerRelayData::NAME, serde_json::to_value(params).unwrap_or(Value::Null)).await;
}

/// After the stream's data, on the same lane: a close must not overtake bytes still queued.
async fn send_close(link: &PeerLink, stream_id: u64, reason: Option<&str>) {
    let params = PeerRelayCloseParams { stream_id, reason: reason.map(str::to_string) };
    let _ = link.notify_bulk(PeerRelayClose::NAME, serde_json::to_value(params).unwrap_or(Value::Null)).await;
}

/// `peer.relay.data` and `peer.ports.data`: a frame for a stream of that kind, or credit, or both.
pub async fn handle_data(link: &Arc<PeerLink>, params: PeerRelayDataParams, kind: StreamKind) -> Result<Value, RpcError> {
    let Some(stream) = link.relays.get(params.stream_id, kind) else { return Ok(json!({})) };
    if let Some(credit) = params.credit {
        stream.credit.add(credit as i64);
    }
    if params.data.is_empty() && !params.text {
        return Ok(json!({}));
    }
    let bytes = B64.decode(params.data.as_bytes()).map_err(|_| RpcError::invalid_params("data is not base64"))?;
    deliver(link, &stream, bytes, params.text);
    Ok(json!({}))
}

/// A binary data frame for a stream of `kind` (the caller checked the kind's scope).
pub fn handle_binary(link: &Arc<PeerLink>, kind: StreamKind, stream_id: u64, text: bool, bytes: &[u8]) {
    if let Some(stream) = link.relays.get(stream_id, kind) {
        deliver(link, &stream, bytes.to_vec(), text);
    }
}

fn deliver(link: &Arc<PeerLink>, stream: &Arc<Stream>, bytes: Vec<u8>, text: bool) {
    stream.touch();
    let chunk = if text { Chunk::Text(bytes) } else { Chunk::Binary(bytes) };
    match stream.incoming.try_send(chunk) {
        Ok(()) => {}
        // The local consumer is gone (a GET has no request body to receive): nothing to deliver.
        Err(mpsc::error::TrySendError::Closed(_)) => {}
        // The sender honours the window, so a full queue is a peer that does not: end the stream.
        Err(mpsc::error::TrySendError::Full(_)) => {
            tracing::warn!(target: TARGET, stream = stream.id, "a relayed stream overran its window; closing it");
            link.relays.remove(stream.id);
        }
    }
}

/// `peer.relay.close` and `peer.ports.close`: the other side ended a stream of that kind.
pub fn handle_close(link: &Arc<PeerLink>, params: PeerRelayCloseParams, kind: StreamKind) -> Result<Value, RpcError> {
    if let Some(stream) = link.relays.get(params.stream_id, kind) {
        let _ = stream.incoming.try_send(Chunk::End);
        stream.cancel.cancel();
        link.relays.remove(params.stream_id);
    }
    Ok(json!({}))
}

// ---- header rules ----

/// Headers that describe one hop, not the request. They never cross a relay.
fn hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
            | "forwarded"
            | "x-forwarded-for"
            | "x-forwarded-host"
            | "x-forwarded-proto"
    ) || name.starts_with("sec-websocket-")
        || name == RELAY_AUTH_HEADER
}

/// The client's headers to carry: never the per-hop ones, and never the page headers only M and B set
/// (a client that sent them could otherwise choose the prefix B writes into a page's policy).
fn forwarded_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| !hop_by_hop(name.as_str()))
        .filter(|(name, _)| name.as_str() != relay_pages::PREFIX_HEADER && name.as_str() != relay_pages::LOCAL_MARK_HEADER)
        .filter_map(|(name, value)| Some((name.as_str().to_string(), value.to_str().ok()?.to_string())))
        .collect()
}

/// Paths a relay never reaches: anything but an origin-relative path, other relays (no onward hops) and the
/// session handshake. See [`local_url`].
fn path_refused(path: &str) -> bool {
    local_url("http", std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 1)), path).is_none()
}

/// The URL B opens on its own listener at `addr` for a relayed `path` (with its query), or `None` when the path
/// is refused. The path must be origin-relative (one leading `/`, no backslash, fragment or control character),
/// so it can never turn the daemon's address into user-info and name another host or port; the parsed URL is
/// checked to still point at `addr`. The deny list runs on the path the URL parser normalized (dot segments,
/// `%2e` included), percent-decoded once more, because that is what the listener routes on.
fn local_url(scheme: &str, addr: std::net::SocketAddr, path: &str) -> Option<reqwest::Url> {
    if !path.starts_with('/') || path.starts_with("//") || path.chars().any(|c| c == '\\' || c == '#' || c == ' ' || c.is_control()) {
        return None;
    }
    let url = reqwest::Url::parse(&format!("{scheme}://{addr}{path}")).ok()?;
    let host = match addr.ip() {
        std::net::IpAddr::V4(ip) => ip.to_string(),
        std::net::IpAddr::V6(ip) => format!("[{ip}]"),
    };
    if url.host_str() != Some(host.as_str())
        || url.port_or_known_default() != Some(addr.port())
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let decoded = percent_decoded(url.path());
    let mut segments = decoded.split('/').filter(|segment| !segment.is_empty());
    if decoded.split('/').any(|segment| segment == "." || segment == "..") {
        return None;
    }
    let first = segments.next().unwrap_or_default().to_ascii_lowercase();
    if first == "relay" || first == "network" {
        return None;
    }
    Some(url)
}

/// `text` with each `%XX` replaced by its byte; anything else is kept as it is.
pub(super) fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex = |byte: u8| (byte as char).to_digit(16);
        if bytes[at] == b'%'
            && let (Some(high), Some(low)) = (bytes.get(at + 1).and_then(|b| hex(*b)), bytes.get(at + 2).and_then(|b| hex(*b)))
        {
            out.push((high * 16 + low) as u8);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---- M: the relay endpoint ----

fn refuse(status: StatusCode, message: impl Into<String>) -> Response {
    (status, message.into()).into_response()
}

/// Authenticate the caller to this relay member. The refusal is the HTTP response to send.
#[allow(clippy::result_large_err)]
fn caller(state: &AppState, headers: &HeaderMap) -> Result<Principal, Response> {
    let raw = |value: &HeaderValue| {
        value.to_str().ok().and_then(|text| text.strip_prefix("Bearer ").or_else(|| text.strip_prefix("bearer "))).map(str::to_string)
    };
    let candidates = [headers.get(RELAY_AUTH_HEADER).and_then(raw), headers.get(header::AUTHORIZATION).and_then(raw)];
    for token in candidates.into_iter().flatten() {
        if let Ok(Some(principal)) = crate::auth::authenticate(&state.store, &token) {
            return if principal.has(Scope::OrchestrationRead) {
                Ok(principal)
            } else {
                Err(refuse(StatusCode::FORBIDDEN, "this token cannot use the relay"))
            };
        }
    }
    Err(refuse(StatusCode::UNAUTHORIZED, "the relay needs a device token for this machine in X-Kybern-Relay-Auth"))
}

/// The link to the target, or the status and message to refuse with.
pub(super) fn route_check(state: &AppState, target: &str) -> Result<Arc<PeerLink>, (StatusCode, &'static str)> {
    let network = &state.network;
    let Some(membership) = network.membership() else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "the machine network is unavailable"));
    };
    if !network.is_enabled() {
        return Err((StatusCode::NOT_FOUND, "the machine network is turned off on this machine"));
    }
    if target == membership.machine_id() {
        return Err((StatusCode::BAD_REQUEST, "this is the machine you are talking to; connect to it directly"));
    }
    if !membership.is_member(target) {
        return Err((StatusCode::NOT_FOUND, "that machine is not in this network"));
    }
    network.peers.link(target).ok_or((StatusCode::BAD_GATEWAY, "this machine has no open session to that machine"))
}

/// The link to the target, or the refusal to send.
#[allow(clippy::result_large_err)]
fn route(state: &AppState, target: &str) -> Result<Arc<PeerLink>, Response> {
    route_check(state, target).map_err(|(status, message)| refuse(status, message))
}

fn open_error(error: RpcError) -> Response {
    let status = match error.code {
        codes::UNAUTHORIZED => StatusCode::UNAUTHORIZED,
        codes::FORBIDDEN => StatusCode::FORBIDDEN,
        codes::NOT_FOUND => StatusCode::NOT_FOUND,
        _ => error
            .data
            .as_ref()
            .and_then(|data| data["status"].as_u64())
            .and_then(|status| StatusCode::from_u16(status as u16).ok())
            .unwrap_or(StatusCode::BAD_GATEWAY),
    };
    refuse(status, error.message)
}

/// A carrier for the WebSocket subprotocols a client offered; `sec-websocket-*` headers are per hop, so
/// they travel under this name and B offers them to the server it connects to.
const PROTOCOLS_HEADER: &str = "x-kybern-ws-protocols";

fn requested_protocols(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all("sec-websocket-protocol")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|protocol| protocol.trim().to_string())
        .filter(|protocol| !protocol.is_empty())
        .collect()
}

fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers.get(header::UPGRADE).and_then(|value| value.to_str().ok()).is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

/// `GET /relay/{machine_id}/ws`.
pub async fn serve_ws(
    State(state): State<AppState>,
    Path(target): Path<String>,
    headers: HeaderMap,
    raw_query: axum::extract::RawQuery,
    ws: WebSocketUpgrade,
) -> Response {
    let path = match raw_query.0 {
        Some(query) => format!("/ws?{query}"),
        None => "/ws".to_string(),
    };
    relay_ws(&state, &target, &headers, path, ws).await
}

/// Open a relayed WebSocket to `path` on `target`: `/ws` for a client's connection, or any path of the
/// target for a page that opens a socket through `/relay/{machine_id}/http/…` (a preview's HMR).
async fn relay_ws(state: &AppState, target: &str, headers: &HeaderMap, path: String, ws: WebSocketUpgrade) -> Response {
    let principal = match caller(state, headers) {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let link = match route(state, target) {
        Ok(link) => link,
        Err(response) => return response,
    };
    open_ws(state, Limits::device(&principal.token_id.to_string()), link, headers, forwarded_headers(headers), path, ws).await
}

/// What ends a page's stream besides the stream itself: its ticket's cancellation and hard cap.
#[derive(Clone)]
struct PageStop {
    cancel: CancellationToken,
    deadline: Instant,
}

/// The stream-limit key, limit and stop of a request: a device's own (`None`), or a page ticket's.
struct Limits<'a> {
    client: &'a str,
    limit: usize,
    stop: Option<PageStop>,
}

impl<'a> Limits<'a> {
    fn device(client: &'a str) -> Self {
        Self { client, limit: MAX_STREAMS_PER_CLIENT, stop: None }
    }
}

/// End `stream` when the page it serves is closed, revoked, evicted or past its hard cap.
fn stop_with(link: &Arc<PeerLink>, stream: &Arc<Stream>, stop: Option<PageStop>) {
    let Some(stop) = stop else { return };
    let link = link.clone();
    let stream = stream.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = stop.cancel.cancelled() => {}
            _ = tokio::time::sleep_until(stop.deadline.into()) => {}
            _ = stream.cancel.cancelled() => return,
        }
        tracing::debug!(target: TARGET, machine = %link.machine_id, stream = stream.id, "a page's ticket ended; closing its stream");
        send_close(&link, stream.id, Some("the page was closed")).await;
        link.relays.remove(stream.id);
    });
}

/// Relay a WebSocket over `link`: `forwarded` are the request headers to pass on, `limits` the stream-limit key.
async fn open_ws(
    state: &AppState,
    limits: Limits<'_>,
    link: Arc<PeerLink>,
    headers: &HeaderMap,
    mut forwarded: Vec<(String, String)>,
    path: String,
    ws: WebSocketUpgrade,
) -> Response {
    // A burst waits for a place (a page opens many connections at once); only a long wait is refused.
    let Some(slot) = state.network.relays.acquire_wait(limits.client, limits.limit, QUEUE_WAIT, &CancellationToken::new()).await else {
        return refuse(StatusCode::TOO_MANY_REQUESTS, "too many relayed connections");
    };
    let Some((stream, incoming)) = link.relays.create(None, StreamKind::Relay, None) else {
        return refuse(StatusCode::INTERNAL_SERVER_ERROR, "stream id clash");
    };
    stop_with(&link, &stream, limits.stop);
    let protocols = requested_protocols(headers);
    if !protocols.is_empty() {
        forwarded.push((PROTOCOLS_HEADER.to_string(), protocols.join(", ")));
    }
    let open = PeerRelayOpenParams { stream_id: stream.id, kind: RelayStreamKind::Ws, path, method: None, headers: forwarded };
    let opened = tokio::time::timeout(OPEN_TIMEOUT, link.call::<PeerRelayOpen>(open)).await;
    match opened {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            link.relays.remove(stream.id);
            return open_error(error);
        }
        Err(_) => {
            link.relays.remove(stream.id);
            return refuse(StatusCode::GATEWAY_TIMEOUT, "the machine did not open the relayed connection in time");
        }
    }
    tracing::debug!(target: TARGET, machine = %link.machine_id, stream = stream.id, "relaying a WebSocket");
    ws.protocols(protocols).on_upgrade(move |socket| async move {
        pump_client_ws(link, stream, incoming, socket).await;
        drop(slot);
    })
}

/// Client socket on M's side: client frames go to B as stream data, B's frames go to the client.
async fn pump_client_ws(link: Arc<PeerLink>, stream: Arc<Stream>, mut incoming: mpsc::Receiver<Chunk>, socket: WebSocket) {
    let (mut sink, mut source) = socket.split();
    loop {
        // Frames already queued for the client go out before a close is honoured.
        tokio::select! {
            biased;
            chunk = incoming.recv() => match chunk {
                Some(Chunk::Text(bytes)) => {
                    let size = bytes.len();
                    let Ok(text) = String::from_utf8(bytes) else { break };
                    if sink.send(Message::Text(text.into())).await.is_err() { break }
                    grant(&link, &stream, size).await;
                }
                Some(Chunk::Binary(bytes)) => {
                    let size = bytes.len();
                    if sink.send(Message::Binary(bytes.into())).await.is_err() { break }
                    grant(&link, &stream, size).await;
                }
                Some(Chunk::End) | None => break,
            },
            frame = source.next() => match frame {
                Some(Ok(Message::Text(text))) => if !send_data(&link, &stream, text.as_bytes(), true).await { break },
                Some(Ok(Message::Binary(bytes))) => if !send_data(&link, &stream, &bytes, false).await { break },
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
            _ = stream.cancel.cancelled() => break,
            _ = link.closed() => break,
        }
    }
    let _ = sink.send(Message::Close(Some(CloseFrame { code: 1000, reason: "".into() }))).await;
    send_close(&link, stream.id, None).await;
    link.relays.remove(stream.id);
}

/// `ANY /relay/{machine_id}/http/{*path}`.
pub async fn serve_http(State(state): State<AppState>, Path((target, path)): Path<(String, String)>, request: Request) -> Response {
    let (mut parts, body) = request.into_parts();
    // A page loaded through the relay opens its WebSockets (a dev server's hot reload) the same way.
    if is_websocket_upgrade(&parts.headers) {
        let path = format!("/{path}");
        if path_refused(&path) {
            return refuse(StatusCode::FORBIDDEN, "the relay does not carry this path");
        }
        let target_path = match parts.uri.query() {
            Some(query) => format!("{path}?{query}"),
            None => path,
        };
        let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
            Ok(ws) => ws,
            Err(rejection) => return rejection.into_response(),
        };
        return relay_ws(&state, &target, &parts.headers, target_path, ws).await;
    }
    let principal = match caller(&state, &parts.headers) {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let link = match route(&state, &target) {
        Ok(link) => link,
        Err(response) => return response,
    };
    let path = format!("/{path}");
    if path_refused(&path) {
        return refuse(StatusCode::FORBIDDEN, "the relay does not carry this path");
    }
    let target_path = match parts.uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path,
    };
    forward_http(
        &state,
        Limits::device(&principal.token_id.to_string()),
        link,
        parts.method.to_string(),
        forwarded_headers(&parts.headers),
        target_path,
        body,
    )
    .await
}

/// Carry one HTTP request over `link` and stream the answer back. `client` is the stream-limit key, `target_path`
/// the origin-relative path and query on the target, already checked.
async fn forward_http(
    state: &AppState,
    limits: Limits<'_>,
    link: Arc<PeerLink>,
    method: String,
    headers: Vec<(String, String)>,
    target_path: String,
    body: Body,
) -> Response {
    // A burst waits for a place (a page opens many connections at once); only a long wait is refused.
    let Some(slot) = state.network.relays.acquire_wait(limits.client, limits.limit, QUEUE_WAIT, &CancellationToken::new()).await else {
        return refuse(StatusCode::TOO_MANY_REQUESTS, "too many relayed connections");
    };
    let Some((stream, mut incoming)) = link.relays.create(None, StreamKind::Relay, None) else {
        return refuse(StatusCode::INTERNAL_SERVER_ERROR, "stream id clash");
    };
    stop_with(&link, &stream, limits.stop);
    let open = PeerRelayOpenParams { stream_id: stream.id, kind: RelayStreamKind::Http, path: target_path, method: Some(method), headers };
    match tokio::time::timeout(OPEN_TIMEOUT, link.call::<PeerRelayOpen>(open)).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            link.relays.remove(stream.id);
            return open_error(error);
        }
        Err(_) => {
            link.relays.remove(stream.id);
            return refuse(StatusCode::GATEWAY_TIMEOUT, "the machine did not open the relayed request in time");
        }
    }

    // The request body flows to B while the response head is awaited.
    let body_link = link.clone();
    let body_stream = stream.clone();
    tokio::spawn(async move {
        let mut data = body.into_data_stream();
        while let Some(piece) = data.next().await {
            let Ok(piece) = piece else { break };
            if !send_chunks(&body_link, &body_stream, &piece, false).await {
                return;
            }
        }
        // An empty text frame ends the request body.
        let _ = send_data(&body_link, &body_stream, &[], true).await;
    });

    let idle = state.network.timing.relay_idle;
    let head = loop {
        match tokio::time::timeout(idle, incoming.recv()).await {
            Ok(Some(Chunk::Text(bytes))) => {
                grant(&link, &stream, bytes.len()).await;
                break serde_json::from_slice::<Value>(&bytes).ok();
            }
            Ok(Some(Chunk::Binary(_))) => continue,
            Ok(Some(Chunk::End)) | Ok(None) | Err(_) => break None,
        }
    };
    let Some(head) = head else {
        send_close(&link, stream.id, Some("no response")).await;
        link.relays.remove(stream.id);
        return refuse(StatusCode::BAD_GATEWAY, "the machine did not answer the relayed request");
    };
    let status = head["status"].as_u64().and_then(|status| StatusCode::from_u16(status as u16).ok()).unwrap_or(StatusCode::BAD_GATEWAY);
    let (tx, rx) = mpsc::channel::<Result<Vec<u8>, std::io::Error>>(32);
    let pump_link = link.clone();
    let pump_stream = stream.clone();
    tokio::spawn(async move {
        loop {
            // Body already received is delivered before a cancellation is honoured.
            let chunk = tokio::select! {
                biased;
                chunk = tokio::time::timeout(idle, incoming.recv()) => chunk,
                _ = pump_stream.cancel.cancelled() => break,
            };
            match chunk {
                Ok(Some(Chunk::Binary(bytes))) => {
                    let size = bytes.len();
                    if tx.send(Ok(bytes)).await.is_err() {
                        break;
                    }
                    grant(&pump_link, &pump_stream, size).await;
                }
                Ok(Some(Chunk::Text(_))) => {}
                Ok(Some(Chunk::End)) | Ok(None) => break,
                Err(_) => {
                    let _ = tx.send(Err(std::io::Error::other("the relayed response went idle"))).await;
                    break;
                }
            }
        }
        send_close(&pump_link, pump_stream.id, None).await;
        pump_link.relays.remove(pump_stream.id);
        drop(slot);
    });
    let mut response = Response::new(Body::from_stream(tokio_stream_from(rx)));
    *response.status_mut() = status;
    for pair in head["headers"].as_array().into_iter().flatten() {
        let (Some(name), Some(value)) = (pair[0].as_str(), pair[1].as_str()) else { continue };
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
            response.headers_mut().append(name, value);
        }
    }
    response
}

/// Request headers for a page request: the usual relay rules, but never an `Authorization` header and no cookie whose
/// value is a Kybern credential (the web view has none; this keeps a page from replaying one). A `Referer` loses the
/// page's ticket prefix (one from another page is dropped), so the ticket never reaches B's dev server, and B is told
/// the prefix (`prefix`, `/relay/<machine>/page/<ticket>`) for the policy it writes.
fn page_headers(state: &AppState, headers: &HeaderMap, prefix: &str) -> Vec<(String, String)> {
    let is_secret = |value: &str| crate::previews::proxy::is_kybern_credential(state, value);
    let mut out = Vec::new();
    for (name, value) in forwarded_headers(headers) {
        match name.as_str() {
            "authorization" => {}
            "cookie" => {
                if let Some(cookie) = crate::previews::proxy::filter_cookie(&value, &is_secret) {
                    out.push((name, cookie));
                }
            }
            "referer" => {
                if let Some(referer) = unprefixed_referer(&value, prefix) {
                    out.push((name, referer));
                }
            }
            _ => out.push((name, value)),
        }
    }
    out.push((relay_pages::PREFIX_HEADER.to_string(), prefix.to_string()));
    out
}

/// `referer` with the page's own ticket prefix taken out of its path, or `None` when it is not this page's address.
fn unprefixed_referer(referer: &str, prefix: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(referer).ok()?;
    let rest = url.path().strip_prefix(prefix).filter(|rest| rest.is_empty() || rest.starts_with('/'))?;
    let rest = if rest.is_empty() { "/".to_string() } else { rest.to_string() };
    url.set_path(&rest);
    Some(url.to_string())
}

/// A page's `Set-Cookie` confined to its own ticket: `Domain` is dropped, and `Path` becomes the page's address on this
/// machine. A page's cookie therefore never reaches another page relayed through this machine, another machine's dev
/// server included. `scope` is the ticket's outside path (`/relay/<machine>/page/<ticket><bound>`); for a subtree the
/// cookie's own path is kept under it.
fn scoped_set_cookie(cookie: &str, scope: &str, document: bool) -> String {
    let mut parts = cookie.split(';');
    let pair = parts.next().unwrap_or_default().trim();
    let mut own_path = None;
    let mut kept = Vec::new();
    for attribute in parts {
        let attribute = attribute.trim();
        let name = attribute.split('=').next().unwrap_or_default().trim().to_ascii_lowercase();
        match name.as_str() {
            "domain" => {}
            "path" => own_path = attribute.split_once('=').map(|(_, value)| value.trim().to_string()),
            _ if attribute.is_empty() => {}
            _ => kept.push(attribute.to_string()),
        }
    }
    let path = if document {
        scope.to_string()
    } else {
        let base = scope.trim_end_matches('/');
        match own_path.filter(|path| path.starts_with('/') && path.bytes().all(|b| b.is_ascii_graphic() && b != b';' && b != b',')) {
            Some(path) if path != "/" => format!("{base}{path}"),
            _ => format!("{base}/"),
        }
    };
    let mut out = pair.to_string();
    for attribute in kept {
        out.push_str("; ");
        out.push_str(&attribute);
    }
    out.push_str("; Path=");
    out.push_str(&path);
    out
}

/// `ANY /relay/{machine_id}/page/{ticket}/{*path}`: a page of another machine, opened with a relay page ticket
/// (`network.relay_page.open`) instead of the relay header. The path is read from the raw URI below, not from the
/// decoded parameter, so it is checked as the page wrote it.
pub async fn serve_page(
    State(state): State<AppState>,
    Path((target, ticket, _path)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    let (mut parts, body) = request.into_parts();
    let Some(PageGrant { info: page, cancel, deadline }) = state.network.pages.lookup(&ticket) else {
        return refuse(StatusCode::NOT_FOUND, "this page address has expired");
    };
    if page.target != target {
        return refuse(StatusCode::NOT_FOUND, "this page address has expired");
    }
    if !state.store.token_is_active(page.token_id).unwrap_or(false) {
        return refuse(StatusCode::UNAUTHORIZED, "the device that opened this page was revoked");
    }
    let link = match route(&state, &target) {
        Ok(link) => link,
        Err(response) => return response,
    };
    // The raw path keeps percent-encoding as the page wrote it; it is normalized and bound-checked below.
    let base = format!("/relay/{target}/page/{ticket}");
    let Some(rest) = parts.uri.path().strip_prefix(base.as_str()).filter(|rest| rest.is_empty() || rest.starts_with('/')) else {
        return refuse(StatusCode::NOT_FOUND, "this page address has expired");
    };
    let requested = match parts.uri.query() {
        Some(query) => format!("{rest}?{query}"),
        None => rest.to_string(),
    };
    let Some(url) = local_url("http", std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 1)), &requested) else {
        return refuse(StatusCode::FORBIDDEN, "the relay does not carry this path");
    };
    if !page.admits(url.path()) {
        return refuse(StatusCode::FORBIDDEN, "this address is outside the page that was opened");
    }
    let target_path = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };
    // The page's streams count under its own ticket, with their own small cap, and stop with it.
    let client = format!("page:{ticket}");
    let limits = Limits { client: &client, limit: MAX_STREAMS_PER_PAGE, stop: Some(PageStop { cancel, deadline }) };
    if is_websocket_upgrade(&parts.headers) {
        if page.document {
            return refuse(StatusCode::FORBIDDEN, "this page address does not open connections");
        }
        let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
            Ok(ws) => ws,
            Err(rejection) => return rejection.into_response(),
        };
        let forwarded = page_headers(&state, &parts.headers, &base);
        return open_ws(&state, limits, link, &parts.headers, forwarded, target_path, ws).await;
    }
    let headers = page_headers(&state, &parts.headers, &base);
    let mut response = forward_http(&state, limits, link, parts.method.to_string(), headers, target_path, body).await;
    let scope = format!("{base}{}", page.bound);
    let cookies: Vec<String> = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(|cookie| scoped_set_cookie(cookie, &scope, page.document))
        .collect();
    response.headers_mut().remove(header::SET_COOKIE);
    for cookie in cookies {
        if let Ok(value) = HeaderValue::from_str(&cookie) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

fn tokio_stream_from(
    mut rx: mpsc::Receiver<Result<Vec<u8>, std::io::Error>>,
) -> impl futures::Stream<Item = Result<Vec<u8>, std::io::Error>> {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

// ---- B: the local end ----

/// Where B reaches its own listener: its primary address with a wildcard mapped to loopback.
fn local_addr(state: &AppState) -> Result<std::net::SocketAddr, RpcError> {
    let mut addr = (*state.listen_addr.read().unwrap_or_else(|poisoned| poisoned.into_inner()))
        .ok_or_else(|| RpcError::internal("the daemon is still starting"))?;
    if addr.ip().is_unspecified() {
        addr.set_ip(if addr.is_ipv4() { std::net::Ipv4Addr::LOCALHOST.into() } else { std::net::Ipv6Addr::LOCALHOST.into() });
    }
    Ok(addr)
}

/// `peer.relay.open`: open the local end of a stream M asked for.
pub async fn handle_open(state: &AppState, link: &Arc<PeerLink>, params: PeerRelayOpenParams) -> Result<Value, RpcError> {
    let addr = local_addr(state)?;
    let scheme = match params.kind {
        RelayStreamKind::Ws => "ws",
        RelayStreamKind::Http => "http",
    };
    let Some(url) = local_url(scheme, addr, &params.path) else {
        return Err(RpcError::new(codes::FORBIDDEN, "the relay does not carry this path"));
    };
    let mut params = params;
    // Only this process can mark its own request as relayed; a mark the other machine sent is not kept.
    params.headers.retain(|(name, _)| !name.eq_ignore_ascii_case(relay_pages::LOCAL_MARK_HEADER));
    params.headers.push((relay_pages::LOCAL_MARK_HEADER.to_string(), state.network.pages.local_mark().to_string()));
    let slot = state
        .network
        .relays
        .acquire_within(&format!("inbound:{}", link.machine_id), MAX_INBOUND_PER_MEMBER)
        .ok_or_else(|| RpcError::new(codes::CONFLICT, "too many relayed connections on this machine"))?;
    let (stream, incoming) = link
        .relays
        .create(Some(params.stream_id), StreamKind::Relay, Some(Scope::MachineRelay))
        .ok_or_else(|| RpcError::invalid_params("that stream id is in use"))?;
    let result = match params.kind {
        RelayStreamKind::Ws => open_local_ws(state, link, &stream, incoming, url, &params, slot).await,
        RelayStreamKind::Http => open_local_http(link, &stream, incoming, url, params, slot).await,
    };
    if result.is_err() {
        link.relays.remove(stream.id);
    }
    result.map(|()| json!({}))
}

fn status_error(status: u16, message: String) -> RpcError {
    let code = match status {
        401 => codes::UNAUTHORIZED,
        403 => codes::FORBIDDEN,
        404 => codes::NOT_FOUND,
        _ => codes::INTERNAL_ERROR,
    };
    RpcError::new(code, message).with_data(json!({ "status": status }))
}

async fn open_local_ws(
    _state: &AppState,
    link: &Arc<PeerLink>,
    stream: &Arc<Stream>,
    mut incoming: mpsc::Receiver<Chunk>,
    url: reqwest::Url,
    params: &PeerRelayOpenParams,
    slot: Slot,
) -> Result<(), RpcError> {
    let mut request = url.as_str().into_client_request().map_err(RpcError::internal)?;
    for (name, value) in &params.headers {
        if name == PROTOCOLS_HEADER {
            if let Ok(value) = TValue::from_str(value) {
                request.headers_mut().insert("sec-websocket-protocol", value);
            }
            continue;
        }
        if matches!(name.as_str(), "host" | "content-type" | "accept-encoding") || hop_by_hop(name) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (TName::from_bytes(name.as_bytes()), TValue::from_str(value)) {
            request.headers_mut().insert(name, value);
        }
    }
    let (socket, _) = match tokio_tungstenite::connect_async(request).await {
        Ok(connected) => connected,
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            let status = response.status().as_u16();
            return Err(status_error(status, format!("the machine refused the connection ({status})")));
        }
        Err(error) => return Err(RpcError::internal(format!("could not open the local connection: {error}"))),
    };
    let (mut sink, mut source) = socket.split();
    let link = link.clone();
    let stream = stream.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                chunk = incoming.recv() => match chunk {
                    Some(Chunk::Text(bytes)) => {
                        let size = bytes.len();
                        let Ok(text) = String::from_utf8(bytes) else { break };
                        if sink.send(WsMessage::Text(text.into())).await.is_err() { break }
                        grant(&link, &stream, size).await;
                    }
                    Some(Chunk::Binary(bytes)) => {
                        let size = bytes.len();
                        if sink.send(WsMessage::Binary(bytes.into())).await.is_err() { break }
                        grant(&link, &stream, size).await;
                    }
                    Some(Chunk::End) | None => break,
                },
                frame = source.next() => match frame {
                    Some(Ok(WsMessage::Text(text))) => if !send_data(&link, &stream, text.as_bytes(), true).await { break },
                    Some(Ok(WsMessage::Binary(bytes))) => if !send_data(&link, &stream, &bytes, false).await { break },
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                },
                _ = stream.cancel.cancelled() => break,
                _ = link.closed() => break,
            }
        }
        let _ = sink.close().await;
        send_close(&link, stream.id, None).await;
        link.relays.remove(stream.id);
        drop(slot);
    });
    Ok(())
}

async fn open_local_http(
    link: &Arc<PeerLink>,
    stream: &Arc<Stream>,
    mut incoming: mpsc::Receiver<Chunk>,
    url: reqwest::Url,
    params: PeerRelayOpenParams,
    slot: Slot,
) -> Result<(), RpcError> {
    let method = params.method.as_deref().unwrap_or("GET");
    let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| RpcError::invalid_params("bad method"))?;
    let client = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build().map_err(RpcError::internal)?;
    let mut request = client.request(method.clone(), url);
    for (name, value) in &params.headers {
        if hop_by_hop(name) {
            continue;
        }
        // `Host` is kept: the well-known proof covers the host the client asked for.
        request = request.header(name, value);
    }
    let has_body = !matches!(method, reqwest::Method::GET | reqwest::Method::HEAD | reqwest::Method::OPTIONS);
    let link = link.clone();
    let stream = stream.clone();
    tokio::spawn(async move {
        let (body_tx, body_rx) = mpsc::channel::<Result<Vec<u8>, std::io::Error>>(32);
        let request = if has_body { request.body(reqwest::Body::wrap_stream(tokio_stream_from(body_rx))) } else { request };
        // Request body: stream data from M into the local request until the empty text frame.
        let feed_link = link.clone();
        let feed_stream = stream.clone();
        let feed = tokio::spawn(async move {
            loop {
                let chunk = tokio::select! {
                    biased;
                    chunk = incoming.recv() => chunk,
                    _ = feed_stream.cancel.cancelled() => return,
                };
                match chunk {
                    Some(Chunk::Binary(bytes)) => {
                        let size = bytes.len();
                        if body_tx.send(Ok(bytes)).await.is_err() {
                            return;
                        }
                        grant(&feed_link, &feed_stream, size).await;
                    }
                    Some(Chunk::Text(_)) => return, // end of body
                    Some(Chunk::End) | None => {
                        let _ = body_tx.send(Err(std::io::Error::other("the request was cancelled"))).await;
                        return;
                    }
                }
            }
        });
        let response = if has_body {
            request.send().await
        } else {
            feed.abort();
            request.send().await
        };
        match response {
            Ok(response) => {
                let headers: Vec<Value> = response
                    .headers()
                    .iter()
                    .filter(|(name, _)| !hop_by_hop(name.as_str()))
                    .filter_map(|(name, value)| Some(json!([name.as_str(), value.to_str().ok()?])))
                    .collect();
                let head = json!({ "status": response.status().as_u16(), "headers": headers });
                if send_data(&link, &stream, head.to_string().as_bytes(), true).await {
                    let mut body = response.bytes_stream();
                    while let Some(piece) = body.next().await {
                        let Ok(piece) = piece else { break };
                        if !send_chunks(&link, &stream, &piece, false).await {
                            break;
                        }
                    }
                }
            }
            Err(error) => {
                let head = json!({ "status": 502, "headers": [] });
                tracing::debug!(target: TARGET, %error, "the local request of a relayed stream failed");
                let _ = send_data(&link, &stream, head.to_string().as_bytes(), true).await;
            }
        }
        send_close(&link, stream.id, None).await;
        link.relays.remove(stream.id);
        drop(slot);
    });
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn binary_frames_round_trip_and_unknown_kinds_are_ignored() {
        let frame = encode_frame(StreamKind::Ports, 0x0102_0304_0506_0708, true, b"hello");
        assert_eq!(frame.len(), FRAME_HEADER + 5);
        let (kind, id, text, bytes) = decode_frame(&frame).unwrap();
        assert_eq!((kind, id, text, bytes), (StreamKind::Ports, 0x0102_0304_0506_0708, true, &b"hello"[..]));
        let empty = encode_frame(StreamKind::Relay, 3, false, b"");
        assert_eq!(decode_frame(&empty).unwrap(), (StreamKind::Relay, 3, false, &b""[..]));
        let mut unknown = frame.clone();
        unknown[0] = 9;
        assert!(decode_frame(&unknown).is_none());
        assert!(decode_frame(&frame[..4]).is_none());
        // A stream created after the other machine listed the larger window may send that much.
        let relays = LinkRelays::new(Direction::Dialed);
        let (small, _rx) = relays.create(None, StreamKind::Ports, None).unwrap();
        relays.set_large_window(true);
        let (large, _rx2) = relays.create(None, StreamKind::Ports, None).unwrap();
        assert_eq!(small.credit.balance.load(Ordering::SeqCst), WINDOW);
        assert_eq!(large.credit.balance.load(Ordering::SeqCst), RELAY_WINDOW_V2);
        assert!(QUEUE_FRAMES as i64 * MIN_FRAME_COST as i64 >= RELAY_WINDOW_V2, "the queue holds a full window of small frames");
    }

    #[test]
    fn many_streams_on_one_session_share_the_large_window_instead_of_each_taking_it() {
        assert_eq!(stream_window(WINDOW, 100), WINDOW, "an older machine keeps the original window");
        assert_eq!(stream_window(RELAY_WINDOW_V2, 0), RELAY_WINDOW_V2);
        assert_eq!(stream_window(RELAY_WINDOW_V2, 3), RELAY_WINDOW_V2, "a few streams each get the whole window");
        assert_eq!(stream_window(RELAY_WINDOW_V2, 7), LINK_WINDOW_BUDGET / 8);
        assert_eq!(stream_window(RELAY_WINDOW_V2, 127), WINDOW, "never below the original window");
        let relays = LinkRelays::new(Direction::Accepted);
        relays.set_large_window(true);
        let held: Vec<_> = (0..MAX_INBOUND_PER_MEMBER).map(|_| relays.create(None, StreamKind::Ports, None).unwrap()).collect();
        let in_flight: i64 = held.iter().map(|(stream, _)| stream.credit.balance.load(Ordering::SeqCst)).sum();
        // 128 streams at 4 MiB each would be 512 MiB the other machine may have to queue.
        assert!(in_flight < 96 * 1024 * 1024, "{in_flight} bytes in flight on one session");
        assert!(held.iter().all(|(stream, _)| stream.credit.balance.load(Ordering::SeqCst) >= WINDOW));
    }

    use super::*;

    #[test]
    fn a_pages_cookies_are_confined_to_its_own_address() {
        let scope = "/relay/b/page/t/preview-proxy/x/";
        assert_eq!(scoped_set_cookie("sid=1; Path=/; HttpOnly", scope, false), "sid=1; HttpOnly; Path=/relay/b/page/t/preview-proxy/x/");
        assert_eq!(
            scoped_set_cookie("a=2; Domain=example.com; path=/api; Secure", scope, false),
            "a=2; Secure; Path=/relay/b/page/t/preview-proxy/x/api"
        );
        assert_eq!(scoped_set_cookie("a=3", scope, false), "a=3; Path=/relay/b/page/t/preview-proxy/x/", "no path is the page's root");
        assert_eq!(
            scoped_set_cookie("a=4; Path=api", scope, false),
            "a=4; Path=/relay/b/page/t/preview-proxy/x/",
            "a relative path is not kept"
        );
        assert_eq!(scoped_set_cookie("a=5; Path=/x;y", scope, false), "a=5; y; Path=/relay/b/page/t/preview-proxy/x/x");
        let document = "/relay/b/page/t/visual-frame/v";
        assert_eq!(scoped_set_cookie("a=6; Path=/anything", document, true), "a=6; Path=/relay/b/page/t/visual-frame/v");
    }

    #[test]
    fn a_referer_loses_its_page_prefix_and_another_pages_is_dropped() {
        let prefix = "/relay/b/page/t";
        assert_eq!(
            unprefixed_referer("http://m:4173/relay/b/page/t/preview-proxy/x/app?q=1", prefix).as_deref(),
            Some("http://m:4173/preview-proxy/x/app?q=1")
        );
        assert_eq!(unprefixed_referer("http://m:4173/relay/b/page/t", prefix).as_deref(), Some("http://m:4173/"));
        assert_eq!(unprefixed_referer("http://m:4173/relay/b/page/other/preview-proxy/x/", prefix), None);
        assert_eq!(unprefixed_referer("http://m:4173/relay/b/page/tt/x", prefix), None);
        assert_eq!(unprefixed_referer("not a url", prefix), None);
    }

    #[test]
    fn page_streams_count_under_their_own_cap() {
        let hub = RelayHub::default();
        let page: Vec<_> = (0..MAX_STREAMS_PER_PAGE).map(|_| hub.acquire_within("page:t", MAX_STREAMS_PER_PAGE).unwrap()).collect();
        assert!(hub.acquire_within("page:t", MAX_STREAMS_PER_PAGE).is_none(), "the page is at its cap");
        assert!(hub.acquire("device").is_some(), "the device's own streams are not the page's");
        drop(page);
        assert!(hub.acquire_within("page:t", MAX_STREAMS_PER_PAGE).is_some());
    }

    #[test]
    fn page_headers_never_carry_the_daemons_page_headers_a_client_sent() {
        let mut headers = HeaderMap::new();
        headers.insert(relay_pages::PREFIX_HEADER, HeaderValue::from_static("/relay/x/page/y"));
        headers.insert(relay_pages::LOCAL_MARK_HEADER, HeaderValue::from_static("forged"));
        headers.insert("accept", HeaderValue::from_static("text/html"));
        let forwarded = forwarded_headers(&headers);
        assert_eq!(forwarded, vec![("accept".to_string(), "text/html".to_string())]);
    }

    #[test]
    fn hop_by_hop_headers_and_the_relay_header_never_cross() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer b-token"));
        headers.insert(RELAY_AUTH_HEADER, HeaderValue::from_static("Bearer m-token"));
        headers.insert("connection", HeaderValue::from_static("upgrade"));
        headers.insert("sec-websocket-key", HeaderValue::from_static("x"));
        headers.insert("host", HeaderValue::from_static("100.64.0.1:4173"));
        headers.insert("x-forwarded-for", HeaderValue::from_static("1.2.3.4"));
        let forwarded: Vec<String> = forwarded_headers(&headers).into_iter().map(|(name, _)| name).collect();
        assert!(forwarded.contains(&"authorization".to_string()), "the client's own credential travels end to end");
        assert!(forwarded.contains(&"host".to_string()), "the proof covers the host the client asked for");
        assert!(!forwarded.iter().any(|name| name == RELAY_AUTH_HEADER
            || name == "connection"
            || name.starts_with("sec-websocket")
            || name == "x-forwarded-for"));
    }

    #[test]
    fn the_relay_carries_no_onward_hops_and_no_handshakes() {
        assert!(path_refused("/relay/abc/ws"));
        assert!(path_refused("/network/session"));
        assert!(!path_refused("/ws"));
        assert!(!path_refused("/.well-known/kybern/machine"));
        assert!(!path_refused("/preview-files/t/index.html"));
    }

    #[test]
    fn a_relayed_path_stays_on_the_daemon_and_is_checked_after_normalization() {
        let addr = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 4199));
        // Concatenated, this would make the daemon's address user-info and connect to port 8080.
        let naive = reqwest::Url::parse(&format!("http://{addr}@127.0.0.1:8080/admin")).unwrap();
        assert_eq!(naive.port(), Some(8080), "the parser reqwest uses reads the authority this way");
        for path in [
            "@127.0.0.1:8080/admin",
            ":8080/admin",
            "//127.0.0.1:8080/admin",
            "/\\127.0.0.1:8080/admin",
            "/ok/../network/session",
            "/ok/%2e%2e/network/session",
            "/ok/.%2E/relay/abc/ws",
            "/%6Eetwork/session",
            "/Network/session",
            "/x/..%2Fnetwork/session",
            "/ws#frag",
            "",
        ] {
            assert!(path_refused(path), "{path:?} is refused");
            assert!(local_url("http", addr, path).is_none(), "{path:?} gets no URL");
        }
        for path in ["/ws?token=abc", "/preview-files/t/index.html", "/.well-known/kybern/machine", "/api/x?q=a@b:c"] {
            let url = local_url("http", addr, path).unwrap_or_else(|| panic!("{path:?} is carried"));
            assert_eq!((url.host_str(), url.port()), (Some("127.0.0.1"), Some(4199)));
        }
        let ws = local_url("ws", std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, 4199)), "/ws").unwrap();
        assert_eq!(ws.as_str(), "ws://[::1]:4199/ws");
    }

    #[test]
    fn stream_ids_do_not_collide_between_the_two_directions() {
        let dialer = LinkRelays::new(Direction::Dialed);
        let acceptor = LinkRelays::new(Direction::Accepted);
        let (a, _) = dialer.create(None, StreamKind::Relay, None).unwrap();
        let (b, _) = dialer.create(None, StreamKind::Ports, None).unwrap();
        let (c, _) = acceptor.create(None, StreamKind::Relay, None).unwrap();
        assert_eq!((a.id, b.id, c.id), (1, 3, 2));
        assert!(dialer.create(Some(1), StreamKind::Ports, None).is_none(), "an id in use is refused");
        assert!(
            dialer.get(1, StreamKind::Ports).is_none() && dialer.get(1, StreamKind::Relay).is_some(),
            "a frame of the other kind finds nothing"
        );
    }

    #[test]
    fn a_withdrawn_capability_ends_the_streams_served_for_it_and_nothing_else() {
        let relays = LinkRelays::new(Direction::Accepted);
        let (port, _port_rx) = relays.create(Some(11), StreamKind::Ports, Some(Scope::MachinePorts)).unwrap();
        let (relay, _relay_rx) = relays.create(Some(13), StreamKind::Relay, Some(Scope::MachineRelay)).unwrap();
        let (own, _own_rx) = relays.create(None, StreamKind::Ports, None).unwrap();
        assert_eq!(relays.close_unpermitted(&[Scope::MachinePeer, Scope::MachinePorts, Scope::MachineRelay]), 0);
        // Ports turned off: the port stream it serves ends; the relay and the stream this side opened stay.
        assert_eq!(relays.close_unpermitted(&[Scope::MachinePeer, Scope::MachineRelay]), 1);
        assert!(port.cancel.is_cancelled() && !relay.cancel.is_cancelled() && !own.cancel.is_cancelled());
        // Paused: only machine:peer is left.
        assert_eq!(relays.close_unpermitted(&[Scope::MachinePeer]), 1);
        assert!(relay.cancel.is_cancelled() && !own.cancel.is_cancelled());
        assert_eq!(relays.len(), 1);
    }

    #[test]
    fn streams_are_counted_against_per_client_and_daemon_limits() {
        let hub = RelayHub::default();
        let slots: Vec<_> = (0..MAX_STREAMS_PER_CLIENT).map(|_| hub.acquire("c1").unwrap()).collect();
        assert!(hub.acquire("c1").is_none());
        let others: Vec<_> =
            (0..(MAX_STREAMS_PER_DAEMON - MAX_STREAMS_PER_CLIENT)).map(|i| hub.acquire(&format!("o{}", i / 10)).unwrap()).collect();
        assert!(hub.acquire("fresh").is_none(), "the daemon limit holds");
        drop(slots);
        assert!(hub.acquire("c1").is_some());
        drop(others);
        assert_eq!(hub.open_streams(), 0);
    }

    #[tokio::test]
    async fn credit_blocks_a_sender_until_the_receiver_grants_more_and_stops_on_cancel() {
        let credit = Arc::new(Credit::new(WINDOW));
        let cancel = CancellationToken::new();
        // A message larger than the window goes through once and leaves a debt.
        assert!(credit.take(WINDOW as usize + 1000, &cancel).await);
        let waiting = {
            let credit = credit.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move { credit.take(10, &cancel).await })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!waiting.is_finished(), "in debt, the next send waits");
        credit.add(2000);
        assert!(waiting.await.unwrap());
        assert!(credit.take(5000, &cancel).await, "990 bytes left, so one more goes and leaves a debt");
        cancel.cancel();
        assert!(!credit.take(WINDOW as usize, &cancel).await, "a cancelled stream stops waiting for credit");
    }
}
