//! The small `network.*` writes: pause, rename, a machine's own policy, the master switch and
//! Rejoin. Each one is a register or a setting; the result is spread to every member at once.

use kybern_protocol::{MachineCard, Scope};
use serde_json::json;

use super::join::JoinError;
use super::membership::MembershipError;
use super::{TARGET, status};
use crate::state::AppState;

/// Sign a register write and spread it.
fn write_register(state: &AppState, key: &str, value: serde_json::Value) -> Result<(), JoinError> {
    let network = &state.network;
    let membership =
        network.membership().ok_or_else(|| JoinError::Unavailable("The machine network is unavailable on this machine.".into()))?;
    let op = membership.set(key, value)?;
    network.publish_ops(vec![op]);
    network.wake();
    Ok(())
}

fn require_member(state: &AppState, machine_id: &str) -> Result<(), JoinError> {
    let membership =
        state.network.membership().ok_or_else(|| JoinError::Unavailable("The machine network is unavailable on this machine.".into()))?;
    if membership.is_member(machine_id) { Ok(()) } else { Err(JoinError::NotFound("that machine is not in your network".into())) }
}

async fn card_of(state: &AppState, machine_id: &str) -> Result<MachineCard, JoinError> {
    status::card(state, machine_id).await.ok_or_else(|| JoinError::NotFound("that machine is not in your network".into()))
}

/// `network.pause`: a paused machine keeps syncing operations and opens no other sessions (§2.5).
pub async fn pause(state: &AppState, machine_id: &str, paused: bool) -> Result<MachineCard, JoinError> {
    require_member(state, machine_id)?;
    write_register(state, &format!("machine.{machine_id}.paused"), json!(paused))?;
    if paused && let Some(link) = state.network.peers.link(machine_id) {
        // The session stays for operation sync; its scopes narrow to `machine:peer` at use time.
        tracing::info!(target: TARGET, machine = machine_id, "paused a machine (session kept for operation sync)");
        let _ = link;
    }
    card_of(state, machine_id).await
}

/// `network.rename`.
pub async fn rename(state: &AppState, machine_id: &str, name: &str) -> Result<MachineCard, JoinError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 80 {
        return Err(JoinError::Invalid("A machine name is 1 to 80 characters.".into()));
    }
    require_member(state, machine_id)?;
    write_register(state, &format!("machine.{machine_id}.name"), json!(name))?;
    card_of(state, machine_id).await
}

/// `network.allow.set`: what this machine lets the others use. A machine's policy is a register
/// signed by its own key (§4.1), so it can only be written on that machine.
pub async fn allow(state: &AppState, machine_id: &str, capability: &str, allowed: bool) -> Result<MachineCard, JoinError> {
    let network = &state.network;
    let membership =
        network.membership().ok_or_else(|| JoinError::Unavailable("The machine network is unavailable on this machine.".into()))?;
    let me = membership.machine_id();
    if machine_id != me {
        return Err(JoinError::Invalid("A machine's policy is changed on that machine. Connect to it and set it there.".into()));
    }
    let Some(scope) = Scope::from_capability(capability) else {
        return Err(JoinError::Invalid(format!("Unknown capability \"{capability}\".")));
    };
    if scope == Scope::MachinePeer {
        return Err(JoinError::Invalid("Operation sync cannot be turned off.".into()));
    }
    write_register(state, &format!("allow.{me}.{capability}"), json!(allowed))?;
    card_of(state, machine_id).await
}

/// `network.set_enabled`: the master switch ("Connect to your other machines"). Off leaves the
/// network and keeps membership (§4.1).
pub async fn set_enabled(state: &AppState, enabled: bool) -> Result<(), JoinError> {
    set_enabled_guarded(state, enabled, false).await
}

/// The check and write share a lock with explicit toggles, so a concurrent off always wins
/// over a later automatic setup. This is an authenticated NetworkAdmin RPC, never discovery.
pub async fn set_enabled_guarded(state: &AppState, enabled: bool, only_if_unconfigured: bool) -> Result<(), JoinError> {
    let _guard = state.network.enabled_change.lock().await;
    let mut settings = state.settings.get();
    if only_if_unconfigured {
        if !enabled {
            return Err(JoinError::Invalid("Guarded setup can only turn the network on.".into()));
        }
        if settings.network.configured {
            return Ok(());
        }
    }
    if settings.network.enabled != enabled || !settings.network.configured {
        settings.network.enabled = enabled;
        settings.network.configured = true;
        state.settings.set(settings).map_err(|error| JoinError::Unavailable(format!("{error:#}")))?;
    }
    state.network.set_enabled(enabled);
    // Either way the worker starts over: dials in flight are given up and every wait after failed attempts ends, so
    // turning the network off and on again is a way out of anything stuck.
    state.network.peers.after_resume();
    crate::exposure::apply_network(state, enabled).await;
    if !enabled {
        state.network.peers.close_all();
    }
    tracing::info!(target: TARGET, enabled, "the machine network was switched; the worker starts over");
    state.network.notify_changed();
    state.network.wake();
    Ok(())
}

/// `network.rejoin`: a removed machine makes a new key and machine id and is a lone machine again
/// (§2.6). The daemon's cached machine id is read when it starts, so the daemon restarts.
pub async fn rejoin(state: &AppState) -> Result<(), JoinError> {
    let network = &state.network;
    let membership =
        network.membership().ok_or_else(|| JoinError::Unavailable("The machine network is unavailable on this machine.".into()))?;
    match membership.rejoin() {
        Ok(reset) => {
            tracing::info!(target: TARGET, old = %reset.old_machine_id, new = %reset.new_machine_id, "rejoined with a new identity; restarting");
            network.peers.close_all();
            network.notify_changed();
            let shutdown = state.shutdown.clone();
            let process_owned = state.process_owned.load(std::sync::atomic::Ordering::SeqCst);
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                shutdown.cancel();
                // A graceful shutdown waits for every open connection, and a peer's idle connection can
                // hold it for minutes. The daemon must come back with its new identity, so a process
                // that is still here after a few seconds ends; whatever supervises it restarts it.
                if process_owned {
                    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
                    tracing::warn!(target: TARGET, "the daemon did not stop in time after a rejoin; exiting");
                    std::process::exit(0);
                }
            });
            Ok(())
        }
        Err(MembershipError::NotRemoved) => Err(JoinError::Invalid("This machine was not removed from a network.".into())),
        Err(error) => Err(error.into()),
    }
}
