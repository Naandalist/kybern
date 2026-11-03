//! The outbox worker and the way delegation reaches other machines (design §6.4).
//!
//! * [`NetworkHub`] is the [`RemoteHub`] the orchestrator uses: it names members, tells whether they are
//!   online, and sends a `peer.*` request over the open session.
//! * [`run`] delivers queued items. It wakes when a session opens or membership changes, when an item is
//!   queued or retried, and on a 60 s safety timer while items are due. Each delivery carries the item's
//!   `operation_id`, which the target deduplicates, and items for one target and kind go one at a time in
//!   the order they were queued. Retries back off 2 s → 5 min. An item expires after 7 days; the user
//!   sees it as expired and can retry it. A target that was removed fails its items at once.
//! * [`list`], [`retry`], [`cancel`] and [`enqueue`] are the `outbox.*` methods.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use kybern_protocol::methods::{Method, PeerThreadCreate, PeerThreadSend, ThreadsCreateParams, ThreadsSendParams};
use kybern_protocol::*;
use serde_json::Value;
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{Network, TARGET};
use crate::orchestrator::remote_delegation::{CallFailure, Delivery, MemberView, RemoteHub, RemoteTarget};
use crate::state::AppState;

/// While items are queued the worker looks again at least this often.
const SAFETY_TICK: Duration = Duration::from_secs(60);
/// With nothing queued the worker only wakes on a signal.
const IDLE_TICK: Duration = Duration::from_secs(60 * 60);

// ---- the hub ----

pub struct NetworkHub {
    network: Arc<Network>,
    shutdown: CancellationToken,
    kick: Notify,
}

impl NetworkHub {
    pub fn new(network: Arc<Network>, shutdown: CancellationToken) -> Arc<Self> {
        Arc::new(Self { network, shutdown, kick: Notify::new() })
    }

    fn target_of(&self, id: &str) -> Option<RemoteTarget> {
        let membership = self.network.membership()?;
        if id == membership.machine_id() {
            return None;
        }
        let projection = membership.projection();
        let entry = projection.entry(id).filter(|entry| entry.removed.is_none())?;
        let status = self.network.peers.status(id);
        Some(RemoteTarget {
            id: id.to_string(),
            name: entry.descriptor.name.clone(),
            // Online only with a session to deliver on: the peer status can still read online for a
            // moment after the link closed, and an attempt then would claim the item for nothing.
            online: status.online && self.network.peers.link(id).is_some(),
            paused: entry.paused,
            features: status.presence.map(|presence| presence.features),
        })
    }
}

#[async_trait]
impl RemoteHub for NetworkHub {
    fn self_id(&self) -> String {
        self.network.machine_id().unwrap_or_default()
    }

    fn self_name(&self) -> String {
        let Some(membership) = self.network.membership() else { return crate::rpc::hostname() };
        let projection = membership.projection();
        projection.entry(&membership.machine_id()).map(|entry| entry.descriptor.name.clone()).unwrap_or_else(crate::rpc::hostname)
    }

    fn resolve(&self, query: &str) -> Result<RemoteTarget, String> {
        let Some(membership) = self.network.membership() else {
            return Err("The machine network is not available on this machine.".into());
        };
        let me = membership.machine_id();
        let projection = membership.projection();
        let query = query.trim();
        let members: Vec<(String, String)> = projection
            .members()
            .filter(|(_, entry)| entry.removed.is_none())
            .map(|(id, entry)| (id.clone(), entry.descriptor.name.clone()))
            .collect();
        let named: Vec<&(String, String)> = members.iter().filter(|(id, name)| id == query || name.eq_ignore_ascii_case(query)).collect();
        match named.as_slice() {
            [(id, _)] if *id == me => Err("That is this machine. Leave out `machine` to delegate here.".into()),
            [(id, _)] => self.target_of(id).ok_or_else(|| format!("{query} is not in your network any more.")),
            [] => {
                let others: Vec<&str> = members.iter().filter(|(id, _)| *id != me).map(|(_, name)| name.as_str()).collect();
                if others.is_empty() {
                    Err(format!("There is no machine called \"{query}\": this machine is not in a network with others."))
                } else {
                    Err(format!("There is no machine called \"{query}\" in your network. Machines: {}.", others.join(", ")))
                }
            }
            _ => Err(format!("More than one machine is called \"{query}\". Use the machine id from kybern_agent_capabilities.")),
        }
    }

    fn target(&self, id: &str) -> Option<RemoteTarget> {
        self.target_of(id)
    }

    fn members(&self) -> Vec<MemberView> {
        let Some(membership) = self.network.membership() else { return Vec::new() };
        let me = membership.machine_id();
        let projection = membership.projection();
        projection
            .members()
            .filter(|(_, entry)| entry.removed.is_none())
            .map(|(id, entry)| {
                let is_self = *id == me;
                let status = self.network.peers.status(id);
                let presence = status.presence;
                MemberView {
                    id: id.clone(),
                    name: entry.descriptor.name.clone(),
                    os: entry.descriptor.os.clone(),
                    is_self,
                    status: if entry.paused {
                        "paused"
                    } else if is_self || status.online {
                        "online"
                    } else {
                        "offline"
                    },
                    last_seen: status.last_seen,
                    version: is_self
                        .then(|| env!("CARGO_PKG_VERSION").to_string())
                        .or_else(|| presence.as_ref().map(|p| p.version.clone())),
                    features: if is_self {
                        self.network.features()
                    } else {
                        presence.as_ref().map(|p| p.features.clone()).unwrap_or_default()
                    },
                    capabilities: super::forward::capability_view(&projection.allowed(id)),
                    harnesses: presence.map(|p| p.harnesses).unwrap_or_default(),
                }
            })
            .collect()
    }

    fn targets(&self) -> Vec<RemoteTarget> {
        let Some(membership) = self.network.membership() else { return Vec::new() };
        let ids: Vec<String> =
            membership.projection().members().filter(|(_, entry)| entry.removed.is_none()).map(|(id, _)| id.clone()).collect();
        ids.iter().filter_map(|id| self.target_of(id)).collect()
    }

    async fn call(&self, target: &str, method: &str, params: Value) -> Result<Value, CallFailure> {
        let Some(info) = self.target_of(target) else {
            return Err(CallFailure::Gone("That machine is not in your network any more.".into()));
        };
        if info.paused {
            return Err(CallFailure::Offline(format!("{} is paused", info.name)));
        }
        let Some(link) = self.network.peers.link(target) else {
            return Err(CallFailure::Offline(format!("{} is offline", info.name)));
        };
        match link.call_raw(method, params).await {
            Ok(value) => Ok(value),
            Err(error) if is_transport_error(&error) => Err(CallFailure::Offline(format!("{}: {}", info.name, error.message))),
            Err(error) => Err(CallFailure::Remote(error)),
        }
    }

    fn changes(&self) -> broadcast::Receiver<()> {
        self.network.subscribe_changes()
    }

    fn kick(&self) {
        self.kick.notify_one();
    }

    async fn kicked(&self) {
        self.kick.notified().await;
    }

    fn shutdown(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    async fn forward_port(&self, target: &str, remote_port: u16) -> Result<u16, String> {
        let info = self.target_of(target).ok_or_else(|| "That machine is not in your network any more.".to_string())?;
        super::ports::forward_to(&self.network, &self.shutdown, target, &info.name, remote_port, None, false, PortForwardOrigin::Preview)
            .await
            .map(|forward| forward.local_port)
            .map_err(|error| error.message)
    }
}

/// The session broke or never answered, as opposed to the peer saying no.
fn is_transport_error(error: &RpcError) -> bool {
    error.code == codes::INTERNAL_ERROR
        && (error.message.starts_with("the session")
            || error.message.contains("did not answer")
            || error.message.contains("too many concurrent requests"))
}

// ---- the worker ----

type InFlight = Arc<Mutex<HashSet<(String, OutboxKind)>>>;

/// Deliver queued items until the daemon shuts down.
pub async fn run(state: AppState) {
    let Some(hub) = state.orchestrator.remote_hub().cloned() else { return };
    match state.store.outbox_requeue_sending() {
        Ok(0) => {}
        Ok(count) => tracing::info!(target: TARGET, count, "queued deliveries that a restart interrupted"),
        Err(error) => tracing::warn!(target: TARGET, %error, "could not requeue interrupted deliveries"),
    }
    let in_flight: InFlight = Default::default();
    let mut changes = hub.changes();
    loop {
        pass(&state, &hub, &in_flight).await;
        let wait = next_wait(&state);
        tokio::select! {
            _ = state.shutdown.cancelled() => return,
            _ = hub.kicked() => {}
            result = changes.recv() => {
                if matches!(result, Err(broadcast::error::RecvError::Closed)) {
                    changes = hub.changes();
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

/// How long to sleep: until the soonest item is due, at most the safety tick; long when nothing is queued.
fn next_wait(state: &AppState) -> Duration {
    match state.store.outbox_next_due() {
        Ok(Some(due)) => (due - Utc::now()).to_std().unwrap_or(Duration::ZERO).clamp(Duration::from_millis(25), SAFETY_TICK),
        _ => IDLE_TICK,
    }
}

fn announce(state: &AppState, item: &OutboxItem) {
    state.network.notify_outbox(item.clone());
}

/// One look at the outbox: expire what ran out, fail what can never be delivered, start what is due.
async fn pass(state: &AppState, hub: &Arc<dyn RemoteHub>, in_flight: &InFlight) {
    let now = Utc::now();
    state.orchestrator.move_sweep();
    match state.store.outbox_overdue(now) {
        Ok(overdue) => {
            for mut item in overdue {
                let name = hub.target(&item.target).map_or_else(|| "The machine".to_string(), |target| target.name);
                item.state = OutboxState::Expired;
                item.last_error = Some(format!("{name} did not come back within {OUTBOX_EXPIRY_DAYS} days."));
                item.updated_at = now;
                if state.store.outbox_update(&item).unwrap_or(false) {
                    settle(state, &item);
                }
            }
        }
        Err(error) => tracing::warn!(target: TARGET, %error, "could not look for expired outbox items"),
    }
    let queued = state.store.outbox_list(None, Some(OutboxState::Queued)).unwrap_or_default();
    for mut item in queued.iter().cloned() {
        if hub.target(&item.target).is_some() {
            continue;
        }
        item.state = OutboxState::Failed;
        item.last_error = Some("That machine was removed from your network, so this cannot be delivered.".into());
        item.updated_at = now;
        if state.store.outbox_update(&item).unwrap_or(false) {
            settle(state, &item);
        }
    }
    let due = match state.store.outbox_due(now) {
        Ok(due) => due,
        Err(error) => {
            tracing::warn!(target: TARGET, %error, "could not read the outbox");
            return;
        }
    };
    for item in due {
        let Some(target) = hub.target(&item.target) else { continue };
        if !target.online || target.paused {
            continue;
        }
        let key = (item.target.clone(), item.kind);
        if !in_flight.lock().unwrap_or_else(|error| error.into_inner()).insert(key.clone()) {
            continue;
        }
        let claimed = match state.store.outbox_claim(item.id, now) {
            Ok(Some(claimed)) => claimed,
            _ => {
                in_flight.lock().unwrap_or_else(|error| error.into_inner()).remove(&key);
                continue;
            }
        };
        announce(state, &claimed);
        let state = state.clone();
        let hub = hub.clone();
        let in_flight = in_flight.clone();
        tokio::spawn(async move {
            let delivery = deliver(&state, &claimed).await;
            finish(&state, claimed, delivery);
            in_flight.lock().unwrap_or_else(|error| error.into_inner()).remove(&key);
            // The next item for this target goes now.
            hub.kick();
        });
    }
}

async fn deliver(state: &AppState, item: &OutboxItem) -> Delivery {
    match item.kind {
        OutboxKind::Delegate => state.orchestrator.remote_deliver_delegate(item).await,
        OutboxKind::ThreadCreate => state.orchestrator.remote_deliver_call(item, PeerThreadCreate::NAME).await,
        OutboxKind::ThreadSend => state.orchestrator.remote_deliver_call(item, PeerThreadSend::NAME).await,
        OutboxKind::Fork => state.orchestrator.remote_deliver_fork(item).await,
        OutboxKind::Move => state.orchestrator.remote_deliver_move(item).await,
        OutboxKind::SettingsPatch => crate::settings_share::deliver(state, item).await,
        OutboxKind::SecretsPut => super::broker::deliver_secrets_put(state, item).await,
        OutboxKind::FilesPush => super::files_push::deliver(state, item).await,
        OutboxKind::Provision => super::provision::deliver(state, item).await,
        OutboxKind::SkillCopy => super::skill_sync::deliver(state, item).await,
        OutboxKind::PluginInstall => super::plugin_sync::deliver(state, item).await,
    }
}

/// Record how a delivery ended.
fn finish(state: &AppState, mut item: OutboxItem, delivery: Delivery) {
    let now = Utc::now();
    item.updated_at = now;
    match delivery {
        Delivery::Delivered(result) => {
            item.state = OutboxState::Delivered;
            item.result = Some(result);
            item.last_error = None;
        }
        Delivery::Retry(reason) => {
            item.state = OutboxState::Queued;
            item.last_error = Some(reason);
            let wait = super::peers::backoff(&state.network.timing, item.attempts);
            item.next_attempt_at = Some(now + chrono::Duration::from_std(wait).unwrap_or_default());
        }
        Delivery::Failed(reason) => {
            item.state = OutboxState::Failed;
            item.last_error = Some(reason);
        }
    }
    match state.store.outbox_finish(&item) {
        Ok(true) => {}
        // A cancel got there first. A delivery that landed anyway still has to reach the child.
        Ok(false) if item.state == OutboxState::Delivered => {}
        Ok(false) => return,
        Err(error) => {
            tracing::warn!(target: TARGET, %error, "could not record a delivery");
            return;
        }
    }
    settle(state, &item);
}

/// Tell the clients and the orchestrator that an item changed.
fn settle(state: &AppState, item: &OutboxItem) {
    announce(state, item);
    if item.kind == OutboxKind::Provision {
        super::provision::settled(state, item);
    }
    state.orchestrator.remote_outbox_changed(item);
}

// ---- the outbox.* methods ----

pub fn list(state: &AppState, params: OutboxListParams) -> Result<OutboxListResult, RpcError> {
    let items = state.store.outbox_list(params.target.as_deref(), params.state).map_err(RpcError::internal)?;
    Ok(OutboxListResult { items })
}

fn item_of(state: &AppState, id: Uuid) -> Result<OutboxItem, RpcError> {
    state.store.outbox_get(id).map_err(RpcError::internal)?.ok_or_else(|| RpcError::new(codes::NOT_FOUND, "no such outbox item"))
}

/// Queue an expired or failed item again, with a fresh seven days.
pub fn retry(state: &AppState, params: OutboxIdParams) -> Result<OutboxItem, RpcError> {
    let mut item = item_of(state, params.id)?;
    if !matches!(item.state, OutboxState::Failed | OutboxState::Expired | OutboxState::Cancelled) {
        return Err(RpcError::new(
            codes::CONFLICT,
            format!("this item is {}; only a failed, expired or cancelled item can be retried", item.state.as_str()),
        ));
    }
    if state.orchestrator.remote_hub().and_then(|hub| hub.target(&item.target)).is_none() {
        return Err(RpcError::new(codes::NOT_FOUND, "that machine is not in your network any more"));
    }
    let now = Utc::now();
    item.state = OutboxState::Queued;
    item.attempts = 0;
    item.next_attempt_at = None;
    item.last_error = None;
    item.result = None;
    item.expires_at = now + chrono::Duration::days(OUTBOX_EXPIRY_DAYS);
    item.updated_at = now;
    state.store.outbox_update(&item).map_err(RpcError::internal)?;
    settle(state, &item);
    kick(state);
    Ok(item)
}

/// Cancel a queued (or in-flight) item. A delegation that already reached its machine is stopped there.
pub fn cancel(state: &AppState, params: OutboxIdParams) -> Result<OutboxItem, RpcError> {
    let mut item = item_of(state, params.id)?;
    if !matches!(item.state, OutboxState::Queued | OutboxState::Sending) {
        return Ok(item);
    }
    item.state = OutboxState::Cancelled;
    item.last_error = None;
    item.next_attempt_at = None;
    item.updated_at = Utc::now();
    state.store.outbox_update(&item).map_err(RpcError::internal)?;
    settle(state, &item);
    Ok(item)
}

/// A client queues work for a machine it cannot reach (§6.4). Agents queue delegations through
/// `kybern_agent_delegate`, never through this.
pub fn enqueue(state: &AppState, params: OutboxEnqueueParams) -> Result<OutboxItem, RpcError> {
    let hub = state.orchestrator.remote_hub().ok_or_else(|| RpcError::new(codes::NOT_AVAILABLE, "the machine network is not available"))?;
    if hub.target(&params.target).is_none() {
        return Err(RpcError::new(codes::NOT_FOUND, "that machine is not in your network"));
    }
    let operation_id = params.operation.operation_id.unwrap_or_else(Uuid::now_v7);
    // The target deduplicates on the ids a request carries, so they are fixed now, from the operation:
    // a delivery that is repeated finds the thread and the message it already made.
    let mut payload = params.operation.payload.clone();
    match params.operation.kind {
        OutboxKind::Delegate => return Err(RpcError::invalid_params("delegations are queued by agents with kybern_agent_delegate")),
        OutboxKind::ThreadCreate => {
            let mut create: ThreadsCreateParams = serde_json::from_value(payload).map_err(RpcError::invalid_params)?;
            create.thread_id.get_or_insert_with(|| derive(operation_id, 0x21));
            if create.message.is_some() {
                create.message_id.get_or_insert_with(|| derive(operation_id, 0x22));
            }
            payload = serde_json::to_value(create).map_err(RpcError::internal)?;
        }
        OutboxKind::ThreadSend => {
            let mut send: ThreadsSendParams = serde_json::from_value(payload).map_err(RpcError::invalid_params)?;
            send.message_id.get_or_insert_with(|| derive(operation_id, 0x23));
            payload = serde_json::to_value(send).map_err(RpcError::internal)?;
        }
        OutboxKind::SettingsPatch => {
            let patch: SettingsPatch = serde_json::from_value(payload.clone()).map_err(RpcError::invalid_params)?;
            crate::settings_share::check_patch(&patch)?;
        }
        OutboxKind::SecretsPut => {
            // Only the sealed blob is queued; the value is never in the outbox in the clear.
            serde_json::from_value::<PeerSecretsPutParams>(payload.clone()).map_err(RpcError::invalid_params)?;
        }
        other => {
            return Err(RpcError::new(
                codes::NOT_AVAILABLE,
                format!("Queuing {} for an offline machine is not available in this version yet.", other.as_str()),
            ));
        }
    }
    queue(state, &params.target, params.operation.kind, operation_id, payload)
}

/// Insert a queued item for `target` and wake the worker. The callers have checked the payload.
pub(super) fn queue(state: &AppState, target: &str, kind: OutboxKind, operation_id: Uuid, payload: Value) -> Result<OutboxItem, RpcError> {
    let now = Utc::now();
    let item = OutboxItem {
        id: Uuid::now_v7(),
        operation_id,
        target: target.to_string(),
        kind,
        payload,
        origin: None,
        state: OutboxState::Queued,
        attempts: 0,
        next_attempt_at: None,
        last_error: None,
        result: None,
        created_at: now,
        expires_at: now + chrono::Duration::days(OUTBOX_EXPIRY_DAYS),
        updated_at: now,
    };
    let item = state.store.outbox_insert(&item).map_err(RpcError::internal)?;
    announce(state, &item);
    kick(state);
    Ok(item)
}

/// A stable id for one part of an operation (the thread, the message), the way local delegation derives its own.
fn derive(operation_id: Uuid, tag: u8) -> Uuid {
    let mut bytes = *operation_id.as_bytes();
    bytes[0] ^= tag;
    Uuid::from_bytes(bytes)
}

fn kick(state: &AppState) {
    if let Some(hub) = state.orchestrator.remote_hub() {
        hub.kick();
    }
}
