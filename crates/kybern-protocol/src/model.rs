//! Domain model shared by daemon and clients.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::event::NoticeLevel;
use crate::network::{InboundInfo, MoveInfo, NetworkSettings, Origin, RepoIdentity};

pub type ProjectId = Uuid;
/// Daemon-owned neutral workspace used by chats that are not attached to a
/// user project. Clients present these threads as project-free.
pub const FREE_CHAT_PROJECT_ID: ProjectId = Uuid::from_u128(1);

pub fn is_free_chat_project(id: ProjectId) -> bool {
    id == FREE_CHAT_PROJECT_ID
}
pub type ThreadId = Uuid;
pub type TurnId = Uuid;
pub type MessageId = Uuid;
pub type ApprovalId = Uuid;
pub type SubscriptionId = Uuid;
pub type AssetId = Uuid;
pub type TerminalId = Uuid;

/// Monotonic per-daemon event sequence number. Clients resume from the last one they saw.
pub type EventSeq = i64;

/// Coding agent backends the daemon can drive. Each has its own native driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    ClaudeCode,
    Codex,
    Opencode,
    Pi,
    Omp,
    Cursor,
    Antigravity,
    GrokBuild,
}

impl ProviderKind {
    pub const ALL: [ProviderKind; 8] = [
        ProviderKind::ClaudeCode,
        ProviderKind::Codex,
        ProviderKind::Opencode,
        ProviderKind::Pi,
        ProviderKind::Omp,
        ProviderKind::Cursor,
        ProviderKind::Antigravity,
        ProviderKind::GrokBuild,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::ClaudeCode => "claude-code",
            ProviderKind::Codex => "codex",
            ProviderKind::Opencode => "opencode",
            ProviderKind::Pi => "pi",
            ProviderKind::Omp => "omp",
            ProviderKind::Cursor => "cursor",
            ProviderKind::Antigravity => "antigravity",
            ProviderKind::GrokBuild => "grok-build",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            ProviderKind::ClaudeCode => "Claude Code",
            ProviderKind::Codex => "Codex",
            ProviderKind::Opencode => "OpenCode",
            ProviderKind::Pi => "pi",
            ProviderKind::Omp => "Oh My Pi",
            ProviderKind::Cursor => "Cursor",
            ProviderKind::Antigravity => "Antigravity",
            ProviderKind::GrokBuild => "Grok Build",
        }
    }

    /// Executable name looked up on PATH when no override is configured.
    pub fn default_binary(self) -> &'static str {
        match self {
            ProviderKind::ClaudeCode => "claude",
            ProviderKind::Codex => "codex",
            ProviderKind::Opencode => "opencode",
            ProviderKind::Pi => "pi",
            ProviderKind::Omp => "omp",
            ProviderKind::Cursor => "agent",
            ProviderKind::Antigravity => "agy",
            ProviderKind::GrokBuild => "grok",
        }
    }
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ProviderKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ProviderKind::ALL.into_iter().find(|p| p.as_str() == s).ok_or_else(|| format!("unknown provider: {s}"))
    }
}

/// A configured account/instance of a provider. `default` is created implicitly.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct ProviderInstance {
    pub kind: ProviderKind,
    pub instance: String,
}

impl ProviderInstance {
    pub fn default_for(kind: ProviderKind) -> Self {
        Self { kind, instance: "default".into() }
    }
}

/// A named, native-isolated account. Secrets remain in the harness directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderAccount {
    pub name: String,
    pub directory: String,
    /// Palette key: blue | green | purple | pink | teal | amber. Unknown keys render without a dot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Last identity seen by the daemon, for instant rendering. Never a credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

/// The target of the next message, separate from the admitted live session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionTarget {
    pub provider: ProviderInstance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// Availability of a provider binary on the daemon host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderModel {
    /// Model selector accepted by the provider (for example `gpt-5.6-sol` or `openai/gpt-5.6-sol`).
    pub id: String,
    pub display_name: String,
    /// Concrete model id the selector currently resolves to, when `id` is an
    /// alias (Claude's `opus` → `claude-opus-5-5`). Sessions report the concrete
    /// id, so clients match a thread's model against either field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_id: Option<String>,
    /// The harness's own one-line description (Claude Code: `Opus 5.5 · Best
    /// for everyday, complex tasks`), shown under the name in pickers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Upstream model provider. Harnesses that aggregate providers use this to build a paged picker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Effort values accepted for this model, in the provider's preferred order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(default)]
    pub is_default: bool,
    /// Traits a model can be run with besides effort (Cursor's context size,
    /// fast mode). A picker shows one row for the model and one control per
    /// parameter instead of a row for every combination.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<ModelParameter>,
    /// The selectable combinations of `parameters`. Selecting one sends its
    /// `id` as the model; `id` on this row is the default combination.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<ModelVariant>,
}

/// One trait of a model. A parameter whose values are exactly `true` and
/// `false` is a switch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelParameter {
    pub id: String,
    pub label: String,
    pub values: Vec<ModelParameterValue>,
    /// Value of the model's default combination.
    pub default: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelParameterValue {
    pub value: String,
    pub label: String,
}

/// One combination of a model's parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelVariant {
    /// Model selector to send for this combination.
    pub id: String,
    /// Value of every parameter, keyed by `ModelParameter::id`.
    pub params: std::collections::BTreeMap<String, String>,
    /// Efforts this combination accepts; empty means the row's `efforts`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
}

/// A command advertised by a live harness's native protocol.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProviderCommand {
    pub name: String,
    pub description: String,
}

/// Availability and selectable capabilities of a provider binary on the daemon host.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProviderStatus {
    pub kind: ProviderKind,
    pub display_name: String,
    pub available: bool,
    /// Resolved executable path when found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Human-readable reason when unavailable (not found, too old, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    /// Which permission modes this provider can honor.
    pub supported_permission_modes: Vec<PermissionMode>,
    /// Whether the provider can fork its conversation, enabling conversation rewind.
    pub supports_fork: bool,
    pub supports_model_switch: bool,
    /// Whether effort can be changed after a provider session has started.
    #[serde(default)]
    pub supports_effort_switch: bool,
    /// Fallback effort choices when the catalog does not vary by model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_efforts: Vec<String>,
    /// Models reported by the installed harness. Empty means the harness has no catalog API.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ProviderModel>,
    pub instances: Vec<String>,
}

/// The four permission modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// Every tool call that mutates or executes asks for approval.
    Supervised,
    /// File edits proceed; shell and network still ask.
    AcceptEdits,
    /// The provider decides with its own safety heuristics; asks only when unsure.
    Auto,
    /// Nothing asks. Sandboxing is the provider's problem.
    FullAccess,
}

impl PermissionMode {
    pub const ALL: [PermissionMode; 4] =
        [PermissionMode::Supervised, PermissionMode::AcceptEdits, PermissionMode::Auto, PermissionMode::FullAccess];
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    /// Absolute path on the daemon host.
    pub path: String,
    pub is_git: bool,
    /// Per-project override for "new threads use a worktree". Global default is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktrees_default: Option<bool>,
    /// Prefix for this project's task keys, such as `ADE`. Assigned from the
    /// project name when it is added; editable through `projects.update`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_prefix: Option<String>,
    /// When on, every new thread in the project also gets the project brief and an
    /// index of its notes at session start (not only the Coordinator's children).
    #[serde(default)]
    pub brief_in_all_threads: bool,
    /// This project's overrides of the Coordinator settings in
    /// [`OrchestrationSettings`]. Absent fields fall back to the global value;
    /// see [`OrchestrationSettings::effective_for`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<CoordinatorOverrides>,
    /// The space this project belongs to. Absent only for free chat and on daemons without spaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<SpaceId>,
    /// `group` for a parent folder of several git repositories. Absent means single.
    #[serde(default, skip_serializing_if = "ProjectKind::is_single")]
    pub kind: ProjectKind,
    /// Group members in order; the first is the primary (the group folder itself
    /// when it is a member, else the first member by path). Empty for single projects.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<ProjectMember>,
    /// Top-level entries of the group folder (files or folders, never a member or
    /// `.git`) the user chose to copy into each new worktree container, after
    /// `AGENTS.md`, `CLAUDE.md` and `.claude/`. Entries missing at copy time are skipped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worktree_copy: Vec<String>,
    /// The user hid the "Add shared instructions" offer for this group.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub instructions_offer_dismissed: bool,
    /// What identifies this repository across machines, refreshed with Git status. Absent until
    /// the first refresh and for folders that are not repositories. For a group, the primary member's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepoIdentity>,
    /// A group's members' identities, sorted. Empty for single projects.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<RepoIdentity>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectKind {
    #[default]
    Single,
    /// A parent folder whose members are git repositories inside it.
    Group,
}

impl ProjectKind {
    pub fn is_single(&self) -> bool {
        *self == ProjectKind::Single
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ProjectKind::Single => "single",
            ProjectKind::Group => "group",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MemberStatus {
    Ok,
    /// The folder is gone (deleted or renamed).
    Missing,
    /// The folder exists but is no longer a git repository.
    NotGit,
}

/// One git repository of a group project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectMember {
    /// '/'-separated, relative to `Project.path`; "." is the group folder itself.
    pub path: String,
    /// Display name: the last path component, or the group folder's name for ".".
    /// Two members with the same last component get their parent segment too ("a/api").
    pub name: String,
    pub status: MemberStatus,
}

/// A group thread's record of one member, fixed when the thread starts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ThreadMember {
    /// Member path, relative to the project folder and to the thread cwd.
    pub path: String,
    pub name: String,
    /// False when the user left this member out of a worktree chat: it has no
    /// worktree and the agent is told to treat `source_path` as read-only.
    pub included: bool,
    /// Absolute path of the member's checkout when the thread started.
    pub source_path: String,
    /// Worktree mode: this member's worktree (path inside the container, branch `kybern/<thread-id>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeInfo>,
    /// Branch the worktree forked from (worktree) or the branch checked out at start (local).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    /// Commit the worktree forked from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    /// Set once cleanup has run for this member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_state: Option<WorktreeState>,
    /// The member was missing or not a repository when the thread started, so
    /// it is not available to this thread.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub missing: bool,
}

pub type SpaceId = Uuid;

/// A group of projects (work, personal…). Every project except free chat is in exactly one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Space {
    pub id: SpaceId,
    /// 1–32 characters, trimmed; unique ignoring case.
    pub name: String,
    /// The swatch behind initials and icons, and the fill while an image loads.
    pub color: SpaceColor,
    pub logo: SpaceLogo,
    /// 0-based order in the rail.
    pub position: u32,
    /// Exactly one space is the default. It can be renamed and restyled but not deleted.
    #[serde(default)]
    pub is_default: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SpaceColor {
    Blue,
    Green,
    Purple,
    Pink,
    Teal,
    Amber,
    Red,
    Graphite,
}

impl SpaceColor {
    /// Every swatch, in the order the first free one is picked for a new space.
    pub const ALL: [SpaceColor; 8] = [
        SpaceColor::Blue,
        SpaceColor::Green,
        SpaceColor::Purple,
        SpaceColor::Pink,
        SpaceColor::Teal,
        SpaceColor::Amber,
        SpaceColor::Red,
        SpaceColor::Graphite,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            SpaceColor::Blue => "blue",
            SpaceColor::Green => "green",
            SpaceColor::Purple => "purple",
            SpaceColor::Pink => "pink",
            SpaceColor::Teal => "teal",
            SpaceColor::Amber => "amber",
            SpaceColor::Red => "red",
            SpaceColor::Graphite => "graphite",
        }
    }

    /// Parses the stored string; an unknown colour (a newer daemon's database) reads as blue.
    pub fn from_stored(value: &str) -> SpaceColor {
        SpaceColor::ALL.into_iter().find(|c| c.as_str() == value).unwrap_or(SpaceColor::Blue)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpaceLogo {
    /// The name's initials on the colour.
    Initials,
    /// A glyph from the client's curated set, on the colour. `icon` is a slug
    /// (`^[a-z0-9][a-z0-9-]{0,31}$`); clients that do not know it draw initials.
    Icon { icon: String },
    /// A square 128×128 PNG the daemon derived from an upload (`GET /assets/{id}`).
    Image { asset_id: AssetId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ThreadStatus {
    Idle,
    Running,
    AwaitingApproval,
    Failed,
    Archived,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Thread {
    pub id: ThreadId,
    pub project_id: ProjectId,
    pub title: String,
    pub provider: ProviderInstance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub permission_mode: PermissionMode,
    pub status: ThreadStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeInfo>,
    /// Working directory the agent runs in: the worktree path or the project path.
    pub cwd: String,
    /// The provider's own session id once bound, used for resume and fork.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<String>,
    pub pinned: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Sequence of the last event on this thread. Clients use it to detect gaps.
    pub last_seq: EventSeq,
    /// Immutable parent for a Kybern-managed helper thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<ThreadId>,
    /// Set when this thread is the project's Coordinator, the lead thread that
    /// plans and delegates. At most one non-archived thread per project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator_project_id: Option<ProjectId>,
    /// Set when this thread mirrors one provider-native subagent. Such a thread
    /// is read-only: it records what the subagent said and did, and its
    /// `parent_thread_id` is the thread (or subagent thread) that launched it.
    /// Clients that do not know about subagent threads must hide them from
    /// thread lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentInfo>,
    /// Set when this thread is a child another thread delegated work to with
    /// `kybern_agent_delegate`. Unlike `subagent` it is a normal, writable
    /// thread; `parent_thread_id` is the delegating thread. Clients that do
    /// not know about delegation hide such threads from thread lists the same
    /// way they hide subagents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<DelegationInfo>,
    /// Group threads: the members this thread started with, in project order. Never
    /// changes when the project's members change. Empty for single-repo threads.
    /// For a group worktree thread `worktree.path` and `cwd` are the container
    /// that holds one worktree per included member.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<ThreadMember>,
    /// Set when another machine started this thread (a remote delegation, a forwarded call). Only a
    /// daemon sets it, from the live session, and it never changes. Absent for work the user started here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
    /// Set on the hidden, read-only thread a machine keeps for one thread of another machine: it records
    /// that thread's forwarded tool calls (`peer.tool.call`). Written once and never changes. Clients
    /// keep such threads out of lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inbound: Option<Box<InboundInfo>>,
    /// Set while "Continue on…" is moving this thread to another machine (`departing`: it refuses sends
    /// until the target confirms) and after it moved (`departed`: read-only, with a link to the thread
    /// that continues the work). Only the daemon writes it, and a stale copy of the thread never changes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved: Option<MoveInfo>,
}

/// What the delegating agent asked this child to be.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DelegationRole {
    Implementation,
    Research,
    Review,
    Design,
    Test,
    #[default]
    General,
}

impl DelegationRole {
    pub fn as_str(self) -> &'static str {
        match self {
            DelegationRole::Implementation => "implementation",
            DelegationRole::Research => "research",
            DelegationRole::Review => "review",
            DelegationRole::Design => "design",
            DelegationRole::Test => "test",
            DelegationRole::General => "general",
        }
    }
}

impl std::fmt::Display for DelegationRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a delegated child works.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DelegationWorkspace {
    /// The parent's checkout and branch.
    #[default]
    Shared,
    /// Its own git worktree on `kybern/<child-id>`, seeded from the parent's
    /// current uncommitted state.
    Worktree,
}

impl DelegationWorkspace {
    pub fn as_str(self) -> &'static str {
        match self {
            DelegationWorkspace::Shared => "shared",
            DelegationWorkspace::Worktree => "worktree",
        }
    }
}

impl std::fmt::Display for DelegationWorkspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DelegationStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    /// Cut off by a daemon restart.
    Interrupted,
}

impl DelegationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DelegationStatus::Running => "running",
            DelegationStatus::Completed => "completed",
            DelegationStatus::Failed => "failed",
            DelegationStatus::Cancelled => "cancelled",
            DelegationStatus::Interrupted => "interrupted",
        }
    }
}

impl std::fmt::Display for DelegationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// State of a delegated child's own worktree (`DelegationWorkspace::Worktree`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeState {
    Active,
    /// Left on disk because it was dirty or its branch is unmerged.
    Kept,
    Removed,
}

/// Files and lines changed between two commits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DiffStat {
    pub files: u32,
    pub additions: u32,
    pub deletions: u32,
}

/// A shared-checkout edit that landed on a path a sibling delegation owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DelegationConflict {
    /// Repo-relative path that was edited.
    pub path: String,
    /// The sibling thread whose `owns` globs cover `path`.
    pub owner_thread_id: ThreadId,
    pub at: DateTime<Utc>,
}

/// A Coordinator delegation that used another harness or model than the
/// default configured for its role. `provider` and `model` are the default that
/// was set aside (the child's own are on its thread), so a client can write
/// "Claude Opus 5.5 · instead of the Sonnet default: <reason>".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DefaultOverride {
    pub provider: ProviderInstance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The Coordinator's reason, as it passed it in `override_reason`.
    pub reason: String,
}

/// Delegation metadata carried by a child [`Thread`] created with
/// `kybern_agent_delegate`. Refreshed through `thread_updated`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DelegationInfo {
    /// Stable id agents use to refer to this delegation.
    pub task_id: Uuid,
    /// Idempotency key of the delegate call.
    pub operation_id: Uuid,
    /// Duplicate of `Thread::parent_thread_id` for convenience.
    pub parent_thread_id: ThreadId,
    /// 1 for a direct child of a top-level thread.
    pub depth: u8,
    #[serde(default)]
    pub role: DelegationRole,
    #[serde(default)]
    pub workspace: DelegationWorkspace,
    /// Set when a Coordinator chose another harness or model than the default
    /// configured for this child's role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_override: Option<DefaultOverride>,
    /// Globs relative to the checkout root this child owns (shared only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owns: Vec<String>,
    pub status: DelegationStatus,
    /// The child's final assistant text of the completing turn (at most
    /// 16 KiB, truncated with a marker).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Repo-relative paths the child edited, deduplicated, at most 200.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files_touched: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<DelegationConflict>,
    /// Worktree: the snapshot commit the worktree was seeded from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    /// Worktree: `kybern/<child-id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Worktree: HEAD when the child completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_commit: Option<String>,
    /// Worktree: `base_commit..head_commit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diffstat: Option<DiffStat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_state: Option<WorktreeState>,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    /// Whether the delegating parent has been handed this outcome (delivered,
    /// steered or queued). Cleared when the delegation reaches a terminal state
    /// and set once its `AgentResultItem` is out, so a restart can re-send an
    /// outcome that was still waiting in the debounce window. Rows written
    /// before this field existed read as `true`: nothing is re-sent.
    #[serde(default = "default_true")]
    pub parent_notified: bool,
}

/// Why a message exists. `Task` is a delegation brief; `Warning` comes from
/// Kybern itself (for example a file ownership clash).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadMessagePurpose {
    Task,
    Message,
    Question,
    Reply,
    Warning,
}

impl Thread {
    /// A delegated child another machine started here. Nothing on this machine is its parent, so `threads.list` lists it as a
    /// thread of its own (`origin` names the thread on the other machine that delegated it) and clients nest it under that thread.
    pub fn is_remote_child(&self) -> bool {
        self.delegation.is_some()
            && self.subagent.is_none()
            && self.parent_thread_id.is_none()
            && self.origin.as_ref().is_some_and(|origin| origin.thread_id.is_some())
    }
}

impl ThreadMessagePurpose {
    pub fn as_str(self) -> &'static str {
        match self {
            ThreadMessagePurpose::Task => "task",
            ThreadMessagePurpose::Message => "message",
            ThreadMessagePurpose::Question => "question",
            ThreadMessagePurpose::Reply => "reply",
            ThreadMessagePurpose::Warning => "warning",
        }
    }
}

impl std::fmt::Display for ThreadMessagePurpose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How the sender asked for a thread message to be delivered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadMessageDelivery {
    #[default]
    Queue,
    /// Into the recipient's running turn when its harness can steer; falls
    /// back to `Queue` otherwise.
    Steer,
}

impl ThreadMessageDelivery {
    pub fn as_str(self) -> &'static str {
        match self {
            ThreadMessageDelivery::Queue => "queue",
            ThreadMessageDelivery::Steer => "steer",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadMessageState {
    /// Blocked by the permission rule; waiting for the user to deliver or dismiss it.
    Held,
    Queued,
    Steered,
    /// The recipient's turn that consumed it has started.
    Delivered,
    /// A question that received its reply.
    Answered,
    Dismissed,
    Failed,
}

impl ThreadMessageState {
    pub fn as_str(self) -> &'static str {
        match self {
            ThreadMessageState::Held => "held",
            ThreadMessageState::Queued => "queued",
            ThreadMessageState::Steered => "steered",
            ThreadMessageState::Delivered => "delivered",
            ThreadMessageState::Answered => "answered",
            ThreadMessageState::Dismissed => "dismissed",
            ThreadMessageState::Failed => "failed",
        }
    }
}

/// A message one thread sent another through `kybern_thread_send` (or Kybern
/// sent on its own behalf). The row id is also the queued or steered message id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ThreadMessageRecord {
    pub id: MessageId,
    /// Idempotency key of the send.
    pub operation_id: Uuid,
    /// `None` when Kybern itself sent it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_thread_id: Option<ThreadId>,
    pub to_thread_id: ThreadId,
    pub purpose: ThreadMessagePurpose,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<MessageId>,
    pub body: String,
    /// Requested delivery.
    pub delivery: ThreadMessageDelivery,
    pub state: ThreadMessageState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// How a held message left the held state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HeldResolution {
    Delivered,
    Dismissed,
}

/// One delegated agent's outcome inside an [`ContentPart::AgentResults`] batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentResultItem {
    pub task_id: Uuid,
    pub thread_id: ThreadId,
    pub title: String,
    pub provider: ProviderKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub role: DelegationRole,
    pub status: DelegationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub workspace: DelegationWorkspace,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_commit: Option<String>,
    /// Worktree: the commit the worktree was seeded from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diffstat: Option<DiffStat>,
    /// At most 50.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files_touched: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<DelegationConflict>,
}

/// Provider-native subagent metadata carried by a read-only child [`Thread`].
///
/// The title, model and effort live on the thread itself. This struct holds
/// the rest of what a subagent page needs and is refreshed (through
/// `thread_updated`) whenever the parent's runtime task for it changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SubagentInfo {
    /// Provider-stable id of the runtime task in the parent thread. Together
    /// with the root thread it identifies this child thread.
    pub task_id: String,
    /// The thread that owns the provider session. For a subagent launched by
    /// another subagent this is the root, not `Thread::parent_thread_id`.
    /// `tasks.stop` and `tasks.background` accept the child thread id and
    /// resolve the session through it.
    pub root_thread_id: ThreadId,
    /// Turn of the root thread that launched the subagent.
    pub parent_turn_id: TurnId,
    /// Tool call in the parent that launched it, when the provider has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_thread_id: Option<String>,
    /// Provider agent type or role (`Explore`, `worker`, `task:general`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    pub status: RuntimeTaskStatus,
    #[serde(default)]
    pub backgrounded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool_name: Option<String>,
    /// Latest progress or final result summary from the provider, as reported
    /// (see `progress` and `result` for the normalized one-line forms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// One live line while the subagent works: the provider's latest activity
    /// text, else the last tool it used (`Using Read`). Cleared once it settles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<String>,
    /// First line of the subagent's final answer once it settled (its last
    /// message, else the provider's result summary, else the failure message).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub stats: RuntimeTaskStats,
    /// Whether `tasks.stop` / `tasks.background` currently work for it.
    #[serde(default)]
    pub capabilities: RuntimeTaskCapabilities,
    /// False when the harness only reports lifecycle (Pi/OMP and providers
    /// without native subagent forwarding): the transcript holds just the
    /// prompt and the outcome.
    #[serde(default)]
    pub transcript: bool,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
}

/// One piece of a user message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    /// Inline image. `data` is base64 of the raw bytes.
    Image {
        media_type: String,
        data: String,
    },
    /// Uploaded attachment served by the daemon's HTTP asset route.
    Attachment {
        asset_id: AssetId,
        name: String,
        media_type: String,
        size: u64,
    },
    /// `@path` mention resolved relative to the thread cwd.
    FileMention {
        path: String,
    },
    /// Inert reference to an existing Kybern thread. Selecting or rendering it
    /// never reads, wakes, or sends to the referenced thread.
    ThreadReference {
        thread_id: ThreadId,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<ProjectId>,
    },
    /// A user-selected agent skill. Drivers translate this into the provider's
    /// native invocation syntax instead of treating a lookalike `$NAME` as a
    /// skill accidentally.
    Skill {
        name: String,
        /// Canonical `SKILL.md` path returned by the provider catalog. Codex
        /// requires it for its structured `skill` turn input; other drivers
        /// may use only the name.
        path: String,
    },
    /// A provider plugin or app the user pulled in with `@name`, such as
    /// Codex's Computer Use. `path` is the provider's own id for it
    /// (`plugin://name@marketplace` or `app://connector-id`); drivers without
    /// a structured form send the mention as text.
    Mention {
        name: String,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
    },
    /// A message from another thread (or from Kybern itself when
    /// `from_thread_id` is `None`). Providers receive it as flattened text; see
    /// [`thread_message_text`].
    ThreadMessage {
        /// The `thread_messages` row id; also the queued or steered message id.
        message_id: MessageId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_thread_id: Option<ThreadId>,
        from_title: String,
        purpose: ThreadMessagePurpose,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<MessageId>,
        body: String,
    },
    /// A batch of delegated-agent outcomes delivered to the delegating thread.
    /// Providers receive it as flattened text; see [`agent_results_text`].
    AgentResults {
        items: Vec<AgentResultItem>,
    },
    /// A row Kybern itself adds to a conversation, such as "Setup started".
    /// Clients draw it as a small system row, never a user bubble; it may sit in
    /// front of the user's own parts. Providers never receive it as written:
    /// the daemon swaps in its own instructions for the provider's copy, and a
    /// driver that meets one flattens `text`.
    SystemRow {
        kind: SystemRowKind,
        text: String,
    },
}

/// What a [`ContentPart::SystemRow`] announces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SystemRowKind {
    /// The Coordinator's setup interview began (a new Coordinator without a
    /// brief, or "Redo setup").
    CoordinatorSetup,
    /// The user chose "Tidy notes": the Coordinator was asked to check outdated
    /// memory notes. The provider's copy carries the instructions.
    CoordinatorTidyNotes,
}

impl ContentPart {
    /// Provider-facing text of a [`ContentPart::ThreadMessage`] or
    /// [`ContentPart::AgentResults`]; `None` for every other part. Drivers
    /// flatten these parts through this one helper so every harness sees the
    /// same wording.
    pub fn orchestration_text(&self) -> Option<String> {
        match self {
            ContentPart::ThreadMessage { message_id, from_thread_id, from_title, purpose, reply_to, body } => {
                Some(thread_message_text(*message_id, *from_thread_id, from_title, *purpose, *reply_to, body))
            }
            ContentPart::AgentResults { items } => Some(agent_results_text(items)),
            ContentPart::SystemRow { text, .. } => Some(text.clone()),
            _ => None,
        }
    }
}

impl ContentPart {
    /// `orchestration_text` for a Coordinator's own copy, where a child result
    /// or a thread message is cut to [`LEAD_COPY_BYTES`] with a pointer to the
    /// rest. `None` for every part that is not a [`ContentPart::ThreadMessage`]
    /// or [`ContentPart::AgentResults`]. Stored messages and clients keep the
    /// full text; only the provider's copy is capped.
    pub fn lead_orchestration_text(&self) -> Option<String> {
        match self {
            ContentPart::ThreadMessage { message_id, from_thread_id, from_title, purpose, reply_to, body } => {
                let total = body.len();
                let (shown, cut) = cut_on_sentence(body, LEAD_COPY_BYTES);
                let mut text = thread_message_text(*message_id, *from_thread_id, from_title, *purpose, *reply_to, shown);
                if cut {
                    match from_thread_id {
                        Some(from) => text.push_str(&format!(
                            "\n\n[Message cut at {} of {total} bytes. Read the rest with kybern_thread_read {from}.]",
                            shown.len()
                        )),
                        None => text.push_str(&format!("\n\n[Message cut at {} of {total} bytes.]", shown.len())),
                    }
                }
                Some(text)
            }
            ContentPart::AgentResults { items } => Some(agent_results_text_with(items, Some(LEAD_COPY_BYTES))),
            _ => None,
        }
    }
}

/// Bytes of a child result or thread message a Coordinator's own copy carries.
pub const LEAD_COPY_BYTES: usize = 1500;

/// `text` cut to at most `max` bytes, on a sentence boundary when the first
/// half has one (else on a word, else on a character), and whether it was cut.
pub fn cut_on_sentence(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let head = &text[..end];
    let sentence = head
        .char_indices()
        .filter(|(i, c)| match c {
            '\n' => true,
            '.' | '!' | '?' => head[i + 1..].chars().next().is_none_or(char::is_whitespace),
            _ => false,
        })
        .map(|(i, c)| i + c.len_utf8())
        .next_back()
        .filter(|cut| *cut >= max / 2);
    let cut = sentence.or_else(|| head.rfind(char::is_whitespace).filter(|cut| *cut >= max / 2)).unwrap_or(end);
    (text[..cut].trim_end(), true)
}

pub fn thread_reference_text(thread_id: ThreadId, title: &str) -> String {
    format!("@thread[{thread_id}] {title} (you can read it with kybern_thread_read and message it with kybern_thread_send)")
}

/// Provider-facing text for a [`ContentPart::ThreadMessage`]. Every driver
/// flattens the part through this so agents see one format.
pub fn thread_message_text(
    message_id: MessageId,
    from_thread_id: Option<ThreadId>,
    from_title: &str,
    purpose: ThreadMessagePurpose,
    reply_to: Option<MessageId>,
    body: &str,
) -> String {
    let mut out = match from_thread_id {
        Some(from) => format!("Message from thread \"{from_title}\" ({from}) · {purpose} · id {message_id}"),
        None => format!("Message from Kybern · {purpose} · id {message_id}"),
    };
    if let Some(reply_to) = reply_to {
        out.push_str(&format!(" · reply to {reply_to}"));
    }
    out.push_str(":\n");
    out.push_str(body);
    let guidance = match purpose {
        ThreadMessagePurpose::Question => match from_thread_id {
            Some(from) => format!(
                "Reply with kybern_thread_send to {from} with purpose \"reply\" and reply_to \"{message_id}\". If you do not, your final message this turn is sent back as the reply."
            ),
            None => String::new(),
        },
        ThreadMessagePurpose::Message | ThreadMessagePurpose::Reply | ThreadMessagePurpose::Warning => {
            "No acknowledgement needed.".to_string()
        }
        ThreadMessagePurpose::Task => String::new(),
    };
    if !guidance.is_empty() {
        out.push_str("\n\n");
        out.push_str(&guidance);
    }
    out
}

/// Provider-facing text for a [`ContentPart::AgentResults`] batch.
pub fn agent_results_text(items: &[AgentResultItem]) -> String {
    agent_results_text_with(items, None)
}

/// Files listed per item in a capped batch.
const CAPPED_FILES_LISTED: usize = 10;

/// `agent_results_text`, or with `cap` (a Coordinator's copy) each item reduced
/// to status, branch, commit and diffstat, the start of its result cut on a
/// sentence boundary, and where to read the rest.
fn agent_results_text_with(items: &[AgentResultItem], cap: Option<usize>) -> String {
    let mut out = format!("Results from {} delegated agent(s):", items.len());
    for item in items {
        let model = item.model.as_deref().unwrap_or("default");
        out.push_str(&format!(
            "\n\n- {} ({}/{}, {}) — {} · task {} · thread {}",
            item.title, item.provider, model, item.role, item.status, item.task_id, item.thread_id
        ));
        match item.workspace {
            DelegationWorkspace::Shared => {
                if item.files_touched.is_empty() {
                    out.push_str("\n  workspace: shared checkout, no files touched");
                } else if cap.is_some() && item.files_touched.len() > CAPPED_FILES_LISTED {
                    out.push_str(&format!(
                        "\n  files touched: {}, +{} more",
                        item.files_touched[..CAPPED_FILES_LISTED].join(", "),
                        item.files_touched.len() - CAPPED_FILES_LISTED
                    ));
                } else {
                    out.push_str(&format!("\n  files touched: {}", item.files_touched.join(", ")));
                }
            }
            DelegationWorkspace::Worktree => {
                let branch = item.branch.as_deref().unwrap_or("(unknown branch)");
                let commit = item.head_commit.as_deref().unwrap_or("(no commit)");
                out.push_str(&format!("\n  branch {branch} at {commit}"));
                if let Some(base) = item.base_commit.as_deref() {
                    out.push_str(&format!(", based on {base}"));
                }
                if let Some(stat) = item.diffstat {
                    out.push_str(&format!(", {} files +{} −{}", stat.files, stat.additions, stat.deletions));
                }
                match (item.base_commit.as_deref(), item.head_commit.as_deref()) {
                    _ if cap.is_some() => out.push_str(&format!(". Merge it with kybern_git merge_child {}.", item.thread_id)),
                    (Some(base), Some(head)) => out.push_str(&format!(
                        ". If your checkout is clean, merge the branch with git. Otherwise apply only this agent's changes with `git diff {base}..{head} | git apply --3way`; do not merge the branch, because its first commit is a snapshot of your uncommitted changes."
                    )),
                    _ => out.push_str(". Merge it with git."),
                }
            }
        }
        for conflict in &item.conflicts {
            out.push_str(&format!("\n  conflict: edited {} owned by thread {}", conflict.path, conflict.owner_thread_id));
        }
        if item.status != DelegationStatus::Completed
            && let Some(error) = &item.error
        {
            out.push_str("\n  Error: ");
            out.push_str(cap.map_or(error.as_str(), |cap| cut_on_sentence(error, cap).0));
        }
        match (&item.result, &item.error) {
            (Some(result), _) if !result.trim().is_empty() => {
                out.push_str("\n  Result:\n");
                match cap {
                    Some(cap) => {
                        let (shown, cut) = cut_on_sentence(result, cap);
                        out.push_str(shown);
                        if cut {
                            out.push_str(&format!("\n  […cut at {} of {} bytes]", shown.len(), result.len()));
                        }
                    }
                    None => out.push_str(result),
                }
            }
            (_, Some(error)) if item.status == DelegationStatus::Completed => {
                out.push_str("\n  Error: ");
                out.push_str(cap.map_or(error.as_str(), |cap| cut_on_sentence(error, cap).0));
            }
            _ => {}
        }
        if cap.is_some() {
            out.push_str(&format!("\n  Full result: kybern_thread_read {}", item.thread_id));
        }
    }
    out.push_str("\n\nIntegrate these results. For another round, delegate again with a full brief.");
    out
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UserMessage {
    pub parts: Vec<ContentPart>,
}

impl UserMessage {
    pub fn text(text: impl Into<String>) -> Self {
        Self { parts: vec![ContentPart::Text { text: text.into() }] }
    }

    /// Plain-text rendering used for titles and provider prompts.
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for p in &self.parts {
            match p {
                ContentPart::Text { text } => out.push_str(text),
                ContentPart::FileMention { path } => {
                    out.push('@');
                    out.push_str(path);
                }
                ContentPart::ThreadReference { thread_id, title, .. } => {
                    out.push_str(&thread_reference_text(*thread_id, title));
                }
                ContentPart::Skill { name, .. } => {
                    out.push('$');
                    out.push_str(name);
                }
                ContentPart::Mention { name, display_name, .. } => {
                    out.push('@');
                    out.push_str(display_name.as_deref().unwrap_or(name));
                }
                ContentPart::ThreadMessage { message_id, from_thread_id, from_title, purpose, reply_to, body } => {
                    out.push_str(&thread_message_text(*message_id, *from_thread_id, from_title, *purpose, *reply_to, body));
                }
                ContentPart::AgentResults { items } => out.push_str(&agent_results_text(items)),
                ContentPart::SystemRow { text, .. } => out.push_str(text),
                ContentPart::Image { .. } => out.push_str("[image]"),
                ContentPart::Attachment { name, .. } => {
                    out.push('[');
                    out.push_str(name);
                    out.push(']');
                }
            }
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillScope {
    Project,
    Repo,
    User,
    System,
    Admin,
    App,
    /// A provider plugin or connector mentioned with `@name` rather than
    /// invoked as a `$skill`.
    Plugin,
    Other,
}

/// One skill the selected provider can invoke in a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub path: String,
    pub scope: SkillScope,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
    }
}

/// A durable unit of work started by the provider while handling a thread.
///
/// Providers expose different names for these (subagent, task, background
/// command, monitor). Kybern preserves the provider type while normalizing the
/// lifecycle so every client can render the same activity surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeTaskKind {
    Agent,
    Process,
    Monitor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeTaskStatus {
    Pending,
    Running,
    Waiting,
    Stopping,
    Completed,
    Failed,
    Stopped,
    Interrupted,
}

impl RuntimeTaskStatus {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Pending | Self::Running | Self::Waiting | Self::Stopping)
    }
}

/// Ownership of provider output after it has been normalized by a driver.
/// Root output is eligible for the parent transcript; agent-owned output is
/// retained as activity metadata and must never be promoted to the root answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventOrigin {
    #[default]
    Root,
    Agent {
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_thread_id: Option<String>,
    },
}

impl EventOrigin {
    pub fn is_root(&self) -> bool {
        matches!(self, Self::Root)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeTaskCapabilities {
    /// The provider exposes a targeted stop operation for this task.
    #[serde(default)]
    pub stop: bool,
    /// The provider can detach this foreground task without interrupting it.
    #[serde(default)]
    pub background: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeTaskStats {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_uses: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rss_kb: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeTask {
    /// Provider-stable task, child-thread, tool-call, or process id.
    pub id: String,
    pub thread_id: ThreadId,
    /// Kybern turn that launched this task. Background work can outlive it.
    pub origin_turn_id: TurnId,
    /// Event sequence that first anchored this task in the transcript. Older
    /// stored payloads use zero and are repaired by the event projector.
    #[serde(default)]
    pub started_seq: EventSeq,
    /// Sequence of the latest accepted task snapshot. Progress updates change
    /// this value without changing `started_seq` or the row's visual position.
    #[serde(default)]
    pub updated_seq: EventSeq,
    pub kind: RuntimeTaskKind,
    pub status: RuntimeTaskStatus,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default)]
    pub backgrounded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub stats: RuntimeTaskStats,
    #[serde(default)]
    pub capabilities: RuntimeTaskCapabilities,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadActivityState {
    Working,
    Monitoring,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ThreadActivitySummary {
    pub thread_id: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<ThreadActivityState>,
    pub active_agents: u32,
    pub active_processes: u32,
    pub active_monitors: u32,
    /// Running delegated children (Kybern delegation, not provider subagents). Clients load a parent's children from this
    /// at startup: a delegated child is no runtime task of its parent, so `active_agents` never counts it.
    #[serde(default)]
    pub active_children: u32,
    /// What this thread's oldest waiting request asks for, if one waits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_request: Option<crate::PendingRequestKind>,
    /// A delegated descendant waits on the user: `question` when any of them asks one, else `approval`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_child_request: Option<crate::PendingRequestKind>,
}

/// A pending or resolved request for the user to approve a tool call.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalRequest {
    pub id: ApprovalId,
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    /// Provider's tool-call id, when the provider exposes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub tool_name: String,
    /// Provider-native input, rendered by the client (command, file path, diff, ...).
    pub input: Value,
    /// Human-readable one-line summary, e.g. `bash: git status`.
    pub summary: String,
    /// Provider-suggested "always allow" rules the client may offer.
    #[serde(default)]
    pub suggestions: Vec<Value>,
    pub created_at: DateTime<Utc>,
    /// The request's questions in one shape for every harness (see
    /// [`crate::user_input_questions`]); empty when it is not a question.
    /// Older clients keep reading `input`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<crate::UserInputQuestion>,
}

/// `tool_name` of the daemon-raised approval behind the `kybern_request_secret`
/// tool. The card asks the user for one secret value; the daemon writes it to a
/// file and never returns, persists or logs it.
///
/// `input` is a [`SecretRequestInput`]. The answer is
/// `ApprovalDecision::Submit { response: {"secret": "<value>"} }` or `Deny`.
/// Stored and broadcast copies of the answer carry [`REDACTED_SECRET`] instead
/// of the value (see [`ApprovalRequest::redacted_decision`]).
pub const SECRET_REQUEST_TOOL: &str = "kybern_request_secret";
/// What stored and broadcast copies of a secret answer contain instead of it.
pub const REDACTED_SECRET: &str = "[redacted]";
/// Longest secret, in bytes, the daemon accepts.
pub const MAX_SECRET_BYTES: usize = 16 * 1024;

/// The `input` of a [`SECRET_REQUEST_TOOL`] approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SecretRequestInput {
    /// Short name of the secret, shown as the card title.
    pub label: String,
    /// Why the agent needs it, shown as the card body.
    pub reason: String,
    /// The variable the daemon writes, e.g. `STRIPE_KEY`.
    pub env_key: String,
    /// The file it is written to, relative to the thread workspace, e.g. `.env`.
    pub file: String,
    /// The key is already in the file, so saving replaces its current value.
    pub replaces: bool,
    /// Hint for the empty field, if the agent gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
}

/// `tool_name` of the daemon-raised card behind `kybern_html_publish` with
/// `await_answer`: the published visual waits for the reader's answer. Clients
/// draw it as an answer bar under the visual rather than as a card.
///
/// `input` is a [`VisualAnswerInput`]. The answer is
/// `ApprovalDecision::Submit { response: {"answer": <value>, "label": "<words>"} }`
/// (the value matches `input.schema`, at most [`MAX_VISUAL_ANSWER_BYTES`]),
/// `Submit { response: {"reply": "<message>"} }` when the reader typed instead,
/// or `Deny` to skip.
pub const VISUAL_ANSWER_TOOL: &str = "kybern_visual_answer";
/// Longest visual answer, in bytes of JSON.
pub const MAX_VISUAL_ANSWER_BYTES: usize = 8 * 1024;

/// The `input` of a [`VISUAL_ANSWER_TOOL`] card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VisualAnswerInput {
    pub visual_id: uuid::Uuid,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
}

/// `tool_name` of the daemon-raised approval behind the `kybern_request_pick`
/// tool: the agent asks the user to point at something (an element in the
/// Preview or an inline visual, a file or folder, an image, a window) instead
/// of describing it.
///
/// `input` is a [`PickRequestInput`]. The answer is
/// `ApprovalDecision::Submit { response }` with the picked target (see
/// [`PickKind`] for the shape per kind) or `Deny`.
pub const PICK_REQUEST_TOOL: &str = "kybern_request_pick";
/// Largest answer, in bytes of JSON, a pick card may submit.
pub const MAX_PICK_RESPONSE_BYTES: usize = 48 * 1024;

/// What the user is asked to point at, and the `response` object the card submits for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PickKind {
    /// An element in the Preview page. `{selector, tag, text?, box:{x,y,width,height}, html?, page_url?, screenshot_asset?}`.
    PreviewElement,
    /// An element in an inline visual. Same as `preview_element` plus `id` (the element's `data-kybern-id`, when it has one).
    VisualElement,
    /// A file in the workspace. `{path}`, workspace-relative.
    File,
    /// A folder in the workspace. `{path}`, workspace-relative (`.` is the workspace itself).
    Folder,
    /// An image the user drops, pastes or chooses. `{asset_id, name}`.
    Image,
    /// A window or screen area the user captures. `{asset_id, app?, title?}`.
    Window,
}

/// The `input` of a [`PICK_REQUEST_TOOL`] approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PickRequestInput {
    pub kind: PickKind,
    /// What the agent needs, in a sentence the user can act on.
    pub prompt: String,
}

/// `tool_name` of the daemon-raised approval behind `kybern_request_login`: the
/// card "Sign in to GitHub to continue". The sign-in itself runs in the daemon
/// and the card follows it through [`RequestProgress`]; the client drives it
/// with `requests.act` (`browser`, `terminal`, `paste`, `cancel`, `retry`).
///
/// `input` is a [`LoginRequestInput`]. Clients may only answer `Deny` (the
/// decline row); the daemon resolves the card with `Submit { response:
/// {"status": "signed_in", "cli", "account"?} }` once the CLI is signed in.
pub const LOGIN_REQUEST_TOOL: &str = "kybern_request_login";
/// `tool_name` of the daemon-raised approval behind `kybern askpass`, the
/// helper behind `SUDO_ASKPASS`, `SSH_ASKPASS` and `GIT_ASKPASS`. Always a
/// private card, never answered automatically in any permission mode.
///
/// `input` is a [`PromptRequestInput`]. The answer is `Submit { response:
/// {"secret": "<value>"} }` for a masked prompt, `{"value": "<text>"}` for a
/// visible one (a git username) and `{"confirmed": true|false}` for a yes/no
/// prompt (an SSH host key), or `Deny`. A `secret` is redacted like
/// [`REDACTED_SECRET`] in stored and broadcast copies.
pub const PROMPT_REQUEST_TOOL: &str = "kybern_askpass";
/// `tool_name` of the daemon-raised approval behind `kybern_request_permission`:
/// the card "Let <app> use the camera". The daemon watches for the grant where
/// the OS lets it; the client drives the rest with `requests.act` (`open`,
/// `check`, `granted`, `stop`). Resolved by the daemon with `Submit { response:
/// {"status": "granted"|"user_confirmed"|...} }`; clients may only `Deny`.
///
/// `input` is a [`PermissionRequestInput`].
pub const PERMISSION_REQUEST_TOOL: &str = "kybern_request_permission";
/// Longest askpass answer, in bytes.
pub const MAX_PROMPT_ANSWER_BYTES: usize = 4096;

/// The `input` of a [`LOGIN_REQUEST_TOOL`] approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LoginRequestInput {
    /// The CLI to sign in, e.g. `gh`.
    pub cli: String,
    /// The service's name for the title, e.g. `GitHub`.
    pub service: String,
    /// Why the agent needs it.
    pub reason: String,
    /// The Account line of the card, e.g. "github.com, any account you choose".
    pub account: String,
    /// The Access line (scopes), if the CLI has any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<String>,
    /// What the sign-in changes, e.g. "`gh` for this Mac, so your terminal uses it too".
    pub signs_in: String,
    /// The command that signs in, as the terminal path runs it.
    pub command: String,
    /// The ways offered, in row order: `browser` and `terminal`.
    pub methods: Vec<String>,
    /// The CLI is not in Kybern's table; the agent supplied the command.
    #[serde(default)]
    pub custom: bool,
}

/// The `input` of a [`PROMPT_REQUEST_TOOL`] approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PromptRequestInput {
    /// `sudo`, `ssh_passphrase`, `ssh_host_key`, `git_username`, `git_password`
    /// or `other`.
    pub purpose: String,
    /// The prompt exactly as the program printed it (credentials in URLs removed).
    pub prompt: String,
    /// Mask the field.
    pub secret: bool,
    /// Ask yes or no (an SSH host key). There is no default answer.
    pub confirm: bool,
    /// The command that asked, found by walking parent processes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The harness's display name, e.g. `Claude Code`.
    pub harness: String,
    /// The host the prompt is about (ssh and git), if the prompt names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// An SSH host key's type, e.g. `ED25519`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_type: Option<String>,
    /// An SSH host key's fingerprint, e.g. `SHA256:...`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// 1 for the first ask, 2 when the same command asked again (a wrong password).
    pub attempt: u32,
    /// Seconds the helper waits before it fails the command.
    pub wait_seconds: u64,
}

/// The `input` of a [`PERMISSION_REQUEST_TOOL`] approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionRequestInput {
    /// `camera`, `microphone`, `screen_recording`, `accessibility`,
    /// `local_network`, `full_disk_access`, `notifications` or `location`.
    pub pane: String,
    /// The app that needs the permission, as the agent named it.
    pub app: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    /// Where the app is, when Kybern found it (for "show in Finder").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_path: Option<String>,
    pub reason: String,
    /// The pane's name as System Settings shows it, e.g. "Privacy & Security › Camera".
    pub pane_label: String,
    /// The `x-apple.systempreferences:` link that opens the pane.
    pub url: String,
    /// The user must add the app to a list (Full Disk Access, Accessibility).
    pub needs_app_added: bool,
    /// The app has to be relaunched once the permission is on.
    pub relaunch: bool,
    /// Kybern can see the grant itself; otherwise the user confirms it.
    pub detects: bool,
}

/// How a question card's deadline works (`kybern_ask_user` with `default` and
/// `deadline_seconds`). Lives in an `AskUserQuestion` approval's `input` as
/// `deadline`. The daemon answers with the defaults at `ends_at` unless the
/// user pauses (`requests.act` `pause` and `resume`) or holds it (`hold`);
/// the answer then carries `auto: {"at", "waited_seconds"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QuestionDeadline {
    /// The question text → the option label chosen when the deadline fires.
    pub defaults: std::collections::BTreeMap<String, String>,
    /// The wait the agent asked for after the user's cap, in seconds.
    pub seconds: u64,
    pub started_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
}

/// What kind of request a [`RequestProgress`] belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    Login,
    Permission,
    Question,
    /// A terminal tab a `kybern_run_in_terminal` call opened; `approval_id`
    /// is the call's own id and no card exists.
    Terminal,
}

/// Where a request is. Not every phase applies to every kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestPhase {
    /// The card is waiting for a choice.
    Asking,
    /// Kybern waits for the user to finish elsewhere (browser, terminal, System Settings).
    Waiting,
    /// A login needs the code the page showed (`requests.act` `paste`).
    PasteCode,
    Verifying,
    /// Succeeded; the card stays about 900 ms, then leaves.
    Done,
    /// The attempt ended without success; `error` says why and the rows return.
    Failed,
    /// A permission is still off after the user said or Kybern saw otherwise.
    NotGranted,
    /// A question deadline is paused; `remaining_ms` is what is left.
    Paused,
    /// The user chose to wait; no deadline remains.
    Held,
}

/// Live state of a request card, pushed as `requests.progress` and readable
/// with `requests.progress`. Not persisted; the card's approval is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RequestProgress {
    pub approval_id: ApprovalId,
    pub thread_id: ThreadId,
    pub kind: RequestKind,
    pub phase: RequestPhase,
    /// `browser` or `terminal` for a login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// A login's sign-in page, or a permission's System Settings link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The one-time code the page asks for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
    /// The terminal a sign-in or `kybern_run_in_terminal` opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<crate::methods::TerminalInfo>,
    /// Who is signed in, once known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// What went wrong, in a sentence that says what to do next.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When the wait ends: a login's timeout or a question's deadline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ends_at: Option<DateTime<Utc>>,
    /// Time left while a deadline is paused, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_ms: Option<u64>,
    pub updated_at: DateTime<Utc>,
}

/// `tool_name` of the daemon-raised approval behind `kybern_coordinator_propose_brief`:
/// the card "Save this as the project brief?" holds the Coordinator's proposed
/// text, which the user can edit. This card is the only way agent text reaches
/// the project brief.
///
/// `input` is a [`BriefProposalInput`]. The answer is
/// `ApprovalDecision::Submit { response: {"text": "<the user's final wording>"} }`
/// (non-empty, at most [`MAX_BRIEF_BYTES`]) or `Deny`.
pub const COORDINATOR_BRIEF_TOOL: &str = "kybern_coordinator_brief";
/// `tool_name` of the daemon-raised approval behind
/// `kybern_coordinator_propose_settings`: the card "Apply these Coordinator
/// settings?" lists each proposed change as old → new, and the user can untick rows.
///
/// `input` is a [`SettingsProposalInput`]. The answer is
/// `ApprovalDecision::Submit { response: {"accepted": ["<change path>", ...]} }`
/// (at least one path from `input.changes`) or `Deny`.
pub const COORDINATOR_SETTINGS_TOOL: &str = "kybern_coordinator_settings";
/// `tool_name` of the daemon-raised approval behind
/// `kybern_coordinator_propose_note_changes`: the card "Apply N note changes?"
/// lists each proposed append, with the line it retires when it supersedes one, and
/// the user can untick rows.
///
/// `input` is a [`NoteChangesProposalInput`]. The answer is
/// `ApprovalDecision::Submit { response: {"accepted": ["<change path>", ...]} }`
/// (at least one path from `input.changes`) or `Deny`.
pub const COORDINATOR_NOTES_TOOL: &str = "kybern_coordinator_notes";
/// Most changes one note-changes proposal may hold.
pub const MAX_NOTE_CHANGES: usize = 30;
/// Longest project brief, in bytes.
pub const MAX_BRIEF_BYTES: usize = 16 * 1024;

/// The `input` of a [`COORDINATOR_BRIEF_TOOL`] approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BriefProposalInput {
    /// The proposed brief, markdown.
    pub text: String,
    /// The project already has a brief that saving replaces.
    pub replaces: bool,
}

/// One proposed change in a [`SettingsProposalInput`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SettingChange {
    /// Stable id of the field, and the value `accepted` lists: `lead_autonomy`,
    /// `lead_workspace`, `lead_push_without_asking`, `lead_max_children` or
    /// `role_defaults.<role>` (research, implementation, review, design, test).
    pub path: String,
    /// Short label for the row, e.g. `Autonomy`.
    pub label: String,
    /// The effective value today. Enum fields are their snake_case strings; a
    /// role default is a [`RoleDefault`] object, or `null` when none is set.
    pub old: Value,
    /// The proposed value, in the same shape.
    pub new: Value,
}

/// The `input` of a [`COORDINATOR_SETTINGS_TOOL`] approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SettingsProposalInput {
    /// Why the Coordinator proposes these, in its words.
    pub reason: String,
    pub changes: Vec<SettingChange>,
}

/// What one row of a [`NoteChangesProposalInput`] does to its note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NoteChangeOp {
    /// Add `text` as a new dated claim.
    Append,
    /// Add `text` and retire the line that holds `supersedes`: it is struck through
    /// and marked superseded.
    Supersede,
}

/// One proposed change in a [`NoteChangesProposalInput`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NoteChange {
    /// Stable id of the row, and the value `accepted` lists. The daemon numbers rows from 1.
    pub path: String,
    pub note_id: crate::methods::NoteId,
    /// The note's title, for the row.
    pub note_title: String,
    pub op: NoteChangeOp,
    /// The markdown added to the note, a dated claim.
    pub text: String,
    /// For `Supersede`: the exact line (or the part of it) that is retired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    /// Sets the note's category when it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<crate::methods::NoteCategory>,
}

/// The `input` of a [`COORDINATOR_NOTES_TOOL`] approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NoteChangesProposalInput {
    /// Why the Coordinator proposes these, in its words.
    pub reason: String,
    pub changes: Vec<NoteChange>,
}

/// Whether an approval with this `tool_name` asks the user a question or for
/// input, rather than for permission to run a tool.
pub fn is_user_input_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "AskUserQuestion"
            | SECRET_REQUEST_TOOL
            | VISUAL_ANSWER_TOOL
            | PICK_REQUEST_TOOL
            | LOGIN_REQUEST_TOOL
            | PROMPT_REQUEST_TOOL
            | PERMISSION_REQUEST_TOOL
            | COORDINATOR_BRIEF_TOOL
            | COORDINATOR_SETTINGS_TOOL
            | COORDINATOR_NOTES_TOOL
            | crate::PROJECT_PROVISION_TOOL
            | "request_user_input"
            | "opencode_question"
            | crate::CURSOR_QUESTION_TOOL
            | "mcp_elicitation"
            | "ui_select"
            | "ui_confirm"
            | "ui_input"
            | "ui_editor"
    )
}

impl ApprovalRequest {
    pub fn is_user_input(&self) -> bool {
        is_user_input_tool(&self.tool_name)
    }

    pub fn validate_decision(&self, decision: &ApprovalDecision) -> Result<(), String> {
        let ApprovalDecision::Submit { response } = decision else {
            return if self.is_user_input() && !matches!(decision, ApprovalDecision::Deny { .. }) {
                Err("This request needs an answer. Submit a response or decline.".into())
            } else {
                Ok(())
            };
        };
        let invalid = || "The response does not match this request. Check the answers and try again.".to_string();
        match self.tool_name.as_str() {
            "AskUserQuestion" | "request_user_input" | "opencode_question" => {
                let questions = self.input.get("questions").and_then(Value::as_array).ok_or_else(invalid)?;
                for (index, question) in questions.iter().enumerate() {
                    let answer = if self.tool_name == "AskUserQuestion" {
                        response.get("answers").and_then(|a| a.get(question.get("question").and_then(Value::as_str).unwrap_or("")))
                    } else if self.tool_name == "request_user_input" {
                        response
                            .get("answers")
                            .and_then(|a| a.get(question.get("id").and_then(Value::as_str).unwrap_or("")))
                            .and_then(|a| a.get("answers"))
                    } else {
                        response.get("answers").and_then(|a| a.get(index))
                    };
                    let valid = if self.tool_name == "AskUserQuestion" {
                        // A multi-select answer is a list of labels; Claude also takes the older comma-joined text.
                        answer.is_some_and(|answer| match answer {
                            Value::String(s) => !s.trim().is_empty(),
                            Value::Array(items) => {
                                !items.is_empty() && items.iter().all(|v| v.as_str().is_some_and(|s| !s.trim().is_empty()))
                            }
                            _ => false,
                        })
                    } else {
                        answer
                            .and_then(Value::as_array)
                            .is_some_and(|a| !a.is_empty() && a.iter().all(|v| v.as_str().is_some_and(|s| !s.trim().is_empty())))
                    };
                    if !valid {
                        return Err(invalid());
                    }
                }
            }
            crate::CURSOR_QUESTION_TOOL => {
                let questions = self.user_input_questions();
                let answers = response.get("answers").and_then(Value::as_array).ok_or_else(invalid)?;
                for question in &questions {
                    let chosen = answers
                        .iter()
                        .find(|answer| answer.get("questionId").and_then(Value::as_str) == Some(question.id.as_str()))
                        .and_then(|answer| answer.get("selectedOptionIds"))
                        .and_then(Value::as_array)
                        .ok_or_else(invalid)?;
                    let known = |id: &Value| {
                        question.options.iter().any(|option| Some(option.value.as_deref().unwrap_or(&option.label)) == id.as_str())
                    };
                    if chosen.is_empty() || (!question.multi_select && chosen.len() > 1) || !chosen.iter().all(known) {
                        return Err(invalid());
                    }
                }
            }
            "ui_confirm" => {
                if !response.get("confirmed").is_some_and(Value::is_boolean) {
                    return Err(invalid());
                }
            }
            "ui_input" | "ui_editor" => {
                if !response.get("value").is_some_and(Value::is_string) {
                    return Err(invalid());
                }
            }
            "ui_select" => {
                if !response
                    .get("value")
                    .is_some_and(|value| self.input.get("options").and_then(Value::as_array).is_some_and(|options| options.contains(value)))
                {
                    return Err(invalid());
                }
            }
            "mcp_elicitation" => {
                if response.get("action").and_then(Value::as_str) != Some("accept") {
                    return Err(invalid());
                }
                if self.input.get("mode").and_then(Value::as_str) != Some("url") {
                    let content = response.get("content").filter(|v| v.is_object()).ok_or_else(invalid)?;
                    let schema = self.input.get("requestedSchema").or_else(|| self.input.get("requested_schema")).ok_or_else(invalid)?;
                    validate_form_value(schema, content)?;
                }
            }
            SECRET_REQUEST_TOOL => {
                let secret = response.get("secret").and_then(Value::as_str);
                if !secret.is_some_and(|secret| !secret.is_empty() && secret.len() <= MAX_SECRET_BYTES && !secret.contains('\0')) {
                    return Err(format!("Enter a value of up to {} KiB, or decline.", MAX_SECRET_BYTES / 1024));
                }
            }
            VISUAL_ANSWER_TOOL => {
                if let Some(reply) = response.get("reply") {
                    if !reply.as_str().is_some_and(|reply| !reply.trim().is_empty() && reply.len() <= 64 * 1024) {
                        return Err("Write a reply, or skip.".into());
                    }
                    return Ok(());
                }
                let answer = response.get("answer").filter(|answer| !answer.is_null()).ok_or_else(invalid)?;
                if serde_json::to_vec(answer).map_or(true, |bytes| bytes.len() > MAX_VISUAL_ANSWER_BYTES) {
                    return Err(format!("The answer is larger than {} KiB. Choose again in the visual.", MAX_VISUAL_ANSWER_BYTES / 1024));
                }
                if response.get("label").is_some_and(|label| !label.as_str().is_some_and(|label| label.chars().count() <= 200)) {
                    return Err(invalid());
                }
                if let Some(schema) = self.input.get("schema").filter(|schema| schema.is_object()) {
                    validate_answer_value(schema, answer)?;
                }
            }
            PICK_REQUEST_TOOL => {
                let small = serde_json::to_vec(response).is_ok_and(|bytes| bytes.len() <= MAX_PICK_RESPONSE_BYTES);
                if !(response.is_object() && small) {
                    return Err("Pick something again, or decline.".into());
                }
            }
            PROMPT_REQUEST_TOOL => {
                let text = |key: &str| response.get(key).and_then(Value::as_str);
                let valid = if self.input.get("confirm").and_then(Value::as_bool) == Some(true) {
                    response.get("confirmed").is_some_and(Value::is_boolean)
                } else if self.input.get("secret").and_then(Value::as_bool) == Some(true) {
                    text("secret")
                        .is_some_and(|secret| !secret.is_empty() && secret.len() <= MAX_PROMPT_ANSWER_BYTES && !secret.contains('\0'))
                } else {
                    text("value").is_some_and(|value| value.len() <= MAX_PROMPT_ANSWER_BYTES && !value.contains('\0'))
                };
                if !valid {
                    return Err("Enter an answer, or cancel.".into());
                }
            }
            // The daemon resolves these cards; a client can only decline them.
            LOGIN_REQUEST_TOOL | PERMISSION_REQUEST_TOOL => return Err("Use the card's buttons, or decline.".into()),
            COORDINATOR_BRIEF_TOOL => {
                let text = response.get("text").and_then(Value::as_str).map(str::trim);
                if !text.is_some_and(|text| !text.is_empty() && text.len() <= MAX_BRIEF_BYTES) {
                    return Err(format!("Write a brief of up to {} KiB, or decline.", MAX_BRIEF_BYTES / 1024));
                }
            }
            COORDINATOR_NOTES_TOOL => {
                let known: Vec<&str> = self
                    .input
                    .get("changes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|change| change.get("path").and_then(Value::as_str))
                    .collect();
                let accepted = response.get("accepted").and_then(Value::as_array);
                let valid = accepted.is_some_and(|accepted| {
                    !accepted.is_empty() && accepted.iter().all(|path| path.as_str().is_some_and(|path| known.contains(&path)))
                });
                if !valid {
                    return Err("Keep at least one change ticked, or decline.".into());
                }
            }
            COORDINATOR_SETTINGS_TOOL => {
                let known: Vec<&str> = self
                    .input
                    .get("changes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|change| change.get("path").and_then(Value::as_str))
                    .collect();
                let accepted = response.get("accepted").and_then(Value::as_array);
                let valid = accepted.is_some_and(|accepted| {
                    !accepted.is_empty() && accepted.iter().all(|path| path.as_str().is_some_and(|path| known.contains(&path)))
                });
                if !valid {
                    return Err("Keep at least one change ticked, or decline.".into());
                }
            }
            crate::PROJECT_PROVISION_TOOL => {
                let path = response.get("path").and_then(Value::as_str).map(str::trim);
                if !path.is_some_and(|path| !path.is_empty() && path.len() <= 4096 && !path.contains('\0')) {
                    return Err("Choose a folder on that machine, or cancel.".into());
                }
                let known: Vec<&str> = self
                    .input
                    .pointer("/plan/env_files")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|file| file.get("path").and_then(Value::as_str))
                    .collect();
                let files = response.get("env_files").and_then(Value::as_array);
                if !files.is_some_and(|files| files.iter().all(|file| file.as_str().is_some_and(|file| known.contains(&file)))) {
                    return Err(invalid());
                }
            }
            // Kybern's computer-use consent: `{"scope":"always"}` remembers the app.
            "kybern_computer_use" => {
                if response.get("scope").and_then(Value::as_str) != Some("always") {
                    return Err(invalid());
                }
            }
            _ => return Err("This request needs a permission decision.".into()),
        }
        Ok(())
    }
}

impl ApprovalRequest {
    /// The decision as it may be stored, broadcast or logged. Secret answers
    /// (a [`SECRET_REQUEST_TOOL`] value, and Codex `request_user_input` answers
    /// to questions marked `isSecret`) are replaced by [`REDACTED_SECRET`];
    /// everything else is returned unchanged. Only the live delivery to the
    /// waiting tool or provider may use the original.
    pub fn redacted_decision(&self, decision: &ApprovalDecision) -> ApprovalDecision {
        let ApprovalDecision::Submit { response } = decision else { return decision.clone() };
        match self.tool_name.as_str() {
            SECRET_REQUEST_TOOL => ApprovalDecision::Submit { response: serde_json::json!({ "secret": REDACTED_SECRET }) },
            PROMPT_REQUEST_TOOL if response.get("secret").is_some() => {
                ApprovalDecision::Submit { response: serde_json::json!({ "secret": REDACTED_SECRET }) }
            }
            "request_user_input" => {
                let secret_ids: Vec<&str> = self
                    .input
                    .get("questions")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|question| question.get("isSecret").and_then(Value::as_bool) == Some(true))
                    .filter_map(|question| question.get("id").and_then(Value::as_str))
                    .collect();
                let mut response = response.clone();
                if let Some(answers) = response.get_mut("answers").and_then(Value::as_object_mut) {
                    for (id, answer) in answers.iter_mut() {
                        if secret_ids.contains(&id.as_str()) {
                            *answer = serde_json::json!({ "answers": [REDACTED_SECRET] });
                        }
                    }
                }
                ApprovalDecision::Submit { response }
            }
            _ => decision.clone(),
        }
    }
}

// Elicitation schemas are provider-owned. Validate the structural constraints at
// the daemon boundary as well as the form, so CLI and concurrent clients agree.
/// A visual's answer against the agent's `answer_schema` (the form subset elicitation supports).
fn validate_answer_value(schema: &Value, value: &Value) -> Result<(), String> {
    validate_form_value(schema, value)
        .map_err(|_| "This answer does not match what the agent asked for. Choose again in the visual.".into())
}

fn validate_form_value(schema: &Value, value: &Value) -> Result<(), String> {
    let invalid = || "Check the form values and required fields before submitting.".to_string();
    let valid_type = match schema.get("type").and_then(Value::as_str) {
        Some("object") => value.is_object(),
        Some("array") => value.is_array(),
        Some("string") => value.is_string(),
        Some("boolean") => value.is_boolean(),
        Some("number") => value.is_number(),
        Some("integer") => value.as_i64().is_some() || value.as_u64().is_some(),
        _ => true,
    };
    if !valid_type {
        return Err(invalid());
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array)
        && !options.contains(value)
    {
        return Err(invalid());
    }
    if let Some(options) = schema.get("oneOf").and_then(Value::as_array)
        && !options.iter().any(|option| option.get("const") == Some(value))
    {
        return Err(invalid());
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for key in schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if value.get(key).is_none() {
                return Err(format!("Provide a value for {key}."));
            }
        }
        for (key, child) in properties {
            if let Some(field) = value.get(key) {
                validate_form_value(child, field)?;
            }
        }
    }
    if let Some(values) = value.as_array() {
        if schema.get("minItems").and_then(Value::as_u64).is_some_and(|n| values.len() < n as usize)
            || schema.get("maxItems").and_then(Value::as_u64).is_some_and(|n| values.len() > n as usize)
        {
            return Err(invalid());
        }
        if let Some(items) = schema.get("items") {
            for item in values {
                validate_form_value(items, item)?;
            }
        }
    }
    if let Some(number) = value.as_f64()
        && (schema.get("minimum").and_then(Value::as_f64).is_some_and(|n| number < n)
            || schema.get("maximum").and_then(Value::as_f64).is_some_and(|n| number > n))
    {
        return Err(invalid());
    }
    if let Some(text) = value.as_str()
        && (schema.get("minLength").and_then(Value::as_u64).is_some_and(|n| text.chars().count() < n as usize)
            || schema.get("maxLength").and_then(Value::as_u64).is_some_and(|n| text.chars().count() > n as usize))
    {
        return Err(invalid());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Answer a structured question or form using the provider-native payload.
    Submit {
        response: Value,
    },
    AllowOnce,
    /// Allow and, where the provider supports it, remember for the session.
    AllowAlways,
    Deny {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// A tool invocation as shown in the transcript.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Completed,
    Interrupted,
    MaxTurns,
    Error,
}

/// A durable, sandboxed HTML reply. Source bytes are stored separately from events.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct HtmlVisual {
    pub id: uuid::Uuid,
    pub title: String,
    /// Agent-requested height cap in CSS pixels.
    pub height: u32,
    /// Content heights measured at publish, ascending by width. Empty when the
    /// preview browser was not installed or measuring exceeded its budget.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub heights: Vec<VisualHeight>,
    /// Set when the agent waits for the reader's answer (`kybern_html_publish`
    /// with `await_answer`). The pending question is a [`VISUAL_ANSWER_TOOL`] card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask: Option<VisualAsk>,
}

/// What a visual asks the reader.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct VisualAsk {
    /// JSON Schema the answer must match, when the agent gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
}

/// How a visual's question ended.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VisualAnswerStatus {
    /// The reader sent an answer from the visual.
    Answered,
    /// The reader typed a message instead.
    Replied,
    /// The reader chose Skip.
    Skipped,
    /// The turn ended first.
    Cancelled,
    /// Nobody answered in time.
    TimedOut,
}

/// The receipt under an answered visual.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct VisualAnswerRecord {
    pub status: VisualAnswerStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<Value>,
    /// The words the visual gave the answer ("Layout B · Side panel").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub at: DateTime<Utc>,
}

/// One content height measured at a frame width, in CSS pixels.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct VisualHeight {
    pub width: u32,
    pub height: u32,
}

/// Why a Coordinator's provider session was replaced (`ContextRotationStarted`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RotationReason {
    /// The session's context passed the rotation budget at the end of a turn.
    Budget,
    /// The session was released for idleness past half the budget.
    Idle,
    /// The user asked for a refresh (`coordinator.refresh_context`).
    Manual,
    /// The Coordinator moved to another harness, model or account.
    HarnessSwitch,
}

impl RotationReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RotationReason::Budget => "budget",
            RotationReason::Idle => "idle",
            RotationReason::Manual => "manual",
            RotationReason::HarnessSwitch => "harness_switch",
        }
    }
}

/// Whether the old session wrote a handoff that passed validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum HandoffStatus {
    Written,
    /// No usable handoff; the fresh session gets the facts and recent messages only.
    Missing {
        why: String,
    },
}

/// One repository's HEAD when a rotation happened.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepoHead {
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub sha: String,
}

/// Counts of what the daemon carried into a fresh Coordinator session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RotationCarried {
    pub children: u32,
    pub pull_requests: u32,
    pub tasks: u32,
    pub commits: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub head: Vec<RepoHead>,
}

/// The harness and model on one side of a rotation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HarnessRef {
    pub kind: ProviderKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Rendered transcript entries, projected from events by the daemon.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum TranscriptEntry {
    Visual {
        turn_id: TurnId,
        seq: EventSeq,
        at: DateTime<Utc>,
        visual: HtmlVisual,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answer: Option<VisualAnswerRecord>,
    },
    Image {
        id: String,
        turn_id: TurnId,
        seq: EventSeq,
        at: DateTime<Utc>,
        origin: EventOrigin,
        source: String,
    },
    User {
        id: MessageId,
        turn_id: TurnId,
        #[serde(default)]
        seq: EventSeq,
        message: UserMessage,
        at: DateTime<Utc>,
    },
    Assistant {
        id: MessageId,
        turn_id: TurnId,
        #[serde(default)]
        seq: EventSeq,
        #[serde(default)]
        origin: EventOrigin,
        /// Segment index within one `message_id`. A provider can keep a single
        /// message id across a tool call (Claude streams preamble → tool →
        /// answer under one id); the projection splits the text at each
        /// row-making event so post-tool prose is its own entry, ordered after
        /// the tool. `0` is the first/only segment. `(id, segment)` is the
        /// stable identity a client keys on.
        #[serde(default)]
        segment: u32,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
        /// The provider closed this segment's reasoning before the message
        /// completed. Only meaningful while `complete` is false.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        thinking_complete: bool,
        at: DateTime<Utc>,
        complete: bool,
    },
    ToolCall {
        turn_id: TurnId,
        #[serde(default)]
        seq: EventSeq,
        #[serde(default)]
        origin: EventOrigin,
        call: ToolCall,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<Value>,
        /// Settled result exists in the event log but was not inlined here.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        output_omitted: bool,
        /// Settled output-delta stream exists in the event log but was not
        /// inlined here. Fetch it through `threads.tool_output`.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        stream_omitted: bool,
        is_error: bool,
        complete: bool,
        at: DateTime<Utc>,
    },
    Approval {
        turn_id: TurnId,
        #[serde(default)]
        seq: EventSeq,
        approval: ApprovalRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decision: Option<ApprovalDecision>,
        /// Allowed by Full access without asking the user.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        auto: bool,
        /// The delegating agent that answered for the user.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answered_by: Option<crate::RequestAnswerer>,
    },
    TurnSummary {
        turn_id: TurnId,
        #[serde(default)]
        seq: EventSeq,
        stop_reason: StopReason,
        usage: Usage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
        duration_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminal_message_id: Option<MessageId>,
        at: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Stable launch row for a provider-owned task. Later snapshots update the
    /// task in place; `seq` remains the first event that made it visible.
    RuntimeTask {
        turn_id: TurnId,
        #[serde(default)]
        seq: EventSeq,
        task: RuntimeTask,
        at: DateTime<Utc>,
    },
    Notice {
        turn_id: TurnId,
        #[serde(default)]
        seq: EventSeq,
        level: NoticeLevel,
        text: String,
        at: DateTime<Utc>,
        /// What the provider notice carried (a fork notice names its machine and thread). Absent for plain notices.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
    Reverted {
        turn_id: TurnId,
        #[serde(default)]
        seq: EventSeq,
        commit: String,
        at: DateTime<Utc>,
    },
    /// Deselected repositories of a group worktree thread changed outside its
    /// worktree during this turn. Kybern never reverts them; clients show a quiet warning.
    OutsideRepoChanged {
        turn_id: TurnId,
        #[serde(default)]
        seq: EventSeq,
        repos: Vec<OutsideRepoChange>,
        at: DateTime<Utc>,
    },
    /// A Coordinator context refresh. `pending` while the old session is still
    /// writing its handoff; then the outcome, with the handoff the user may edit.
    Rotation {
        /// The turn the refresh followed (nil before the first turn).
        turn_id: TurnId,
        #[serde(default)]
        seq: EventSeq,
        rotation_id: String,
        reason: RotationReason,
        pending: bool,
        used_tokens: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        handoff: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        handoff_status: Option<HandoffStatus>,
        /// The user edited the handoff after the refresh.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        edited: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        carried: Option<RotationCarried>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<HarnessRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<HarnessRef>,
        at: DateTime<Utc>,
    },
    /// What a thread moved here with "Continue on…" said and did on its first machine.
    History {
        turn_id: TurnId,
        #[serde(default)]
        seq: EventSeq,
        from_machine: crate::MachineId,
        from_machine_name: String,
        from_thread: ThreadId,
        from_title: String,
        items: Vec<crate::HistoryItem>,
        #[serde(default, skip_serializing_if = "is_zero_u32")]
        omitted: u32,
        at: DateTime<Utc>,
    },
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

/// Per-provider configuration in settings.json.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderSettings {
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub accounts: std::collections::BTreeMap<String, ProviderAccount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_account: Option<String>,
    /// Account overrides keyed by the registered project path.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub project_accounts: std::collections::BTreeMap<String, String>,
    /// Absolute path to the executable; omit to look it up on PATH.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// Default model for new threads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Extra environment variables for the provider process.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    /// OMP profiles keyed by the registered project's absolute path. Worktrees
    /// inherit their project's profile; an empty value selects OMP's default.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub project_profiles: std::collections::BTreeMap<String, String>,
}

/// User settings persisted at `<data_dir>/settings.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Settings {
    pub default_provider: ProviderKind,
    pub default_permission_mode: PermissionMode,
    /// Global default for new threads; projects can override.
    pub worktrees_default: bool,
    /// Remove clean, merged, inactive managed worktrees when their thread is archived.
    pub automatic_worktree_cleanup: bool,
    /// Generate thread titles with a model after the first turn.
    pub generate_titles: bool,
    /// Provider used for titles; falls back to the thread's own provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title_provider: Option<ProviderKind>,
    pub providers: std::collections::BTreeMap<ProviderKind, ProviderSettings>,
    /// Show OS notifications when a turn ends or needs approval.
    pub notifications: bool,
    /// Check installed harnesses daily and update them when idle. Opt-in per environment.
    pub auto_update_harnesses: bool,
    /// Check the release feed daily and replace this daemon when idle. Opt-in
    /// per environment; ignored for a daemon the desktop app manages.
    pub auto_update_daemon: bool,
    /// Which releases the desktop app and a self-updating daemon move to.
    pub update_track: UpdateTrack,
    /// How the daemon trims CPU and memory while nothing needs it.
    pub background: BackgroundSettings,
    /// Which networks other devices can reach this daemon on.
    pub access: AccessSettings,
    /// Desktop control through CuaDriver for non-Codex harnesses.
    pub computer_use: ComputerUseSettings,
    /// Antigravity's link to Kybern's tools. Per machine: it is one entry in this machine's `agy` MCP servers.
    pub antigravity: AntigravitySettings,
    /// Limits on agents delegating work to other agents.
    pub orchestration: OrchestrationSettings,
    /// Give new agent sessions a short guide to Kybern: images, notes and
    /// tasks, other threads, helpers. Applies to sessions started after the change.
    pub tell_agents_about_kybern: bool,
    /// Whether an agent's question may carry a default and a wait, and the
    /// longest wait it can set (`kybern_ask_user`).
    pub ask_deadlines: AskDeadlineSettings,
    /// Folders outside a project the user allowed the Preview panel to serve
    /// files from. Canonical absolute paths; a grant covers descendants.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preview_allowed_folders: Vec<String>,
    /// Joining and staying joined to the machine network.
    pub network: NetworkSettings,
}

/// The release line updates come from. Every release reaches Dev first as a
/// `vX.Y.Z-dev.N` prerelease; Stable gets a version later, once it has shipped
/// on Dev. A machine never moves to an older version: after switching from Dev
/// to Stable it keeps its dev build until a stable release is newer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UpdateTrack {
    #[default]
    Stable,
    Dev,
}

/// Limits for questions that answer themselves after a deadline. Read each
/// time an agent asks. Approvals, passwords, secrets, sign-ins and
/// permissions never have a deadline, whatever these say.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct AskDeadlineSettings {
    /// Let an agent give a question a default and a wait. Off: the question waits for the user.
    pub enabled: bool,
    /// The longest wait an agent can set, in seconds (60..=14400).
    pub max_seconds: u64,
}

impl Default for AskDeadlineSettings {
    fn default() -> Self {
        Self { enabled: true, max_seconds: 15 * 60 }
    }
}

/// Limits for `kybern_agent_delegate`. Read each time an agent delegates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct OrchestrationSettings {
    /// Most delegated children one thread may have running at once (1..=16).
    pub max_active_children: u32,
    /// The same limit when the delegating thread is a project's Coordinator (1..=16).
    pub lead_max_children: u32,
    /// Deepest chain of delegation: 1 means only top-level threads delegate (1..=4).
    pub max_depth: u32,
    /// The harness, model and effort a Coordinator uses for a kind of work when
    /// it does not ask for another. Keys: research, implementation, review,
    /// design and test (`general` has no default). Projects override per role.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub role_defaults: BTreeMap<DelegationRole, RoleDefault>,
    /// How freely the Coordinator acts. Projects can override it.
    pub lead_autonomy: LeadAutonomy,
    /// Where the Coordinator's children work when it does not say.
    pub lead_workspace: DelegationWorkspace,
    /// Let the Coordinator push and open pull requests without asking first.
    /// Merging a pull request always asks.
    pub lead_push_without_asking: bool,
    /// Start a fresh Coordinator session, with a handoff, once its context passes
    /// this many tokens (at most half the model's window). 0 turns it off;
    /// otherwise at least 50 000. Projects can override it.
    pub rotation_budget_tokens: u64,
}

/// The built-in Coordinator context budget.
pub const DEFAULT_ROTATION_BUDGET_TOKENS: u64 = 200_000;
/// The smallest budget other than 0 (off).
pub const MIN_ROTATION_BUDGET_TOKENS: u64 = 50_000;

/// Why a context budget cannot be saved, if it cannot.
pub fn validate_rotation_budget(tokens: u64) -> Result<(), String> {
    if tokens != 0 && tokens < MIN_ROTATION_BUDGET_TOKENS {
        return Err("The Coordinator context budget must be 0 (off) or at least 50 000 tokens. Change the value and save again.".into());
    }
    Ok(())
}

impl Default for OrchestrationSettings {
    fn default() -> Self {
        Self {
            max_active_children: 4,
            lead_max_children: 6,
            max_depth: 2,
            role_defaults: BTreeMap::new(),
            lead_autonomy: LeadAutonomy::default(),
            lead_workspace: DelegationWorkspace::Shared,
            lead_push_without_asking: false,
            rotation_budget_tokens: DEFAULT_ROTATION_BUDGET_TOKENS,
        }
    }
}

/// How freely a Coordinator acts before asking the user.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LeadAutonomy {
    /// Acts on its own judgment and reports.
    Act,
    /// Acts on small work; proposes a plan first for anything large or risky.
    #[default]
    ProposeLarge,
    /// Proposes a plan before every piece of work.
    ProposeFirst,
}

impl LeadAutonomy {
    pub fn as_str(self) -> &'static str {
        match self {
            LeadAutonomy::Act => "act",
            LeadAutonomy::ProposeLarge => "propose_large",
            LeadAutonomy::ProposeFirst => "propose_first",
        }
    }

    /// The label users see.
    pub fn label(self) -> &'static str {
        match self {
            LeadAutonomy::Act => "Act",
            LeadAutonomy::ProposeLarge => "Propose large changes",
            LeadAutonomy::ProposeFirst => "Propose first",
        }
    }
}

/// The harness, model and effort used for one kind of delegated work. Every
/// field is optional: an unset one follows the delegating thread.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RoleDefault {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderInstance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// A project's overrides of the Coordinator settings. Absent fields fall back
/// to [`OrchestrationSettings`]. `role_defaults` replaces the global entry of
/// each role it lists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoordinatorOverrides {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub role_defaults: BTreeMap<DelegationRole, RoleDefault>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead_autonomy: Option<LeadAutonomy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead_workspace: Option<DelegationWorkspace>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead_push_without_asking: Option<bool>,
    /// Most delegated children the Coordinator may have running (1..=16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead_max_children: Option<u32>,
    /// Context budget for this project's Coordinator (0 is off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation_budget_tokens: Option<u64>,
}

impl CoordinatorOverrides {
    /// Whether nothing is overridden.
    pub fn is_empty(&self) -> bool {
        self.role_defaults.is_empty()
            && self.lead_autonomy.is_none()
            && self.lead_workspace.is_none()
            && self.lead_push_without_asking.is_none()
            && self.lead_max_children.is_none()
            && self.rotation_budget_tokens.is_none()
    }

    /// Why these overrides cannot be saved, if they cannot.
    pub fn validate(&self) -> Result<(), String> {
        if self.role_defaults.contains_key(&DelegationRole::General) {
            return Err("The general role has no default model. Remove it and save again.".into());
        }
        if self.lead_max_children.is_some_and(|children| !(1..=16).contains(&children)) {
            return Err("Active agents per Coordinator must be between 1 and 16. Change the value and save again.".into());
        }
        if let Some(tokens) = self.rotation_budget_tokens {
            validate_rotation_budget(tokens)?;
        }
        for (role, default) in &self.role_defaults {
            if default.model.as_deref().is_some_and(|model| model.trim().is_empty() || model.len() > 256)
                || default.effort.as_deref().is_some_and(|effort| effort.trim().is_empty() || effort.len() > 128)
            {
                return Err(format!("The {role} default has an empty or oversized model or effort. Fix it and save again."));
            }
        }
        Ok(())
    }
}

/// The Coordinator settings in force for one project: its overrides, then the
/// global values, then the built-in defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EffectiveCoordinatorSettings {
    pub role_defaults: BTreeMap<DelegationRole, RoleDefault>,
    pub lead_autonomy: LeadAutonomy,
    pub lead_workspace: DelegationWorkspace,
    pub lead_push_without_asking: bool,
    pub lead_max_children: u32,
    /// 0 when context refresh is off.
    #[serde(default)]
    pub rotation_budget_tokens: u64,
}

impl OrchestrationSettings {
    /// Resolve the settings for a project: its override, else the global value.
    pub fn effective_for(&self, project: Option<&CoordinatorOverrides>) -> EffectiveCoordinatorSettings {
        let mut role_defaults = self.role_defaults.clone();
        if let Some(project) = project {
            for (role, default) in &project.role_defaults {
                role_defaults.insert(*role, default.clone());
            }
        }
        EffectiveCoordinatorSettings {
            role_defaults,
            lead_autonomy: project.and_then(|p| p.lead_autonomy).unwrap_or(self.lead_autonomy),
            lead_workspace: project.and_then(|p| p.lead_workspace).unwrap_or(self.lead_workspace),
            lead_push_without_asking: project.and_then(|p| p.lead_push_without_asking).unwrap_or(self.lead_push_without_asking),
            lead_max_children: project.and_then(|p| p.lead_max_children).unwrap_or(self.lead_max_children),
            rotation_budget_tokens: project.and_then(|p| p.rotation_budget_tokens).unwrap_or(self.rotation_budget_tokens),
        }
    }

    /// Why these settings cannot be saved, if they cannot.
    pub fn validate_coordinator(&self) -> Result<(), String> {
        validate_rotation_budget(self.rotation_budget_tokens)?;
        CoordinatorOverrides { role_defaults: self.role_defaults.clone(), ..Default::default() }.validate()
    }
}

/// Agents drive apps on this computer through a separately installed
/// program: CuaDriver or agent-desktop on macOS, agent-desktop on Windows and
/// Linux. Codex keeps OpenAI's own computer-use plugin where it has one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ComputerUseSettings {
    /// Offer the computer tools to new agent sessions.
    pub enabled: bool,
    /// Whether an agent may ask to use the real cursor and focus.
    pub foreground: ComputerForeground,
    /// Draw CuaDriver's agent cursor where agents act. Off by default: the
    /// cursor overlay in CuaDriver 0.28 can hold over a gigabyte of
    /// screen-sized frames.
    pub show_cursor: bool,
    /// Apps (display names) agents may control in the background without
    /// asking. Filled by "Always allow" on the approval card.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub always_allowed_apps: Vec<String>,
    /// The program behind the tools on this machine. Unset means the OS
    /// default: CuaDriver on macOS, agent-desktop on Windows and Linux. A
    /// driver this OS lacks falls back to the default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<ComputerDriver>,
}

/// A program that reads and drives apps for the computer tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComputerDriver {
    /// trycua's CuaDriver.app (macOS).
    CuaDriver,
    /// vercel-labs' agent-desktop CLI (macOS, Windows, Linux).
    AgentDesktop,
}

/// Antigravity has no per-session MCP configuration, so Kybern keeps one entry,
/// `kybern`, in `agy`'s own MCP servers (added and removed with `agy mcp`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct AntigravitySettings {
    /// Let Antigravity sessions use Kybern's tools through that entry. On by
    /// default: a session adds the entry when it starts, and turning this off
    /// removes it.
    pub kybern_tools: bool,
}

impl Default for AntigravitySettings {
    fn default() -> Self {
        Self { kybern_tools: true }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComputerForeground {
    /// Background by default; foreground after the user approves it per app.
    #[default]
    Ask,
    /// Background only.
    Never,
}

/// Extra listeners the daemon opens besides its loopback port. Persisted so
/// a restart keeps the daemon reachable for paired phones.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct AccessSettings {
    /// Also listen on this machine's Tailscale address, so devices on the
    /// same tailnet can pair and connect directly.
    pub tailscale: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_provider: ProviderKind::ClaudeCode,
            default_permission_mode: PermissionMode::Supervised,
            worktrees_default: false,
            automatic_worktree_cleanup: false,
            generate_titles: true,
            title_provider: None,
            providers: Default::default(),
            notifications: true,
            auto_update_harnesses: false,
            auto_update_daemon: false,
            update_track: UpdateTrack::Stable,
            background: BackgroundSettings::default(),
            access: AccessSettings::default(),
            computer_use: ComputerUseSettings::default(),
            antigravity: AntigravitySettings::default(),
            orchestration: OrchestrationSettings::default(),
            tell_agents_about_kybern: true,
            ask_deadlines: AskDeadlineSettings::default(),
            preview_allowed_folders: Vec::new(),
            network: NetworkSettings::default(),
        }
    }
}

/// Limits on what the daemon keeps alive after work finishes. Agent processes
/// are resumed from the provider's own session on the next message, so
/// releasing one loses no conversation. Every value is in minutes; `0` turns
/// that limit off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct BackgroundSettings {
    /// Minutes an idle thread keeps its agent process before the daemon
    /// releases it. Threads that are running, awaiting approval, or have
    /// background work are never released.
    pub session_idle_minutes: u32,
    /// Most idle agent processes kept warm at once. The least recently used
    /// are released first, even before their idle time is up.
    pub max_idle_sessions: u32,
    /// Minutes a shell with no attached client and no foreground job stays
    /// open before the daemon closes it.
    pub terminal_idle_minutes: u32,
    /// Minutes with no clients, agents, terminals, or queued work before the
    /// daemon exits. Only for daemons the desktop app starts on demand: the
    /// CLI and remote clients do not restart a daemon that has exited.
    pub daemon_idle_exit_minutes: u32,
    /// While the host runs on battery, release idle agent processes after
    /// `BATTERY_SESSION_IDLE` instead of `session_idle_minutes` and hold
    /// automatic harness updates until power returns.
    pub save_power_on_battery: bool,
}

impl Default for BackgroundSettings {
    fn default() -> Self {
        Self {
            session_idle_minutes: 10,
            max_idle_sessions: 4,
            terminal_idle_minutes: 60,
            daemon_idle_exit_minutes: 0,
            save_power_on_battery: false,
        }
    }
}

impl BackgroundSettings {
    fn minutes(value: u32) -> Option<std::time::Duration> {
        (value > 0).then(|| std::time::Duration::from_secs(u64::from(value) * 60))
    }
    /// `None` keeps idle agent processes indefinitely.
    pub fn session_idle(&self) -> Option<std::time::Duration> {
        Self::minutes(self.session_idle_minutes)
    }
    /// `None` keeps every idle agent process.
    pub fn idle_session_cap(&self) -> Option<usize> {
        (self.max_idle_sessions > 0).then_some(self.max_idle_sessions as usize)
    }
    /// `None` keeps detached shells open until they exit or are closed.
    pub fn terminal_idle(&self) -> Option<std::time::Duration> {
        Self::minutes(self.terminal_idle_minutes)
    }
    /// `None` keeps the daemon running until it is stopped.
    pub fn daemon_idle_exit(&self) -> Option<std::time::Duration> {
        Self::minutes(self.daemon_idle_exit_minutes)
    }
    /// Grace an idle agent process gets on battery before it is released:
    /// long enough to answer a follow-up without a resume, short enough to
    /// matter for the battery.
    pub const BATTERY_SESSION_IDLE: std::time::Duration = std::time::Duration::from_secs(60);
}

/// Why the daemon closed an agent process. The thread's conversation is kept
/// by the provider and resumes with the next message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionReleaseReason {
    /// The user released a parked process to reconnect on the next message.
    Manual,
    /// The thread was idle for longer than `background.session_idle_minutes`.
    Idle,
    /// More idle processes were alive than `background.max_idle_sessions`.
    Capacity,
    /// The harness was closed so it could be updated.
    Update,
    /// The host is on battery and `background.save_power_on_battery` is on.
    Power,
}

impl SessionReleaseReason {
    /// Transcript copy for the releases that can surprise. Idle and update
    /// releases are the expected end of a quiet thread and get no row.
    /// Mirrored in `packages/kybern-client/src/transcript.ts` for live events.
    pub fn notice_text(self) -> Option<&'static str> {
        match self {
            Self::Manual => Some("Agent released. Your next message resumes the saved conversation."),
            Self::Capacity => Some("Agent process closed to stay under the warm limit. Your next message resumes it."),
            Self::Power => Some("Agent process closed to save battery. Your next message resumes it."),
            Self::Idle | Self::Update => None,
        }
    }
}

/// Git snapshots bracketing one turn. `after` is absent while the turn runs.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Checkpoint {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    /// Commit hash of the working tree before the turn started.
    pub before: String,
    /// Commit hash after the turn ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// Provider-side id of this turn, for conversation rewind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_turn_id: Option<String>,
    /// Provider-side id of the last entry of this turn, for conversation rewind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_turn_end: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Group threads: one entry per included member, in project order. `before`
    /// and `after` above are the primary member's, so older clients keep working.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<RepoCheckpoint>,
    /// A member could not be snapshotted; rewinding to this turn is refused.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
}

/// One member's snapshots for a group thread's turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RepoCheckpoint {
    /// Member path ("." for the group folder).
    pub repo: String,
    pub before: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FileChange {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub additions: u32,
    pub deletions: u32,
    pub binary: bool,
    /// Group threads: the member this file belongs to. `path` stays relative to the
    /// thread cwd (`api/src/x.rs`), so links and `threads.files.read` work unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Diff {
    pub from: String,
    pub to: String,
    pub files: Vec<FileChange>,
    /// Unified diff text (`git diff --no-color`), empty when nothing changed.
    pub patch: String,
    /// True when an oversized patch was cut to the daemon's transport limit.
    #[serde(default)]
    pub patch_truncated: bool,
    /// Group threads: per-member summary in project order, including included
    /// members with no changes. `from`/`to` above are the primary member's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<RepoDiff>,
}

/// One member's totals in a group thread's diff.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RepoDiff {
    pub repo: String,
    pub from: String,
    pub to: String,
    pub files: u32,
    pub additions: u32,
    pub deletions: u32,
}

/// A deselected member whose original checkout changed during a turn of a
/// group worktree thread (see `EventPayload::OutsideRepoChanged`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OutsideRepoChange {
    /// Member path, relative to the project folder.
    pub repo: String,
    pub name: String,
    /// Absolute path of the original checkout that changed.
    pub source_path: String,
    /// Files `git status --porcelain` reported changed at the end of the turn.
    #[serde(default)]
    pub dirty_files: u32,
    /// HEAD moved during the turn (a commit, checkout or reset).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub head_moved: bool,
}

/// Last daemon-owned harness update attempt. Persisted across client reconnects.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HarnessUpdate {
    pub kind: ProviderKind,
    pub status: HarnessUpdateStatus,
    pub message: String,
    pub version: Option<String>,
    pub checked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HarnessUpdateStatus {
    NotChecked,
    Waiting,
    Updating,
    Updated,
    Current,
    Unsupported,
    Failed,
}

/// The daemon's own update state. The daemon replaces its binary from the
/// GitHub release feed and restarts under its service manager; persisted so
/// clients see the same record after a reconnect or the restart itself.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DaemonUpdate {
    pub status: DaemonUpdateStatus,
    pub message: String,
    /// Version of the running daemon.
    pub current_version: String,
    /// Newest published version, once a check has succeeded.
    pub latest_version: Option<String>,
    pub checked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DaemonUpdateStatus {
    NotChecked,
    Checking,
    Current,
    Available,
    Waiting,
    Updating,
    Restarting,
    Unsupported,
    Failed,
}

/// Authoritative context occupancy, distinct from cumulative billable tokens.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProviderUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Vec<UsageLimit>>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ContextUsage {
    pub used_tokens: u64,
    pub window_tokens: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UsageLimit {
    pub name: String,
    pub used_percent: f64,
    pub window_minutes: Option<u64>,
    pub resets_at: Option<i64>,
}

/// A provider question that does not pause the running turn.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AsyncQuestion {
    pub title: String,
    #[serde(default, deserialize_with = "deserialize_null_options")]
    pub options: Vec<String>,
}

fn deserialize_null_options<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    Ok(Option::<Vec<String>>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AsyncQuestionRequest {
    pub id: String,
    pub questions: Vec<AsyncQuestion>,
}

#[cfg(test)]
mod orchestration_tests {
    use super::*;

    #[test]
    fn antigravity_is_a_provider_kind_with_its_own_binary() {
        assert_eq!(ProviderKind::ALL.last(), Some(&ProviderKind::Antigravity));
        let kind: ProviderKind = "antigravity".parse().unwrap();
        assert_eq!((kind, kind.display_name(), kind.default_binary()), (ProviderKind::Antigravity, "Antigravity", "agy"));
        assert_eq!(serde_json::to_value(kind).unwrap(), "antigravity");
        for kind in ProviderKind::ALL {
            assert_eq!(kind.as_str().parse::<ProviderKind>().unwrap(), kind);
        }
    }

    #[test]
    fn provider_model_traits_are_additive() {
        let plain: ProviderModel = serde_json::from_str(r#"{"id":"m","display_name":"M","is_default":false}"#).unwrap();
        assert!(plain.parameters.is_empty() && plain.variants.is_empty());
        let encoded = serde_json::to_value(&plain).unwrap();
        assert!(encoded.get("parameters").is_none() && encoded.get("variants").is_none());

        let rich: ProviderModel = serde_json::from_str(
            r#"{"id":"v1","display_name":"Opus","resolved_id":"opus","efforts":["low"],"default_effort":"low","is_default":false,
                "parameters":[{"id":"fast","label":"Fast","default":"false","values":[{"value":"false","label":"Off"},{"value":"true","label":"On"}]}],
                "variants":[{"id":"v1","params":{"fast":"false"},"efforts":["low"],"default_effort":"low"},{"id":"v2","params":{"fast":"true"}}]}"#,
        )
        .unwrap();
        assert_eq!(rich.parameters[0].values[1].label, "On");
        assert_eq!(rich.variants[1].params["fast"], "true");
        assert!(rich.variants[1].efforts.is_empty());
    }

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn thread_reference_text_tells_the_agent_how_to_read_and_message_the_thread() {
        let text = thread_reference_text(id(1), "Fix login");
        assert_eq!(
            text,
            format!("@thread[{}] Fix login (you can read it with kybern_thread_read and message it with kybern_thread_send)", id(1))
        );
    }

    #[test]
    fn question_message_asks_for_a_reply_and_names_the_fallback() {
        let text = thread_message_text(id(9), Some(id(2)), "Parent", ThreadMessagePurpose::Question, None, "What is the port?");
        assert_eq!(
            text,
            format!(
                "Message from thread \"Parent\" ({}) · question · id {}:\nWhat is the port?\n\nReply with kybern_thread_send to {} with purpose \"reply\" and reply_to \"{}\". If you do not, your final message this turn is sent back as the reply.",
                id(2),
                id(9),
                id(2),
                id(9)
            )
        );
    }

    #[test]
    fn reply_task_and_kybern_warning_use_the_contract_wording() {
        let reply = thread_message_text(id(9), Some(id(2)), "Child", ThreadMessagePurpose::Reply, Some(id(7)), "8080");
        assert!(reply.starts_with(&format!(
            "Message from thread \"Child\" ({}) · reply · id {} · reply to {}:\n8080",
            id(2),
            id(9),
            id(7)
        )));
        assert!(reply.ends_with("\n\nNo acknowledgement needed."));

        let task = thread_message_text(id(9), Some(id(2)), "Parent", ThreadMessagePurpose::Task, None, "Do the thing");
        assert!(task.ends_with(":\nDo the thing"), "a task carries its own brief and no guidance: {task}");

        let warning = thread_message_text(id(9), None, "Kybern", ThreadMessagePurpose::Warning, None, "stop");
        assert_eq!(warning, format!("Message from Kybern · warning · id {}:\nstop\n\nNo acknowledgement needed.", id(9)));
    }

    fn item(workspace: DelegationWorkspace) -> AgentResultItem {
        AgentResultItem {
            task_id: id(3),
            thread_id: id(4),
            title: "Port the parser".into(),
            provider: ProviderKind::Codex,
            model: Some("gpt-5".into()),
            role: DelegationRole::Implementation,
            status: DelegationStatus::Completed,
            result: Some("Done. Tests pass.".into()),
            error: None,
            workspace,
            branch: Some("kybern/abc".into()),
            head_commit: Some("1234567".into()),
            base_commit: Some("abcdef0".into()),
            diffstat: Some(DiffStat { files: 2, additions: 10, deletions: 4 }),
            files_touched: vec!["a.rs".into(), "b.rs".into()],
            conflicts: Vec::new(),
        }
    }

    #[test]
    fn agent_results_list_each_agent_with_its_workspace_and_result() {
        let mut failed = item(DelegationWorkspace::Shared);
        failed.status = DelegationStatus::Failed;
        failed.result = None;
        failed.error = Some("boom".into());
        failed.files_touched.clear();
        failed.conflicts =
            vec![DelegationConflict { path: "src/x.rs".into(), owner_thread_id: id(5), at: chrono::DateTime::<Utc>::default() }];
        let text = agent_results_text(&[item(DelegationWorkspace::Worktree), item(DelegationWorkspace::Shared), failed]);
        assert!(text.starts_with("Results from 3 delegated agent(s):"));
        assert!(text.contains(&format!("- Port the parser (codex/gpt-5, implementation) — completed · task {} · thread {}", id(3), id(4))));
        assert!(
            text.contains(
                "branch kybern/abc at 1234567, based on abcdef0, 2 files +10 −4. If your checkout is clean, merge the branch with git."
            ),
            "{text}"
        );
        assert!(text.contains("`git diff abcdef0..1234567 | git apply --3way`"), "{text}");
        let mut bare = item(DelegationWorkspace::Worktree);
        bare.base_commit = None;
        assert!(agent_results_text(&[bare]).contains("branch kybern/abc at 1234567, 2 files +10 −4. Merge it with git."));
        assert!(text.contains("files touched: a.rs, b.rs"));
        assert!(text.contains("Result:\nDone. Tests pass."));
        assert!(text.contains(&format!("conflict: edited src/x.rs owned by thread {}", id(5))));
        assert!(text.contains("Error: boom"));
        assert!(text.ends_with("Integrate these results. For another round, delegate again with a full brief."));
    }

    #[test]
    fn interrupted_agents_show_their_error_next_to_the_partial_result() {
        let mut interrupted = item(DelegationWorkspace::Shared);
        interrupted.status = DelegationStatus::Interrupted;
        interrupted.result = Some("Waiting for sleep".into());
        interrupted.error = Some("interrupted by a Kybern restart".into());
        let text = agent_results_text(&[interrupted]);
        assert!(text.contains("Error: interrupted by a Kybern restart"), "{text}");
        assert!(text.contains("Result:\nWaiting for sleep"), "{text}");
    }

    #[test]
    fn cut_on_sentence_prefers_a_sentence_then_a_word_and_never_splits_a_character() {
        assert_eq!(cut_on_sentence("short", 10), ("short", false));
        assert_eq!(cut_on_sentence("One two. Three four five.", 16), ("One two.", true));
        assert_eq!(cut_on_sentence("alpha beta gamma delta", 14), ("alpha beta", true));
        assert_eq!(cut_on_sentence("ééééééé", 5), ("éé", true));
        // A dot inside a token ("v1.2") is not a sentence end.
        assert_eq!(cut_on_sentence("Use v1.2 now and later", 12), ("Use v1.2", true));
    }

    #[test]
    fn the_lead_copy_caps_results_and_messages_only() {
        assert!(ContentPart::Text { text: "x".into() }.lead_orchestration_text().is_none());
        let body = "Sentence one. ".repeat(300);
        let message = ContentPart::ThreadMessage {
            message_id: Uuid::nil(),
            from_thread_id: Some(Uuid::nil()),
            from_title: "Child".into(),
            purpose: ThreadMessagePurpose::Message,
            reply_to: None,
            body: body.clone(),
        };
        let full = message.orchestration_text().unwrap();
        let capped = message.lead_orchestration_text().unwrap();
        assert!(full.contains(&body) && capped.len() < full.len() / 2 && capped.contains("kybern_thread_read"), "{capped}");
        let short = ContentPart::ThreadMessage {
            message_id: Uuid::nil(),
            from_thread_id: None,
            from_title: String::new(),
            purpose: ThreadMessagePurpose::Warning,
            reply_to: None,
            body: "tiny".into(),
        };
        assert_eq!(short.lead_orchestration_text(), short.orchestration_text(), "a short message is not changed");
    }

    #[test]
    fn orchestration_text_is_only_for_the_new_parts() {
        assert!(ContentPart::Text { text: "x".into() }.orchestration_text().is_none());
        assert!(ContentPart::AgentResults { items: Vec::new() }.orchestration_text().unwrap().starts_with("Results from 0"));
    }

    #[test]
    fn new_parts_roundtrip_and_plain_text_flattens_them() {
        let message = UserMessage {
            parts: vec![
                ContentPart::ThreadMessage {
                    message_id: id(1),
                    from_thread_id: Some(id(2)),
                    from_title: "Parent".into(),
                    purpose: ThreadMessagePurpose::Task,
                    reply_to: None,
                    body: "brief".into(),
                },
                ContentPart::AgentResults { items: vec![item(DelegationWorkspace::Shared)] },
            ],
        };
        let wire = serde_json::to_value(&message).unwrap();
        assert_eq!(wire["parts"][0]["type"], "thread_message");
        assert_eq!(wire["parts"][1]["type"], "agent_results");
        assert!(wire["parts"][0].get("reply_to").is_none());
        let restored: UserMessage = serde_json::from_value(wire).unwrap();
        assert_eq!(restored, message);
        let text = restored.plain_text();
        assert!(text.contains("brief") && text.contains("Results from 1 delegated agent(s):"));
    }

    #[test]
    fn thread_omits_delegation_when_absent_and_old_payloads_still_parse() {
        let now = Utc::now();
        let mut thread = serde_json::json!({
            "id": id(1), "project_id": id(2), "title": "t",
            "provider": { "kind": "codex", "instance": "default" },
            "permission_mode": "supervised", "status": "idle", "cwd": "/",
            "pinned": false, "created_at": now, "updated_at": now, "last_seq": 0
        });
        let parsed: Thread = serde_json::from_value(thread.clone()).unwrap();
        assert!(parsed.delegation.is_none());
        assert!(serde_json::to_value(&parsed).unwrap().get("delegation").is_none());
        thread["delegation"] = serde_json::json!({
            "task_id": id(3), "operation_id": id(4), "parent_thread_id": id(5), "depth": 1,
            "role": "research", "workspace": "worktree", "status": "interrupted", "started_at": now
        });
        let parsed: Thread = serde_json::from_value(thread).unwrap();
        let delegation = parsed.delegation.unwrap();
        assert_eq!(delegation.status, DelegationStatus::Interrupted);
        assert_eq!(delegation.workspace, DelegationWorkspace::Worktree);
        assert!(delegation.owns.is_empty() && delegation.files_touched.is_empty());
    }

    #[test]
    fn orchestration_settings_default_and_partial_parse() {
        assert_eq!(
            Settings::default().orchestration,
            OrchestrationSettings { max_active_children: 4, lead_max_children: 6, max_depth: 2, ..Default::default() }
        );
        let settings: Settings = serde_json::from_value(serde_json::json!({ "orchestration": { "max_depth": 3 } })).unwrap();
        assert_eq!(
            settings.orchestration,
            OrchestrationSettings { max_active_children: 4, lead_max_children: 6, max_depth: 3, ..Default::default() }
        );
        let legacy: Settings = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(legacy.orchestration, OrchestrationSettings::default());
    }

    fn role(kind: ProviderKind, model: &str) -> RoleDefault {
        RoleDefault { provider: Some(ProviderInstance::default_for(kind)), model: Some(model.into()), effort: None }
    }

    #[test]
    fn coordinator_settings_default_and_resolve_project_then_global_then_builtin() {
        let builtin = OrchestrationSettings::default().effective_for(None);
        assert_eq!(builtin.lead_autonomy, LeadAutonomy::ProposeLarge);
        assert_eq!(builtin.lead_workspace, DelegationWorkspace::Shared);
        assert!(!builtin.lead_push_without_asking && builtin.role_defaults.is_empty());
        assert_eq!(builtin.lead_max_children, 6);

        // Old settings files have none of the new fields.
        let global: OrchestrationSettings = serde_json::from_value(serde_json::json!({
            "lead_autonomy": "propose_first",
            "lead_max_children": 4,
            "role_defaults": { "research": { "provider": { "kind": "codex", "instance": "default" }, "model": "fast" }, "review": { "model": "m" } }
        }))
        .unwrap();
        assert_eq!(global.lead_workspace, DelegationWorkspace::Shared, "an absent field takes its default");
        let project = CoordinatorOverrides {
            lead_autonomy: Some(LeadAutonomy::Act),
            lead_push_without_asking: Some(true),
            role_defaults: [(DelegationRole::Research, role(ProviderKind::ClaudeCode, "opus"))].into_iter().collect(),
            ..Default::default()
        };
        let effective = global.effective_for(Some(&project));
        assert_eq!(effective.lead_autonomy, LeadAutonomy::Act, "project beats global");
        assert_eq!(effective.lead_max_children, 4, "unset project fields fall back to the global value");
        assert!(effective.lead_push_without_asking);
        assert_eq!(
            effective.role_defaults[&DelegationRole::Research],
            role(ProviderKind::ClaudeCode, "opus"),
            "a project role replaces the global entry"
        );
        assert_eq!(effective.role_defaults[&DelegationRole::Review].model.as_deref(), Some("m"), "other roles keep the global entry");
        assert_eq!(global.effective_for(None).lead_autonomy, LeadAutonomy::ProposeFirst);
    }

    #[test]
    fn coordinator_overrides_validate_and_round_trip_without_empty_fields() {
        assert!(CoordinatorOverrides::default().is_empty());
        assert_eq!(serde_json::to_value(CoordinatorOverrides::default()).unwrap(), serde_json::json!({}));
        let general = CoordinatorOverrides {
            role_defaults: [(DelegationRole::General, role(ProviderKind::Codex, "x"))].into_iter().collect(),
            ..Default::default()
        };
        assert!(general.validate().unwrap_err().contains("general"));
        assert!(CoordinatorOverrides { lead_max_children: Some(0), ..Default::default() }.validate().is_err());
        assert!(CoordinatorOverrides { lead_max_children: Some(17), ..Default::default() }.validate().is_err());
        assert!(CoordinatorOverrides { rotation_budget_tokens: Some(49_999), ..Default::default() }.validate().is_err());
        for ok in [0, 50_000, 400_000] {
            assert!(CoordinatorOverrides { rotation_budget_tokens: Some(ok), ..Default::default() }.validate().is_ok(), "{ok}");
        }
        assert_eq!(OrchestrationSettings::default().effective_for(None).rotation_budget_tokens, DEFAULT_ROTATION_BUDGET_TOKENS);
        let off = CoordinatorOverrides { rotation_budget_tokens: Some(0), ..Default::default() };
        assert_eq!(OrchestrationSettings::default().effective_for(Some(&off)).rotation_budget_tokens, 0, "a project can turn it off");
        let blank = CoordinatorOverrides {
            role_defaults: [(DelegationRole::Test, RoleDefault { model: Some("  ".into()), ..Default::default() })].into_iter().collect(),
            ..Default::default()
        };
        assert!(blank.validate().is_err());
        let ok =
            CoordinatorOverrides { lead_workspace: Some(DelegationWorkspace::Worktree), lead_max_children: Some(3), ..Default::default() };
        assert!(ok.validate().is_ok() && !ok.is_empty());
        assert_eq!(serde_json::from_value::<CoordinatorOverrides>(serde_json::to_value(&ok).unwrap()).unwrap(), ok);
    }

    fn card(tool_name: &str, input: serde_json::Value) -> ApprovalRequest {
        ApprovalRequest {
            id: Uuid::nil(),
            thread_id: Uuid::nil(),
            turn_id: Uuid::nil(),
            tool_call_id: None,
            tool_name: tool_name.into(),
            input,
            summary: String::new(),
            suggestions: vec![],
            created_at: Utc::now(),
            questions: Vec::new(),
        }
    }

    #[test]
    fn proposal_cards_only_accept_their_own_kind_of_answer() {
        let brief = card(COORDINATOR_BRIEF_TOOL, serde_json::json!({ "text": "Draft", "replaces": false }));
        assert!(brief.is_user_input());
        assert!(brief.validate_decision(&ApprovalDecision::Submit { response: serde_json::json!({ "text": " Final wording " }) }).is_ok());
        assert!(brief.validate_decision(&ApprovalDecision::Deny { reason: None }).is_ok());
        for bad in [
            serde_json::json!({}),
            serde_json::json!({ "text": "" }),
            serde_json::json!({ "text": "  " }),
            serde_json::json!({ "text": 5 }),
            serde_json::json!({ "text": "x".repeat(MAX_BRIEF_BYTES + 1) }),
        ] {
            assert!(brief.validate_decision(&ApprovalDecision::Submit { response: bad }).is_err());
        }
        assert!(brief.validate_decision(&ApprovalDecision::AllowOnce).is_err(), "a proposal needs a wording or a decline");
        assert_eq!(
            brief.redacted_decision(&ApprovalDecision::Submit { response: serde_json::json!({ "text": "t" }) }),
            ApprovalDecision::Submit { response: serde_json::json!({ "text": "t" }) }
        );

        let settings = card(
            COORDINATOR_SETTINGS_TOOL,
            serde_json::json!({ "reason": "r", "changes": [
                { "path": "lead_autonomy", "label": "Autonomy", "old": "propose_large", "new": "act" },
                { "path": "role_defaults.research", "label": "Research model", "old": null, "new": { "model": "m" } }
            ] }),
        );
        assert!(settings.is_user_input());
        assert!(
            settings
                .validate_decision(&ApprovalDecision::Submit { response: serde_json::json!({ "accepted": ["role_defaults.research"] }) })
                .is_ok()
        );
        assert!(
            settings
                .validate_decision(&ApprovalDecision::Submit {
                    response: serde_json::json!({ "accepted": ["lead_autonomy", "role_defaults.research"] })
                })
                .is_ok()
        );
        for bad in [
            serde_json::json!({}),
            serde_json::json!({ "accepted": [] }),
            serde_json::json!({ "accepted": ["lead_workspace"] }),
            serde_json::json!({ "accepted": [3] }),
            serde_json::json!({ "accepted": "lead_autonomy" }),
        ] {
            assert!(settings.validate_decision(&ApprovalDecision::Submit { response: bad }).is_err());
        }
        assert!(settings.validate_decision(&ApprovalDecision::AllowAlways).is_err());
    }

    #[test]
    fn a_system_row_flattens_to_its_text_and_parses_back() {
        let row = ContentPart::SystemRow { kind: SystemRowKind::CoordinatorSetup, text: "Setup started".into() };
        assert_eq!(
            serde_json::to_value(&row).unwrap(),
            serde_json::json!({ "type": "system_row", "kind": "coordinator_setup", "text": "Setup started" })
        );
        let message = UserMessage { parts: vec![row.clone(), ContentPart::Text { text: " and my words".into() }] };
        assert_eq!(message.plain_text(), "Setup started and my words");
        assert_eq!(row.orchestration_text().as_deref(), Some("Setup started"));
    }
}

/// Delivery through one native child's next tool callback. Pending is not delivered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubagentMessageStatus {
    Pending,
    Delivered,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SubagentMessage {
    pub id: MessageId,
    pub thread_id: ThreadId,
    pub root_thread_id: ThreadId,
    pub task_id: String,
    /// Daemon-local admitted process. Never reuse this input in a replacement session.
    pub native_task_id: String,
    pub session_instance_id: uuid::Uuid,
    pub turn_id: TurnId,
    pub message: UserMessage,
    pub status: SubagentMessageStatus,
    pub error: Option<String>,
    pub parent_message_id: Option<MessageId>,
    pub parent_queued: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
