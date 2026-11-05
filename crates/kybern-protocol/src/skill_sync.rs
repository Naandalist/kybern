//! Your skills on every machine (ADE-86, the skills part of shared state).
//!
//! A *skill* is a folder with a `SKILL.md`. Each harness reads skills from its own user folders, called
//! *homes* here ([`SkillHome`]): `~/.agents/skills` (the cross-agent folder Codex, Cursor, OpenCode and
//! omp read), `~/.claude/skills`, `~/.codex/skills`, `~/.cursor/skills`, `~/.config/opencode/skills`,
//! `~/.pi/agent/skills` and `~/.omp/agent/skills`. Project skills travel with their repository and are
//! not listed.
//!
//! * **Reading.** `skills.inventory` lists this machine's user skills with a content hash of each copy;
//!   `skills.network_view` asks every online member for its inventory (`peer.skills.inventory`) and lays
//!   the answers out per skill, so a client sees which machines have it and whether the copies match.
//! * **Copying** is explicit only. `skills.copy` sends one skill from a source machine (any member) to the
//!   machines the user chose. A target that has a different version keeps it unless its hash is in the
//!   target's `replaces` (the versions the user saw and agreed to replace); the replaced folder is moved
//!   to `<data dir>/skills-replaced/` there, never deleted. Daemons never copy skills on their own.
//! * **Transfer.** The source queues a `skill_copy` outbox item per other machine, so an offline target
//!   gets the skill when it returns. Delivery reuses the file send (`peer.files.*`, gated by
//!   `machine:files`) with a [`crate::PushDest::Skill`] destination. A copy is capped at
//!   [`MAX_SKILL_COPY_BYTES`]; symbolic links, `.git`, `node_modules` and files that look like secrets are
//!   left out and listed.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{MachineId, ProviderKind};

/// The largest skill a copy carries: all its files together.
pub const MAX_SKILL_COPY_BYTES: u64 = 20 * 1024 * 1024;
/// The most files one skill may hold.
pub const MAX_SKILL_COPY_FILES: usize = 2_000;

/// A harness's user skill folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillHome {
    /// `~/.agents/skills`: read by Codex, Cursor, OpenCode and omp.
    Agents,
    /// `$CLAUDE_CONFIG_DIR/skills` or `~/.claude/skills`: Claude Code, also read by Cursor and OpenCode.
    Claude,
    /// `$CODEX_HOME/skills` or `~/.codex/skills`: Codex, also read by Cursor.
    Codex,
    /// `~/.cursor/skills`.
    Cursor,
    /// `$XDG_CONFIG_HOME/opencode/skills` or `~/.config/opencode/skills`.
    Opencode,
    /// `$PI_CODING_AGENT_DIR/skills` or `~/.pi/agent/skills`: pi, also read by omp.
    Pi,
    /// `~/.omp/agent/skills` (or the active omp profile's).
    Omp,
}

impl SkillHome {
    pub const ALL: [SkillHome; 7] =
        [SkillHome::Agents, SkillHome::Claude, SkillHome::Codex, SkillHome::Cursor, SkillHome::Opencode, SkillHome::Pi, SkillHome::Omp];

    pub fn as_str(self) -> &'static str {
        match self {
            SkillHome::Agents => "agents",
            SkillHome::Claude => "claude",
            SkillHome::Codex => "codex",
            SkillHome::Cursor => "cursor",
            SkillHome::Opencode => "opencode",
            SkillHome::Pi => "pi",
            SkillHome::Omp => "omp",
        }
    }

    pub fn parse(value: &str) -> Option<SkillHome> {
        SkillHome::ALL.into_iter().find(|home| home.as_str() == value.trim().to_ascii_lowercase())
    }

    /// The usual spelling of the folder, for words; the real path is in [`SkillHomeInfo::path`].
    pub fn usual_path(self) -> &'static str {
        match self {
            SkillHome::Agents => "~/.agents/skills",
            SkillHome::Claude => "~/.claude/skills",
            SkillHome::Codex => "~/.codex/skills",
            SkillHome::Cursor => "~/.cursor/skills",
            SkillHome::Opencode => "~/.config/opencode/skills",
            SkillHome::Pi => "~/.pi/agent/skills",
            SkillHome::Omp => "~/.omp/agent/skills",
        }
    }

    /// The harnesses that load skills from this folder.
    pub fn readers(self) -> &'static [ProviderKind] {
        match self {
            // Antigravity loads `.agents/skills/<name>/SKILL.md` (its own help text); the
            // per-user location is not documented, so this lists the shared folder only.
            SkillHome::Agents => &[
                ProviderKind::Codex,
                ProviderKind::Cursor,
                ProviderKind::Opencode,
                ProviderKind::Omp,
                ProviderKind::Antigravity,
                ProviderKind::GrokBuild,
            ],
            SkillHome::Claude => &[ProviderKind::ClaudeCode, ProviderKind::Cursor, ProviderKind::Opencode],
            SkillHome::Codex => &[ProviderKind::Codex, ProviderKind::Cursor],
            SkillHome::Cursor => &[ProviderKind::Cursor],
            SkillHome::Opencode => &[ProviderKind::Opencode],
            SkillHome::Pi => &[ProviderKind::Pi, ProviderKind::Omp],
            SkillHome::Omp => &[ProviderKind::Omp],
        }
    }
}

impl std::fmt::Display for SkillHome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One home on one machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillHomeInfo {
    pub home: SkillHome,
    /// The absolute folder on that machine.
    pub path: String,
    pub exists: bool,
}

/// One copy of a skill: the skill's folder in one home.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillCopyInfo {
    pub home: SkillHome,
    /// The skill's folder on that machine.
    pub path: String,
    /// Lowercase hex SHA-256 over the copied files' paths and contents (`kybern-skill-v1`). Two copies
    /// with the same hash are the same skill. Absent when the copy cannot be copied (too large, unreadable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// Bytes of the files a copy would carry.
    pub size: u64,
    pub files: u32,
    /// The folder is a symbolic link to this skill's copy in another home, so it is that copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_to: Option<SkillHome>,
    /// The folder is a symbolic link (anywhere). A link is read through, and never replaced by a copy.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_link: bool,
    /// Paths inside the skill a copy leaves out: links, `.git`, `node_modules`, secret-looking files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
    /// Why this copy cannot be sent, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// A skill on one machine, with every home that holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MachineSkill {
    /// The skill's folder relative to its home, `/`-separated (`better-ui`, `team/review`). It names the
    /// skill across machines and homes.
    pub name: String,
    /// The front matter's `name`, when it differs from the folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub copies: Vec<SkillCopyInfo>,
}

/// `skills.inventory` / `peer.skills.inventory`: this machine's user skills.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillInventory {
    pub homes: Vec<SkillHomeInfo>,
    pub skills: Vec<MachineSkill>,
    /// `macos`, `linux` or `windows`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub os: String,
}

/// Where a machine stands in a [`SkillsNetworkView`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillsMachineState {
    /// Its skills are in the view.
    Reported,
    /// Offline or paused: copies to it wait in the outbox.
    Offline,
    /// It runs a Kybern without skill copies.
    Unsupported,
    /// It did not answer in time, or refused (Files is off for this machine there).
    Unreachable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillsMachineView {
    pub id: MachineId,
    pub name: String,
    pub is_self: bool,
    pub state: SkillsMachineState,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub os: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub homes: Vec<SkillHomeInfo>,
    /// Why it did not report, when it did not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One machine's copies of a skill in the network view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillOnMachine {
    pub machine_id: MachineId,
    pub copies: Vec<SkillCopyInfo>,
}

/// A copy of a skill still in the outbox for a machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillPending {
    pub machine_id: MachineId,
    pub outbox_id: Uuid,
    /// The machine the outbox item waits on.
    pub from: MachineId,
}

/// One skill across the machines that reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillNetworkEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The machines that have it, in the view's machine order.
    pub machines: Vec<SkillOnMachine>,
    /// Distinct content hashes among every copy that reported one, most common first.
    pub versions: Vec<String>,
    /// More than one version exists.
    pub differs: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<SkillPending>,
}

/// `skills.network_view`: every reported skill, laid out per machine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillsNetworkView {
    /// This machine first, then the members by name.
    pub machines: Vec<SkillsMachineView>,
    /// By name.
    pub skills: Vec<SkillNetworkEntry>,
}

/// A machine a skill goes to, and where it goes there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillCopyTarget {
    /// The machine, by id or name. The source itself is allowed: the skill is then copied to more homes there.
    pub machine: String,
    /// The homes it lands in. Left empty: the homes it is in on the source (a link counts as its own home).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub homes: Vec<SkillHome>,
    /// The hashes of versions on this machine the user saw and chose to replace. A different version whose
    /// hash is not listed is kept, and the copy fails with what is there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaces: Vec<String>,
}

/// `skills.copy` (and `peer.skills.copy`, asked of the source by another member).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillsCopyParams {
    /// [`MachineSkill::name`].
    pub name: String,
    /// The machine that sends it, by id or name; this machine when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Which copy on the source, when its copies differ. Required then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_home: Option<SkillHome>,
    pub to: Vec<SkillCopyTarget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillCopyState {
    /// In the outbox; it is delivered when the machine is reachable (at once when it is online).
    Queued,
    /// Copied (the source machine itself, which needs no transfer).
    Done,
    /// The machine runs a Kybern without skill copies.
    Unsupported,
    /// Nothing was copied there; `error` says why.
    Failed,
}

/// What happened to a skill in one home of a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillPlaceAction {
    /// It was not there; now it is.
    Written,
    /// A different version was there; it was moved to the backup and this one put in its place.
    Replaced,
    /// The same version was already there.
    Same,
    /// The folder there is a link to another home that this copy also writes.
    Linked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillPlaced {
    pub home: SkillHome,
    /// The skill's folder on the target.
    pub path: String,
    pub action: SkillPlaceAction,
    /// Where the replaced version was moved, for `replaced`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
}

/// What a target reports once a skill landed (the `skill` of `peer.files.finish`, and a delivered
/// `skill_copy` item's result).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillLanded {
    pub name: String,
    pub hash: String,
    pub placed: Vec<SkillPlaced>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillCopyOutcome {
    pub machine_id: MachineId,
    pub name: String,
    pub state: SkillCopyState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outbox_id: Option<Uuid>,
    /// For `done`: each home.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub placed: Vec<SkillPlaced>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillsCopyResult {
    /// The machine that sends it.
    pub source: MachineId,
    pub name: String,
    /// The version that is sent (a queued copy sends the folder as it is when it is delivered).
    pub hash: String,
    pub files: u32,
    pub bytes: u64,
    /// What the copy leaves out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
    pub targets: Vec<SkillCopyOutcome>,
}

/// What the outbox keeps of a `skill_copy` item (its payload), on the source machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillCopyPayload {
    pub name: String,
    /// The home of the source copy; the folder is found again from it when the item is delivered.
    pub from_home: SkillHome,
    pub homes: Vec<SkillHome>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaces: Vec<String>,
}

/// Data of the `CONFLICT` error a target answers when a home holds a version the user did not agree to replace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillConflict {
    pub home: SkillHome,
    pub path: String,
    /// Absent for something that is not a copyable skill (a file, or a link elsewhere).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}
