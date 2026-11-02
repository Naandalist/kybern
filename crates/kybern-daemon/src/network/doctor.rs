//! `network.doctor` and `kybern machines doctor` (design §10.3): a list of checks, each pass, warn or
//! fail, with what to do next. Nothing here changes state; it reads the tailnet, probes each peer's
//! well-known endpoint, and compares membership digests over the open sessions.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use kybern_protocol::methods::PeerOpsSync;
use kybern_protocol::{DoctorCheck, DoctorReport, DoctorStatus};

use super::well_known;
use crate::state::AppState;

/// A clock this far from ours is worth a warning (§10.3).
const CLOCK_SKEW_WARN: i64 = 30;

fn check(id: &str, status: DoctorStatus, title: impl Into<String>, next_step: Option<&str>, machine_id: Option<&str>) -> DoctorCheck {
    DoctorCheck {
        id: id.to_string(),
        status,
        title: title.into(),
        next_step: next_step.filter(|_| status != DoctorStatus::Pass).map(str::to_string),
        machine_id: machine_id.map(str::to_string),
    }
}

fn pass(id: &str, title: impl Into<String>, machine_id: Option<&str>) -> DoctorCheck {
    check(id, DoctorStatus::Pass, title, None, machine_id)
}

/// Whether this machine stays reachable when the app is closed (ADE-119), and what is wrong with the
/// services that start it.
fn keep_reachable_checks(state: &AppState) -> Vec<DoctorCheck> {
    let mut checks: Vec<DoctorCheck> = state
        .login_service
        .warnings(state)
        .into_iter()
        .map(|(id, title, next)| check(&id, DoctorStatus::Warn, title, Some(&next), None))
        .collect();
    let status = state.login_service.status(state);
    let member = state.network.membership().is_some_and(|membership| membership.self_is_member() && !membership.self_removed());
    if status.kind.is_none() || !member || !state.network.is_enabled() {
        return checks;
    }
    let next = "Turn on Keep this machine reachable in Settings → Machines, or run `kybern machines reachable on`.";
    checks.push(match (&status.external, status.enabled, status.registered, &status.error) {
        (Some(external), _, _, _) => pass("service.login", external.clone(), None),
        (None, false, _, _) => {
            check("service.login", DoctorStatus::Warn, "Other machines lose this one when Kybern is closed", Some(next), None)
        }
        (None, true, true, None) => {
            pass("service.login", "Kybern starts at login, so other machines reach this one when the app is closed", None)
        }
        (None, true, _, error) => check(
            "service.login",
            DoctorStatus::Warn,
            error.clone().unwrap_or_else(|| "Kybern is not registered to start at login yet".into()),
            Some("Turn Keep this machine reachable off and on again in Settings → Machines, or run `kybern machines reachable on`."),
            None,
        ),
    });
    checks
}

/// Run the checks. `only` limits the per-peer checks to one machine and skips the machine-wide ones.
pub async fn report(state: &AppState, only: Option<&str>) -> DoctorReport {
    let network = &state.network;
    let mut checks = Vec::new();
    let Some(membership) = network.membership() else {
        checks.push(check(
            "membership.available",
            DoctorStatus::Fail,
            "This machine's network key could not be opened",
            Some("Check the permissions of the network folder in the data directory, then restart Kybern."),
            None,
        ));
        return DoctorReport { checks, generated_at: Utc::now() };
    };
    let status = network.tailscale.status().await;

    if only.is_none() {
        match &status {
            Ok(status) if status.running => {
                checks.push(pass("tailscale.running", "Tailscale is running", None));
                match status.self_node.as_ref().and_then(|node| node.ip()) {
                    Some(ip) => checks.push(pass("tailscale.self_ip", format!("This machine's Tailscale address is {ip}"), None)),
                    None => checks.push(check(
                        "tailscale.self_ip",
                        DoctorStatus::Fail,
                        "This machine has no Tailscale address",
                        Some("Wait for Tailscale to finish connecting, or sign in again."),
                        None,
                    )),
                }
                match status.lock_enabled {
                    Some(true) => checks.push(pass("tailscale.lock", "Tailnet lock is on", None)),
                    _ => checks.push(check(
                        "tailscale.lock",
                        DoctorStatus::Warn,
                        "Tailnet lock is off",
                        Some("Turn on tailnet lock in the Tailscale admin console so only machines you approve can join the tailnet."),
                        None,
                    )),
                }
            }
            Ok(status) => checks.push(check(
                "tailscale.running",
                DoctorStatus::Fail,
                status.detail.clone().unwrap_or_else(|| "Tailscale is not running".into()),
                Some("Start Tailscale and sign in. Machines are found over your tailnet."),
                None,
            )),
            Err(error) => checks.push(check(
                "tailscale.running",
                DoctorStatus::Fail,
                format!("Tailscale is not available: {error}"),
                Some("Install Tailscale from tailscale.com/download and sign in with the same account on each machine."),
                None,
            )),
        }
        if !network.is_enabled() {
            checks.push(check(
                "network.enabled",
                DoctorStatus::Warn,
                "Connect to your other machines is turned off",
                Some("Turn it on in Settings → Machines, or run `kybern machines enable`."),
                None,
            ));
        }
        match network.listen_info() {
            info if info.addr.is_some() => {
                checks.push(pass("tailscale.listener", format!("Listening for machines on {}", info.addr.unwrap()), None))
            }
            info if network.is_enabled() => checks.push(check(
                "tailscale.listener",
                DoctorStatus::Fail,
                info.error.unwrap_or_else(|| "Not listening for machines yet".into()),
                Some("Check that Tailscale is running, and that ports 4173 to 4179 are free on this machine."),
                None,
            )),
            _ => {}
        }
        checks.extend(keep_reachable_checks(state));
        if !membership.has_network() {
            checks.push(check(
                "membership.self_member",
                DoctorStatus::Warn,
                "This machine is not in a network yet",
                Some("It joins on its own when another of your machines is on the tailnet. Otherwise add it with a code."),
                None,
            ));
        } else if membership.self_removed() {
            checks.push(check(
                "membership.self_member",
                DoctorStatus::Fail,
                "This machine was removed from its network",
                Some("Run `kybern machines rejoin` to get a new identity, then add it from another machine."),
                None,
            ));
        } else if membership.key_mismatch() {
            checks.push(check(
                "membership.self_member",
                DoctorStatus::Fail,
                "This machine's key does not match its membership",
                Some("The key file was replaced. Remove this machine from another one, then rejoin."),
                None,
            ));
        } else {
            checks.push(pass("membership.self_member", "This machine is a member of its network", None));
        }
    }

    let me = membership.machine_id();
    let projection = membership.projection();
    let tailnet = status.as_ref().ok().filter(|status| status.running);
    for (id, entry) in projection.members() {
        if *id == me || only.is_some_and(|only| only != id) {
            continue;
        }
        let name = entry.descriptor.name.clone();
        let mid = Some(id.as_str());
        if entry.paused {
            checks.push(check(
                "peer.paused",
                DoctorStatus::Warn,
                format!("{name} is paused"),
                Some("Resume it in Settings → Machines."),
                mid,
            ));
        }
        // Session and digest first: they need no network round trip of their own.
        match network.peers.link(id) {
            Some(link) => {
                checks.push(pass("peer.session", format!("{name}: connected for {}", human(link.opened_at.elapsed())), mid));
                match tokio::time::timeout(Duration::from_secs(5), link.call::<PeerOpsSync>(membership.sync_request())).await {
                    Ok(Ok(theirs)) if theirs.digest == membership.digest() => {
                        checks.push(pass("peer.digest", format!("{name}: has the same membership"), mid))
                    }
                    Ok(Ok(_)) => checks.push(check(
                        "peer.digest",
                        DoctorStatus::Warn,
                        format!("{name}: its membership differs from this machine's"),
                        Some("They sync on their own within a moment. If this stays, restart Kybern on the older machine."),
                        mid,
                    )),
                    _ => checks.push(check(
                        "peer.digest",
                        DoctorStatus::Warn,
                        format!("{name}: did not answer a membership check"),
                        None,
                        mid,
                    )),
                }
                if let Some(presence) = network.peers.status(id).presence {
                    checks.push(pass(
                        "peer.version",
                        format!("{name}: Kybern {} ({} features)", presence.version, presence.features.len()),
                        mid,
                    ));
                }
            }
            None => checks.push(check(
                "peer.session",
                DoctorStatus::Fail,
                format!("{name}: no session"),
                Some(if tailnet.is_some() {
                    "Check that Kybern is running on that machine and that it is online in Tailscale."
                } else {
                    "Start Tailscale first."
                }),
                mid,
            )),
        }
        // The tailnet's view of the node: online, the same node id, a probe with a stopwatch.
        let node = tailnet.and_then(|status| entry.descriptor.ts_node_id.as_deref().and_then(|node_id| status.peer(node_id)));
        let Some(node) = node else {
            if tailnet.is_some() {
                checks.push(check(
                    "peer.whois",
                    DoctorStatus::Fail,
                    format!("{name}: its Tailscale node is not on this tailnet"),
                    Some("Check that it is signed in to the same Tailscale account."),
                    mid,
                ));
            }
            continue;
        };
        if !node.online {
            checks.push(check(
                "peer.well_known",
                DoctorStatus::Fail,
                format!("{name}: offline in Tailscale"),
                Some("Turn that machine on."),
                mid,
            ));
            continue;
        }
        let Some(ip) = node.ip() else { continue };
        match network.tailscale.whois(ip).await {
            Ok(whois) if entry.descriptor.ts_node_id.as_deref() == Some(whois.node_id.as_str()) => {
                checks.push(pass("peer.whois", format!("{name}: Tailscale confirms the node"), mid))
            }
            Ok(_) => checks.push(check(
                "peer.whois",
                DoctorStatus::Fail,
                format!("{name}: Tailscale names another node for its address"),
                Some("The address changed hands. Remove the machine and add it again."),
                mid,
            )),
            Err(error) => {
                checks.push(check("peer.whois", DoctorStatus::Warn, format!("{name}: could not ask Tailscale: {error}"), None, mid))
            }
        }
        let port = entry.descriptor.port.unwrap_or(kybern_protocol::DEFAULT_PORT);
        let base = well_known::base_url(network.tailscale.dial_target(ip), port);
        let started = Instant::now();
        match well_known::probe(&network.http, &base, None, Duration::from_secs(10), Duration::from_secs(3600)).await {
            Ok(answer) => {
                checks.push(pass("peer.well_known", format!("{name}: answers in {} ms", started.elapsed().as_millis()), mid));
                if answer.machine_id == *id && answer.signing_key == entry.descriptor.signing_key {
                    checks.push(pass("peer.key_matches", format!("{name}: its key matches the network's record"), mid));
                } else {
                    checks.push(check(
                        "peer.key_matches",
                        DoctorStatus::Fail,
                        format!("{name}: the address answers as a different machine or key"),
                        Some("Do not trust that address. Remove the machine here and add it again from the machine itself."),
                        mid,
                    ));
                }
                if let Ok(at) = DateTime::parse_from_rfc3339(&answer.at) {
                    let skew = (Utc::now() - at.with_timezone(&Utc)).num_seconds().abs();
                    if skew > CLOCK_SKEW_WARN {
                        checks.push(check(
                            "peer.clock",
                            DoctorStatus::Warn,
                            format!("{name}: its clock is {skew} s from this machine's"),
                            Some("Turn on automatic time on both machines. Sessions fail when clocks are a minute apart."),
                            mid,
                        ));
                    } else {
                        checks.push(pass("peer.clock", format!("{name}: clocks agree within {skew} s"), mid));
                    }
                }
            }
            Err(error) => checks.push(check(
                "peer.well_known",
                DoctorStatus::Fail,
                format!("{name}: could not be reached ({error})"),
                Some("Check that Kybern is running on that machine and that its Connect to your other machines switch is on."),
                mid,
            )),
        }
    }

    if only.is_none()
        && let Some(status) = tailnet
    {
        // A node that is a member again (a rejoined machine) is not a leftover.
        let members: Vec<&str> = projection.members().filter_map(|(_, entry)| entry.descriptor.ts_node_id.as_deref()).collect();
        for node in status
            .peers
            .iter()
            .filter(|node| node.online && projection.is_node_tombstoned(&node.node_id) && !members.contains(&node.node_id.as_str()))
        {
            checks.push(check(
                "network.removed_nodes",
                DoctorStatus::Warn,
                format!("{} was removed from your network but is still on your tailnet", node.name()),
                Some("Remove it from the Tailscale admin console, or lock it, so it cannot reach your machines' identity checks."),
                None,
            ));
        }
    }
    DoctorReport { checks, generated_at: Utc::now() }
}

fn human(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds {
        0..=89 => format!("{seconds} s"),
        90..=5399 => format!("{} min", seconds / 60),
        _ => format!("{} h", seconds / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_in_the_largest_sensible_unit() {
        assert_eq!(human(Duration::from_secs(5)), "5 s");
        assert_eq!(human(Duration::from_secs(120)), "2 min");
        assert_eq!(human(Duration::from_secs(7200)), "2 h");
    }

    #[test]
    fn a_passing_check_carries_no_next_step() {
        let ok = check("x", DoctorStatus::Pass, "fine", Some("do something"), None);
        assert_eq!(ok.next_step, None);
        let bad = check("x", DoctorStatus::Warn, "hmm", Some("do something"), Some("m1"));
        assert_eq!(bad.next_step.as_deref(), Some("do something"));
        assert_eq!(bad.machine_id.as_deref(), Some("m1"));
    }
}
