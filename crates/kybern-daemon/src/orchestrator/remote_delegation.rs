//! Remote `kybern_agent_delegate` (ADE-86 M2 / R1, design §6.2): run a delegated child on another
//! machine of the network.
//!
//! **The delegating machine (A).** `kybern_agent_delegate { machine }` writes a `remote_children` row
//! and an outbox item in one transaction *before anything is sent*. The outbox worker (`network::outbox`)
//! delivers the item as `peer.delegate` once the target is online; the target deduplicates on the
//! operation id namespaced by the origin, so a delivery that is repeated after a restart runs once. A
//! follower per unfinished child then long-polls `peer.delegation.get` (waits of at most 50 s) and, when
//! the child ends, hands its result to the parent through the same batching local children use. The
//! first terminal result wins. A stop sets `stop_requested` and is resent on every reconnect until the
//! target confirms it.
//!
//! **The target machine (B).** [`Orchestrator::peer_dispatch`] serves `peer.delegate`,
//! `peer.delegation.get`, `peer.delegation.cancel`, `peer.git.bundle` and `peer.git.import`. The child is
//! an ordinary delegated thread, stamped with an immutable [`Origin`] taken from the session (never from
//! the caller's payload) and capped to the permission mode of the parent that asked.
//!
//! The machine network is reached through the [`RemoteHub`] trait, which `network::outbox::NetworkHub`
//! implements, so this module has no dependency on sessions or membership.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, ensure};
use async_trait::async_trait;
use chrono::Utc;
use kybern_git::Repo;
use kybern_protocol::methods::{
    Method, PeerDelegate, PeerDelegationCancel, PeerDelegationGet, PeerFork, PeerGitBundle, PeerGitImport, PeerMoveAbort, PeerMoveCommit,
    PeerMoveGet, PeerMovePrepare, PeerThreadCreate, PeerThreadSend,
};
use kybern_protocol::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::delegation::{cap_error, child_title, delegation_brief, validate_owns};
use super::group_transfer::{SourceMember, needed_members, needs_bundle, validate_group};
use super::{LiveSession, Orchestrator};
use crate::network::git_transfer;

/// How long an async `kybern_agent_delegate { machine }` waits for the target's acknowledgement before it
/// answers "queued".
const ACK_GRACE: Duration = Duration::from_secs(3);
/// The longest `peer.delegation.get` wait, so a socket that died silently is noticed (§6.2).
const FOLLOW_WAIT_MS: u64 = 50_000;
/// Follower retry delays: 2 s doubling to 5 min.
const FOLLOW_BACKOFF_MIN: Duration = Duration::from_secs(2);
const FOLLOW_BACKOFF_MAX: Duration = Duration::from_secs(300);

// ---- the seam to the machine network ----

/// A member of the network as the delegation engine needs to see it.
#[derive(Debug, Clone)]
pub struct RemoteTarget {
    pub id: String,
    pub name: String,
    pub online: bool,
    pub paused: bool,
    /// The features it listed in its last presence, `None` until one arrived.
    pub features: Option<Vec<String>>,
}

/// A member of the network as `kybern_machines` shows it (design §6.3). Never carries an address or a key.
#[derive(Debug, Clone)]
pub struct MemberView {
    pub id: String,
    pub name: String,
    pub os: String,
    pub is_self: bool,
    /// `online`, `offline` or `paused`.
    pub status: &'static str,
    pub last_seen: Option<chrono::DateTime<Utc>>,
    pub version: Option<String>,
    pub features: Vec<String>,
    /// What the machine lets the others use (`tools`, `files`, `terminals`, `delegate`, …).
    pub capabilities: std::collections::BTreeMap<String, bool>,
    pub harnesses: Vec<HarnessSummary>,
}

/// Why a call to a machine did not produce an answer.
#[derive(Debug)]
pub enum CallFailure {
    /// No session, or it broke: try again when the machine returns.
    Offline(String),
    /// The machine is not (or no longer) a member.
    Gone(String),
    /// The machine answered with an error.
    Remote(RpcError),
}

impl std::fmt::Display for CallFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallFailure::Offline(reason) | CallFailure::Gone(reason) => f.write_str(reason),
            CallFailure::Remote(error) => f.write_str(&error.message),
        }
    }
}

impl std::error::Error for CallFailure {}

/// What remote delegation needs from the machine network.
#[async_trait]
pub trait RemoteHub: Send + Sync {
    fn self_id(&self) -> String;
    fn self_name(&self) -> String;
    /// A member by name or id (not this machine), or why there is none.
    fn resolve(&self, query: &str) -> Result<RemoteTarget, String>;
    /// A current member by id; `None` when it was removed or never joined.
    fn target(&self, id: &str) -> Option<RemoteTarget>;
    fn targets(&self) -> Vec<RemoteTarget>;
    /// Every current member, this machine included, for `kybern_machines`.
    fn members(&self) -> Vec<MemberView> {
        Vec::new()
    }
    /// Call a `peer.*` method on a member over the open session.
    async fn call(&self, target: &str, method: &str, params: Value) -> Result<Value, CallFailure>;
    /// A signal after a session opened or closed and after membership changed.
    fn changes(&self) -> broadcast::Receiver<()>;
    /// Wake the outbox worker.
    fn kick(&self);
    /// Resolves after the next [`RemoteHub::kick`].
    async fn kicked(&self);
    /// Cancelled when the daemon shuts down.
    fn shutdown(&self) -> CancellationToken;
    /// Forward a loopback port of a member to a loopback port here (M4 / U3) and say which port it is.
    async fn forward_port(&self, _target: &str, _remote_port: u16) -> Result<u16, String> {
        Err("This daemon cannot forward ports.".into())
    }
}

/// Orchestrator-side state of remote delegation.
pub(super) struct RemoteState {
    pub(super) hub: Option<Arc<dyn RemoteHub>>,
    /// The login broker (S4): borrowed logins for sessions of accounts bound to it.
    pub(super) login: Option<Arc<crate::network::broker::LoginBroker>>,
    /// Tasks that have a follower running.
    followers: Mutex<HashSet<Uuid>>,
    /// Serializes read-modify-write of `remote_children`.
    writes: Mutex<()>,
    /// Threads with a move attempt in flight (`move_thread.rs`).
    pub(super) moving: Mutex<HashSet<ThreadId>>,
    /// Every change to a `remote_children` row, for `delegations.remote_changed`.
    changed: broadcast::Sender<RemoteChild>,
}

impl Default for RemoteState {
    fn default() -> Self {
        Self {
            hub: None,
            login: None,
            followers: Mutex::default(),
            writes: Mutex::default(),
            moving: Mutex::default(),
            changed: broadcast::channel(256).0,
        }
    }
}

/// What one delivery attempt of an outbox item came to.
#[derive(Debug)]
pub enum Delivery {
    Delivered(Value),
    /// A transport problem: try again later.
    Retry(String),
    /// A permanent problem: stop and say why.
    Failed(String),
}

/// What `kybern_agent_delegate { machine }` asks for, after `delegation.rs` parsed the arguments.
pub(super) struct RemoteRequest {
    pub operation_id: Uuid,
    pub machine: String,
    pub project: Option<String>,
    pub task: String,
    pub title: Option<String>,
    pub role: DelegationRole,
    pub provider: Option<ProviderKind>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<PermissionMode>,
    pub workspace: DelegationWorkspace,
    pub owns: Vec<String>,
    pub wait: bool,
    pub timeout: Duration,
}

impl Orchestrator {
    pub fn with_remote_hub(mut self, hub: Arc<dyn RemoteHub>) -> Self {
        Arc::get_mut(&mut self.inner).expect("the remote hub must be installed before cloning the orchestrator").remote.hub = Some(hub);
        self
    }

    pub fn remote_hub(&self) -> Option<&Arc<dyn RemoteHub>> {
        self.inner.remote.hub.as_ref()
    }

    /// Changes to this machine's records of children on other machines, for forwarding to clients.
    pub fn subscribe_remote_children(&self) -> broadcast::Receiver<RemoteChild> {
        self.inner.remote.changed.subscribe()
    }

    /// `delegations.remote_list`: a thread's children on other machines, as this machine records them.
    pub fn remote_children_list(&self, parent_thread_id: ThreadId) -> Result<Vec<RemoteChild>> {
        self.inner.store.remote_children_of(parent_thread_id)
    }

    /// Tell clients a record changed. No receivers just means no client is connected.
    fn publish_remote_child(&self, child: &RemoteChild) {
        let _ = self.inner.remote.changed.send(child.clone());
    }

    fn hub(&self) -> Result<&Arc<dyn RemoteHub>> {
        self.remote_hub().ok_or_else(|| anyhow!("This daemon cannot reach other machines."))
    }

    // ---- the delegating machine ----

    /// `kybern_agent_delegate { machine }`.
    pub(super) async fn remote_delegate(
        &self,
        parent: &Thread,
        turn_id: TurnId,
        live: &Arc<LiveSession>,
        request: RemoteRequest,
    ) -> Result<Value> {
        let hub = self.hub()?.clone();
        if let Some(existing) = self.inner.store.remote_child_by_operation(request.operation_id)? {
            return self.remote_response(existing, parent.id, request.wait, request.timeout).await;
        }
        let task = request.task.trim().to_owned();
        ensure!(!task.is_empty(), "task is required: write the full brief for the agent");
        ensure!(
            !super::fork_machine::second_hop(parent),
            "Another machine started this thread, so it cannot hand work on to a third machine. Delegate on this machine instead."
        );
        ensure!(
            request.workspace == DelegationWorkspace::Shared || request.owns.is_empty(),
            "owns only applies to a shared workspace. A worktree agent has its own copy of the files."
        );
        validate_owns(&request.owns)?;
        let target = hub.resolve(&request.machine).map_err(|reason| anyhow!(reason))?;
        if let Some(features) = &target.features {
            ensure!(
                features.iter().any(|feature| feature == features::DELEGATE_REMOTE_V1),
                "{} needs a newer Kybern to run delegated agents. Update it from Settings → Machines.",
                target.name
            );
            ensure!(
                !super::group_git::is_group(parent) || features.iter().any(|feature| feature == features::GROUP_V1),
                "{} needs a newer Kybern to run agents for a group project. Update it from Settings → Machines.",
                target.name
            );
        }
        Self::ensure_turn_active(live, turn_id).await?;
        // A machine without the repository is offered the project first (a card, or Full access by itself); the
        // delegation then waits for it instead of failing there.
        let provision = self
            .provision_for_agent(
                parent,
                turn_id,
                live,
                &target,
                "delegate",
                None,
                None,
                super::derived_operation_id(request.operation_id, 0x9b),
            )
            .await?;

        let create_lock = self.delegation_create_lock(parent.id);
        let _creating = create_lock.lock().await;
        if let Some(existing) = self.inner.store.remote_child_by_operation(request.operation_id)? {
            drop(_creating);
            return self.remote_response(existing, parent.id, request.wait, request.timeout).await;
        }
        ensure!(!self.delegation_is_stopping(parent.id), "This thread is being stopped, so it cannot delegate more work. End your turn.");

        let (max_children, max_depth) = self.orchestration_limits(parent);
        let depth = parent.delegation.as_ref().map_or(0, |info| u32::from(info.depth)) + 1;
        ensure!(
            depth <= max_depth,
            "Delegation depth limit reached (max {max_depth}). Do this work yourself, or ask the user to raise \"Delegation depth\" in Settings."
        );
        let active = self.inner.store.delegation_active_count(parent.id)? + self.inner.store.remote_children_active_count(parent.id)?;
        ensure!(
            active < max_children,
            "You already have {active} delegated agents running (max {max_children}). End your turn to be woken with their results, or cancel one with kybern_agent_cancel."
        );

        // Harness, model and mode. The target checks that it has the harness; the mode can only narrow.
        let kind = request.provider.unwrap_or(parent.provider.kind);
        let same_kind = kind == parent.provider.kind;
        let asked_model = request.model.clone().filter(|model| !model.trim().is_empty());
        let model = asked_model.clone().or_else(|| same_kind.then(|| parent.model.clone()).flatten());
        let effort = request
            .effort
            .clone()
            .filter(|effort| !effort.trim().is_empty())
            .or_else(|| (same_kind && asked_model.is_none()).then(|| parent.effort.clone()).flatten());
        let permission_mode =
            super::delegation::resolve_child_permission(parent.permission_mode, parent.provider.kind, kind, request.permission_mode)?;
        super::antigravity_rules::require_full_access(kind, permission_mode)?;

        // The repository, so the target can find its checkout of it; the base for a worktree.
        let (repo_identity, base_commit, group) = if super::group_git::is_group(parent) {
            // Per member, as for a local group child: a clean checkout is its HEAD, a dirty one a snapshot of its changes.
            let (mut work, targets) = self.group_work(parent).await.map_err(|error| anyhow!(error.message))?;
            if request.workspace == DelegationWorkspace::Worktree {
                for (member, target) in work.members.iter_mut().zip(&targets) {
                    let repo = target.git();
                    let clean = repo.status_porcelain(&target.exclude).await.is_ok_and(|lines| lines.is_empty());
                    member.commit = Some(match repo.head().await {
                        Some(head) if clean => head,
                        _ => repo.snapshot_excluding("kybern delegation base", &target.exclude).await?,
                    });
                }
            }
            let primary = work.members.first().ok_or_else(|| anyhow!("This group thread has no repository to delegate in."))?;
            (primary.repo.clone(), primary.commit.clone(), Some(work))
        } else {
            let identity = git_transfer::identity_of(Path::new(&parent.cwd)).await;
            let base = match request.workspace {
                DelegationWorkspace::Shared => None,
                DelegationWorkspace::Worktree => Some(git_transfer::base_commit(&Repo::new(&parent.cwd)).await?),
            };
            (identity, base, None)
        };
        ensure!(
            !repo_identity.is_empty(),
            "Delegating to another machine needs a git repository, so Kybern can find the matching checkout there. Delegate on this machine."
        );

        let task_id = Uuid::now_v7();
        let origin = Origin {
            machine_id: hub.self_id(),
            machine_name: hub.self_name(),
            thread_id: Some(parent.id),
            operation_id: request.operation_id,
            kind: OriginKind::Delegation { task_id: task_id.to_string() },
            permission_cap: parent.permission_mode,
            hops: vec![hub.self_id()],
        };
        let params = PeerDelegateParams {
            operation_id: request.operation_id,
            task_id,
            origin: origin.clone(),
            parent_title: parent.title.clone(),
            task: task.clone(),
            title: request.title.clone(),
            role: request.role,
            provider: Some(kind),
            model: model.clone(),
            effort,
            permission_mode,
            workspace: request.workspace,
            owns: request.owns.clone(),
            repo: repo_identity,
            project: request.project.clone(),
            base_commit: base_commit.clone(),
            group,
        };
        let now = Utc::now();
        let title = child_title(request.title.as_deref(), &task);
        let child = RemoteChild {
            task_id,
            operation_id: request.operation_id,
            parent_thread_id: parent.id,
            target: target.id.clone(),
            remote_thread_id: None,
            state: RemoteChildState::Pending,
            stop_requested: false,
            last_seq: 0,
            title,
            provider: Some(kind),
            model,
            info: DelegationInfo {
                task_id,
                operation_id: request.operation_id,
                parent_thread_id: parent.id,
                depth: depth as u8,
                role: request.role,
                workspace: request.workspace,
                default_override: None,
                owns: request.owns,
                status: DelegationStatus::Running,
                result: None,
                error: None,
                files_touched: Vec::new(),
                conflicts: Vec::new(),
                base_commit,
                branch: None,
                head_commit: None,
                diffstat: None,
                worktree_state: None,
                started_at: now,
                completed_at: None,
                parent_notified: true,
            },
            completed_at_from_target: false,
            created_at: now,
            updated_at: now,
        };
        let item = OutboxItem {
            id: Uuid::now_v7(),
            operation_id: request.operation_id,
            target: target.id.clone(),
            kind: OutboxKind::Delegate,
            payload: serde_json::to_value(&params)?,
            origin: Some(origin),
            state: OutboxState::Queued,
            attempts: 0,
            next_attempt_at: None,
            last_error: None,
            result: None,
            created_at: now,
            expires_at: now + chrono::Duration::days(self.outbox_expiry_days()),
            updated_at: now,
        };
        // Both rows or neither, and before anything is sent: a crash from here on resends, never loses.
        let (child, _) = self.inner.store.remote_child_enqueue(&child, &item)?;
        drop(_creating);
        hub.kick();
        self.inner.delegation.changed.notify_waiters();
        self.publish_remote_child(&child);
        let mut response = self.remote_response(child, parent.id, request.wait, request.timeout).await?;
        if let (Some(note), Some(object)) = (provision.note(), response.as_object_mut()) {
            object.insert("provision".into(), note);
        }
        Ok(response)
    }

    pub(crate) fn outbox_expiry_days(&self) -> i64 {
        OUTBOX_EXPIRY_DAYS
    }

    async fn remote_response(&self, child: RemoteChild, caller: ThreadId, wait: bool, timeout: Duration) -> Result<Value> {
        ensure!(child.parent_thread_id == caller, "operation_id already belongs to another thread's delegation. Use a fresh request_key.");
        let task_id = child.task_id;
        let (child, timed_out) = if wait {
            let (_, mut children, timed_out, _) = self.wait_for_delegations(&[task_id], timeout).await?;
            (children.pop().unwrap_or(child), timed_out)
        } else {
            (self.wait_for_ack(task_id, ACK_GRACE).await?.unwrap_or(child), false)
        };
        if wait {
            self.mark_task_delivered(&[&child]);
        }
        let mut response = self.remote_view(&child, RESULT_CAP);
        let object = response.as_object_mut().expect("view is an object");
        if timed_out {
            object.insert("wait_timed_out".into(), json!(true));
            object.insert(
                "note".into(),
                json!("The agent is still running. End your turn: Kybern wakes you with its result when it finishes."),
            );
        } else if child.state == RemoteChildState::Pending {
            let name = self.machine_name(&child.target);
            object.insert(
                "note".into(),
                json!(format!(
                    "Waiting for {name}: the task is queued and starts when the machine is reachable. End your turn instead of polling; Kybern wakes you with the result."
                )),
            );
        } else if child.state == RemoteChildState::Running {
            object.insert(
                "note".into(),
                json!("The agent is working. End your turn instead of polling; Kybern wakes you with its result when it finishes."),
            );
        }
        Ok(response)
    }

    /// Wait until the target acknowledged the child (or it ended), at most `budget`.
    async fn wait_for_ack(&self, task_id: Uuid, budget: Duration) -> Result<Option<RemoteChild>> {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let notified = self.inner.delegation.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let child = self.inner.store.remote_child_get(task_id)?;
            if child.as_ref().is_none_or(|child| child.remote_thread_id.is_some() || child.state.is_terminal()) {
                return Ok(child);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.inner.store.remote_child_get(task_id);
            }
        }
    }

    pub(super) fn machine_name(&self, id: &str) -> String {
        self.remote_hub().and_then(|hub| hub.target(id)).map(|target| target.name).unwrap_or_else(|| "the other machine".to_string())
    }

    /// The JSON the delegation tools return for a remote child (the shape of local children's, plus `machine`).
    pub(super) fn remote_view(&self, child: &RemoteChild, result_cap: usize) -> Value {
        let info = &child.info;
        let end = info.completed_at.unwrap_or_else(Utc::now);
        let mut view = json!({
            "task_id": child.task_id,
            "thread_id": child.remote_thread_id,
            "title": child.title,
            "provider": child.provider,
            "model": child.model,
            "role": info.role,
            "workspace": info.workspace,
            "status": info.status,
            "machine": { "id": child.target, "name": self.machine_name(&child.target) },
            "remote_state": child.state,
            "elapsed_ms": (end - info.started_at).num_milliseconds().max(0),
            "started_at": info.started_at,
        });
        let object = view.as_object_mut().expect("view is an object");
        if let Some(result) = &info.result {
            object.insert("result".into(), json!(super::delegation::truncate_with_marker(result, result_cap)));
        }
        if let Some(error) = &info.error {
            object.insert("error".into(), json!(error));
        }
        if let Some(at) = info.completed_at {
            object.insert("completed_at".into(), json!(at));
        }
        if !info.files_touched.is_empty() {
            object.insert("files_touched".into(), json!(info.files_touched.iter().take(50).collect::<Vec<_>>()));
        }
        for (key, value) in [("branch", &info.branch), ("head_commit", &info.head_commit)] {
            if let Some(value) = value {
                object.insert(key.into(), json!(value));
            }
        }
        if let Some(diffstat) = info.diffstat {
            object.insert("diffstat".into(), json!(diffstat));
        }
        if child.stop_requested {
            object.insert("stop_pending".into(), json!(true));
        }
        view
    }

    /// Split task ids into the remote children among them and the rest.
    pub(super) fn remote_partition(&self, task_ids: &[Uuid]) -> Result<(Vec<RemoteChild>, Vec<Uuid>)> {
        let mut remote = Vec::new();
        let mut local = Vec::new();
        for task_id in task_ids {
            match self.inner.store.remote_child_get(*task_id)? {
                Some(child) => remote.push(child),
                None => local.push(*task_id),
            }
        }
        Ok((remote, local))
    }

    /// A caller's remote children, newest first.
    pub(super) fn remote_children_of_caller(&self, caller: &Thread) -> Result<Vec<RemoteChild>> {
        let mut children = self.inner.store.remote_children_of(caller.id)?;
        children.reverse();
        Ok(children)
    }

    /// Check that `caller` may see/stop `child` and return it.
    pub(super) fn remote_owned_by(&self, caller: &Thread, child: RemoteChild) -> Result<RemoteChild> {
        ensure!(
            child.parent_thread_id == caller.id || self.is_remote_descendant(caller.id, child.parent_thread_id),
            "You can only stop agents you delegated work to. Call kybern_agent_status to list them."
        );
        Ok(child)
    }

    fn is_remote_descendant(&self, ancestor: ThreadId, thread_id: ThreadId) -> bool {
        self.inner
            .store
            .thread_get(thread_id)
            .ok()
            .flatten()
            .is_some_and(|thread| self.is_descendant_of(ancestor, &thread).unwrap_or(false))
    }

    // ---- changing a remote child ----

    /// Change a remote child under the write lock. `change` returns whether it modified anything.
    pub(super) fn remote_update(&self, task_id: Uuid, change: impl FnOnce(&mut RemoteChild) -> bool) -> Result<Option<RemoteChild>> {
        let updated = {
            let _writes = self.inner.remote.writes.lock().unwrap_or_else(|error| error.into_inner());
            let Some(mut child) = self.inner.store.remote_child_get(task_id)? else { return Ok(None) };
            if !change(&mut child) {
                return Ok(None);
            }
            child.updated_at = Utc::now();
            self.inner.store.remote_child_update(&child)?;
            child
        };
        self.inner.delegation.changed.notify_waiters();
        self.publish_remote_child(&updated);
        Ok(Some(updated))
    }

    /// An outcome as an `AgentResultItem` for the parent.
    fn remote_result_item(&self, child: &RemoteChild) -> AgentResultItem {
        let info = &child.info;
        AgentResultItem {
            task_id: child.task_id,
            thread_id: child.remote_thread_id.unwrap_or(child.task_id),
            title: format!("{} (on {})", child.title, self.machine_name(&child.target)),
            provider: child.provider.unwrap_or(ProviderKind::ClaudeCode),
            model: child.model.clone(),
            role: info.role,
            status: info.status,
            result: info.result.clone(),
            error: info.error.clone().map(cap_error),
            workspace: info.workspace,
            branch: info.branch.clone(),
            head_commit: info.head_commit.clone(),
            base_commit: info.base_commit.clone(),
            diffstat: info.diffstat,
            files_touched: info.files_touched.iter().take(50).cloned().collect(),
            conflicts: Vec::new(),
        }
    }

    /// End a remote child that has not ended yet, once: the first terminal result wins. The parent is
    /// told unless `notify_parent` is false.
    fn remote_settle(&self, task_id: Uuid, notify_parent: bool, change: impl FnOnce(&mut RemoteChild)) -> Result<Option<RemoteChild>> {
        let updated = self.remote_update(task_id, |child| {
            if child.state.is_terminal() {
                return false;
            }
            change(child);
            child.info.completed_at.get_or_insert_with(Utc::now);
            child.info.parent_notified = !notify_parent;
            true
        })?;
        if let Some(child) = &updated
            && notify_parent
        {
            let item = self.remote_result_item(child);
            self.delegation_push_result(child.parent_thread_id, item);
        }
        Ok(updated)
    }

    /// Stop a remote child: it counts as cancelled at once, and the target is told on every connection
    /// until it confirms.
    pub(super) fn remote_request_stop(&self, task_id: Uuid, notify_parent: bool) -> Result<Option<RemoteChild>> {
        // The target only needs telling when it may have the child: a delivery in flight or done.
        let stop = self.target_may_have(task_id);
        self.cancel_unsent_outbox(task_id);
        let updated = self.remote_settle(task_id, notify_parent, |child| {
            ended(child, RemoteChildState::Cancelled);
            child.stop_requested = stop;
        })?;
        if updated.is_some() {
            self.remote_follow(task_id);
        }
        Ok(updated)
    }

    /// Whether the target may already have this child: it acknowledged, or a delivery is in flight.
    fn target_may_have(&self, task_id: Uuid) -> bool {
        let Ok(Some(child)) = self.inner.store.remote_child_get(task_id) else { return false };
        if child.remote_thread_id.is_some() {
            return true;
        }
        matches!(
            self.inner.store.outbox_get_by_operation(child.operation_id),
            Ok(Some(item)) if matches!(item.state, OutboxState::Sending | OutboxState::Delivered)
        )
    }

    /// Cancel the queued delivery of a child that was never sent. Returns whether nothing was sent.
    fn cancel_unsent_outbox(&self, task_id: Uuid) -> bool {
        let Ok(Some(child)) = self.inner.store.remote_child_get(task_id) else { return false };
        let Ok(Some(mut item)) = self.inner.store.outbox_get_by_operation(child.operation_id) else { return false };
        if item.state != OutboxState::Queued {
            return false;
        }
        item.state = OutboxState::Cancelled;
        item.updated_at = Utc::now();
        self.inner.store.outbox_update(&item).is_ok_and(|found| found)
    }

    /// Stop every remote child below `root` (a stopped or archived thread).
    pub(super) fn remote_stop_tree(&self, root: ThreadId, notify_parent: bool) {
        let mut parents = vec![root];
        if let Ok(descendants) = self.delegation_descendants(root) {
            parents.extend(descendants.into_iter().map(|thread| thread.id));
        }
        for parent in parents {
            let Ok(children) = self.inner.store.remote_children_of(parent) else { continue };
            for child in children.into_iter().filter(|child| !child.state.is_terminal()) {
                if let Err(error) = self.remote_request_stop(child.task_id, notify_parent) {
                    tracing::warn!(task_id = %child.task_id, %error, "could not stop a remote agent");
                }
            }
        }
    }

    /// `kybern_agent_cancel` on a remote child.
    pub(super) async fn remote_cancel(&self, caller: &Thread, turn_id: TurnId, live: &LiveSession, child: RemoteChild) -> Result<Value> {
        let child = self.remote_owned_by(caller, child)?;
        Self::ensure_turn_active(live, turn_id).await?;
        tracing::info!(caller = %caller.id, task_id = %child.task_id, "an agent stopped a remote agent it delegated work to");
        self.remote_request_stop(child.task_id, false)?;
        let child = self.inner.store.remote_child_get(child.task_id)?.unwrap_or(child);
        Ok(self.remote_view(&child, 4 * 1024))
    }

    /// Mark a remote child's outcome as delivered to its parent.
    pub(super) fn remote_mark_notified(&self, task_id: Uuid) {
        let _ = self.remote_update(task_id, |child| {
            if child.info.parent_notified || !child.state.is_terminal() {
                return false;
            }
            child.info.parent_notified = true;
            true
        });
    }

    // ---- the outbox worker's side ----

    /// Deliver a queued item that is one plain request to the target (`peer.thread.create`,
    /// `peer.thread.send`): the request is idempotent by the ids it carries.
    pub async fn remote_deliver_call(&self, item: &OutboxItem, method: &str) -> Delivery {
        let hub = match self.hub() {
            Ok(hub) => hub.clone(),
            Err(error) => return Delivery::Failed(error.to_string()),
        };
        match hub.call(&item.target, method, item.payload.clone()).await {
            Ok(value) => Delivery::Delivered(value),
            Err(CallFailure::Offline(reason)) => Delivery::Retry(reason),
            Err(CallFailure::Gone(reason)) => Delivery::Failed(reason),
            Err(CallFailure::Remote(error)) => remote_error_delivery(&error),
        }
    }

    /// Deliver a queued `delegate` item: `peer.delegate`, sending the base commit first if the target asks.
    pub async fn remote_deliver_delegate(&self, item: &OutboxItem) -> Delivery {
        let hub = match self.hub() {
            Ok(hub) => hub.clone(),
            Err(error) => return Delivery::Failed(error.to_string()),
        };
        let params: PeerDelegateParams = match serde_json::from_value(item.payload.clone()) {
            Ok(params) => params,
            Err(error) => return Delivery::Failed(format!("the queued delegation is unreadable: {error}")),
        };
        match self.inner.store.remote_child_by_operation(item.operation_id) {
            Ok(Some(child)) if child.state.is_terminal() => {
                return Delivery::Failed("The delegation was stopped before it was delivered.".into());
            }
            Ok(Some(_)) => {}
            _ => return Delivery::Failed("The delegation no longer exists on this machine.".into()),
        }
        let value = serde_json::to_value(&params).unwrap_or(Value::Null);
        let mut answer = hub.call(&item.target, PeerDelegate::NAME, value.clone()).await;
        // The target has the repository but not the commit the worktree starts from: send it, once.
        if let Err(CallFailure::Remote(error)) = &answer
            && error.data.as_ref().and_then(|data| data.get("needs")).and_then(Value::as_str) == Some(NEEDS_BUNDLE)
        {
            let only = needed_members(error);
            match self.push_base(&*hub, item, &params, only).await {
                Ok(()) => answer = hub.call(&item.target, PeerDelegate::NAME, value).await,
                Err(delivery) => return delivery,
            }
        }
        match answer {
            Ok(value) => match serde_json::from_value::<PeerDelegateResult>(value.clone()) {
                Ok(_) => Delivery::Delivered(value),
                Err(error) => Delivery::Failed(format!("the machine answered with something unreadable: {error}")),
            },
            Err(CallFailure::Offline(reason)) => Delivery::Retry(reason),
            Err(CallFailure::Gone(reason)) => Delivery::Failed(reason),
            Err(CallFailure::Remote(error)) => self.remote_refusal(item, &error),
        }
    }

    /// Send the commit a worktree child starts from as a bundle and have the target import it.
    async fn push_base(
        &self,
        hub: &dyn RemoteHub,
        item: &OutboxItem,
        params: &PeerDelegateParams,
        only: Option<Vec<String>>,
    ) -> Result<(), Delivery> {
        let Some(commit) = params.base_commit.clone() else {
            return Err(Delivery::Failed("The target asked for a base commit that this delegation does not have.".into()));
        };
        let parent_cwd = match self.inner.store.remote_child_by_operation(item.operation_id) {
            Ok(Some(child)) => self.inner.store.thread_get(child.parent_thread_id).ok().flatten().map(|thread| thread.cwd),
            _ => None,
        };
        let Some(parent_cwd) = parent_cwd else {
            return Err(Delivery::Failed("The delegating thread no longer exists, so its changes cannot be sent.".into()));
        };
        if let Some(group) = &params.group {
            // Each member's base is a bundle of its own, from the member's checkout in the parent.
            let parent = match self.inner.store.remote_child_by_operation(item.operation_id) {
                Ok(Some(child)) => self.inner.store.thread_get(child.parent_thread_id).ok().flatten(),
                _ => None,
            };
            let Some(parent) = parent else {
                return Err(Delivery::Failed("The delegating thread no longer exists, so its changes cannot be sent.".into()));
            };
            let sources: Vec<SourceMember> = super::group_git::targets(&parent)
                .into_iter()
                .map(|target| SourceMember { path: target.repo_path().to_string(), dir: target.dir.to_string_lossy().into_owned() })
                .collect();
            return self.send_group_bundles(hub, item, group, params.project.as_deref(), &sources, only.as_deref()).await;
        }
        let repo = Repo::new(&parent_cwd);
        let staged = git_transfer::staged_path(&self.inner.paths.root, item.operation_id);
        let mut full = false;
        loop {
            let bundle = match git_transfer::create_bundle(&repo, &commit, item.operation_id, &staged, full).await {
                Ok(Some(bundle)) => bundle,
                // A remote already has everything yet the target lacks it: send the whole history.
                Ok(None) if !full => {
                    full = true;
                    continue;
                }
                Ok(None) => return Err(Delivery::Failed("There was nothing to send, yet the target lacks the commit.".into())),
                Err(error) => return Err(Delivery::Failed(format!("{error:#}"))),
            };
            let result =
                self.stream_bundle(hub, item, item.operation_id, &[], &params.repo, params.project.as_deref(), &commit, &bundle).await;
            let _ = std::fs::remove_file(&staged);
            match result {
                Ok(()) => return Ok(()),
                Err(CallFailure::Remote(error))
                    if !full && error.data.as_ref().and_then(|data| data.get("needs")).and_then(Value::as_str) == Some("full_bundle") =>
                {
                    full = true;
                }
                Err(CallFailure::Offline(reason)) => return Err(Delivery::Retry(reason)),
                Err(CallFailure::Gone(reason)) => return Err(Delivery::Failed(reason)),
                Err(CallFailure::Remote(error)) => return Err(self.remote_refusal(item, &error)),
            }
        }
    }

    /// Stream `bundle` as transfer `transfer_id` (the operation's own id, or a group member's) and import it.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn stream_bundle(
        &self,
        hub: &dyn RemoteHub,
        item: &OutboxItem,
        transfer_id: Uuid,
        group_repos: &[RepoIdentity],
        repo: &RepoIdentity,
        project: Option<&str>,
        commit: &str,
        bundle: &git_transfer::BundleFile,
    ) -> Result<(), CallFailure> {
        use base64::Engine;
        let chunk = |offset: u64, data: &[u8]| PeerGitBundleParams {
            operation_id: transfer_id,
            offset,
            total: bundle.size,
            data: base64::engine::general_purpose::STANDARD.encode(data),
        };
        // An empty chunk at offset 0 starts the transfer, or tells us how much an earlier try delivered.
        let started = hub.call(&item.target, PeerGitBundle::NAME, serde_json::to_value(chunk(0, &[])).unwrap_or(Value::Null)).await?;
        let mut offset = serde_json::from_value::<PeerGitBundleResult>(started).map(|result| result.received).unwrap_or(0).min(bundle.size);
        while offset < bundle.size {
            let data = git_transfer::read_chunk(&bundle.path, offset).map_err(|error| CallFailure::Gone(format!("{error:#}")))?;
            if data.is_empty() {
                break;
            }
            let answer =
                hub.call(&item.target, PeerGitBundle::NAME, serde_json::to_value(chunk(offset, &data)).unwrap_or(Value::Null)).await?;
            offset =
                serde_json::from_value::<PeerGitBundleResult>(answer).map(|result| result.received).unwrap_or(offset + data.len() as u64);
        }
        let import = PeerGitImportParams {
            operation_id: transfer_id,
            total: bundle.size,
            sha256: bundle.sha256.clone(),
            commit: commit.to_string(),
            repo: repo.clone(),
            project: project.map(str::to_string),
            group_repos: group_repos.to_vec(),
        };
        hub.call(&item.target, PeerGitImport::NAME, serde_json::to_value(import).unwrap_or(Value::Null)).await?;
        Ok(())
    }

    /// The outbox item reached a final state (or was queued again): bring the remote child in line.
    pub fn remote_outbox_changed(&self, item: &OutboxItem) {
        if item.kind == OutboxKind::Fork {
            self.fork_outbox_changed(item);
            return;
        }
        if item.kind == OutboxKind::Move {
            self.move_outbox_changed(item);
            return;
        }
        if item.kind != OutboxKind::Delegate {
            return;
        }
        let Ok(Some(child)) = self.inner.store.remote_child_by_operation(item.operation_id) else { return };
        let name = self.machine_name(&item.target);
        let result = match item.state {
            OutboxState::Delivered => {
                let delivered = item.result.clone().and_then(|value| serde_json::from_value::<PeerDelegateResult>(value).ok());
                if let Some(delivered) = delivered {
                    let _ = self.remote_update(child.task_id, |child| {
                        if child.remote_thread_id.is_some() {
                            return false;
                        }
                        child.remote_thread_id = Some(delivered.thread_id);
                        child.info.branch = delivered.branch.clone();
                        if !child.state.is_terminal() {
                            child.state = RemoteChildState::Running;
                        }
                        true
                    });
                    self.remote_follow(child.task_id);
                }
                return;
            }
            OutboxState::Failed => {
                let reason = item.last_error.clone().unwrap_or_else(|| "delivery failed".into());
                let removed = self.remote_hub().is_some_and(|hub| hub.target(&item.target).is_none());
                self.remote_settle(child.task_id, true, |child| {
                    if removed {
                        ended(child, RemoteChildState::Unreachable);
                        child.info.error =
                            Some("That machine was removed from your network, so the agent could not be started there.".into());
                    } else {
                        ended(child, RemoteChildState::Failed);
                        child.info.error = Some(cap_error(format!("{name} could not start the agent: {reason}")));
                    }
                })
            }
            OutboxState::Expired => self.remote_settle(child.task_id, true, |child| {
                ended(child, RemoteChildState::Unreachable);
                child.info.error = Some(format!(
                    "{name} did not come back within {} days, so the task was not delivered. Retry it from Settings → Machines.",
                    OUTBOX_EXPIRY_DAYS
                ));
            }),
            OutboxState::Cancelled => {
                let _ = self.cancel_unsent_outbox(child.task_id);
                let may_have = self.target_may_have(child.task_id);
                self.remote_settle(child.task_id, true, |child| {
                    ended(child, RemoteChildState::Cancelled);
                    child.stop_requested = may_have;
                })
            }
            OutboxState::Queued => {
                // Retry: the child waits for its target again.
                self.remote_update(child.task_id, |child| {
                    if !matches!(child.state, RemoteChildState::Failed | RemoteChildState::Unreachable) || child.remote_thread_id.is_some()
                    {
                        return false;
                    }
                    child.state = RemoteChildState::Pending;
                    child.info.status = DelegationStatus::Running;
                    child.info.error = None;
                    child.info.completed_at = None;
                    child.completed_at_from_target = false;
                    child.info.parent_notified = true;
                    true
                })
            }
            OutboxState::Sending => return,
        };
        if let Err(error) = result {
            tracing::warn!(task_id = %child.task_id, %error, "could not update a remote agent after its delivery changed");
        }
    }

    // ---- the follower ----

    /// Restart-time work: re-send outcomes that never reached their parent and follow unfinished children.
    pub fn remote_start(&self) {
        if self.remote_hub().is_none() {
            return;
        }
        match self.inner.store.remote_children_unnotified() {
            Ok(children) => {
                for child in children {
                    let item = self.remote_result_item(&child);
                    self.delegation_push_result(child.parent_thread_id, item);
                }
            }
            Err(error) => tracing::warn!(%error, "could not look for remote agent outcomes that were not delivered"),
        }
        match self.inner.store.remote_children_active() {
            Ok(children) => {
                for child in children {
                    self.remote_follow(child.task_id);
                }
            }
            Err(error) => tracing::warn!(%error, "could not resume following remote agents"),
        }
    }

    /// Start the follower of a remote child unless one is running.
    pub(super) fn remote_follow(&self, task_id: Uuid) {
        let Some(hub) = self.remote_hub().cloned() else { return };
        if !self.inner.remote.followers.lock().unwrap_or_else(|error| error.into_inner()).insert(task_id) {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            this.follow(hub, task_id).await;
            this.inner.remote.followers.lock().unwrap_or_else(|error| error.into_inner()).remove(&task_id);
        });
    }

    async fn follow(&self, hub: Arc<dyn RemoteHub>, task_id: Uuid) {
        let shutdown = hub.shutdown();
        let mut changes = hub.changes();
        let mut failures = 0u32;
        loop {
            if shutdown.is_cancelled() {
                return;
            }
            let child = match self.inner.store.remote_child_get(task_id) {
                Ok(Some(child)) => child,
                _ => return,
            };
            if child.state.is_terminal() && !child.stop_requested {
                return;
            }
            let name = hub.target(&child.target).map(|target| target.name);
            let Some(name) = name else {
                self.remote_unreachable(&child, "was removed from your network");
                return;
            };
            if child.remote_thread_id.is_none() {
                // Not acknowledged yet: the outbox worker delivers it; wait for that.
                self.remote_idle(&mut changes, &shutdown, Duration::from_secs(30)).await;
                continue;
            }
            let outcome =
                if child.stop_requested { self.follow_cancel(&*hub, &child).await } else { self.follow_poll(&*hub, &child).await };
            match outcome {
                Follow::Continue => failures = 0,
                Follow::Done => return,
                Follow::Unreachable(reason) => {
                    self.remote_unreachable(&child, &reason);
                    return;
                }
                Follow::Retry(reason) => {
                    failures = failures.saturating_add(1);
                    tracing::debug!(task_id = %task_id, machine = %name, %reason, "waiting to follow a remote agent again");
                    let wait = FOLLOW_BACKOFF_MIN.saturating_mul(1u32 << failures.saturating_sub(1).min(8)).min(FOLLOW_BACKOFF_MAX);
                    // A reconnect cuts the wait short.
                    self.remote_idle(&mut changes, &shutdown, wait).await;
                }
            }
        }
    }

    async fn remote_idle(&self, changes: &mut broadcast::Receiver<()>, shutdown: &CancellationToken, max: Duration) {
        let notified = self.inner.delegation.changed.notified();
        tokio::select! {
            _ = shutdown.cancelled() => {}
            _ = notified => {}
            result = changes.recv() => {
                if matches!(result, Err(broadcast::error::RecvError::Closed)) {
                    tokio::time::sleep(max.min(Duration::from_secs(1))).await;
                }
            }
            _ = tokio::time::sleep(max) => {}
        }
    }

    async fn follow_poll(&self, hub: &dyn RemoteHub, child: &RemoteChild) -> Follow {
        let params = PeerDelegationGetParams { task_id: child.task_id, since_seq: Some(child.last_seq), wait_ms: Some(FOLLOW_WAIT_MS) };
        match hub.call(&child.target, PeerDelegationGet::NAME, serde_json::to_value(params).unwrap_or(Value::Null)).await {
            Ok(value) => match serde_json::from_value::<PeerDelegationView>(value) {
                Ok(view) => self.apply_view(child, view),
                Err(error) => Follow::Retry(format!("unreadable answer: {error}")),
            },
            Err(CallFailure::Offline(reason)) => Follow::Retry(reason),
            Err(CallFailure::Gone(reason)) => Follow::Unreachable(reason),
            Err(CallFailure::Remote(error)) if error.code == codes::NOT_FOUND => Follow::Unreachable("no longer has this agent".into()),
            Err(CallFailure::Remote(error)) => Follow::Retry(error.message),
        }
    }

    async fn follow_cancel(&self, hub: &dyn RemoteHub, child: &RemoteChild) -> Follow {
        let params = PeerDelegationCancelParams { task_id: child.task_id };
        match hub.call(&child.target, PeerDelegationCancel::NAME, serde_json::to_value(params).unwrap_or(Value::Null)).await {
            Ok(_) => {
                let _ = self.remote_update(child.task_id, |child| {
                    let was = child.stop_requested;
                    child.stop_requested = false;
                    was
                });
                Follow::Continue
            }
            Err(CallFailure::Offline(reason)) => Follow::Retry(reason),
            // The target no longer has it, so there is nothing left to stop.
            Err(CallFailure::Gone(_)) => {
                let _ = self.remote_update(child.task_id, |child| std::mem::replace(&mut child.stop_requested, false));
                Follow::Done
            }
            Err(CallFailure::Remote(error)) if error.code == codes::NOT_FOUND => {
                let _ = self.remote_update(child.task_id, |child| std::mem::replace(&mut child.stop_requested, false));
                Follow::Done
            }
            Err(CallFailure::Remote(error)) => Follow::Retry(error.message),
        }
    }

    /// Fold the target's view into the mirror; a terminal view ends the child (first result wins).
    fn apply_view(&self, child: &RemoteChild, view: PeerDelegationView) -> Follow {
        let terminal = view.status != DelegationStatus::Running;
        let apply = |child: &mut RemoteChild| {
            child.remote_thread_id = Some(view.thread_id);
            child.last_seq = child.last_seq.max(view.last_seq);
            let info = &mut child.info;
            info.result = view.result.clone();
            info.error = view.error.clone();
            info.files_touched = view.files_touched.clone();
            info.base_commit = view.base_commit.clone().or(info.base_commit.take());
            info.branch = view.branch.clone();
            info.head_commit = view.head_commit.clone();
            info.diffstat = view.diffstat;
        };
        if !terminal {
            let _ = self.remote_update(child.task_id, |child| {
                if child.state.is_terminal() {
                    return false;
                }
                apply(child);
                child.state = RemoteChildState::Running;
                true
            });
            return Follow::Continue;
        }
        let status = view.status;
        let settled = self.remote_settle(child.task_id, true, |child| {
            apply(child);
            let state = match status {
                DelegationStatus::Completed => RemoteChildState::Completed,
                DelegationStatus::Cancelled => RemoteChildState::Cancelled,
                DelegationStatus::Failed | DelegationStatus::Interrupted | DelegationStatus::Running => RemoteChildState::Failed,
            };
            ended(child, state);
            // Keep the target's own word ("interrupted by a restart") rather than the generic status.
            child.info.status = status;
            child.info.completed_at = view.completed_at.or_else(|| Some(Utc::now()));
            child.completed_at_from_target = view.completed_at.is_some();
        });
        if let Err(error) = settled {
            tracing::warn!(task_id = %child.task_id, %error, "could not record a remote agent's result");
            return Follow::Retry("could not save the result".into());
        }
        // A stop that is still unconfirmed keeps the follower; otherwise this child is finished.
        match self.inner.store.remote_child_get(child.task_id) {
            Ok(Some(current)) if current.stop_requested => Follow::Continue,
            _ => Follow::Done,
        }
    }

    fn remote_unreachable(&self, child: &RemoteChild, reason: &str) {
        let name = self.machine_name(&child.target);
        let reason = reason.to_string();
        let result = self.remote_settle(child.task_id, true, |child| {
            ended(child, RemoteChildState::Unreachable);
            child.stop_requested = false;
            child.info.error = Some(cap_error(format!("{name} {reason}. Its result cannot be read any more.")));
        });
        if let Err(error) = result {
            tracing::warn!(task_id = %child.task_id, %error, "could not mark a remote agent unreachable");
        }
        // Whatever was waiting for it is not coming.
        let _ = self.remote_update(child.task_id, |child| std::mem::replace(&mut child.stop_requested, false));
    }

    // ---- the target machine ----

    /// Serve one daemon-to-daemon delegation request from machine `from`.
    pub async fn peer_dispatch(&self, from: &str, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            PeerDelegate::NAME => to_value(self.accept_delegate(from, from_value(params)?).await),
            PeerDelegationGet::NAME => to_value(self.serve_delegation_get(from, from_value(params)?).await),
            PeerDelegationCancel::NAME => to_value(self.serve_delegation_cancel(from, from_value(params)?).await),
            PeerGitBundle::NAME => to_value(self.accept_bundle_chunk(from_value(params)?)),
            PeerGitImport::NAME => to_value(self.accept_bundle_import(from_value(params)?).await),
            PeerThreadCreate::NAME => to_value(self.create_thread(from_value(params)?).await.map_err(thread_error)),
            PeerThreadSend::NAME => to_value(self.send_client_message(from_value(params)?).await.map_err(thread_error)),
            PeerFork::NAME => to_value(self.accept_fork(from, from_value(params)?).await),
            PeerMovePrepare::NAME => to_value(self.accept_move_prepare(from, from_value(params)?).await),
            PeerMoveCommit::NAME => to_value(self.accept_move_commit(from, from_value(params)?).await),
            PeerMoveGet::NAME => to_value(self.accept_move_status(from, from_value(params)?, false).await),
            PeerMoveAbort::NAME => to_value(self.accept_move_status(from, from_value(params)?, true).await),
            _ => Err(RpcError::method_not_found(method)),
        }
    }

    /// `peer.delegate`.
    async fn accept_delegate(&self, from: &str, params: PeerDelegateParams) -> Result<PeerDelegateResult, RpcError> {
        let hub = self.hub().map_err(internal)?;
        let from_name = hub.target(from).map(|target| target.name).unwrap_or_else(|| from.to_string());
        // The origin is the session's machine and nothing the caller wrote, and it travels one hop only.
        if params.origin.hops.len() > 1 || params.origin.hops.first().is_some_and(|hop| hop != from) {
            return Err(RpcError::new(codes::FORBIDDEN, "Delegations are not forwarded to a third machine."));
        }
        let origin = Origin {
            machine_id: from.to_string(),
            machine_name: from_name.clone(),
            thread_id: params.origin.thread_id,
            operation_id: params.operation_id,
            kind: OriginKind::Delegation { task_id: params.task_id.to_string() },
            permission_cap: params.origin.permission_cap,
            hops: vec![from.to_string()],
        };
        let operation = namespaced_operation(from, origin.thread_id, params.operation_id);
        if let Some(existing) = self.inner.store.delegation_find_by_operation(operation).map_err(internal)? {
            ensure_origin(&existing, from)?;
            return Ok(delegate_result(&existing));
        }
        let task = params.task.trim().to_owned();
        if task.is_empty() {
            return Err(RpcError::invalid_params("task is required"));
        }
        if let Some(clash) = self.inner.store.delegation_find_by_task(params.task_id).map_err(internal)? {
            return Err(RpcError::new(
                codes::CONFLICT,
                format!("task {} already exists for another delegation", clash.delegation.map_or(params.task_id, |d| d.task_id)),
            ));
        }
        validate_owns(&params.owns).map_err(RpcError::invalid_params)?;

        if let Some(group) = &params.group {
            validate_group(group, params.workspace == DelegationWorkspace::Worktree)?;
        }
        let (project, matched) = match &params.group {
            Some(work) => {
                let (candidate, matched) = self.group_target(work, params.project.as_deref(), &hub.self_name()).await?;
                (candidate.project, Some(matched))
            }
            None => (self.match_project(&params.repo, params.project.as_deref(), &hub.self_name()).await?, None),
        };
        let kind = params.provider.ok_or_else(|| RpcError::invalid_params("provider is required"))?;
        if self.inner.drivers.get(kind).is_none() {
            return Err(RpcError::new(codes::PROVIDER_UNAVAILABLE, format!("{kind} is not available on {}.", hub.self_name())));
        }
        if let Ok(Some(statuses)) = self.cached_provider_statuses(project.id).await
            && let Some(status) = statuses.iter().find(|status| status.kind == kind)
            && !status.available
        {
            return Err(RpcError::new(
                codes::PROVIDER_UNAVAILABLE,
                format!(
                    "{kind} is not installed or usable on {}: {}.",
                    hub.self_name(),
                    status.unavailable_reason.as_deref().unwrap_or("it was not found")
                ),
            ));
        }
        let provider = ProviderInstance::default_for(kind);
        self.validate_provider_selection(project.id, &provider, params.model.as_deref(), params.effort.as_deref())
            .await
            .map_err(|error| RpcError::new(codes::PROVIDER_UNAVAILABLE, format!("{error:#}")))?;
        // The mode can only narrow: never above what the delegating parent held.
        let permission_mode = narrower(params.permission_mode, origin.permission_cap);
        super::antigravity_rules::require_full_access(kind, permission_mode)
            .map_err(|error| RpcError::invalid_params(error.to_string()))?;

        let child_id = Uuid::now_v7();
        let mut members = Vec::new();
        let (cwd, worktree, base_commit, branch) = match params.workspace {
            DelegationWorkspace::Shared => {
                if let Some(matched) = &matched {
                    members = Self::group_thread_members(&project, matched);
                }
                (project.path.clone(), None, None, None)
            }
            DelegationWorkspace::Worktree if matched.is_some() => {
                let matched = matched.as_deref().unwrap_or_default();
                let missing = Self::group_missing_commits(matched).await;
                if !missing.is_empty() {
                    return Err(needs_bundle(NEEDS_BUNDLE, missing, project.id));
                }
                let template = Self::group_thread_members(&project, matched);
                let starts: std::collections::BTreeMap<String, String> =
                    matched.iter().filter_map(|member| member.work.commit.clone().map(|commit| (member.path.clone(), commit))).collect();
                let (worktree, group_members) = self
                    .create_group_workspace(&project, child_id, template, &starts, None)
                    .await
                    .map_err(|error| RpcError::new(codes::CONFLICT, format!("Could not create the worktrees for this group: {error:#}")))?;
                self.group_forget(matched, params.operation_id).await;
                members = group_members;
                let base = params.base_commit.clone().or_else(|| matched.first().and_then(|member| member.work.commit.clone()));
                (worktree.path.clone(), Some(worktree.clone()), base, Some(worktree.branch))
            }
            DelegationWorkspace::Worktree => {
                let base = params.base_commit.clone().ok_or_else(|| RpcError::invalid_params("base_commit is required for a worktree"))?;
                let repo = Repo::new(&project.path);
                if !git_transfer::has_commit(&repo, &base).await {
                    return Err(RpcError::new(codes::CONFLICT, format!("{} does not have commit {base} yet.", hub.self_name()))
                        .with_data(json!({ "needs": NEEDS_BUNDLE, "project_id": project.id })));
                }
                let branch = format!("kybern/{child_id}");
                let dir = self.worktree_dir(&project, &child_id.to_string());
                if let Some(parent_dir) = dir.parent() {
                    std::fs::create_dir_all(parent_dir).map_err(internal)?;
                }
                repo.worktree_add(&dir, &branch, Some(&base)).await.map_err(internal)?;
                let path = dir.to_string_lossy().into_owned();
                (path.clone(), Some(WorktreeInfo { path, branch: branch.clone() }), Some(base), Some(branch))
            }
        };

        let now = Utc::now();
        let info = DelegationInfo {
            task_id: params.task_id,
            operation_id: operation,
            // The delegating thread lives on another machine; nothing here is its parent.
            parent_thread_id: origin.thread_id.unwrap_or_else(Uuid::nil),
            depth: 1,
            role: params.role,
            workspace: params.workspace,
            default_override: None,
            owns: params.owns.clone(),
            status: DelegationStatus::Running,
            result: None,
            error: None,
            files_touched: Vec::new(),
            conflicts: Vec::new(),
            base_commit,
            branch: branch.clone(),
            head_commit: None,
            diffstat: None,
            worktree_state: (params.workspace == DelegationWorkspace::Worktree).then_some(WorktreeState::Active),
            started_at: now,
            completed_at: None,
            parent_notified: true,
        };
        let thread = Thread {
            id: child_id,
            project_id: project.id,
            title: child_title(params.title.as_deref(), &task),
            provider,
            model: params.model.clone(),
            effort: params.effort.clone(),
            permission_mode,
            status: ThreadStatus::Idle,
            worktree,
            cwd,
            provider_session_id: None,
            pinned: false,
            created_at: now,
            updated_at: now,
            last_seq: 0,
            parent_thread_id: None,
            coordinator_project_id: None,
            origin: Some(origin),
            inbound: None,
            moved: None,
            subagent: None,
            delegation: Some(info),
            members,
        };
        self.inner.store.thread_upsert(&thread).map_err(internal)?;
        let event = self.emit(thread.id, None, EventPayload::ThreadCreated { thread: thread.clone() }).map_err(internal)?;
        let thread = Thread { last_seq: event.seq, ..thread };
        self.inner.store.thread_upsert(&thread).map_err(internal)?;

        let brief = remote_brief(&params, &from_name, branch.as_deref(), &task);
        let message_id = Uuid::now_v7();
        let record = ThreadMessageRecord {
            id: message_id,
            operation_id: super::derived_operation_id(operation, 0x7a),
            from_thread_id: None,
            to_thread_id: child_id,
            purpose: ThreadMessagePurpose::Task,
            reply_to: None,
            body: brief.clone(),
            delivery: ThreadMessageDelivery::Queue,
            state: ThreadMessageState::Delivered,
            held_reason: None,
            created_at: now,
            updated_at: now,
        };
        self.inner.store.thread_message_insert(&record).map_err(internal)?;
        let message = UserMessage {
            parts: vec![ContentPart::ThreadMessage {
                message_id,
                from_thread_id: None,
                from_title: format!("{} on {from_name}", params.parent_title),
                purpose: ThreadMessagePurpose::Task,
                reply_to: None,
                body: brief,
            }],
        };
        if let Err(error) = self.send_with_id(child_id, message_id, message, false, false).await {
            let reason = cap_error(format!("The agent could not start: {error}"));
            let _ = self.delegation_update(child_id, |info| {
                info.status = DelegationStatus::Failed;
                info.error = Some(reason.clone());
                info.completed_at = Some(Utc::now());
                true
            });
        }
        let created = self.inner.store.thread_get(child_id).map_err(internal)?.unwrap_or(thread);
        Ok(delegate_result(&created))
    }

    /// The one of this machine's projects that is the repository `repo` (§6.2).
    pub(super) async fn match_project(&self, repo: &RepoIdentity, hint: Option<&str>, this_machine: &str) -> Result<Project, RpcError> {
        let projects = self.inner.store.projects_list().map_err(internal)?;
        let mut matches = Vec::new();
        for project in projects.into_iter().filter(|project| project.kind.is_single() && project.is_git) {
            let identity = match &project.repo {
                Some(identity) if !identity.is_empty() => identity.clone(),
                _ => git_transfer::identity_of(Path::new(&project.path)).await,
            };
            if git_transfer::same_repo(repo, &identity) {
                matches.push(project);
            }
        }
        if let Some(hint) = hint.map(str::trim).filter(|hint| !hint.is_empty()) {
            let narrowed: Vec<Project> = matches
                .iter()
                .filter(|project| project.id.to_string() == hint || project.name.eq_ignore_ascii_case(hint) || project.path == hint)
                .cloned()
                .collect();
            if !narrowed.is_empty() {
                matches = narrowed;
            }
        }
        match matches.len() {
            0 => Err(RpcError::new(
                codes::NOT_FOUND,
                format!(
                    "{this_machine} has no checkout of this repository. Add the project there (its menu → Add to another machine…), or delegate on this machine."
                ),
            )
            .with_data(json!({ "needs": NEEDS_PROJECT }))),
            1 => Ok(matches.remove(0)),
            _ => {
                let list = matches.iter().map(|project| format!("{} ({})", project.name, project.path)).collect::<Vec<_>>().join(", ");
                Err(RpcError::new(
                    codes::CONFLICT,
                    format!("{this_machine} has several checkouts of this repository: {list}. Retry with project set to one of them."),
                ))
            }
        }
    }

    /// `peer.delegation.get`: the child's state, waiting up to `wait_ms` for it to end.
    async fn serve_delegation_get(&self, from: &str, params: PeerDelegationGetParams) -> Result<PeerDelegationView, RpcError> {
        let wait = Duration::from_millis(params.wait_ms.unwrap_or(0).min(FOLLOW_WAIT_MS));
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.inner.delegation.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let thread = self.owned_child(from, params.task_id)?;
            let running = thread.delegation.as_ref().is_some_and(|info| info.status == DelegationStatus::Running);
            if !running || wait.is_zero() || tokio::time::timeout_at(deadline, notified).await.is_err() {
                // One more read so a result saved at the boundary is not missed.
                return Ok(delegation_view(&self.owned_child(from, params.task_id)?));
            }
        }
    }

    /// `peer.delegation.cancel`.
    async fn serve_delegation_cancel(&self, from: &str, params: PeerDelegationCancelParams) -> Result<PeerDelegationView, RpcError> {
        let thread = self.owned_child(from, params.task_id)?;
        if thread.delegation.as_ref().is_some_and(|info| info.status == DelegationStatus::Running) {
            tracing::info!(task_id = %params.task_id, machine = from, "another machine stopped a delegated agent");
            if let Err(error) = self.delegation_stop(thread.id, false).await {
                tracing::debug!(thread_id = %thread.id, %error, "stopped agent had no live session to interrupt");
            }
        }
        Ok(delegation_view(&self.owned_child(from, params.task_id)?))
    }

    /// The child `task_id` that machine `from` started; anything else is "not found", so one machine
    /// cannot read or stop another's.
    fn owned_child(&self, from: &str, task_id: Uuid) -> Result<Thread, RpcError> {
        let thread = self
            .inner
            .store
            .delegation_find_by_task(task_id)
            .map_err(internal)?
            .filter(|thread| thread.origin.as_ref().is_some_and(|origin| origin.machine_id == from))
            .ok_or_else(|| RpcError::new(codes::NOT_FOUND, "no such agent"))?;
        Ok(thread)
    }

    /// `peer.git.bundle`: one chunk of a bundle on its way.
    fn accept_bundle_chunk(&self, params: PeerGitBundleParams) -> Result<PeerGitBundleResult, RpcError> {
        use base64::Engine;
        let data = base64::engine::general_purpose::STANDARD
            .decode(params.data.as_bytes())
            .map_err(|error| RpcError::invalid_params(format!("the chunk is not base64: {error}")))?;
        let path = git_transfer::staged_path(&self.inner.paths.root, params.operation_id);
        let received = git_transfer::append_chunk(&path, params.offset, params.total, &data)
            .map_err(|error| RpcError::invalid_params(format!("{error:#}")))?;
        Ok(PeerGitBundleResult { received })
    }

    /// `peer.git.import`: check the staged bundle and import it into the matching checkout.
    async fn accept_bundle_import(&self, params: PeerGitImportParams) -> Result<PeerGitImportResult, RpcError> {
        let hub = self.hub().map_err(internal)?;
        let path = git_transfer::staged_path(&self.inner.paths.root, params.operation_id);
        let file = git_transfer::describe(&path).map_err(|_| RpcError::new(codes::NOT_FOUND, "no bundle was staged for this transfer"))?;
        if file.size != params.total || file.sha256 != params.sha256 {
            return Err(RpcError::new(codes::CONFLICT, "the staged bundle is incomplete or damaged; send it again"));
        }
        // A group member's bundle goes into that member's checkout of the matching group.
        let dir = if params.group_repos.is_empty() {
            self.match_project(&params.repo, params.project.as_deref(), &hub.self_name()).await?.path
        } else {
            let repos: Vec<GroupRepo> =
                params.group_repos.iter().map(|repo| GroupRepo { name: String::new(), repo: repo.clone() }).collect();
            let candidate = self.match_group(&repos, params.project.as_deref(), &hub.self_name()).await?;
            let member =
                candidate.members.iter().find(|member| git_transfer::same_repo(&params.repo, &member.repo)).ok_or_else(|| {
                    RpcError::new(codes::NOT_FOUND, "This machine has no checkout of that repository in the matching group.")
                })?;
            member.dir.to_string_lossy().into_owned()
        };
        let repo = Repo::new(&dir);
        let imported = git_transfer::import_bundle(&repo, &path, params.operation_id, &params.commit).await;
        let _ = std::fs::remove_file(&path);
        match imported {
            Ok(imported_ref) => Ok(PeerGitImportResult { commit: params.commit, imported_ref }),
            Err(git_transfer::ImportError::MissingBase(detail)) => {
                Err(RpcError::new(codes::CONFLICT, format!("the checkout lacks the commits this bundle builds on: {detail}"))
                    .with_data(json!({ "needs": "full_bundle" })))
            }
            Err(git_transfer::ImportError::Other(error)) => Err(RpcError::new(codes::CONFLICT, format!("{error:#}"))),
        }
    }
}

/// Put a remote child in a final state, and its delegation status with it.
fn ended(child: &mut RemoteChild, state: RemoteChildState) {
    child.state = state;
    child.info.status = state.delegation_status();
}

/// What a follower step decided.
enum Follow {
    /// Go around again at once.
    Continue,
    /// The child is finished (or nothing is left to do).
    Done,
    Retry(String),
    Unreachable(String),
}

/// A thread that was busy is worth another try; anything else the target refused is final.
pub(super) fn thread_error(error: anyhow::Error) -> RpcError {
    let message = format!("{error:#}");
    if message.contains("busy") { RpcError::new(codes::THREAD_BUSY, message) } else { RpcError::invalid_params(message) }
}

pub(super) fn internal(error: impl std::fmt::Display) -> RpcError {
    RpcError::internal(error)
}

pub(super) fn from_value<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(if params.is_null() { json!({}) } else { params }).map_err(RpcError::invalid_params)
}

fn to_value<T: serde::Serialize>(result: Result<T, RpcError>) -> Result<Value, RpcError> {
    result.and_then(|value| serde_json::to_value(value).map_err(RpcError::internal))
}

/// A remote error as a delivery outcome: busy or broken peers are retried, refusals are final.
pub(crate) fn remote_error_delivery(error: &RpcError) -> Delivery {
    match error.code {
        codes::INTERNAL_ERROR | codes::THREAD_BUSY | codes::NOT_AVAILABLE => Delivery::Retry(error.message.clone()),
        _ => Delivery::Failed(error.message.clone()),
    }
}

/// B's key for a delegation: A's operation id, namespaced by who asked (§6.2), so two parents cannot
/// collide and a retry finds the first child.
pub(crate) fn namespaced_operation(origin_machine: &str, origin_thread: Option<ThreadId>, operation_id: Uuid) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(origin_machine.as_bytes());
    hasher.update([0]);
    hasher.update(origin_thread.unwrap_or_else(Uuid::nil).as_bytes());
    hasher.update(operation_id.as_bytes());
    let digest = hasher.finalize();
    Uuid::from_slice(&digest[..16]).unwrap_or(operation_id)
}

fn rank(mode: PermissionMode) -> u8 {
    match mode {
        PermissionMode::Supervised => 0,
        PermissionMode::AcceptEdits => 1,
        PermissionMode::Auto => 2,
        PermissionMode::FullAccess => 3,
    }
}

/// The lower of two permission modes.
pub(crate) fn narrower(a: PermissionMode, b: PermissionMode) -> PermissionMode {
    if rank(a) <= rank(b) { a } else { b }
}

fn ensure_origin(thread: &Thread, from: &str) -> Result<(), RpcError> {
    if thread.origin.as_ref().is_some_and(|origin| origin.machine_id == from) {
        Ok(())
    } else {
        Err(RpcError::new(codes::CONFLICT, "operation_id already belongs to another delegation"))
    }
}

fn delegate_result(thread: &Thread) -> PeerDelegateResult {
    let info = thread.delegation.as_ref();
    PeerDelegateResult {
        task_id: info.map_or_else(Uuid::nil, |info| info.task_id),
        thread_id: thread.id,
        status: info.map_or(DelegationStatus::Running, |info| info.status),
        permission_mode: thread.permission_mode,
        project_name: None,
        branch: info.and_then(|info| info.branch.clone()),
    }
}

fn delegation_view(thread: &Thread) -> PeerDelegationView {
    let info = thread.delegation.clone().expect("owned children are delegations");
    PeerDelegationView {
        task_id: info.task_id,
        thread_id: thread.id,
        title: thread.title.clone(),
        provider: thread.provider.kind,
        model: thread.model.clone(),
        role: info.role,
        workspace: info.workspace,
        status: info.status,
        result: info.result,
        error: info.error,
        files_touched: info.files_touched,
        base_commit: info.base_commit,
        branch: info.branch,
        head_commit: info.head_commit,
        diffstat: info.diffstat,
        started_at: info.started_at,
        completed_at: info.completed_at,
        last_seq: thread.last_seq,
    }
}

/// The child's first message: the usual brief, plus what is different about a machine of its own.
fn remote_brief(params: &PeerDelegateParams, from_name: &str, branch: Option<&str>, task: &str) -> String {
    let parent_title = format!("{} (on {from_name})", params.parent_title);
    let mut brief = delegation_brief(params.role, &parent_title, params.workspace, &params.owns, branch, task);
    let note = match params.workspace {
        DelegationWorkspace::Shared => format!(
            "\n\nYou run on a different machine than your parent. This checkout is this machine's own: {from_name}'s uncommitted changes are not here, and paths in the task are this machine's paths."
        ),
        DelegationWorkspace::Worktree => format!(
            "\n\nYou run on a different machine than your parent. Your worktree starts from a snapshot of {from_name}'s changes; paths in the task are this machine's paths. Nothing you commit is pushed anywhere: your parent reads your branch name and commit in your result."
        ),
    };
    brief.push_str(&note);
    brief
}

const RESULT_CAP: usize = 16 * 1024;
