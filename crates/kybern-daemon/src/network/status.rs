//! What `network.status` reports (design §4.1): every member as a card, the machines found on the
//! tailnet, Tailscale's state and the outbox, in one call. Also the single-card form the joining and
//! pause handlers return.

use std::sync::Arc;

use kybern_protocol::{BrokerInfo, MachineCard, MachineStatus, NetworkStatus, OutboxSummary, TailscaleState};
use tokio::sync::{broadcast, watch};

use super::join;
use super::tailscale::TsStatus;
use crate::state::AppState;

/// One current member as a card: the membership set's facts, then reachability from the peer
/// manager and presence. Reads (and may write) the store, on the blocking pool.
pub async fn card(state: &AppState, machine_id: &str) -> Option<MachineCard> {
    let (state, machine_id) = (state.clone(), machine_id.to_string());
    tokio::task::spawn_blocking(move || card_now(&state, &machine_id)).await.ok().flatten()
}

/// [`card`], blocking: it reads the member's stored row and may store when it was last seen.
fn card_now(state: &AppState, machine_id: &str) -> Option<MachineCard> {
    let network = &state.network;
    let membership = network.membership()?;
    let me = membership.machine_id();
    let projection = membership.projection();
    let mut card = projection.card(machine_id, &me)?;
    if machine_id == me {
        card.arch = Some(std::env::consts::ARCH.to_string());
        let presence = network.presence();
        card.version = Some(presence.version);
        card.network_protocol = presence.network_protocol;
        card.features = presence.features;
        card.harnesses = presence.harnesses;
        return Some(card);
    }
    let peer = network.peers.status(machine_id);
    let cached = state.store.network_machine_get(machine_id).ok().flatten();
    let presence = peer.presence.clone().or_else(|| cached.as_ref().and_then(|row| row.presence.clone()));
    if let Some(presence) = presence {
        card.version = Some(presence.version);
        card.network_protocol = presence.network_protocol;
        card.features = presence.features;
        card.harnesses = presence.harnesses;
    }
    // A machine with a session up is seen now; its stored time moves on about once a minute.
    if peer.online
        && let Some(seen) = network.peers.refresh_seen(machine_id)
    {
        let _ = state.store.network_machine_touch(machine_id, seen);
    }
    let peer = if peer.online { network.peers.status(machine_id) } else { peer };
    card.last_seen_at = peer.last_seen.or_else(|| cached.and_then(|row| row.last_seen_at));
    if card.status != MachineStatus::Paused {
        card.status = if peer.online { MachineStatus::Online } else { MachineStatus::Offline };
    }
    Some(card)
}

/// This machine as a card when it is not a member of any network yet.
async fn lone_card(state: &AppState) -> MachineCard {
    let network = &state.network;
    let info = network.self_info().await;
    let presence = network.presence();
    MachineCard {
        machine_id: network.machine_id().unwrap_or_else(|| state.environment_id.clone()),
        name: info.name,
        os: info.os,
        arch: Some(std::env::consts::ARCH.to_string()),
        ts_name: info.ts_name,
        signing_key: network.membership().map(|membership| membership.keys().signing_public_b64()),
        status: MachineStatus::Online,
        is_self: true,
        version: Some(presence.version),
        network_protocol: presence.network_protocol,
        features: presence.features,
        last_seen_at: None,
        relay_via: None,
        harnesses: presence.harnesses,
        allow: Default::default(),
        added_by: None,
        added_via: None,
        added_at: None,
    }
}

fn tailscale_state(state: &AppState, status: &anyhow::Result<TsStatus>) -> TailscaleState {
    let network = &state.network;
    let listening_port = network.listening_port();
    match status {
        Ok(status) => TailscaleState {
            installed: true,
            running: status.running,
            self_ip: status.self_node.as_ref().and_then(|node| node.ip()).map(|ip| ip.to_string()),
            self_name: status.self_node.as_ref().map(|node| if node.dns_name.is_empty() { node.name() } else { node.dns_name.clone() }),
            tailnet: status.tailnet.clone(),
            login_name: status.self_node.as_ref().and_then(|node| status.login_of(node.user_id)).map(str::to_string),
            listening_port,
            lock_enabled: status.lock_enabled,
            detail: status.detail.clone().or_else(|| network.listen_info().error),
        },
        Err(error) => TailscaleState {
            installed: false,
            running: false,
            self_ip: None,
            self_name: None,
            tailnet: None,
            login_name: None,
            listening_port,
            lock_enabled: None,
            detail: Some(format!("{error:#}")),
        },
    }
}

/// Everything Settings → Machines needs.
pub async fn network_status(state: &AppState) -> Option<NetworkStatus> {
    // A client looking at Machines makes discovery tick every minute for a while (§3.1).
    state.network.touch_watching();
    snapshot(state).await
}

/// What `network.status` reports, without counting as a client looking at Machines: what the
/// `network.changed` notification carries.
pub async fn snapshot(state: &AppState) -> Option<NetworkStatus> {
    let network = &state.network;
    let membership = network.membership()?;
    let me = membership.machine_id();
    let projection = membership.projection();
    // The members' stored rows are read (and their sightings stored) on the blocking pool, all in one go.
    let ids: Vec<String> = projection.members().map(|(id, _)| id.to_string()).collect();
    let blocking = state.clone();
    let mut members: Vec<MachineCard> =
        tokio::task::spawn_blocking(move || ids.iter().filter_map(|id| card_now(&blocking, id)).collect()).await.unwrap_or_default();
    members.sort_by(|a, b| {
        b.is_self
            .cmp(&a.is_self)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.machine_id.cmp(&b.machine_id))
    });
    let self_machine = match members.iter().find(|card| card.machine_id == me) {
        Some(card) => card.clone(),
        None => lone_card(state).await,
    };
    let broker = projection.broker().map(|id| {
        let entry = projection.entry(id);
        BrokerInfo {
            machine_id: id.to_string(),
            name: entry.map(|entry| entry.descriptor.name.clone()),
            reachable: id == me || network.peers.link(id).is_some(),
        }
    });
    let found = join::found_list(state).await;
    // One read of Tailscale's status for the whole report.
    let tailscale = network.tailscale.status().await;
    let setup_candidates = if network.is_enabled() {
        tailscale
            .as_ref()
            .ok()
            .map(|ts| {
                ts.peers
                    .iter()
                    .filter(|node| {
                        !projection.members().any(|(_, entry)| entry.descriptor.ts_node_id.as_deref() == Some(node.node_id.as_str()))
                            && !found.iter().any(|item| item.found_id == node.node_id)
                    })
                    .take(64)
                    .map(|node| kybern_protocol::NetworkSetupCandidate {
                        node_id: node.node_id.clone(),
                        name: node.name(),
                        address: if node.dns_name.is_empty() {
                            node.ip().map(|ip| ip.to_string()).unwrap_or_default()
                        } else {
                            node.dns_name.clone()
                        },
                        online: node.online,
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    Some(NetworkStatus {
        configured: Some(state.settings.get().network.configured),
        setup_candidates,
        enabled: network.is_enabled(),
        network_id: membership.network_id(),
        self_machine,
        members,
        found,
        outbox: OutboxSummary::default(),
        broker,
        tailscale: tailscale_state(state, &tailscale),
        digest: membership.has_network().then(|| membership.digest()),
        removed: membership.self_removed(),
        follow_updates: projection.follow_updates(),
        keep_reachable: Some(state.login_service.status(state)),
    })
}

/// How long `network.changed` waits for more changes before it reads the status once for all of them.
const CHANGED_COALESCE: std::time::Duration = std::time::Duration::from_millis(300);

/// What `network.changed` carries: the status after each burst of changes (members, the found list, a link), built
/// once and shared by every connection that forwards it. The builder runs while some connection listens and stops
/// with the last one. The receiver starts with the current value seen, so it wakes on the next change.
pub fn subscribe_changes(state: &AppState) -> watch::Receiver<Option<Arc<NetworkStatus>>> {
    let mut feed = state.network.status_feed.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(sender) = feed.as_ref() {
        return sender.subscribe();
    }
    let (sender, receiver) = watch::channel(None);
    *feed = Some(sender.clone());
    // Subscribed before the builder starts, so no change between now and its first poll is missed.
    let changes = state.network.subscribe_changes();
    tokio::spawn(publish_changes(state.clone(), sender, changes));
    receiver
}

async fn publish_changes(state: AppState, sender: watch::Sender<Option<Arc<NetworkStatus>>>, mut changes: broadcast::Receiver<()>) {
    use broadcast::error::{RecvError, TryRecvError};
    loop {
        let changed = tokio::select! {
            changed = changes.recv() => changed,
            () = sender.closed() => {
                // Subscribing happens under this lock: with no receiver now, none can join this sender.
                let mut feed = state.network.status_feed.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                if sender.receiver_count() == 0 {
                    *feed = None;
                    return;
                }
                continue;
            }
        };
        match changed {
            Ok(()) | Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => break,
        }
        tokio::time::sleep(CHANGED_COALESCE).await;
        while matches!(changes.try_recv(), Ok(()) | Err(TryRecvError::Lagged(_))) {}
        if let Some(status) = snapshot(&state).await {
            sender.send_replace(Some(Arc::new(status)));
        }
    }
    // The network's change channel closed (the daemon is stopping). The feed is only replaced once it is cleared, so
    // the one there is this one.
    *state.network.status_feed.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}
