//! Read-only child threads for provider-native subagents.
//!
//! Every runtime task of kind `Agent` gets one child [`Thread`] (marked by
//! `Thread::subagent`) the moment it starts. The parent thread keeps its
//! `RuntimeTask*` launch rows and nested tool calls exactly as before; the
//! child's own log holds the subagent's prompt, messages, reasoning and tool
//! calls under a turn of its own. Child prose never reaches the parent log.
//!
//! Drivers tag subagent output in one of three ways (see
//! `DriverEvent::SubagentOnly`): an origin-tagged text event, a tool call whose
//! `parent_id` names the launching task or call, or the explicit wrapper. All
//! three arrive here, are resolved to a child through the task id, the provider
//! thread id or the launching tool call id, and are appended to the child's log
//! with root ownership (it is the child's own conversation). Nothing is routed
//! on a guess: output that names no known subagent waits in a small bounded
//! buffer for its task to start and is otherwise dropped.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use anyhow::{Result, anyhow};
use chrono::Utc;
use kybern_drivers::DriverEvent;
use kybern_protocol::*;
use uuid::Uuid;

use super::Orchestrator;

/// Subagent output that arrived before its task started and may still be claimed.
const PENDING_LIMIT: usize = 512;
/// The longest prompt stored as the subagent's first message.
const PROMPT_LIMIT: usize = 24_000;
/// How many launch prompts are remembered for tasks that have not started yet.
const PROMPT_CACHE_LIMIT: usize = 256;

pub(super) const READ_ONLY_ERROR: &str = "This action is unavailable in a read-only subagent thread. Active Claude subagents can receive messages in their own composer. Open its parent for other actions.";

#[derive(Default)]
pub(super) struct SubagentRouter {
    /// (root thread, task id | provider thread id | launching tool call id) -> child thread.
    keys: HashMap<(ThreadId, String), ThreadId>,
    children: HashMap<ThreadId, Child>,
    /// (root thread, tool call id) -> the child thread whose log holds that call.
    calls: HashMap<(ThreadId, String), ThreadId>,
    /// (root thread, task id | tool call id) -> prompt the subagent was launched with.
    prompts: HashMap<(ThreadId, String), String>,
    prompt_order: VecDeque<(ThreadId, String)>,
    /// Output of subagents whose task has not started yet.
    pending: VecDeque<(ThreadId, String, DriverEvent)>,
}

impl SubagentRouter {
    pub(super) fn message_turn(&self, child: ThreadId) -> Option<TurnId> {
        self.children.get(&child).filter(|child| child.turn_open).map(|child| child.turn_id)
    }
}

struct Child {
    root: ThreadId,
    title: String,
    turn_id: TurnId,
    /// The subagent is working on this turn.
    turn_open: bool,
    /// `turn_started` is in the child's log. It waits for the prompt when the
    /// provider reports it after the subagent itself (Codex).
    turn_started: bool,
    started: Instant,
    /// Provider message id -> our message id.
    messages: HashMap<String, MessageId>,
    /// The logical message currently receiving deltas.
    active_message: Option<MessageId>,
    /// Last completed non-empty message, recorded as the turn's final answer.
    terminal_message: Option<MessageId>,
    terminal_text: Option<String>,
    keys: Vec<String>,
    calls: HashSet<String>,
}

fn thread_status(status: RuntimeTaskStatus) -> ThreadStatus {
    match status {
        RuntimeTaskStatus::Pending | RuntimeTaskStatus::Running | RuntimeTaskStatus::Waiting | RuntimeTaskStatus::Stopping => {
            ThreadStatus::Running
        }
        RuntimeTaskStatus::Failed => ThreadStatus::Failed,
        RuntimeTaskStatus::Completed | RuntimeTaskStatus::Stopped | RuntimeTaskStatus::Interrupted => ThreadStatus::Idle,
    }
}

/// Whether the harness forwards the subagent's own conversation.
fn forwards_transcript(provider: ProviderKind, task: &RuntimeTask) -> bool {
    match provider {
        ProviderKind::ClaudeCode | ProviderKind::Codex => true,
        ProviderKind::Opencode => task.provider_thread_id.is_some(),
        ProviderKind::Cursor => task.id.starts_with("cursor-sdk:"),
        // Antigravity's subagents show as tool cards only.
        ProviderKind::Pi | ProviderKind::Omp | ProviderKind::Antigravity | ProviderKind::GrokBuild => false,
    }
}

fn first_line(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    Some(clip(line.trim_start_matches(['#', '*', '>', '-', ' ']), 240))
}

fn subagent_info(root: ThreadId, task: &RuntimeTask, provider: ProviderKind, final_text: Option<&str>) -> SubagentInfo {
    let progress = task
        .status
        .is_active()
        .then(|| task.detail.as_deref().and_then(first_line).or_else(|| task.last_tool_name.as_ref().map(|tool| format!("Using {tool}"))))
        .flatten();
    let result = (!task.status.is_active())
        .then(|| final_text.and_then(first_line).or_else(|| task.detail.as_deref().and_then(first_line)))
        .flatten();
    SubagentInfo {
        progress,
        result,
        task_id: task.id.clone(),
        root_thread_id: root,
        parent_turn_id: task.origin_turn_id,
        tool_call_id: task.tool_call_id.clone(),
        provider_thread_id: task.provider_thread_id.clone(),
        agent_type: task.provider_type.clone(),
        status: task.status,
        backgrounded: task.backgrounded,
        last_tool_name: task.last_tool_name.clone(),
        detail: task.detail.clone(),
        usage: task.usage.clone(),
        stats: task.stats.clone(),
        capabilities: task.capabilities.clone(),
        transcript: forwards_transcript(provider, task),
        started_at: task.started_at,
        completed_at: task.completed_at,
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

impl Orchestrator {
    /// Reject a mutation of a subagent thread.
    pub(super) fn ensure_not_subagent(&self, thread_id: ThreadId) -> Result<()> {
        match self.inner.store.thread_get(thread_id)? {
            Some(thread) if thread.subagent.is_some() => Err(anyhow!(READ_ONLY_ERROR)),
            _ => Ok(()),
        }
    }

    /// The thread and task a stop or background control acts on. A subagent
    /// thread resolves to its own task in the root session.
    pub(super) fn resolve_task_control(&self, thread_id: ThreadId, task_id: &str) -> Result<(ThreadId, String)> {
        match self.inner.store.thread_get(thread_id)?.and_then(|thread| thread.subagent) {
            Some(info) => Ok((info.root_thread_id, info.task_id)),
            None => Ok((thread_id, task_id.to_string())),
        }
    }

    /// Remember the prompt a subagent is launched with. A subagent whose first
    /// message was waiting for it starts its turn now.
    pub(super) fn subagent_remember_prompt(&self, root: ThreadId, key: &str, prompt: &str) {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return;
        }
        let prompt = clip(prompt, PROMPT_LIMIT);
        let mut router = self.inner.subagents.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(child_id) = router.keys.get(&(root, key.to_string())).copied()
            && let Some(child) = router.children.get_mut(&child_id)
        {
            if !child.turn_started
                && child.turn_open
                && let Err(error) = self.subagent_start_turn(child_id, child, prompt)
            {
                tracing::warn!(%child_id, %error, "could not start the subagent turn");
            }
            return;
        }
        let key = (root, key.to_string());
        if router.prompts.insert(key.clone(), prompt).is_none() {
            router.prompt_order.push_back(key);
            while router.prompt_order.len() > PROMPT_CACHE_LIMIT {
                if let Some(oldest) = router.prompt_order.pop_front() {
                    router.prompts.remove(&oldest);
                }
            }
        }
    }

    fn subagent_start_turn(&self, child_id: ThreadId, child: &mut Child, prompt: String) -> Result<()> {
        child.turn_started = true;
        self.emit(
            child_id,
            Some(child.turn_id),
            EventPayload::TurnStarted { message_id: Uuid::now_v7(), message: UserMessage::text(prompt) },
        )?;
        Ok(())
    }

    /// Start the turn of a subagent whose prompt never arrived, using its title.
    pub(super) fn subagent_ensure_started(&self, router: &mut SubagentRouter, child_id: ThreadId) -> Result<()> {
        let Some(child) = router.children.get(&child_id) else { return Ok(()) };
        if child.turn_started || !child.turn_open {
            return Ok(());
        }
        let root = child.root;
        let prompt = child.keys.iter().find_map(|key| router.prompts.remove(&(root, key.clone()))).unwrap_or_else(|| child.title.clone());
        if let Some(child) = router.children.get_mut(&child_id) {
            self.subagent_start_turn(child_id, child, prompt)?;
        }
        Ok(())
    }

    /// Create or refresh the child thread of an `Agent` runtime task. Idempotent:
    /// call it after every runtime-task event with the latest snapshot.
    pub(super) fn subagent_sync(&self, root: ThreadId, task: &RuntimeTask, session_owner: Option<Uuid>) -> Result<()> {
        if task.kind != RuntimeTaskKind::Agent {
            return Ok(());
        }
        let Some(root_thread) = self.inner.store.thread_get(root)? else { return Ok(()) };
        if root_thread.subagent.is_some() {
            return Ok(());
        }
        let mut router = self.inner.subagents.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let existing = match router.keys.get(&(root, task.id.clone())) {
            Some(id) => self.inner.store.thread_get(*id)?,
            None => self.inner.store.subagent_thread_find(root, &task.id)?,
        };
        let provider = root_thread.provider.kind;
        let info = subagent_info(root, task, provider, None);
        let (mut child_thread, created) = match existing {
            Some(thread) => (thread, false),
            None => {
                let parent = self.subagent_parent(&router, root, task);
                let now = Utc::now();
                let thread = Thread {
                    id: Uuid::now_v7(),
                    project_id: root_thread.project_id,
                    title: clip(&task.title, 160),
                    provider: root_thread.provider.clone(),
                    model: task.model.clone(),
                    effort: task.effort.clone(),
                    permission_mode: root_thread.permission_mode,
                    status: thread_status(task.status),
                    worktree: root_thread.worktree.clone(),
                    cwd: root_thread.cwd.clone(),
                    provider_session_id: None,
                    pinned: false,
                    created_at: task.started_at.min(now),
                    updated_at: now,
                    last_seq: 0,
                    parent_thread_id: Some(parent),
                    coordinator_project_id: None,
                    origin: None,
                    inbound: None,
                    moved: None,
                    subagent: Some(info.clone()),
                    delegation: None,
                    members: Vec::new(),
                };
                self.inner.store.thread_upsert(&thread)?;
                let event = self.emit(thread.id, None, EventPayload::ThreadCreated { thread: thread.clone() })?;
                let mut thread = thread;
                thread.last_seq = event.seq;
                (thread, true)
            }
        };

        if let Some(owner) = session_owner {
            let key = format!("subagent_owner:{}", child_thread.id);
            // Task identities must stay bound to their original process.
            if self.inner.store.meta_get(&key)?.is_none() {
                self.inner.store.meta_set(&key, &owner.to_string())?;
            }
        }
        if !task.status.is_active() {
            self.fail_subagent_messages(child_thread.id, "Not delivered — subagent finished.")?;
        }

        // Register every id the provider may use for this subagent.
        let mut keys = vec![task.id.clone()];
        keys.extend(task.tool_call_id.clone());
        keys.extend(task.provider_thread_id.clone());
        for key in &keys {
            router.keys.insert((root, key.clone()), child_thread.id);
        }

        let is_new = match router.children.entry(child_thread.id) {
            Entry::Vacant(slot) => {
                // A new subagent, or one the daemon knew before a restart: a turn
                // of its own starts when the task is live.
                slot.insert(Child {
                    root,
                    title: child_thread.title.clone(),
                    turn_id: Uuid::now_v7(),
                    turn_open: created || task.status.is_active(),
                    turn_started: false,
                    started: Instant::now(),
                    messages: HashMap::new(),
                    active_message: None,
                    terminal_message: None,
                    terminal_text: None,
                    keys: keys.clone(),
                    calls: HashSet::new(),
                });
                true
            }
            Entry::Occupied(mut slot) => {
                let child = slot.get_mut();
                for key in &keys {
                    if !child.keys.contains(key) {
                        child.keys.push(key.clone());
                    }
                }
                if task.status.is_active() && !child.turn_open {
                    // The task began another unit of work after finishing the last one.
                    child.turn_open = true;
                    child.started = Instant::now();
                    child.terminal_message = None;
                    child.terminal_text = None;
                    self.emit(child_thread.id, Some(child.turn_id), EventPayload::TurnResumed)?;
                }
                false
            }
        };
        // The prompt is known up front for most harnesses; Codex reports it later.
        if is_new && keys.iter().any(|key| router.prompts.contains_key(&(root, key.clone()))) {
            self.subagent_ensure_started(&mut router, child_thread.id)?;
        }

        // The thread row follows the task snapshot.
        let final_text = router.children.get(&child_thread.id).and_then(|child| child.terminal_text.clone());
        let info = subagent_info(root, task, provider, final_text.as_deref());
        let status = thread_status(task.status);
        let mut changed = false;
        if child_thread.subagent.as_ref() != Some(&info) {
            child_thread.subagent = Some(info);
            changed = true;
        }
        if child_thread.status != status && child_thread.status != ThreadStatus::Archived {
            child_thread.status = status;
            changed = true;
        }
        if !task.title.trim().is_empty() && child_thread.title != clip(&task.title, 160) {
            child_thread.title = clip(&task.title, 160);
            changed = true;
        }
        if task.model.is_some() && child_thread.model != task.model {
            child_thread.model = task.model.clone();
            changed = true;
        }
        if task.effort.is_some() && child_thread.effort != task.effort {
            child_thread.effort = task.effort.clone();
            changed = true;
        }

        let settled = !task.status.is_active();
        if settled && router.children.get(&child_thread.id).is_some_and(|child| child.turn_open) {
            self.subagent_ensure_started(&mut router, child_thread.id)?;
            if let Some(child) = router.children.get_mut(&child_thread.id) {
                self.subagent_finish_turn(child_thread.id, child, task)?;
            }
        }
        if settled {
            // Late ids can no longer route anywhere; free them.
            if let Some(child) = router.children.get(&child_thread.id) {
                for call in child.calls.clone() {
                    router.calls.remove(&(root, call));
                }
            }
            if let Some(child) = router.children.get_mut(&child_thread.id) {
                child.calls.clear();
            }
        }
        if changed {
            child_thread = self.update_thread(child_thread)?;
        }

        // A nested subagent also shows as a launch row inside its parent's thread.
        if let Some(parent_id) = child_thread.parent_thread_id.filter(|parent| *parent != root)
            && let Some(parent) = router.children.get(&parent_id)
        {
            let mut row = task.clone();
            row.thread_id = parent_id;
            row.origin_turn_id = parent.turn_id;
            row.started_seq = 0;
            row.updated_seq = 0;
            let payload = if created {
                EventPayload::RuntimeTaskStarted { task: row }
            } else if task.status.is_active() {
                EventPayload::RuntimeTaskUpdated { task: row }
            } else {
                EventPayload::RuntimeTaskCompleted { task: row }
            };
            self.emit(parent_id, Some(parent.turn_id), payload)?;
        }

        // Output that arrived ahead of the task start now has a home.
        let claimed: Vec<_> = {
            let mut kept = VecDeque::with_capacity(router.pending.len());
            let mut claimed = Vec::new();
            while let Some((pending_root, key, event)) = router.pending.pop_front() {
                if pending_root == root && router.keys.get(&(root, key.clone())) == Some(&child_thread.id) {
                    claimed.push(event);
                } else {
                    kept.push_back((pending_root, key, event));
                }
            }
            router.pending = kept;
            claimed
        };
        for event in claimed {
            self.subagent_apply(&mut router, child_thread.id, event)?;
        }
        Ok(())
    }

    /// Thread that becomes the parent of a new subagent thread: the subagent
    /// that launched it, when known, else the root thread.
    fn subagent_parent(&self, router: &SubagentRouter, root: ThreadId, task: &RuntimeTask) -> ThreadId {
        let by_parent_task = task.parent_id.as_ref().and_then(|parent| router.keys.get(&(root, parent.clone())));
        let by_launch_call = task.tool_call_id.as_ref().and_then(|call| router.calls.get(&(root, call.clone())));
        by_parent_task.or(by_launch_call).copied().unwrap_or(root)
    }

    fn subagent_finish_turn(&self, child_id: ThreadId, child: &mut Child, task: &RuntimeTask) -> Result<()> {
        child.turn_open = false;
        child.active_message = None;
        let duration_ms = task.stats.duration_ms.unwrap_or_else(|| child.started.elapsed().as_millis() as u64);
        if task.status == RuntimeTaskStatus::Failed {
            let error = task.detail.clone().filter(|detail| !detail.trim().is_empty()).unwrap_or_else(|| "The subagent failed.".into());
            self.emit(child_id, Some(child.turn_id), EventPayload::TurnFailed { error })?;
        } else {
            self.emit(
                child_id,
                Some(child.turn_id),
                EventPayload::TurnCompleted {
                    stop_reason: match task.status {
                        RuntimeTaskStatus::Stopped | RuntimeTaskStatus::Interrupted => StopReason::Interrupted,
                        _ => StopReason::Completed,
                    },
                    usage: task.usage.clone().unwrap_or_default(),
                    cost_usd: None,
                    duration_ms,
                    terminal_message_id: child.terminal_message,
                },
            )?;
        }
        Ok(())
    }

    /// Route one subagent event to its thread. `key` is the id the driver used
    /// for the subagent. Returns whether a child took it.
    pub(super) fn subagent_route(&self, root: ThreadId, key: &str, event: DriverEvent) -> Result<bool> {
        let mut router = self.inner.subagents.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(child_id) = router.keys.get(&(root, key.to_string())).copied() else {
            if router.pending.len() >= PENDING_LIMIT {
                router.pending.pop_front();
            }
            router.pending.push_back((root, key.to_string(), event));
            return Ok(false);
        };
        self.subagent_apply(&mut router, child_id, event)?;
        Ok(true)
    }

    /// Mirror a tool call a driver nested under its launching task into that
    /// subagent's thread. `owner` is the task or launch-call id from `parent_id`.
    pub(super) fn subagent_mirror_tool_started(&self, root: ThreadId, owner: &str, call: &ToolCall) -> Result<()> {
        let mut router = self.inner.subagents.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(child_id) = router.keys.get(&(root, owner.to_string())).copied() else { return Ok(()) };
        self.subagent_apply(&mut router, child_id, DriverEvent::ToolStarted(call.clone()))
    }

    /// Whether a subagent thread holds this tool call, so its output must be mirrored there.
    pub(super) fn subagent_holds_call(&self, root: ThreadId, call_id: &str) -> bool {
        let router = self.inner.subagents.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        router.calls.contains_key(&(root, call_id.to_string()))
    }

    /// Mirror a tool output or completion into the subagent that holds the call.
    pub(super) fn subagent_mirror_tool_event(&self, root: ThreadId, call_id: &str, event: DriverEvent) -> Result<()> {
        let mut router = self.inner.subagents.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(child_id) = router.calls.get(&(root, call_id.to_string())).copied() else { return Ok(()) };
        self.subagent_apply(&mut router, child_id, event)
    }

    /// Consume a driver event that belongs to a subagent's own conversation.
    /// Returns the event back when it is the parent's.
    pub(super) fn subagent_take_event(&self, root: ThreadId, event: DriverEvent) -> Result<Option<DriverEvent>> {
        let key = match &event {
            DriverEvent::TextDelta { origin, .. }
            | DriverEvent::ThinkingDelta { origin, .. }
            | DriverEvent::ThinkingCompleted { origin, .. }
            | DriverEvent::MessageCompleted { origin, .. }
            | DriverEvent::ImageReceived { origin, .. } => match origin {
                EventOrigin::Agent { task_id, .. } => task_id.clone(),
                EventOrigin::Root => return Ok(Some(event)),
            },
            DriverEvent::SubagentPrompt { task_id, prompt } => {
                self.subagent_remember_prompt(root, task_id, prompt);
                return Ok(None);
            }
            DriverEvent::SubagentOnly { task_id, .. } => task_id.clone(),
            _ => return Ok(Some(event)),
        };
        let event = match event {
            DriverEvent::SubagentOnly { event, .. } => *event,
            other => other,
        };
        self.subagent_route(root, &key, event)?;
        Ok(None)
    }

    fn subagent_apply(&self, router: &mut SubagentRouter, child_id: ThreadId, event: DriverEvent) -> Result<()> {
        self.subagent_ensure_started(router, child_id)?;
        let Some(child) = router.children.get_mut(&child_id) else { return Ok(()) };
        if !child.turn_open {
            tracing::trace!(%child_id, "dropped subagent output after its turn ended");
            return Ok(());
        }
        let turn = Some(child.turn_id);
        match event {
            DriverEvent::TextDelta { message_id, delta, .. } => {
                let id = Self::subagent_message(child, &message_id);
                self.emit(child_id, turn, EventPayload::AssistantTextDelta { message_id: id, origin: EventOrigin::Root, delta })?;
            }
            DriverEvent::ThinkingDelta { message_id, delta, .. } => {
                let id = Self::subagent_message(child, &message_id);
                self.emit(child_id, turn, EventPayload::AssistantThinkingDelta { message_id: id, origin: EventOrigin::Root, delta })?;
            }
            DriverEvent::ThinkingCompleted { message_id, .. } => {
                // Close the reasoning of the message it streamed into; never open one.
                if let (Some(active), Some(mapped)) = (child.active_message, child.messages.get(&message_id))
                    && active == *mapped
                {
                    self.emit(child_id, turn, EventPayload::AssistantThinkingCompleted { message_id: active, origin: EventOrigin::Root })?;
                }
            }
            DriverEvent::MessageCompleted { message_id, text, thinking, .. } => {
                let thinking = thinking.filter(|thinking| !thinking.trim().is_empty());
                if text.trim().is_empty() && thinking.is_none() {
                    return Ok(());
                }
                let id = child.active_message.take().or_else(|| child.messages.get(&message_id).copied()).unwrap_or_else(Uuid::now_v7);
                child.messages.insert(message_id, id);
                if !text.trim().is_empty() {
                    child.terminal_message = Some(id);
                    child.terminal_text = Some(clip(&text, 2_000));
                }
                self.emit(
                    child_id,
                    turn,
                    EventPayload::AssistantMessageCompleted { message_id: id, origin: EventOrigin::Root, text, thinking },
                )?;
            }
            DriverEvent::ImageReceived { id, source, .. } => {
                if source.len() <= 70_000_000 {
                    self.emit(child_id, turn, EventPayload::ImageReceived { id, origin: EventOrigin::Root, source })?;
                }
            }
            DriverEvent::ToolStarted(mut call) => {
                call.parent_id = None;
                // A response boundary: later prose belongs to a new message.
                child.active_message = None;
                child.calls.insert(call.id.clone());
                let root = child.root;
                router.calls.insert((root, call.id.clone()), child_id);
                self.emit(child_id, turn, EventPayload::ToolCallStarted { call, origin: EventOrigin::Root })?;
            }
            DriverEvent::ToolOutputDelta { tool_call_id, delta } => {
                self.emit(child_id, turn, EventPayload::ToolCallOutputDelta { tool_call_id, delta })?;
            }
            DriverEvent::ToolCompleted { tool_call_id, output, is_error } => {
                self.emit(
                    child_id,
                    turn,
                    EventPayload::ToolCallCompleted { tool_call_id, output, output_omitted: false, stream_recoverable: false, is_error },
                )?;
            }
            _ => {}
        }
        Ok(())
    }

    fn subagent_message(child: &mut Child, provider_id: &str) -> MessageId {
        if let Some(active) = child.active_message {
            child.messages.insert(provider_id.to_string(), active);
            return active;
        }
        // A delta after completion opens a fresh logical message, even if the
        // provider reused a raw id.
        let id = Uuid::now_v7();
        child.messages.insert(provider_id.to_string(), id);
        child.active_message = Some(id);
        id
    }

    /// Settle and archive every subagent thread below `parent_id`.
    pub(super) fn subagents_archive_below(&self, parent_id: ThreadId) -> Result<()> {
        for child in self.inner.store.subagent_children(parent_id)? {
            self.subagents_archive_below(child.id)?;
            self.subagent_archive(child)?;
        }
        Ok(())
    }

    fn subagent_archive(&self, mut child: Thread) -> Result<()> {
        {
            let mut router = self.inner.subagents.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(state) = router.children.remove(&child.id) {
                if state.turn_open && state.turn_started {
                    self.emit(
                        child.id,
                        Some(state.turn_id),
                        EventPayload::TurnCompleted {
                            stop_reason: StopReason::Interrupted,
                            usage: Usage::default(),
                            cost_usd: None,
                            duration_ms: state.started.elapsed().as_millis() as u64,
                            terminal_message_id: state.terminal_message,
                        },
                    )?;
                }
                let root = state.root;
                router.keys.retain(|(owner, _), id| !(*owner == root && *id == child.id));
                router.calls.retain(|_, id| *id != child.id);
            }
        }
        if child.status == ThreadStatus::Archived {
            return Ok(());
        }
        child.status = ThreadStatus::Archived;
        self.update_thread(child.clone())?;
        self.emit(child.id, None, EventPayload::ThreadArchived)?;
        Ok(())
    }
}
