//! Machine network types: membership operations, machines, routes, the status
//! Settings → Machines reads, and the daemon-to-daemon `peer.*` payloads.
//!
//! The methods that use them are in `methods.rs`. Only M1 types live here;
//! the outbox, remote delegation, forwarded tools and the rest arrive with
//! their milestones.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::model::{PermissionMode, ProviderKind, ThreadId};

/// Version of the daemon-to-daemon protocol, echoed as `DaemonInfo.network_protocol`.
pub const NETWORK_PROTOCOL: u32 = 1;

/// Version written into every membership operation body (`body.v`).
pub const OP_VERSION: u32 = 1;

/// Domain separator signed in front of an operation body.
pub const OP_SIGNATURE_DOMAIN: &str = "kybern-network-op-v1\0";

/// Domain separator signed in front of a well-known proof.
pub const WELL_KNOWN_PROOF_DOMAIN: &str = "kybern-wk-v1\0";

/// Feature names a daemon lists in `DaemonInfo.features` and `peer.presence`.
pub mod features {
    pub const SPACES_SYNC_V1: &str = "spaces.sync.v1";
    pub const NETWORK_V1: &str = "network.v1";
    pub const RELAY_V1: &str = "relay.v1";
    pub const DELEGATE_REMOTE_V1: &str = "delegate.remote.v1";
    pub const TOOLS_FORWARD_V1: &str = "tools.forward.v1";
    pub const FORK_V1: &str = "fork.v1";
    pub const MOVE_V1: &str = "move.v1";
    pub const FILES_PUSH_V1: &str = "files.push.v1";
    pub const CLIPBOARD_V1: &str = "clipboard.v1";
    pub const PORTS_DIRECT_V1: &str = "ports.direct.v1";
    pub const PORTS_V1: &str = "ports.v1";
    /// The direct port transport's second handshake: an authorized socket waits for a mode byte, so a forward keeps
    /// sockets ready before the browser asks (pre-warmed), and one socket can carry an HTTP/2 connection that the
    /// target spreads over pooled HTTP/1.1 connections to its dev server (the Fast mode of a preview).
    pub const PORTS_DIRECT_V2: &str = "ports.direct.v2";
    /// A forward's HTTP/2 connection (mode `H`) also takes one batched revalidation request: the viewing machine
    /// sends the validators of the responses it keeps, this machine checks each against its dev server over
    /// loopback, and only changed bodies come back (ADE-126).
    pub const PORTS_CACHE_V1: &str = "ports.cache.v1";
    /// Port routes through another member (ADE-126): `peer.ports.hop` (this machine relays a member's forward
    /// connections to a third machine), `peer.ports.via` (this machine accepts a forward's connections relayed by
    /// a named member), and a timed throughput probe (mode `P`) on ready sockets.
    pub const PORTS_ROUTE_V1: &str = "ports.route.v1";
    /// Relay and port stream data travels as binary WebSocket frames, without base64 or JSON, and a stream's
    /// credit window is [`crate::RELAY_WINDOW_V2`] instead of 256 KiB.
    pub const RELAY_FRAMES_V2: &str = "relay.frames.v2";
    pub const SETTINGS_FANOUT_V1: &str = "settings.fanout.v1";
    pub const NOTES_FEDERATED_V1: &str = "notes.federated.v1";
    pub const SECRETS_STORE_V1: &str = "secrets.store.v1";
    pub const LOGIN_BROKER_V1: &str = "login.broker.v1";
    /// `peer.update.start`: another member can ask this machine to update itself.
    pub const UPDATE_V1: &str = "update.v1";
    /// A daemon the desktop app manages answers `peer.update.start` by updating the app: through an open window,
    /// or by itself while the app is closed (ADE-125). Older versions answer "Updates with the app" instead.
    pub const APP_UPDATE_V1: &str = "update.app.v1";
    /// A group project's thread (several repositories) can be forked, moved or delegated to this machine:
    /// `group` in `peer.fork`, `peer.move.prepare` and `peer.delegate`, `group_repos` in `peer.git.import`.
    pub const GROUP_V1: &str = "group.v1";
    /// `peer.provision.*`: a project can be added to this machine from another one (clone or bundle, env files).
    pub const PROVISION_V1: &str = "provision.v1";
    /// `peer.skills.*` and the `skill` destination of `peer.files.*`: this machine lists its user skills
    /// and takes skill copies.
    pub const PLUGINS_SYNC_V1: &str = "plugins.sync.v1";
    pub const SKILLS_SYNC_V1: &str = "skills.sync.v1";
    /// `network.relay_page.open`: this machine, as a relay member, serves ticket pages of another machine to a web
    /// view that cannot send `X-Kybern-Relay-Auth`, under `/relay/{machine_id}/page/{ticket}/…`.
    pub const RELAY_PAGE_V1: &str = "relay.page.v1";
    /// This machine's `kybern_computer_*` tools are set up and ready, so another machine's agents may
    /// pass `machine` to use apps here. Listed in presence only while that is true, so it is not part
    /// of [`ALL`].
    pub const COMPUTER_USE_V1: &str = "computer.use.v1";
    /// A machine that asks for a session after it was removed is answered with the signed operations that
    /// removed it (a 403 with a `RemovalNotice` body), so one that was offline at its removal learns of it.
    /// A session request lists it in its `features` to ask for that answer.
    pub const REMOVAL_PROOF_V1: &str = "removal.proof.v1";

    /// Every feature the network design names, in design order.
    pub const ALL: [&str; 27] = [
        NETWORK_V1,
        SPACES_SYNC_V1,
        RELAY_V1,
        DELEGATE_REMOTE_V1,
        TOOLS_FORWARD_V1,
        FORK_V1,
        MOVE_V1,
        FILES_PUSH_V1,
        CLIPBOARD_V1,
        PORTS_V1,
        PORTS_DIRECT_V1,
        PORTS_DIRECT_V2,
        PORTS_CACHE_V1,
        PORTS_ROUTE_V1,
        RELAY_FRAMES_V2,
        SETTINGS_FANOUT_V1,
        NOTES_FEDERATED_V1,
        SECRETS_STORE_V1,
        LOGIN_BROKER_V1,
        UPDATE_V1,
        APP_UPDATE_V1,
        GROUP_V1,
        PROVISION_V1,
        SKILLS_SYNC_V1,
        PLUGINS_SYNC_V1,
        RELAY_PAGE_V1,
        REMOVAL_PROOF_V1,
    ];
}

/// A machine's id: the daemon's `environment_id`, a UUIDv7 string.
pub type MachineId = String;

// ---- repository identity ----

/// What makes two checkouts the same logical project on different machines.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
pub struct RepoIdentity {
    /// The smallest hash in `git rev-list --max-parents=0 HEAD`. Absent for a shallow clone, whose
    /// "root" is only where the history was cut.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_commit: Option<String>,
    /// Every root commit, sorted. A repository with several roots (merged histories) matches a
    /// checkout that shares any of them. Empty from a daemon that predates this field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub root_commits: Vec<String>,
    /// The `origin` URL normalized to `host/owner/repo`: lowercase, no scheme, user or `.git`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
}

impl RepoIdentity {
    pub fn is_empty(&self) -> bool {
        self.root_commit.is_none() && self.root_commits.is_empty() && self.remote.is_none()
    }

    /// Every root commit known for this checkout, `root_commit` included.
    pub fn roots(&self) -> impl Iterator<Item = &String> {
        self.root_commit.iter().chain(self.root_commits.iter())
    }
}

/// One member repository of a group project, named as its people know it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GroupRepo {
    pub name: String,
    pub repo: RepoIdentity,
}

/// A member repository a group thread works in, and the commit that holds its work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GroupMemberWork {
    /// The member's path in the sender's group folder ("." is the group folder's own repository).
    pub path: String,
    pub name: String,
    pub repo: RepoIdentity,
    /// The snapshot of the member's work (a fork or move: its branch and uncommitted changes; a worktree
    /// delegation: the base). `None` when the receiver needs no commit (a shared workspace).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// What a group project's thread carries to another machine instead of a single repository.
///
/// The receiver looks for a group project made of exactly `repos` (the same rule clients merge by), then
/// maps every member of `members` to its checkout there by repository identity. A partial match is
/// refused with the missing repositories named: nothing is transferred half way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GroupWork {
    /// Every repository of the sender's group project.
    pub repos: Vec<GroupRepo>,
    /// The members the thread works in (all of them unless a worktree chat left some out).
    pub members: Vec<GroupMemberWork>,
}

impl GroupWork {
    /// The identities of the group project, sorted: the key a group is matched by.
    pub fn repo_identities(&self) -> Vec<RepoIdentity> {
        let mut identities: Vec<RepoIdentity> = self.repos.iter().map(|repo| repo.repo.clone()).collect();
        identities.sort();
        identities.dedup();
        identities
    }
}

/// What `projects.repo_identity` stores for a project.
// N1: the design says a group project has "the sorted list" of its members' identities and
// §4.4 adds one column, so both shapes are one JSON value. A single project has `repo` and no
// `repos`; a group has `repos` (sorted) and `repo` for its primary member.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectRepoIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepoIdentity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<RepoIdentity>,
}

// ---- membership operations (§2.3) ----

/// A machine as a membership operation describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MachineDescriptor {
    pub machine_id: MachineId,
    pub name: String,
    pub os: String,
    /// Ed25519 public key, standard base64.
    pub signing_key: String,
    /// X25519 public key, standard base64.
    pub sealing_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_name: Option<String>,
    /// The network port this machine listens on (§3.1) when it is not the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

/// How a machine was added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JoinVia {
    /// Same Tailscale user, no clicks.
    Auto,
    /// "Found on your tailnet · Add".
    Found,
    /// Six-digit code or QR.
    Code,
    /// A saved environment became a member.
    Migrate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoveReason {
    User,
    Stolen,
    Rejoin,
}

/// The kind-specific part of an operation body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OpKind {
    /// Creates the network; the signer is the first member.
    Genesis { owner_login: String, tailnet: String, machine: MachineDescriptor },
    /// Adds a member.
    Add { machine: MachineDescriptor, via: JoinVia },
    /// Removes a member for good: tombstones the key and the Tailscale node id.
    Remove {
        machine_id: MachineId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ts_node_id: Option<String>,
        reason: RemoveReason,
    },
    /// A last-writer register. Keys: `machine.<id>.name`, `machine.<id>.paused`, `broker`,
    /// `follow_updates`, `allow.<id>.<capability>`.
    Set { key: String, value: serde_json::Value },
    /// Joins two networks of the same owner.
    Merge { other_network_id: String, other_genesis: OpEnvelope },
    /// A kind this build does not know. It is kept, verified and gossiped, and otherwise ignored.
    #[serde(other)]
    Unknown,
}

/// The signed JSON body of an operation, parsed. The signed bytes are the envelope's `body`
/// text and are never re-serialized from this.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OpBody {
    pub v: u32,
    pub network_id: String,
    /// Wall-clock time, for display only.
    pub at: DateTime<Utc>,
    /// `max(seen) + 1` when the operation was created; registers resolve by `(lamport, signer)`.
    pub lamport: u64,
    #[serde(flatten)]
    pub op: OpKind,
}

/// An operation as it is stored and sent.
// N1: the design says `body` is bytes and `signature` an Ed25519 signature; on the wire `body` is
// the exact JSON text (so it is the signed bytes) and `signature` is standard base64.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OpEnvelope {
    /// `sha256(body)` as lowercase hex; the primary key.
    pub id: String,
    pub body: String,
    pub signer: MachineId,
    /// `ed25519(signer, OP_SIGNATURE_DOMAIN || body)`, standard base64.
    pub signature: String,
}

// ---- machines ----

/// Whether a machine can be reached right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MachineStatus {
    Online,
    /// Only a relay route works.
    Relaying,
    Offline,
    /// Paused by the owner: ops still sync, nothing else opens.
    Paused,
}

/// A harness on a machine, in the short form the Machines page shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HarnessSummary {
    pub kind: ProviderKind,
    pub available: bool,
    /// Absent when the daemon cannot tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_in: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// One member as Settings → Machines shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MachineCard {
    pub machine_id: MachineId,
    pub name: String,
    pub os: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_name: Option<String>,
    /// The machine's Ed25519 signing key, standard base64. A client pins it, so a route can only
    /// attach to this machine by proving that key (§3.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_key: Option<String>,
    pub status: MachineStatus,
    /// This daemon.
    #[serde(default)]
    pub is_self: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default)]
    pub network_protocol: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<DateTime<Utc>>,
    /// The member relaying to this machine when `status` is `relaying`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_via: Option<MachineId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harnesses: Vec<HarnessSummary>,
    /// Capabilities this machine lets the machine this card is served from use (`allow.*` registers).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub allow: std::collections::BTreeMap<String, bool>,
    /// "Added by <machine>": the signer of the machine's `add` operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_by: Option<MachineId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_via: Option<JoinVia>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<DateTime<Utc>>,
}

/// What to do with a machine found on the tailnet that is not a member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FoundAction {
    /// One click joins it.
    Add,
    /// The target cannot verify the clicker; the user enters the code shown on it.
    Code,
    /// It is in another network of the same owner.
    Merge,
}

/// A machine that answered the well-known endpoint but is not a member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FoundMachine {
    /// Opaque id for `network.add` and `network.merge`. It is the Tailscale node id.
    pub found_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<MachineId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub action: FoundAction,
    /// Set when the machine is in another network of the owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_network_id: Option<String>,
}

/// What the daemon knows about Tailscale.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TailscaleState {
    pub installed: bool,
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailnet: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_name: Option<String>,
    /// The network port the Tailscale listener holds, once bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listening_port: Option<u16>,
    /// Tailnet lock is on; the doctor recommends it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_enabled: Option<bool>,
    /// Why the state is what it is, when it is not running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

// N1: only the counts of §4.1's `OutboxSummary`; the outbox itself (v33) is M2.
/// Work waiting to reach machines that are offline or paused.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OutboxSummary {
    pub queued: u32,
    pub failed: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BrokerInfo {
    pub machine_id: MachineId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub reachable: bool,
}

/// A discovery hint only. A saved pairing or separately trusted SSH connection is needed for setup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NetworkSetupCandidate {
    pub node_id: String,
    pub name: String,
    pub address: String,
    pub online: bool,
}

/// Everything Settings → Machines needs, in one call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NetworkStatus {
    /// Present on daemons that support guarded setup. Missing means the client must not auto-enable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured: Option<bool>,
    /// Tailnet devices that have not advertised Kybern. Discovery does not grant shell access.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub setup_candidates: Vec<NetworkSetupCandidate>,
    /// The master toggle ("Connect to your other machines").
    pub enabled: bool,
    /// Absent on a lone machine: the first membership operation sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_id: Option<String>,
    pub self_machine: MachineCard,
    /// Every member, this machine included.
    pub members: Vec<MachineCard>,
    pub found: Vec<FoundMachine>,
    pub outbox: OutboxSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broker: Option<BrokerInfo>,
    pub tailscale: TailscaleState,
    /// The membership set's digest: the SHA-256 of the sorted operation ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// This machine was removed from its network and can rejoin.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
    /// "Keep all machines on the same version": the `follow_updates` register. A member that sees a
    /// newer version on another member updates itself when it is idle.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub follow_updates: bool,
    /// "Keep this machine reachable when Kybern is closed" and the login item behind it. Absent on
    /// daemons that cannot keep themselves running at login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_reachable: Option<KeepReachableStatus>,
}

/// What starts this machine's daemon at login while it is in a network (ADE-119).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LoginServiceKind {
    /// Windows: a per-user Task Scheduler task that runs at logon.
    TaskScheduler,
    /// macOS: a LaunchAgent in `~/Library/LaunchAgents`.
    LaunchAgent,
    /// Linux: a `systemd --user` service.
    SystemdUser,
    /// Linux without a user systemd: an XDG autostart entry, started by the desktop session.
    XdgAutostart,
}

/// `NetworkStatus::keep_reachable`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct KeepReachableStatus {
    /// The setting, `settings.network.keep_reachable`.
    pub enabled: bool,
    /// The login item should exist now: the setting is on, the network is on and this machine is a member.
    pub wanted: bool,
    /// The login item this platform uses. Absent where Kybern cannot register one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<LoginServiceKind>,
    /// The login item is registered for this data folder and starts this daemon's binary.
    pub registered: bool,
    /// This daemon was started by the login item.
    pub running: bool,
    /// Another service already starts a daemon for this data folder (a remote install's `kybernd`
    /// service). Kybern leaves it as it is and registers nothing of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<String>,
    /// Why the last registration or removal failed, and what to do next.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Problems with a service that starts this daemon, each with what to do next (a systemd unit that
    /// stops everything when one agent runs out of memory).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

// ---- routes (§3.4) ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RouteKind {
    /// `ws://<tailnet ip>:<port>/ws`.
    DirectTailnet,
    /// `wss://…` from `--advertise-url` or Tailscale Serve.
    DirectAdvertised,
    /// A saved SSH tunnel from the old environment profile.
    Ssh,
    /// `<member route>/relay/<machine_id>/ws`.
    Relay,
}

/// One way to reach a machine. Routes are tried in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Route {
    pub kind: RouteKind,
    /// The base URL (`http`/`https`; the client derives `ws`/`wss`).
    pub url: String,
    /// The member a `relay` route goes through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<MachineId>,
}

// ---- method params and results (§4.1) ----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NetworkSetEnabledParams {
    pub enabled: bool,
    /// Trusted setup may enable a never-configured machine, but must preserve an explicit choice.
    #[serde(default)]
    pub only_if_unconfigured: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NetworkFoundParams {
    pub found_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NetworkAddResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<MachineCard>,
    /// The target could not verify the clicker; use `network.join` with its code.
    pub needs_code: bool,
}

/// Run on the joining machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NetworkCodeCreateResult {
    /// Six digits.
    pub code: String,
    pub expires_at: DateTime<Utc>,
    /// Addresses the machine can be reached on.
    pub endpoints: Vec<String>,
    /// `kybern://join?url=…&code=…&machine=<id>`.
    pub qr: String,
}

/// Run on a member with the joining machine's code.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NetworkJoinParams {
    pub code: String,
    pub address: String,
    /// The joining machine is an environment a client already holds a token for (§2.4d): the
    /// `add` operation is recorded `via: migrate` instead of `via: code`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub migrate: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NetworkRemoveParams {
    pub machine_id: MachineId,
    /// "This machine might be lost or stolen": writes `reason: stolen`.
    #[serde(default)]
    pub stolen: bool,
    /// Machines this one added that the user also removes.
    #[serde(default)]
    pub also_remove: Vec<MachineId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NetworkPauseParams {
    pub machine_id: MachineId,
    pub paused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NetworkRenameParams {
    pub machine_id: MachineId,
    pub name: String,
}

/// `network.relay_page.open`: mint a short-lived address on this relay member for one ticket page of `machine_id`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NetworkRelayPageOpenParams {
    /// The machine the page is on. Never the relay member itself.
    pub machine_id: MachineId,
    /// `/preview-proxy/<id>/…` or `/preview-files/<id>/…` (the whole ticket subtree), or `/visual-frame/<id>` /
    /// `/artifact-preview/<id>` with an optional query (that one document).
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NetworkRelayPageOpenResult {
    /// The relay page ticket; give it back with `network.relay_page.close`.
    pub ticket: String,
    /// `/relay/<machine_id>/page/<ticket>` followed by the requested path.
    pub path: String,
    /// The ticket root on this member the page stays under. Ends with `/` for a subtree.
    pub scope: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NetworkRelayPageCloseParams {
    pub ticket: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NetworkClientGrantParams {
    pub machine_id: MachineId,
    pub device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NetworkClientGrantResult {
    /// A long-lived device token for `machine_id`.
    pub token: String,
    pub scopes: Vec<crate::Scope>,
    pub routes: Vec<Route>,
    /// The machine's Ed25519 signing key, standard base64, from the membership set. The client
    /// pins it before it first trusts a route, closing trust on first use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NetworkRoutesParams {
    pub machine_id: MachineId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NetworkRoutesResult {
    pub routes: Vec<Route>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NetworkAllowSetParams {
    pub machine_id: MachineId,
    /// A capability name: see `Scope::capability`.
    pub capability: String,
    pub allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NetworkUpdateSkipped {
    pub machine_id: MachineId,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NetworkUpdateAllResult {
    pub started: Vec<MachineId>,
    pub skipped: Vec<NetworkUpdateSkipped>,
}

/// `network.follow_updates.set`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NetworkFollowUpdatesParams {
    pub enabled: bool,
}

/// `network.keep_reachable.set`: turn "Keep this machine reachable when Kybern is closed" on or off.
/// The login item is registered or removed before the answer.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NetworkKeepReachableParams {
    pub enabled: bool,
}

/// `peer.update.start`: a member asks this machine to start its own self-update.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PeerUpdateStartParams {
    /// The version the caller wants this machine to reach. A machine already at or past it answers
    /// that it is current instead of looking at the release feed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PeerUpdateStartResult {
    /// The update is under way, or waits for the machine's running work to finish.
    pub started: bool,
    /// Why not, in words the Machines page can show ("Updates with the app", "Already up to date").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct NetworkDoctorParams {
    /// One peer's checks only; absent runs everything.
    #[serde(default)]
    pub machine_id: Option<MachineId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DoctorCheck {
    /// A stable id such as `tailscale.running` or `peer.key_matches`.
    pub id: String,
    pub status: DoctorStatus,
    pub title: String,
    /// What to do next, when the check did not pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    /// The peer a per-peer check is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<MachineId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
    pub generated_at: DateTime<Utc>,
}

// ---- machines.info ----

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CpuInfo {
    pub model: String,
    pub logical_cores: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MemoryInfo {
    pub total_bytes: u64,
}

/// Per-machine data for the Machines page. Each machine serves it about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MachineInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<CpuInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpu: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem: Option<MemoryInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harnesses: Vec<HarnessSummary>,
    /// The identity of every project on this machine, for repo mapping.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<RepoIdentity>,
}

// ---- peer.* (daemon to daemon) ----

/// `peer.ops.sync`: both sides send the ids they hold and receive the operations they lack.
// N1: the design says each side "sends the ids it has and the other pulls what it is missing";
// one request does both halves, so the answer carries the responder's ids and the operations the
// caller lacks.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PeerOpsSyncParams {
    pub digest: String,
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PeerOpsSyncResult {
    pub digest: String,
    pub ids: Vec<String>,
    /// Operations the caller does not have.
    pub ops: Vec<OpEnvelope>,
}

/// `peer.ops.push`: new operations, pushed to every connected member and re-gossiped once.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PeerOpsPushParams {
    pub ops: Vec<OpEnvelope>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PeerOpsPushResult {
    pub accepted: u32,
    pub rejected: u32,
}

/// `peer.presence`: pushed on change and with the heartbeat.
// N1: `load` and `battery` are plain optional numbers; the design lists them without a shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PeerPresence {
    pub version: String,
    pub network_protocol: u32,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub harnesses: Vec<HarnessSummary>,
    /// 1-minute load average divided by logical cores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<f32>,
    /// Percent, absent without a battery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery: Option<f32>,
    pub at: DateTime<Utc>,
}

/// `peer.client_token.mint`: B mints a device token for a client of A and returns it sealed to A.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PeerClientTokenMintParams {
    pub device_id: String,
    /// Shown as "<device> (via <A>)".
    pub label: String,
    /// The machines the asking client's own token came through before the caller, so removing any
    /// of them revokes the token minted here too.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lineage: Vec<MachineId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PeerClientTokenMintResult {
    /// The token sealed to the caller's sealing key, standard base64.
    pub sealed_token: String,
    pub scopes: Vec<crate::Scope>,
}

/// `peer.device.revoke`: revoke a device's token on every member.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PeerDeviceRevokeParams {
    pub device_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RelayStreamKind {
    /// The WebSocket at `/relay/{machine_id}/ws`.
    Ws,
    /// A request at `/relay/{machine_id}/http/{*path}`.
    Http,
}

// N1: relayed frames are binary on the wire (§3.4). These are the JSON control messages and a
// text-safe carrier for `data`; N3 may switch `data` to binary frames without a protocol bump.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PeerRelayOpenParams {
    pub stream_id: u64,
    pub kind: RelayStreamKind,
    /// Path and query on the target, such as `/ws` or `/preview-files/…`.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Request headers, minus the relay member's own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PeerRelayDataParams {
    pub stream_id: u64,
    /// Standard base64 of the bytes.
    pub data: String,
    /// Bytes the sender will accept next (the 256 KiB credit window).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit: Option<u32>,
    /// Marks a text frame on a WebSocket stream.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub text: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PeerRelayCloseParams {
    pub stream_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// ---- origins (§4.4) ----
//
// N1: the types exist now so later packages share them. `Thread.origin`, `Thread.inbound` and
// `Thread.moved` come with their columns in v34 (R1) and v35 (F2).

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OriginKind {
    Composer,
    Delegation { task_id: String },
    ToolCall,
    Fork,
    Move,
}

/// Where work that another machine started came from. Only a daemon sets it, and it never changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Origin {
    pub machine_id: MachineId,
    pub machine_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
    pub operation_id: uuid::Uuid,
    pub kind: OriginKind,
    pub permission_cap: PermissionMode,
    /// Machine ids, oldest first; at most one in v1.
    #[serde(default)]
    pub hops: Vec<MachineId>,
}

/// A hidden, read-only thread on the target that records one origin thread's forwarded tool calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InboundInfo {
    pub machine_id: MachineId,
    pub thread_id: ThreadId,
    pub title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MoveState {
    Departing,
    Departed,
    Arrived,
}

/// A thread moved with "Continue on…" (design §7.2). `departing` holds the lease: the thread refuses
/// sends until the target confirms (`departed`, read-only with a link to `peer_thread_id`) or the move
/// is aborted (the field goes away). The daemon sets it; a stale copy of the thread never changes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MoveInfo {
    pub state: MoveState,
    pub peer: MachineId,
    /// The target's name when the move started, for the banner while it is offline.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub peer_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_thread_id: Option<ThreadId>,
    /// While `departing`: when the lease ends unless it is renewed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_until: Option<DateTime<Utc>>,
    pub operation_id: uuid::Uuid,
    /// The branch and snapshot commit the move carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The commit request went out and no answer came back, so the target may or may not have the
    /// thread. The lease no longer expires; Retry asks the target, Abandon gives up.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub uncertain: bool,
}

// ---- settings ----

/// The `settings.network` section.
// N1: §3.1 names `settings.network.port` and the master toggle but gives no struct. `enabled` is off
// until the user turns it on (the desktop's network intro, Settings → Machines or `kybern machines
// enable`); a settings file that already says `true` keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(default)]
pub struct NetworkSettings {
    /// A user has chosen on or off. Old saved switches are conservatively treated as choices.
    pub configured: bool,
    /// "Connect to your other machines".
    pub enabled: bool,
    /// The fixed port the Tailscale listener binds. When taken, the daemon tries the next six.
    pub port: u16,
    /// "Keep this machine reachable when Kybern is closed" (ADE-119): while this machine is in a
    /// network with the network on, its daemon is started at login by a per-user login item, so the
    /// other members reach it with no app window open. Per machine like the rest of this section: it
    /// registers an item with this computer's own login, and a laptop may opt out where a desktop
    /// opts in.
    pub keep_reachable: bool,
}

impl Default for NetworkSettings {
    fn default() -> Self {
        Self { configured: false, enabled: false, port: crate::DEFAULT_PORT, keep_reachable: true }
    }
}

// A missing network section is new; a legacy saved `enabled: false` could be deliberate.
// Keeping `configured: false` in newly serialized defaults makes that distinction durable.
impl<'de> Deserialize<'de> for NetworkSettings {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Saved {
            enabled: Option<bool>,
            configured: Option<bool>,
            port: Option<u16>,
            keep_reachable: Option<bool>,
        }
        let saved = Saved::deserialize(deserializer)?;
        Ok(Self {
            configured: saved.configured.unwrap_or(saved.enabled.is_some()),
            enabled: saved.enabled.unwrap_or(false),
            port: saved.port.unwrap_or(crate::DEFAULT_PORT),
            keep_reachable: saved.keep_reachable.unwrap_or(true),
        })
    }
}

// ---- notifications ----

/// Notification: the whole status changed. Params: [`NetworkStatus`].
pub const NETWORK_CHANGED_NOTIFICATION: &str = "network.changed";
/// Notification: one machine changed. Params: [`MachineCard`].
pub const NETWORK_MACHINE_CHANGED_NOTIFICATION: &str = "network.machine.changed";
