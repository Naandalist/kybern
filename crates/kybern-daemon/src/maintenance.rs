//! Daemon-owned background sweep: releases agent processes and shells that
//! finished their work, purges exited terminal records, and, when configured,
//! exits the daemon once nothing has needed it for a while.
//!
//! Everything here is driven by `settings.background`; see
//! `BackgroundSettings` for what each limit means. The sweep never touches a
//! thread that is running, awaiting approval, or doing background work.

use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use kybern_protocol::methods::DaemonActivity;
use kybern_protocol::{BackgroundSettings, HarnessUpdateStatus};

use crate::state::AppState;
use crate::terminal::EXITED_RETENTION;

/// How often the sweep runs. Idle limits are minutes, so this only bounds how
/// late a release can be.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Snapshot of what the daemon is holding open.
pub async fn activity(state: &AppState) -> Result<DaemonActivity> {
    let sessions = state.orchestrator.session_activity().await?;
    let idle_since = *state.idle_since.read().unwrap_or_else(|poisoned| poisoned.into_inner());
    let on_battery = *state.on_battery.read().unwrap_or_else(|poisoned| poisoned.into_inner());
    let idle_exit = state.settings.get().background.daemon_idle_exit();
    Ok(DaemonActivity {
        connections: count(state.connections.load(Ordering::SeqCst)),
        live_sessions: count(sessions.live),
        idle_sessions: count(sessions.idle),
        running_threads: count(state.store.threads_running()?.len()),
        terminals: count(state.terminals.alive()),
        queued_messages: count(state.store.queue_list(None)?.len()),
        idle_since,
        idle_exit_at: idle_since
            .zip(idle_exit)
            .and_then(|(since, limit)| since.checked_add_signed(chrono::Duration::from_std(limit).ok()?)),
        on_battery,
    })
}

fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Anything that means a client, an agent, or queued work still needs the daemon.
pub fn is_busy(activity: &DaemonActivity, updating_harness: bool) -> bool {
    activity.connections > 0
        || activity.live_sessions > 0
        || activity.running_threads > 0
        || activity.terminals > 0
        || activity.queued_messages > 0
        || updating_harness
}

/// Whether the daemon has been idle for at least `limit`.
pub fn idle_exit_due(idle_since: Option<DateTime<Utc>>, limit: Option<Duration>, now: DateTime<Utc>) -> bool {
    match (idle_since, limit.and_then(|limit| chrono::Duration::from_std(limit).ok())) {
        (Some(since), Some(limit)) => now.signed_duration_since(since) >= limit,
        _ => false,
    }
}

/// Where the idle clock stands after one sweep. A busy daemon has no idle
/// start. An idle one starts its clock at this sweep, but never earlier than
/// its latest event: a whole turn can start and finish between two sweeps,
/// and that work must push the exit deadline back.
pub fn next_idle_since(
    previous: Option<DateTime<Utc>>,
    busy: bool,
    latest_event: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if busy {
        return None;
    }
    let started = previous.unwrap_or(now);
    Some(latest_event.map_or(started, |event| event.max(started)))
}

/// Whether the idle exit may stop this daemon. A daemon that keeps this machine reachable for its
/// network (`settings.network.keep_reachable`, ADE-119) stays up: the other members need it while
/// no client is connected.
pub fn idle_exit_allowed(allow_idle_exit: bool, keeps_reachable: bool) -> bool {
    allow_idle_exit && !keeps_reachable
}

/// Whether another daemon has taken over this data directory: the port file
/// names a different port than ours. Clients follow the port file, so nobody
/// new can reach a superseded daemon; it should finish its work and exit
/// instead of lingering.
pub fn superseded_by(port_file_contents: Option<&str>, own_port: u16) -> Option<u16> {
    if own_port == 0 {
        return None;
    }
    let published = port_file_contents?.trim().parse::<u16>().ok()?;
    (published != own_port && published != 0).then_some(published)
}

fn superseded(state: &AppState) -> Option<u16> {
    let contents = std::fs::read_to_string(&state.paths.port_file).ok();
    superseded_by(contents.as_deref(), state.port.load(Ordering::Relaxed))
}

/// One pass: read the power state, release what is idle, then decide
/// whether the daemon itself is.
pub async fn sweep(state: &AppState, policy: &BackgroundSettings, allow_idle_exit: bool) -> Result<()> {
    let on_battery = tokio::task::spawn_blocking(crate::power::on_battery).await.unwrap_or(None);
    *state.on_battery.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = on_battery;
    let saving_power = policy.save_power_on_battery && on_battery == Some(true);
    // Detect `git init` (or repository removal) while a draft is open, without
    // adding per-view polling or waking Git when no client is connected.
    if state.connections.load(Ordering::Relaxed) > 0 {
        state.orchestrator.refresh_projects_git().await?;
    }
    match state.orchestrator.release_idle_sessions(policy, saving_power).await {
        Ok(released) if !released.is_empty() => tracing::debug!(count = released.len(), "released idle agent processes"),
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "could not release idle agent processes"),
    }
    let terminals = state.terminals.release_idle(policy.terminal_idle());
    if !terminals.is_empty() {
        tracing::debug!(count = terminals.len(), "closed idle terminals");
    }
    state.terminals.purge_exited(EXITED_RETENTION);
    state.orchestrator.cleanup_eligible_worktrees().await;

    let activity = activity(state).await?;
    // A first-run scan or import keeps the daemon up even with no client connected.
    let updating = state.harness_updates.list().iter().any(|record| matches!(record.status, HarnessUpdateStatus::Updating))
        || state.session_imports.running() > 0;
    let now = Utc::now();
    let latest_event = state.store.events_latest_at()?;
    let idle_since = {
        let mut idle_since = state.idle_since.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        *idle_since = next_idle_since(*idle_since, is_busy(&activity, updating), latest_event, now);
        *idle_since
    };
    let may_exit = idle_exit_allowed(allow_idle_exit, crate::login_service::wanted(state));
    if may_exit && idle_exit_due(idle_since, policy.daemon_idle_exit(), now) {
        tracing::info!(idle_since = ?idle_since, "nothing has used the daemon; exiting until a client starts it again");
        state.shutdown.cancel();
    } else if idle_since.is_some()
        && let Some(port) = superseded(state)
    {
        tracing::info!(port, "another daemon took over this data directory and nothing here is in use; exiting");
        state.shutdown.cancel();
    }
    Ok(())
}

/// Runs `sweep` on `SWEEP_INTERVAL` until shutdown. `allow_idle_exit` is off
/// for daemons started with `--pair`, which wait for a device to connect.
pub async fn run(state: AppState, allow_idle_exit: bool) {
    let mut interval = tokio::time::interval(SWEEP_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick fires immediately; there is nothing to sweep yet.
    interval.tick().await;
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            _ = interval.tick() => {
                let policy = state.settings.get().background;
                if let Err(error) = sweep(&state, &policy, allow_idle_exit).await {
                    tracing::warn!(%error, "background sweep failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_exit_waits_for_the_full_limit_and_never_fires_when_off() {
        let since = Utc::now() - chrono::Duration::minutes(9);
        let limit = Some(Duration::from_secs(600));
        assert!(!idle_exit_due(Some(since), limit, Utc::now()));
        assert!(idle_exit_due(Some(since - chrono::Duration::minutes(2)), limit, Utc::now()));
        assert!(!idle_exit_due(Some(since - chrono::Duration::hours(5)), None, Utc::now()));
        assert!(!idle_exit_due(None, limit, Utc::now()));
    }

    #[test]
    fn work_between_sweeps_pushes_the_idle_clock_forward() {
        let now = Utc::now();
        let earlier = now - chrono::Duration::minutes(5);
        let old_event = now - chrono::Duration::minutes(20);
        assert_eq!(next_idle_since(None, true, Some(now), now), None, "busy daemons have no idle clock");
        assert_eq!(next_idle_since(None, false, None, now), Some(now), "a fresh idle clock starts at this sweep");
        assert_eq!(next_idle_since(Some(earlier), false, Some(old_event), now), Some(earlier), "older events do not move it");
        let turn_finished = now - chrono::Duration::seconds(20);
        assert_eq!(
            next_idle_since(Some(earlier), false, Some(turn_finished), now),
            Some(turn_finished),
            "a turn since the last sweep restarts it"
        );
    }

    #[test]
    fn a_daemon_is_superseded_only_when_the_port_file_names_another_one() {
        assert_eq!(superseded_by(Some("4200\n"), 4199), Some(4200));
        assert_eq!(superseded_by(Some("4199"), 4199), None, "our own port");
        assert_eq!(superseded_by(None, 4199), None, "no port file yet");
        assert_eq!(superseded_by(Some("garbage"), 4199), None);
        assert_eq!(superseded_by(Some("0"), 4199), None);
        assert_eq!(superseded_by(Some("4200"), 0), None, "not listening yet");
    }

    #[test]
    fn a_daemon_that_keeps_its_machine_reachable_is_never_idle_exited() {
        assert!(idle_exit_allowed(true, false));
        assert!(!idle_exit_allowed(true, true), "the network needs it");
        assert!(!idle_exit_allowed(false, false), "a --pair daemon waits for its device");
    }

    #[test]
    fn any_client_agent_terminal_or_queued_work_keeps_the_daemon_busy() {
        let quiet = DaemonActivity::default();
        assert!(!is_busy(&quiet, false));
        assert!(is_busy(&quiet, true));
        assert!(is_busy(&DaemonActivity { connections: 1, ..quiet.clone() }, false));
        assert!(is_busy(&DaemonActivity { live_sessions: 1, ..quiet.clone() }, false));
        assert!(is_busy(&DaemonActivity { running_threads: 1, ..quiet.clone() }, false));
        assert!(is_busy(&DaemonActivity { terminals: 1, ..quiet.clone() }, false));
        assert!(is_busy(&DaemonActivity { queued_messages: 1, ..quiet }, false));
    }
}
