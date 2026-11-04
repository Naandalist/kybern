//! Every RPC method: its name, required scope, params and result types.
//!
//! Adding a method means adding a struct pair here and an entry in `METHODS`.

use std::collections::BTreeMap;

use crate::integrations::*;
use crate::space_sync::*;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::auth::Scope;
use crate::event::ThreadEvent;
use crate::fork::*;
use crate::forward::*;
use crate::login_broker::*;
use crate::model::*;
use crate::move_thread::*;
use crate::network::*;
use crate::outbox::*;
use crate::plugin_sync::*;
use crate::provision::*;
use crate::shared_state::*;
use crate::skill_sync::*;
use crate::thread_read::*;
use crate::utilities::*;

/// Static description of a method, used for auth checks and docs.
pub trait Method {
    const NAME: &'static str;
    const SCOPE: Option<Scope>;
    type Params: Serialize + DeserializeOwned + JsonSchema;
    type Result: Serialize + DeserializeOwned + JsonSchema;
}

macro_rules! method {
    ($ty:ident, $name:literal, $scope:expr, $params:ty, $result:ty) => {
        pub struct $ty;
        impl Method for $ty {
            const NAME: &'static str = $name;
            const SCOPE: Option<Scope> = $scope;
            type Params = $params;
            type Result = $result;
        }
    };
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Empty {}

// ---- daemon ----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DaemonInfo {
    pub version: String,
    pub protocol_version: u32,
    /// Stable id of this daemon install, generated on first start.
    pub environment_id: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub data_dir: String,
    pub scopes: Vec<Scope>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// This machine's id on the network; equal to `environment_id`. Empty from a daemon older than the network.
    #[serde(default)]
    pub machine_id: String,
    /// Version of the daemon-to-daemon protocol; 0 from a daemon older than the network.
    #[serde(default)]
    pub network_protocol: u32,
    /// Feature names this daemon supports; see [`crate::features`].
    #[serde(default)]
    pub features: Vec<String>,
}
method!(DaemonInfoMethod, "daemon.info", None, Empty, DaemonInfo);
method!(DaemonShutdown, "daemon.shutdown", Some(Scope::AccessWrite), Empty, Empty);

/// What the daemon is holding open right now. Lets clients and the CLI verify
/// that finished work does not linger.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DaemonActivity {
    /// Connected WebSocket clients.
    pub connections: u32,
    /// Threads with an agent process alive.
    pub live_sessions: u32,
    /// Live sessions with no turn, approval, or background work in progress.
    pub idle_sessions: u32,
    /// Threads running a turn or waiting for an approval.
    pub running_threads: u32,
    /// Shells and agent CLIs with a live pseudo-terminal.
    pub terminals: u32,
    /// Follow-ups waiting for their thread to become idle.
    pub queued_messages: u32,
    /// When the daemon last became fully idle, if it is idle now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_since: Option<chrono::DateTime<chrono::Utc>>,
    /// When the daemon will exit if it stays idle, per `background.daemon_idle_exit_minutes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_exit_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Whether the host is running on battery. Absent when the daemon cannot tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_battery: Option<bool>,
}
method!(DaemonActivityMethod, "daemon.activity", Some(Scope::OrchestrationRead), Empty, DaemonActivity);

// ---- saved provider sessions ----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SavedSession {
    pub provider: ProviderKind,
    /// Native harness session identifier, not a Kybern thread id.
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionsListParams {
    pub provider: ProviderKind,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SessionsListResult {
    pub sessions: Vec<SavedSession>,
    pub next_cursor: Option<String>,
}
method!(SessionsList, "sessions.list", Some(Scope::OrchestrationRead), SessionsListParams, SessionsListResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionsResumeParams {
    pub provider: ProviderKind,
    pub session_id: String,
    /// Use the same project/profile context as sessions.list.
    #[serde(default)]
    pub project_id: Option<ProjectId>,
}
method!(SessionsResume, "sessions.resume", Some(Scope::OrchestrationOperate), SessionsResumeParams, Thread);

// ---- first run: find and import past chats ----

/// Pushed to every client that can read orchestration state as a scan finds more
/// (`SessionsScanChanged`). Each notification carries the whole scan, so a client
/// that misses one catches up with the next, or reads `sessions.scan_get`.
pub const SESSIONS_SCAN_CHANGED_NOTIFICATION: &str = "sessions.scan.changed";
/// Pushed while an import job works: one per project step, batched (`SessionsImportProgress`).
pub const SESSIONS_IMPORT_PROGRESS_NOTIFICATION: &str = "sessions.import.progress";
/// Pushed once when an import job ends, with the whole summary (`SessionsImportFinished`).
pub const SESSIONS_IMPORT_FINISHED_NOTIFICATION: &str = "sessions.import.finished";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionsJobState {
    Running,
    Done,
    Canceled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScanProviderState {
    Pending,
    Scanning,
    Done,
    /// Not installed, or no saved history to read.
    Unavailable,
    /// Reading failed or timed out; `error` says what to do next. The scan goes on without it.
    Failed,
}

/// One harness account's part of a scan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ScanProviderStatus {
    pub provider: ProviderKind,
    /// `default` or the name of a Kybern account.
    pub instance: String,
    pub state: ScanProviderState,
    /// Sessions found so far (all folders).
    pub sessions: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScannedProviderCount {
    pub provider: ProviderKind,
    pub sessions: u32,
    /// Of those, already imported into Kybern.
    pub imported: u32,
}

/// Saved sessions grouped by repository: sessions in subfolders and linked git
/// worktrees join the repository root.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ScannedProject {
    /// Git root of the sessions' folders, else the folder itself. Canonical when it exists.
    pub root: String,
    /// Last path component of `root`.
    pub name: String,
    pub is_git: bool,
    /// `origin` URL from the repository config, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_remote: Option<String>,
    /// The Kybern project at `root`, if one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    pub providers: Vec<ScannedProviderCount>,
    pub session_count: u32,
    pub imported_count: u32,
    /// Sessions active in the last 30 days.
    pub recent_count: u32,
    pub last_activity: chrono::DateTime<chrono::Utc>,
    /// Neither `root` nor any parent of it exists on this machine.
    pub folder_missing: bool,
    /// A git repository with at least three sessions active in the last 30 days.
    pub suggested: bool,
}

/// A scan of every installed harness for saved sessions, newest repository first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionsScan {
    pub scan_id: String,
    pub state: SessionsJobState,
    pub started_at: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub providers: Vec<ScanProviderStatus>,
    pub projects: Vec<ScannedProject>,
    /// Sessions found in all folders.
    pub sessions_found: u32,
    /// Sessions that record no working folder; they cannot be imported.
    pub sessions_without_folder: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SessionsScanProjectsParams {
    /// Only these harnesses; omitted scans every installed one.
    #[serde(default)]
    pub providers: Option<Vec<ProviderKind>>,
}
// Starts a scan and returns at once (`state: running`); results arrive as `sessions.scan.changed`.
method!(SessionsScanProjects, "sessions.scan_projects", Some(Scope::OrchestrationRead), SessionsScanProjectsParams, SessionsScan);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionsScanRef {
    pub scan_id: String,
}
method!(SessionsScanGet, "sessions.scan_get", Some(Scope::OrchestrationRead), SessionsScanRef, SessionsScan);
// Stops the scan; the result is the scan as it stood.
method!(SessionsScanCancel, "sessions.scan_cancel", Some(Scope::OrchestrationRead), SessionsScanRef, SessionsScan);

/// Params of the `sessions.scan.changed` notification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionsScanChanged {
    pub scan: SessionsScan,
}

fn default_import_limit() -> Option<u32> {
    Some(50)
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionsImportRoot {
    /// A `ScannedProject.root`.
    pub root: String,
    /// Import into this project instead of creating or finding one for `root`.
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    /// Only these harnesses; omitted imports all.
    #[serde(default)]
    pub providers: Option<Vec<ProviderKind>>,
    /// The most recent N sessions under the root (all harnesses together).
    /// Omitted means 50; `null` means all.
    #[serde(default = "default_import_limit")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionsImportAllParams {
    pub roots: Vec<SessionsImportRoot>,
    /// Reuse the session list of this scan instead of listing again.
    #[serde(default)]
    pub scan_id: Option<String>,
}
// Starts an import job and returns at once; it keeps running without the client.
// Imports create no turns and send no prompts.
method!(SessionsImportAll, "sessions.import_all", Some(Scope::OrchestrationOperate), SessionsImportAllParams, SessionsImportJob);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionsImportRef {
    pub job_id: String,
}
method!(SessionsImportGet, "sessions.import_get", Some(Scope::OrchestrationRead), SessionsImportRef, SessionsImportJob);
// Stops after the session being read; the result is the job as it stood.
method!(SessionsImportCancel, "sessions.import_cancel", Some(Scope::OrchestrationOperate), SessionsImportRef, SessionsImportJob);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImportProjectState {
    Queued,
    Running,
    Done,
    /// The project could not be created or found; no session of it was imported.
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionSkipReason {
    /// Still being written by its harness.
    InUse,
    /// Over the 64 MB history limit.
    TooLarge,
    /// The session's folder no longer exists.
    FolderMissing,
    ReadFailed,
    /// The harness could not be started to read the session.
    HarnessUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSkip {
    pub provider: ProviderKind,
    /// Native harness session id.
    pub session_id: String,
    pub title: String,
    pub reason: SessionSkipReason,
    /// One sentence saying what happened and what to do.
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionsImportProject {
    pub root: String,
    pub state: ImportProjectState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    /// Sessions selected under the root (after the provider filter and limit).
    pub total: u32,
    /// Imported, already imported or skipped.
    pub done: u32,
    pub imported: u32,
    pub already_imported: u32,
    pub skipped_count: u32,
    /// The newest 200 skips; `skipped_count` is exact.
    pub skipped: Vec<SessionSkip>,
    pub imported_thread_ids: Vec<ThreadId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionsImportJob {
    pub job_id: String,
    pub state: SessionsJobState,
    pub started_at: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub projects: Vec<SessionsImportProject>,
    pub imported: u32,
    pub already_imported: u32,
    pub skipped_count: u32,
}

/// Params of the `sessions.import.progress` notification: counts so far for one
/// project plus what is new since the previous notification of that project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionsImportProgress {
    pub job_id: String,
    pub root: String,
    pub state: ImportProjectState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    pub total: u32,
    pub done: u32,
    pub imported: u32,
    pub already_imported: u32,
    pub skipped_count: u32,
    pub new_thread_ids: Vec<ThreadId>,
    pub new_skipped: Vec<SessionSkip>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Params of the `sessions.import.finished` notification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionsImportFinished {
    pub job: SessionsImportJob,
}

// ---- providers ----

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProvidersListParams {
    /// Resolve project-scoped harness settings for this project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    /// Bypass the daemon's short-lived provider catalog cache.
    #[serde(default)]
    pub force_refresh: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProvidersListResult {
    pub providers: Vec<ProviderStatus>,
}
method!(ProvidersList, "providers.list", Some(Scope::OrchestrationRead), ProvidersListParams, ProvidersListResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AccountsCreateParams {
    pub kind: ProviderKind,
    pub name: String,
    /// Existing native account directory; omitted creates an isolated directory.
    #[serde(default)]
    pub directory: Option<String>,
}
method!(AccountsCreate, "providers.accounts.create", Some(Scope::OrchestrationOperate), AccountsCreateParams, ProviderInstance);
method!(AccountsSignIn, "providers.accounts.sign_in", Some(Scope::OrchestrationOperate), ProviderInstance, TerminalInfo);
method!(AccountsUsage, "providers.accounts.usage", Some(Scope::OrchestrationRead), ProviderInstance, ProviderUsage);

// ---- account identity, sign-in and management ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    SignedIn,
    NeedsSignIn,
    SignedOut,
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AccountIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// "Max", "Pro", "Plus", "Pro+", "API key".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// The organization the harness reports (Claude `orgName`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AccountSummary {
    /// Instance `default` is the CLI account.
    pub provider: ProviderInstance,
    /// "CLI account" for the default instance.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<AccountIdentity>,
    pub status: AccountStatus,
    pub is_default: bool,
    /// Registered project paths that override to this account.
    #[serde(default)]
    pub projects: Vec<String>,
    /// None for the CLI account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    /// Kybern created the directory under `<data>/accounts` (removal deletes it).
    #[serde(default)]
    pub managed: bool,
    #[serde(default)]
    pub can_sign_out: bool,
    /// Set when this account signs in through the login broker (design §9.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broker: Option<AccountBroker>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct AccountsListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ProviderKind>,
    /// Bypass the identity cache.
    #[serde(default)]
    pub refresh: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AccountsListResult {
    pub accounts: Vec<AccountSummary>,
}
method!(AccountsList, "providers.accounts.list", Some(Scope::OrchestrationRead), AccountsListParams, AccountsListResult);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AccountLoginMode {
    Browser,
    Paste,
    DeviceCode,
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AccountLoginPhase {
    Starting,
    Waiting,
    Verifying,
    SignedIn,
    Failed,
    Canceled,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AccountLoginStartParams {
    pub kind: ProviderKind,
    /// Sign in again to this account. Omitted: a new account in a staging directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    pub mode: AccountLoginMode,
    /// New account only: an existing native folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    /// omp only: upstream provider id passed to `omp login <provider>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AccountLogin {
    pub id: String,
    pub kind: ProviderKind,
    /// Set for re-sign-in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    pub mode: AccountLoginMode,
    pub phase: AccountLoginPhase,
    /// https only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Device code, e.g. `ABCD-12345`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<AccountIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_color: Option<String>,
    /// Instance id of an account with the same email; `default` is the CLI account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<String>,
    /// For a re-sign-in: the email the account had before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AccountLoginIdParams {
    pub id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AccountLoginInputParams {
    pub id: String,
    pub code: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AccountLoginFinishParams {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default)]
    pub make_default: bool,
}
// Sign-in stores credentials on the daemon machine, so paired devices can't start it.
method!(AccountsLoginStart, "providers.accounts.login.start", Some(Scope::AccessWrite), AccountLoginStartParams, AccountLogin);
method!(AccountsLoginGet, "providers.accounts.login.get", Some(Scope::OrchestrationRead), AccountLoginIdParams, AccountLogin);
method!(AccountsLoginInput, "providers.accounts.login.input", Some(Scope::AccessWrite), AccountLoginInputParams, AccountLogin);
method!(AccountsLoginCancel, "providers.accounts.login.cancel", Some(Scope::AccessWrite), AccountLoginIdParams, AccountLogin);
method!(AccountsLoginFinish, "providers.accounts.login.finish", Some(Scope::AccessWrite), AccountLoginFinishParams, ProviderInstance);
/// Sent to every client that may read orchestration state whenever a sign-in changes phase. Params are an `AccountLogin`.
pub const ACCOUNTS_LOGIN_CHANGED_NOTIFICATION: &str = "providers.accounts.login.changed";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AccountsUpdateParams {
    pub kind: ProviderKind,
    pub instance: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}
method!(AccountsUpdate, "providers.accounts.update", Some(Scope::OrchestrationOperate), AccountsUpdateParams, AccountSummary);
method!(AccountsSignOut, "providers.accounts.sign_out", Some(Scope::AccessWrite), ProviderInstance, AccountSummary);
method!(AccountsRemove, "providers.accounts.remove", Some(Scope::AccessWrite), ProviderInstance, Empty);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AccountsCatalogParams {
    pub provider: ProviderInstance,
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    #[serde(default)]
    pub force_refresh: bool,
}
method!(AccountsCatalog, "providers.accounts.catalog", Some(Scope::OrchestrationRead), AccountsCatalogParams, ProviderStatus);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadTargetParams {
    pub thread_id: ThreadId,
    pub target: SessionTarget,
    /// Clear the explicit override and follow project/global selection.
    #[serde(default)]
    pub inherit_account: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadTargetState {
    pub target: SessionTarget,
    /// Only the native session owned by this resolved account/configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    pub account_override: bool,
    pub effective_permission_mode: PermissionMode,
    #[serde(default)]
    pub pending_permission_mode: Option<PermissionMode>,
    /// The latest settled turn was interrupted by a confirmed account limit.
    #[serde(default)]
    pub quota_limited: bool,
}
method!(ThreadsTargetGet, "threads.target.get", Some(Scope::OrchestrationRead), ThreadsInterruptParams, ThreadTargetState);
method!(ThreadsTargetSet, "threads.target.set", Some(Scope::OrchestrationOperate), ThreadTargetParams, ThreadTargetState);
method!(ThreadsPermissionsApply, "threads.permissions.apply", Some(Scope::OrchestrationOperate), ThreadsInterruptParams, Thread);
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsSwitchContinueParams {
    pub thread_id: ThreadId,
    pub provider: ProviderInstance,
    /// Idempotent continuation request. Retrying cannot send a second prompt.
    pub message_id: MessageId,
}
method!(
    ThreadsSwitchContinue,
    "threads.switch_continue",
    Some(Scope::OrchestrationOperate),
    ThreadsSwitchContinueParams,
    ThreadsSendResult
);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HarnessUpdatesResult {
    pub updates: Vec<HarnessUpdate>,
}
method!(HarnessUpdatesList, "harness_updates.list", Some(Scope::OrchestrationRead), Empty, HarnessUpdatesResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HarnessUpdateParams {
    pub kind: ProviderKind,
}
method!(HarnessUpdatesRun, "harness_updates.run", Some(Scope::OrchestrationOperate), HarnessUpdateParams, HarnessUpdate);

// `check` asks the release feed without installing; `run` installs the newest
// version once nothing is running, then restarts the daemon.
method!(DaemonUpdateStatusMethod, "daemon_update.status", Some(Scope::OrchestrationRead), Empty, DaemonUpdate);
method!(DaemonUpdateCheck, "daemon_update.check", Some(Scope::OrchestrationOperate), Empty, DaemonUpdate);
method!(DaemonUpdateRun, "daemon_update.run", Some(Scope::OrchestrationOperate), Empty, DaemonUpdate);

// A daemon the desktop app manages updates through the app (ADE-125). Each window of the app on this machine
// attaches over its local connection; when this machine is asked to update ("Update all machines", "Keep all
// machines on the same version", `daemon_update.run`) and nothing is running, the daemon sends
// `app_update.requested` to one attached window, which installs the release from its own feed, verified with
// its own key, and restarts the app and the daemon. With no window attached the daemon updates the installed
// app itself. Only the local desktop client (the bootstrap token) may attach.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppUpdaterAttachParams {
    /// The app's version.
    pub version: String,
    /// Whether this copy of the app can install a release by itself.
    pub can_install: bool,
    /// Why it cannot, in words for the machines page, when `can_install` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
method!(AppUpdaterAttach, "app_updater.attach", Some(Scope::OrchestrationOperate), AppUpdaterAttachParams, Empty);

/// How far the app got with an update the daemon asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AppUpdatePhase {
    /// Downloading and verifying the release.
    Installing,
    /// Verified; waiting for running work to finish before the restart.
    Waiting,
    /// About to restart on the new version.
    Restarting,
    /// The feed has nothing newer for the app's update track.
    Current,
    /// The update failed; `message` says why.
    Failed,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppUpdaterReportParams {
    pub phase: AppUpdatePhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}
method!(AppUpdaterReport, "app_updater.report", Some(Scope::OrchestrationOperate), AppUpdaterReportParams, Empty);

/// Sent to the one attached window that should install an update. `version` is the version this machine was
/// asked to reach; the app still reads its own track's feed and installs only a release newer than itself.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppUpdateRequestedNotification {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Nobody asked for this update: the machine follows another one ("Keep all machines on the same version").
    /// The daemon waited for terminals to close as well; the app does the same before it restarts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub automatic: bool,
}
pub const APP_UPDATE_REQUESTED_NOTIFICATION: &str = "app_update.requested";

// ---- projects ----

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProjectsListParams {
    /// False returns the stored project index immediately, without probing Git.
    /// Omitted or true refreshes Git status before returning, as usual.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_git: Option<bool>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectsListResult {
    pub projects: Vec<Project>,
}
method!(ProjectsList, "projects.list", Some(Scope::OrchestrationRead), ProjectsListParams, ProjectsListResult);

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProjectsBrowseParams {
    /// Absolute directory on the daemon host. Omit to start in its home directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectDirectory {
    pub name: String,
    pub path: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectsBrowseResult {
    pub path: String,
    pub parent: Option<String>,
    pub directories: Vec<ProjectDirectory>,
    pub has_more: bool,
}
method!(ProjectsBrowse, "projects.browse", Some(Scope::OrchestrationRead), ProjectsBrowseParams, ProjectsBrowseResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectsAddParams {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The space for a new project; the default space when absent. An existing
    /// path is returned unchanged (it is not moved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<SpaceId>,
    /// `group` adds a group project; `members` is then required. Absent means single.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ProjectKind>,
    /// Group member paths relative to `path` (or absolute inside it). At least 2,
    /// where "." (the folder itself, allowed only when it is a repository) counts
    /// as one. The daemon stores them primary first: "." when present, then by path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub members: Option<Vec<String>>,
}
method!(ProjectsAdd, "projects.add", Some(Scope::OrchestrationOperate), ProjectsAddParams, Project);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectsUpdateParams {
    pub project_id: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `Some(None)` clears the override. Encoded as `null` on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktrees_default: Option<Option<bool>>,
    /// New prefix for task keys: 2 to 5 letters A-Z (lowercase is accepted and
    /// uppercased), unique across projects. Existing task keys do not change.
    /// Absent or `null` leaves the prefix as it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_prefix: Option<String>,
    /// When true, every new thread in the project also gets the project brief and
    /// an index of its notes at session start. Absent leaves the switch as it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brief_in_all_threads: Option<bool>,
    /// Replaces the project's Coordinator overrides. Absent leaves them as they
    /// are; `null` removes every override; an object sets exactly the fields it
    /// holds (a field left out falls back to the global setting). Send the whole
    /// object each time.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present_or_null")]
    pub coordinator: Option<Option<CoordinatorOverrides>>,
    /// Moves the project to this space.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<SpaceId>,
    /// Group projects only (an error on a single project): replaces the member
    /// list, with the same rules as `projects.add`. Existing threads keep the
    /// members they started with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub members: Option<Vec<String>>,
    /// Group projects only: replaces the top-level entries copied into new
    /// worktree containers (`Project.worktree_copy`). An empty list clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_copy: Option<Vec<String>>,
    /// Hides (true) or brings back (false) the "Add shared instructions" offer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions_offer_dismissed: Option<bool>,
}
method!(ProjectsUpdate, "projects.update", Some(Scope::OrchestrationOperate), ProjectsUpdateParams, Project);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectsRemoveParams {
    pub project_id: ProjectId,
}
method!(ProjectsRemove, "projects.remove", Some(Scope::OrchestrationOperate), ProjectsRemoveParams, Empty);

// ---- group projects ----

/// A git repository found inside a folder, offered as a group member.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RepoCandidate {
    /// Relative to the scanned folder, '/'-separated; "." for the folder itself.
    pub path: String,
    pub name: String,
    /// Current branch, or the short SHA when detached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detached: bool,
    /// Set when this is a submodule of another candidate (that member's path).
    /// Listed, not selectable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submodule_of: Option<String>,
    /// Already a single project. Informational: both may exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    /// Came from `include` ("Add folder…") rather than the scan.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub added: bool,
}

/// A folder that can't be a member, or a repository skipped by an operation,
/// with a user-facing sentence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RejectedFolder {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectsScanReposParams {
    /// Absolute folder on the daemon host (`~` expanded).
    pub path: String,
    /// Extra folders the user picked with "Add folder…" (absolute or relative to `path`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// Folder levels to search below `path`. Default 2, max 4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u8>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectsScanReposResult {
    /// Canonical folder.
    pub path: String,
    /// Folder name, the default project name.
    pub name: String,
    /// The folder itself is a repository (listed as "." in `candidates`).
    pub is_git: bool,
    /// Primary first ("." when present), then by path.
    pub candidates: Vec<RepoCandidate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejected: Vec<RejectedFolder>,
    /// Set when the folder is already a project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing_project_id: Option<ProjectId>,
    /// The scan stopped at 200 candidates or 20,000 folders.
    #[serde(default)]
    pub truncated: bool,
}
method!(ProjectsScanRepos, "projects.scan_repos", Some(Scope::OrchestrationRead), ProjectsScanReposParams, ProjectsScanReposResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectsMembersStatusParams {
    pub project_id: ProjectId,
}
/// Live state of one member, for the Repositories dialog, badge tooltip and mobile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MemberState {
    pub path: String,
    pub name: String,
    pub status: MemberStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detached: bool,
    /// Uncommitted files (`git status --porcelain` lines).
    #[serde(default)]
    pub dirty_files: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    /// The member's own instruction file relative to the group folder
    /// (`api/AGENTS.md`, else `api/CLAUDE.md`), when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}
/// A top-level entry of the group folder that may go in `Project.worktree_copy`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CopyCandidate {
    /// Entry name (top level only, never a member or `.git`).
    pub name: String,
    pub is_dir: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectsMembersStatusResult {
    /// In project order (primary first).
    pub members: Vec<MemberState>,
    /// Which shared instruction files exist in the group folder.
    pub agents_md: bool,
    pub claude_md: bool,
    /// Entries the "Also copy into worktrees" picker offers, sorted by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copy_candidates: Vec<CopyCandidate>,
}
method!(
    ProjectsMembersStatus,
    "projects.members.status",
    Some(Scope::OrchestrationRead),
    ProjectsMembersStatusParams,
    ProjectsMembersStatusResult
);

/// The project map: a daemon-built description of the project's repositories,
/// stamped with each repository's HEAD. Built from git and manifests only, with no
/// model involved, and rebuilt when a request sees a different HEAD.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectsMapParams {
    pub project_id: ProjectId,
}
/// One repository of the project as the map saw it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectMapRepo {
    /// Member path relative to the project folder; "." for a single project or the group folder itself.
    pub path: String,
    pub name: String,
    /// The branch work lands on: the remote's default, else `main` or `master`, else the current branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    /// The branch checked out when the map was built; absent when HEAD is detached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Full HEAD commit id the map describes.
    pub head: String,
    /// Tracked files counted, at most 20 000.
    pub files: u32,
    /// More files are tracked than were counted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}
/// A directory (top two levels) with its tracked-file count.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectMapArea {
    /// The repository's member path (see [`ProjectMapRepo::path`]).
    pub repo: String,
    /// Directory relative to the repository, '/'-separated; "." is the repository root.
    pub path: String,
    pub files: u32,
    /// Name from the manifest in this directory (Cargo package, npm package, Go module, Python project).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `cargo`, `npm`, `go` or `python`, when a manifest is here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
}
/// A command the project itself documents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectMapCommand {
    pub repo: String,
    /// The file it comes from, relative to the repository (`apps/desktop/package.json`, `Makefile`, `AGENTS.md`).
    pub source: String,
    /// Script, alias or target name; for a fenced block, the heading text copied verbatim.
    pub name: String,
    /// The script body, alias expansion, `make`/`just` invocation or fenced block text.
    pub run: String,
}
/// Commits per directory (two levels deep) over the last 30 days.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectMapBusy {
    pub repo: String,
    pub path: String,
    pub commits: u32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectMapChangeKind {
    Deleted,
    Renamed,
}
/// A file deleted or renamed in the last 14 days.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectMapChange {
    pub repo: String,
    pub kind: ProjectMapChangeKind,
    /// The path that is gone (deleted), or the new path (renamed).
    pub path: String,
    /// Renamed only: the old path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub commit: String,
    /// Commit date, strict ISO 8601.
    pub date: String,
    pub subject: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectMap {
    pub built_at: chrono::DateTime<chrono::Utc>,
    pub repos: Vec<ProjectMapRepo>,
    pub layout: Vec<ProjectMapArea>,
    pub commands: Vec<ProjectMapCommand>,
    pub busy: Vec<ProjectMapBusy>,
    pub removed: Vec<ProjectMapChange>,
    /// The part sessions receive: at most 6 KiB, starting `Map as of {repo}@{sha}`.
    pub header: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectsMapResult {
    pub project_id: ProjectId,
    pub map: ProjectMap,
}
method!(ProjectsMap, "projects.map", Some(Scope::OrchestrationRead), ProjectsMapParams, ProjectsMapResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectsInstructionsCreateParams {
    pub project_id: ProjectId,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectsInstructionsCreateResult {
    /// The files written in the group folder, e.g. `["AGENTS.md", "CLAUDE.md"]`.
    /// Existing files are never overwritten.
    pub created: Vec<String>,
}
method!(
    ProjectsInstructionsCreate,
    "projects.instructions.create",
    Some(Scope::OrchestrationOperate),
    ProjectsInstructionsCreateParams,
    ProjectsInstructionsCreateResult
);

pub const PROJECTS_CHANGED_NOTIFICATION: &str = "projects.changed";

/// Params of the `projects.changed` notification, sent after a project is added,
/// updated or removed by any client or by the daemon itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProjectsChangedNotification {
    /// Every project after the change, in `projects.list` order.
    pub projects: Vec<Project>,
}

// ---- spaces ----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SpacesListResult {
    pub spaces: Vec<Space>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub placements: Vec<SpacePlacement>,
}
method!(SpacesList, "spaces.list", Some(Scope::OrchestrationRead), Empty, SpacesListResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SpacesCreateParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<SpaceColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<SpaceLogo>,
}
method!(SpacesCreate, "spaces.create", Some(Scope::OrchestrationOperate), SpacesCreateParams, Space);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SpacesUpdateParams {
    pub space_id: SpaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<SpaceColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<SpaceLogo>,
}
method!(SpacesUpdate, "spaces.update", Some(Scope::OrchestrationOperate), SpacesUpdateParams, Space);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SpacesDeleteParams {
    pub space_id: SpaceId,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SpacesDeleteResult {
    /// The space the deleted space's projects moved to.
    pub moved_to: SpaceId,
    pub moved_projects: u32,
}
method!(SpacesDelete, "spaces.delete", Some(Scope::OrchestrationOperate), SpacesDeleteParams, SpacesDeleteResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SpacesReorderParams {
    /// Every space id exactly once, in the new order.
    pub space_ids: Vec<SpaceId>,
}
method!(SpacesReorder, "spaces.reorder", Some(Scope::OrchestrationOperate), SpacesReorderParams, SpacesListResult);

method!(SpacesPlace, "spaces.place", Some(Scope::OrchestrationOperate), SpacesPlaceParams, SpacesListResult);
method!(PeerSpacesSync, "peer.spaces.sync", Some(Scope::MachinePeer), PeerSpacesSyncParams, PeerSpacesSyncResult);
method!(PeerSpaceLogo, "peer.spaces.logo", Some(Scope::MachinePeer), PeerSpaceLogoParams, PeerSpaceLogoResult);

pub const SPACES_CHANGED_NOTIFICATION: &str = "spaces.changed";

/// Params of the `spaces.changed` notification, sent after a space is created,
/// updated, deleted or reordered, by any client.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SpacesChangedNotification {
    /// Every space after the change, in order.
    pub spaces: Vec<Space>,
}

// ---- threads ----

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default)]
    pub include_archived: bool,
    /// Also return read-only subagent threads (`Thread::subagent`) and
    /// delegated children (`Thread::delegation`). They are left out by default
    /// so clients that predate them never show one as a normal thread.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_subagents: bool,
    /// Return only the direct subagent and delegated children of this thread.
    /// Implies `include_subagents`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<ThreadId>,
    /// Also return the hidden threads that record tool calls other machines made here (`Thread::inbound`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_inbound: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsListResult {
    pub threads: Vec<Thread>,
    /// Compact activity summaries for sidebar rendering.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activity: Vec<ThreadActivitySummary>,
}
method!(ThreadsList, "threads.list", Some(Scope::OrchestrationRead), ThreadsListParams, ThreadsListResult);

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsSearchParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default)]
    pub all_projects: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default)]
    pub include_archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default = "default_thread_search_limit")]
    pub limit: u32,
}
fn default_thread_search_limit() -> u32 {
    50
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsSearchResult {
    pub threads: Vec<ThreadSearchHit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}
method!(ThreadsSearch, "threads.search", Some(Scope::OrchestrationRead), ThreadsSearchParams, ThreadsSearchResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsReadParams {
    pub thread_id: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_seq: Option<crate::EventSeq>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub through_seq: Option<crate::EventSeq>,
    /// Continue reading a single large settled message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_seq: Option<crate::EventSeq>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_offset: Option<u64>,
    #[serde(default = "default_thread_read_limit")]
    pub limit: u32,
}
fn default_thread_read_limit() -> u32 {
    100
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsReadResult {
    pub thread: Thread,
    pub messages: Vec<ThreadReadMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_before_seq: Option<crate::EventSeq>,
    pub through_seq: crate::EventSeq,
}
method!(ThreadsRead, "threads.read", Some(Scope::OrchestrationRead), ThreadsReadParams, ThreadsReadResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsCreateParams {
    /// Pin the explicitly selected account, including the regular CLI account.
    #[serde(default)]
    pub pin_account: bool,
    /// Omit to create a free chat in the daemon-owned neutral workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    pub provider: ProviderInstance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    /// Create a git worktree for this thread. Defaults to the project override, then global off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_worktree: Option<bool>,
    /// Branch to start from. A worktree forks its branch from it; a local thread
    /// switches the project checkout to it first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optionally send a first message in the same call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<UserMessage>,
    /// A client-chosen id for the new thread (UUID v7), so a client can show the thread before the
    /// daemon answers. A repeat with the same id returns the thread the first call made, never a
    /// second one, and sends `message` only if that call did not already deliver it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
    /// The client's id for `message`, as `threads.send` takes it: a repeat with the same id is
    /// answered from the first delivery instead of starting another turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<MessageId>,
    /// Group projects, worktree mode: member paths to include. Omit for all.
    /// Ignored in local mode (the agent works in the group folder with every member).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub members: Option<Vec<String>>,
    /// Group projects: base branch per member path. Worktree: each member's fork
    /// point. Local: the branch to switch that member's checkout to first.
    /// `base_branch` still applies to the primary when this map does not name it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branches: Option<BTreeMap<String, String>>,
}
method!(ThreadsCreate, "threads.create", Some(Scope::OrchestrationOperate), ThreadsCreateParams, Thread);

pub const THREADS_WORKSPACE_PROGRESS_NOTIFICATION: &str = "threads.workspace_progress";

/// Params of the transient `threads.workspace_progress` notification, sent while
/// `threads.create` prepares worktrees: before each repository and once at the
/// end. Single-repo worktree threads send it too (`total: 1`). Not an event.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsWorkspaceProgress {
    pub thread_id: ThreadId,
    /// Repositories whose worktree exists.
    pub done: u32,
    pub total: u32,
    /// Member being created (its path); absent for single-repo threads and at the end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsGetParams {
    pub thread_id: ThreadId,
    /// Return the newest N transcript entries (1..=500), plus older unfinished
    /// rows on the initial page. Equal-sequence entries stay together. Omit for full history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_limit: Option<u32>,
    /// Return entries strictly before this cursor. Requires transcript_limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_seq: Option<crate::EventSeq>,
    /// Freeze the projection at this acknowledged event sequence while paging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub through_seq: Option<crate::EventSeq>,
    /// When `Some(false)`, large settled tool results are omitted from the page
    /// and `output_omitted` is set so clients can fetch them on demand. Omit or
    /// `true` keeps compatibility with CLI and older clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_tool_output: Option<bool>,
    /// When true, completed output-delta streams are represented by
    /// `stream_omitted` markers and can be fetched through
    /// `threads.tool_output`. Omit to preserve legacy snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defer_tool_stream: Option<bool>,
    /// When true, a paged request also returns the earlier rows of its oldest
    /// turn, so every page starts at a turn boundary (a bounded number of extra
    /// rows). Clients that show one row per turn otherwise get pages that add
    /// nothing visible inside long tool-heavy turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whole_turns: Option<bool>,
    /// With `before_seq`: return only the transcript page. Thread-wide state the
    /// client holds from its first page (runtime tasks, provider commands and
    /// usage, pending questions and approvals, notes) comes back empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_only: Option<bool>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsGetResult {
    #[serde(default)]
    pub notes: ThreadNotes,
    #[serde(default)]
    pub pending_questions: Vec<AsyncQuestionRequest>,
    #[serde(default)]
    pub provider_commands: Vec<crate::ProviderCommand>,
    #[serde(default)]
    pub provider_usage: crate::ProviderUsage,
    pub thread: Thread,
    pub transcript: Vec<TranscriptEntry>,
    /// Cursor for the next older page; absent when all earlier entries are present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_before_seq: Option<crate::EventSeq>,
    pub pending_approvals: Vec<ApprovalRequest>,
    /// Durable latest-state task projection for this thread.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtime_tasks: Vec<RuntimeTask>,
}
method!(ThreadsGet, "threads.get", Some(Scope::OrchestrationRead), ThreadsGetParams, ThreadsGetResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsToolOutputParams {
    pub thread_id: ThreadId,
    pub tool_call_id: String,
    /// Exact tool-start row, required to disambiguate reused provider IDs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_seq: Option<crate::EventSeq>,
    /// Never read a completion newer than this snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub through_seq: Option<crate::EventSeq>,
    /// Include the exact persisted output-delta stream for this invocation.
    /// Omit or set false to retain the legacy output-only response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_tool_stream: Option<bool>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsToolOutputResult {
    pub output: serde_json::Value,
    pub is_error: bool,
    /// Exact persisted output-delta stream when requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,
    /// Deltas exist but were omitted because `include_tool_stream` was false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stream_omitted: bool,
}
method!(ThreadsToolOutput, "threads.tool_output", Some(Scope::OrchestrationRead), ThreadsToolOutputParams, ThreadsToolOutputResult);

/// Restore exact historical OMP message boundaries from its retained native session.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsRecoverOmpAnswerParams {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsRecoverOmpAnswerResult {
    pub recovered: bool,
    pub native_entry_id: String,
    pub block_count: u32,
    pub correction_seq: EventSeq,
}
method!(
    ThreadsRecoverOmpAnswer,
    "threads.recover_omp_answer",
    Some(Scope::OrchestrationOperate),
    ThreadsRecoverOmpAnswerParams,
    ThreadsRecoverOmpAnswerResult
);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsUpdateParams {
    pub thread_id: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}
method!(ThreadsUpdate, "threads.update", Some(Scope::OrchestrationOperate), ThreadsUpdateParams, Thread);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsArchiveParams {
    pub thread_id: ThreadId,
}
method!(ThreadsArchive, "threads.archive", Some(Scope::OrchestrationOperate), ThreadsArchiveParams, Empty);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsSendParams {
    pub thread_id: ThreadId,
    pub message: UserMessage,
    /// Stable client identity for retrying a send after a lost response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<MessageId>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsSendResult {
    /// The turn the message started. Nil when `queued`.
    pub turn_id: TurnId,
    pub message_id: MessageId,
    /// The message waits in the thread's queue instead of starting a turn (a
    /// Coordinator's context is being refreshed). It starts the next turn by
    /// itself; a retry with the same `message_id` returns this result again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub queued: bool,
}
method!(ThreadsSend, "threads.send", Some(Scope::OrchestrationOperate), ThreadsSendParams, ThreadsSendResult);

/// A user-owned notepad, separate from the agent's conversation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ThreadNotes {
    pub text: String,
    pub revision: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadNotesSetParams {
    pub thread_id: ThreadId,
    pub text: String,
    /// Refuse to overwrite edits saved by another client.
    pub expected_revision: i64,
}
method!(ThreadNotesGet, "threads.notes.get", Some(Scope::OrchestrationRead), ThreadsInterruptParams, ThreadNotes);
method!(ThreadNotesSet, "threads.notes.set", Some(Scope::OrchestrationOperate), ThreadNotesSetParams, ThreadNotes);
method!(ThreadsSteer, "threads.steer", Some(Scope::OrchestrationOperate), QueuedMessage, ThreadsSendResult);
method!(SubagentsSend, "subagents.send", Some(Scope::OrchestrationOperate), QueuedMessage, SubagentMessage);
method!(SubagentsMessages, "subagents.messages", Some(Scope::OrchestrationRead), ThreadsInterruptParams, Vec<SubagentMessage>);
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SubagentMessageActionParams {
    pub thread_id: ThreadId,
    pub message_id: MessageId,
}
method!(SubagentsSendToParent, "subagents.send_to_parent", Some(Scope::OrchestrationOperate), SubagentMessageActionParams, SubagentMessage);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsInterruptParams {
    pub thread_id: ThreadId,
}
method!(ThreadsRelease, "threads.release", Some(Scope::OrchestrationOperate), ThreadsInterruptParams, Empty);
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsAnswerParams {
    pub thread_id: ThreadId,
    pub request_id: String,
    pub answers: Vec<String>,
}
method!(ThreadsAnswer, "threads.answer", Some(Scope::OrchestrationOperate), ThreadsAnswerParams, Empty);
method!(ThreadsCompact, "threads.compact", Some(Scope::OrchestrationOperate), ThreadsInterruptParams, ThreadsSendResult);
method!(ThreadsInterrupt, "threads.interrupt", Some(Scope::OrchestrationOperate), ThreadsInterruptParams, Empty);

// ---- thread messages and delegations ----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadMessagesListParams {
    pub thread_id: ThreadId,
    /// Only messages in these states. Omit for every state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub states: Option<Vec<ThreadMessageState>>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ThreadMessagesListResult {
    /// Messages sent to or from the thread, oldest first, at most 200 (the newest).
    pub messages: Vec<ThreadMessageRecord>,
}
method!(ThreadMessagesList, "threads.messages.list", Some(Scope::OrchestrationRead), ThreadMessagesListParams, ThreadMessagesListResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadMessageIdParams {
    pub message_id: MessageId,
}
// The user approves a held message; it is delivered with its requested delivery.
method!(ThreadMessagesDeliver, "threads.messages.deliver", Some(Scope::OrchestrationOperate), ThreadMessageIdParams, ThreadMessageRecord);
method!(ThreadMessagesDismiss, "threads.messages.dismiss", Some(Scope::OrchestrationOperate), ThreadMessageIdParams, ThreadMessageRecord);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DelegationsWorktreeRemoveParams {
    /// The delegated child whose worktree to remove.
    pub thread_id: ThreadId,
    /// Required when the worktree is dirty or its branch is unmerged. The
    /// branch is deleted only when it is merged.
    #[serde(default)]
    pub force: bool,
}
method!(
    DelegationsWorktreeRemove,
    "delegations.worktree_remove",
    Some(Scope::OrchestrationOperate),
    DelegationsWorktreeRemoveParams,
    Thread
);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DelegationsRemoteListParams {
    /// The delegating thread on this machine.
    pub parent_thread_id: ThreadId,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DelegationsRemoteListResult {
    /// The thread's children on other machines, as this machine records them, oldest first.
    pub children: Vec<crate::RemoteChild>,
}
// This machine's own record of the agents a thread delegated to other machines: the state that
// `kybern_agent_status` reports, which a client reads instead of its copy of the other machine's list.
method!(
    DelegationsRemoteList,
    "delegations.remote_list",
    Some(Scope::OrchestrationRead),
    DelegationsRemoteListParams,
    DelegationsRemoteListResult
);
/// Sent to every client that may read orchestration state whenever this machine's record of a
/// child on another machine changes (created, acknowledged, followed, settled). Params are the
/// whole [`crate::RemoteChild`]; a client that missed one reads `delegations.remote_list` again.
pub const DELEGATIONS_REMOTE_CHANGED_NOTIFICATION: &str = "delegations.remote_changed";
/// Sent instead of [`DELEGATIONS_REMOTE_CHANGED_NOTIFICATION`]s a client was too slow to take: some records changed
/// unseen, and the client reads `delegations.remote_list` again for the parents it shows. Params are an empty object.
pub const DELEGATIONS_REMOTE_RESYNC_NOTIFICATION: &str = "delegations.remote_resync";

// ---- project coordinator (the lead thread) ----

/// Tell an absent field (`None`, via `default`) from an explicit `null`
/// (`Some(None)`).
fn present_or_null<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// The project's Coordinator thread, if it has one.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoordinatorGetParams {
    pub project_id: ProjectId,
}
method!(CoordinatorGet, "coordinator.get", Some(Scope::OrchestrationRead), CoordinatorGetParams, Option<Thread>);

/// Open the project's Coordinator, creating it when the project has none. Idempotent:
/// a project has at most one Coordinator, so a retry or a second client gets the same
/// thread back with `created: false`. Archiving the Coordinator frees the slot.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoordinatorOpenParams {
    pub project_id: ProjectId,
    /// Used only when the Coordinator is created.
    pub provider: ProviderInstance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    /// The user's opening words, their first answer in the Coordinator's setup
    /// interview. Used only when the Coordinator is created for a project that
    /// has no brief: the daemon then starts the setup (a "Setup started" row,
    /// then this text as the user's own message) and the Coordinator's first
    /// turn begins. It is never saved as the brief; only the brief proposal
    /// card, or the user in Notes, writes the brief.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opening_message: Option<String>,
    /// Older clients' name for [`Self::opening_message`], treated the same way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_brief: Option<String>,
    /// The id the Coordinator gets when this call creates it (see `threads.create`). Ignored
    /// when the project already has one: the result carries that thread instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
    /// The client's id for the opening message, so its optimistic bubble meets the real one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<MessageId>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoordinatorOpenResult {
    pub thread: Thread,
    pub created: bool,
}
method!(CoordinatorOpen, "coordinator.open", Some(Scope::OrchestrationOperate), CoordinatorOpenParams, CoordinatorOpenResult);

/// Start the setup interview again ("Redo setup"): the daemon sends the
/// Coordinator the setup kickoff, a "Setup started" row for clients. Refused
/// while the Coordinator is running a turn. Returns the Coordinator thread.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoordinatorSetupParams {
    pub project_id: ProjectId,
}
method!(CoordinatorSetup, "coordinator.setup", Some(Scope::OrchestrationOperate), CoordinatorSetupParams, Thread);

/// Move the Coordinator to another harness, model or account. This is a context
/// refresh onto the new harness: the old session writes a handoff (unless its
/// harness is signed out), its process closes, and the next message starts the
/// new session with the handoff. The thread and its history stay.
///
/// The call returns only when the refresh is over: up to 90 s for the handoff
/// and 90 s more for one correction, so allow about three minutes. Messages
/// sent meanwhile queue and start on the new harness.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoordinatorSwitchHarnessParams {
    pub project_id: ProjectId,
    pub provider: ProviderInstance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
}
method!(CoordinatorSwitchHarness, "coordinator.switch_harness", Some(Scope::OrchestrationOperate), CoordinatorSwitchHarnessParams, Thread);

/// Refresh an idle Coordinator's context now: its session writes a handoff and is
/// replaced by a fresh one, inside the same thread. Refused while it is busy.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoordinatorRefreshContextParams {
    pub thread_id: ThreadId,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoordinatorRefreshContextResult {
    pub rotation_id: String,
}
method!(
    CoordinatorRefreshContext,
    "coordinator.refresh_context",
    Some(Scope::OrchestrationOperate),
    CoordinatorRefreshContextParams,
    CoordinatorRefreshContextResult
);

/// Replace the handoff of the Coordinator's latest context refresh. Before the
/// fresh session's first message it replaces the handoff that message carries;
/// after it, the correction is sent once with the next message.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoordinatorEditHandoffParams {
    pub thread_id: ThreadId,
    pub rotation_id: String,
    pub text: String,
}
method!(CoordinatorEditHandoff, "coordinator.edit_handoff", Some(Scope::OrchestrationOperate), CoordinatorEditHandoffParams, Empty);

// ---- daemon-owned follow-ups ----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QueuedMessage {
    pub id: MessageId,
    pub thread_id: ThreadId,
    pub message: UserMessage,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QueueListParams {
    pub thread_id: Option<ThreadId>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QueueListResult {
    pub messages: Vec<QueuedMessage>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QueueRemoveParams {
    pub thread_id: ThreadId,
    pub id: MessageId,
}
method!(QueueAdd, "queue.add", Some(Scope::OrchestrationOperate), QueuedMessage, Empty);
method!(QueueUpdate, "queue.update", Some(Scope::OrchestrationOperate), QueuedMessage, Empty);
method!(QueueList, "queue.list", Some(Scope::OrchestrationRead), QueueListParams, QueueListResult);
method!(QueueRemove, "queue.remove", Some(Scope::OrchestrationOperate), QueueRemoveParams, Empty);

// ---- provider-owned runtime tasks ----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TasksListParams {
    pub thread_id: ThreadId,
    /// Include terminal tasks in addition to active work.
    #[serde(default)]
    pub include_completed: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TasksListResult {
    pub tasks: Vec<RuntimeTask>,
}
method!(TasksList, "tasks.list", Some(Scope::OrchestrationRead), TasksListParams, TasksListResult);

/// Stop or background one provider-owned task. `thread_id` may also be a
/// subagent thread (`Thread::subagent`): the daemon then acts on that
/// subagent's own task through its root session and ignores `task_id`, so
/// clients can pass an empty string.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskControlParams {
    pub thread_id: ThreadId,
    #[serde(default)]
    pub task_id: String,
}
method!(TaskStop, "tasks.stop", Some(Scope::OrchestrationOperate), TaskControlParams, RuntimeTask);
method!(TaskBackground, "tasks.background", Some(Scope::OrchestrationOperate), TaskControlParams, RuntimeTask);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsRegenerateTitleParams {
    pub thread_id: ThreadId,
}
method!(ThreadsRegenerateTitle, "threads.regenerateTitle", Some(Scope::OrchestrationOperate), ThreadsRegenerateTitleParams, Thread);

// ---- checkpoints ----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsCheckpointsParams {
    pub thread_id: ThreadId,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsCheckpointsResult {
    pub checkpoints: Vec<Checkpoint>,
}
method!(ThreadsCheckpoints, "threads.checkpoints", Some(Scope::OrchestrationRead), ThreadsCheckpointsParams, ThreadsCheckpointsResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsDiffParams {
    pub thread_id: ThreadId,
    /// Diff of one turn (its before → after, or before → now while running).
    /// Omit for the whole thread: first checkpoint → current working tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    /// Include unified patch text. Defaults to true for older clients.
    #[serde(default = "default_true")]
    pub include_patch: bool,
    /// Limit the diff to one path relative to the thread cwd. In a group thread
    /// the path's leading member folder selects the member (`api/src/x.rs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}
method!(ThreadsDiff, "threads.diff", Some(Scope::OrchestrationRead), ThreadsDiffParams, Diff);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsRevertParams {
    pub thread_id: ThreadId,
    /// Restore the working tree to the state before this turn.
    pub turn_id: TurnId,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadsRevertResult {
    pub commit: String,
    /// Whether the provider conversation was also rewound. False when the driver cannot fork yet.
    pub conversation_rewound: bool,
}
method!(ThreadsRevert, "threads.revert", Some(Scope::OrchestrationOperate), ThreadsRevertParams, ThreadsRevertResult);

// ---- request cards ----

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RequestsProgressParams {
    /// Only this thread's cards; every thread's when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RequestsProgressResult {
    pub items: Vec<RequestProgress>,
}
/// What the user did on a request card that is not an answer.
///
/// Login (`kybern_request_login`): `browser`, `terminal`, `paste` (`value` is
/// the code), `cancel` (back to the rows), `retry`. Needs `access:write`: it
/// changes the machine's own CLI login. Permission: `check`, `granted` ("I've
/// granted it"), `stop`. Question with a deadline: `pause`, `resume`, `hold`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RequestsActParams {
    pub approval_id: ApprovalId,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}
method!(RequestsProgress, "requests.progress", Some(Scope::OrchestrationRead), RequestsProgressParams, RequestsProgressResult);
method!(RequestsAct, "requests.act", Some(Scope::OrchestrationOperate), RequestsActParams, Empty);
/// Sent to every client that may read orchestration state whenever a request card changes. Params are a `RequestProgress`.
pub const REQUESTS_PROGRESS_NOTIFICATION: &str = "requests.progress";

// ---- terminals ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TerminalInfo {
    pub id: TerminalId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    pub title: String,
    pub alive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TerminalsCreateParams {
    /// A client-generated identity makes reconnecting creation idempotent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_id: Option<TerminalId>,
    /// Run in this thread's working directory (worktree or project).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
    /// Explicit working directory; overrides the thread's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
    /// Program to run. Defaults to the user's shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
}
fn default_cols() -> u16 {
    120
}
fn default_rows() -> u16 {
    32
}
method!(TerminalsCreate, "terminals.create", Some(Scope::TerminalOperate), TerminalsCreateParams, TerminalInfo);

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TerminalsListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TerminalsListResult {
    pub terminals: Vec<TerminalInfo>,
}
method!(TerminalsList, "terminals.list", Some(Scope::TerminalOperate), TerminalsListParams, TerminalsListResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TerminalsInputParams {
    pub terminal_id: TerminalId,
    /// Raw bytes, base64.
    pub data: String,
}
method!(TerminalsInput, "terminals.input", Some(Scope::TerminalOperate), TerminalsInputParams, Empty);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TerminalsResizeParams {
    pub terminal_id: TerminalId,
    pub cols: u16,
    pub rows: u16,
}
method!(TerminalsResize, "terminals.resize", Some(Scope::TerminalOperate), TerminalsResizeParams, Empty);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TerminalsCloseParams {
    pub terminal_id: TerminalId,
}
method!(TerminalsClose, "terminals.close", Some(Scope::TerminalOperate), TerminalsCloseParams, Empty);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TerminalsSubscribeParams {
    pub terminal_id: TerminalId,
    /// Send the retained scrollback (up to the daemon's cap) before live output.
    #[serde(default = "default_true")]
    pub replay: bool,
}
fn default_true() -> bool {
    true
}
method!(TerminalsSubscribe, "terminals.subscribe", Some(Scope::TerminalOperate), TerminalsSubscribeParams, Empty);
method!(TerminalsUnsubscribe, "terminals.unsubscribe", Some(Scope::TerminalOperate), TerminalsCloseParams, Empty);

/// Params of the `terminal.output` notification.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TerminalOutputNotification {
    pub terminal_id: TerminalId,
    /// Raw bytes, base64.
    pub data: String,
}
/// Params of the `terminal.exited` notification.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TerminalExitedNotification {
    pub terminal_id: TerminalId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}
pub const TERMINAL_OUTPUT_NOTIFICATION: &str = "terminal.output";
pub const TERMINAL_EXITED_NOTIFICATION: &str = "terminal.exited";

// ---- settings ----

method!(SettingsGet, "settings.get", Some(Scope::OrchestrationRead), Empty, Settings);
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SettingsUpdateParams {
    pub settings: Settings,
}
method!(SettingsUpdate, "settings.update", Some(Scope::OrchestrationOperate), SettingsUpdateParams, Settings);

// ---- computer use ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComputerPermission {
    Granted,
    Missing,
    Unknown,
}

/// One line of `kybern computer doctor`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ComputerCheck {
    pub name: String,
    pub ok: bool,
    pub message: String,
    /// What the user should do next, when the check fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ComputerStatus {
    /// This daemon's OS and architecture have a computer-use driver.
    pub supported: bool,
    pub enabled: bool,
    pub installed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The CuaDriver release Kybern installs and requires at minimum.
    pub required_version: String,
    /// Signed by Cua AI with the expected bundle identifier.
    pub signed: bool,
    pub accessibility: ComputerPermission,
    pub screen_recording: ComputerPermission,
    /// Agents can use computer tools now.
    pub ready: bool,
    pub checks: Vec<ComputerCheck>,
    /// `macos`, `windows` or `linux`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// The driver in use here (the setting, or the OS default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<crate::model::ComputerDriver>,
    /// The drivers this machine can use, the default first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drivers: Vec<crate::model::ComputerDriver>,
    /// macOS with agent-desktop: the app System Settings lists for
    /// Accessibility and Screen Recording (Kybern itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_holder: Option<String>,
    /// Linux: the display session, e.g. `wayland (GNOME)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}
method!(ComputerStatusGet, "computer.status", Some(Scope::OrchestrationRead), Empty, ComputerStatus);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComputerSetupAction {
    /// Install or update CuaDriver into /Applications with its pinned installer.
    Install,
    /// Start CuaDriver's own permission prompts.
    GrantPermissions,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ComputerSetupParams {
    pub action: ComputerSetupAction,
}
/// What an agent last saw while using an app, for a live view. Transient:
/// frames are kept in memory and never written to the transcript.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ComputerFrame {
    /// Increases with every new frame across the daemon.
    pub seq: u64,
    pub window_id: u64,
    pub app: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The step that led to this frame, e.g. `Pressed “Equals”`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Where that step acted, from 0 to 1 across and down the picture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point: Option<[f64; 2]>,
    pub media_type: String,
    /// Base64 image bytes.
    pub data: String,
    pub width: u32,
    pub height: u32,
    pub captured_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ComputerFrameParams {
    pub thread_id: ThreadId,
    /// Return a frame only if it is newer than this `seq`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ComputerFrameResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<ComputerFrame>,
    /// Pictures are off for now, and why (e.g. screenshot consent was
    /// declined on Wayland). The view says "Live view paused".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<String>,
}
// Polling also tells the daemon someone is watching, which turns capture on.
method!(ComputerFrameGet, "computer.frame", Some(Scope::OrchestrationRead), ComputerFrameParams, ComputerFrameResult);

// Installs software on the daemon machine, so paired devices cannot call it.
method!(ComputerSetup, "computer.setup", Some(Scope::AccessWrite), ComputerSetupParams, ComputerStatus);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CursorAccount {
    SignedIn,
    SignedOut,
    /// `CURSOR_API_KEY` is set for the provider, which overrides browser sign-in.
    ApiKey,
    /// The SDK is not installed, or its sign-in state could not be read.
    Unknown,
}

/// The Cursor SDK on the daemon machine: what `kybern cursor install` and
/// `kybern cursor login` set up, readable without a terminal.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CursorSetupStatus {
    /// The exact `@cursor/sdk` version this daemon installs and requires.
    pub sdk_version: String,
    pub installed: bool,
    /// An earlier Kybern release's SDK, set up on this machine while `sdk_version`
    /// is not: setup is an update, not a first install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_version: Option<String>,
    /// The Node.js version the SDK runs on, e.g. `v24.1.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_version: Option<String>,
    /// What to fix on the machine before installing, e.g. a missing Node.js.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    pub account: CursorAccount,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// An install is running in the background.
    pub installing: bool,
    /// A browser sign-in is waiting for the user.
    pub signing_in: bool,
    /// The page that completes the pending sign-in. Open it on any device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_url: Option<String>,
    /// Why the last install or sign-in failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
method!(CursorStatusGet, "cursor.status", Some(Scope::OrchestrationRead), Empty, CursorSetupStatus);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CursorSetupAction {
    /// Install the pinned SDK in the background; poll `cursor.status`.
    Install,
    /// Start a browser sign-in. The result carries `login_url` once Cursor issues it.
    SignIn,
    CancelSignIn,
    SignOut,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CursorSetupParams {
    pub action: CursorSetupAction,
}
// Installs software and stores credentials on the daemon machine.
method!(CursorSetup, "cursor.setup", Some(Scope::AccessWrite), CursorSetupParams, CursorSetupStatus);

/// What agents learned about using one app, shown to later sessions the first
/// time they use it. Stored as Markdown under `<data dir>/computer-notes/`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ComputerNote {
    /// The app id (see `app_id`); kept under its old name for older clients.
    pub bundle_id: String,
    /// macOS bundle id, Windows lowercase exe name (`code.exe`), or Linux
    /// desktop-entry id (`org.gnome.Nautilus`).
    #[serde(default)]
    pub app_id: String,
    pub app: String,
    pub text: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ComputerNotesResult {
    pub notes: Vec<ComputerNote>,
}
method!(ComputerNotesList, "computer.notes.list", Some(Scope::OrchestrationRead), Empty, ComputerNotesResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ComputerNoteSetParams {
    /// The app id; `app_id` is accepted too.
    #[serde(alias = "app_id")]
    pub bundle_id: String,
    /// The whole note. Empty deletes it.
    pub text: String,
    /// The app's display name, for a note that does not exist yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
}
// Notes reach every later agent session, like settings do.
method!(ComputerNoteSet, "computer.notes.set", Some(Scope::OrchestrationOperate), ComputerNoteSetParams, ComputerNotesResult);

// ---- pasted secrets ----

/// Where `secrets.store` and `secrets.target` look: a thread's workspace, or the
/// project's folder for a chat that has not started yet. Pass exactly one.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SecretsWorkspace {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SecretsTargetParams {
    #[serde(flatten)]
    pub workspace: SecretsWorkspace,
    pub env_key: String,
    /// Env file relative to the workspace. Default `.env`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// What storing into a file would do, checked without a value.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SecretsTargetResult {
    /// The file relative to the workspace, e.g. `.env`.
    pub file: String,
    /// The key is already in the file; storing replaces it.
    pub replaces: bool,
    /// Git ignores the file (true outside a repository).
    pub ignored: bool,
    /// Git tracks the file, so it can never take a secret.
    pub tracked: bool,
    /// The file already exists.
    pub exists: bool,
}
method!(SecretsTarget, "secrets.target", Some(Scope::OrchestrationOperate), SecretsTargetParams, SecretsTargetResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SecretsStoreParams {
    #[serde(flatten)]
    pub workspace: SecretsWorkspace,
    pub env_key: String,
    /// The secret. Written to the file and never stored, traced or returned.
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Add the file to the `.gitignore` next to it first, when git does not ignore it yet.
    #[serde(default)]
    pub add_to_gitignore: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SecretsStoreResult {
    pub env_key: String,
    pub file: String,
    pub replaced: bool,
    /// The daemon added the file to `.gitignore`.
    pub gitignore_updated: bool,
}
// Writes a file in the workspace, like a settings change, so it needs the same scope as the card's answer.
method!(SecretsStore, "secrets.store", Some(Scope::OrchestrationOperate), SecretsStoreParams, SecretsStoreResult);

// ---- usage ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UsageGroup {
    #[default]
    Provider,
    Model,
    Day,
    Thread,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct UsageSummaryParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub group_by: UsageGroup,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UsageRow {
    pub key: String,
    pub turns: u64,
    pub usage: Usage,
    pub cost_usd: f64,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UsageSummaryResult {
    pub rows: Vec<UsageRow>,
    pub total: UsageRow,
}
method!(UsageSummary, "usage.summary", Some(Scope::OrchestrationRead), UsageSummaryParams, UsageSummaryResult);

/// Current plan limits (e.g. 5-hour / weekly / monthly) per provider. The daemon
/// keeps one account-wide cache fed by turn-free live reads, limits running turns
/// report, and, until the first read, the last stored report.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct UsageLimitsParams {
    /// Answer from the cache at once. Providers whose values are older than
    /// their freshness window are re-read in the background and arrive as a
    /// `usage.limits.changed` notification. Without it the call waits for
    /// those reads (bounded), as older clients expect.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cached: bool,
    /// Re-read every provider now, even if its values are fresh. Reads closer
    /// together than a few seconds are coalesced.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub refresh: bool,
    /// Also read these accounts (the composer's thread account).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instances: Vec<ProviderInstance>,
    /// Also read every named account (Settings, rail popover, Usage page).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all_accounts: bool,
}
/// Where a provider's current limits came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LimitsSource {
    /// A turn-free read of the account (Claude usage endpoint, Codex rate limits, Cursor dashboard).
    Live,
    /// Reported by a running turn.
    Session,
    /// The last report in the event log, from before this daemon read the account.
    Stored,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProviderLimits {
    pub provider: crate::ProviderKind,
    pub limits: Vec<crate::UsageLimit>,
    /// When these values were observed. Absent from older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<LimitsSource>,
    /// Plan name when the provider reports one, e.g. "Pro+".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Why the last live read left these values as they were. Absent while
    /// live reads succeed, and from older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale: Option<LimitsStale>,
    /// While reads are throttled: when the next one may run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Set on per-account entries; the global default per kind leaves it unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
}
/// Why a provider's live read did not update its limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LimitsStale {
    /// The provider's login is due for a refresh, which only the provider's
    /// own session may do; the next session (or turn) renews it.
    LoginRefresh,
    /// The provider is limiting how often usage can be read.
    Throttled,
    /// The read failed: not signed in, unreachable, or an unexpected answer.
    Unavailable,
    /// Read from the cache file of an earlier daemon run, and no live read has
    /// succeeded since. Shown until the first one does.
    FromLastRun,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct UsageLimitsResult {
    pub providers: Vec<ProviderLimits>,
    /// Providers with a live read in flight.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refreshing: Vec<crate::ProviderKind>,
    /// Per-account entries (`instance` set). Absent from older daemons.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<ProviderLimits>,
}
method!(UsageLimits, "usage.limits", Some(Scope::OrchestrationRead), UsageLimitsParams, UsageLimitsResult);
/// Sent to every client that may read orchestration state whenever the limits
/// cache changes or a read finishes. Params are a full `UsageLimitsResult`.
pub const USAGE_LIMITS_CHANGED_NOTIFICATION: &str = "usage.limits.changed";

// ---- access (pairing and tokens) ----

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct PairingCreateParams {
    /// Label for the token that pairing will mint, e.g. "iPhone".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PairingCreateResult {
    /// Six-digit code the other device enters.
    pub code: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    /// Endpoints the daemon is reachable at (loopback first, then LAN addresses).
    pub endpoints: Vec<String>,
}
method!(PairingCreate, "access.pairing.create", Some(Scope::AccessWrite), PairingCreateParams, PairingCreateResult);

/// Which networks the daemon listens on besides loopback.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Exposure {
    /// This machine's Tailscale IPv4 address when Tailscale is running and
    /// the daemon could bind it; `None` when Tailscale is absent or userspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailscale_ip: Option<String>,
    /// Whether the daemon currently listens on the Tailscale address.
    pub tailscale: bool,
    /// Every address the daemon listens on, loopback included.
    pub listeners: Vec<String>,
}
method!(ExposureGet, "access.exposure.get", Some(Scope::AccessRead), Empty, Exposure);

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ExposureSetParams {
    /// Listen on the Tailscale address as well. Persisted in settings.
    pub tailscale: bool,
}
method!(ExposureSet, "access.exposure.set", Some(Scope::AccessWrite), ExposureSetParams, Exposure);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TokenInfo {
    pub id: uuid::Uuid,
    pub label: String,
    pub scopes: Vec<Scope>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub revoked: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TokensListResult {
    pub tokens: Vec<TokenInfo>,
}
method!(TokensList, "access.tokens.list", Some(Scope::AccessRead), Empty, TokensListResult);
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TokensRevokeParams {
    pub token_id: uuid::Uuid,
}
method!(TokensRevoke, "access.tokens.revoke", Some(Scope::AccessWrite), TokensRevokeParams, Empty);

/// Body of `POST /pair` (unauthenticated) and its response.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PairRequest {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PairResponse {
    pub token: String,
    pub scopes: Vec<Scope>,
    pub environment_id: String,
}

// ---- assets ----

/// Response of `POST /assets` (multipart or raw body with `X-Kybern-Filename`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AssetInfo {
    pub id: AssetId,
    pub name: String,
    pub media_type: String,
    pub size: u64,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

// ---- git and GitHub ----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GitStatusParams {
    pub thread_id: ThreadId,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GitStatus {
    pub is_git: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub dirty_files: u32,
    pub ahead: u32,
    pub behind: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    /// Open pull request for this branch, if `gh` knows one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<PullRequest>,
    /// Why a folder that is a repository is reported as `is_git: false`, for a person to act on
    /// (git refuses a folder another user owns).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
    /// Group threads: one entry per member in project order, excluded members
    /// included (`included: false`). The fields above are the primary's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<RepoGitStatus>,
}
/// One member's git state in a group thread.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RepoGitStatus {
    pub repo: String,
    pub name: String,
    pub included: bool,
    pub status: MemberStatus,
    /// `repos` is always empty here.
    #[serde(flatten)]
    pub git: GitStatus,
}
method!(GitStatusMethod, "git.status", Some(Scope::OrchestrationRead), GitStatusParams, GitStatus);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GitBranchesParams {
    pub project_id: ProjectId,
    /// Group projects: the member whose branches to list. Defaults to the primary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Fetch every remote first (prunes deleted branches; never prompts for credentials).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fetch: bool,
    /// Also list remote-tracking branches (`origin/feature`) that have no local branch of the same name.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_remote: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BranchInfo {
    pub name: String,
    pub is_current: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// Committer time of the tip commit, unix seconds.
    pub committed_at: i64,
    /// A remote-tracking branch (`name` is `origin/feature`). Starting a chat on it in the checkout makes a local
    /// branch that tracks it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remote: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GitBranchesResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// Local branches, most recently committed first, then the remote ones when asked for. Empty when the project is
    /// not a repository.
    pub branches: Vec<BranchInfo>,
}
method!(GitBranches, "git.branches", Some(Scope::OrchestrationRead), GitBranchesParams, GitBranchesResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GitCommitParams {
    pub thread_id: ThreadId,
    /// Omit to generate a message from the diff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GitCommitResult {
    /// The primary's commit, or the first member committed in a group thread.
    pub commit: String,
    pub message: String,
    /// Group threads: every member committed, in project order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<RepoCommit>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RepoCommit {
    pub repo: String,
    pub commit: String,
    pub message: String,
}
method!(GitCommit, "git.commit", Some(Scope::OrchestrationOperate), GitCommitParams, GitCommitResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: String,
    pub head: String,
    pub base: String,
    pub is_draft: bool,
    pub author: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_avatar_url: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub author_is_bot: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additions: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletions: Option<u32>,
    /// APPROVED | CHANGES_REQUESTED | REVIEW_REQUIRED
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_decision: Option<String>,
    /// MERGEABLE | CONFLICTING | UNKNOWN
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mergeable: Option<String>,
    /// CLEAN | BLOCKED | BEHIND | DIRTY | UNSTABLE | DRAFT | HAS_HOOKS | UNKNOWN.
    /// Lists carry it so rows group by readiness without a detail fetch per row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_state_status: Option<String>,
    /// Auto-merge is armed on this pull request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_merge: Option<PrAutoMerge>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<PrLabel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks_summary: Option<PrChecksSummary>,
    /// Group projects: the member repository (path relative to the group folder)
    /// this pull request belongs to. None for single-repository projects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

/// GitHub auto-merge: the pull request merges itself once its requirements pass.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrAutoMerge {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_by: Option<String>,
    /// squash | merge | rebase
    pub merge_method: String,
}

/// A person on a pull request (assignee, or any actor).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrPerson {
    pub login: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_bot: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct PrLabel {
    pub name: String,
    /// Hex without '#', as GitHub returns it.
    pub color: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
pub struct PrChecksSummary {
    pub total: u32,
    pub passed: u32,
    pub failed: u32,
    pub pending: u32,
    pub skipped: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrCreateParams {
    pub thread_id: ThreadId,
    /// Omit either to generate from the thread's diff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default)]
    pub draft: bool,
    /// Commit uncommitted changes first (message generated when needed).
    #[serde(default = "default_true_pr")]
    pub commit_first: bool,
}
fn default_true_pr() -> bool {
    true
}
method!(PrCreate, "github.pr.create", Some(Scope::OrchestrationOperate), PrCreateParams, PullRequest);

/// Group threads: one pull request per changed member, cross-linked in their bodies.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrCreateGroupParams {
    pub thread_id: ThreadId,
    /// Members to open pull requests for. Default: every included member with
    /// changes and a GitHub remote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repos: Option<Vec<String>>,
    #[serde(default)]
    pub draft: bool,
    /// Commit uncommitted changes first (messages generated per member).
    #[serde(default = "default_true_pr")]
    pub commit_first: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RepoPullRequest {
    pub repo: String,
    pub pull_request: PullRequest,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrCreateGroupResult {
    /// Created pull requests, in project order.
    pub pull_requests: Vec<RepoPullRequest>,
    /// Members without a pull request: `path` is the member, `reason` a sentence
    /// such as "No GitHub remote", "No changes" or the error.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<RejectedFolder>,
}
method!(PrCreateGroup, "github.pr.create_group", Some(Scope::OrchestrationOperate), PrCreateGroupParams, PrCreateGroupResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrListParams {
    pub project_id: ProjectId,
    #[serde(default = "default_pr_state")]
    pub state: String,
    #[serde(default = "default_pr_limit")]
    pub limit: u32,
}
fn default_pr_state() -> String {
    "open".into()
}
fn default_pr_limit() -> u32 {
    30
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrListResult {
    /// Group projects: the pull requests of every member that answered, in
    /// member order, each tagged with `repo`.
    pub pull_requests: Vec<PullRequest>,
    /// Group projects only: one row per member repository, in order, with the
    /// sentence explaining why its pull requests could not be listed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<PrListRepo>,
}
/// One member repository of a group project in a `github.pr.list` answer.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrListRepo {
    /// Member path, relative to the group folder ("." is the folder itself).
    pub repo: String,
    /// Display name of the member.
    pub name: String,
    /// Why this member has no list: "No GitHub remote", a missing folder or the `gh` error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The member is not on GitHub (no remote, or none `gh` knows): nothing to list, and
    /// clients show no error for it. `error` still says why.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skipped: bool,
    /// What `error` means and how to fix it, when Kybern knows (the same shape as an RPC
    /// error's `data.problem`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<crate::rpc::Problem>,
}
method!(PrList, "github.pr.list", Some(Scope::OrchestrationRead), PrListParams, PrListResult);

// Native pull request review. Pages keep large repositories bounded.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrDetailParams {
    pub project_id: ProjectId,
    pub number: u64,
    /// Group projects: the member repository (a `PullRequest.repo`) the pull
    /// request belongs to. Required for a group project; rejected for a single project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrCheck {
    pub name: String,
    /// QUEUED | IN_PROGRESS | COMPLETED (check runs); empty for a commit status.
    pub status: String,
    /// GitHub's own conclusion, never folded: SUCCESS | FAILURE | CANCELLED | TIMED_OUT |
    /// ACTION_REQUIRED | SKIPPED | NEUTRAL | STALE | STARTUP_FAILURE, or the state of a commit status.
    pub conclusion: String,
    pub url: String,
    /// The branch rules require this check. None when GitHub did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_required: Option<bool>,
    /// The workflow a check run belongs to (a status or an app check has none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_name: Option<String>,
    /// RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// RFC 3339; absent while the check is running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrDetailResult {
    pub pull_request: PullRequest,
    pub body: String,
    pub head_sha: String,
    pub reviewers: Vec<String>,
    pub checks: Vec<PrCheck>,
    pub changed_files: u32,
    /// CLEAN | BLOCKED | BEHIND | DIRTY | UNSTABLE | DRAFT | HAS_HOOKS | UNKNOWN
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_state_status: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviews: Vec<PrReviewer>,
    /// Issue comments plus review threads. None when the count query failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assignees: Vec<PrPerson>,
    /// What the signed-in GitHub account may do here. None when GitHub did not answer
    /// (clients then show no bypass or branch-update actions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewer: Option<PrViewer>,
    /// Commits the head is behind the base. Read only while `merge_state_status` is BEHIND.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behind_by: Option<u32>,
    /// The checks list stopped at 500; `pull_request.checks_summary` covers what was read.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub checks_truncated: bool,
    /// Entries per timeline filter, for tab and filter counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeline_counts: Option<PrTimelineCounts>,
}
/// The signed-in GitHub account's abilities on one pull request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrViewer {
    /// Write access or above: may merge when the rules pass.
    pub can_merge: bool,
    /// May bypass branch protection and merge now ("Merge anyway").
    pub can_merge_as_admin: bool,
    pub can_update_branch: bool,
    pub can_enable_auto_merge: bool,
    pub can_disable_auto_merge: bool,
    /// May edit the pull request, so may take it out of draft.
    pub can_mark_ready: bool,
    /// Merge methods the repository allows, in the order squash, merge, rebase.
    pub merge_methods: Vec<String>,
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
pub struct PrTimelineCounts {
    pub comments: u32,
    pub reviews: u32,
    pub commits: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrReviewer {
    pub login: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_bot: bool,
    /// APPROVED | CHANGES_REQUESTED | COMMENTED | DISMISSED | REQUESTED
    pub state: String,
}
method!(PrDetail, "github.pr.detail", Some(Scope::OrchestrationRead), PrDetailParams, PrDetailResult);
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrPageKind {
    Files,
    Comments,
    Reviews,
    ReviewComments,
    Checks,
    /// One chronological history, oldest first: comments, reviews (with their inline
    /// comments), commits, force pushes, review requests, ready/draft, merge and close.
    /// Paged by `cursor`, not `page`.
    Timeline,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrPageParams {
    pub project_id: ProjectId,
    pub number: u64,
    pub kind: PrPageKind,
    #[serde(default = "default_pr_page")]
    pub page: u32,
    /// Group projects: the member repository (a `PullRequest.repo`) the pull
    /// request belongs to. Required for a group project; rejected for a single project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// `timeline` only: the `next_cursor` of the previous page. Omit for the first page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}
fn default_pr_page() -> u32 {
    1
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrFile {
    pub path: String,
    pub old_path: Option<String>,
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
    pub patch: String,
    pub patch_truncated: bool,
    /// Blob sha of the file at the head; it changes when the file does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrReviewEntry {
    pub id: u64,
    pub author: String,
    pub body: String,
    pub state: String,
    pub path: Option<String>,
    pub line: Option<u32>,
    pub side: Option<String>,
    pub url: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub author_is_bot: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrPageResult {
    #[serde(default)]
    pub checks: Vec<PrCheck>,
    pub files: Vec<PrFile>,
    pub entries: Vec<PrReviewEntry>,
    pub page: u32,
    pub has_more: bool,
    /// `timeline` only, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timeline: Vec<PrTimelineEvent>,
    /// `timeline` only: pass as `cursor` to read the following page. Present while `has_more`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrTimelineKind {
    Comment,
    Review,
    Commit,
    ForcePush,
    ReviewRequested,
    ReadyForReview,
    ConvertedToDraft,
    Merged,
    Closed,
    Reopened,
}
/// An inline comment inside a timeline review.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrTimelineInline {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub body: String,
    pub url: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrTimelineEvent {
    /// GitHub's node id; stable, so it is the row key.
    pub id: String,
    pub kind: PrTimelineKind,
    /// RFC 3339.
    pub created_at: String,
    /// Who did it. Empty when the account was deleted.
    pub author: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub author_is_bot: bool,
    /// Comment or review text (bounded), or a commit's first line.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// `review`: APPROVED | CHANGES_REQUESTED | COMMENTED | DISMISSED | PENDING.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `commit`: the commit. `force_push`: the new head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// `force_push`: the head it replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_sha: Option<String>,
    /// `review_requested`: the user or team asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// `review`: its inline comments (first 20).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inline_comments: Vec<PrTimelineInline>,
    /// `review`: how many inline comments it has in all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_count: Option<u32>,
}
method!(PrPage, "github.pr.page", Some(Scope::OrchestrationRead), PrPageParams, PrPageResult);
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrActionKind {
    Comment,
    Approve,
    RequestChanges,
    Checkout,
    /// Squash unless `merge_method` says otherwise; `admin` bypasses branch protection.
    Merge,
    Close,
    /// Arm GitHub auto-merge with `merge_method` (squash by default).
    EnableAutoMerge,
    DisableAutoMerge,
    /// Merge the base branch into the head. Needs `head_sha`.
    UpdateBranch,
    /// Take a draft out of draft.
    MarkReady,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrInlineComment {
    pub path: String,
    pub line: u32,
    /// RIGHT for the new version, LEFT for a removed line.
    pub side: String,
    pub body: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrActionParams {
    pub project_id: ProjectId,
    pub number: u64,
    pub action: PrActionKind,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub inline_comments: Vec<PrInlineComment>,
    /// Required for checkout, reviews and merge: reject a changed PR head.
    #[serde(default)]
    pub head_sha: String,
    /// Checkout is allowed only into this inactive thread's managed worktree.
    #[serde(default)]
    pub thread_id: Option<ThreadId>,
    /// Verified repair preparation may preserve local changes belonging to this PR.
    #[serde(default)]
    pub for_repair: bool,
    /// Group projects: the member repository (a `PullRequest.repo`) the pull
    /// request belongs to. Required for a group project; rejected for a single project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// `merge` and `enable_auto_merge`: squash (default) | merge | rebase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_method: Option<String>,
    /// `merge`: bypass branch protection ("Merge anyway"). GitHub refuses it unless the
    /// signed-in account may.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub admin: bool,
}
method!(PrAction, "github.pr.action", Some(Scope::OrchestrationOperate), PrActionParams, Empty);

// GitHub Actions through `gh` (ADE-97): workflow runs on every branch, their jobs and steps, bounded
// job logs, reruns, cancel and `workflow_dispatch`. Group projects name the member in `repo`.

/// One workflow run, as GitHub's REST API reports it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowRun {
    pub id: u64,
    /// The run's number within its workflow (`#695`).
    pub number: u64,
    pub attempt: u32,
    /// The workflow's name (`CI`).
    pub workflow_name: String,
    pub workflow_id: u64,
    /// The workflow file, `.github/workflows/ci.yml`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workflow_path: String,
    /// The commit title or the dispatch's run name.
    pub title: String,
    /// The branch or tag the run is on.
    pub head_branch: String,
    pub head_sha: String,
    /// push | pull_request | workflow_dispatch | schedule | …
    pub event: String,
    /// queued | in_progress | completed | waiting | requested | pending
    pub status: String,
    /// success | failure | cancelled | skipped | timed_out | action_required | neutral | stale | startup_failure;
    /// empty until the run completes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub conclusion: String,
    pub url: String,
    /// RFC 3339.
    pub created_at: String,
    /// RFC 3339; the current attempt's start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// RFC 3339.
    pub updated_at: String,
    /// Who started this attempt (a rerun names the person who reran it).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub actor: String,
    /// Pull request numbers this run belongs to (in this repository).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pull_requests: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ActionsRunsParams {
    pub project_id: ProjectId,
    /// Group projects: the member repository. Required for a group project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// A workflow's id or file name (`ci.yml`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// A status (queued, in_progress, completed, …) or a conclusion (success, failure, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(default = "default_actions_page")]
    pub page: u32,
    /// 1–100.
    #[serde(default = "default_actions_per_page")]
    pub per_page: u32,
    /// Pages to read from `page` on, 1–5, so a branch-grouped list holds enough runs; `has_more` speaks for the last.
    #[serde(default = "default_actions_page")]
    pub pages: u32,
    /// Also name each branch's pull request (`pulls`): one `gh pr list` per refresh, kept for a minute.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pulls: bool,
}
fn default_actions_page() -> u32 {
    1
}
fn default_actions_per_page() -> u32 {
    30
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ActionsRunsResult {
    pub runs: Vec<WorkflowRun>,
    /// Runs matching the filters in all.
    pub total_count: u64,
    pub page: u32,
    pub has_more: bool,
    /// The signed-in `gh` account, so clients can tell runs the user started. None when `gh` did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewer: Option<String>,
    /// With `pulls`: the latest pull request of each head branch (the 100 most recent, any state).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pulls: Vec<BranchPull>,
}
/// A head branch and its most recent pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BranchPull {
    pub branch: String,
    pub number: u64,
    /// OPEN | CLOSED | MERGED
    pub state: String,
}
method!(ActionsRuns, "github.actions.runs", Some(Scope::OrchestrationRead), ActionsRunsParams, ActionsRunsResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowStep {
    pub number: u32,
    pub name: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub conclusion: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowJob {
    pub id: u64,
    pub name: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub conclusion: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    pub url: String,
    /// The runner's labels (`ubuntu-latest`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    pub steps: Vec<WorkflowStep>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ActionsRunParams {
    pub project_id: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub run_id: u64,
    /// An earlier attempt to read instead of the latest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ActionsRunResult {
    /// The run as of the attempt read (`attempt`, else the latest).
    pub run: WorkflowRun,
    /// That attempt's jobs, in GitHub's order (first 100).
    pub jobs: Vec<WorkflowJob>,
    /// The jobs the workflow file declares at the run's commit, for the job graph; empty when the file can't be read
    /// (a dynamic workflow, a fork's commit) and the client lays the jobs out in run order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflow_jobs: Vec<WorkflowJobSpec>,
    /// Every attempt's outcome, oldest first, when the run has more than one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<RunAttempt>,
}
/// One job of a workflow file: what the job graph needs to place GitHub's jobs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowJobSpec {
    /// The key under `jobs:`.
    pub id: String,
    /// `name:` as written, `${{ }}` expressions included; empty when unset (GitHub then names the job by its id).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    /// `strategy.matrix`'s keys (`include`/`exclude` left out); empty for a matrix built by an expression. None without a matrix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matrix: Option<Vec<String>>,
    /// A reusable workflow the job calls; its jobs report as `<caller> / <callee>`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub uses: String,
    /// The job has an `if:` condition.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub conditional: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunAttempt {
    pub attempt: u32,
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub conclusion: String,
}
method!(ActionsRun, "github.actions.run", Some(Scope::OrchestrationRead), ActionsRunParams, ActionsRunResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ActionsLogParams {
    pub project_id: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub job_id: u64,
    /// First line to return, 0-based.
    #[serde(default)]
    pub offset: u32,
    /// Lines to return, 1–2000.
    #[serde(default = "default_actions_log_limit")]
    pub limit: u32,
    /// Instead of `offset`: the lines around the job's errors (or its tail), bounded for an agent prompt.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub excerpt: bool,
}
fn default_actions_log_limit() -> u32 {
    500
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ActionsLogResult {
    /// False while the job runs: GitHub publishes a job's log when it finishes.
    pub available: bool,
    /// The lines from `offset`, without GitHub's timestamps. ANSI codes are kept for the client to map.
    pub lines: Vec<String>,
    pub offset: u32,
    pub total_lines: u32,
    /// 0-based lines GitHub marked `##[error]` (first 50).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub error_lines: Vec<u32>,
    /// A line longer than 4 KiB was cut.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clipped: bool,
}
method!(ActionsLog, "github.actions.log", Some(Scope::OrchestrationRead), ActionsLogParams, ActionsLogResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Workflow {
    pub id: u64,
    pub name: String,
    /// `.github/workflows/ci.yml`
    pub path: String,
    /// active | disabled_manually | disabled_inactivity | …
    pub state: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ActionsWorkflowsParams {
    pub project_id: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ActionsWorkflowsResult {
    pub workflows: Vec<Workflow>,
    /// The repository's default branch, where a dispatch runs unless told otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default_branch: String,
}
method!(ActionsWorkflows, "github.actions.workflows", Some(Scope::OrchestrationRead), ActionsWorkflowsParams, ActionsWorkflowsResult);

/// One `workflow_dispatch` input.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowInput {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default: String,
    /// string | boolean | choice | number | environment
    pub kind: String,
    /// `choice` only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ActionsWorkflowParams {
    pub project_id: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// A workflow's id or file name.
    pub workflow: String,
    /// Read the workflow file at this branch. Default: the repository's default branch.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ActionsWorkflowResult {
    /// The file has a `workflow_dispatch` trigger, so it can be started by hand.
    pub dispatchable: bool,
    pub inputs: Vec<WorkflowInput>,
}
method!(ActionsWorkflow, "github.actions.workflow", Some(Scope::OrchestrationRead), ActionsWorkflowParams, ActionsWorkflowResult);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActionsActionKind {
    /// Run every job again.
    Rerun,
    /// Run the failed jobs (and what they need) again.
    RerunFailed,
    /// Run one job (`job_id`) again.
    RerunJob,
    Cancel,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ActionsActionParams {
    pub project_id: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub action: ActionsActionKind,
    pub run_id: u64,
    /// `rerun_job` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<u64>,
}
method!(ActionsAction, "github.actions.action", Some(Scope::OrchestrationOperate), ActionsActionParams, Empty);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ActionsDispatchParams {
    pub project_id: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// A workflow's id or file name.
    pub workflow: String,
    /// The branch or tag to run on.
    #[serde(rename = "ref")]
    pub git_ref: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, String>,
}
method!(ActionsDispatch, "github.actions.dispatch", Some(Scope::OrchestrationOperate), ActionsDispatchParams, Empty);

// The desktop's "star Kybern on GitHub" prompt (ADE-117), through this machine's `gh`. The repository is
// fixed (`Tresnanda/kybern`): neither method can read or star any other.

/// `github.starred`: no parameters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct GithubStarredParams {}
/// Whether `gh`'s signed-in account starred Kybern's repository.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GithubStarredResult {
    /// GitHub answered 204: starred. False when it answered 404, and when `gh` is missing or signed out.
    pub starred: bool,
    /// `gh` is installed and signed in, so `github.star` can star the repository.
    pub can_star: bool,
}
method!(GithubStarred, "github.starred", Some(Scope::OrchestrationRead), GithubStarredParams, GithubStarredResult);

/// `github.star`: no parameters. Stars Kybern's repository as `gh`'s signed-in account; an error when
/// `gh` is missing, signed out or GitHub refuses (the client then opens the repository page instead).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct GithubStarParams {}
method!(GithubStar, "github.star", Some(Scope::OrchestrationOperate), GithubStarParams, Empty);
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorktreeInspectParams {
    pub thread_id: ThreadId,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorktreeInspectResult {
    pub path: String,
    pub branch: String,
    pub exists: bool,
    pub clean: bool,
    pub merged: bool,
    pub ignored_files: u32,
    pub blockers: Vec<String>,
    pub eligible: bool,
}
method!(WorktreeInspect, "threads.worktree.inspect", Some(Scope::OrchestrationRead), WorktreeInspectParams, WorktreeInspectResult);
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorktreeRemoveParams {
    pub thread_id: ThreadId,
    /// Explicit confirmation for dirty or unmerged work. Dirty source is retained in a recovery ref.
    #[serde(default)]
    pub force: bool,
    /// Only merged branches can be deleted. The default keeps the branch.
    #[serde(default)]
    pub delete_branch: bool,
}
method!(WorktreeRemove, "threads.worktree.remove", Some(Scope::OrchestrationOperate), WorktreeRemoveParams, WorktreeInspectResult);

// ---- files ----

/// Find files in a project by fuzzy path match, for @mentions in the composer.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FilesSearchParams {
    pub project_id: ProjectId,
    /// Case-insensitive; every character must appear in order in the path.
    #[serde(default)]
    pub query: String,
    #[serde(default = "default_files_limit")]
    pub limit: u32,
}
fn default_files_limit() -> u32 {
    30
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FilesSearchResult {
    /// Paths relative to the project root, best match first.
    pub files: Vec<String>,
    /// Total files considered, so clients can say "of N".
    pub total: u32,
}
method!(FilesSearch, "files.search", Some(Scope::OrchestrationRead), FilesSearchParams, FilesSearchResult);

/// One entry of a directory listing, for the file explorer.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FileEntry {
    pub name: String,
    /// Path relative to the project root.
    pub path: String,
    pub kind: FileEntryKind,
    /// Bytes, for files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileEntryKind {
    File,
    Directory,
}

/// List one directory of a project (directories first, then files, sorted by name).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FilesListParams {
    pub project_id: ProjectId,
    /// Directory relative to the project root; empty for the root.
    #[serde(default)]
    pub path: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FilesListResult {
    pub entries: Vec<FileEntry>,
}
method!(FilesList, "files.list", Some(Scope::OrchestrationRead), FilesListParams, FilesListResult);

/// Read a text file of a project, capped at `max_bytes`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FilesReadParams {
    pub project_id: ProjectId,
    /// File relative to the project root.
    pub path: String,
    #[serde(default = "default_files_read_max")]
    pub max_bytes: u64,
    /// A `version` from an earlier read: when the file still has it, the
    /// result is `unchanged` with no content (a cheap "changed on disk" check).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_version: Option<String>,
}
fn default_files_read_max() -> u64 {
    512 * 1024
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FilesReadResult {
    /// UTF-8 content (lossy), empty for binary files.
    pub content: String,
    /// True when the file was cut at `max_bytes`.
    pub truncated: bool,
    /// True when the file does not look like text.
    pub binary: bool,
    /// Full size on disk in bytes.
    pub size: u64,
    /// Opaque on-disk version (modification time and size). Pass it back as
    /// `if_version` or as a write's `expected_version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// True when `if_version` still matched; `content` is then empty.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unchanged: bool,
}
method!(FilesRead, "files.read", Some(Scope::OrchestrationRead), FilesReadParams, FilesReadResult);

/// Replace the contents of an existing text file of a project (atomically).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FilesWriteParams {
    pub project_id: ProjectId,
    /// File relative to the project root.
    pub path: String,
    /// The new UTF-8 contents, at most 2 MiB.
    pub content: String,
    /// The `version` the edit was based on. When the file changed since, the
    /// write fails with a conflict instead of overwriting it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FilesWriteResult {
    /// The file's new on-disk version.
    pub version: String,
    /// Size on disk in bytes.
    pub size: u64,
}
method!(FilesWrite, "files.write", Some(Scope::OrchestrationOperate), FilesWriteParams, FilesWriteResult);

/// Read a linked file inside the conversation's workspace, including its worktree.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadFileReadParams {
    pub thread_id: ThreadId,
    /// Relative to the thread cwd, or an absolute path inside it.
    pub path: String,
    #[serde(default = "default_files_read_max")]
    pub max_bytes: u64,
    /// See `FilesReadParams::if_version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_version: Option<String>,
}
method!(ThreadFileRead, "threads.files.read", Some(Scope::OrchestrationRead), ThreadFileReadParams, FilesReadResult);

/// Replace the contents of an existing text file inside the conversation's
/// workspace (the same files `threads.files.read` opens), atomically.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadFileWriteParams {
    pub thread_id: ThreadId,
    /// Relative to the thread cwd, or an absolute path inside it.
    pub path: String,
    /// The new UTF-8 contents, at most 2 MiB.
    pub content: String,
    /// See `FilesWriteParams::expected_version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<String>,
}
method!(ThreadFileWrite, "threads.files.write", Some(Scope::OrchestrationOperate), ThreadFileWriteParams, FilesWriteResult);

/// Find which of an answer's file references name a file in the conversation's
/// workspace, so a client can show them as file links.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadFilesResolveParams {
    pub thread_id: ThreadId,
    /// Paths as written, without a `:line` or `#L` suffix: relative to the
    /// thread cwd, absolute inside it, or a bare file name. At most 200.
    pub paths: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadFilesResolveResult {
    /// One entry per requested path, in order: the file relative to the thread
    /// cwd (what `threads.files.read` opens), or null when the path names no
    /// file, a file outside the workspace, or a bare name shared by several files.
    pub paths: Vec<Option<String>>,
}
method!(ThreadFilesResolve, "threads.files.resolve", Some(Scope::OrchestrationRead), ThreadFilesResolveParams, ThreadFilesResolveResult);

// ---- skills ----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SkillsListParams {
    /// Omit to follow project/provider defaults; use "default" for the CLI account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    pub project_id: ProjectId,
    pub provider: ProviderKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SkillsListResult {
    pub skills: Vec<SkillInfo>,
}
method!(SkillsList, "skills.list", Some(Scope::OrchestrationRead), SkillsListParams, SkillsListResult);

// ---- approvals ----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalsRespondParams {
    pub approval_id: ApprovalId,
    #[serde(flatten)]
    pub decision: ApprovalDecision,
}
method!(ApprovalsRespond, "approvals.respond", Some(Scope::OrchestrationOperate), ApprovalsRespondParams, Empty);

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalsListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalsListResult {
    pub approvals: Vec<ApprovalRequest>,
}
method!(ApprovalsList, "approvals.list", Some(Scope::OrchestrationRead), ApprovalsListParams, ApprovalsListResult);

// ---- events ----

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct EventsSubscribeParams {
    /// Restrict to one thread. Omit to receive every thread's events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
    /// Replay persisted events with `seq > after_seq` before going live. Omit for live only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_seq: Option<EventSeq>,
    /// Include completed tool output in replay and live notifications. Omit or
    /// set true for the legacy full event stream; false sends large results as
    /// an explicit `output_omitted` marker that can be fetched by call id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_tool_output: Option<bool>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EventsSubscribeResult {
    pub subscription_id: SubscriptionId,
    /// Latest persisted seq at subscribe time. Replay covers up to and including this.
    pub head_seq: EventSeq,
    /// This daemon sends `events.ready` after replay, before live delivery resumes.
    #[serde(default)]
    pub replay_ready: bool,
}
method!(EventsSubscribe, "events.subscribe", Some(Scope::OrchestrationRead), EventsSubscribeParams, EventsSubscribeResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EventsUnsubscribeParams {
    pub subscription_id: SubscriptionId,
}
method!(EventsUnsubscribe, "events.unsubscribe", Some(Scope::OrchestrationRead), EventsUnsubscribeParams, Empty);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EventsRangeParams {
    pub thread_id: ThreadId,
    #[serde(default)]
    pub after_seq: EventSeq,
    #[serde(default = "default_limit")]
    pub limit: u32,
}
fn default_limit() -> u32 {
    500
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EventsRangeResult {
    pub events: Vec<ThreadEvent>,
    pub has_more: bool,
}
method!(EventsRange, "events.range", Some(Scope::OrchestrationRead), EventsRangeParams, EventsRangeResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IntegrationsListParams {
    pub project_id: ProjectId,
    pub provider: ProviderKind,
}
method!(IntegrationsList, "integrations.list", Some(Scope::OrchestrationRead), IntegrationsListParams, IntegrationsCatalog);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IntegrationChangeParams {
    pub project_id: ProjectId,
    pub provider: ProviderKind,
    pub id: String,
    pub kind: IntegrationKind,
    pub scope: Option<String>,
    pub action: IntegrationAction,
}
method!(IntegrationChange, "integrations.change", Some(Scope::OrchestrationOperate), IntegrationChangeParams, IntegrationChangeResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IntegrationLoginParams {
    pub thread_id: ThreadId,
    pub name: String,
}
method!(IntegrationLogin, "integrations.login", Some(Scope::TerminalOperate), IntegrationLoginParams, TerminalInfo);

// ---- inline HTML visual replies ----
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HtmlPublishParams {
    pub thread_id: ThreadId,
    pub html: String,
    pub title: String,
    pub height: u32,
    /// Agents only (`kybern_html_publish`): the call waits for the reader's answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub await_answer: Option<bool>,
    /// JSON Schema the answer must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_schema: Option<serde_json::Value>,
    /// How long to wait for the answer, 30–3600 seconds (default 900).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HtmlPublishResult {
    pub visual: HtmlVisual,
}
method!(HtmlPublish, "threads.visuals.publish", Some(Scope::OrchestrationOperate), HtmlPublishParams, HtmlPublishResult);
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HtmlPreviewParams {
    pub thread_id: ThreadId,
    pub html: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub appearance: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HtmlConsoleMessage {
    pub level: String,
    pub text: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HtmlPreviewResult {
    pub width: u32,
    pub content_height: u32,
    pub captured_height: u32,
    pub console_messages: Vec<HtmlConsoleMessage>,
    pub missing_images: Vec<String>,
    /// PNG screenshot, transient: never stored in the transcript event log.
    pub screenshot: String,
}
method!(HtmlPreview, "threads.visuals.preview", Some(Scope::OrchestrationRead), HtmlPreviewParams, HtmlPreviewResult);
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HtmlReadParams {
    pub thread_id: ThreadId,
    pub visual_id: uuid::Uuid,
    /// Optional source-preview byte budget. Full source remains the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u32>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HtmlReadResult {
    pub html: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}
method!(HtmlRead, "threads.visuals.read", Some(Scope::OrchestrationRead), HtmlReadParams, HtmlReadResult);
method!(HtmlFrame, "threads.visuals.frame", Some(Scope::OrchestrationRead), HtmlReadParams, ArtifactPreviewResult);
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HtmlRevokeParams {
    pub thread_id: ThreadId,
    pub ticket: String,
}
method!(HtmlRevoke, "threads.visuals.revoke", Some(Scope::OrchestrationRead), HtmlRevokeParams, Empty);
/// A picture of a visual for the transcript: rendered headlessly at the
/// column width in the reader's theme, on a transparent background.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HtmlSnapshotParams {
    pub thread_id: ThreadId,
    pub visual_id: uuid::Uuid,
    /// The figure's width in CSS pixels, 240–1600.
    pub width: u32,
    /// The theme the page receives: `{appearance, variables}`, as in `#kybern-theme=`.
    pub theme: serde_json::Value,
    /// The answered state to draw (`{state, answer}`, as `kybern-answer-state` carries), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<serde_json::Value>,
}
/// One control in a snapshot, in CSS pixels from the page's top-left.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct VisualControl {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Corner radius.
    pub r: f32,
    /// `button`, `link`, `tab`, `switch`, `checkbox`, `radio`, `slider`, `field` or `select`.
    pub role: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HtmlSnapshotResult {
    /// Names the picture: `GET /threads/{thread}/visuals/{visual}/snapshot/{key}.png`
    /// (bearer token), and `{key}-fade.png` when `clamped`.
    pub key: String,
    pub width: u32,
    /// Content height in CSS pixels (the picture is twice as many device pixels).
    pub height: u32,
    pub scale: u32,
    /// Content taller than the clamp has a picture cut at `clamp` that fades out.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clamped: bool,
    pub clamp: u32,
    pub controls: Vec<VisualControl>,
}
method!(HtmlSnapshot, "threads.visuals.snapshot", Some(Scope::OrchestrationRead), HtmlSnapshotParams, HtmlSnapshotResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactsListParams {
    pub thread_id: ThreadId,
    pub before_seq: Option<EventSeq>,
    #[serde(default = "default_artifact_limit")]
    pub limit: u32,
}
fn default_artifact_limit() -> u32 {
    30
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactTool {
    pub seq: EventSeq,
    pub at: chrono::DateTime<chrono::Utc>,
    pub call: ToolCall,
    pub output: Option<serde_json::Value>,
    pub is_error: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactsListResult {
    pub artifacts: Vec<ArtifactTool>,
    pub next_before_seq: Option<EventSeq>,
}
method!(ArtifactsList, "threads.artifacts.list", Some(Scope::OrchestrationRead), ArtifactsListParams, ArtifactsListResult);
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactReadParams {
    pub thread_id: ThreadId,
    pub path: String,
}
method!(ArtifactRead, "threads.artifacts.read", Some(Scope::OrchestrationRead), ArtifactReadParams, FilesReadResult);
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactPreviewResult {
    pub ticket: String,
}
method!(ArtifactPreview, "threads.artifacts.preview", Some(Scope::OrchestrationRead), ArtifactReadParams, ArtifactPreviewResult);

// ---- in-app browser preview ----

/// Open a page in the Preview panel: a local HTML file, a dev server, or an
/// external address. The daemon normalizes the target, checks folder grants
/// and mints a ticket for file and relayed pages.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PreviewOpenParams {
    pub thread_id: ThreadId,
    /// Raw input: a URL, a file path, or `host:port`.
    pub target: String,
    /// The user confirmed the folder grant card.
    #[serde(default)]
    pub allow_folder: bool,
    /// Mint a proxy ticket for a server target (remote environments and
    /// "Preview through Kybern").
    #[serde(default)]
    pub proxy: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PreviewTargetInfo {
    File { path: String, root: String, in_project: bool },
    Server { url: String, port: u16 },
    External { url: String },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PreviewFolderRequest {
    pub folder: String,
    pub grantable: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PreviewOpenResult {
    pub target: PreviewTargetInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    /// HTTP path to load, e.g. `/preview-files/{ticket}/mock/index.html`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_permission: Option<PreviewFolderRequest>,
}
method!(PreviewOpen, "previews.open", Some(Scope::OrchestrationOperate), PreviewOpenParams, PreviewOpenResult);

/// Open a loopback dev server on the serving daemon, independent of any thread's machine.
/// This cannot open files or grant access to folders. Proxy tickets belong to the calling device.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PreviewServerOpenParams {
    pub url: String,
    #[serde(default)]
    pub proxy: bool,
}
method!(PreviewServerOpen, "previews.server.open", Some(Scope::OrchestrationOperate), PreviewServerOpenParams, PreviewOpenResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PreviewCloseParams {
    pub ticket: String,
}
method!(PreviewClose, "previews.close", Some(Scope::OrchestrationOperate), PreviewCloseParams, Empty);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PreviewProbeParams {
    /// Loopback or private-network address only.
    pub url: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PreviewProbeError {
    ConnectionRefused,
    TimedOut,
    Dns,
    Tls,
    HttpStatus,
    NotHttp,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PreviewFrameBlock {
    /// `x-frame-options` or `content-security-policy`.
    pub header: String,
    pub value: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PreviewProbeResult {
    pub reachable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<PreviewProbeError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<PreviewFrameBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}
method!(PreviewProbe, "previews.probe", Some(Scope::OrchestrationOperate), PreviewProbeParams, PreviewProbeResult);

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct PreviewServersListParams {
    #[serde(default)]
    pub thread_id: Option<ThreadId>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PreviewServer {
    pub url: String,
    pub port: u16,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Data URL, at most 16 KB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favicon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framework: Option<String>,
    #[serde(default)]
    pub in_project: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PreviewServersListResult {
    pub servers: Vec<PreviewServer>,
    pub scanned_at: chrono::DateTime<chrono::Utc>,
    /// `lsof`, `proc` or `common_ports`.
    pub method: String,
}
method!(PreviewServersList, "previews.servers.list", Some(Scope::OrchestrationRead), PreviewServersListParams, PreviewServersListResult);

/// Pushed to clients when an agent asks to show a page. Not persisted.
pub const PREVIEW_OPEN_REQUESTED_NOTIFICATION: &str = "previews.open_requested";
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PreviewOpenRequestedNotification {
    pub thread_id: ThreadId,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub requested_by_agent: bool,
}

// ---- notes ----

/// Notification method delivered to every client that can read orchestration
/// state after a note changes. Older clients ignore it.
pub const NOTES_CHANGED_NOTIFICATION: &str = "notes.changed";

pub type NoteId = uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NoteScope {
    Global,
    Project,
    /// Exactly one per thread; titled after the thread.
    Thread,
}

impl NoteScope {
    pub fn as_str(self) -> &'static str {
        match self {
            NoteScope::Global => "global",
            NoteScope::Project => "project",
            NoteScope::Thread => "thread",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NoteChecklist {
    pub done: u32,
    pub total: u32,
}

/// What a project note is for. The brief is the user's own instructions for the
/// project; the setup note records how to build, run and test it and is kept by the
/// project's Coordinator. Ordinary notes (memory) have no role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectNoteRole {
    Brief,
    Setup,
}

impl ProjectNoteRole {
    pub fn as_str(self) -> &'static str {
        match self {
            ProjectNoteRole::Brief => "brief",
            ProjectNoteRole::Setup => "setup",
        }
    }
}

/// What kind of memory a project note holds. Agents file dated claims under one;
/// the Coordinator's notes index groups by it. A note without a category is
/// "Other", or "Undated" when it predates categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NoteCategory {
    /// What the user wants, in their words.
    Preference,
    Decision,
    Convention,
    Gotcha,
}

impl NoteCategory {
    pub const ALL: [NoteCategory; 4] = [NoteCategory::Preference, NoteCategory::Decision, NoteCategory::Convention, NoteCategory::Gotcha];

    pub fn as_str(self) -> &'static str {
        match self {
            NoteCategory::Preference => "preference",
            NoteCategory::Decision => "decision",
            NoteCategory::Convention => "convention",
            NoteCategory::Gotcha => "gotcha",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|category| category.as_str() == text)
    }

    /// Decisions and gotchas are history: agents add dated claims and retire old
    /// ones instead of rewriting the body.
    pub fn is_history(self) -> bool {
        matches!(self, NoteCategory::Decision | NoteCategory::Gotcha)
    }
}

/// Why a note may be outdated: a claim in it cites a commit (`As of \`abc1234\``)
/// and a file, and that file changed after the commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StaleInfo {
    /// The cited path that changed, as the note writes it.
    pub path: String,
    /// Short id of the latest commit that changed it after the cited one.
    pub commit: String,
}

/// A note without its body, as listed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NoteSummary {
    pub id: NoteId,
    pub scope: NoteScope,
    /// The project of a project note, or the thread's project for a thread note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
    /// A thread note's title is the thread's title.
    pub title: String,
    /// Plain-text excerpt of the body, markdown syntax removed, at most 240 characters.
    pub preview: String,
    pub checklist: NoteChecklist,
    pub pinned: bool,
    /// Content revision: bumps when the title or body changes. 0 means no saved note yet.
    pub revision: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// Set while the note is in Recently deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Where a deleted note came from once its project or thread was removed,
    /// such as "kybern" or "kybern › Fix login".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// The thread whose agent created the note with its native tools. Absent for
    /// notes the user wrote; agents may only append to those.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by_thread: Option<ThreadId>,
    /// Set on a project's brief and setup notes (one of each per project). Only the
    /// user edits the brief; only the project's Coordinator writes the setup note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_role: Option<ProjectNoteRole>,
    /// The kind of memory this note holds. Absent for ordinary notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<NoteCategory>,
    /// Set when a dated claim in the note cites a file that changed since the
    /// commit it names. Computed by the daemon from git, never stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale: Option<StaleInfo>,
    /// A project note without a category that was written before categories existed
    /// (see `NotesListResult::history_since`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub undated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Note {
    #[serde(flatten)]
    pub summary: NoteSummary,
    /// Markdown.
    pub body: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct NotesListResult {
    pub notes: Vec<NoteSummary>,
    /// When categories were introduced. Uncategorized project notes created before
    /// it are `undated`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_since: Option<chrono::DateTime<chrono::Utc>>,
    /// Which machine each note came from, and which machines were left out. Only with `machine`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub federation: Option<Federation>,
}
method!(NotesList, "notes.list", Some(Scope::OrchestrationRead), NotesListParams, NotesListResult);

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct NotesGetParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<NoteId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct NotesGetResult {
    /// Absent when the thread has no note yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<Note>,
}
method!(NotesGet, "notes.get", Some(Scope::OrchestrationRead), NotesGetParams, NotesGetResult);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NotesCreateParams {
    /// Only `global` or `project`; thread notes are created by `notes.update` with `thread_id`.
    pub scope: NoteScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Creates the project's pinned brief or setup note (scope `project` with a
    /// `project_id`). A project has at most one of each. Absent for an ordinary note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_role: Option<ProjectNoteRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<NoteCategory>,
}
method!(NotesCreate, "notes.create", Some(Scope::OrchestrationOperate), NotesCreateParams, Note);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NotesUpdateParams {
    /// Exactly one of `id` or `thread_id`. `thread_id` with `expected_revision: 0` creates the thread's note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<NoteId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
    /// Fails with CONFLICT (-32004) when the note changed elsewhere.
    pub expected_revision: i64,
    /// Ignored for thread notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Sets the note's category. Absent leaves it as it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<NoteCategory>,
    /// Removes the note's category. Not together with `category`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_category: bool,
}
method!(NotesUpdate, "notes.update", Some(Scope::OrchestrationOperate), NotesUpdateParams, Note);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NotesPinParams {
    pub id: NoteId,
    pub pinned: bool,
}
method!(NotesPin, "notes.pin", Some(Scope::OrchestrationOperate), NotesPinParams, NoteSummary);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NotesMoveParams {
    pub id: NoteId,
    /// Only `global` or `project`; thread notes cannot be moved.
    pub scope: NoteScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
}
method!(NotesMove, "notes.move", Some(Scope::OrchestrationOperate), NotesMoveParams, NoteSummary);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NotesIdParams {
    pub id: NoteId,
}
// Soft delete: the note moves to Recently deleted for 30 days.
method!(NotesDelete, "notes.delete", Some(Scope::OrchestrationOperate), NotesIdParams, NoteSummary);
method!(NotesRestore, "notes.restore", Some(Scope::OrchestrationOperate), NotesIdParams, NoteSummary);
// Permanent removal; only allowed for a deleted note.
method!(NotesPurge, "notes.purge", Some(Scope::OrchestrationOperate), NotesIdParams, Empty);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NotesSearchParams {
    pub query: String,
    /// Defaults to 50, at most 200.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// `"all"` searches every online machine; a name or id searches that machine. Absent: this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NoteSearchHit {
    pub id: NoteId,
    /// About 120 characters around the first match.
    pub snippet: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct NotesSearchResult {
    pub results: Vec<NoteSearchHit>,
    /// Which machine each hit came from, and which machines were left out. Only with `machine`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub federation: Option<Federation>,
}
method!(NotesSearch, "notes.search", Some(Scope::OrchestrationRead), NotesSearchParams, NotesSearchResult);

/// Stale notes a project needs before the Coordinator offers to tidy them.
pub const NOTES_TIDY_THRESHOLD: u32 = 5;
/// Days the "Tidy notes" offer stays quiet after the user chose "Not now".
pub const NOTES_TIDY_SNOOZE_DAYS: i64 = 7;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NotesTidyParams {
    pub project_id: ProjectId,
}

/// The state of a project's "Tidy notes" offer: the card "N notes may be outdated".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TidyOffer {
    /// Live project notes whose claims cite files that changed since.
    pub stale: Vec<NoteId>,
    /// True when `stale` holds at least [`NOTES_TIDY_THRESHOLD`] notes and the
    /// offer is not snoozed: clients show the card.
    pub offer: bool,
    /// While set and in the future, the offer is snoozed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snoozed_until: Option<chrono::DateTime<chrono::Utc>>,
}
method!(NotesTidyOffer, "notes.tidy_offer", Some(Scope::OrchestrationRead), NotesTidyParams, TidyOffer);

// "Not now": snoozes the offer for 7 days. Returns the offer as it is now.
method!(NotesTidySnooze, "notes.tidy_snooze", Some(Scope::OrchestrationOperate), NotesTidyParams, TidyOffer);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NotesTidyStartResult {
    /// The Coordinator thread that received the request.
    pub thread_id: ThreadId,
    /// How many flagged notes the request named.
    pub notes: u32,
}
// "Tidy notes": sends the project's Coordinator a request to check the flagged
// claims against HEAD and propose changes. Fails when the project has no
// Coordinator, or none of its notes is flagged.
method!(NotesTidyStart, "notes.tidy_start", Some(Scope::OrchestrationOperate), NotesTidyParams, NotesTidyStartResult);

/// Params of the `notes.changed` notification.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NotesChangedNotification {
    /// The note after the change, including soft deletes and restores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<NoteSummary>,
    /// Set instead of `note` when a note was permanently removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purged_id: Option<NoteId>,
}

// ---- user tasks ----

/// Notification method delivered to every client that can read orchestration
/// state after a user task changes. Older clients ignore it.
pub const TASK_ITEMS_CHANGED_NOTIFICATION: &str = "tasks.items.changed";

pub type TaskItemId = uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskScope {
    Global,
    Project,
}

/// `Running` and `NeedsReview` are owned by the latest run; the rest are set by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Inbox,
    Todo,
    Running,
    NeedsReview,
    Done,
    Canceled,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Inbox => "inbox",
            TaskStatus::Todo => "todo",
            TaskStatus::Running => "running",
            TaskStatus::NeedsReview => "needs_review",
            TaskStatus::Done => "done",
            TaskStatus::Canceled => "canceled",
        }
    }

    /// Statuses a user may set by hand.
    pub fn is_user_settable(self) -> bool {
        matches!(self, TaskStatus::Inbox | TaskStatus::Todo | TaskStatus::Done | TaskStatus::Canceled)
    }
}

/// 0 none, 1 urgent, 2 high, 3 medium, 4 low (Linear order).
pub type TaskPriority = u8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskRunState {
    /// Live: the agent is working.
    Running,
    /// Live: waiting for an approval or an answer.
    Waiting,
    Completed,
    Failed,
    Interrupted,
}

impl TaskRunState {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskRunState::Running => "running",
            TaskRunState::Waiting => "waiting",
            TaskRunState::Completed => "completed",
            TaskRunState::Failed => "failed",
            TaskRunState::Interrupted => "interrupted",
        }
    }

    pub fn is_live(self) -> bool {
        matches!(self, TaskRunState::Running | TaskRunState::Waiting)
    }
}

/// Lines added and removed by a run's changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskRunDiff {
    pub added: u32,
    pub removed: u32,
    pub files: u32,
}

/// A note sent with a run and the revision it was at when the run started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskRunNote {
    pub note_id: NoteId,
    pub revision: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TaskRun {
    pub thread_id: ThreadId,
    /// 1-based within the task.
    pub number: u32,
    pub provider: ProviderInstance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    pub state: TaskRunState,
    /// What the agent is doing now, such as "Editing src/main.rs". Live runs only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    /// Lines added and removed by the run's changes, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<TaskRunDiff>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<TaskRunNote>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TaskItem {
    pub id: TaskItemId,
    /// Display id such as `ADE-14`; global tasks use `TSK`. Never reused.
    pub key: String,
    pub scope: TaskScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    pub title: String,
    /// Markdown: description, optionally followed by acceptance criteria as a checklist.
    pub body: String,
    pub status: TaskStatus,
    pub priority: TaskPriority,
    /// Position within its status column, ascending.
    pub rank: f64,
    /// Notes attached as context.
    pub note_ids: Vec<NoteId>,
    /// The note whose checklist line created this task, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_note_id: Option<NoteId>,
    /// Saved follow-up text for the next run, when no run could receive it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_followup: Option<String>,
    pub runs: Vec<TaskRun>,
    /// Content revision for title and body edits.
    pub revision: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// When the status last changed.
    pub status_changed_at: chrono::DateTime<chrono::Utc>,
    /// The thread whose agent created the task with its native tools. Absent for
    /// tasks the user created; agents may only append to and check items of those.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by_thread: Option<ThreadId>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TaskItemsListResult {
    pub tasks: Vec<TaskItem>,
    /// Which machine each task came from, and which machines were left out. Only with `machine`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub federation: Option<Federation>,
}
method!(TaskItemsList, "tasks.items.list", Some(Scope::OrchestrationRead), TaskItemsListParams, TaskItemsListResult);

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TaskItemsGetParams {
    /// Exactly one of `id` or `key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<TaskItemId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TaskItemsGetResult {
    /// Absent when no live task matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskItem>,
}
method!(TaskItemsGet, "tasks.items.get", Some(Scope::OrchestrationRead), TaskItemsGetParams, TaskItemsGetResult);

/// A checklist line of a note that becomes a task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TaskItemSource {
    pub note_id: NoteId,
    /// The line's text, with or without its `- [ ]` marker.
    pub line_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskItemsCreateParams {
    pub scope: TaskScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Defaults to `inbox`. Only user statuses are accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TaskStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<TaskPriority>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note_ids: Option<Vec<NoteId>>,
    /// Create from a checklist line: the daemon rewrites that line to end with `[KEY](kybern://task/<id>)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<TaskItemSource>,
}
method!(TaskItemsCreate, "tasks.items.create", Some(Scope::OrchestrationOperate), TaskItemsCreateParams, TaskItem);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskItemsUpdateParams {
    pub id: TaskItemId,
    /// Required when `title` or `body` change. Fails with CONFLICT (-32004) when the task changed elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// User statuses only: inbox, todo, done, canceled. Sending a status without
    /// `before_id` places the task at the end of that column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TaskStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<TaskPriority>,
    /// Move between Global and a project. The key changes only when the task has no runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<TaskScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    /// Replaces the attached notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note_ids: Option<Vec<NoteId>>,
    /// Replaces the saved follow-up; an empty string clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_followup: Option<String>,
    /// Place before this task in the target status column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_id: Option<TaskItemId>,
}
method!(TaskItemsUpdate, "tasks.items.update", Some(Scope::OrchestrationOperate), TaskItemsUpdateParams, TaskItem);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskItemsIdParams {
    pub id: TaskItemId,
}
// Soft delete: the task is hidden and removed for good after 30 days unless restored.
method!(TaskItemsDelete, "tasks.items.delete", Some(Scope::OrchestrationOperate), TaskItemsIdParams, Empty);
method!(TaskItemsRestore, "tasks.items.restore", Some(Scope::OrchestrationOperate), TaskItemsIdParams, TaskItem);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskItemsSendParams {
    pub id: TaskItemId,
    pub provider: ProviderInstance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_worktree: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    /// Required for global tasks: the project the run happens in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    /// The (possibly edited) prompt text. Exactly one of `prompt` or `message` is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// The full first message (text, mentions, skills, files, thread references,
    /// attachments). The daemon adds a `kybern://task/<id>` mention for this task
    /// at the start when the message has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<UserMessage>,
    /// Notes attached as `kybern://note/<id>` mentions; the daemon expands them in the provider's copy only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note_ids: Option<Vec<NoteId>>,
    /// A client-chosen id for the run's thread (see `threads.create`). A repeat with the same id
    /// returns the run it started; after a failed first message the thread is kept, not archived,
    /// and a retry with this id starts the run in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
    /// The client's id for the first message (see `threads.send`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<MessageId>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskItemsSendResult {
    pub task: TaskItem,
    pub thread_id: ThreadId,
}
method!(TaskItemsSend, "tasks.items.send", Some(Scope::OrchestrationOperate), TaskItemsSendParams, TaskItemsSendResult);

/// How `tasks.items.send_batch` turns several tasks into agent threads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskBatchMode {
    /// One thread per task, started in parallel. Each message carries its own task.
    #[default]
    Separate,
    /// One thread for every task. Each task records a run that points at it, they
    /// all move to Running and Needs review together, and a follow-up from any of
    /// them goes to the shared thread.
    Combined,
}

/// Send several tasks to an agent at once. Mirrors [`TaskItemsSendParams`], with
/// `ids` instead of `id`. The same message and agent settings apply to every task.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskItemsSendBatchParams {
    /// The tasks to send, in order. Repeats are ignored. At most 200.
    pub ids: Vec<TaskItemId>,
    #[serde(default)]
    pub mode: TaskBatchMode,
    pub provider: ProviderInstance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_worktree: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    /// The project to run global tasks in. Required when any task is global; tasks
    /// of a project always run in their own project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    /// The prompt text. Exactly one of `prompt` or `message` is required. The
    /// daemon adds a `kybern://task/<id>` mention for each task at the start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// The full first message, as for `tasks.items.send`. Each task's mention is
    /// added at the start when the message has none for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<UserMessage>,
    /// Notes attached as `kybern://note/<id>` mentions to every run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note_ids: Option<Vec<NoteId>>,
}

/// A task that started a run, and the thread it runs in. A combined run lists
/// every task with the same thread.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskBatchStarted {
    pub task: TaskItem,
    pub thread_id: ThreadId,
}

/// A task that did not start, and why in words for the user ("Already running").
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskBatchSkipped {
    pub id: TaskItemId,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskItemsSendBatchResult {
    pub started: Vec<TaskBatchStarted>,
    pub skipped: Vec<TaskBatchSkipped>,
}
method!(
    TaskItemsSendBatch,
    "tasks.items.send_batch",
    Some(Scope::OrchestrationOperate),
    TaskItemsSendBatchParams,
    TaskItemsSendBatchResult
);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskItemsFollowupParams {
    pub id: TaskItemId,
    /// Plain text. Exactly one of `text` or `message` is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The full follow-up (text, mentions, skills, files, thread references,
    /// attachments), sent or queued as is. When no run can receive it, a message
    /// of text and references is saved as readable text for the next run; one
    /// with attachments is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<UserMessage>,
}
impl TaskItemsFollowupParams {
    /// A plain-text follow-up.
    pub fn text(id: TaskItemId, text: impl Into<String>) -> Self {
        Self { id, text: Some(text.into()), message: None }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskItemsFollowupResult {
    pub task: TaskItem,
    /// The run thread that received the follow-up, sent now or queued. Absent when it was saved for the next run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_to: Option<ThreadId>,
}
method!(TaskItemsFollowup, "tasks.items.followup", Some(Scope::OrchestrationOperate), TaskItemsFollowupParams, TaskItemsFollowupResult);

/// Params of the `tasks.items.changed` notification.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TaskItemsChangedNotification {
    /// The task after the change, including restores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskItem>,
    /// Set instead of `task` when a task was deleted or permanently removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_id: Option<TaskItemId>,
}

// ---- machine network (M1; payloads in network.rs) ----

method!(NetworkStatusMethod, "network.status", Some(Scope::NetworkRead), Empty, NetworkStatus);
method!(NetworkSetEnabled, "network.set_enabled", Some(Scope::NetworkAdmin), NetworkSetEnabledParams, NetworkStatus);
method!(NetworkAdd, "network.add", Some(Scope::NetworkAdmin), NetworkFoundParams, NetworkAddResult);
method!(NetworkCodeCreate, "network.code.create", Some(Scope::NetworkAdmin), Empty, NetworkCodeCreateResult);
method!(NetworkJoin, "network.join", Some(Scope::NetworkAdmin), NetworkJoinParams, MachineCard);
method!(NetworkRemove, "network.remove", Some(Scope::NetworkAdmin), NetworkRemoveParams, NetworkStatus);
method!(NetworkPause, "network.pause", Some(Scope::NetworkAdmin), NetworkPauseParams, MachineCard);
method!(NetworkRename, "network.rename", Some(Scope::NetworkAdmin), NetworkRenameParams, MachineCard);
method!(NetworkMerge, "network.merge", Some(Scope::NetworkAdmin), NetworkFoundParams, NetworkStatus);
method!(NetworkRejoin, "network.rejoin", Some(Scope::NetworkAdmin), Empty, NetworkStatus);
method!(NetworkClientGrant, "network.client_grant", Some(Scope::NetworkRead), NetworkClientGrantParams, NetworkClientGrantResult);
method!(
    NetworkRelayPageOpen,
    "network.relay_page.open",
    Some(Scope::OrchestrationRead),
    NetworkRelayPageOpenParams,
    NetworkRelayPageOpenResult
);
method!(NetworkRelayPageClose, "network.relay_page.close", Some(Scope::OrchestrationRead), NetworkRelayPageCloseParams, Empty);
method!(NetworkRoutes, "network.routes", Some(Scope::NetworkRead), NetworkRoutesParams, NetworkRoutesResult);
method!(NetworkAllowSet, "network.allow.set", Some(Scope::NetworkAdmin), NetworkAllowSetParams, MachineCard);
method!(NetworkUpdateAll, "network.update_all", Some(Scope::NetworkAdmin), Empty, NetworkUpdateAllResult);
method!(NetworkDoctor, "network.doctor", Some(Scope::NetworkRead), NetworkDoctorParams, DoctorReport);
method!(MachinesInfo, "machines.info", Some(Scope::NetworkRead), Empty, MachineInfo);
method!(NetworkFollowUpdatesSet, "network.follow_updates.set", Some(Scope::NetworkAdmin), NetworkFollowUpdatesParams, NetworkStatus);
method!(NetworkKeepReachableSet, "network.keep_reachable.set", Some(Scope::NetworkAdmin), NetworkKeepReachableParams, NetworkStatus);

// Daemon-to-daemon. A device token never carries a machine scope, so these refuse clients.
method!(PeerOpsSync, "peer.ops.sync", Some(Scope::MachinePeer), PeerOpsSyncParams, PeerOpsSyncResult);
method!(PeerOpsPush, "peer.ops.push", Some(Scope::MachinePeer), PeerOpsPushParams, PeerOpsPushResult);
method!(PeerPresenceMethod, "peer.presence", Some(Scope::MachinePeer), PeerPresence, Empty);
method!(PeerClientTokenMint, "peer.client_token.mint", Some(Scope::MachinePeer), PeerClientTokenMintParams, PeerClientTokenMintResult);
method!(PeerDeviceRevoke, "peer.device.revoke", Some(Scope::MachinePeer), PeerDeviceRevokeParams, Empty);
method!(PeerRelayOpen, "peer.relay.open", Some(Scope::MachineRelay), PeerRelayOpenParams, Empty);
method!(PeerRelayData, "peer.relay.data", Some(Scope::MachineRelay), PeerRelayDataParams, Empty);
method!(PeerRelayClose, "peer.relay.close", Some(Scope::MachineRelay), PeerRelayCloseParams, Empty);
// Asking a member to update itself. `machine:settings` is the narrowest machine scope that a machine's owner
// can turn off per machine and that is not the always-on operation sync: a machine that does not let others
// change its settings does not let them restart it into a new version either.
method!(PeerUpdateStart, "peer.update.start", Some(Scope::MachineSettings), PeerUpdateStartParams, PeerUpdateStartResult);

// Remote delegation and the outbox (M2, R1; payloads in outbox.rs).
method!(OutboxList, "outbox.list", Some(Scope::OrchestrationRead), OutboxListParams, OutboxListResult);
method!(OutboxRetry, "outbox.retry", Some(Scope::OrchestrationOperate), OutboxIdParams, OutboxItem);
method!(OutboxCancel, "outbox.cancel", Some(Scope::OrchestrationOperate), OutboxIdParams, OutboxItem);
method!(OutboxEnqueue, "outbox.enqueue", Some(Scope::OrchestrationOperate), OutboxEnqueueParams, OutboxItem);
method!(PeerDelegate, "peer.delegate", Some(Scope::MachineDelegate), PeerDelegateParams, PeerDelegateResult);
method!(PeerDelegationGet, "peer.delegation.get", Some(Scope::MachineDelegate), PeerDelegationGetParams, PeerDelegationView);
method!(PeerDelegationCancel, "peer.delegation.cancel", Some(Scope::MachineDelegate), PeerDelegationCancelParams, PeerDelegationView);
method!(PeerGitBundle, "peer.git.bundle", Some(Scope::MachineDelegate), PeerGitBundleParams, PeerGitBundleResult);
method!(PeerGitImport, "peer.git.import", Some(Scope::MachineDelegate), PeerGitImportParams, PeerGitImportResult);
// Outbox delivery of a composer's new thread and message: the same parameters as `threads.create` and
// `threads.send`, which already take a client-chosen `thread_id` and `message_id`, so a repeat is the first.
method!(PeerThreadCreate, "peer.thread.create", Some(Scope::MachineDelegate), ThreadsCreateParams, Thread);
method!(PeerThreadSend, "peer.thread.send", Some(Scope::MachineDelegate), ThreadsSendParams, ThreadsSendResult);
// Shared state (M5; payloads in shared_state.rs). `peer.settings.*` need `machine:settings` on the target.
method!(SettingsNetworkViewMethod, "settings.network_view", Some(Scope::OrchestrationRead), Empty, SettingsNetworkView);
method!(SettingsShare, "settings.share", Some(Scope::OrchestrationOperate), SettingsShareParams, SettingsShareResult);
method!(PeerSettingsGet, "peer.settings.get", Some(Scope::MachineSettings), Empty, PeerSettingsGetResult);
method!(PeerSettingsPatch, "peer.settings.patch", Some(Scope::MachineSettings), SettingsPatch, PeerSettingsPatchResult);
method!(PeerFederatedRead, "peer.federated.read", Some(Scope::MachineTools), PeerFederatedReadParams, PeerFederatedReadResult);
// The login broker and pushed keys (S4; payloads in login_broker.rs). The `machine:logins` calls are served only
// by the machine the `broker` register names (borrow, usage, release_all) or sent only by it (push); `machine:secrets`
// carries a key sealed to the target.
method!(PeerLoginBorrow, "peer.login.borrow", Some(Scope::MachineLogins), PeerLoginBorrowParams, PeerLoginBorrowResult);
method!(PeerLoginPush, "peer.login.push", Some(Scope::MachineLogins), PeerLoginPushParams, Empty);
method!(PeerLoginRelease, "peer.login.release", Some(Scope::MachineLogins), PeerLoginReleaseParams, Empty);
method!(PeerLoginStatusMethod, "peer.login.status", Some(Scope::MachineLogins), Empty, PeerLoginStatus);
method!(PeerLoginReleaseAll, "peer.login.release_all", Some(Scope::MachineLogins), PeerLoginReleaseAllParams, PeerLoginReleaseAllResult);
method!(PeerLoginUsage, "peer.login.usage", Some(Scope::MachineLogins), PeerLoginUsageParams, UsageLimitsResult);
method!(PeerSecretsPut, "peer.secrets.put", Some(Scope::MachineSecrets), PeerSecretsPutParams, Empty);
method!(LoginBrokerStatusMethod, "login.broker.status", Some(Scope::OrchestrationRead), Empty, LoginBrokerStatus);
method!(LoginBrokerBind, "login.broker.bind", Some(Scope::AccessWrite), LoginBrokerBindParams, LoginBrokerStatus);
method!(LoginBrokerSet, "login.broker.set", Some(Scope::NetworkAdmin), LoginBrokerSetParams, LoginBrokerSetResult);
method!(SecretsPush, "secrets.push", Some(Scope::AccessWrite), SecretsPushParams, SecretsPushResult);
// Forwarded agent tools (M2, R2; payloads in forward.rs). Files and terminals also need their own scope on the target.
// Fork to a machine (M3, F1; payloads in fork.rs).
method!(ThreadsForkNote, "threads.fork_note", Some(Scope::OrchestrationRead), ThreadsForkNoteParams, ThreadsForkNoteResult);
method!(
    ThreadsForkToMachine,
    "threads.fork_to_machine",
    Some(Scope::OrchestrationOperate),
    ThreadsForkToMachineParams,
    ThreadsForkToMachineResult
);
method!(PeerFork, "peer.fork", Some(Scope::MachineDelegate), PeerForkParams, PeerForkResult);
// Continue on another machine (M3, F2; payloads in move_thread.rs).
method!(
    ThreadsMoveToMachine,
    "threads.move_to_machine",
    Some(Scope::OrchestrationOperate),
    ThreadsMoveToMachineParams,
    ThreadsMoveToMachineResult
);
method!(ThreadsMoveAbandon, "threads.move_abandon", Some(Scope::OrchestrationOperate), ThreadsMoveAbandonParams, ThreadsMoveAbandonResult);
method!(PeerMovePrepare, "peer.move.prepare", Some(Scope::MachineDelegate), PeerMovePrepareParams, PeerMovePrepareResult);
method!(PeerMoveCommit, "peer.move.commit", Some(Scope::MachineDelegate), PeerMoveCommitParams, PeerMoveCommitResult);
method!(PeerMoveGet, "peer.move.get", Some(Scope::MachineDelegate), PeerMoveRefParams, PeerMoveStatus);
method!(PeerMoveAbort, "peer.move.abort", Some(Scope::MachineDelegate), PeerMoveRefParams, PeerMoveStatus);
method!(PeerToolCall, "peer.tool.call", Some(Scope::MachineTools), PeerToolCallParams, PeerToolCallResult);
method!(PeerThreadArchiveInbound, "peer.thread.archive_inbound", Some(Scope::MachineTools), PeerThreadArchiveInboundParams, Empty);

// Add a project to another machine (payloads in provision.rs). A client calls the `projects.provision*` methods on
// the source machine; the source talks to the target with `peer.provision.*`.
method!(ProjectsProvisionPlan, "projects.provision_plan", Some(Scope::OrchestrationRead), ProjectsProvisionPlanParams, ProvisionPlan);
method!(ProjectsProvision, "projects.provision", Some(Scope::OrchestrationOperate), ProjectsProvisionParams, ProvisionStatus);
method!(
    ProjectsProvisionStatusMethod,
    "projects.provision.status",
    Some(Scope::OrchestrationRead),
    ProjectsProvisionStatusParams,
    ProvisionStatus
);
method!(PeerProvisionProbeMethod, "peer.provision.probe", Some(Scope::MachineDelegate), PeerProvisionProbeParams, PeerProvisionProbe);
method!(
    PeerProvisionPrepare,
    "peer.provision.prepare",
    Some(Scope::MachineDelegate),
    PeerProvisionPrepareParams,
    PeerProvisionPrepareResult
);
// A chunk of the bundle of a repository the target could not clone (up to `MAX_PROVISION_BUNDLE_BYTES`).
method!(PeerProvisionChunk, "peer.provision.chunk", Some(Scope::MachineDelegate), PeerGitBundleParams, PeerGitBundleResult);
method!(PeerProvisionImport, "peer.provision.import", Some(Scope::MachineDelegate), PeerProvisionImportParams, PeerProvisionPrepareResult);
method!(PeerProvisionFinish, "peer.provision.finish", Some(Scope::MachineDelegate), PeerProvisionFinishParams, ProvisionResult);

// Machine utilities (M4, U1 files and U3 ports; payloads in utilities.rs).
method!(FilesSend, "files.send", Some(Scope::OrchestrationOperate), FilesSendParams, FilesSendResult);
method!(PeerFilesBegin, "peer.files.begin", Some(Scope::MachineFiles), PeerFilesBeginParams, PeerFilesBeginResult);
method!(PeerFilesChunk, "peer.files.chunk", Some(Scope::MachineFiles), PeerFilesChunkParams, PeerFilesChunkResult);
method!(PeerFilesFinish, "peer.files.finish", Some(Scope::MachineFiles), PeerFilesTransferParams, PeerFilesFinishResult);
method!(PeerFilesAbort, "peer.files.abort", Some(Scope::MachineFiles), PeerFilesTransferParams, Empty);
method!(PortsForward, "ports.forward", Some(Scope::OrchestrationOperate), PortsForwardParams, PortForward);
method!(PortsList, "ports.list", Some(Scope::OrchestrationRead), Empty, PortsListResult);
method!(PortsStop, "ports.stop", Some(Scope::OrchestrationOperate), PortsStopParams, PortsListResult);
method!(ClipboardSet, "clipboard.set", Some(Scope::OrchestrationOperate), ClipboardSetParams, ClipboardSetResult);
method!(ClipboardGet, "clipboard.get", Some(Scope::OrchestrationOperate), ClipboardGetParams, ClipboardGetResult);
method!(ClipboardStatusMethod, "clipboard.status", Some(Scope::OrchestrationRead), Empty, ClipboardStatus);
method!(PeerClipboardSet, "peer.clipboard.set", Some(Scope::MachineClipboard), PeerClipboardSetParams, ClipboardSetResult);
method!(PeerClipboardGet, "peer.clipboard.get", Some(Scope::MachineClipboard), Empty, ClipboardGetResult);
method!(PeerClipboardAuto, "peer.clipboard.auto", Some(Scope::MachineClipboardAuto), PeerClipboardAutoParams, Empty);
method!(PeerPortsPrepare, "peer.ports.prepare", Some(Scope::MachinePorts), PeerPortsPrepareParams, PeerPortsPrepareResult);
method!(PeerPortsRelease, "peer.ports.release", Some(Scope::MachinePeer), PeerPortsReleaseParams, Empty);
method!(PeerPortsOpen, "peer.ports.open", Some(Scope::MachinePorts), PeerPortsOpenParams, Empty);
method!(PeerPortsVia, "peer.ports.via", Some(Scope::MachinePorts), PeerPortsViaParams, PeerPortsViaResult);
method!(PeerPortsHop, "peer.ports.hop", Some(Scope::MachineRelay), PeerPortsHopParams, PeerPortsHopResult);
method!(PeerPortsHopCheck, "peer.ports.hop_check", Some(Scope::MachinePorts), PeerPortsHopCheckParams, Empty);
// Data and close frames only reach a stream that an authorized open created, so `machine:peer` is enough.
method!(PeerPortsData, "peer.ports.data", Some(Scope::MachinePeer), PeerRelayDataParams, Empty);
method!(PeerPortsClose, "peer.ports.close", Some(Scope::MachinePeer), PeerRelayCloseParams, Empty);

// User skills on every machine (payloads in skill_sync.rs). Reading another machine's skills and asking it to
// send one are file reads there, and the copy itself is a file send: `machine:files` gates all of it.
method!(PluginsInventory, "plugins.inventory", Some(Scope::OrchestrationRead), Empty, PluginInventory);
method!(PluginsNetworkViewMethod, "plugins.network_view", Some(Scope::OrchestrationRead), Empty, PluginsNetworkView);
method!(PluginsCopy, "plugins.copy", Some(Scope::OrchestrationOperate), PluginsCopyParams, PluginsCopyResult);
method!(PeerPluginsInventory, "peer.plugins.inventory", Some(Scope::MachineFiles), Empty, PluginInventory);
method!(PeerPluginsCopy, "peer.plugins.copy", Some(Scope::MachineFiles), PluginsCopyParams, PluginsCopyResult);
method!(PeerPluginsInstall, "peer.plugins.install", Some(Scope::MachineFiles), PluginInstallRequest, PluginInstallResult);

method!(SkillsInventoryMethod, "skills.inventory", Some(Scope::OrchestrationRead), Empty, SkillInventory);
method!(SkillsNetworkViewMethod, "skills.network_view", Some(Scope::OrchestrationRead), Empty, SkillsNetworkView);
method!(SkillsCopy, "skills.copy", Some(Scope::OrchestrationOperate), SkillsCopyParams, SkillsCopyResult);
method!(PeerSkillsInventory, "peer.skills.inventory", Some(Scope::MachineFiles), Empty, SkillInventory);
method!(PeerSkillsCopy, "peer.skills.copy", Some(Scope::MachineFiles), SkillsCopyParams, SkillsCopyResult);

/// Registry used by the daemon's auth check and the schema dump.
pub struct MethodInfo {
    pub name: &'static str,
    pub scope: Option<Scope>,
}

macro_rules! registry {
    ($($ty:ty),* $(,)?) => {
        pub const METHODS: &[MethodInfo] = &[
            $(MethodInfo { name: <$ty as Method>::NAME, scope: <$ty as Method>::SCOPE }),*
        ];
    };
}

registry!(
    DaemonInfoMethod,
    DaemonShutdown,
    DaemonActivityMethod,
    SessionsList,
    SessionsResume,
    SessionsScanProjects,
    SessionsScanGet,
    SessionsScanCancel,
    SessionsImportAll,
    SessionsImportGet,
    SessionsImportCancel,
    ProvidersList,
    AccountsCreate,
    AccountsSignIn,
    AccountsUsage,
    AccountsCatalog,
    AccountsList,
    AccountsLoginStart,
    AccountsLoginGet,
    RequestsProgress,
    RequestsAct,
    AccountsLoginInput,
    AccountsLoginCancel,
    AccountsLoginFinish,
    AccountsUpdate,
    AccountsSignOut,
    AccountsRemove,
    ThreadsTargetGet,
    ThreadsTargetSet,
    ThreadsPermissionsApply,
    ThreadsSwitchContinue,
    HarnessUpdatesList,
    HarnessUpdatesRun,
    DaemonUpdateStatusMethod,
    DaemonUpdateCheck,
    DaemonUpdateRun,
    AppUpdaterAttach,
    AppUpdaterReport,
    ProjectsList,
    ProjectsBrowse,
    ProjectsAdd,
    ProjectsUpdate,
    ProjectsRemove,
    ProjectsScanRepos,
    ProjectsMembersStatus,
    ProjectsMap,
    ProjectsInstructionsCreate,
    SpacesList,
    SpacesCreate,
    SpacesUpdate,
    SpacesDelete,
    SpacesReorder,
    SpacesPlace,
    PeerSpacesSync,
    PeerSpaceLogo,
    ThreadsList,
    ThreadsSearch,
    ThreadsRead,
    ThreadsCreate,
    ThreadsGet,
    ThreadsToolOutput,
    ThreadsRecoverOmpAnswer,
    ThreadsUpdate,
    ThreadsArchive,
    ThreadsSend,
    ThreadsSteer,
    SubagentsSend,
    SubagentsMessages,
    SubagentsSendToParent,
    ThreadNotesGet,
    ThreadNotesSet,
    NotesList,
    NotesGet,
    NotesCreate,
    NotesUpdate,
    NotesPin,
    NotesMove,
    NotesDelete,
    NotesRestore,
    NotesPurge,
    NotesSearch,
    NotesTidyOffer,
    NotesTidySnooze,
    NotesTidyStart,
    TaskItemsList,
    TaskItemsGet,
    TaskItemsCreate,
    TaskItemsUpdate,
    TaskItemsDelete,
    TaskItemsRestore,
    TaskItemsSend,
    TaskItemsSendBatch,
    TaskItemsFollowup,
    QueueAdd,
    QueueUpdate,
    QueueList,
    QueueRemove,
    ThreadsRelease,
    ThreadsCompact,
    ThreadsAnswer,
    ThreadsInterrupt,
    ThreadMessagesList,
    ThreadMessagesDeliver,
    ThreadMessagesDismiss,
    DelegationsWorktreeRemove,
    DelegationsRemoteList,
    CoordinatorGet,
    CoordinatorOpen,
    CoordinatorSetup,
    CoordinatorSwitchHarness,
    CoordinatorRefreshContext,
    CoordinatorEditHandoff,
    TasksList,
    TaskStop,
    TaskBackground,
    ThreadsRegenerateTitle,
    ThreadsCheckpoints,
    ThreadsDiff,
    ThreadsRevert,
    TerminalsCreate,
    TerminalsList,
    TerminalsInput,
    TerminalsResize,
    TerminalsClose,
    TerminalsSubscribe,
    TerminalsUnsubscribe,
    SettingsGet,
    SettingsUpdate,
    ComputerStatusGet,
    ComputerSetup,
    CursorStatusGet,
    CursorSetup,
    ComputerFrameGet,
    ComputerNotesList,
    ComputerNoteSet,
    SecretsTarget,
    SecretsStore,
    UsageSummary,
    UsageLimits,
    PairingCreate,
    ExposureGet,
    ExposureSet,
    TokensList,
    TokensRevoke,
    GitStatusMethod,
    GitBranches,
    GitCommit,
    PrCreate,
    PrCreateGroup,
    PrList,
    PrDetail,
    PrPage,
    PrAction,
    ActionsRuns,
    ActionsRun,
    ActionsLog,
    ActionsWorkflows,
    ActionsWorkflow,
    ActionsAction,
    ActionsDispatch,
    GithubStarred,
    GithubStar,
    WorktreeInspect,
    WorktreeRemove,
    FilesSearch,
    FilesList,
    FilesRead,
    FilesWrite,
    ThreadFileRead,
    ThreadFileWrite,
    ThreadFilesResolve,
    SkillsList,
    IntegrationsList,
    HtmlPublish,
    HtmlPreview,
    HtmlRead,
    HtmlFrame,
    HtmlRevoke,
    HtmlSnapshot,
    PreviewOpen,
    PreviewServerOpen,
    PreviewClose,
    PreviewProbe,
    PreviewServersList,
    ArtifactsList,
    ArtifactRead,
    ArtifactPreview,
    IntegrationChange,
    IntegrationLogin,
    ApprovalsRespond,
    ApprovalsList,
    EventsSubscribe,
    EventsUnsubscribe,
    EventsRange,
    NetworkStatusMethod,
    NetworkSetEnabled,
    NetworkAdd,
    NetworkCodeCreate,
    NetworkJoin,
    NetworkRemove,
    NetworkPause,
    NetworkRename,
    NetworkMerge,
    NetworkRejoin,
    NetworkClientGrant,
    NetworkRelayPageOpen,
    NetworkRelayPageClose,
    NetworkRoutes,
    NetworkAllowSet,
    NetworkUpdateAll,
    NetworkDoctor,
    MachinesInfo,
    NetworkFollowUpdatesSet,
    NetworkKeepReachableSet,
    PeerOpsSync,
    PeerOpsPush,
    PeerPresenceMethod,
    PeerClientTokenMint,
    PeerDeviceRevoke,
    PeerRelayOpen,
    PeerRelayData,
    PeerRelayClose,
    PeerUpdateStart,
    OutboxList,
    OutboxRetry,
    OutboxCancel,
    OutboxEnqueue,
    PeerDelegate,
    PeerDelegationGet,
    PeerDelegationCancel,
    PeerGitBundle,
    PeerGitImport,
    PeerThreadCreate,
    PeerThreadSend,
    ThreadsForkNote,
    ThreadsForkToMachine,
    PeerFork,
    ThreadsMoveToMachine,
    ThreadsMoveAbandon,
    PeerMovePrepare,
    PeerMoveCommit,
    PeerMoveGet,
    PeerMoveAbort,
    ProjectsProvisionPlan,
    ProjectsProvision,
    ProjectsProvisionStatusMethod,
    PeerProvisionProbeMethod,
    PeerProvisionPrepare,
    PeerProvisionChunk,
    PeerProvisionImport,
    PeerProvisionFinish,
    PeerToolCall,
    PeerThreadArchiveInbound,
    SettingsNetworkViewMethod,
    SettingsShare,
    PeerSettingsGet,
    PeerSettingsPatch,
    PeerFederatedRead,
    PeerLoginBorrow,
    PeerLoginPush,
    PeerLoginRelease,
    PeerLoginStatusMethod,
    PeerLoginReleaseAll,
    PeerLoginUsage,
    PeerSecretsPut,
    LoginBrokerStatusMethod,
    LoginBrokerBind,
    LoginBrokerSet,
    SecretsPush,
    FilesSend,
    PeerFilesBegin,
    PeerFilesChunk,
    PeerFilesFinish,
    PeerFilesAbort,
    PortsForward,
    PortsList,
    PortsStop,
    ClipboardSet,
    ClipboardGet,
    ClipboardStatusMethod,
    PeerClipboardSet,
    PeerClipboardGet,
    PeerClipboardAuto,
    PeerPortsPrepare,
    PeerPortsRelease,
    PeerPortsOpen,
    PeerPortsVia,
    PeerPortsHop,
    PeerPortsHopCheck,
    PeerPortsData,
    PeerPortsClose,
    PluginsInventory,
    PluginsNetworkViewMethod,
    PluginsCopy,
    PeerPluginsInventory,
    PeerPluginsCopy,
    PeerPluginsInstall,
    SkillsInventoryMethod,
    SkillsNetworkViewMethod,
    SkillsCopy,
    PeerSkillsInventory,
    PeerSkillsCopy,
);

pub fn scope_for(method: &str) -> Option<Option<Scope>> {
    METHODS.iter().find(|m| m.name == method).map(|m| m.scope)
}
