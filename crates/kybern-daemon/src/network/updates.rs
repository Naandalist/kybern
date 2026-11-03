//! Keeping the machines of a network on one version (design §5, §9.1; decision Q40).
//!
//! * **Update all machines** (`network.update_all`): this machine works out the newest version it can
//!   see (its own, every member's presence, the release feed) and asks each online member to update
//!   itself with `peer.update.start`. Each machine answers for itself: started, or why not. The result
//!   lists every member once, started or skipped with the reason.
//! * **Keep all machines on the same version** (`follow_updates`): a network register. While it is on,
//!   a member that sees a newer version on another member updates itself, once per version and only
//!   while nothing is running; it never interrupts a turn.
//!
//! A machine whose daemon the desktop app manages updates the app (ADE-125, `crate::app_update`): through the open
//! app, or by itself while the app is closed, always once nothing is running. A deb or rpm install says what to run
//! instead, and a development build is rebuilt rather than replaced. Machines on older versions still answer
//! "Updates with the app".
//!
//! Each machine keeps to its own `update_track`: a Stable machine asked to reach a dev build, or
//! following a member that runs one, looks at its own (stable) feed instead.

use std::sync::atomic::Ordering;
use std::time::Duration;

use kybern_protocol::methods::PeerUpdateStart;
use kybern_protocol::{
    MachineId, NetworkStatus, NetworkUpdateAllResult, NetworkUpdateSkipped, PeerUpdateStartParams, PeerUpdateStartResult, RpcError,
    UpdateTrack, codes, features,
};
use serde_json::json;

use super::TARGET;
use super::join::JoinError;
use crate::self_update::{self, CURRENT_VERSION, newer, on_track};
use crate::state::AppState;

/// How long one member gets to answer `peer.update.start`.
const ASK_BUDGET: Duration = Duration::from_secs(15);
/// How often a following machine looks at what the others run.
const FOLLOW_EVERY: Duration = Duration::from_secs(30);
/// The `meta` key that remembers the version a following machine already tried to reach.
const ATTEMPT_KEY: &str = "network_follow_attempt";

const ALREADY_CURRENT: &str = "Already up to date";

/// What a machine does with a request to update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Start,
    Skip(String),
}

/// Who replaces this machine's Kybern, and why it cannot when it cannot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Updater {
    /// The daemon replaces its own binary.
    Daemon { unsupported: Option<String> },
    /// The daemon belongs to the desktop app, which is updated instead.
    App { blocked: Option<String> },
}

/// The pure part of the answer: whether a machine at `current` on `track` starts for a request that wants `wanted`.
/// A version off its track counts as no version: the machine checks its own feed. A daemon that cannot replace itself
/// says so first, as before; a desktop app's machine already on the version says that first, so a deb or rpm install
/// is not told to install what it runs.
pub fn decide(updater: &Updater, current: &str, wanted: Option<&str>, track: UpdateTrack) -> Decision {
    let wanted = wanted.filter(|wanted| on_track(track, wanted));
    let current_already = wanted.is_some_and(|wanted| !newer(wanted, current));
    match updater {
        Updater::Daemon { unsupported: Some(reason) } => Decision::Skip(reason.clone()),
        _ if current_already => Decision::Skip(ALREADY_CURRENT.into()),
        Updater::App { blocked: Some(reason) } => Decision::Skip(reason.clone()),
        Updater::Daemon { unsupported: None } | Updater::App { blocked: None } => Decision::Start,
    }
}

/// `peer.update.start`, and this machine's own part of `network.update_all`: queue the update. The
/// updater itself waits for running work to finish and restarts the daemon when it is done.
pub async fn start_self(state: &AppState, wanted: Option<&str>) -> PeerUpdateStartResult {
    start(state, wanted, false).await
}

/// Queue an update nobody asked for: this machine follows another one. It also waits for terminals to close.
async fn start_following(state: &AppState, wanted: &str) -> PeerUpdateStartResult {
    start(state, Some(wanted), true).await
}

async fn start(state: &AppState, wanted: Option<&str>, automatic: bool) -> PeerUpdateStartResult {
    let track = state.settings.get().update_track;
    if state.desktop_managed.load(Ordering::SeqCst) {
        // A package install's answer names the release to install, so it needs a version even when none was asked for.
        let mut wanted = wanted.filter(|wanted| on_track(track, wanted)).map(str::to_string);
        if wanted.is_none() && matches!(crate::app_update::current_route(state), crate::app_update::Route::Package(_)) {
            wanted = self_update::feed_latest(state).await;
        }
        let updater = Updater::App { blocked: crate::app_update::blocked(state, wanted.as_deref()) };
        return match decide(&updater, CURRENT_VERSION, wanted.as_deref(), track) {
            Decision::Skip(reason) => PeerUpdateStartResult { started: false, reason: Some(reason) },
            Decision::Start => {
                crate::app_update::request(state, wanted, automatic);
                PeerUpdateStartResult { started: true, reason: None }
            }
        };
    }
    let updater = Updater::Daemon { unsupported: self_update::unsupported(state) };
    match decide(&updater, CURRENT_VERSION, wanted, track) {
        Decision::Skip(reason) => PeerUpdateStartResult { started: false, reason: Some(reason) },
        Decision::Start => {
            state.daemon_updates.request(&state.store);
            // A test only observes the request; the updater would go to the release feed.
            #[cfg(test)]
            let run = !state.daemon_updates.allow_development.load(Ordering::SeqCst);
            #[cfg(not(test))]
            let run = true;
            if run {
                tokio::spawn(self_update::tick(state.clone()));
            }
            PeerUpdateStartResult { started: true, reason: None }
        }
    }
}

/// The highest of two optional versions.
fn highest(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if newer(&b, &a) { b } else { a }),
        (a, None) => a,
        (None, b) => b,
    }
}

/// What the member `machine_id` last said it runs, from its open session or the cached presence.
fn presence_of(state: &AppState, machine_id: &str) -> Option<kybern_protocol::PeerPresence> {
    let live = state.network.peers.status(machine_id).presence;
    live.or_else(|| state.store.network_machine_get(machine_id).ok().flatten().and_then(|row| row.presence))
}

/// The newest version this machine can see: its own, every online member's and the release feed's.
async fn target_version(state: &AppState, members: &[MachineId], me: &str) -> Option<String> {
    let mut target = Some(CURRENT_VERSION.to_string());
    for id in members.iter().filter(|id| id.as_str() != me) {
        if state.network.peers.link(id).is_some()
            && let Some(presence) = presence_of(state, id)
        {
            target = highest(target, Some(presence.version));
        }
    }
    // The feed names the release everyone is moving to even when every member is still on the old one.
    highest(target, self_update::feed_latest(state).await)
}

/// `network.update_all`.
pub async fn update_all(state: &AppState) -> NetworkUpdateAllResult {
    let mut result = NetworkUpdateAllResult { started: Vec::new(), skipped: Vec::new() };
    let Some(membership) = state.network.membership() else { return result };
    let me = membership.machine_id();
    let members: Vec<MachineId> = membership.projection().members().map(|(id, _)| id.clone()).collect();
    let target = target_version(state, &members, &me).await;

    let mut asked = Vec::new();
    for id in &members {
        if *id == me {
            let answer = start_self(state, target.as_deref()).await;
            record(&mut result, id, answer);
            continue;
        }
        let Some(link) = state.network.peers.link(id) else {
            result.skipped.push(NetworkUpdateSkipped { machine_id: id.clone(), reason: "Offline".into() });
            continue;
        };
        let presence = presence_of(state, id);
        if let Some(presence) = &presence
            && target.as_deref().is_some_and(|target| !newer(target, &presence.version))
        {
            result.skipped.push(NetworkUpdateSkipped { machine_id: id.clone(), reason: ALREADY_CURRENT.into() });
            continue;
        }
        if let Some(presence) = &presence
            && !presence.features.is_empty()
            && !presence.features.iter().any(|feature| feature == features::UPDATE_V1)
        {
            result.skipped.push(NetworkUpdateSkipped { machine_id: id.clone(), reason: needs_newer(&presence.version) });
            continue;
        }
        asked.push((id.clone(), link));
    }

    // The members answer on their own sessions at the same time; one slow machine does not hold the rest.
    let answers = futures::future::join_all(asked.into_iter().map(|(id, link)| {
        let params = PeerUpdateStartParams { version: target.clone() };
        async move {
            let answer = tokio::time::timeout(ASK_BUDGET, link.call::<PeerUpdateStart>(params)).await;
            (id, answer)
        }
    }))
    .await;
    for (id, answer) in answers {
        match answer {
            Ok(Ok(answer)) => record(&mut result, &id, answer),
            Ok(Err(error)) => result.skipped.push(NetworkUpdateSkipped { machine_id: id, reason: describe_failure(&error) }),
            Err(_) => result.skipped.push(NetworkUpdateSkipped { machine_id: id, reason: "It did not answer in time".into() }),
        }
    }
    // Members in the order the network lists them, not the order their answers arrived.
    let position = |id: &MachineId| members.iter().position(|member| member == id).unwrap_or(usize::MAX);
    result.started.sort_by_key(position);
    result.skipped.sort_by_key(|skipped| position(&skipped.machine_id));
    result
}

fn needs_newer(version: &str) -> String {
    format!("Running Kybern {version}, which cannot be updated from here. Update it from its own Kybern")
}

fn describe_failure(error: &RpcError) -> String {
    match error.code {
        codes::METHOD_NOT_FOUND => "It runs an older Kybern that cannot be updated from here. Update it from its own Kybern".into(),
        codes::FORBIDDEN => "It does not let other machines change its settings".into(),
        _ => error.message.clone(),
    }
}

fn record(result: &mut NetworkUpdateAllResult, id: &MachineId, answer: PeerUpdateStartResult) {
    if answer.started {
        result.started.push(id.clone());
    } else {
        result
            .skipped
            .push(NetworkUpdateSkipped { machine_id: id.clone(), reason: answer.reason.unwrap_or_else(|| "It did not update".into()) });
    }
}

// ---- keep all machines on the same version ----

/// `network.follow_updates.set`: the register every member reads.
pub async fn set_follow(state: &AppState, enabled: bool) -> Result<NetworkStatus, JoinError> {
    let network = &state.network;
    let membership =
        network.membership().ok_or_else(|| JoinError::Unavailable("The machine network is unavailable on this machine.".into()))?;
    let op = membership.set("follow_updates", json!(enabled))?;
    network.publish_ops(vec![op]);
    network.wake();
    super::status::network_status(state)
        .await
        .ok_or_else(|| JoinError::Unavailable("The machine network is unavailable on this machine.".into()))
}

/// The newest version an online member runs that is newer than this machine's own and on its track.
fn newest_peer_version(state: &AppState) -> Option<String> {
    let versions = state.network.peers.links().into_iter().filter_map(|link| state.network.peers.status(&link.machine_id).presence);
    newest_to_follow(state.settings.get().update_track, CURRENT_VERSION, versions.map(|presence| presence.version))
}

/// Of the versions the members run, the newest a machine at `current` on `track` would follow.
fn newest_to_follow(track: UpdateTrack, current: &str, versions: impl IntoIterator<Item = String>) -> Option<String> {
    versions
        .into_iter()
        .filter(|version| newer(version, current) && on_track(track, version))
        .fold(None, |newest, version| highest(newest, Some(version)))
}

/// What a following machine does after looking at the others.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowStep {
    /// Nothing newer, or this version was already tried.
    Nothing,
    /// A newer version exists, but work is running: look again later without counting the attempt.
    Wait,
    /// Begin updating to this version.
    Start(String),
}

/// The rule behind following: a version is tried once, and never while work is running.
pub fn follow_step(newest: Option<&str>, attempted: Option<&str>, busy: bool) -> FollowStep {
    match newest {
        None => FollowStep::Nothing,
        Some(version) if attempted == Some(version) => FollowStep::Nothing,
        Some(_) if busy => FollowStep::Wait,
        Some(version) => FollowStep::Start(version.to_string()),
    }
}

/// One look at the other machines. Returns the version this machine began updating to, if it did.
/// It tries each version once, and waits (without marking the version tried) while a turn is running
/// or a message is queued, so an update never interrupts work.
pub async fn follow_tick(state: &AppState) -> Option<String> {
    let network = &state.network;
    let membership = network.membership()?;
    if !network.is_enabled() || !membership.self_is_member() || !membership.projection().follow_updates() {
        return None;
    }
    // The desktop app's daemon follows too, by updating the app; one that cannot (a deb or rpm, a development build)
    // stays out rather than try every version.
    let can_update = if state.desktop_managed.load(Ordering::SeqCst) {
        !matches!(crate::app_update::current_route(state), crate::app_update::Route::Package(_) | crate::app_update::Route::Manual(_))
    } else {
        self_update::unsupported(state).is_none()
    };
    if !can_update {
        return None;
    }
    let newest = newest_peer_version(state)?;
    let attempted = state.store.meta_get(ATTEMPT_KEY).ok().flatten();
    // A failed read of the activity counts as busy: better late than in the middle of a turn.
    let busy = self_update::busy(state, true).await.unwrap_or(true);
    let FollowStep::Start(version) = follow_step(Some(&newest), attempted.as_deref(), busy) else { return None };
    if let Err(error) = state.store.meta_set(ATTEMPT_KEY, &version) {
        tracing::warn!(target: TARGET, %error, "could not remember the version this machine is following");
        return None;
    }
    tracing::info!(target: TARGET, %version, "another machine runs a newer version; updating to follow it");
    let answer = start_following(state, &version).await;
    answer.started.then_some(version)
}

/// The background worker behind "Keep all machines on the same version".
pub async fn run(state: AppState) {
    let mut interval = tokio::time::interval(FOLLOW_EVERY);
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            _ = interval.tick() => {
                follow_tick(&state).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daemon(unsupported: Option<&str>) -> Updater {
        Updater::Daemon { unsupported: unsupported.map(str::to_string) }
    }

    fn app(blocked: Option<&str>) -> Updater {
        Updater::App { blocked: blocked.map(str::to_string) }
    }

    #[test]
    fn a_machine_decides_from_who_manages_it_and_what_is_wanted() {
        let stable = UpdateTrack::Stable;
        assert_eq!(
            decide(&daemon(Some("Development build; rebuild it instead.")), "0.19.0", None, stable),
            Decision::Skip("Development build; rebuild it instead.".into())
        );
        assert_eq!(decide(&daemon(None), "0.20.0", Some("0.20.0"), stable), Decision::Skip("Already up to date".into()));
        assert_eq!(decide(&daemon(None), "0.21.0", Some("0.20.0"), stable), Decision::Skip("Already up to date".into()));
        assert_eq!(decide(&daemon(None), "0.19.0", Some("0.20.0"), stable), Decision::Start);
        // Without a target version the machine looks at the release feed itself.
        assert_eq!(decide(&daemon(None), "0.19.0", None, stable), Decision::Start);
        // A daemon that cannot replace itself says so even when it is on the version.
        assert_eq!(decide(&daemon(Some("x")), "0.20.0", Some("0.20.0"), stable), Decision::Skip("x".into()));
    }

    #[test]
    fn a_desktop_app_machine_starts_through_the_app_unless_it_cannot() {
        let stable = UpdateTrack::Stable;
        // The open app, or the daemon for the closed app, installs it (ADE-125): no longer "Updates with the app".
        assert_eq!(decide(&app(None), "0.19.0", Some("0.20.0"), stable), Decision::Start);
        assert_eq!(decide(&app(None), "0.19.0", None, stable), Decision::Start);
        // A deb or rpm says what to run instead.
        let package = "Needs an update — download kybern-gpuix_0.20.0_x86_64.deb from the release page, then run sudo apt install ./kybern-gpuix_0.20.0_x86_64.deb";
        assert_eq!(decide(&app(Some(package)), "0.19.0", Some("0.20.0"), stable), Decision::Skip(package.into()));
        // A machine that is already there says so first, even one that could not update anyway.
        assert_eq!(decide(&app(Some(package)), "0.20.0", Some("0.20.0"), stable), Decision::Skip("Already up to date".into()));
        assert_eq!(decide(&app(None), "0.20.0", Some("0.20.0"), stable), Decision::Skip("Already up to date".into()));
        // Each keeps to its own track: a dev build is no target for a Stable app machine.
        assert_eq!(decide(&app(None), "0.20.0", Some("0.21.0-dev.2"), stable), Decision::Start);
    }

    #[test]
    fn a_machine_keeps_to_its_own_update_track() {
        // A dev build is no target for a Stable machine: it checks its own feed, which may still have a newer stable.
        assert_eq!(decide(&daemon(None), "0.20.0", Some("0.21.0-dev.2"), UpdateTrack::Stable), Decision::Start);
        assert_eq!(decide(&daemon(None), "0.21.0-dev.2", Some("0.21.0-dev.2"), UpdateTrack::Stable), Decision::Start);
        assert_eq!(
            decide(&daemon(None), "0.21.0-dev.2", Some("0.21.0-dev.2"), UpdateTrack::Dev),
            Decision::Skip("Already up to date".into())
        );
        assert_eq!(
            decide(&daemon(None), "0.21.0-dev.3", Some("0.21.0-dev.2"), UpdateTrack::Dev),
            Decision::Skip("Already up to date".into())
        );

        let members = || ["0.20.0".to_string(), "0.21.0-dev.2".to_string(), "0.20.1".to_string()];
        assert_eq!(newest_to_follow(UpdateTrack::Dev, "0.20.0", members()).as_deref(), Some("0.21.0-dev.2"));
        assert_eq!(
            newest_to_follow(UpdateTrack::Stable, "0.20.0", members()).as_deref(),
            Some("0.20.1"),
            "a Stable machine never follows a dev build"
        );
        assert_eq!(newest_to_follow(UpdateTrack::Stable, "0.21.0-dev.2", members()), None, "nor moves back to an older stable");
        assert_eq!(newest_to_follow(UpdateTrack::Stable, "0.21.0-dev.2", ["0.21.0".to_string()]).as_deref(), Some("0.21.0"));
    }

    #[test]
    fn a_following_machine_tries_each_version_once_and_never_while_busy() {
        assert_eq!(follow_step(None, None, false), FollowStep::Nothing);
        assert_eq!(follow_step(Some("0.20.0"), None, false), FollowStep::Start("0.20.0".into()));
        assert_eq!(follow_step(Some("0.20.0"), None, true), FollowStep::Wait, "work is running: wait, and do not count it");
        assert_eq!(follow_step(Some("0.20.0"), Some("0.20.0"), false), FollowStep::Nothing, "already tried this version");
        assert_eq!(follow_step(Some("0.20.0"), Some("0.20.0"), true), FollowStep::Nothing);
        assert_eq!(follow_step(Some("0.20.1"), Some("0.20.0"), false), FollowStep::Start("0.20.1".into()), "a newer one is a new attempt");
    }

    #[test]
    fn the_highest_version_wins() {
        assert_eq!(highest(Some("0.19.0".into()), Some("0.20.0".into())).as_deref(), Some("0.20.0"));
        assert_eq!(highest(Some("0.20.0".into()), Some("0.19.9".into())).as_deref(), Some("0.20.0"));
        assert_eq!(highest(None, Some("0.1.0".into())).as_deref(), Some("0.1.0"));
        assert_eq!(highest(Some("0.1.0".into()), None).as_deref(), Some("0.1.0"));
        assert_eq!(highest(None, None), None);
        // A version nobody can read never beats one that is real.
        assert_eq!(highest(Some("0.19.0".into()), Some("garbage".into())).as_deref(), Some("0.19.0"));
    }

    #[test]
    fn failures_are_described_for_the_machines_page() {
        assert!(describe_failure(&RpcError::new(codes::METHOD_NOT_FOUND, "x")).contains("older Kybern"));
        assert!(describe_failure(&RpcError::forbidden("machine:settings")).contains("settings"));
        assert_eq!(describe_failure(&RpcError::internal("the session is closed")), "the session is closed");
    }
}
