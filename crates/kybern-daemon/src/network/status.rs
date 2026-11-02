//! What `network.status` reports (design §4.1): every member as a card, the machines found on the
//! tailnet, Tailscale's state and the outbox, in one call. Also the single-card form the joining and
//! pause handlers return.

use kybern_protocol::{BrokerInfo, MachineCard, MachineStatus, NetworkStatus, OutboxSummary, TailscaleState};

use super::join;
use crate::state::AppState;

/// One current member as a card: the membership set's facts, then reachability from the peer
/// manager and presence.
pub async fn card(state: &AppState, machine_id: &str) -> Option<MachineCard> {
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

async fn tailscale_state(state: &AppState) -> TailscaleState {
    let network = &state.network;
    let listening_port = network.listening_port();
    match network.tailscale.status().await {
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
    let network = &state.network;
    let membership = network.membership()?;
    // A client looking at Machines makes discovery tick every minute for a while (§3.1).
    network.touch_watching();
    let me = membership.machine_id();
    let projection = membership.projection();
    let mut members = Vec::new();
    for (id, _) in projection.members() {
        if let Some(card) = card(state, id).await {
            members.push(card);
        }
    }
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
    let setup_candidates = if network.is_enabled() {
        network
            .tailscale
            .status()
            .await
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
        tailscale: tailscale_state(state).await,
        digest: membership.has_network().then(|| membership.digest()),
        removed: membership.self_removed(),
        follow_updates: projection.follow_updates(),
        keep_reachable: Some(state.login_service.status(state)),
    })
}
