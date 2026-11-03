//! Remote delegation and the outbox (ADE-86 M2: design §6.2, §6.4, §7.1).
//!
//! The outbox is the visible queue of work an offline machine will get when it returns. The `peer.*`
//! types here are what two daemons say to each other to run a delegated agent on another machine:
//! `peer.delegate` creates the child, `peer.delegation.get` follows it, `peer.delegation.cancel`
//! stops it, and `peer.git.bundle` / `peer.git.import` carry the commits a worktree child starts from.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    DelegationRole, DelegationStatus, DelegationWorkspace, DiffStat, EventSeq, GroupWork, MachineId, Origin, PermissionMode, ProviderKind,
    RepoIdentity, ThreadId,
};

/// Notification: an outbox item changed. Params: [`OutboxItem`].
pub const OUTBOX_CHANGED_NOTIFICATION: &str = "outbox.changed";

/// Days an outbox item waits for its target before it expires (`settings.network.outbox_expiry_days`).
pub const OUTBOX_EXPIRY_DAYS: i64 = 7;

/// The largest bundle one delegation sends (§6.2).
pub const MAX_BUNDLE_BYTES: u64 = 200 * 1024 * 1024;

/// What a queued item does when it is delivered (§6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutboxKind {
    ThreadCreate,
    ThreadSend,
    Delegate,
    FilesPush,
    SettingsPatch,
    SecretsPut,
    Fork,
    Move,
    // Add a project to the target (`projects.provision`; payload `ProvisionRequest`).
    Provision,
    // Copy a skill of this machine to the target (`skills.copy`; payload `SkillCopyPayload`).
    SkillCopy,
    PluginInstall,
}

impl OutboxKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OutboxKind::ThreadCreate => "thread_create",
            OutboxKind::ThreadSend => "thread_send",
            OutboxKind::Delegate => "delegate",
            OutboxKind::FilesPush => "files_push",
            OutboxKind::SettingsPatch => "settings_patch",
            OutboxKind::SecretsPut => "secrets_put",
            OutboxKind::Fork => "fork",
            OutboxKind::Move => "move",
            OutboxKind::Provision => "provision",
            OutboxKind::SkillCopy => "skill_copy",
            OutboxKind::PluginInstall => "plugin_install",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutboxState {
    /// Waiting for the target, or for the next attempt.
    Queued,
    /// A delivery is in flight.
    Sending,
    Delivered,
    /// A permanent error: the target was removed, refused the scope, or the project is gone.
    Failed,
    /// The target did not return within the expiry; Retry queues it again.
    Expired,
    Cancelled,
}

impl OutboxState {
    pub fn as_str(self) -> &'static str {
        match self {
            OutboxState::Queued => "queued",
            OutboxState::Sending => "sending",
            OutboxState::Delivered => "delivered",
            OutboxState::Failed => "failed",
            OutboxState::Expired => "expired",
            OutboxState::Cancelled => "cancelled",
        }
    }

    /// Nothing more happens to the item unless the user retries it.
    pub fn is_settled(self) -> bool {
        !matches!(self, OutboxState::Queued | OutboxState::Sending)
    }
}

/// One queued piece of work for another machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OutboxItem {
    pub id: Uuid,
    /// The key the target deduplicates on, so a delivery that is repeated runs once.
    pub operation_id: Uuid,
    /// The machine the work is for.
    pub target: MachineId,
    pub kind: OutboxKind,
    /// The operation's parameters (a `peer.delegate` body for `delegate`).
    pub payload: Value,
    /// Who started the work, for agent work. Stamped by the daemon, never by a caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
    pub state: OutboxState,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_attempt_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// What the target answered, once delivered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Work a client queues for a machine it cannot reach right now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OutboxOp {
    pub kind: OutboxKind,
    pub payload: Value,
    /// Reuse the same key to make a repeated enqueue return the first item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<Uuid>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct OutboxListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<MachineId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<OutboxState>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct OutboxListResult {
    pub items: Vec<OutboxItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OutboxIdParams {
    pub id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OutboxEnqueueParams {
    pub target: MachineId,
    pub operation: OutboxOp,
}

// ---- peer.delegate and friends (§6.2) ----

/// `peer.delegate`: A asks B to run a delegated child. Everything but `repo` and `project` was
/// resolved on A; B checks the origin against the session's machine and clamps the permission mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PeerDelegateParams {
    /// A's operation id. B namespaces it by the origin before it deduplicates.
    pub operation_id: Uuid,
    /// The id both machines use for this delegation (`kybern_agent_status` shows it).
    pub task_id: Uuid,
    pub origin: Origin,
    /// The parent thread's title on A, for the child's brief.
    pub parent_title: String,
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub role: DelegationRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Already resolved against the parent's mode on A; B can only lower it.
    pub permission_mode: PermissionMode,
    #[serde(default)]
    pub workspace: DelegationWorkspace,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owns: Vec<String>,
    /// The repository the parent works in, to find the matching checkout on B.
    pub repo: RepoIdentity,
    /// Picks one of B's projects when more than one matches `repo`: an id, a name or a path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Worktree: the commit B seeds its worktree from (A's snapshot of the parent's changes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    /// A group project's delegation: its repositories and, for a worktree, each member's base commit.
    /// `repo` and `base_commit` are then the first member's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<GroupWork>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PeerDelegateResult {
    pub task_id: Uuid,
    pub thread_id: ThreadId,
    pub status: DelegationStatus,
    /// The mode the child runs in after B clamped it.
    pub permission_mode: PermissionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// The error `data` of a `peer.delegate` that needs the base commit first (`"needs": "bundle"`).
pub const NEEDS_BUNDLE: &str = "bundle";

/// `peer.delegation.get`: the child's state, waiting up to `wait_ms` (at most 50 s) for it to leave
/// `running`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PeerDelegationGetParams {
    pub task_id: Uuid,
    /// Return at once when the child's last event is not past this sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_seq: Option<EventSeq>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u64>,
}

/// The child as B knows it: the mirror A keeps in `remote_children.info`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PeerDelegationView {
    pub task_id: Uuid,
    pub thread_id: ThreadId,
    pub title: String,
    pub provider: ProviderKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub role: DelegationRole,
    pub workspace: DelegationWorkspace,
    pub status: DelegationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files_touched: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diffstat: Option<DiffStat>,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    /// Sequence of the child thread's last event.
    pub last_seq: EventSeq,
}

/// `peer.delegation.cancel`: stop the child. Repeating it is harmless.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PeerDelegationCancelParams {
    pub task_id: Uuid,
}

/// `peer.git.bundle`: one chunk of a bundle, in order. Sending the same offset again is accepted, so a
/// bundle that was interrupted resumes from `received`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PeerGitBundleParams {
    /// The delegation's operation id: it names the staged file on B.
    pub operation_id: Uuid,
    pub offset: u64,
    /// The whole bundle's size.
    pub total: u64,
    /// Standard base64 of this chunk.
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PeerGitBundleResult {
    /// Bytes B holds so far; the next chunk starts here.
    pub received: u64,
}

/// `peer.git.import`: the bundle is complete; check it and import it into the checkout that matches
/// `repo`, under `refs/kybern/incoming/<operation_id>`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PeerGitImportParams {
    pub operation_id: Uuid,
    pub total: u64,
    /// SHA-256 of the whole bundle, hex.
    pub sha256: String,
    /// The commit the bundle is for.
    pub commit: String,
    pub repo: RepoIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// A member of a group project: the identities of the group's repositories. B imports into the checkout of
    /// the group project made of exactly these repositories whose member is `repo`. `operation_id` then names
    /// this member's transfer, not the whole operation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_repos: Vec<RepoIdentity>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PeerGitImportResult {
    pub commit: String,
    pub imported_ref: String,
}

// ---- remote children (§4.4 `remote_children`) ----

/// Where a delegation that runs on another machine stands, as the delegating machine records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteChildState {
    /// Written before anything was sent: the target has not acknowledged yet.
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
    /// The target was removed, or no longer knows the child.
    Unreachable,
}

impl RemoteChildState {
    pub fn as_str(self) -> &'static str {
        match self {
            RemoteChildState::Pending => "pending",
            RemoteChildState::Running => "running",
            RemoteChildState::Completed => "completed",
            RemoteChildState::Failed => "failed",
            RemoteChildState::Cancelled => "cancelled",
            RemoteChildState::Unreachable => "unreachable",
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, RemoteChildState::Pending | RemoteChildState::Running)
    }

    /// The status the delegation tools show for this state.
    pub fn delegation_status(self) -> DelegationStatus {
        match self {
            RemoteChildState::Pending | RemoteChildState::Running => DelegationStatus::Running,
            RemoteChildState::Completed => DelegationStatus::Completed,
            RemoteChildState::Failed | RemoteChildState::Unreachable => DelegationStatus::Failed,
            RemoteChildState::Cancelled => DelegationStatus::Cancelled,
        }
    }
}

/// A delegated child that runs on another machine. The delegating machine keeps this row instead of a
/// thread: the thread lives on `target`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteChild {
    /// The id `kybern_agent_status`, `wait` and `cancel` use, the same on both machines.
    pub task_id: Uuid,
    pub operation_id: Uuid,
    /// The delegating thread on this machine.
    pub parent_thread_id: ThreadId,
    pub target: MachineId,
    /// The child thread on the target, once it acknowledged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_thread_id: Option<ThreadId>,
    pub state: RemoteChildState,
    /// Stop was asked for and the target has not confirmed it yet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stop_requested: bool,
    pub last_seq: EventSeq,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The mirror of the target's delegation (result, files touched, branch, head commit).
    pub info: crate::DelegationInfo,
    /// `info.completed_at` is the target's own time: the child was followed to its end and its delegation said when
    /// that was. Otherwise an end time is this machine's (a cancel before delivery, an unreachable target, a failed
    /// delivery), and only comparable with this machine's clock.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub completed_at_from_target: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
