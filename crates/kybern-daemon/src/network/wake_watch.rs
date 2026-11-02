//! Prompt reconnects (ADE-119). The peer manager backs off up to five minutes from a member that does
//! not answer, and its discovery tick is five minutes when no client shows Machines. Two things cut
//! that short:
//!
//! * **Waking from sleep.** A timer that fires every [`Timing::resume_tick`] compares the wall clock
//!   with its own pace: a gap far longer than the tick means the machine slept (a stalled process
//!   looks the same, and costs one early pass). Then every wait ends, the members that dial this
//!   machine are nudged at once, and a few passes follow over the next minute while Tailscale
//!   reconnects. The woken machine reaching out is what makes it show online on the others within
//!   seconds: they reset that member's wait when it nudges or dials them.
//! * **Tailscale peer changes.** While some member is not linked, Tailscale's status is read every
//!   [`Timing::status_poll`] (three times that on battery). When who is online changed since the
//!   last pass, the worker runs now, and the pass resets the backoff as before. A member that is
//!   really off costs one status read per poll and no connection attempts beyond the usual backoff.

use std::time::{Duration, Instant, SystemTime};

use super::Timing;
use super::peers;
use crate::state::AppState;

const TARGET: &str = "kybern::network::session";

/// How much later than its tick the timer may come before it counts as a wake from sleep.
pub fn resumed(wall_elapsed: Option<Duration>, tick: Duration) -> bool {
    // `None`: the clock was set back; that is not a sleep.
    wall_elapsed.is_some_and(|elapsed| elapsed > tick.saturating_mul(4))
}

/// The passes after a wake, from the wake: Tailscale needs a few seconds to reconnect, and the first
/// pass may run before it has.
pub fn resume_passes(timing: &Timing) -> Vec<Duration> {
    [1u32, 3, 8, 20, 45].iter().map(|step| timing.backoff_min.saturating_mul(*step)).collect()
}

/// How often Tailscale is read while a member is not linked.
pub fn status_poll(timing: &Timing, on_battery: bool) -> Duration {
    if on_battery { timing.status_poll.saturating_mul(3) } else { timing.status_poll }
}

impl super::Network {
    /// This machine woke from sleep: retry everyone now and a few times more while Tailscale reconnects.
    pub fn after_resume(self: &std::sync::Arc<Self>) {
        self.peers.after_resume();
        self.wake();
        let network = self.clone();
        tokio::spawn(async move {
            let started = tokio::time::Instant::now();
            for at in resume_passes(&network.timing) {
                tokio::time::sleep_until(started + at).await;
                network.peers.after_resume();
                network.wake();
            }
        });
    }
}

/// Members other than this machine that are not paused, when the network is on and this machine is in it.
fn reachable_members(state: &AppState) -> Vec<String> {
    let network = &state.network;
    let Some(membership) = network.membership().filter(|membership| network.is_enabled() && membership.self_is_member()) else {
        return Vec::new();
    };
    let me = membership.machine_id();
    membership.projection().members().filter(|(id, entry)| **id != me && !entry.paused).map(|(id, _)| id.clone()).collect()
}

/// Read Tailscale once and wake the worker when who is online changed since the last pass.
async fn poll_tailnet(state: &AppState) {
    let network = &state.network;
    let members = reachable_members(state);
    if !network.peers.any_unlinked(&members) {
        return;
    }
    let Ok(status) = network.tailscale.status().await else { return };
    if status.running && network.peers.signature_changed(&peers::signature(&status)) {
        tracing::debug!(target: TARGET, "the tailnet changed while a member was away; looking now");
        network.wake();
    }
}

/// Runs until the daemon shuts down.
pub async fn run(state: AppState) {
    let network = state.network.clone();
    let tick = network.timing.resume_tick;
    let mut interval = tokio::time::interval(tick);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await;
    let mut last_wall = SystemTime::now();
    let mut next_poll = Instant::now() + network.timing.status_poll;
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            _ = interval.tick() => {}
        }
        let now = SystemTime::now();
        let elapsed = now.duration_since(last_wall).ok();
        last_wall = now;
        if resumed(elapsed, tick) {
            if network.is_enabled() {
                tracing::info!(target: TARGET, slept = ?elapsed.unwrap_or_default(), "this machine woke up; reconnecting to the other machines now");
                network.after_resume();
            }
            next_poll = Instant::now() + network.timing.status_poll;
            continue;
        }
        if Instant::now() >= next_poll {
            let on_battery = *state.on_battery.read().unwrap_or_else(|poisoned| poisoned.into_inner()) == Some(true);
            next_poll = Instant::now() + status_poll(&network.timing, on_battery);
            poll_tailnet(&state).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::harness::{Cluster, NodeSpec, WAIT, eventually, linked};
    use kybern_protocol::JoinVia;

    #[test]
    fn a_wall_clock_far_ahead_of_the_timer_is_a_wake_from_sleep() {
        let tick = Duration::from_secs(5);
        assert!(!resumed(Some(Duration::from_secs(5)), tick));
        assert!(!resumed(Some(Duration::from_secs(19)), tick), "a busy runtime is late, not asleep");
        assert!(resumed(Some(Duration::from_secs(21)), tick));
        assert!(resumed(Some(Duration::from_secs(8 * 60 * 60)), tick), "a night asleep");
        assert!(!resumed(None, tick), "the clock was set back");
    }

    #[test]
    fn after_a_wake_the_passes_come_soon_and_stop_within_two_minutes() {
        let passes = resume_passes(&Timing::default());
        assert_eq!(passes.first(), Some(&Duration::from_secs(2)));
        assert!(passes.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(*passes.last().unwrap() <= Duration::from_secs(120), "no reconnect storm: a few passes, then the usual backoff");
        assert_eq!(status_poll(&Timing::default(), false), Duration::from_secs(30));
        assert_eq!(status_poll(&Timing::default(), true), Duration::from_secs(90));
    }

    /// Slow enough that only the wake can explain a quick reconnect: a growing backoff and a long tick.
    fn sleepy(timing: &mut Timing) {
        timing.backoff_min = Duration::from_secs(4);
        timing.backoff_max = Duration::from_secs(120);
        timing.offline_retry = Duration::from_secs(120);
        timing.tick_idle = Duration::from_secs(300);
        timing.tick_watching = Duration::from_secs(300);
        timing.found_ttl = Duration::from_secs(300);
        // The tests wake the machine by hand; ticks this long never fire on their own.
        timing.resume_tick = Duration::from_secs(600);
        timing.status_poll = Duration::from_secs(600);
    }

    /// `sleeper` slept: its listener and links went away, and the other side, when it is the one that
    /// dials, failed until its wait grew long. The sleeper's settings come back without waking its
    /// worker, as a frozen process's would, and nothing reconnects until it notices the wake.
    async fn sleeps_then_wakes(sleeper_id: &str, other_id: &str) {
        let Some(cluster) = Cluster::new(&[NodeSpec::owner("mac"), NodeSpec::owner("pc")]).map(|cluster| cluster.timing(sleepy)) else {
            return;
        };
        let other = cluster.start_as("mac", other_id, false).await;
        let sleeper = cluster.start_as("pc", sleeper_id, false).await;
        cluster.form(&other, &[&sleeper], JoinVia::Auto).await;
        sleeper.state.network.wake();
        eventually("linked before the sleep", WAIT, || linked(&other, &sleeper)).await;

        let mut settings = sleeper.state.settings.get();
        settings.network.enabled = false;
        sleeper.state.settings.set(settings.clone()).unwrap();
        sleeper.state.network.wake();
        eventually("the other side loses it", WAIT, || other.state.network.peers.link(&sleeper.id()).is_none()).await;
        // The links closing wake the sleeper's worker once more; let that pass run with the network still off.
        tokio::time::sleep(Duration::from_secs(1)).await;
        if other_id < sleeper_id {
            // 4 s after the first failure, 8 s after the second. (Failing to reach a closed port takes
            // seconds on Windows, so the count grows slowly there.)
            eventually("the dialer backs off", Duration::from_secs(45), || other.state.network.peers.status(&sleeper.id()).failures >= 2)
                .await;
        }
        settings.network.enabled = true;
        sleeper.state.settings.set(settings).unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!linked(&other, &sleeper), "nothing reconnects before the wake");

        let woke = Instant::now();
        sleeper.state.network.after_resume();
        eventually("linked again within seconds of the wake", Duration::from_secs(10), || linked(&other, &sleeper)).await;
        assert!(woke.elapsed() < Duration::from_secs(3), "{:?}", woke.elapsed());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_machine_that_dials_reconnects_within_seconds_of_waking() {
        sleeps_then_wakes("00000000-0000-7000-8000-000000000001", "ffffffff-ffff-7fff-8fff-ffffffffffff").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_machine_that_is_dialed_gets_its_dialer_back_within_seconds_of_waking() {
        sleeps_then_wakes("ffffffff-ffff-7fff-8fff-ffffffffffff", "00000000-0000-7000-8000-000000000001").await;
    }
}
