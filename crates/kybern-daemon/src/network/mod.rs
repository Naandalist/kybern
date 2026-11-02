//! The machine network (ADE-86, design: `docs/design/machine-network.md`).
//!
//! * **Identity and membership (N2)**: per-OS machine keys ([`keys`]), the signed membership
//!   operations and the rules that validate them ([`ops`]), and the in-process sync functions
//!   ([`membership`]).
//! * **Discovery and transport (N3)**: the Tailscale seam ([`tailscale`]), the well-known identity
//!   endpoint ([`well_known`]), daemon-to-daemon sessions and their short-lived grants
//!   ([`session`]), the peer manager with its heartbeat and backoff ([`peers`]) and the relay
//!   ([`relay`]).
//!
//! [`Network`] is the one handle both halves hang off. It is created in `AppState::initialize`
//! and lives on `AppState::network`; [`peers::run`] is its background worker. The joining flows,
//! removal side effects and the `network.*` handlers (N4) use [`Network::membership`],
//! [`Network::publish_ops`], [`peers::Peers`] and [`Network::found`].

pub mod broker;
pub mod client_grant;
pub mod clipboard;
pub mod clipboard_filter;
pub mod dial_out;
pub mod doctor;
pub mod federated;
pub mod files_push;
pub mod forward;
pub mod git_transfer;
pub mod join;
pub mod keys;
pub mod machine_info;
pub mod manage;
pub mod membership;
pub mod ops;
pub mod outbox;
pub mod peers;
pub mod plugin_sync;
pub mod ports;
pub mod provision;
pub mod relay;
pub mod relay_pages;
pub mod removal;
pub mod repo_identity;
pub mod session;
pub mod skill_sync;
pub mod spaces;
pub mod status;
pub mod tailscale;
pub mod updates;
pub mod wake_watch;
pub mod well_known;

#[cfg(all(test, unix))]
mod broker_tests;
#[cfg(test)]
mod gap_tests;
#[cfg(test)]
pub(crate) mod harness;
#[cfg(test)]
mod join_tests;
#[cfg(test)]
mod plugin_sync_tests;
#[cfg(test)]
mod recovery_tests;
#[cfg(test)]
mod shared_state_tests;
#[cfg(test)]
mod skill_sync_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod utilities_tests;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use kybern_protocol::{OpEnvelope, Scope, features};
use kybern_store::Store;
use tokio::sync::{Notify, broadcast};

#[allow(unused_imports)]
pub use keys::MachineKeys;
#[allow(unused_imports)]
pub use membership::{ApplyOutcome, IdentityReset, Membership, MembershipError, SelfInfo, previous_machine_ids, start};
#[allow(unused_imports)]
pub use ops::{OpError, Projection, Rejection};
pub use tailscale::Tailscale;

/// The network's HTTP routes, served on every listener (§3.2, §3.3, §3.4): the well-known identity
/// endpoint, the session handshake and the relay.
pub fn routes() -> axum::Router<crate::state::AppState> {
    use axum::routing::{any, get, post};
    axum::Router::new()
        .route(well_known::PATH, get(well_known::serve))
        .route(session::SESSION_PATH, post(session::serve))
        .route(join::OFFER_PATH, post(join::serve_offer))
        .route(join::MERGE_PATH, post(join::serve_merge))
        .route("/relay/{machine_id}/ws", get(relay::serve_ws))
        .route("/relay/{machine_id}/http/{*path}", any(relay::serve_http))
        .route("/relay/{machine_id}/page/{ticket}/{*path}", any(relay::serve_page))
}

/// Tracing target for the network (§10.3). Sub-targets are `::discovery`, `::session`, `::relay`.
pub const TARGET: &str = "kybern::network";

/// Every interval the network runs on. The defaults are the design's numbers (§3.1, §3.3, §3.4);
/// tests use [`Timing::fast`].
#[derive(Debug, Clone)]
pub struct Timing {
    /// A WebSocket ping on every session.
    pub ping_every: Duration,
    /// A session with no frame for this long is dead (three missed pings).
    pub dead_after: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    /// The soonest a member that Tailscale lists offline is dialed again after a failed attempt.
    pub offline_retry: Duration,
    /// The discovery tick while no client shows Settings → Machines.
    pub tick_idle: Duration,
    /// The discovery tick while a client does.
    pub tick_watching: Duration,
    /// How long a client counts as showing Machines after its last `network.status`.
    pub watching_for: Duration,
    pub grant_ttl: Duration,
    pub renew_after: Duration,
    /// How long a probe result is reused per node.
    pub found_ttl: Duration,
    /// A relayed stream with no traffic for this long is closed, the main `/ws` stream excepted.
    pub relay_idle: Duration,
    /// A port forward with no connection for this long closes (§8.3).
    pub ports_idle: Duration,
    /// The window of a handshake request's `at`, and of a well-known proof.
    pub clock_window: Duration,
    /// Join machines of the same Tailscale user on discovery (§2.4a). Always on in a daemon; a test
    /// that builds its network by hand turns it off.
    pub auto_join: bool,
    /// How long a lone machine waits for a lone same-owner machine with a smaller id to found the
    /// network before it founds one itself (§2.4a). The other one may have its network turned off, or
    /// not run Kybern at all by now; a network founded twice is reconciled by the merge path.
    pub lone_wait: Duration,
    /// How often the wall clock is compared with the timer, to notice that the machine slept
    /// ([`wake_watch`]).
    pub resume_tick: Duration,
    /// How often Tailscale's status is read while a member is not linked, to notice it came back
    /// without waiting for the discovery tick or a long backoff ([`wake_watch`]).
    pub status_poll: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            ping_every: Duration::from_secs(30),
            dead_after: Duration::from_secs(95),
            backoff_min: Duration::from_secs(2),
            backoff_max: Duration::from_secs(300),
            offline_retry: Duration::from_secs(30),
            tick_idle: Duration::from_secs(5 * 60),
            tick_watching: Duration::from_secs(60),
            watching_for: Duration::from_secs(90),
            grant_ttl: Duration::from_secs(60 * 60),
            renew_after: Duration::from_secs(30 * 60),
            found_ttl: Duration::from_secs(10 * 60),
            relay_idle: Duration::from_secs(30),
            ports_idle: Duration::from_secs(kybern_protocol::PORT_FORWARD_IDLE_SECS),
            clock_window: Duration::from_secs(60),
            auto_join: true,
            // Two discovery ticks at the idle pace, so a machine that is merely slow to notice us
            // still founds before this one gives up.
            lone_wait: Duration::from_secs(12 * 60),
            resume_tick: Duration::from_secs(5),
            status_poll: Duration::from_secs(30),
        }
    }
}

impl Timing {
    /// Intervals short enough for a test to watch a heartbeat or a backoff.
    pub fn fast() -> Self {
        Self {
            ping_every: Duration::from_millis(150),
            dead_after: Duration::from_millis(600),
            backoff_min: Duration::from_millis(100),
            backoff_max: Duration::from_millis(800),
            offline_retry: Duration::from_millis(300),
            tick_idle: Duration::from_millis(400),
            tick_watching: Duration::from_millis(400),
            watching_for: Duration::from_secs(1),
            grant_ttl: Duration::from_secs(60 * 60),
            renew_after: Duration::from_secs(30 * 60),
            found_ttl: Duration::from_millis(300),
            relay_idle: Duration::from_secs(30),
            ports_idle: Duration::from_millis(900),
            clock_window: Duration::from_secs(60),
            auto_join: true,
            lone_wait: Duration::from_secs(3),
            // Long enough that a loaded test runner's stall (four ticks) is not taken for a sleep.
            resume_tick: Duration::from_secs(1),
            status_poll: Duration::from_millis(300),
        }
    }
}

/// What the network listener is doing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListenInfo {
    /// Where the Tailscale listener is bound.
    pub addr: Option<SocketAddr>,
    /// Why it is not, when the network is on but nothing is bound.
    pub error: Option<String>,
}

/// This node's Tailscale identity, as the descriptor and the well-known answer report it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelfTs {
    pub node_id: Option<String>,
    pub name: Option<String>,
    pub tailnet: Option<String>,
    pub login: Option<String>,
}

pub struct Network {
    membership: Option<Arc<Membership>>,
    /// Why the network is unavailable (the key file could not be read), when it is.
    pub unavailable: Option<String>,
    pub tailscale: Arc<dyn Tailscale>,
    pub timing: Timing,
    pub grants: session::Grants,
    pub replay: session::ReplayCache,
    pub limiter: well_known::RateLimiter,
    pub peers: peers::Peers,
    pub relays: relay::RelayHub,
    /// Page tickets this machine minted for devices that open pages of other machines through it (ADE-99).
    pub pages: relay_pages::RelayPages,
    /// Sends this machine is receiving (M4, U1).
    pub files: files_push::Incoming,
    /// Ports this machine forwards from other machines (M4, U3).
    pub ports: ports::Forwards,
    /// This machine's clipboard, explicit sends and the universal clipboard (M4, U2).
    pub clipboard: clipboard::Clipboard,
    /// Projects this machine is adding to other machines, by operation.
    pub provisions: provision::Tracker,
    /// The client every dial and probe uses. Under the fake it is bound to this node's loopback
    /// address, so the peer's `whois` can name the caller.
    pub http: reqwest::Client,
    /// Retry bookkeeping for the joining pass (N4).
    pub join: join::JoinState,
    store: Store,
    pub(crate) spaces_lock: tokio::sync::Mutex<()>,
    /// The daemon's token-revocation channel, so tokens revoked by a removal close their sockets.
    revoked: OnceLock<broadcast::Sender<uuid::Uuid>>,
    listen: Mutex<ListenInfo>,
    self_ts: Mutex<Option<(Instant, SelfTs)>>,
    watching: Mutex<Option<Instant>>,
    nudged: Mutex<Option<Instant>>,
    /// Features that come and go while the daemon runs (computer use is listed only while this
    /// machine's tools are ready). Appended to [`Network::features`] after the static list.
    dynamic_features: Mutex<std::collections::BTreeSet<&'static str>>,
    wake: Notify,
    changed: broadcast::Sender<()>,
    /// Outbox items as they change, forwarded to clients as `outbox.changed`.
    outbox_changed: broadcast::Sender<kybern_protocol::OutboxItem>,
    /// Notifications of the machine utilities (file progress, received files, port forwards) for clients.
    utility_events: broadcast::Sender<(&'static str, serde_json::Value)>,
    enabled: AtomicBool,
    pub(crate) enabled_change: tokio::sync::Mutex<()>,
}

impl Network {
    /// Open the machine's identity and membership (N2) and the empty transport state. A key file
    /// that cannot be read leaves the network unavailable rather than stopping the daemon.
    pub fn new(data_dir: &Path, store: &Store, machine_id: &str, tailscale: Arc<dyn Tailscale>, timing: Timing) -> Self {
        let (membership, unavailable) = match membership::start(data_dir, store, machine_id) {
            Ok(membership) => (Some(Arc::new(membership)), None),
            Err(error) => {
                tracing::error!(target: TARGET, %error, "the machine network is unavailable: this machine's key could not be opened");
                (None, Some(format!("{error:#}")))
            }
        };
        let mut http = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none());
        if let Some(source) = tailscale.dial_source() {
            http = http.local_address(source);
        }
        Self {
            membership,
            unavailable,
            http: http.build().unwrap_or_default(),
            join: Default::default(),
            store: store.clone(),
            spaces_lock: Default::default(),
            revoked: OnceLock::new(),
            tailscale,
            timing,
            grants: Default::default(),
            replay: Default::default(),
            limiter: Default::default(),
            peers: Default::default(),
            relays: Default::default(),
            pages: Default::default(),
            files: Default::default(),
            ports: ports::Forwards::new(data_dir),
            clipboard: Default::default(),
            provisions: Default::default(),
            listen: Default::default(),
            self_ts: Default::default(),
            watching: Default::default(),
            nudged: Default::default(),
            dynamic_features: Default::default(),
            wake: Notify::new(),
            changed: broadcast::channel(16).0,
            outbox_changed: broadcast::channel(64).0,
            utility_events: broadcast::channel(64).0,
            enabled: AtomicBool::new(true),
            enabled_change: Default::default(),
        }
    }

    /// Connect the daemon's token-revocation channel (`AppState` calls this once).
    pub fn attach_revocations(&self, sender: broadcast::Sender<uuid::Uuid>) {
        let _ = self.revoked.set(sender);
    }

    /// Tell the connection layer a token was revoked, so its sockets close at once.
    pub(crate) fn announce_revoked(&self, id: uuid::Uuid) {
        if let Some(sender) = self.revoked.get() {
            let _ = sender.send(id);
        }
    }

    /// The membership, `None` when the network is unavailable.
    pub fn membership(&self) -> Option<&Arc<Membership>> {
        self.membership.as_ref()
    }

    /// This machine's id, `None` when the network is unavailable.
    pub fn machine_id(&self) -> Option<String> {
        self.membership.as_ref().map(|membership| membership.machine_id())
    }

    /// The features this build lists in `DaemonInfo.features` and in presence.
    pub fn features(&self) -> Vec<String> {
        if self.membership.is_none() {
            return Vec::new();
        }
        let mut list = vec![
            features::NETWORK_V1.to_string(),
            features::SPACES_SYNC_V1.to_string(),
            features::RELAY_V1.to_string(),
            features::RELAY_PAGE_V1.to_string(),
            features::DELEGATE_REMOTE_V1.to_string(),
            features::TOOLS_FORWARD_V1.to_string(),
            features::FORK_V1.to_string(),
            features::MOVE_V1.to_string(),
            features::SETTINGS_FANOUT_V1.to_string(),
            features::NOTES_FEDERATED_V1.to_string(),
            features::SECRETS_STORE_V1.to_string(),
            features::LOGIN_BROKER_V1.to_string(),
            features::UPDATE_V1.to_string(),
            features::FILES_PUSH_V1.to_string(),
            features::PORTS_V1.to_string(),
            features::PORTS_DIRECT_V1.to_string(),
            features::PORTS_DIRECT_V2.to_string(),
            features::RELAY_FRAMES_V2.to_string(),
            features::CLIPBOARD_V1.to_string(),
            features::GROUP_V1.to_string(),
            features::PROVISION_V1.to_string(),
            features::SKILLS_SYNC_V1.to_string(),
            features::PLUGINS_SYNC_V1.to_string(),
        ];
        list.extend(self.lock(&self.dynamic_features).iter().map(|feature| feature.to_string()));
        list
    }

    /// List or drop a feature that depends on this machine's state right now. Presence carries the
    /// change at its next heartbeat.
    pub fn set_dynamic_feature(&self, feature: &'static str, on: bool) {
        let changed = {
            let mut list = self.lock(&self.dynamic_features);
            if on { list.insert(feature) } else { list.remove(feature) }
        };
        if changed {
            self.wake();
        }
    }

    /// `settings.network.enabled`, as the worker last saw it.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, enabled: bool) {
        if self.enabled.swap(enabled, Ordering::Relaxed) != enabled {
            self.wake();
        }
    }

    /// Run the worker's next pass now: after the settings changed, after membership changed, after
    /// a session dropped.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    pub(crate) async fn woken(&self) {
        self.wake.notified().await;
    }

    /// A signal for N4's `network.changed` notification: the members, the found list or a link
    /// changed.
    pub fn subscribe_changes(&self) -> broadcast::Receiver<()> {
        self.changed.subscribe()
    }

    /// Outbox items as they change.
    pub fn subscribe_outbox(&self) -> broadcast::Receiver<kybern_protocol::OutboxItem> {
        self.outbox_changed.subscribe()
    }

    /// Notifications of the machine utilities, forwarded to clients that may read orchestration state.
    pub fn subscribe_utility(&self) -> broadcast::Receiver<(&'static str, serde_json::Value)> {
        self.utility_events.subscribe()
    }

    pub(crate) fn notify_utility(&self, method: &'static str, params: serde_json::Value) {
        let _ = self.utility_events.send((method, params));
    }

    pub(crate) fn notify_outbox(&self, item: kybern_protocol::OutboxItem) {
        let _ = self.outbox_changed.send(item);
    }

    pub(crate) fn notify_changed(&self) {
        let _ = self.changed.send(());
    }

    /// Someone on the tailnet asked this machine who it is. If it is a Kybern machine that has just
    /// started, it wants to be found: look at the tailnet again now. This is how a machine that starts
    /// after its peer gets noticed without waiting a tick. `node_id` is the caller's tailnet node when
    /// known: only that node is probed again, and each node nudges at most every few seconds, so the
    /// identity checks of members' sessions do not hide the nudge of a new machine. A member that nudges
    /// is back from a restart: the wait its failed attempts earned is over, and it is dialed now.
    pub fn nudge_discovery(&self, node_id: Option<&str>) {
        let every = Duration::from_secs(10).min(self.timing.tick_watching);
        let Some(node_id) = node_id else {
            let mut last = self.lock(&self.nudged);
            if last.is_some_and(|at| at.elapsed() < every) {
                return;
            }
            *last = Some(Instant::now());
            drop(last);
            self.peers.forget_probes(None);
            self.wake();
            return;
        };
        if let Some(machine_id) = self.member_at_node(node_id) {
            if self.peers.nudge_due(&format!("member:{node_id}"), every.min(Duration::from_secs(2))) {
                self.peers.reset_pair(&machine_id);
                self.wake();
            }
            return;
        }
        if self.peers.nudge_due(node_id, every) {
            self.peers.forget_probes(Some(node_id));
            self.peers.expect_node(node_id);
            self.wake();
        }
    }

    /// The member that runs on a tailnet node.
    fn member_at_node(&self, node_id: &str) -> Option<String> {
        let projection = self.membership.as_ref()?.projection();
        projection.members().find(|(_, entry)| entry.descriptor.ts_node_id.as_deref() == Some(node_id)).map(|(id, _)| id.clone())
    }

    /// Close the session with a member and revoke the grants it holds; the member reconnects with a new
    /// grant that reflects this machine's policy now.
    pub(crate) fn drop_session(&self, machine_id: &str) {
        self.peers.close(machine_id);
        for id in self.grants.revoke_machine(machine_id) {
            self.announce_revoked(id);
        }
    }

    /// A client shows Settings → Machines or the Found list: discovery ticks every 60 s for a
    /// while (§3.1).
    pub fn touch_watching(&self) {
        *self.lock(&self.watching) = Some(Instant::now());
        self.wake();
    }

    pub fn is_watching(&self) -> bool {
        self.lock(&self.watching).is_some_and(|at| at.elapsed() < self.timing.watching_for)
    }

    pub fn listen_info(&self) -> ListenInfo {
        self.lock(&self.listen).clone()
    }

    pub(crate) fn set_listen(&self, info: ListenInfo) {
        let mut slot = self.lock(&self.listen);
        if *slot != info {
            *slot = info;
            drop(slot);
            self.notify_changed();
        }
    }

    /// The port the Tailscale listener holds, once bound.
    pub fn listening_port(&self) -> Option<u16> {
        self.lock(&self.listen).addr.map(|addr| addr.port())
    }

    fn lock<'a, T>(&self, mutex: &'a Mutex<T>) -> MutexGuard<'a, T> {
        mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// This node's Tailscale identity, cached for a minute: it names the node in descriptors and
    /// well-known answers, and `tailscale status` is a child process.
    pub async fn self_ts(&self) -> SelfTs {
        if let Some((at, ts)) = &*self.lock(&self.self_ts)
            && at.elapsed() < Duration::from_secs(60)
        {
            return ts.clone();
        }
        let ts = match self.tailscale.status().await {
            Ok(status) if status.running => SelfTs {
                node_id: status.self_node.as_ref().map(|node| node.node_id.clone()).filter(|id| !id.is_empty()),
                name: status.self_node.as_ref().map(|node| if node.dns_name.is_empty() { node.name() } else { node.dns_name.clone() }),
                login: status.login_of(status.self_node.as_ref().and_then(|node| node.user_id)).map(str::to_string),
                tailnet: status.tailnet,
            },
            _ => return SelfTs::default(),
        };
        *self.lock(&self.self_ts) = Some((Instant::now(), ts.clone()));
        ts
    }

    /// What this machine says about itself in the operations it writes and the well-known answer.
    pub async fn self_info(&self) -> SelfInfo {
        let ts = self.self_ts().await;
        SelfInfo {
            name: crate::rpc::hostname(),
            os: std::env::consts::OS.to_string(),
            ts_node_id: ts.node_id,
            ts_name: ts.name,
            port: self.listening_port().filter(|port| *port != kybern_protocol::DEFAULT_PORT),
        }
    }

    /// The capabilities this machine lets `caller` use right now (§4.2): `machine:peer` always, the
    /// rest from this machine's `allow.<self>.<capability>` registers, none but `machine:peer` for
    /// a paused or non-member caller.
    pub fn policy_scopes(&self, caller: &str) -> Vec<Scope> {
        let Some(membership) = &self.membership else { return Vec::new() };
        let projection = membership.projection();
        let Some(entry) = projection.entry(caller).filter(|entry| entry.removed.is_none()) else { return Vec::new() };
        if entry.paused {
            return vec![Scope::MachinePeer];
        }
        let allowed = projection.allowed(&membership.machine_id());
        Scope::MACHINE
            .into_iter()
            .filter(|scope| match scope.capability() {
                None => true,
                Some(capability) => allowed.get(capability).copied().unwrap_or_else(|| Scope::MACHINE_DEFAULT.contains(scope)),
            })
            .collect()
    }

    /// Send operations this machine just created to every connected member (§2.3): the way N4's
    /// handlers spread an Add, a Remove or a register.
    pub fn publish_ops(&self, ops: Vec<OpEnvelope>) {
        self.peers.gossip(ops, None);
        self.close_unpermitted_streams();
        self.notify_changed();
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};

    /// A directory under the system temp directory that is removed on drop. The daemon's
    /// dev-dependencies carry no `tempfile`, and these tests must never touch `~/.kybern`.
    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!("kybern-net-{label}-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
