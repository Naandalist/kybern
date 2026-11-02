//! WebSocket transport: authenticate the upgrade, then run one JSON-RPC
//! connection with its own subscription set.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, FromRequestParts, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use kybern_protocol::methods::scope_for;
use kybern_protocol::*;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

use crate::auth::{Principal, PrincipalKind, authenticate, effective_scopes, principal_is_active};
use crate::network::peers::{self, Direction, FrameSink, PeerLink};
use crate::network::session::GRANT_PREFIX;
use crate::state::AppState;

/// The address a request came from, when the listener records it (`into_make_service_with_connect_info`).
/// Never rejects: tests that serve the router without connection info get `None`.
pub struct Peer(pub Option<std::net::SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for Peer {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Peer(parts.extensions.get::<ConnectInfo<std::net::SocketAddr>>().map(|connection| connection.0)))
    }
}

pub async fn upgrade(
    State(state): State<AppState>,
    Peer(peer): Peer,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    if headers.contains_key(axum::http::header::ORIGIN) && crate::http::allowed_asset_origin(&headers).is_none() {
        return (StatusCode::FORBIDDEN, "origin cannot connect").into_response();
    }
    let header_token = headers
        .get(AUTH_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
        .map(str::to_string);
    // A session grant from another machine (design §3.3). It is accepted from the header only, from
    // the address it was issued to, and only while that machine is a member. Device tokens are
    // `kyb_` plus random base64url, so about one in 4096 of them also starts with the grant prefix:
    // a token that is not a live grant is still tried as a device token.
    let grant =
        header_token.as_deref().filter(|token| token.starts_with(GRANT_PREFIX)).and_then(|token| state.network.grants.lookup(token));
    if let Some(grant) = grant {
        let from_origin = peer.is_some_and(|peer| peer.ip() == grant.origin_ip);
        let member = state.network.membership().is_some_and(|membership| membership.is_member(&grant.machine_id));
        if !from_origin || !member || !state.network.is_enabled() {
            tracing::warn!(target: "kybern::network::session", machine = %grant.machine_id, from_origin, member, "refused a session socket");
            return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
        }
        let label = state
            .network
            .membership()
            .and_then(|m| m.projection().entry(&grant.machine_id).map(|e| e.descriptor.name.clone()))
            .unwrap_or_default();
        let principal = Principal {
            token_id: grant.id,
            label,
            scopes: grant.scopes.clone(),
            kind: PrincipalKind::Machine { machine_id: grant.machine_id.clone(), origin_ip: grant.origin_ip },
        };
        return ws.on_upgrade(move |socket| run_machine(state, socket, principal));
    }
    let raw = header_token.or_else(|| query.get(AUTH_QUERY_PARAM).cloned());
    let authentication = if let Some(ticket) = query.get("ticket") {
        state.tickets.redeem(&state.store, ticket)
    } else if let Some(raw) = raw {
        authenticate(&state.store, &raw)
    } else {
        return (StatusCode::UNAUTHORIZED, "missing credential").into_response();
    };
    let principal = match authentication {
        Ok(Some(p)) => p,
        Ok(None) => return (StatusCode::UNAUTHORIZED, "invalid token").into_response(),
        Err(e) => {
            tracing::error!(%e, "auth lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "auth failure").into_response();
        }
    };
    ws.on_upgrade(move |socket| run(state, socket, principal))
}

/// Serialized output owns permits until the socket write completes. One
/// oversized response may use the whole budget, so history is never truncated.
const OUTBOX_BYTES: usize = 8 * 1024 * 1024;
struct QueuedFrame {
    text: String,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

#[derive(Clone)]
struct Outbox {
    sender: mpsc::Sender<QueuedFrame>,
    budget: Arc<tokio::sync::Semaphore>,
    closed: tokio_util::sync::CancellationToken,
}

// Internal serialization-only envelopes. Public wire/schema types stay unchanged.
// These typed notifications always have object params; null-param notifications
// keep using RpcNotification so its skip-null behavior is preserved.
#[derive(serde::Serialize)]
struct TypedNotification<P> {
    jsonrpc: JsonRpcVersion,
    method: &'static str,
    params: P,
}

#[derive(serde::Serialize)]
struct BorrowedEventNotification<'a> {
    subscription_id: SubscriptionId,
    event: &'a ThreadEvent,
}

/// Build the small event sent to clients that opt into lazy tool outputs.
/// Constructing a fresh `ThreadEvent` here intentionally copies only scalar
/// metadata and the call id. Cloning the original `Value` before serialization
/// would briefly retain a second copy of a large result, defeating the point
/// of compact delivery.
fn compact_tool_completion(event: &ThreadEvent) -> Option<ThreadEvent> {
    let EventPayload::ToolCallCompleted { tool_call_id, output, output_omitted, is_error, .. } = &event.payload else {
        return None;
    };
    if *output_omitted || !kybern_store::should_omit_tool_output(output) {
        return None;
    }
    Some(ThreadEvent {
        seq: event.seq,
        thread_id: event.thread_id,
        turn_id: event.turn_id,
        at: event.at,
        payload: EventPayload::ToolCallCompleted {
            tool_call_id: tool_call_id.clone(),
            output: Value::Null,
            output_omitted: true,
            stream_recoverable: true,
            is_error: *is_error,
        },
    })
}

/// Mark small completions as stream-recoverable too. A large delta stream can
/// legitimately end in a tiny or null canonical result, so capability cannot
/// depend on the completion payload crossing the omission threshold.
fn mark_stream_recoverable(event: &ThreadEvent) -> Option<ThreadEvent> {
    let EventPayload::ToolCallCompleted { tool_call_id, output, output_omitted, stream_recoverable, is_error } = &event.payload else {
        return None;
    };
    if *stream_recoverable {
        return None;
    }
    Some(ThreadEvent {
        seq: event.seq,
        thread_id: event.thread_id,
        turn_id: event.turn_id,
        at: event.at,
        payload: EventPayload::ToolCallCompleted {
            tool_call_id: tool_call_id.clone(),
            output: output.clone(),
            output_omitted: *output_omitted,
            stream_recoverable: true,
            is_error: *is_error,
        },
    })
}

impl Outbox {
    async fn notify(&self, method: &'static str, params: impl serde::Serialize) -> Result<(), ()> {
        self.send(TypedNotification { jsonrpc: JsonRpcVersion, method, params }).await
    }

    async fn send(&self, frame: impl serde::Serialize) -> Result<(), ()> {
        struct Count(usize);
        impl std::io::Write for Count {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self.0.saturating_add(bytes.len());
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut size = Count(0);
        serde_json::to_writer(&mut size, &frame).map_err(|_| ())?;
        let send = async {
            let permit = self.budget.clone().acquire_many_owned(size.0.clamp(1, OUTBOX_BYTES) as u32).await.map_err(|_| ())?;
            let text = serde_json::to_string(&frame).map_err(|_| ())?;
            self.sender.send(QueuedFrame { text, _permit: permit }).await.map_err(|_| ())
        };
        tokio::select! {
            _ = self.closed.cancelled() => Err(()),
            result = tokio::time::timeout(std::time::Duration::from_secs(30), send) => match result {
                Ok(result) => result,
                Err(_) => { self.closed.cancel(); Err(()) }
            }
        }
    }
}

struct Subscription {
    thread_id: Option<ThreadId>,
    /// Live events at or below this seq were covered by replay and are skipped.
    floor_seq: EventSeq,
    /// Whether completed tool outputs should be included in the wire event.
    include_tool_output: bool,
}

pub struct ConnectionCtx {
    pub id: Uuid,
    pub principal: Principal,
    /// A replay is a single ordered delivery operation. Live events stay in
    /// the broadcast receiver until acknowledgment and replay are enqueued.
    delivery: Mutex<()>,
    subs: Mutex<HashMap<SubscriptionId, Subscription>>,
    terminal_subs: Mutex<HashMap<TerminalId, tokio::task::JoinHandle<()>>>,
    out: Outbox,
}

impl ConnectionCtx {
    pub async fn subscribe(&self, thread_id: Option<ThreadId>, head_seq: EventSeq, include_tool_output: bool) -> SubscriptionId {
        let id = Uuid::now_v7();
        self.subs.lock().await.insert(id, Subscription { thread_id, floor_seq: head_seq, include_tool_output });
        id
    }

    pub async fn unsubscribe(&self, id: SubscriptionId) {
        self.subs.lock().await.remove(&id);
    }

    async fn establish_subscription(&self, state: &AppState, request_id: RpcId, params: Value) {
        use kybern_protocol::methods::{EventsSubscribeParams, EventsSubscribeResult};
        let params: EventsSubscribeParams = match serde_json::from_value(if params.is_null() { serde_json::json!({}) } else { params }) {
            Ok(params) => params,
            Err(e) => {
                let _ = self.out.send(ServerFrame::Response(RpcResponse::err(request_id, RpcError::invalid_params(e.to_string())))).await;
                return;
            }
        };
        let _delivery = self.delivery.lock().await;
        let head_seq = match state.store.events_head_seq() {
            Ok(head) => head,
            Err(e) => {
                let _ = self.out.send(ServerFrame::Response(RpcResponse::err(request_id, RpcError::internal(e.to_string())))).await;
                return;
            }
        };
        if params.after_seq.is_some_and(|after| after > head_seq) {
            let _ = self
                .out
                .send(ServerFrame::Response(RpcResponse::err(
                    request_id,
                    RpcError::invalid_params("Event cursor is ahead of this environment"),
                )))
                .await;
            return;
        }
        let include_tool_output = params.include_tool_output.unwrap_or(true);
        let subscription_id = self.subscribe(params.thread_id, head_seq, include_tool_output).await;
        let result = serde_json::to_value(EventsSubscribeResult { subscription_id, head_seq, replay_ready: true }).unwrap();
        if self.out.send(ServerFrame::Response(RpcResponse::ok(request_id, result))).await.is_err() {
            return;
        }
        if let Some(after) = params.after_seq
            && self.replay(state, subscription_id, params.thread_id, after, head_seq, include_tool_output).await.is_err()
        {
            let _ =
                self.out.send(ServerFrame::Notification(RpcNotification::new("events.lagged", serde_json::json!({ "dropped": 0 })))).await;
            return;
        }
        let ready = kybern_protocol::EventsReadyNotification { subscription_id, head_seq };
        let _ = self.out.notify(kybern_protocol::EVENTS_READY_NOTIFICATION, ready).await;
    }

    /// Forward a terminal's output to this connection until it exits or is unsubscribed.
    pub async fn subscribe_terminal(&self, terminal: Arc<crate::terminal::Terminal>, replay: bool) {
        use base64::Engine;
        let id = terminal.info().id;
        self.unsubscribe_terminal(id).await;
        let (mut rx, data) = terminal.subscribe_output(replay);
        let replay_data = (replay && !data.is_empty()).then(|| base64::engine::general_purpose::STANDARD.encode(&data));
        // The terminal's own ring stays intact. Release only this owned replay copy
        // before a slow socket can make the serialized notification wait.
        drop(data);
        let out = self.out.clone();
        if replay {
            if let Some(data) = replay_data {
                let params = kybern_protocol::methods::TerminalOutputNotification { terminal_id: id, data };
                let _ = out.notify(kybern_protocol::methods::TERMINAL_OUTPUT_NOTIFICATION, params).await;
            }
            if !terminal.info().alive {
                let params = kybern_protocol::methods::TerminalExitedNotification { terminal_id: id, exit_code: terminal.info().exit_code };
                let _ = out.notify(kybern_protocol::methods::TERMINAL_EXITED_NOTIFICATION, params).await;
                return;
            }
        }
        let handle = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => match &*ev {
                        crate::terminal::TerminalEvent::Output(bytes) => {
                            let params = kybern_protocol::methods::TerminalOutputNotification {
                                terminal_id: id,
                                data: base64::engine::general_purpose::STANDARD.encode(bytes),
                            };
                            if out.notify(kybern_protocol::methods::TERMINAL_OUTPUT_NOTIFICATION, params).await.is_err() {
                                break;
                            }
                        }
                        crate::terminal::TerminalEvent::Exited(code) => {
                            let params = kybern_protocol::methods::TerminalExitedNotification { terminal_id: id, exit_code: *code };
                            let _ = out.notify(kybern_protocol::methods::TERMINAL_EXITED_NOTIFICATION, params).await;
                            break;
                        }
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let _ = out
                            .send(ServerFrame::Notification(RpcNotification::new(
                                "terminal.lagged",
                                serde_json::json!({ "terminal_id": id }),
                            )))
                            .await;
                        break;
                    }
                    Err(_) => break,
                }
            }
        });
        self.terminal_subs.lock().await.insert(id, handle);
    }

    pub async fn unsubscribe_terminal(&self, id: TerminalId) {
        if let Some(h) = self.terminal_subs.lock().await.remove(&id) {
            h.abort();
        }
    }

    pub async fn replay(
        &self,
        state: &AppState,
        subscription_id: SubscriptionId,
        thread_id: Option<ThreadId>,
        after: EventSeq,
        head: EventSeq,
        include_tool_output: bool,
    ) -> anyhow::Result<()> {
        let mut cursor = after;
        while cursor < head {
            let store = state.store.clone();
            let batch = tokio::task::spawn_blocking(move || {
                if include_tool_output {
                    store.events_after_bounded(thread_id, cursor, 500, 2 * 1024 * 1024)
                } else {
                    store.events_after_bounded_compact(thread_id, cursor, 500, 2 * 1024 * 1024)
                }
            })
            .await??;
            if batch.is_empty() {
                break;
            }
            for ev in batch {
                if ev.seq > head {
                    return Ok(());
                }
                cursor = ev.seq;
                self.send_event(subscription_id, &ev, include_tool_output).await;
            }
        }
        Ok(())
    }

    async fn send_event(&self, subscription_id: SubscriptionId, event: &ThreadEvent, include_tool_output: bool) {
        if !include_tool_output && let Some(compact) = compact_tool_completion(event) {
            let params = BorrowedEventNotification { subscription_id, event: &compact };
            let _ = self.out.notify(EVENT_NOTIFICATION, params).await;
            return;
        }
        if !include_tool_output && let Some(marked) = mark_stream_recoverable(event) {
            let params = BorrowedEventNotification { subscription_id, event: &marked };
            let _ = self.out.notify(EVENT_NOTIFICATION, params).await;
            return;
        }
        let params = BorrowedEventNotification { subscription_id, event };
        let _ = self.out.notify(EVENT_NOTIFICATION, params).await;
    }

    async fn deliver_live(&self, ev: &ThreadEvent) {
        let _delivery = self.delivery.lock().await;
        let targets: Vec<(SubscriptionId, bool)> = {
            let subs = self.subs.lock().await;
            subs.iter()
                .filter(|(_, s)| s.thread_id.is_none_or(|t| t == ev.thread_id) && ev.seq > s.floor_seq)
                .map(|(id, s)| (*id, s.include_tool_output))
                .collect()
        };
        for (id, include_tool_output) in targets {
            self.send_event(id, ev, include_tool_output).await;
        }
    }
}

/// Counts one open connection for the idle-exit decision; released on drop so
/// every exit path, including panics, is covered.
struct ConnectionSlot(AppState);

impl ConnectionSlot {
    fn claim(state: &AppState) -> Self {
        state.connections.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // A client arriving ends any idle stretch at once, without waiting for a sweep.
        *state.idle_since.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        Self(state.clone())
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

async fn run(state: AppState, socket: WebSocket, principal: Principal) {
    let _slot = ConnectionSlot::claim(&state);
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<QueuedFrame>(64);
    let closed = tokio_util::sync::CancellationToken::new();
    let requests = Arc::new(tokio::sync::Semaphore::new(16));
    // Terminal keystrokes are one ordered stream per connection: a task per request would let two writes pass each other
    // (and a burst would hit the concurrency limit), so they queue here and run one at a time.
    let (input_tx, mut input_rx) = mpsc::channel::<String>(INPUT_LANE_FRAMES);
    let ctx = Arc::new(ConnectionCtx {
        id: Uuid::now_v7(),
        principal,
        delivery: Mutex::new(()),
        subs: Mutex::new(HashMap::new()),
        terminal_subs: Mutex::new(HashMap::new()),
        out: Outbox { sender: out_tx, budget: Arc::new(tokio::sync::Semaphore::new(OUTBOX_BYTES)), closed: closed.clone() },
    });
    let mut live = state.events.subscribe();
    let mut revoked = state.revoked_tokens.subscribe();
    // Notes are orchestration state: only clients that may read it hear about changes.
    let mut notes = ctx.principal.has(Scope::OrchestrationRead).then(|| state.orchestrator.subscribe_notes());
    let mut tasks = ctx.principal.has(Scope::OrchestrationRead).then(|| state.orchestrator.subscribe_tasks());
    let mut usage = ctx.principal.has(Scope::OrchestrationRead).then(|| state.orchestrator.usage().subscribe());
    let mut settings = ctx.principal.has(Scope::OrchestrationRead).then(|| state.settings.subscribe());
    let mut account_logins = ctx.principal.has(Scope::OrchestrationRead).then(|| state.account_logins.subscribe());
    let mut session_imports = ctx.principal.has(Scope::OrchestrationRead).then(|| state.session_imports.subscribe());
    let mut projects = ctx.principal.has(Scope::OrchestrationRead).then(|| state.orchestrator.subscribe_projects());
    let mut outbox = ctx.principal.has(Scope::OrchestrationRead).then(|| state.network.subscribe_outbox());
    let mut utility = ctx.principal.has(Scope::OrchestrationRead).then(|| state.network.subscribe_utility());
    let mut workspace_progress = ctx.principal.has(Scope::OrchestrationRead).then(|| state.orchestrator.subscribe_workspace_progress());
    let mut spaces = ctx.principal.has(Scope::OrchestrationRead).then(|| state.orchestrator.subscribe_spaces());
    let mut preview_requests = ctx.principal.has(Scope::OrchestrationRead).then(|| state.orchestrator.subscribe_preview_requests());
    let mut request_progress = ctx.principal.has(Scope::OrchestrationRead).then(|| state.orchestrator.requests().subscribe());
    if !principal_is_active(&state, &ctx.principal) {
        return;
    }
    tracing::info!(conn = %ctx.id, label = %ctx.principal.label, "client connected");

    let input_state = state.clone();
    let input_ctx = ctx.clone();
    let input_lane = tokio::spawn(async move {
        while let Some(text) = input_rx.recv().await {
            handle_text(&input_state, &input_ctx, &text).await;
        }
    });

    let writer_closed = closed.clone();
    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if sink.send(Message::Text(frame.text.into())).await.is_err() {
                break;
            }
        }
        writer_closed.cancel();
        let _ = sink.close().await;
    });

    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            _ = closed.cancelled() => break,
            notice = revoked.recv() => {
                if matches!(notice, Ok(id) if id == ctx.principal.token_id)
                    || !principal_is_active(&state, &ctx.principal)
                { break; }
            }
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if is_terminal_input(text.as_str()) {
                            // Waiting here is the backpressure: the socket is read no faster than the PTY takes input.
                            let _ = input_tx.send(text.as_str().to_string()).await;
                            continue;
                        }
                        let state = state.clone();
                        let ctx = ctx.clone();
                        // Bound producers as well as queued bytes. Do not make a
                        // saturated RPC lane block event/approval delivery.
                        if let Ok(permit) = requests.clone().try_acquire_owned() {
                            tokio::spawn(async move {
                                let _permit = permit;
                                handle_text(&state, &ctx, text.as_str()).await;
                            });
                        } else if let Ok(ClientFrame::Request(request)) = serde_json::from_str(text.as_str()) {
                            let _ = ctx.out.send(ServerFrame::Response(RpcResponse::err(request.id,
                                RpcError::internal("Too many concurrent requests; retry after pending requests finish")))).await;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => { tracing::debug!(%e, "ws read error"); break; }
                }
            }
            note = async {
                match notes.as_mut() {
                    Some(notes) => notes.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match note {
                    Ok(changed) => { let _ = ctx.out.notify(kybern_protocol::methods::NOTES_CHANGED_NOTIFICATION, changed).await; }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(conn = %ctx.id, lagged = n, "client missed note changes");
                    }
                    Err(_) => notes = None,
                }
            }
            task = async {
                match tasks.as_mut() {
                    Some(tasks) => tasks.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match task {
                    Ok(changed) => { let _ = ctx.out.notify(kybern_protocol::methods::TASK_ITEMS_CHANGED_NOTIFICATION, changed).await; }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(conn = %ctx.id, lagged = n, "client missed task changes");
                    }
                    Err(_) => tasks = None,
                }
            }
            changed = async {
                match projects.as_mut() {
                    Some(projects) => projects.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match changed {
                    Ok(changed) => { let _ = ctx.out.notify(kybern_protocol::methods::PROJECTS_CHANGED_NOTIFICATION, changed).await; }
                    // Each notification carries the whole list; the next one catches up.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => projects = None,
                }
            }
            changed = async {
                match outbox.as_mut() {
                    Some(outbox) => outbox.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match changed {
                    Ok(item) => { let _ = ctx.out.notify(kybern_protocol::OUTBOX_CHANGED_NOTIFICATION, item).await; }
                    // A missed item shows the next time the list is read.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => outbox = None,
                }
            }
            event = async {
                match utility.as_mut() {
                    Some(utility) => utility.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match event {
                    Ok((method, params)) => { let _ = ctx.out.notify(method, params).await; }
                    // Progress repeats and lists carry everything; a missed one is covered by the next.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => utility = None,
                }
            }
            progress = async {
                match workspace_progress.as_mut() {
                    Some(progress) => progress.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match progress {
                    Ok(progress) => { let _ = ctx.out.notify(kybern_protocol::methods::THREADS_WORKSPACE_PROGRESS_NOTIFICATION, progress).await; }
                    // Progress is transient; the create's result settles it.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => workspace_progress = None,
                }
            }
            changed = async {
                match spaces.as_mut() {
                    Some(spaces) => spaces.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match changed {
                    Ok(changed) => { let _ = ctx.out.notify(kybern_protocol::methods::SPACES_CHANGED_NOTIFICATION, changed).await; }
                    // Each notification carries the whole list; the next one catches up.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => spaces = None,
                }
            }
            request = async {
                match preview_requests.as_mut() {
                    Some(requests) => requests.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match request {
                    Ok(request) => { let _ = ctx.out.notify(kybern_protocol::methods::PREVIEW_OPEN_REQUESTED_NOTIFICATION, request).await; }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => preview_requests = None,
                }
            }
            updated = async {
                match settings.as_mut() {
                    Some(settings) => settings.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match updated {
                    Ok(updated) => { state.orchestrator.usage().settings_changed(); let _ = ctx.out.notify("settings.changed", serde_json::json!({ "settings": updated })).await; }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => { let _ = ctx.out.notify("settings.changed", serde_json::json!({ "settings": state.settings.get() })).await; }
                    Err(_) => settings = None,
                }
            }
            progress = async {
                match request_progress.as_mut() {
                    Some(progress) => progress.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match progress {
                    Ok(progress) => {
                        // A paired phone sees a card's step, never the sign-in page, code or terminal.
                        let progress = if ctx.principal.has(Scope::AccessWrite) { progress } else { redact_progress(progress) };
                        let _ = ctx.out.notify(kybern_protocol::methods::REQUESTS_PROGRESS_NOTIFICATION, progress).await;
                    }
                    // Clients also read `requests.progress`, so a missed step is recovered.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => request_progress = None,
                }
            }
            login = async {
                match account_logins.as_mut() {
                    Some(logins) => logins.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match login {
                    Ok(login) => {
                        // A paired phone sees progress, never the sign-in page or code.
                        let login = if ctx.principal.has(Scope::AccessWrite) { login } else { crate::account_login::for_observer(login) };
                        let _ = ctx.out.notify(kybern_protocol::methods::ACCOUNTS_LOGIN_CHANGED_NOTIFICATION, login).await;
                    }
                    // Clients also poll `providers.accounts.login.get`, so a missed phase is recovered.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => account_logins = None,
                }
            }
            update = async {
                match session_imports.as_mut() {
                    Some(updates) => updates.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                use crate::session_import::SessionsEvent;
                use kybern_protocol::methods::*;
                match update {
                    Ok(SessionsEvent::Scan(scan)) => { let _ = ctx.out.notify(SESSIONS_SCAN_CHANGED_NOTIFICATION, scan).await; }
                    Ok(SessionsEvent::Progress(progress)) => { let _ = ctx.out.notify(SESSIONS_IMPORT_PROGRESS_NOTIFICATION, progress).await; }
                    Ok(SessionsEvent::Finished(finished)) => { let _ = ctx.out.notify(SESSIONS_IMPORT_FINISHED_NOTIFICATION, finished).await; }
                    // Clients also read `sessions.scan_get` / `sessions.import_get`, so a missed step is recovered.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => session_imports = None,
                }
            }
            limits = async {
                match usage.as_mut() {
                    Some(usage) => usage.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match limits {
                    Ok(changed) => { let _ = ctx.out.notify(kybern_protocol::methods::USAGE_LIMITS_CHANGED_NOTIFICATION, changed).await; }
                    // Each notification carries the whole cache; the next one catches up.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => usage = None,
                }
            }
            ev = live.recv() => {
                match ev {
                    Ok(ev) => ctx.deliver_live(&ev).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(conn = %ctx.id, lagged = n, "client fell behind; asking it to resubscribe");
                        let _ = ctx.out.send(ServerFrame::Notification(RpcNotification::new("events.lagged", serde_json::json!({ "dropped": n })))).await;
                    }
                    Err(_) => break,
                }
            }
        }
    }
    for (_, h) in ctx.terminal_subs.lock().await.drain() {
        h.abort();
    }
    closed.cancel();
    drop(ctx);
    writer.abort();
    input_lane.abort();
    tracing::info!("client disconnected");
}

/// A frame sink over a connection's outbox and a data lane: what a [`PeerLink`] writes to on the accepting side. Control
/// frames keep the outbox's 30 s limit (a socket that takes nothing for that long is broken); stream data waits in the
/// lane for as long as the route needs.
struct OutboxSink {
    out: Outbox,
    bulk: peers::BulkLane,
}

#[async_trait::async_trait]
impl FrameSink for OutboxSink {
    async fn send(&self, frame: Value) -> bool {
        self.out.send(frame).await.is_ok()
    }

    async fn send_bulk(&self, frame: peers::Outgoing) -> bool {
        self.bulk.send(frame).await
    }
}

/// The message for a member's socket of a frame [`peers::next_frame`] picked, and the budget to release once it is sent.
fn machine_message(frame: Result<QueuedFrame, peers::BulkFrame>) -> (Message, Option<tokio::sync::OwnedSemaphorePermit>) {
    match frame {
        Ok(QueuedFrame { text, _permit }) => (Message::Text(text.into()), Some(_permit)),
        Err(data) => {
            let (message, held) = data.into_parts();
            let message = match message {
                peers::Outgoing::Text(text) => Message::Text(text.into()),
                peers::Outgoing::Binary(bytes) => Message::Binary(bytes.into()),
            };
            (message, Some(held))
        }
    }
}

/// Serve a session socket from another member (design §3.3): one JSON-RPC link used both ways. It
/// is not a client connection: it takes no subscriptions and does not count as activity, so a quiet
/// daemon can still exit while peers hold sessions to it.
async fn run_machine(state: AppState, socket: WebSocket, principal: Principal) {
    let PrincipalKind::Machine { machine_id, .. } = principal.kind.clone() else { return };
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<QueuedFrame>(64);
    let (ping_tx, mut ping_rx) = mpsc::channel::<()>(1);
    let closed = tokio_util::sync::CancellationToken::new();
    let out = Outbox { sender: out_tx, budget: Arc::new(tokio::sync::Semaphore::new(OUTBOX_BYTES)), closed: closed.clone() };
    let (bulk, mut bulk_rx) = peers::BulkLane::new(closed.clone());
    let link = PeerLink::new(
        machine_id,
        Direction::Accepted,
        Some(principal.token_id),
        principal.scopes.clone(),
        Arc::new(OutboxSink { out, bulk }),
    );
    let writer_link = link.clone();
    let writer_closed = closed.clone();
    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = writer_link.closed() => break,
                _ = writer_closed.cancelled() => break,
                ping = ping_rx.recv() => match ping {
                    Some(()) => if sink.send(Message::Ping(Vec::new().into())).await.is_err() { break },
                    None => break,
                },
                // Control frames first, then stream data.
                frame = peers::next_frame(&mut out_rx, &mut bulk_rx) => match frame {
                    Some(frame) => {
                        let (message, _held) = machine_message(frame);
                        if sink.send(message).await.is_err() { break }
                    }
                    None => break,
                },
            }
        }
        writer_link.close();
        writer_closed.cancel();
        let _ = sink.close().await;
    });
    let timing = state.network.timing.clone();
    let mut revoked = state.revoked_tokens.subscribe();
    let mut beat = tokio::time::interval(timing.ping_every);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    peers::accept(&state, link.clone());
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            _ = link.closed() => break,
            _ = closed.cancelled() => break,
            notice = revoked.recv() => {
                if matches!(notice, Ok(id) if id == principal.token_id) { break; }
            }
            _ = beat.tick() => {
                // A machine that was removed is told so on this socket, which then closes by itself: the removal's own
                // push may not have reached it, and no member opens another session with it.
                let removed = !state.network.membership().is_some_and(|membership| membership.is_member(&link.machine_id));
                if removed || link.saying_farewell() {
                    state.network.say_farewell(link.clone());
                    continue;
                }
                // Three missed pings, or a grant, membership or policy that no longer allows the session.
                if link.idle_for() > timing.dead_after || !principal_is_active(&state, &principal) {
                    tracing::info!(target: "kybern::network::session", machine = %link.machine_id, "closing a session that went quiet or lost its grant");
                    break;
                }
                let _ = ping_tx.try_send(());
            }
            frame = stream.next() => match frame {
                Some(Ok(Message::Text(text))) => peers::inbound(&state, &link, text.as_str()).await,
                Some(Ok(Message::Binary(frame))) => peers::inbound_binary(&state, &link, &frame),
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => link.touch(),
                Some(Err(error)) => { tracing::debug!(%error, "peer socket read error"); break; }
            }
        }
    }
    closed.cancel();
    peers::accepted_ended(&state, &link);
    writer.abort();
}

/// What a client without `access:write` may see of a card's progress.
pub(crate) fn redact_progress(mut progress: kybern_protocol::RequestProgress) -> kybern_protocol::RequestProgress {
    progress.url = None;
    progress.user_code = None;
    progress.terminal = None;
    progress
}

/// Frames the ordered input lane carries: how many may wait for the PTY.
const INPUT_LANE_FRAMES: usize = 1024;

/// Whether a text frame is a `terminals.input` request (cheap substring test first; the parse only runs for candidates).
fn is_terminal_input(text: &str) -> bool {
    text.contains("terminals.input")
        && serde_json::from_str::<Value>(text).is_ok_and(|v| v.get("method").and_then(Value::as_str) == Some("terminals.input"))
}

async fn handle_text(state: &AppState, ctx: &Arc<ConnectionCtx>, text: &str) {
    let frame: ClientFrame = match serde_json::from_str(text) {
        Ok(f) => f,
        Err(e) => {
            let resp = RpcResponse::err(RpcId::Number(0), RpcError::new(codes::PARSE_ERROR, e.to_string()));
            let _ = ctx.out.send(ServerFrame::Response(resp)).await;
            return;
        }
    };
    let req = match frame {
        ClientFrame::Request(r) => r,
        ClientFrame::Notification(_) => return,
    };
    if !principal_is_active(state, &ctx.principal) {
        let _ = ctx
            .out
            .send(ServerFrame::Response(RpcResponse::err(req.id, RpcError::new(codes::UNAUTHORIZED, "Device access was revoked"))))
            .await;
        return;
    }
    if req.method == "events.subscribe" && ctx.principal.has(Scope::OrchestrationRead) {
        ctx.establish_subscription(state, req.id, req.params).await;
        return;
    }
    let result = match scope_for(&req.method) {
        None => Err(RpcError::method_not_found(&req.method)),
        Some(Some(scope)) if !ctx.principal.has(scope) || !effective_scopes(state, &ctx.principal).contains(&scope) => {
            Err(RpcError::forbidden(scope.as_str()))
        }
        Some(_) => crate::rpc::dispatch(state, ctx, &req.method, req.params).await,
    };
    let resp = match result {
        Ok(v) => RpcResponse::ok(req.id, v),
        Err(e) => RpcResponse::err(req.id, e),
    };
    let _ = ctx.out.send(ServerFrame::Response(resp)).await;
}

#[cfg(test)]
mod memory_tests {
    use super::*;
    #[tokio::test]
    async fn oversized_frame_holds_budget_until_writer_releases_it_and_cancel_wakes_waiters() {
        let (sender, mut receiver) = mpsc::channel(64);
        let out = Outbox {
            sender,
            budget: Arc::new(tokio::sync::Semaphore::new(OUTBOX_BYTES)),
            closed: tokio_util::sync::CancellationToken::new(),
        };
        let frame = || ServerFrame::Notification(RpcNotification::new("test", serde_json::json!({"data": "x".repeat(OUTBOX_BYTES + 1)})));
        out.send(frame()).await.unwrap();
        assert_eq!(out.budget.available_permits(), 0);
        let received = receiver.recv().await.unwrap();
        assert!(received.text.len() > OUTBOX_BYTES);
        assert_eq!(out.budget.available_permits(), 0);
        let waiting = {
            let out = out.clone();
            tokio::spawn(async move { out.send(ServerFrame::Notification(RpcNotification::new("next", Value::Null))).await })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        drop(received);
        waiting.await.unwrap().unwrap();
        drop(receiver.recv().await.unwrap());
        assert_eq!(out.budget.available_permits(), OUTBOX_BYTES);
        out.send(frame()).await.unwrap();
        let waiting = {
            let out = out.clone();
            tokio::spawn(async move { out.send(ServerFrame::Notification(RpcNotification::new("next", Value::Null))).await })
        };
        out.closed.cancel();
        assert!(waiting.await.unwrap().is_err());
    }

    /// A member's session over a slow route (256 KiB/s) while twelve streams each push 4 MiB in 1 MiB messages: 48 MiB,
    /// about three minutes of sending. With data in the outbox, the 30 s send limit ended the session; now the streams
    /// wait for room, the session stays up, every byte arrives, and an answer queued meanwhile goes out on the next write.
    #[tokio::test(start_paused = true)]
    async fn a_slow_route_backs_stream_data_up_without_ending_the_session_and_answers_go_first() {
        use peers::Outgoing;
        use std::time::Duration;
        const RATE: f64 = 256.0 * 1024.0;
        const STREAMS: usize = 12;
        const MESSAGES: usize = 4;
        const MESSAGE: usize = 1024 * 1024;
        let closed = tokio_util::sync::CancellationToken::new();
        let (sender, mut out_rx) = mpsc::channel(64);
        let out = Outbox { sender, budget: Arc::new(tokio::sync::Semaphore::new(OUTBOX_BYTES)), closed: closed.clone() };
        let (bulk, mut bulk_rx) = peers::BulkLane::new(closed.clone());
        let sink = Arc::new(OutboxSink { out, bulk });
        let (seen_tx, mut seen_rx) = mpsc::unbounded_channel::<(String, tokio::time::Instant)>();
        let writer = tokio::spawn(async move {
            let mut data = 0usize;
            while let Some(frame) = peers::next_frame(&mut out_rx, &mut bulk_rx).await {
                let (message, _held) = machine_message(frame);
                match message {
                    Message::Binary(bytes) => {
                        tokio::time::sleep(Duration::from_secs_f64(bytes.len() as f64 / RATE)).await;
                        data += bytes.len();
                        if data == STREAMS * MESSAGES * MESSAGE {
                            return data;
                        }
                    }
                    Message::Text(text) => {
                        let _ = seen_tx.send((text.to_string(), tokio::time::Instant::now()));
                    }
                    _ => {}
                }
            }
            data
        });
        let streams: Vec<_> = (0..STREAMS)
            .map(|stream| {
                let sink = sink.clone();
                tokio::spawn(async move {
                    for _ in 0..MESSAGES {
                        if !sink.send_bulk(Outgoing::Binary(vec![stream as u8; MESSAGE])).await {
                            return false;
                        }
                    }
                    true
                })
            })
            .collect();
        tokio::time::sleep(Duration::from_secs(40)).await;
        let asked = tokio::time::Instant::now();
        assert!(sink.send(serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {}})).await, "an answer is queued at once");
        let (answer, sent) = seen_rx.recv().await.unwrap();
        assert!(answer.contains("\"id\":1"));
        assert!(sent - asked <= Duration::from_secs_f64(MESSAGE as f64 / RATE + 0.5), "the answer waited behind data: {:?}", sent - asked);
        for stream in streams {
            assert!(stream.await.unwrap(), "every stream completes");
        }
        assert_eq!(writer.await.unwrap(), STREAMS * MESSAGES * MESSAGE);
        assert!(!closed.is_cancelled(), "the session survives the slow route");
    }
}

#[cfg(test)]
mod typed_notification_memory_tests {
    use super::*;

    fn event(output: Value) -> ThreadEvent {
        ThreadEvent {
            seq: 17,
            thread_id: ThreadId::nil(),
            turn_id: Some(TurnId::nil()),
            at: "2026-09-17T00:00:00Z".parse().unwrap(),
            payload: EventPayload::ToolCallCompleted {
                tool_call_id: "a:b\n\0é".into(),
                output,
                output_omitted: false,
                stream_recoverable: false,
                is_error: false,
            },
        }
    }

    fn assert_typed_wire<T: serde::Serialize>(method: &'static str, params: T) {
        let old = ServerFrame::Notification(RpcNotification::new(method, serde_json::to_value(&params).unwrap()));
        let new = TypedNotification { jsonrpc: JsonRpcVersion, method, params };
        assert_eq!(serde_json::to_string(&new).unwrap(), serde_json::to_string(&old).unwrap());
    }

    #[test]
    fn borrowed_event_matches_owned_notification_wire_bytes() {
        for output in
            [Value::Null, serde_json::json!([null, true, -3, 0.125, "😀\n\"\\\0"]), serde_json::json!({ "long": "x".repeat(1024 * 1024) })]
        {
            let event = event(output);
            for turn_id in [None, Some(TurnId::nil())] {
                let event = ThreadEvent { turn_id, ..event.clone() };
                let params = BorrowedEventNotification { subscription_id: SubscriptionId::nil(), event: &event };
                let old = ServerFrame::Notification(RpcNotification::new(
                    EVENT_NOTIFICATION,
                    serde_json::to_value(EventNotification { subscription_id: SubscriptionId::nil(), event: event.clone() }).unwrap(),
                ));
                let new = TypedNotification { jsonrpc: JsonRpcVersion, method: EVENT_NOTIFICATION, params };
                let wire = serde_json::to_string(&new).unwrap();
                assert_eq!(wire, serde_json::to_string(&old).unwrap());
                assert!(matches!(serde_json::from_str::<ServerFrame>(&wire).unwrap(), ServerFrame::Notification(_)));
            }
        }
    }

    #[test]
    fn compact_subscription_event_replaces_only_large_completion_payload() {
        let original = event(serde_json::json!({ "long": "x".repeat(kybern_store::LARGE_TOOL_OUTPUT_BYTES + 1) }));
        let compact = compact_tool_completion(&original).expect("large completion should be compacted");
        let EventPayload::ToolCallCompleted { output, output_omitted, stream_recoverable, tool_call_id, is_error } = &compact.payload
        else {
            panic!("expected tool completion")
        };
        assert_eq!(tool_call_id, "a:b\n\0é");
        assert_eq!(output, &Value::Null);
        assert!(*output_omitted);
        assert!(*stream_recoverable);
        assert!(!is_error);
        assert!(serde_json::to_string(&compact).unwrap().len() < 512);

        let small = event(serde_json::json!("ok"));
        assert!(compact_tool_completion(&small).is_none());
        let marked = mark_stream_recoverable(&small).expect("small completion remains stream-recoverable");
        assert!(matches!(marked.payload, EventPayload::ToolCallCompleted { stream_recoverable: true, .. }));
        let full_wire = serde_json::to_value(&small.payload).unwrap();
        assert!(full_wire.get("output_omitted").is_none(), "legacy full wire stays unchanged");
    }

    #[test]
    fn typed_ready_and_terminal_notifications_preserve_wire_fields() {
        use kybern_protocol::methods::*;
        assert_typed_wire(EVENTS_READY_NOTIFICATION, EventsReadyNotification { subscription_id: SubscriptionId::nil(), head_seq: 123 });
        assert_typed_wire(
            TERMINAL_OUTPUT_NOTIFICATION,
            TerminalOutputNotification { terminal_id: TerminalId::nil(), data: "AAECAw==".into() },
        );
        assert_typed_wire(TERMINAL_EXITED_NOTIFICATION, TerminalExitedNotification { terminal_id: TerminalId::nil(), exit_code: None });
        assert_typed_wire(TERMINAL_EXITED_NOTIFICATION, TerminalExitedNotification { terminal_id: TerminalId::nil(), exit_code: Some(2) });
    }

    #[tokio::test]
    async fn queued_wire_owns_data_after_borrowed_source_drops() {
        let (sender, mut receiver) = mpsc::channel(1);
        let out = Outbox {
            sender,
            budget: Arc::new(tokio::sync::Semaphore::new(OUTBOX_BYTES)),
            closed: tokio_util::sync::CancellationToken::new(),
        };
        {
            let event = event(serde_json::json!({ "preserved": "😀\n" }));
            out.notify(EVENT_NOTIFICATION, BorrowedEventNotification { subscription_id: SubscriptionId::nil(), event: &event })
                .await
                .unwrap();
        }
        let queued = receiver.recv().await.unwrap();
        let wire: Value = serde_json::from_str(&queued.text).unwrap();
        assert_eq!(wire["params"]["event"]["output"]["preserved"], "😀\n");
        assert_eq!(out.budget.available_permits(), OUTBOX_BYTES - queued.text.len());
        drop(queued);
        assert_eq!(out.budget.available_permits(), OUTBOX_BYTES);
    }

    #[tokio::test]
    async fn typed_large_frames_obey_existing_budget_and_cancellation() {
        use kybern_protocol::methods::*;
        let (sender, mut receiver) = mpsc::channel(1);
        let out = Outbox {
            sender,
            budget: Arc::new(tokio::sync::Semaphore::new(OUTBOX_BYTES)),
            closed: tokio_util::sync::CancellationToken::new(),
        };
        out.notify(
            TERMINAL_OUTPUT_NOTIFICATION,
            TerminalOutputNotification { terminal_id: TerminalId::nil(), data: "x".repeat(OUTBOX_BYTES + 1) },
        )
        .await
        .unwrap();
        let queued = receiver.recv().await.unwrap();
        assert!(queued.text.len() > OUTBOX_BYTES);
        assert_eq!(out.budget.available_permits(), 0);
        let waiting = {
            let out = out.clone();
            tokio::spawn(async move {
                out.notify(TERMINAL_OUTPUT_NOTIFICATION, TerminalOutputNotification { terminal_id: TerminalId::nil(), data: "next".into() })
                    .await
            })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        out.closed.cancel();
        assert!(waiting.await.unwrap().is_err());
        drop(queued);
        assert_eq!(out.budget.available_permits(), OUTBOX_BYTES);
        assert!(receiver.try_recv().is_err());
    }
}
