//! Owns live provider sessions and turns their events into the persisted,
//! broadcast thread log.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use kybern_drivers::registry::DriverRegistry;
use kybern_drivers::{
    AgentSession, DriverEvent, DriverRuntimeTask, DriverRuntimeTaskUpdate, RewindPoint, SessionConfig, SpawnedSession, TurnAnchors,
};
use kybern_git::{Repo, checkpoint_ref};
use kybern_protocol::*;
use kybern_store::{Store, TurnUsageRow};
use tokio::sync::{Mutex, Notify, Semaphore};
use uuid::Uuid;

use crate::config::Paths;
use crate::settings::SettingsStore;

mod accounts;
mod agent_items;
mod antigravity_rules;
mod askpass;
mod change_report;
mod child_questions;
mod delegation;
mod federated_tools;
pub(crate) mod fork_machine;
pub(crate) mod group_git;
mod group_pr;
mod group_transfer;
mod groups;
pub(crate) mod inbound;
mod lead;
mod lead_git;
mod lead_proposals;
mod login_borrow;
mod login_request;
mod messaging;
pub(crate) mod move_thread;
mod note_claims;
mod note_staleness;
mod note_tidy;
mod notes;
mod omp_recovery;
mod pick_request;
mod project_map;
pub(crate) mod provision;
pub(crate) mod remote_delegation;
mod request_tools;
mod rotation;
mod secret_request;
mod spaces;
mod subagent_messaging;
mod subagents;
mod tasks;
mod visual_answer;
mod worktrees;

#[derive(Clone)]
pub struct Orchestrator {
    inner: Arc<Inner>,
}

/// A native session's history ready to become a Kybern thread (`adopt_session_history`).
pub(crate) struct AdoptSession {
    pub project: Project,
    /// The account the history was discovered in.
    pub instance: String,
    pub history: kybern_drivers::sessions::SessionHistory,
    pub imported_profile: Option<String>,
    /// The thread's `updated_at`: now for a single resume, the session's own last activity for a bulk import.
    pub updated_at: chrono::DateTime<Utc>,
    /// Stream the whole history to live subscribers (a single resume, which the client opens next). A bulk import
    /// announces only the thread's creation and import: its history stays in the store for whoever opens it.
    pub announce_history: bool,
}

/// A rejected send, before any turn was accepted. Thread messaging may retry
/// this condition; provider/startup failures must still surface for inspection.
#[derive(Debug)]
struct ThreadBusy;

impl std::fmt::Display for ThreadBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("thread is busy")
    }
}

impl std::error::Error for ThreadBusy {}

impl Orchestrator {
    pub(crate) fn provider_catalog_cache(&self) -> Arc<crate::state::ProviderCatalogCache> {
        self.inner.provider_catalogs.clone()
    }

    /// On open, upgrade old Cursor picker IDs from the cached account catalog.
    /// Only thread fields change; saved transcript events retain their original text.
    pub(crate) fn migrate_cursor_model(&self, thread: Thread) -> Result<Thread> {
        if thread.provider.kind != ProviderKind::Cursor
            || !thread.model.as_deref().is_some_and(|model| model.starts_with("cursor-") && !model.starts_with("cursor-model:"))
        {
            return Ok(thread);
        }
        let settings = self.inner.settings.get();
        let provider = settings.providers.get(&ProviderKind::Cursor).cloned().unwrap_or_default();
        let project = self.inner.store.project_get(thread.project_id)?.ok_or_else(|| anyhow!("project not found"))?;
        // Last-known catalogs describe the project's default account only.
        if thread.provider.instance != crate::provider_accounts::resolve(&provider, Some(&project.path), None) {
            return Ok(thread);
        }
        let Some((statuses, _)) = self.inner.provider_catalogs.last_known(&thread.project_id.to_string()) else { return Ok(thread) };
        let Some(status) = statuses.iter().find(|status| status.kind == ProviderKind::Cursor) else { return Ok(thread) };
        let Some((model, effort)) = kybern_drivers::cursor::legacy_model_selection(thread.model.as_deref().unwrap(), &status.models) else {
            return Ok(thread);
        };
        let _updates = self.inner.thread_updates.lock().map_err(|_| anyhow!("thread update lock poisoned"))?;
        let mut current = self.inner.store.thread_get(thread.id)?.ok_or_else(|| anyhow!("thread not found"))?;
        if current.model != thread.model {
            return Ok(current);
        }
        current.model = Some(model);
        if current.effort.is_none() {
            current.effort = effort;
        }
        self.write_thread_update(current, true)
    }

    async fn cached_provider_statuses(&self, project_id: ProjectId) -> Result<Option<Vec<ProviderStatus>>> {
        let project = self.inner.store.project_get(project_id)?.ok_or_else(|| anyhow!("project not found"))?;
        let cwd = PathBuf::from(project.path);
        let settings = self.inner.settings.get();
        let cache_key = serde_json::to_string(&(Some(project_id), Some(&cwd), &settings.providers))?;
        Ok(self.inner.provider_catalogs.get_if_fresh(&cache_key).await)
    }

    async fn refresh_provider_statuses(&self, project_id: ProjectId) -> Result<Vec<ProviderStatus>> {
        let project = self.inner.store.project_get(project_id)?.ok_or_else(|| anyhow!("project not found"))?;
        let cwd = PathBuf::from(project.path);
        let settings = self.inner.settings.get();
        let cache_key = serde_json::to_string(&(Some(project_id), Some(&cwd), &settings.providers))?;
        let drivers = self.inner.drivers.clone();
        let catalogs = self.inner.provider_catalogs.clone();
        Ok(self
            .inner
            .provider_catalogs
            .get_or_refresh(cache_key, false, || async move {
                let probes = ProviderKind::ALL.into_iter().map(|kind| {
                    let driver = drivers.get(kind);
                    let provider_settings = crate::settings::provider_settings(&settings, kind, cwd.to_str());
                    let context = kybern_drivers::ProbeContext {
                        binary: provider_settings.binary.map(PathBuf::from),
                        cwd: Some(cwd.clone()),
                        env: provider_settings.env,
                    };
                    async move {
                        match driver {
                            Some(driver) => driver.probe_with_context(&context).await,
                            None => ProviderStatus {
                                kind,
                                display_name: kind.display_name().to_string(),
                                available: false,
                                binary_path: None,
                                version: None,
                                unavailable_reason: Some("driver not implemented yet".into()),
                                supported_permission_modes: vec![],
                                supports_fork: false,
                                supports_model_switch: false,
                                supports_effort_switch: false,
                                supported_efforts: vec![],
                                models: vec![],
                                instances: vec![],
                            },
                        }
                    }
                });
                let statuses = futures::future::join_all(probes).await;
                // Keep it for a slow probe later, and across restarts.
                catalogs.remember(&project_id.to_string(), &statuses);
                statuses
            })
            .await)
    }

    async fn validate_provider_selection(
        &self,
        project_id: ProjectId,
        provider: &ProviderInstance,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<()> {
        let settings = self.inner.settings.get();
        let provider_settings = settings.providers.get(&provider.kind).cloned().unwrap_or_default();
        crate::provider_accounts::environment(&provider_settings, provider.kind, &provider.instance)?;
        let project = self.inner.store.project_get(project_id)?.ok_or_else(|| anyhow!("Project not found."))?;
        if provider.instance != crate::provider_accounts::resolve(&provider_settings, Some(&project.path), None) {
            // The legacy aggregate cache describes the project-default account.
            // It cannot reject a model/effort advertised by a different account.
            return Ok(());
        }

        let Some(statuses) = self.cached_provider_statuses(project_id).await? else { return Ok(()) };
        let Some(status) = statuses.iter().find(|status| status.kind == provider.kind) else { return Ok(()) };
        let selected_model = model.filter(|model| !model.trim().is_empty());
        let catalog_model = selected_model.and_then(|selected| status.models.iter().find(|candidate| candidate.id == selected));
        if let Some(effort) = effort.filter(|effort| !effort.trim().is_empty()) {
            let supported = match catalog_model {
                Some(catalog_model) if !catalog_model.efforts.is_empty() => catalog_model.efforts.as_slice(),
                Some(_) if !status.supported_efforts.is_empty() => status.supported_efforts.as_slice(),
                Some(_) => {
                    return Err(anyhow!(
                        "Model {} on {} has no effort levels. Clear the effort, or pick a model that lists efforts.",
                        selected_model.unwrap_or_default(),
                        provider.kind
                    ));
                }
                None => status.supported_efforts.as_slice(),
            };
            if !supported.is_empty() && !supported.iter().any(|candidate| candidate == effort) {
                let choices = supported.iter().take(20).cloned().collect::<Vec<_>>().join(", ");
                return Err(anyhow!("Unknown effort '{}' for {}. Available efforts: {}.", effort, provider.kind, choices));
            }
        }
        Ok(())
    }

    fn set_thread_relationships(
        &self,
        thread_id: ThreadId,
        parent_thread_id: Option<ThreadId>,
        coordinator_project_id: Option<ProjectId>,
    ) -> Result<Thread> {
        self.inner.store.thread_set_relationships(thread_id, parent_thread_id, coordinator_project_id)?;
        let thread = self.inner.store.thread_get(thread_id)?.ok_or_else(|| anyhow!("thread disappeared while updating relationships"))?;
        self.update_thread(thread)
    }

    pub fn execute_native_app_tool_call<'a>(
        &'a self,
        thread_id: ThreadId,
        session_instance_id: Uuid,
        call_id: &'a str,
        name: &'a str,
        mut arguments: serde_json::Value,
    ) -> futures::future::BoxFuture<'a, Result<serde_json::Value>> {
        Box::pin(async move {
            let live = self
                .inner
                .sessions
                .lock()
                .await
                .get(&thread_id)
                .cloned()
                .ok_or_else(|| anyhow!("native tool caller session is no longer active"))?;
            if live.session_instance_id != session_instance_id {
                return Err(anyhow!("native tool credential belongs to a replaced session"));
            }
            let turn_id = self
                .owns_app_tool_turn(thread_id, &live, None)
                .await
                .ok_or_else(|| anyhow!("native tool request has no active owning turn"))?;
            let operation_id =
                crate::app_tools::prepare_native_operation(thread_id, session_instance_id, turn_id, call_id, name, &mut arguments)?;
            // `machine: "all"` reads every machine's notes and tasks (design §9.2).
            if crate::network::federated::wants_all(&arguments) {
                return self.federated_tool_call(thread_id, session_instance_id, name, arguments).await;
            }
            // `machine` runs the tool on another machine of the network (forwarded tools, design §6.3).
            if inbound::named_machine(&arguments).is_some() {
                match self.forward_named_call(thread_id, session_instance_id, turn_id, call_id, name, arguments, operation_id).await? {
                    inbound::Forwarded::Done(value) => return Ok(value),
                    inbound::Forwarded::Local(local) => arguments = local,
                }
            }
            let mut result = self.execute_native_app_tool(thread_id, session_instance_id, name, arguments).await?;
            if let (Some(id), Some(object)) = (operation_id, result.as_object_mut()) {
                object.insert("operation_id".into(), serde_json::to_value(id)?);
            }
            Ok(result)
        })
    }

    pub fn execute_native_app_tool<'a>(
        &'a self,
        thread_id: ThreadId,
        session_instance_id: Uuid,
        name: &'a str,
        arguments: serde_json::Value,
    ) -> futures::future::BoxFuture<'a, Result<serde_json::Value>> {
        Box::pin(async move {
            let live = self
                .inner
                .sessions
                .lock()
                .await
                .get(&thread_id)
                .cloned()
                .ok_or_else(|| anyhow!("native tool caller session is no longer active"))?;
            if live.session_instance_id != session_instance_id {
                return Err(anyhow!("native tool credential belongs to a replaced session"));
            }
            let thread = self.inner.store.thread_get(thread_id)?.ok_or_else(|| anyhow!("native tool caller thread no longer exists"))?;
            let restrictions = self.native_tool_restrictions(&thread)?;
            if (!restrictions.allowed_tools.is_empty() && !restrictions.allowed_tools.iter().any(|allowed| allowed == name))
                || restrictions.denied_tools.iter().any(|denied| denied == name)
            {
                return Err(anyhow!(
                    "This tool is unavailable to a project Coordinator. Delegate the task to a child with kybern_agent_delegate."
                ));
            }
            let turn_id = live
                .turn
                .lock()
                .await
                .as_ref()
                .filter(|turn| !turn.completed && turn.internal.is_none())
                .map(|turn| turn.id)
                .ok_or_else(|| anyhow!("native tool request has no active owning turn"))?;
            self.dispatch_native_tool(&thread, turn_id, &live, session_instance_id, name, arguments).await
        })
    }

    /// Run one Kybern agent tool for `thread`, whose live session `live` owns the turn `turn_id`. The caller
    /// has checked that the session is current and that the tool is offered; a forwarded call from another
    /// machine arrives here too, as the inbound thread (`inbound.rs`).
    fn dispatch_native_tool<'a>(
        &'a self,
        thread: &'a Thread,
        turn_id: TurnId,
        live: &'a Arc<LiveSession>,
        session_instance_id: Uuid,
        name: &'a str,
        arguments: serde_json::Value,
    ) -> futures::future::BoxFuture<'a, Result<serde_json::Value>> {
        Box::pin(async move {
            let thread_id = thread.id;
            let live = live.clone();
            if matches!(name, "kybern_html_preview" | "kybern_html_publish") {
                anyhow::ensure!(serde_json::to_vec(&arguments)?.len() <= 768 * 1024, "HTML tool arguments exceed 768 KiB.");
                if name == "kybern_html_preview" {
                    let mut object = arguments.as_object().cloned().ok_or_else(|| anyhow!("HTML arguments must be an object"))?;
                    anyhow::ensure!(!object.contains_key("thread_id"), "HTML tools are bound to the current thread.");
                    object.insert("thread_id".into(), serde_json::json!(thread_id));
                    let params = crate::app_tools::parse(serde_json::Value::Object(object))?;
                    let result = crate::visuals::preview(&self.inner.paths, params).await?;
                    let metadata = serde_json::json!({"width": result.width, "contentHeight":result.content_height,"capturedHeight":result.captured_height,"consoleMessages":result.console_messages,"missingImages":result.missing_images});
                    return Ok(
                        serde_json::json!({"_kybern_content":[{"type":"text","text":metadata.to_string()},{"type":"image","mimeType":"image/png","data":result.screenshot}]}),
                    );
                }
                let mut object = arguments.as_object().cloned().ok_or_else(|| anyhow!("HTML arguments must be an object"))?;
                anyhow::ensure!(!object.contains_key("thread_id"), "HTML tools are bound to the current thread.");
                object.insert("thread_id".into(), serde_json::json!(thread_id));
                let params: methods::HtmlPublishParams = crate::app_tools::parse(serde_json::Value::Object(object))?;
                let ask = crate::visuals::answer_ask(&params)?;
                if ask.is_some() {
                    self.ensure_one_visual_question(turn_id, &live).await?;
                }
                let event = crate::visuals::publish(
                    &self.inner.store,
                    &self.inner.paths,
                    thread_id,
                    turn_id,
                    &params.html,
                    &params.title,
                    params.height,
                    ask,
                )
                .await?;
                let EventPayload::HtmlPublished { visual } = &event.payload else { unreachable!() };
                let visual = visual.clone();
                let _ = self.inner.events.send(event);
                if visual.ask.is_some() {
                    let wait = crate::visuals::answer_wait(params.timeout_seconds);
                    return self.execute_visual_answer(thread_id, turn_id, &live, visual, wait).await;
                }
                return Ok(
                    serde_json::json!({"visual":visual,"message":"The visual is published inline. Add only what the page does not already explain."}),
                );
            }
            if name == crate::network::forward::MACHINES_TOOL {
                return self.execute_machines(thread).await;
            }
            if name == provision::ADD_TOOL {
                return self.execute_add_to_machine(thread, turn_id, &live, arguments).await;
            }
            if name == crate::previews::agent::TOOL {
                let roots = crate::previews::thread_roots(&self.inner.store, thread);
                let policy = crate::previews::GrantPolicy::new(&self.inner.paths.root);
                let settings = self.inner.settings.get();
                let clients = self.inner.preview_requests.receiver_count();
                let outcome = crate::previews::agent::open(thread_id, &roots, &settings, &policy, arguments, clients)?;
                if let Some(notification) = outcome.notify {
                    let _ = self.inner.preview_requests.send(notification);
                }
                return Ok(outcome.result);
            }
            if crate::computer::ComputerUse::is_tool(name) {
                let consent = ComputerConsent { orchestrator: self, thread_id, turn_id, live: live.clone() };
                let ctx = crate::computer::CallContext {
                    thread_id,
                    permission_mode: thread.permission_mode,
                    session_instance_id,
                    turn_id,
                    consent: &consent,
                    ready_elsewhere: self.computer_machines(thread),
                };
                return self.inner.computer.execute(ctx, name, arguments).await;
            }
            if crate::secrets::is_tool(name) {
                return self.execute_request_secret(thread, turn_id, &live, arguments).await;
            }
            if crate::pick::is_tool(name) {
                return self.execute_request_pick(thread, turn_id, &live, arguments).await;
            }
            if login_request::is_tool(name) {
                return self.execute_request_login(thread, turn_id, &live, arguments).await;
            }
            if request_tools::is_tool(name) {
                return self.execute_request_tool(thread, turn_id, &live, name, arguments).await;
            }
            if crate::lead_git::is_tool(name) {
                return self.execute_lead_git(thread, turn_id, &live, arguments).await;
            }
            if lead_proposals::is_tool(name) {
                return self.execute_lead_tool(thread, turn_id, &live, name, arguments).await;
            }
            if crate::project_map::is_tool(name) {
                return self.execute_project_map(thread, arguments).await;
            }
            if agent_items::is_tool(name) {
                return self.execute_notes_tasks_tool(thread, turn_id, &live, name, arguments).await;
            }
            if delegation::is_tool(name) {
                return self.execute_agent_tool(thread, turn_id, &live, name, arguments).await;
            }
            if child_questions::is_tool(name) {
                return self.execute_child_question_tool(thread, name, arguments).await;
            }
            if messaging::is_tool(name) {
                return self.execute_messaging_tool(thread, turn_id, &live, name, arguments).await;
            }
            let mut value = self.inner.app_tools.execute(thread_id, name, arguments).await?;
            if name == "kybern_thread_context" {
                let cached = self.cached_provider_statuses(thread.project_id).await?;
                let catalog_state = if cached.is_some() {
                    "cached"
                } else {
                    let this = self.clone();
                    let project_id = thread.project_id;
                    tokio::spawn(async move {
                        if let Err(error) = this.refresh_provider_statuses(project_id).await {
                            tracing::warn!(%project_id, %error, "provider catalog background refresh failed");
                        }
                    });
                    "loading"
                };
                let mut providers = cached
                    .unwrap_or_default()
                    .into_iter()
                    .map(|status| {
                        let model_count = status.models.len();
                        let models = status
                            .models
                            .into_iter()
                            .take(32)
                            .map(|model| {
                                let mut id = model.id;
                                let mut display_name = model.display_name;
                                truncate_utf8(&mut id, 256);
                                truncate_utf8(&mut display_name, 256);
                                let efforts = model
                                    .efforts
                                    .into_iter()
                                    .take(20)
                                    .map(|mut effort| {
                                        truncate_utf8(&mut effort, 128);
                                        effort
                                    })
                                    .collect::<Vec<_>>();
                                serde_json::json!({
                                    "id": id,
                                    "display_name": display_name,
                                    "efforts": efforts,
                                    "default_effort": model.default_effort,
                                    "is_default": model.is_default,
                                })
                            })
                            .collect::<Vec<_>>();
                        serde_json::json!({
                            "kind": status.kind,
                            "available": status.available,
                            "unavailable_reason": status.unavailable_reason,
                            "instances": status.instances,
                            "supported_permission_modes": status.supported_permission_modes,
                            "supports_fork": status.supports_fork,
                            "supports_model_switch": status.supports_model_switch,
                            "supports_effort_switch": status.supports_effort_switch,
                            "supported_efforts": status.supported_efforts,
                            "models": models,
                            "models_truncated": model_count.saturating_sub(32),
                        })
                    })
                    .collect::<Vec<_>>();
                if catalog_state == "loading" {
                    providers.extend(ProviderKind::ALL.into_iter().map(|kind| {
                        serde_json::json!({
                            "kind": kind,
                            "available": null,
                            "unavailable_reason": "provider catalog is loading for this project",
                            "instances": ["default"],
                            "supported_permission_modes": [],
                            "supports_fork": null,
                            "supports_model_switch": null,
                            "supports_effort_switch": null,
                            "supported_efforts": [],
                            "models": [],
                            "models_truncated": 0,
                        })
                    }));
                }
                let object = value.as_object_mut().expect("thread context serializes as an object");
                object.insert("providers".into(), providers.into());
                object.insert("provider_catalog_state".into(), catalog_state.into());
                if serde_json::to_vec(&value)?.len() > 256 * 1024 {
                    return Err(anyhow!("tool result exceeds the 256 KiB limit"));
                }
            }
            Ok(value)
        })
    }
}

fn derived_operation_id(base: Uuid, discriminator: u8) -> Uuid {
    let mut bytes = *base.as_bytes();
    bytes[0] ^= discriminator;
    Uuid::from_bytes(bytes)
}

fn truncate_utf8(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

/// Live filesystem probe for a project's git status, mirroring the check
/// `add_project` uses at registration. Git reads, project listing and the client
/// maintenance sweep refresh the cached `Project.is_git`; callers that gate
/// git-only behavior also use this fallback to pick up a later `git init`.
fn project_path_is_git(project_path: &str) -> bool {
    std::path::Path::new(project_path).join(".git").exists()
}

type RetainedSessions = HashMap<(ThreadId, String), (ProviderInstance, Arc<LiveSession>)>;

struct Inner {
    commands: std::sync::Mutex<()>,
    session_admission: Mutex<HashMap<ThreadId, Arc<Mutex<()>>>>,
    /// One checkpoint at a time per thread, so a turn's "after" snapshot,
    /// taken once the turn has ended, lands before the next turn's "before"
    /// and before a revert.
    checkpoint_locks: std::sync::Mutex<HashMap<ThreadId, Arc<Mutex<()>>>>,
    workspace_ops: Mutex<()>,
    question_answers: Mutex<()>,
    lead_commands: Mutex<()>,
    /// Creates that carry a client-chosen thread id, one at a time per id (`with_client_thread_id`).
    client_thread_ids: std::sync::Mutex<HashMap<ThreadId, Arc<Mutex<()>>>>,
    thread_updates: std::sync::Mutex<()>,
    store: Store,
    drivers: DriverRegistry,
    events: crate::bounded_broadcast::Sender<ThreadEvent>,
    paths: Paths,
    settings: SettingsStore,
    provider_catalogs: Arc<crate::state::ProviderCatalogCache>,
    app_tools: crate::app_tools::AppTools,
    native_tools: Option<crate::native_tools_mcp::NativeToolsGateway>,
    computer: crate::computer::ComputerUse,
    sessions: Mutex<HashMap<ThreadId, Arc<LiveSession>>>,
    retained_sessions: Mutex<RetainedSessions>,
    releasing: Mutex<HashMap<ThreadId, tokio::sync::watch::Receiver<()>>>,
    harness_gates: HashMap<ProviderKind, Arc<tokio::sync::RwLock<()>>>,
    /// Threads whose next session must fork the provider conversation at this point.
    pending_rewinds: Mutex<HashMap<ThreadId, RewindPoint>>,
    /// Woken whenever a queued follow-up may have become dispatchable, so the
    /// queue worker sleeps instead of polling the store.
    queue_wakeup: Notify,
    /// The daemon is stopping ([`Orchestrator::begin_shutdown`]): a turn whose agent fails now was
    /// interrupted by the stop, not by its own error.
    shutting_down: std::sync::atomic::AtomicBool,
    /// Note changes, forwarded to clients as `notes.changed`.
    notes_changed: tokio::sync::broadcast::Sender<methods::NotesChangedNotification>,
    /// Task changes, forwarded to clients as `tasks.items.changed`.
    tasks_changed: tokio::sync::broadcast::Sender<methods::TaskItemsChangedNotification>,
    /// The project list after each change, forwarded as `projects.changed`.
    projects_changed: tokio::sync::broadcast::Sender<methods::ProjectsChangedNotification>,
    spaces_changed: tokio::sync::broadcast::Sender<methods::SpacesChangedNotification>,
    /// Worktree preparation during `threads.create`, forwarded as `threads.workspace_progress`.
    workspace_progress: tokio::sync::broadcast::Sender<methods::ThreadsWorkspaceProgress>,
    /// Group worktree threads: each deselected member's state when a turn started,
    /// compared when it ends (`group_git::OutsideBaseline`).
    outside_baselines: std::sync::Mutex<HashMap<TurnId, Vec<group_git::OutsideBaseline>>>,
    /// Serialises space writes so name checks and inserts cannot interleave.
    space_writes: std::sync::Mutex<()>,
    /// Agent requests to show a page, forwarded as `previews.open_requested`.
    preview_requests: tokio::sync::broadcast::Sender<methods::PreviewOpenRequestedNotification>,
    /// Live state of sign-in, permission and deadline cards, forwarded as `requests.progress`.
    requests: crate::requests::RequestHub,
    /// Preview tickets, revoked when a thread is archived.
    previews: Arc<crate::previews::tickets::PreviewTickets>,
    /// Account plan limits per provider, forwarded as `usage.limits.changed`.
    usage: crate::usage::UsageMonitor,
    /// Serializes task writes, including the run tracker that `emit` calls.
    /// Always taken after `commands` when both are needed.
    task_writes: std::sync::Mutex<()>,
    /// Threads that are a task's run, so `emit` skips the store for every other thread.
    task_threads: std::sync::Mutex<HashSet<ThreadId>>,
    /// When a run's task was last published, to space out activity-only updates.
    task_published: std::sync::Mutex<HashMap<ThreadId, Instant>>,
    /// Agent notes-and-tasks grants and per-turn task counts.
    agent_items: std::sync::Mutex<agent_items::AgentItemState>,
    /// Serializes agent notes-and-tasks writes with their operation receipts.
    agent_item_ops: Mutex<()>,
    /// Routing state for the read-only child threads of provider subagents.
    subagents: std::sync::Mutex<subagents::SubagentRouter>,
    /// Orchestrator V2 delegation engine state (batches, waiters).
    delegation: delegation::DelegationState,
    /// Remote delegation: the machine hub, followers of children on other machines.
    remote: remote_delegation::RemoteState,
    /// Forwarded agent tools served for other machines: their inbound threads' sessions.
    inbound: inbound::InboundState,
    /// Orchestrator V2 messaging engine state (blocked askers, owed answers, edit guards).
    messaging: messaging::MessagingState,
    /// Test hooks for the Coordinator's git tool (the `gh` program, git environment).
    lead_git: crate::lead_git::LeadGitConfig,
    /// Coordinator context rotations in progress, and the handoff deadline.
    rotation: rotation::RotationState,
}

struct LiveSession {
    /// Daemon-local identity for one spawned process. Native tool credentials bind to it.
    session_instance_id: Uuid,
    session: Box<dyn AgentSession>,
    /// Whether this process was spawned with the computer-use tools.
    computer_tools: bool,
    /// Last moment the user or the provider touched this session. Idle
    /// release is measured from here.
    last_activity: std::sync::Mutex<SessionActivityTime>,
    /// Set once the daemon decided to close this process on purpose, so the
    /// provider's exit is not reported as a failure.
    released: AtomicBool,
    /// Owns outgoing background services; root responses cannot re-enter the thread.
    retained: AtomicBool,
    /// Forced stop owns cleanup and discards late provider events.
    stop_cleanup: AtomicBool,
    /// The turn currently executing, if any.
    turn: Mutex<Option<ActiveTurn>>,
    /// A settled Claude turn can receive native background-task continuations.
    /// Keep only the latest turn; a new user request replaces this context.
    continuation: Mutex<Option<ActiveTurn>>,
    turn_ready: Notify,
    /// Most recent parent turn. Provider task notifications can arrive after
    /// the parent reports completion.
    last_turn_id: Mutex<Option<TurnId>>,
    /// Latest provider-owned task state for targeted controls and checkpointing.
    tasks: Mutex<HashMap<String, RuntimeTask>>,
    /// Raw harness ids may repeat across accounts/processes. Only colliding ids
    /// acquire a public namespace; native controls translate back to their owner.
    task_aliases: Mutex<HashMap<String, String>>,
    /// Parent turns whose after-checkpoint waits for launched work to settle.
    deferred_checkpoints: Mutex<HashSet<TurnId>>,
    /// Approval id -> provider request id, for pending permission requests.
    pending: Mutex<HashMap<ApprovalId, String>>,
    /// Approvals the daemon asks for itself (computer use). They resolve here
    /// instead of in the provider, and keep their answer until the turn ends
    /// so a tool call retried after a slow answer sees it.
    daemon_approvals: Mutex<HashMap<ApprovalId, DaemonApproval>>,
    /// In-flight internal tool request ids. A bounded semaphore prevents one
    /// provider session from monopolizing daemon read work.
    app_tool_requests: Mutex<HashSet<String>>,
    app_tool_permits: Arc<Semaphore>,
    /// Project brief revision this session's instructions carry; only a Coordinator tracks it.
    brief_revision: Mutex<Option<i64>>,
    /// The Coordinator settings this session was last told; only a Coordinator tracks them.
    lead_settings: Mutex<Option<EffectiveCoordinatorSettings>>,
    /// The latest context report of this process, for the rotation budget.
    context_usage: std::sync::Mutex<Option<ContextUsage>>,
    /// A Cursor process told to send Kybern's instructions block with its first
    /// prompt; once a turn completes, its conversation is recorded as having it.
    instructions_unconfirmed: AtomicBool,
}

struct DaemonApproval {
    turn_id: TurnId,
    key: String,
    decision: tokio::sync::watch::Sender<Option<ApprovalDecision>>,
    /// The operation that used this card's one-time answer. Only a retry of that
    /// operation may reuse the card; any other call with the same key opens a new one.
    consumed_by: Option<Uuid>,
}

/// How a daemon-owned approval card was answered.
enum DaemonAnswer {
    Decided(ApprovalDecision),
    /// The turn ended (and denied the card) before the user answered.
    TurnEnded,
    /// No answer within the wait; the card stays open for a retry.
    Pending,
}

/// A Coordinator's copy of `message` with each child-result batch and thread
/// message flattened to text cut at `LEAD_COPY_BYTES`, or `None` when it holds
/// neither. Only the provider sees it; the stored message is untouched.
fn lead_capped_message(message: &UserMessage) -> Option<UserMessage> {
    let capped: Vec<Option<String>> = message.parts.iter().map(ContentPart::lead_orchestration_text).collect();
    if capped.iter().all(Option::is_none) {
        return None;
    }
    let parts =
        message.parts.iter().zip(capped).map(|(part, text)| text.map_or_else(|| part.clone(), |text| ContentPart::Text { text })).collect();
    Some(UserMessage { parts })
}

impl Orchestrator {
    /// `provider_message` for a thread's own session: a Coordinator's copy also
    /// has child results and thread messages cut to `LEAD_COPY_BYTES`, with a
    /// pointer to the rest.
    fn provider_message_for<'a>(&self, thread: &Thread, message: &'a UserMessage, live: &LiveSession) -> std::borrow::Cow<'a, UserMessage> {
        if thread.coordinator_project_id.is_some()
            && let Some(capped) = lead_capped_message(message)
        {
            return std::borrow::Cow::Owned(self.provider_message(&capped, live).into_owned());
        }
        self.provider_message(message, live)
    }

    /// The copy of a user message a provider receives. Kybern-only mentions become
    /// text: `@Computer` an instruction, a `kybern://note/<id>` mention the note, a
    /// `kybern://task/<id>` mention the task.
    /// Everything else passes through unchanged; the stored message keeps the chips.
    fn provider_message<'a>(&self, message: &'a UserMessage, live: &LiveSession) -> std::borrow::Cow<'a, UserMessage> {
        // The tools are in this session but not set up on this machine: they reach another one.
        let remote_only = live.computer_tools && !self.inner.computer.locally_offerable();
        let computer = crate::computer::ComputerUse::expand_mention(message, live.computer_tools, remote_only);
        let items = match self.expand_item_mentions(computer.as_ref().unwrap_or(message)) {
            Some(expanded) => std::borrow::Cow::Owned(expanded),
            None => match computer {
                Some(expanded) => std::borrow::Cow::Owned(expanded),
                None => std::borrow::Cow::Borrowed(message),
            },
        };
        let (store, assets) = (&self.inner.store, &self.inner.paths.assets);
        match crate::chips::expand(&items, |id| {
            let id: AssetId = id.parse().ok()?;
            store.asset_get(id).ok().flatten()?;
            Some(assets.join(id.to_string()))
        }) {
            Some(expanded) => std::borrow::Cow::Owned(expanded),
            None => items,
        }
    }
}

/// The file extension for an uploaded asset: its name's own (short and alphanumeric), else one for its media type.
fn asset_extension(name: &str, media_type: &str) -> Option<String> {
    let own = std::path::Path::new(name).extension().and_then(|ext| ext.to_str()).map(str::to_ascii_lowercase);
    if let Some(ext) = own.filter(|ext| (1..=5).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric())) {
        return Some(ext);
    }
    let ext = match media_type.split(';').next().unwrap_or("").trim() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/heic" => "heic",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        _ => return None,
    };
    Some(ext.into())
}

/// Computer-use consent for one tool call, bound to its owning turn.
struct ComputerConsent<'a> {
    orchestrator: &'a Orchestrator,
    thread_id: ThreadId,
    turn_id: TurnId,
    live: Arc<LiveSession>,
}

impl crate::computer::ConsentGate for ComputerConsent<'_> {
    fn ask(&self, request: crate::computer::ConsentRequest) -> futures::future::BoxFuture<'_, Result<crate::computer::ConsentAnswer>> {
        Box::pin(self.orchestrator.ask_computer_consent(self.thread_id, self.turn_id, &self.live, request))
    }

    fn cancelled(&self) -> futures::future::BoxFuture<'_, bool> {
        Box::pin(async move {
            self.live.is_released() || self.live.turn.lock().await.as_ref().is_none_or(|turn| turn.id != self.turn_id || turn.completed)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeTaskUpdateKind {
    Progress,
    Resume,
    Complete,
}

/// Idle expiry includes suspend time, which `Instant` does not count on macOS.
/// Keep both clocks so moving the wall clock backward cannot extend a session
/// beyond its normal awake-time limit. A forward clock change can expire an
/// idle process early; its conversation remains resumable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SessionActivityTime {
    monotonic: Instant,
    wall: SystemTime,
}

impl SessionActivityTime {
    fn now() -> Self {
        Self { monotonic: Instant::now(), wall: SystemTime::now() }
    }

    fn idle_for(self, now: Instant, wall_now: SystemTime) -> Duration {
        now.saturating_duration_since(self.monotonic).max(wall_now.duration_since(self.wall).unwrap_or_default())
    }
}

/// `meta` key recording that a provider conversation received Kybern's
/// instructions block (for harnesses that carry it in the conversation).
fn instructions_key(session_id: &str) -> String {
    format!("instructions_delivered:{session_id}")
}

/// Harnesses with no system channel: Kybern's instructions block is the start of the first prompt.
fn carries_instructions_in_conversation(kind: ProviderKind) -> bool {
    matches!(kind, ProviderKind::Cursor | ProviderKind::Antigravity | ProviderKind::GrokBuild)
}

/// How `send_with_id` took a message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Admission {
    /// It started a turn.
    Started,
    /// A retry of a message that already started a turn.
    Replayed,
    /// It waits in the thread's queue (a Coordinator's context is being refreshed).
    Queued,
}

/// Who is steering a turn. Only a person's message may restart the turn on a
/// newly selected target; Kybern's own deliveries (agent results, thread
/// messages) are queued for the next turn instead.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SteerSource {
    Client,
    Kybern,
}

struct ActiveTurn {
    id: TurnId,
    /// Distinguishes repeated native continuations of the same persisted turn.
    response_id: Uuid,
    /// Time after the harness was ready; retained for provider-reported turn duration fallback.
    started: std::time::Instant,
    /// Time immediately before `TurnStarted` was persisted, for end-to-end startup timing.
    startup_started: std::time::Instant,
    provider: ProviderKind,
    session_reused: bool,
    first_event_observed: bool,
    /// Provider origin + message id -> our MessageId, so root and child ids can
    /// never collide even if a provider reuses identifiers across threads.
    messages: HashMap<(EventOrigin, String), MessageId>,
    /// Logical message currently receiving deltas for each origin. Provider
    /// item ids are aliases: chunk and completion frames may disagree on them.
    active_messages: HashMap<EventOrigin, MessageId>,
    /// Last completed non-empty root message; persisted with TurnCompleted so
    /// clients never have to guess which assistant row is the final answer.
    terminal_message_id: Option<MessageId>,
    completed: bool,
    /// Claude Code may emit a successful foreground `result` while native
    /// background tasks are still active, then resume the root assistant from
    /// an internal task notification. Hold that provisional result so the
    /// continuation remains part of this turn and retains stable message ids.
    pending_completion: Option<PendingTurnCompletion>,
    /// A Kybern-internal turn (the rotation handoff): its output is collected
    /// here and never persisted, and clients never see the turn.
    internal: Option<rotation::InternalTurn>,
    /// The context-refresh seed this turn's message carried. It is cleared once the
    /// turn completes, so a provider that failed before reading it gets it again.
    rotation_seed: Option<rotation::RotationSeed>,
}

#[derive(Clone)]
struct PendingTurnCompletion {
    stop_reason: StopReason,
    usage: Usage,
    cost_usd: Option<f64>,
    duration_ms: u64,
    anchors: TurnAnchors,
}

impl PendingTurnCompletion {
    fn merge(&mut self, stop_reason: StopReason, usage: Usage, cost_usd: Option<f64>, duration_ms: u64, anchors: TurnAnchors) {
        self.stop_reason = stop_reason;
        self.usage.add(&usage);
        self.cost_usd = match (self.cost_usd, cost_usd) {
            (Some(left), Some(right)) => Some(left + right),
            (left, right) => left.or(right),
        };
        self.duration_ms = self.duration_ms.saturating_add(duration_ms);
        if anchors.turn_id.is_some() {
            self.anchors.turn_id = anchors.turn_id;
        }
        if anchors.previous_end.is_some() {
            self.anchors.previous_end = anchors.previous_end;
        }
    }
}

impl LiveSession {
    fn touch(&self) {
        *self.last_activity.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = SessionActivityTime::now();
    }

    fn last_activity(&self) -> SessionActivityTime {
        *self.last_activity.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn mark_released(&self) {
        self.released.store(true, Ordering::Relaxed);
    }

    fn is_released(&self) -> bool {
        self.released.load(Ordering::Relaxed)
    }
}

/// One agent process the daemon closed because nothing needed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionRelease {
    pub thread_id: ThreadId,
    pub reason: SessionReleaseReason,
}

/// Live-session counts for `daemon.activity` and the idle-exit decision.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionActivity {
    /// Threads with an agent process alive.
    pub live: usize,
    /// Live sessions with no turn, approval, or background work in progress.
    pub idle: usize,
}

pub const DEFAULT_TITLE: &str = "New thread";

impl Orchestrator {
    pub fn new(
        store: Store,
        drivers: DriverRegistry,
        events: crate::bounded_broadcast::Sender<ThreadEvent>,
        paths: Paths,
        settings: SettingsStore,
    ) -> Self {
        let app_tools = crate::app_tools::AppTools::new(store.clone(), crate::terminal::TerminalManager::default());
        let computer = crate::computer::ComputerUse::new(settings.clone());
        let catalog_file = paths.root.join("cache").join("provider-catalogs.json");
        let usage = crate::usage::UsageMonitor::new(store.clone(), settings.clone());
        let task_threads = store.task_run_thread_ids().map(|ids| ids.into_iter().collect::<HashSet<_>>()).unwrap_or_else(|error| {
            tracing::warn!(%error, "could not load the threads that run tasks");
            HashSet::new()
        });
        Self {
            inner: Arc::new(Inner {
                commands: std::sync::Mutex::new(()),
                session_admission: Mutex::new(HashMap::new()),
                checkpoint_locks: std::sync::Mutex::new(HashMap::new()),
                workspace_ops: Mutex::new(()),
                question_answers: Mutex::new(()),
                lead_commands: Mutex::new(()),
                client_thread_ids: std::sync::Mutex::new(HashMap::new()),
                thread_updates: std::sync::Mutex::new(()),
                store,
                drivers,
                events,
                paths,
                settings,
                provider_catalogs: Arc::new(crate::state::ProviderCatalogCache::with_disk(catalog_file)),
                app_tools,
                native_tools: None,
                computer,
                sessions: Mutex::new(HashMap::new()),
                retained_sessions: Mutex::new(HashMap::new()),
                releasing: Mutex::new(HashMap::new()),
                harness_gates: ProviderKind::ALL.into_iter().map(|kind| (kind, Arc::new(tokio::sync::RwLock::new(())))).collect(),
                pending_rewinds: Mutex::new(HashMap::new()),
                queue_wakeup: Notify::new(),
                shutting_down: std::sync::atomic::AtomicBool::new(false),
                notes_changed: tokio::sync::broadcast::channel(1024).0,
                tasks_changed: tokio::sync::broadcast::channel(1024).0,
                projects_changed: tokio::sync::broadcast::channel(64).0,
                spaces_changed: tokio::sync::broadcast::channel(64).0,
                workspace_progress: tokio::sync::broadcast::channel(256).0,
                outside_baselines: std::sync::Mutex::new(HashMap::new()),
                space_writes: std::sync::Mutex::new(()),
                preview_requests: tokio::sync::broadcast::channel(64).0,
                requests: Default::default(),
                previews: Arc::default(),
                usage,
                task_writes: std::sync::Mutex::new(()),
                task_threads: std::sync::Mutex::new(task_threads),
                task_published: std::sync::Mutex::new(HashMap::new()),
                agent_items: std::sync::Mutex::new(Default::default()),
                agent_item_ops: Mutex::new(()),
                subagents: std::sync::Mutex::new(Default::default()),
                delegation: Default::default(),
                remote: Default::default(),
                inbound: Default::default(),
                messaging: Default::default(),
                lead_git: Default::default(),
                rotation: Default::default(),
            }),
        }
    }

    /// Use the daemon's shared terminal registry for thread-bound app tools.
    /// Existing embedders keep an isolated empty registry by default.
    pub fn with_terminal_manager(mut self, terminals: crate::terminal::TerminalManager) -> Self {
        let store = self.inner.store.clone();
        Arc::get_mut(&mut self.inner).expect("terminal manager must be installed before cloning the orchestrator").app_tools =
            crate::app_tools::AppTools::new(store, terminals);
        self
    }

    pub fn with_previews(mut self, previews: Arc<crate::previews::tickets::PreviewTickets>) -> Self {
        Arc::get_mut(&mut self.inner).expect("preview tickets must be installed before cloning the orchestrator").previews = previews;
        self
    }

    /// Live state of the request cards, for `requests.progress` and `requests.act`.
    pub fn requests(&self) -> &crate::requests::RequestHub {
        &self.inner.requests
    }

    /// Agent requests to show a page, for forwarding to connected clients.
    pub fn subscribe_preview_requests(&self) -> tokio::sync::broadcast::Receiver<methods::PreviewOpenRequestedNotification> {
        self.inner.preview_requests.subscribe()
    }

    pub fn with_native_tools(mut self, native_tools: crate::native_tools_mcp::NativeToolsGateway) -> Self {
        Arc::get_mut(&mut self.inner).expect("native tools must be installed before cloning the orchestrator").native_tools =
            Some(native_tools);
        self
    }

    pub(crate) fn computer(&self) -> &crate::computer::ComputerUse {
        &self.inner.computer
    }

    pub(crate) fn usage(&self) -> &crate::usage::UsageMonitor {
        &self.inner.usage
    }

    fn revoke_native_session(&self, live: &LiveSession) {
        if let Err(error) = self.fail_session_subagent_messages(live.session_instance_id) {
            tracing::warn!(%error, "Unable to settle the closed native child inbox");
        }
        if let Some(gateway) = &self.inner.native_tools {
            gateway.revoke(live.session_instance_id);
        }
    }

    /// Resolves the next time a queued follow-up may be ready to dispatch.
    pub async fn queue_changed(&self) {
        self.inner.queue_wakeup.notified().await;
    }

    /// Nothing in the daemon or the provider is using this session: no turn,
    /// no pending approval, no provider-owned background work, and the thread
    /// is not marked busy in the store.
    async fn session_parked(&self, thread_id: ThreadId, live: &LiveSession) -> Result<bool> {
        // A context refresh owns the session between its handoff turns.
        if live.turn.lock().await.is_some() || self.is_rotating(thread_id) {
            return Ok(false);
        }
        if live.tasks.lock().await.values().any(|task| task.status.is_active()) {
            return Ok(false);
        }
        if !live.pending.lock().await.is_empty() {
            return Ok(false);
        }
        let Some(thread) = self.inner.store.thread_get(thread_id)? else { return Ok(true) };
        Ok(!matches!(thread.status, ThreadStatus::Running | ThreadStatus::AwaitingApproval))
    }

    /// Release only a parked session owned by this daemon. Never adopt or kill
    /// an unidentified process merely because it holds a provider writer lock.
    pub async fn release_session(&self, thread_id: ThreadId) -> Result<()> {
        let (live, done) = {
            let mut sessions = self.inner.sessions.lock().await;
            let live = sessions.get(&thread_id).cloned().ok_or_else(|| anyhow!("Kybern has no live agent process for this thread. If another app owns the conversation, close that session there and retry."))?;
            if !self.session_parked(thread_id, &live).await? {
                return Err(anyhow!("This agent has active work or approvals. Wait for them to finish before reconnecting."));
            }
            let (done, waiting) = tokio::sync::watch::channel(());
            self.inner.releasing.lock().await.insert(thread_id, waiting);
            sessions.remove(&thread_id);
            live.mark_released();
            self.revoke_native_session(&live);
            (live, done)
        };
        let this = self.clone();
        // Disconnecting the requesting client must not cancel cleanup.
        tokio::spawn(async move {
            let result = live.session.close().await;
            this.inner.releasing.lock().await.remove(&thread_id);
            drop(done);
            result?;
            this.emit(thread_id, None, EventPayload::ProviderSessionReleased { reason: SessionReleaseReason::Manual })?;
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    pub async fn session_activity(&self) -> Result<SessionActivity> {
        let snapshot: Vec<(ThreadId, Arc<LiveSession>)> =
            self.inner.sessions.lock().await.iter().map(|(id, live)| (*id, live.clone())).collect();
        let mut activity = SessionActivity { live: snapshot.len(), idle: 0 };
        for (thread_id, live) in &snapshot {
            if self.session_parked(*thread_id, live).await? {
                activity.idle += 1;
            }
        }
        Ok(activity)
    }

    /// Close agent processes that nothing needs: parked sessions idle for
    /// longer than the policy allows, then the least recently used parked
    /// sessions beyond the warm cap. Each closed thread resumes its provider
    /// conversation on the next message.
    ///
    /// Candidates are chosen without holding the session map, then claimed
    /// under it with the parked check repeated, so a message that arrived in
    /// between keeps its live process.
    ///
    /// With `saving_power` the host is on battery and the policy asks to
    /// spare it: every parked session is released after a short grace,
    /// and the warm cap does not apply.
    pub async fn release_idle_sessions(&self, policy: &BackgroundSettings, saving_power: bool) -> Result<Vec<SessionRelease>> {
        self.release_idle_sessions_at(policy, saving_power, Instant::now(), SystemTime::now()).await
    }

    async fn release_idle_sessions_at(
        &self,
        policy: &BackgroundSettings,
        saving_power: bool,
        now: Instant,
        wall_now: SystemTime,
    ) -> Result<Vec<SessionRelease>> {
        let (idle_limit, cap, idle_reason) = if saving_power {
            (Some(BackgroundSettings::BATTERY_SESSION_IDLE), None, SessionReleaseReason::Power)
        } else {
            (policy.session_idle(), policy.idle_session_cap(), SessionReleaseReason::Idle)
        };
        let snapshot: Vec<(ThreadId, Arc<LiveSession>)> =
            self.inner.sessions.lock().await.iter().map(|(id, live)| (*id, live.clone())).collect();
        let mut parked = Vec::new();
        for (thread_id, live) in snapshot {
            if self.session_parked(thread_id, &live).await? {
                let last_activity = live.last_activity();
                parked.push((thread_id, live, last_activity));
            }
        }
        parked.sort_by_key(|(_, _, last_activity)| last_activity.monotonic);
        let mut planned = Vec::new();
        let mut fresh = Vec::new();
        for (thread_id, live, last_activity) in parked {
            match idle_limit {
                Some(limit) if last_activity.idle_for(now, wall_now) >= limit => {
                    planned.push((thread_id, live, idle_reason, last_activity));
                }
                _ => fresh.push((thread_id, live, last_activity)),
            }
        }
        if let Some(cap) = cap
            && fresh.len() > cap
        {
            let excess = fresh.len() - cap;
            planned.extend(
                fresh
                    .drain(..excess)
                    .map(|(thread_id, live, last_activity)| (thread_id, live, SessionReleaseReason::Capacity, last_activity)),
            );
        }
        if planned.is_empty() {
            return Ok(Vec::new());
        }
        let mut claimed = Vec::new();
        let mut rotations = Vec::new();
        {
            let mut sessions = self.inner.sessions.lock().await;
            for (thread_id, live, reason, last_activity) in planned {
                if !sessions.get(&thread_id).is_some_and(|current| Arc::ptr_eq(current, &live)) {
                    continue;
                }
                if !self.session_parked(thread_id, &live).await? || live.last_activity() != last_activity {
                    // Work can start and finish while the sweep is selecting
                    // candidates. Give that newly quiet session its full grace.
                    continue;
                }
                // A Coordinator far into its budget writes a handoff before its
                // process closes, so the next message starts fresh.
                if self.idle_rotation_due(thread_id, &live) {
                    rotations.push((thread_id, live));
                    continue;
                }
                let (done, waiting) = tokio::sync::watch::channel(());
                self.inner.releasing.lock().await.insert(thread_id, waiting);
                sessions.remove(&thread_id);
                live.mark_released();
                self.revoke_native_session(&live);
                claimed.push((thread_id, live, reason, done));
            }
        }
        for (thread_id, live) in rotations {
            self.rotate_idle(thread_id, live);
        }
        // Once claimed, cleanup must finish even if the maintenance caller is
        // cancelled. Keep each resume barrier until its own process is closed.
        let closing = claimed
            .into_iter()
            .map(|(thread_id, live, reason, done)| {
                let this = self.clone();
                tokio::spawn(async move {
                    if let Err(error) = live.session.close().await {
                        tracing::warn!(%thread_id, %error, "agent process did not close cleanly");
                    }
                    this.inner.releasing.lock().await.remove(&thread_id);
                    drop(done);
                    tracing::info!(%thread_id, ?reason, "released idle agent process");
                    this.emit(thread_id, None, EventPayload::ProviderSessionReleased { reason })?;
                    Ok::<_, anyhow::Error>(SessionRelease { thread_id, reason })
                })
            })
            .collect::<Vec<_>>();
        let mut released = Vec::with_capacity(closing.len());
        for outcome in futures::future::join_all(closing).await {
            released.push(outcome??);
        }
        Ok(released)
    }

    /// The write guard prevents sends and one-shot jobs from racing an update.
    pub async fn idle_harness_for_update(&self, kind: ProviderKind) -> Result<Option<tokio::sync::OwnedRwLockWriteGuard<()>>> {
        let Ok(guard) = self.inner.harness_gates[&kind].clone().try_write_owned() else { return Ok(None) };
        let threads = self.inner.store.threads_list(None, true)?;
        for thread in threads.iter().filter(|thread| thread.provider.kind == kind) {
            if matches!(thread.status, ThreadStatus::Running | ThreadStatus::AwaitingApproval)
                || self.inner.store.runtime_tasks_for_thread(thread.id)?.iter().any(|task| task.status.is_active())
            {
                return Ok(None);
            }
        }
        // Resume the persisted conversation with the new executable on the next send.
        for thread in threads.iter().filter(|thread| thread.provider.kind == kind) {
            let live = self.inner.sessions.lock().await.remove(&thread.id);
            if let Some(live) = live {
                live.mark_released();
                self.revoke_native_session(&live);
                live.session.close().await?;
                self.emit(thread.id, None, EventPayload::ProviderSessionReleased { reason: SessionReleaseReason::Update })?;
            }
        }
        Ok(Some(guard))
    }

    /// Binary override for a provider from settings.
    fn binary_for(&self, kind: ProviderKind) -> Option<PathBuf> {
        self.inner.settings.get().providers.get(&kind).and_then(|p| p.binary.clone()).map(PathBuf::from)
    }

    /// Provider-owned tasks and turns cannot survive a daemon restart. Close
    /// both projections explicitly so clients never show immortal work.
    pub async fn recover_after_restart(&self) -> Result<()> {
        self.fail_all_subagent_messages("Not delivered — native session ended before delivery could be confirmed.")?;
        let threads = self.inner.store.threads_list(None, true)?;
        for t in &threads {
            let tasks = self.inner.store.runtime_tasks_for_thread(t.id)?;
            let mut checkpoint_turns = HashSet::new();
            for mut task in tasks.into_iter().filter(|task| task.status.is_active()) {
                task.status = RuntimeTaskStatus::Interrupted;
                task.detail = Some("Daemon restarted before this work finished".into());
                task.capabilities = RuntimeTaskCapabilities::default();
                task.updated_at = Utc::now();
                task.completed_at = Some(task.updated_at);
                checkpoint_turns.insert(task.origin_turn_id);
                self.emit(t.id, Some(task.origin_turn_id), EventPayload::RuntimeTaskCompleted { task: task.clone() })?;
                if t.subagent.is_none()
                    && let Err(error) = self.subagent_sync(t.id, &task, None)
                {
                    tracing::warn!(thread_id = %t.id, task_id = %task.id, %error, "could not settle the subagent thread");
                }
            }
            for turn_id in checkpoint_turns {
                if self.inner.store.checkpoint_get(turn_id)?.is_some_and(|checkpoint| checkpoint.after.is_none()) {
                    self.checkpoint(t, turn_id, "after").await;
                }
            }
        }
        for mut t in self.inner.store.threads_running()? {
            let last_turn = self.inner.store.events_for_thread(t.id)?.iter().rev().find_map(|e| match e.payload {
                EventPayload::TurnStarted { .. } => e.turn_id,
                _ => None,
            });
            for a in self.inner.store.approvals_pending(Some(t.id))? {
                let decision = ApprovalDecision::Deny { reason: Some("daemon restarted".into()) };
                self.inner.store.approval_resolve(a.id, &decision)?;
                self.emit(t.id, Some(a.turn_id), EventPayload::ApprovalResolved { approval_id: a.id, decision, answered_by: None })?;
            }
            // A restart is not the agent's failure: end the turn as interrupted and
            // leave the thread idle so its queue keeps draining.
            self.emit(
                t.id,
                last_turn,
                EventPayload::TurnCompleted {
                    stop_reason: StopReason::Interrupted,
                    usage: Usage::default(),
                    cost_usd: None,
                    duration_ms: 0,
                    terminal_message_id: None,
                },
            )?;
            self.emit(
                t.id,
                last_turn,
                EventPayload::ProviderNotice {
                    level: NoticeLevel::Warning,
                    text: "Kybern restarted while this turn was running. Send a message to continue.".into(),
                    data: None,
                },
            )?;
            t.status = ThreadStatus::Idle;
            self.update_thread(t)?;
        }
        self.delegation_recover_after_restart()?;
        self.messaging_recover_after_restart().await?;
        self.rotation_recover_after_restart().await?;
        Ok(())
    }

    /// The daemon is stopping. A signal that stops the daemon reaches its agents too (a terminal's
    /// Ctrl+C, systemd stopping the unit), and an agent stopped that way reports a raw failure.
    /// From here on such a turn ends as interrupted, with a notice that says why.
    pub fn begin_shutdown(&self) {
        self.inner.shutting_down.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn shutting_down(&self) -> bool {
        self.inner.shutting_down.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub async fn shutdown(&self) {
        let mut sessions: Vec<_> = self.inner.sessions.lock().await.drain().collect();
        sessions.extend(self.inner.retained_sessions.lock().await.drain().map(|((thread_id, _), (_, live))| (thread_id, live)));
        for (_, live) in &sessions {
            live.mark_released();
            self.revoke_native_session(live);
        }
        let _ = futures::future::join_all(sessions.iter().map(|(_, live)| live.session.close())).await;
        self.inner.computer.shutdown().await;
    }

    // ---- persistence helpers ----

    pub(crate) fn emit(&self, thread_id: ThreadId, turn_id: Option<TurnId>, payload: EventPayload) -> Result<ThreadEvent> {
        // For a task run's thread the task lock spans the append and the task update,
        // so its events reach the task in the order they were stored.
        let task_order = self.task_event_guard(thread_id, &payload);
        let ev = self.inner.store.event_append(thread_id, turn_id, payload)?;
        if matches!(
            ev.payload,
            EventPayload::MessageQueued { .. } | EventPayload::ThreadUpdated { .. } | EventPayload::RuntimeTaskCompleted { .. }
        ) {
            self.inner.queue_wakeup.notify_one();
        }
        let _ = self.inner.events.send(ev.clone());
        // A move that waited for this thread's turn to end can go now.
        if let EventPayload::ThreadUpdated { thread } = &ev.payload {
            self.move_thread_idle(thread);
        }
        if matches!(
            ev.payload,
            EventPayload::ApprovalRequested { .. } | EventPayload::UserInputRequested { .. } | EventPayload::ApprovalResolved { .. }
        ) {
            self.track_child_question(&ev);
        }
        if task_order.is_some() {
            self.track_task_run(&ev);
        }
        drop(task_order);
        Ok(ev)
    }

    fn update_thread(&self, thread: Thread) -> Result<Thread> {
        antigravity_rules::require_full_access(thread.provider.kind, thread.permission_mode)?;
        if thread.coordinator_project_id.is_some() {
            antigravity_rules::refuse_lead(thread.provider.kind)?;
        }
        let _updates = self.inner.thread_updates.lock().map_err(|_| anyhow!("thread update lock poisoned"))?;
        self.write_thread_update(thread, true)
    }

    /// Persist and publish a thread. The caller holds `thread_updates`. With
    /// `keep_stored_delegation` a stale copy cannot roll a delegation back:
    /// only [`Orchestrator::delegation_update`] changes it.
    fn write_thread_update(&self, mut thread: Thread, keep_stored_delegation: bool) -> Result<Thread> {
        if let Some(current) = self.inner.store.thread_get(thread.id)? {
            // A late provider completion must not unarchive a thread and allow the
            // daemon queue worker to resume it while its session is being closed.
            if current.status == ThreadStatus::Archived {
                thread.status = ThreadStatus::Archived;
                // Archiving freed a Coordinator's slot; a stale copy must not take it back.
                thread.coordinator_project_id = current.coordinator_project_id;
            }
            if keep_stored_delegation {
                thread.delegation = current.delegation;
            }
            // Only the move code changes the lease; a stale copy must not undo it.
            thread.moved = current.moved;
        }
        thread.updated_at = Utc::now();
        self.inner.store.thread_upsert(&thread)?;
        let ev = self.emit(thread.id, None, EventPayload::ThreadUpdated { thread: thread.clone() })?;
        thread.last_seq = ev.seq;
        // Keep the stored last_seq in sync with what we just emitted.
        self.inner.store.thread_upsert(&thread)?;
        Ok(thread)
    }

    // ---- projects ----

    /// Git may have been initialized or removed since registration. Probe the
    /// project checkout itself: a thread can be running in a separate worktree.
    pub async fn refresh_project_git(&self, project_id: ProjectId) -> Result<()> {
        if let Some(project) = self.inner.store.project_get(project_id)? {
            if project.kind == ProjectKind::Group {
                if self.refresh_group_members(&project)? {
                    self.publish_projects();
                }
                return Ok(());
            }
            let is_git = Repo::is_repo(std::path::Path::new(&project.path)).await;
            if self.inner.store.project_set_git_status(project.id, is_git)? {
                self.publish_projects();
            }
        }
        Ok(())
    }

    /// Reconcile at boot and during the existing connected-client sweep, even
    /// when no thread exists yet to issue `git.status`. Publish once per batch.
    pub async fn refresh_projects_git(&self) -> Result<Vec<Project>> {
        let mut changed = false;
        let projects = self.inner.store.projects_list()?;
        // A list read must not wait for one git subprocess per project in
        // sequence. Bound process concurrency, and keep store writes/publishing
        // on this task after every batch completes.
        // Group folders are not repositories themselves; their members are probed instead.
        let (groups, projects): (Vec<_>, Vec<_>) = projects.into_iter().partition(|project| project.kind == ProjectKind::Group);
        for project in &groups {
            changed |= self.refresh_group_members(project)?;
        }
        for project in &projects {
            let plain = kybern_git::simplify_path(std::path::Path::new(&project.path)).to_string_lossy().into_owned();
            if plain != project.path && self.inner.store.project_by_path(&plain)?.is_none() {
                let mut fixed = project.clone();
                fixed.path = plain;
                self.inner.store.project_update(&fixed)?;
                changed = true;
            }
        }
        let projects =
            self.inner.store.projects_list()?.into_iter().filter(|project| project.kind != ProjectKind::Group).collect::<Vec<_>>();
        for batch in projects.chunks(8) {
            let probes = batch.iter().map(|project| async { (project.id, Repo::is_repo(std::path::Path::new(&project.path)).await) });
            for (id, is_git) in futures::future::join_all(probes).await {
                if self.inner.store.project_set_git_status(id, is_git)? {
                    changed = true;
                    // A folder that became (or stopped being) a repository has a new identity.
                    self.spawn_repo_identity(id);
                }
            }
        }
        if changed {
            self.publish_projects();
        }
        self.inner.store.projects_list()
    }

    pub fn add_project(&self, path: String, name: Option<String>, space_id: Option<SpaceId>) -> Result<Project> {
        let p = PathBuf::from(&path);
        // `canonicalize` returns `\\?\D:\x` on Windows; the project is stored and shown as `D:\x`.
        let p = kybern_git::simplify_path(&p.canonicalize().with_context(|| format!("project path {path} does not exist"))?);
        if !p.is_dir() {
            return Err(anyhow!("{} is not a directory", p.display()));
        }
        let path = p.to_string_lossy().to_string();
        if let Some(space_id) = space_id
            && self.inner.store.space_get(space_id)?.is_none()
        {
            return Err(anyhow!("not found: space"));
        }
        // A project registered before the prefix was dropped keeps working: it is found by its old path and renamed.
        let verbatim = format!(r"\\?\{path}");
        let found = match self.inner.store.project_by_path(&path)? {
            Some(existing) => Some(existing),
            None => self.inner.store.project_by_path(&verbatim)?,
        };
        if let Some(mut existing) = found {
            if existing.path != path && existing.kind != ProjectKind::Group {
                existing.path = path.clone();
                existing.updated_at = Utc::now();
                self.inner.store.project_update(&existing)?;
                self.publish_projects();
            }
            // Self-heal a stale cached flag: the folder may have gained (or lost) a
            // repo since it was first registered. Re-probe and persist any change so
            // edit/integration spawns are not permanently blocked after a `git init`.
            if existing.kind == ProjectKind::Group {
                return Ok(existing);
            }
            let live = project_path_is_git(&existing.path);
            if live != existing.is_git {
                existing.is_git = live;
                existing.updated_at = Utc::now();
                self.inner.store.project_update(&existing)?;
                self.publish_projects();
                self.spawn_repo_identity(existing.id);
            }
            return Ok(existing);
        }
        let now = Utc::now();
        let project = Project {
            repo: None,
            repos: Vec::new(),
            id: Uuid::now_v7(),
            name: name.unwrap_or_else(|| p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| path.clone())),
            is_git: project_path_is_git(&path),
            path,
            worktrees_default: None,
            task_prefix: None,
            brief_in_all_threads: false,
            coordinator: None,
            space_id,
            created_at: now,
            updated_at: now,
            kind: Default::default(),
            members: Vec::new(),
            worktree_copy: Vec::new(),
            instructions_offer_dismissed: false,
        };
        if let Err(error) = self.inner.store.project_insert(&project) {
            // Concurrent imports may discover the same folder before either
            // inserts it. Reuse the winner of the unique-path constraint.
            if let Some(existing) = self.inner.store.project_by_path(&project.path)? {
                return Ok(existing);
            }
            return Err(error);
        }
        self.publish_projects();
        self.spawn_repo_identity(project.id);
        // The store assigns the task prefix on insert; return the project as stored.
        Ok(self.inner.store.project_get(project.id)?.unwrap_or(project))
    }

    /// Worktree preparation progress of thread creates, for forwarding to connected clients.
    pub fn subscribe_workspace_progress(&self) -> tokio::sync::broadcast::Receiver<methods::ThreadsWorkspaceProgress> {
        self.inner.workspace_progress.subscribe()
    }

    /// Changes to the project list, for forwarding to connected clients.
    pub fn subscribe_projects(&self) -> tokio::sync::broadcast::Receiver<methods::ProjectsChangedNotification> {
        self.inner.projects_changed.subscribe()
    }

    /// Tell clients the project list changed. Each notification carries the whole
    /// list, so a client that misses one catches up with the next.
    pub fn publish_projects(&self) {
        match self.inner.store.projects_list() {
            // No receivers just means no client is connected.
            Ok(projects) => {
                let _ = self.inner.projects_changed.send(methods::ProjectsChangedNotification { projects });
            }
            Err(error) => tracing::warn!(%error, "could not read projects to announce a change"),
        }
    }

    /// Read a project's repository identity in the background and announce it when it changed
    /// (design §4.4). A project that is just being created does not wait for git.
    pub fn spawn_repo_identity(&self, id: ProjectId) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        let this = self.clone();
        runtime.spawn(async move {
            let Ok(Some(project)) = this.inner.store.project_get(id) else { return };
            if crate::network::repo_identity::refresh(&this.inner.store, &project).await {
                this.publish_projects();
            }
        });
    }

    // ---- threads ----

    pub async fn resume_external_session(&self, params: methods::SessionsResumeParams) -> Result<Thread> {
        let settings = self.inner.settings.get();
        let source_project =
            params.project_id.map(|id| self.inner.store.project_get(id)?.ok_or_else(|| anyhow!("Project not found"))).transpose()?;
        let raw_provider = settings.providers.get(&params.provider).cloned().unwrap_or_default();
        let instance = crate::provider_accounts::resolve(&raw_provider, source_project.as_ref().map(|project| project.path.as_str()), None);
        let existing = self.inner.store.threads_list(None, true)?.into_iter().find(|thread| {
            thread.provider.kind == params.provider
                && thread.provider.instance == instance
                && thread.provider_session_id.as_deref() == Some(params.session_id.as_str())
        });
        if let Some(mut thread) = existing {
            if thread.status == ThreadStatus::Archived {
                thread.status = ThreadStatus::Idle;
                self.inner.store.thread_upsert(&thread)?;
                thread = self.update_thread(thread)?;
            }
            return Ok(thread);
        }
        let _gate = self.inner.harness_gates.get(&params.provider).ok_or_else(|| anyhow!("Harness unavailable"))?.read().await;
        let driver = self.inner.drivers.get(params.provider).ok_or_else(|| anyhow!("Harness unavailable"))?;
        let provider = crate::settings::provider_settings(&settings, params.provider, source_project.as_ref().map(|p| p.path.as_str()));
        let context = kybern_drivers::ProbeContext {
            binary: provider.binary.map(PathBuf::from),
            cwd: source_project.as_ref().map(|p| PathBuf::from(&p.path)),
            env: provider.env,
        };
        let imported_profile =
            if params.provider == ProviderKind::Omp { Some(kybern_drivers::omp_profile::resolve(&context.env)?) } else { None };
        let history = driver.read_session(&context, &params.session_id).await?;
        let session = &history.session;
        if session.provider != params.provider || session.id != params.session_id {
            return Err(anyhow!("The harness returned a different session. Refresh the session list and try again."));
        }
        let cwd = PathBuf::from(&session.cwd);
        if !cwd.is_absolute() || !cwd.is_dir() {
            return Err(anyhow!(
                "The session folder is unavailable: {}. Restore it or move the session in its harness, then try again.",
                session.cwd
            ));
        }
        let project = self.add_project(session.cwd.clone(), None, None)?;
        let (mut thread, _created) = self
            .adopt_session_history(
                AdoptSession { project, instance, history, imported_profile, updated_at: Utc::now(), announce_history: true },
                &settings,
            )
            .await?;
        if thread.status == ThreadStatus::Archived {
            thread.status = ThreadStatus::Idle;
            self.inner.store.thread_upsert(&thread)?;
            thread = self.update_thread(thread)?;
        }
        Ok(thread)
    }

    /// Turn a native session's history into a Kybern thread of `project`, in one store
    /// transaction. `created` is false when the session was already imported (the
    /// existing thread comes back untouched). Starts no turn and sends no prompt.
    pub(crate) async fn adopt_session_history(&self, adopt: AdoptSession, settings: &Settings) -> Result<(Thread, bool)> {
        let AdoptSession { project, instance, history, imported_profile, updated_at, announce_history } = adopt;
        let session = history.session;
        // A session in a group folder joins the group like a local chat: every member. Its cwd stays the
        // session's own spelling, which the harness resumes by.
        let members = if project.kind == ProjectKind::Group {
            self.group_local_members(&project, &std::collections::BTreeMap::new()).await?
        } else {
            Vec::new()
        };
        let now = Utc::now();
        let thread = Thread {
            id: Uuid::now_v7(),
            project_id: project.id,
            title: if session.title.trim().is_empty() { DEFAULT_TITLE.into() } else { session.title },
            model: session.model,
            effort: None,
            provider: ProviderInstance { kind: session.provider, instance: instance.clone() },
            permission_mode: if session.provider == ProviderKind::Antigravity {
                // Antigravity only runs in Full access; resuming in another mode is refused.
                PermissionMode::FullAccess
            } else if session.id.starts_with("cursor-sdk:") {
                default_provider_permission(session.provider, settings.default_permission_mode)
            } else {
                settings.default_permission_mode
            },
            status: ThreadStatus::Idle,
            worktree: None,
            cwd: session.cwd,
            provider_session_id: Some(session.id),
            pinned: false,
            created_at: history.events.first().map_or(now, |event| event.at),
            updated_at,
            last_seq: 0,
            parent_thread_id: None,
            coordinator_project_id: None,
            origin: None,
            inbound: None,
            moved: None,
            subagent: None,
            delegation: None,
            members,
        };
        let store = self.inner.store.clone();
        let (thread, events) = tokio::task::spawn_blocking(move || store.thread_import(thread, history.events)).await??;
        if events.is_empty() {
            // Another import adopted this session first.
            return Ok((thread, false));
        }
        // Imported native ids belong to the discovery account, even if global
        // defaults change later or the session cwd uses a different project.
        self.inner.store.meta_set(&format!("account_override:{}", thread.id), &instance)?;
        self.save_account_binding(&thread)?;
        if let Some(profile) = imported_profile {
            self.inner.store.meta_set(&format!("omp_profile:{}", thread.id), &profile)?;
        }
        // A bulk import of hundreds of chats would otherwise stream every past turn to every window, mark each
        // thread unread on its last `turn_completed`, and can overflow the broadcast's byte budget (`events.lagged`).
        let last = events.len().saturating_sub(1);
        for (index, event) in events.into_iter().enumerate() {
            if announce_history || index == 0 || index == last {
                let _ = self.inner.events.send(event);
            }
        }
        Ok((thread, true))
    }

    /// Hold back harness updates while a helper process of `kind` reads sessions.
    pub(crate) async fn harness_read_gate(&self, kind: ProviderKind) -> Option<tokio::sync::OwnedRwLockReadGuard<()>> {
        Some(self.inner.harness_gates.get(&kind)?.clone().read_owned().await)
    }

    pub async fn create_thread(&self, params: methods::ThreadsCreateParams) -> Result<Thread> {
        let Some(id) = params.thread_id else { return self.create_thread_with_id(params, Uuid::now_v7()).await };
        self.with_client_thread_id(id, async {
            match self.client_thread(id, params.project_id.unwrap_or(FREE_CHAT_PROJECT_ID))? {
                Some(existing) => self.finish_client_create(existing, params.message, params.message_id).await,
                None => self.create_thread_with_id(params, id).await,
            }
        })
        .await
    }

    /// Run `work` for a client-chosen thread id with other calls for that id held back, so a
    /// repeat that arrives while the first is still creating the thread waits and then finds it.
    async fn with_client_thread_id<T>(&self, id: ThreadId, work: impl Future<Output = Result<T>>) -> Result<T> {
        if id.is_nil() {
            return Err(anyhow!("thread_id must be a UUID other than the nil UUID"));
        }
        let lock = self.inner.client_thread_ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).entry(id).or_default().clone();
        let guard = lock.clone().lock_owned().await;
        let result = work.await;
        drop(guard);
        drop(lock);
        let mut locks = self.inner.client_thread_ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if locks.get(&id).is_some_and(|lock| Arc::strong_count(lock) == 1) {
            locks.remove(&id);
        }
        result
    }

    /// The thread an earlier create with this client id made, if any. An id that belongs to
    /// another kind of thread, or to one in another project, is refused rather than reused.
    pub(crate) fn client_thread(&self, id: ThreadId, project_id: ProjectId) -> Result<Option<Thread>> {
        let Some(existing) = self.inner.store.thread_get(id)? else { return Ok(None) };
        if existing.project_id != project_id
            || existing.subagent.is_some()
            || existing.delegation.is_some()
            || existing.parent_thread_id.is_some()
        {
            return Err(anyhow!("thread_id already belongs to a different thread. Start the chat with a new id."));
        }
        if existing.status == ThreadStatus::Archived {
            return Err(anyhow!("This chat was archived. Start a new chat instead."));
        }
        Ok(Some(existing))
    }

    /// A repeated create: the thread exists, so only the first message may still be missing. With
    /// its client id the send is answered from the first delivery when there was one; without it,
    /// the message goes only to a thread that has never started a turn.
    async fn finish_client_create(&self, thread: Thread, message: Option<UserMessage>, message_id: Option<MessageId>) -> Result<Thread> {
        let Some(message) = message else { return Ok(thread) };
        match message_id {
            Some(message_id) => {
                self.send_with_id(thread.id, message_id, message, false, true).await?;
            }
            None => {
                let started = self
                    .inner
                    .store
                    .events_for_thread(thread.id)?
                    .iter()
                    .any(|event| matches!(event.payload, EventPayload::TurnStarted { .. }));
                if !started {
                    self.send(thread.id, message).await?;
                }
            }
        }
        Ok(self.inner.store.thread_get(thread.id)?.unwrap_or(thread))
    }

    async fn create_thread_with_id(&self, params: methods::ThreadsCreateParams, id: ThreadId) -> Result<Thread> {
        let free_chat = params.project_id.is_none();
        let mut project = match params.project_id {
            Some(project_id) => self.inner.store.project_get(project_id)?.ok_or_else(|| anyhow!("project not found"))?,
            None => self.ensure_free_chat_project()?,
        };
        let settings = self.inner.settings.get();
        let permission_mode =
            params.permission_mode.unwrap_or_else(|| default_provider_permission(params.provider.kind, settings.default_permission_mode));
        // Before anything is created: Antigravity only runs in Full access.
        antigravity_rules::require_full_access(params.provider.kind, permission_mode)?;
        let configured_model = settings.providers.get(&params.provider.kind).and_then(|provider| provider.model.clone());
        let selected_model = params.model.as_deref().or(configured_model.as_deref());
        self.validate_provider_selection(project.id, &params.provider, selected_model, params.effort.as_deref()).await?;
        if free_chat && (params.use_worktree == Some(true) || params.base_branch.is_some()) {
            return Err(anyhow!("free chats cannot use a worktree or branch"));
        }
        let use_worktree =
            if free_chat { false } else { params.use_worktree.or(project.worktrees_default).unwrap_or(settings.worktrees_default) };
        let group = project.kind == ProjectKind::Group;
        if use_worktree && !project.is_git && !group {
            // Re-probe a stale cached flag before refusing (see `project_path_is_git`);
            // a folder `git init`-ed after registration must still support worktrees.
            if project_path_is_git(&project.path) {
                project.is_git = true;
                project.updated_at = Utc::now();
                self.inner.store.project_update(&project)?;
                self.publish_projects();
            } else {
                return Err(anyhow!(
                    "This project is not a git repository, so it can't use a worktree. Run git init in it, or start the chat without a worktree."
                ));
            }
        }
        let now = Utc::now();
        let base_branch = params.base_branch.clone().map(|b| b.trim().to_string()).filter(|b| !b.is_empty());
        let mut members = Vec::new();
        let worktree = if group {
            // `base_branch` is the primary's when the map does not name it.
            let mut bases: std::collections::BTreeMap<String, String> = params
                .base_branches
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|(path, branch)| (path, branch.trim().to_string()))
                .filter(|(_, branch)| !branch.is_empty())
                .collect();
            if let (Some(branch), Some(primary)) = (base_branch.clone(), project.members.first()) {
                bases.entry(primary.path.clone()).or_insert(branch);
            }
            if use_worktree {
                let selected = group_git::select_members(&project, params.members.as_deref())?;
                let (worktree, snapshot) = self.create_group_workspace(&project, id, selected, &bases, None).await?;
                members = snapshot;
                Some(worktree)
            } else {
                members = self.group_local_members(&project, &bases).await?;
                None
            }
        } else if use_worktree {
            Some(self.create_worktree(&project, id, base_branch.as_deref()).await?)
        } else {
            if let Some(branch) = base_branch.as_deref() {
                let repo = Repo::new(&project.path);
                if repo.current_branch().await.as_deref() != Some(branch) {
                    repo.switch(branch)
                        .await
                        .map_err(|e| anyhow!("could not switch {} to {branch}: {e}. Commit or stash your changes first", project.name))?;
                }
            }
            None
        };
        let cwd = worktree.as_ref().map(|w| w.path.clone()).unwrap_or_else(|| project.path.clone());
        let thread = Thread {
            id,
            project_id: project.id,
            title: params.title.clone().unwrap_or_else(|| DEFAULT_TITLE.to_string()),
            model: params.model.or(configured_model),
            effort: params.effort,
            permission_mode,
            provider: params.provider,
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
            origin: None,
            inbound: None,
            moved: None,
            subagent: None,
            delegation: None,
            members,
        };
        self.inner.store.thread_upsert(&thread)?;
        if params.pin_account || thread.provider.instance != "default" {
            self.inner.store.meta_set(&format!("account_override:{}", thread.id), &thread.provider.instance)?;
        }

        let ev = self.emit(thread.id, None, EventPayload::ThreadCreated { thread: thread.clone() })?;
        let mut thread = Thread { last_seq: ev.seq, ..thread };
        self.inner.store.thread_upsert(&thread)?;
        if let Some(message) = params.message {
            let retryable = params.message_id.is_some();
            self.send_with_id(thread.id, params.message_id.unwrap_or_else(Uuid::now_v7), message, false, retryable).await?;
            thread = self.inner.store.thread_get(thread.id)?.unwrap_or(thread);
        }
        Ok(thread)
    }

    fn ensure_free_chat_project(&self) -> Result<Project> {
        if let Some(project) = self.inner.store.project_get(FREE_CHAT_PROJECT_ID)? {
            return Ok(project);
        }
        let path = self.inner.paths.root.join("free-chat");
        std::fs::create_dir_all(&path)?;
        let now = Utc::now();
        let project = Project {
            repo: None,
            repos: Vec::new(),
            id: FREE_CHAT_PROJECT_ID,
            name: "Free chats".into(),
            path: path.to_string_lossy().into_owned(),
            is_git: false,
            worktrees_default: Some(false),
            task_prefix: None,
            brief_in_all_threads: false,
            coordinator: None,
            space_id: None,
            created_at: now,
            updated_at: now,
            kind: Default::default(),
            members: Vec::new(),
            worktree_copy: Vec::new(),
            instructions_offer_dismissed: false,
        };
        match self.inner.store.project_insert(&project) {
            Ok(()) => {
                self.publish_projects();
                Ok(project)
            }
            Err(error) => self.inner.store.project_get(FREE_CHAT_PROJECT_ID)?.ok_or(error),
        }
    }

    /// `<worktrees>/<project label>/<id>`. The label is only for people browsing the
    /// folder: it is one sanitized path component, and ownership checks go through
    /// `is_managed_worktree`, so renaming a project never orphans its worktrees.
    pub(crate) fn worktree_dir(&self, project: &Project, id: &str) -> PathBuf {
        self.inner.paths.worktrees.join(worktree_label(&project.name)).join(id)
    }

    /// True for a folder `worktree_dir` could have made for `id`, under any project label.
    pub(crate) fn is_managed_worktree(&self, path: &Path, id: &str) -> bool {
        path.file_name().is_some_and(|name| name == id)
            && path.parent().and_then(Path::parent) == Some(self.inner.paths.worktrees.as_path())
    }

    async fn create_worktree(&self, project: &Project, thread_id: ThreadId, base: Option<&str>) -> Result<WorktreeInfo> {
        let unique = thread_id.to_string();
        let branch = format!("kybern/{unique}");
        let dir = self.worktree_dir(project, &unique);
        std::fs::create_dir_all(dir.parent().unwrap())?;
        self.send_workspace_progress(thread_id, 0, 1, None);
        let repo = Repo::new(&project.path);
        let (had_branch, had_dir) = (repo.rev_parse(&format!("refs/heads/{branch}")).await.is_ok(), dir.exists());
        let added = repo.worktree_add(&dir, &branch, base).await;
        self.send_workspace_progress(thread_id, u32::from(added.is_ok()), 1, None);
        if let Err(error) = added {
            // `git worktree add` can fail after making the branch and folder (a checkout hook, LFS, a full
            // disk). Undo what this call made, so Retry with the same thread id can start again.
            if !had_dir && dir.exists() {
                let _ = repo.worktree_remove(&dir, true).await;
                let _ = std::fs::remove_dir_all(&dir);
                let _ = repo.worktree_prune().await;
            }
            if !had_branch {
                let _ = repo.delete_branch(&branch, true).await;
            }
            return Err(error);
        }
        Ok(WorktreeInfo { path: dir.to_string_lossy().to_string(), branch })
    }

    pub fn update_thread_fields(&self, params: methods::ThreadsUpdateParams) -> Result<Thread> {
        let mut t = self.inner.store.thread_get(params.thread_id)?.ok_or_else(|| anyhow!("thread not found"))?;
        if t.subagent.is_some() {
            return Err(anyhow!(subagents::READ_ONLY_ERROR));
        }
        if let Some(title) = params.title {
            t.title = title;
        }
        if let Some(p) = params.pinned {
            t.pinned = p;
        }
        if let Some(m) = params.permission_mode {
            t.permission_mode = m;
        }
        if let Some(m) = params.model {
            t.model = Some(m);
        }
        if let Some(effort) = params.effort {
            t.effort = Some(effort);
        }
        self.update_thread(t)
    }

    /// Archive a thread and, with it, every agent it delegated work to. Running
    /// children are stopped first; delegated worktrees are removed when safe.
    pub async fn archive_thread(&self, thread_id: ThreadId) -> Result<()> {
        self.inner.store.thread_get(thread_id)?.ok_or_else(|| anyhow!("thread not found"))?;
        // The parent goes first (flagged, then archived with its session closed) so
        // it cannot delegate more; then the cascade, rescanned until nothing runs.
        let _stopping = self.delegation_flag_stopping(thread_id);
        self.delegation_creation_barrier(thread_id).await;
        self.delegation_mark_cancelled(thread_id, true)?;
        self.archive_single(thread_id).await?;
        self.delegation_stop_descendants(thread_id).await;
        let descendants = self.delegation_descendants(thread_id)?;
        for descendant in &descendants {
            if descendant.status != ThreadStatus::Archived
                && let Err(error) = self.archive_single(descendant.id).await
            {
                tracing::warn!(thread_id = %descendant.id, %error, "could not archive a delegated agent");
            }
        }
        for id in descendants.iter().map(|descendant| descendant.id).chain([thread_id]) {
            self.delegation_cleanup_on_archive(id).await;
        }
        self.cleanup_eligible_worktrees().await;
        Ok(())
    }

    async fn archive_single(&self, thread_id: ThreadId) -> Result<()> {
        {
            let _command = self.inner.commands.lock().map_err(|_| anyhow!("command lock poisoned"))?;
            let mut t = self.inner.store.thread_get(thread_id)?.ok_or_else(|| anyhow!("thread not found"))?;
            // Archiving a project's Coordinator frees its slot: a new one can be created.
            t.coordinator_project_id = None;
            t.status = ThreadStatus::Archived;
            self.update_thread(t)?;
            self.inner.previews.revoke_thread(thread_id);
            self.emit(thread_id, None, EventPayload::ThreadArchived)?;
            self.subagents_archive_below(thread_id)?;
        }
        // Machines this thread ran tools on archive their inbound threads for it.
        self.archive_forwarded(thread_id);
        self.close_retained_sessions(thread_id).await?;
        if let Some(live) = self.inner.sessions.lock().await.remove(&thread_id) {
            live.mark_released();
            self.revoke_native_session(&live);
            let _ = live.session.close().await;
        }
        Ok(())
    }

    pub async fn send(&self, thread_id: ThreadId, message: UserMessage) -> Result<(TurnId, MessageId)> {
        let (turn_id, message_id, _admission) = self.send_with_id(thread_id, Uuid::now_v7(), message, false, false).await?;
        Ok((turn_id, message_id))
    }

    pub async fn send_client_message(&self, params: methods::ThreadsSendParams) -> Result<methods::ThreadsSendResult> {
        let retryable = params.message_id.is_some();
        let message_id = params.message_id.unwrap_or_else(Uuid::now_v7);
        let (turn_id, message_id, admission) = self.send_with_id(params.thread_id, message_id, params.message, false, retryable).await?;
        Ok(methods::ThreadsSendResult { turn_id, message_id, queued: admission == Admission::Queued })
    }

    /// Returns the turn the message started (nil when it was queued) and how it was taken.
    async fn send_with_id(
        &self,
        thread_id: ThreadId,
        message_id: MessageId,
        mut message: UserMessage,
        queued: bool,
        retryable: bool,
    ) -> Result<(TurnId, MessageId, Admission)> {
        // The daemon-wide workspace lock serializes worktree and checkout operations across threads. A send needs it only
        // when it may restore or work inside a managed worktree, or apply a permission change; otherwise another thread's
        // worktree removal or PR checkout would hold this message back for its whole git run. The thread's own admission
        // gate still orders the send against operations on this thread. Lock order stays workspace, then admission.
        let mut _workspace = None;
        let (_admission, current, selected_target, selected_mode) = loop {
            let admission = self.session_admission(thread_id).await.lock_owned().await;
            let current = self.inner.store.thread_get(thread_id)?.ok_or_else(|| anyhow!("Thread not found."))?;
            let current = self.migrate_cursor_model(current)?;
            if current.subagent.is_some() {
                return Err(anyhow!(subagents::READ_ONLY_ERROR));
            }
            if current.inbound.is_some() {
                return Err(anyhow!(inbound::INBOUND_READ_ONLY));
            }
            if let Some(moved) = &current.moved {
                return Err(anyhow!(move_thread::moved_message(moved)));
            }
            let selected_target = if queued {
                self.inner
                    .store
                    .meta_get(&format!("queue_target:{message_id}"))?
                    .map(|s| serde_json::from_str(&s))
                    .transpose()?
                    .unwrap_or(self.thread_target(thread_id)?.target)
            } else {
                self.inner
                    .store
                    .meta_get(&format!("quota_target:{message_id}"))?
                    .or(self.inner.store.meta_get(&format!("steer_target:{message_id}"))?)
                    .map(|saved| serde_json::from_str(&saved))
                    .transpose()?
                    .unwrap_or(self.thread_target(thread_id)?.target)
            };
            let selected_mode = if queued {
                self.inner
                    .store
                    .meta_get(&format!("queue_permission:{message_id}"))?
                    .map(|saved| serde_json::from_str(&saved))
                    .transpose()?
            } else {
                self.inner
                    .store
                    .meta_get(&format!("steer_permission:{message_id}"))?
                    .map(|saved| serde_json::from_str(&saved))
                    .transpose()?
                    .or(self.pending_permission_for(&current, &selected_target.provider)?)
            };
            let changes_permission = selected_target.provider == current.provider
                && !matches!(current.status, ThreadStatus::Running | ThreadStatus::AwaitingApproval)
                && selected_mode.is_some_and(|mode| mode != current.permission_mode);
            if _workspace.is_none() && (changes_permission || self.send_needs_workspace(&current)) {
                drop(admission);
                _workspace = Some(self.inner.workspace_ops.lock().await);
                continue;
            }
            break (admission, current, selected_target, selected_mode);
        };
        self.restore_worktree_if_cleaned(&current).await?;
        if selected_target.provider == current.provider
            && !matches!(current.status, ThreadStatus::Running | ThreadStatus::AwaitingApproval)
            && let Some(mode) = selected_mode
            && mode != current.permission_mode
        {
            self.apply_permission_admitted(thread_id, mode).await?;
        }
        let kind = selected_target.provider.kind;
        let _harness = self.inner.harness_gates[&kind]
            .clone()
            .try_read_owned()
            .map_err(|_| anyhow!("This agent is updating. Try sending again after the update finishes."))?;
        let _command = self.inner.commands.lock().map_err(|_| anyhow!("command lock poisoned"))?;
        if queued {
            // Read under the command gate: the user may have edited this item
            // since the queue worker selected it.
            message = self
                .inner
                .store
                .queue_list(Some(thread_id))?
                .into_iter()
                .find(|item| item.id == message_id)
                .ok_or_else(|| anyhow!("queued message was removed"))?
                .message;
        }
        self.resolve_attachments(&mut message);
        if retryable && let Some((stored_thread_id, turn_id, stored_message)) = self.inner.store.turn_started_receipt(message_id)? {
            if stored_thread_id != thread_id || stored_message != message {
                return Err(anyhow!("message_id already belongs to a different thread or message"));
            }
            return Ok((turn_id, message_id, Admission::Replayed));
        }
        let mut thread = self.inner.store.thread_get(thread_id)?.ok_or_else(|| anyhow!("thread not found"))?;
        // A send queued during a context refresh and retried with its id before
        // the queue drains is the same message: it stays queued, once.
        if !queued && let Some(waiting) = self.inner.store.queue_receipt(message_id)? {
            anyhow::ensure!(
                waiting.thread_id == thread_id && serde_json::to_value(&waiting.message)? == serde_json::to_value(&message)?,
                "message_id already belongs to a different thread or message"
            );
            if self.inner.store.queue_is_pending(message_id)? {
                return Ok((Uuid::nil(), message_id, Admission::Queued));
            }
        }
        // While a Coordinator's context is refreshed, a message waits in its
        // queue and starts the first turn of the fresh session.
        if self.is_rotating(thread_id) {
            if queued {
                return Err(ThreadBusy.into());
            }
            if is_compact_message(&message) {
                return Err(anyhow!(
                    "Kybern is refreshing this Coordinator's context into a fresh session. Compact later if you still need to."
                ));
            }
            self.enqueue_admitted(methods::QueuedMessage { id: message_id, thread_id, message })?;
            return Ok((Uuid::nil(), message_id, Admission::Queued));
        }
        if queued && thread.status != ThreadStatus::Idle {
            return Err(anyhow!("thread is no longer idle"));
        }
        if matches!(thread.status, ThreadStatus::Running | ThreadStatus::AwaitingApproval) {
            return Err(ThreadBusy.into());
        }
        if thread.status == ThreadStatus::Archived {
            return Err(anyhow!("This chat is archived. Start a new chat to keep working."));
        }
        if thread.delegation.as_ref().is_some_and(|info| info.worktree_state == Some(WorktreeState::Removed)) {
            return Err(delegation::worktree_removed_error());
        }

        if is_compact_message(&message) {
            let events = self.inner.store.events_for_thread(thread_id)?;
            let advertised = events
                .iter()
                .rev()
                .find_map(|event| match &event.payload {
                    EventPayload::ProviderCommandsUpdated { commands } => Some(commands.iter().any(|command| command.name == "compact")),
                    _ => None,
                })
                .unwrap_or(false);
            if !matches!(kind, ProviderKind::Codex | ProviderKind::Pi | ProviderKind::Omp | ProviderKind::Opencode) && !advertised {
                return Err(anyhow!("This harness does not expose manual compaction. Automatic compaction remains available."));
            }
            if thread.provider_session_id.is_none() {
                return Err(anyhow!("Send a message before compacting this conversation."));
            }
            if kybern_store::project_runtime_tasks(&self.inner.store.events_for_thread(thread_id)?)
                .iter()
                .any(|task| task.status.is_active())
            {
                return Err(anyhow!("Wait for agents and background tasks to finish before compacting."));
            }
        }
        if let Some(mode) = selected_mode {
            thread.permission_mode = mode;
        }
        self.admit_target(&mut thread, selected_target)?;
        let turn_id = Uuid::now_v7();
        self.inner.store.meta_set(&format!("turn_target:{turn_id}"), &serde_json::to_string(&thread.provider)?)?;
        if thread.title == DEFAULT_TITLE {
            thread.title = title_from_message(&message);
        }
        thread.status = ThreadStatus::Running;
        let thread = self.update_thread(thread)?;
        let startup_started = std::time::Instant::now();
        self.emit(thread.id, Some(turn_id), EventPayload::TurnStarted { message_id, message: message.clone() })?;
        if selected_mode.is_some() && self.pending_permission_for(&thread, &thread.provider)? == selected_mode {
            self.inner.store.meta_set_many(&[
                (&format!("pending_permission:{thread_id}"), ""),
                (&format!("pending_permission_target:{thread_id}"), ""),
            ])?;
        }
        self.messaging_turn_started(thread.id, turn_id, message_id);

        // The user's intent is persisted and broadcast, so the call returns now
        // and clients navigate and render immediately. Spawning the harness,
        // taking the checkpoint and delivering the message can take a second or
        // more; that runs in the background and reports failures as events.
        let this = self.clone();
        tokio::spawn(async move {
            if let Err(e) = this.start_turn(thread, turn_id, message_id, message, startup_started).await {
                tracing::warn!(turn_id = %turn_id, error = %e, "failed to start turn");
            }
        });
        Ok((turn_id, message_id, Admission::Started))
    }

    /// Whether a send must hold the daemon-wide workspace lock: its worktree folder is missing (restored under the lock), or
    /// it runs inside a managed worktree that another thread's removal or checkout could change underneath it.
    fn send_needs_workspace(&self, thread: &Thread) -> bool {
        thread.worktree.as_ref().is_some_and(|worktree| !std::path::Path::new(&worktree.path).exists())
            || std::path::Path::new(&thread.cwd).starts_with(&self.inner.paths.worktrees)
    }

    pub fn enqueue(&self, message: methods::QueuedMessage) -> Result<()> {
        let _command = self.inner.commands.lock().map_err(|_| anyhow!("command lock poisoned"))?;
        self.enqueue_admitted(message)
    }

    /// `enqueue` for a caller that holds the command gate.
    fn enqueue_admitted(&self, message: methods::QueuedMessage) -> Result<()> {
        let thread = self.inner.store.thread_get(message.thread_id)?.ok_or_else(|| anyhow!("thread not found"))?;
        if thread.subagent.is_some() {
            return Err(anyhow!(subagents::READ_ONLY_ERROR));
        }
        if let Some(moved) = &thread.moved {
            return Err(anyhow!(move_thread::moved_message(moved)));
        }
        if thread.status == ThreadStatus::Archived {
            return Err(anyhow!("This chat is archived. Start a new chat to keep working."));
        }
        if let Some(receipt) = self.inner.store.queue_receipt(message.id)? {
            if serde_json::to_value(receipt)? != serde_json::to_value(&message)? {
                return Err(anyhow!("message id already belongs to another request"));
            }
            return Ok(());
        }
        let target = self.thread_target(message.thread_id)?.target;
        let mut receiving = self.inner.store.thread_get(message.thread_id)?.ok_or_else(|| anyhow!("Thread not found."))?;
        let mode = self.pending_permission_for(&receiving, &target.provider)?.unwrap_or(receiving.permission_mode);
        receiving.provider_session_id = self.compatible_native_session(&receiving, &target.provider)?;
        receiving.provider = target.provider.clone();
        accounts::validate_permission(&receiving, mode)?;
        self.inner.store.meta_set_many(&[
            (&format!("queue_target:{}", message.id), &serde_json::to_string(&target)?),
            (&format!("queue_permission:{}", message.id), &serde_json::to_string(&mode)?),
        ])?;
        self.emit(message.thread_id, None, EventPayload::MessageQueued { message })?;
        Ok(())
    }

    pub fn remove_queued(&self, thread_id: ThreadId, id: MessageId) -> Result<()> {
        let _command = self.inner.commands.lock().map_err(|_| anyhow!("command lock poisoned"))?;
        let queued = self.inner.store.queue_list(Some(thread_id))?;
        if !queued.iter().any(|message| message.id == id) {
            return Err(anyhow!("follow-up has already started or was removed; refresh the thread"));
        }
        self.emit(thread_id, None, EventPayload::MessageRemoved { message_id: id })?;
        self.messaging_queue_removed(thread_id, id);
        Ok(())
    }

    pub fn update_queued(&self, message: methods::QueuedMessage) -> Result<()> {
        let _command = self.inner.commands.lock().map_err(|_| anyhow!("command lock poisoned"))?;
        if !self.inner.store.queue_list(Some(message.thread_id))?.iter().any(|item| item.id == message.id) {
            return Err(anyhow!("This follow-up has already started or was removed. Send a new message instead."));
        }
        self.emit(message.thread_id, None, EventPayload::MessageQueueUpdated { message })?;
        Ok(())
    }

    /// Deliver new user input within the current native turn. The turn gate also
    /// serializes retries, so a lost RPC reply does not send the prompt twice.
    pub async fn steer(&self, params: methods::QueuedMessage) -> Result<methods::ThreadsSendResult> {
        self.steer_from(params, SteerSource::Client).await
    }

    pub(super) async fn steer_from(&self, mut params: methods::QueuedMessage, source: SteerSource) -> Result<methods::ThreadsSendResult> {
        self.ensure_not_subagent(params.thread_id)?;
        anyhow::ensure!(!self.is_rotating(params.thread_id), rotation::ROTATING_STEER_ERROR);
        self.resolve_attachments(&mut params.message);
        let receipt = || -> Result<Option<methods::ThreadsSendResult>> {
            let Some((thread, turn_id, message)) =
                self.inner.store.steering_receipt(params.id)?.or(self.inner.store.turn_started_receipt(params.id)?)
            else {
                return Ok(None);
            };
            if thread != params.thread_id || serde_json::to_value(message)? != serde_json::to_value(&params.message)? {
                return Err(anyhow!("Message id already belongs to another request."));
            }
            Ok(Some(methods::ThreadsSendResult { turn_id, message_id: params.id, queued: false }))
        };
        if let Some(result) = receipt()? {
            return Ok(result);
        }
        let thread = self.inner.store.thread_get(params.thread_id)?.ok_or_else(|| anyhow!("Thread not found."))?;
        let target = {
            let _command = self.inner.commands.lock().map_err(|_| anyhow!("command lock poisoned"))?;
            let saved = self.inner.store.meta_get(&format!("steer_request:{}", params.id))?;
            if let Some(saved) = saved {
                let original: methods::QueuedMessage = serde_json::from_str(&saved)?;
                anyhow::ensure!(
                    serde_json::to_value(&original)? == serde_json::to_value(&params)?,
                    "Message id already belongs to another steering request."
                );
                serde_json::from_str(
                    &self
                        .inner
                        .store
                        .meta_get(&format!("steer_target:{}", params.id))?
                        .ok_or_else(|| anyhow!("The admitted steering target is missing. Send a new message."))?,
                )?
            } else {
                let target = self.thread_target(thread.id)?.target;
                if self.target_differs(&thread, &target)? {
                    // Restarting the turn on the new target is the user's call;
                    // Kybern's own deliveries wait for that turn in the queue.
                    anyhow::ensure!(
                        source == SteerSource::Client,
                        "This thread switches model or account on its next turn, so the message waits for that turn."
                    );
                    let mode = self.pending_permission_for(&thread, &target.provider)?.unwrap_or(thread.permission_mode);
                    let mut receiving = thread.clone();
                    receiving.provider_session_id = self.compatible_native_session(&thread, &target.provider)?;
                    receiving.provider = target.provider.clone();
                    accounts::validate_permission(&receiving, mode)?;
                    self.inner.store.meta_set_many(&[
                        (&format!("steer_request:{}", params.id), &serde_json::to_string(&params)?),
                        (&format!("steer_target:{}", params.id), &serde_json::to_string(&target)?),
                        (&format!("steer_permission:{}", params.id), &serde_json::to_string(&mode)?),
                    ])?;
                }
                target
            }
        };
        if self.target_differs(&thread, &target)? {
            if matches!(thread.status, ThreadStatus::Running | ThreadStatus::AwaitingApproval) {
                tracing::info!(
                    thread_id = %thread.id,
                    from = ?(&thread.provider, &thread.model, &thread.effort),
                    to = ?(&target.provider, &target.model, &target.effort),
                    "steering message restarts the turn on its new target"
                );
                // Only this thread's turn restarts; what it delegated keeps running.
                self.interrupt_turn(thread.id, "a steering message switches model or account").await?;
            }
            for _ in 0..100 {
                if self
                    .inner
                    .store
                    .thread_get(thread.id)?
                    .is_some_and(|t| !matches!(t.status, ThreadStatus::Running | ThreadStatus::AwaitingApproval))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            return self
                .send_client_message(methods::ThreadsSendParams {
                    thread_id: thread.id,
                    message: params.message,
                    message_id: Some(params.id),
                })
                .await;
        }
        if !matches!(thread.status, ThreadStatus::Running | ThreadStatus::AwaitingApproval) {
            return Err(anyhow!("This turn has ended. Send your message to start the next turn."));
        }
        let live = self
            .inner
            .sessions
            .lock()
            .await
            .get(&params.thread_id)
            .cloned()
            .ok_or_else(|| anyhow!("The agent is still starting. Try steering again in a moment, or queue this message."))?;
        let turn = live.turn.lock().await;
        if let Some(result) = receipt()? {
            return Ok(result);
        }
        // A handoff turn is Kybern's own, and a refresh that began meanwhile
        // (checked again under the turn lock) refuses steering.
        let active = turn
            .as_ref()
            .filter(|turn| !turn.completed && turn.internal.is_none())
            .ok_or_else(|| anyhow!("This turn has ended. Send your message to start the next turn."))?;
        anyhow::ensure!(!self.is_rotating(params.thread_id), rotation::ROTATING_STEER_ERROR);
        live.touch();
        live.session.steer(&params.id.to_string(), &self.provider_message_for(&thread, &params.message, &live)).await?;
        self.emit(params.thread_id, Some(active.id), EventPayload::MessageSteered { message_id: params.id, message: params.message })?;
        Ok(methods::ThreadsSendResult { turn_id: active.id, message_id: params.id, queued: false })
    }

    /// One daemon-owned worker consumes accepted follow-ups, independently of clients.
    /// Failed turns retain their queue until the user sends a new successful turn.
    ///
    /// Returns whether a follow-up is still waiting on a busy thread, so the
    /// worker knows to check back soon.
    ///
    /// Background work never holds the queue. A dev server, monitor or native
    /// subagent can outlive the turn that started it by hours; once that turn
    /// has ended, a follow-up starts the next turn exactly as a direct send
    /// would, and the tasks keep running on the same session.
    pub async fn drain_queues(&self) -> Result<bool> {
        let mut waiting = false;
        for queued in self.inner.store.queue_list(None)? {
            let Some(thread) = self.inner.store.thread_get(queued.thread_id)? else { continue };
            if thread.status == ThreadStatus::Archived {
                continue;
            }
            if self.is_rotating(thread.id) {
                waiting = true;
                continue;
            }
            if thread.status != ThreadStatus::Idle {
                waiting |= matches!(thread.status, ThreadStatus::Running | ThreadStatus::AwaitingApproval);
                continue;
            }
            if let Err(error) = self.send_with_id(thread.id, queued.id, queued.message, true, false).await {
                tracing::debug!(%error, "queue changed before dispatch");
            }
        }
        Ok(waiting)
    }

    /// Bring up the session, snapshot the tree and hand the message to the agent.
    /// Failures mark the thread failed and are surfaced as `TurnFailed`.
    async fn start_turn(
        &self,
        thread: Thread,
        turn_id: TurnId,
        message_id: MessageId,
        message: UserMessage,
        startup_started: std::time::Instant,
    ) -> Result<()> {
        self.prepare_for_computer(&thread, &message).await;
        let (live, session_reused) = match self.ensure_session(&thread).await {
            Ok(live) => live,
            Err(error) => {
                self.emit(thread.id, Some(turn_id), EventPayload::TurnFailed { error: error.to_string() })?;
                let thread_id = thread.id;
                let mut failed = thread;
                failed.status = ThreadStatus::Failed;
                self.update_thread(failed)?;
                self.delegation_turn_finished(thread_id, turn_id, delegation::TurnOutcome::Failed(error.to_string()));
                return Err(error);
            }
        };
        live.touch();
        if self.inner.store.thread_get(thread.id)?.is_some_and(|current| current.status == ThreadStatus::Archived) {
            live.mark_released();
            live.turn_ready.notify_one();
            let _ = live.session.close().await;
            self.inner.sessions.lock().await.remove(&thread.id);
            return Err(anyhow!("thread was archived during session startup"));
        }
        tracing::info!(
            target: "kybern::turn_startup",
            thread_id = %thread.id,
            turn_id = %turn_id,
            provider = %thread.provider.kind,
            session_reused,
            phase = "session_ready",
            elapsed_ms = startup_started.elapsed().as_millis() as u64,
        );
        {
            let mut turn = live.turn.lock().await;
            live.continuation.lock().await.take();
            *turn = Some(ActiveTurn {
                response_id: Uuid::now_v7(),
                id: turn_id,
                started: std::time::Instant::now(),
                startup_started,
                provider: thread.provider.kind,
                session_reused,
                first_event_observed: false,
                messages: HashMap::new(),
                active_messages: HashMap::new(),
                terminal_message_id: None,
                completed: false,
                pending_completion: None,
                internal: None,
                rotation_seed: None,
            });
        }
        live.turn_ready.notify_one();
        *live.last_turn_id.lock().await = Some(turn_id);
        let checkpoint_started = std::time::Instant::now();
        self.checkpoint(&thread, turn_id, "before").await;
        tracing::info!(
            target: "kybern::turn_startup",
            thread_id = %thread.id,
            turn_id = %turn_id,
            provider = %thread.provider.kind,
            session_reused,
            phase = "checkpoint_ready",
            phase_ms = checkpoint_started.elapsed().as_millis() as u64,
            elapsed_ms = startup_started.elapsed().as_millis() as u64,
        );

        let mut workspace_transition = None;
        let delivery = if is_compact_message(&message) {
            live.session.compact().await
        } else {
            match self.portable_message(&thread, &message).and_then(|portable| self.workspace_transition_message(&thread, &portable)) {
                Ok((workspace_message, transition)) => {
                    workspace_transition = transition;
                    // A brief the user changed since this Coordinator's session began
                    // rides in front of the next message, in the provider's copy only.
                    let brief_update = self.lead_brief_update(&thread, &live).await.unwrap_or_else(|error| {
                        tracing::warn!(thread_id = %thread.id, %error, "could not check the project brief for changes");
                        None
                    });
                    // The same goes for a change to the Coordinator's settings.
                    let settings_update = self.lead_settings_update(&thread, &live).await.unwrap_or_else(|error| {
                        tracing::warn!(thread_id = %thread.id, %error, "could not check the Coordinator settings for changes");
                        None
                    });
                    // Code that changed since the Coordinator last looked, also provider-only.
                    let change_report = self.lead_change_report(&thread).await.unwrap_or_else(|error| {
                        tracing::warn!(thread_id = %thread.id, %error, "could not check the project's git for changes");
                        None
                    });
                    // A setup kickoff is a row for clients; the provider gets the script.
                    let kickoff = self.setup_provider_message(&thread, &workspace_message).unwrap_or_else(|error| {
                        tracing::warn!(thread_id = %thread.id, %error, "could not prepare the setup kickoff");
                        None
                    });
                    // A handoff the user corrected after the fresh session already read it.
                    let handoff_correction = self.handoff_correction(&thread).unwrap_or_else(|error| {
                        tracing::warn!(thread_id = %thread.id, %error, "could not check the handoff for corrections");
                        None
                    });
                    // The first message after a context refresh starts with the handoff
                    // and the facts Kybern carried, in the provider's copy only.
                    let seed = self.rotation_seed(thread.id).unwrap_or_else(|error| {
                        tracing::warn!(thread_id = %thread.id, %error, "could not read the context refresh seed");
                        None
                    });
                    let provider_message = self.provider_message_for(&thread, kickoff.as_ref().unwrap_or(&workspace_message), &live);
                    let updates: Vec<&str> = [
                        brief_update.as_ref().map(|(_, text)| text.as_str()),
                        settings_update.as_ref().map(|(_, text)| text.as_str()),
                        // The rotation marked the seed's heads seen, so this holds only
                        // what changed after the refresh: it rides along with the seed.
                        change_report.as_ref().map(|report| report.text.as_str()),
                        handoff_correction.as_ref().map(|(_, text)| text.as_str()),
                    ]
                    .into_iter()
                    .flatten()
                    .collect();
                    let provider_message = if updates.is_empty() {
                        provider_message
                    } else {
                        std::borrow::Cow::Owned(lead::with_brief_update(&provider_message, &updates.join("\n\n")))
                    };
                    let provider_message = match &seed {
                        Some(seed) => std::borrow::Cow::Owned(lead::with_brief_update(&provider_message, &seed.text)),
                        None => provider_message,
                    };
                    if let Some(seed) = &seed
                        && let Some(turn) = live.turn.lock().await.as_mut().filter(|turn| turn.id == turn_id)
                    {
                        turn.rotation_seed = Some(seed.clone());
                    }
                    let sent = live.session.send_message(&message_id.to_string(), &provider_message).await;
                    if sent.is_ok() {
                        if let Some((revision, _)) = brief_update {
                            self.lead_brief_seen(&live, revision).await;
                        }
                        if let Some((settings, _)) = settings_update {
                            self.lead_settings_seen(&live, settings).await;
                        }
                        if let Some(report) = change_report {
                            self.lead_change_seen(&thread, report).await;
                        }
                        if let Some((seq, _)) = handoff_correction
                            && let Err(error) = self.handoff_correction_seen(thread.id, seq)
                        {
                            tracing::warn!(thread_id = %thread.id, %error, "could not record the handoff correction");
                        }
                    }
                    sent
                }
                Err(error) => Err(kybern_drivers::DriverError::Unsupported(error.to_string())),
            }
        };
        if let Err(e) = delivery {
            self.emit(thread.id, Some(turn_id), EventPayload::TurnFailed { error: e.to_string() })?;
            let thread_id = thread.id;
            let mut t = thread;
            t.status = ThreadStatus::Failed;
            self.update_thread(t)?;
            *live.turn.lock().await = None;
            self.delegation_turn_finished(thread_id, turn_id, delegation::TurnOutcome::Failed(e.to_string()));
            return Err(e.into());
        }
        if !is_compact_message(&message) {
            self.inner.store.meta_set(&format!("handoff:{}", thread.id), "")?;
            if let Some(sequence) = workspace_transition {
                self.workspace_transition_delivered(thread.id, sequence)?;
            }
        }
        tracing::info!(
            target: "kybern::turn_startup",
            thread_id = %thread.id,
            turn_id = %turn_id,
            provider = %thread.provider.kind,
            session_reused,
            phase = "prompt_sent",
            elapsed_ms = startup_started.elapsed().as_millis() as u64,
        );
        Ok(())
    }

    /// Stop a thread's turn and everything it delegated (user Stop, Lineage Stop).
    /// `cause` names who asked, for `daemon.log`.
    pub async fn stop_thread(&self, thread_id: ThreadId, cause: &str) -> Result<()> {
        tracing::info!(%thread_id, cause, "stopping the thread and its delegated agents");
        // Interrupting a subagent thread stops that subagent, not its parent.
        if self.inner.store.thread_get(thread_id)?.is_some_and(|thread| thread.subagent.is_some()) {
            self.stop_runtime_task(thread_id, "").await?;
            return Ok(());
        }
        // The thread is flagged (it cannot delegate any more) and interrupted
        // first, and the cascade over what it delegated runs alongside; a
        // delegated child stopped directly (Lineage Stop) is cancelled and its
        // parent told.
        self.delegation_stop(thread_id, true).await
    }

    /// Interrupt only this thread's own turn, for a switch of model, account or
    /// permissions. Its delegated agents keep running.
    pub(super) async fn interrupt_turn(&self, thread_id: ThreadId, cause: &str) -> Result<()> {
        tracing::info!(%thread_id, cause, "interrupting the thread's turn");
        self.interrupt_with_grace(thread_id, Duration::from_secs(5)).await
    }

    async fn interrupt_with_grace(&self, thread_id: ThreadId, grace: Duration) -> Result<()> {
        let live = self.inner.sessions.lock().await.get(&thread_id).cloned().ok_or_else(|| anyhow!("thread has no live session"))?;
        let response_id = live.turn.lock().await.as_ref().map(|turn| turn.response_id);
        live.touch();
        let this = self.clone();
        // The RPC connection may disappear while stopping. Cleanup still owns
        // the deadline, including a provider that never acknowledges interrupt.
        tokio::spawn(async move {
            let stopped = async {
                live.session.interrupt().await?;
                loop {
                    if live.is_released()
                        || live.turn.lock().await.as_ref().map(|turn| turn.response_id) != response_id
                        || (response_id.is_none() && !live.tasks.lock().await.values().any(|task| task.status.is_active()))
                    {
                        return Ok::<_, kybern_drivers::DriverError>(());
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            };
            if !matches!(tokio::time::timeout(grace, stopped).await, Ok(Ok(()))) {
                tracing::info!(%thread_id, grace_ms = grace.as_millis() as u64, "provider did not finish interrupting in time; closing its session");
                this.force_interrupt(thread_id, &live, response_id).await?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    async fn force_interrupt(&self, thread_id: ThreadId, live: &Arc<LiveSession>, response_id: Option<Uuid>) -> Result<()> {
        let done = {
            let mut sessions = self.inner.sessions.lock().await;
            let turn = live.turn.lock().await;
            if !sessions.get(&thread_id).is_some_and(|current| Arc::ptr_eq(current, live))
                || live.is_released()
                || turn.as_ref().map(|turn| turn.response_id) != response_id
            {
                return Ok(());
            }
            let (done, waiting) = tokio::sync::watch::channel(());
            self.inner.releasing.lock().await.insert(thread_id, waiting);
            live.mark_released();
            live.stop_cleanup.store(true, Ordering::Relaxed);
            sessions.remove(&thread_id);
            self.revoke_native_session(live);
            done
        };
        let result = async {
            // close() terminates the owned process tree if graceful EOF fails.
            // Keep the resume barrier until both process and turn are settled.
            live.session.close().await?;
            live.continuation.lock().await.take();
            self.restore_active_runtime_tasks(thread_id, live).await?;
            self.interrupt_runtime_tasks(thread_id, live, "Stopped because the provider did not finish interrupting").await;
            self.process_driver_event(
                thread_id,
                live,
                DriverEvent::TurnCompleted {
                    stop_reason: StopReason::Interrupted,
                    usage: Usage::default(),
                    cost_usd: None,
                    duration_ms: 0,
                    anchors: TurnAnchors::default(),
                },
                true,
            )
            .await?;
            self.emit(
                thread_id,
                None,
                EventPayload::ProviderNotice {
                    level: NoticeLevel::Warning,
                    text: "The agent did not finish stopping, so Kybern closed its process. Send a message to resume the conversation."
                        .into(),
                    data: None,
                },
            )?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        self.inner.releasing.lock().await.remove(&thread_id);
        drop(done);
        result
    }

    pub async fn stop_runtime_task(&self, thread_id: ThreadId, task_id: &str) -> Result<RuntimeTask> {
        let (thread_id, task_id) = self.resolve_task_control(thread_id, task_id)?;
        let task_id = task_id.as_str();
        let live = self.task_session(thread_id, task_id).await?;
        let task = live
            .tasks
            .lock()
            .await
            .get(task_id)
            .cloned()
            .ok_or_else(|| anyhow!("this task is not attached to the live provider session"))?;
        if !task.status.is_active() {
            return Err(anyhow!("this task has already finished"));
        }
        if !task.capabilities.stop {
            return Err(anyhow!("{} does not expose a targeted stop control for this task", self.provider_name(thread_id)?));
        }
        live.touch();
        live.session.stop_runtime_task(&self.native_runtime_task(&live, &task).await).await?;
        self.apply_runtime_task_update(
            thread_id,
            &live,
            DriverRuntimeTaskUpdate::status(task.id.clone(), RuntimeTaskStatus::Stopping),
            RuntimeTaskUpdateKind::Progress,
        )
        .await?
        .ok_or_else(|| anyhow!("task not found"))
    }

    pub async fn background_runtime_task(&self, thread_id: ThreadId, task_id: &str) -> Result<RuntimeTask> {
        let (thread_id, task_id) = self.resolve_task_control(thread_id, task_id)?;
        let task_id = task_id.as_str();
        let live = self.task_session(thread_id, task_id).await?;
        let task = live
            .tasks
            .lock()
            .await
            .get(task_id)
            .cloned()
            .ok_or_else(|| anyhow!("this task is not attached to the live provider session"))?;
        if !task.status.is_active() {
            return Err(anyhow!("this task has already finished"));
        }
        if !task.capabilities.background {
            return Err(anyhow!("{} cannot move this task to the background", self.provider_name(thread_id)?));
        }
        live.touch();
        live.session.background_runtime_task(&self.native_runtime_task(&live, &task).await).await?;
        self.apply_runtime_task_update(
            thread_id,
            &live,
            DriverRuntimeTaskUpdate {
                id: task.id.clone(),
                status: None,
                detail: None,
                backgrounded: Some(true),
                last_tool_name: None,
                usage: None,
                stats: None,
                capabilities: Some(RuntimeTaskCapabilities { stop: task.capabilities.stop, background: false }),
            },
            RuntimeTaskUpdateKind::Progress,
        )
        .await?
        .ok_or_else(|| anyhow!("task not found"))
    }

    fn provider_name(&self, thread_id: ThreadId) -> Result<&'static str> {
        Ok(self.inner.store.thread_get(thread_id)?.ok_or_else(|| anyhow!("thread not found"))?.provider.kind.display_name())
    }

    /// Async questions are ordinary user input, not provider permission responses.
    /// Serialize submissions so two clients cannot answer the same question twice.
    pub async fn answer_questions(&self, params: methods::ThreadsAnswerParams) -> Result<()> {
        self.ensure_not_subagent(params.thread_id)?;
        let _answer = self.inner.question_answers.lock().await;
        let events = self.inner.store.events_for_thread(params.thread_id)?;
        if let Some(previous) = events.iter().find_map(|event| match &event.payload {
            EventPayload::AsyncQuestionsAnswered { request_id, answers, .. } if request_id == &params.request_id => Some(answers),
            _ => None,
        }) {
            return if previous == &params.answers {
                Ok(())
            } else {
                Err(anyhow!("This question has already been answered. Send a new message to change your answer."))
            };
        }
        let request = kybern_store::project_pending_questions(&events)
            .into_iter()
            .find(|r| r.id == params.request_id)
            .ok_or_else(|| anyhow!("Question not found. Refresh the conversation and try again."))?;
        if request.questions.len() != params.answers.len() || params.answers.iter().any(|a| a.trim().is_empty()) {
            return Err(anyhow!("Answer each question before submitting."));
        }
        let message = UserMessage::text(
            request
                .questions
                .iter()
                .zip(&params.answers)
                .map(|(question, answer)| format!("{}\nAnswer: {}", question.title, answer.trim()))
                .collect::<Vec<_>>()
                .join("\n\n"),
        );
        let thread = self.inner.store.thread_get(params.thread_id)?.ok_or_else(|| anyhow!("thread not found"))?;
        if thread.status == ThreadStatus::Archived {
            return Err(anyhow!("Unarchive this conversation before answering."));
        }
        let message_id = Uuid::now_v7();
        let turn_id = if matches!(thread.status, ThreadStatus::Running | ThreadStatus::AwaitingApproval) {
            anyhow::ensure!(!self.is_rotating(params.thread_id), rotation::ROTATING_STEER_ERROR);
            let live = self
                .inner
                .sessions
                .lock()
                .await
                .get(&params.thread_id)
                .cloned()
                .ok_or_else(|| anyhow!("The agent is starting. Submit your answer again in a moment."))?;
            let turn = live.turn.lock().await;
            // A handoff turn is Kybern's own: an answer never steers into it.
            let active = turn
                .as_ref()
                .filter(|turn| !turn.completed && turn.internal.is_none())
                .ok_or_else(|| anyhow!("The turn just ended. Submit your answer again to continue the conversation."))?;
            anyhow::ensure!(!self.is_rotating(params.thread_id), rotation::ROTATING_STEER_ERROR);
            live.touch();
            live.session.steer(&message_id.to_string(), &self.provider_message_for(&thread, &message, &live)).await?;
            Some(active.id)
        } else {
            // Queued during a context refresh: the answer has no turn until it starts one.
            match self.send_with_id(params.thread_id, message_id, message.clone(), false, false).await? {
                (_, _, Admission::Queued) => None,
                (turn_id, ..) => Some(turn_id),
            }
        };
        self.emit(
            params.thread_id,
            turn_id,
            EventPayload::AsyncQuestionsAnswered { request_id: params.request_id, answers: params.answers, message_id, message },
        )?;
        Ok(())
    }

    pub async fn respond_approval(&self, approval_id: ApprovalId, decision: ApprovalDecision) -> Result<()> {
        self.respond_approval_as(approval_id, decision, None).await
    }

    /// Answer a request. `answered_by` names a delegating agent answering for the user.
    /// A `kybern_answers` response (normalized answers, attachments included) is turned
    /// into the harness's own response first.
    pub(super) async fn respond_approval_as(
        &self,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
        answered_by: Option<RequestAnswerer>,
    ) -> Result<()> {
        let (approval, resolved) = self.inner.store.approval_get(approval_id)?.ok_or_else(|| anyhow!("approval not found"))?;
        if resolved {
            return Err(anyhow!("This request was already answered."));
        }
        let decision = self.native_answer(&approval, decision)?;
        approval.validate_decision(&decision).map_err(|message| anyhow!(message))?;
        let live =
            self.inner.sessions.lock().await.get(&approval.thread_id).cloned().ok_or_else(|| anyhow!("thread has no live session"))?;
        // Release the map before touching other state; restoring the thread
        // status reads it again.
        let daemon_owned = {
            let approvals = live.daemon_approvals.lock().await;
            match approvals.get(&approval_id) {
                Some(entry) if entry.decision.borrow().is_some() => return Err(anyhow!("approval already resolved")),
                Some(entry) => {
                    entry.decision.send_replace(Some(decision.clone()));
                    true
                }
                None => false,
            }
        };
        if daemon_owned {
            live.touch();
            self.inner.store.approval_resolve(approval_id, &decision)?;
            self.emit(approval.thread_id, Some(approval.turn_id), EventPayload::ApprovalResolved { approval_id, decision, answered_by })?;
            self.restore_running_after_approval(approval.thread_id, &live).await?;
            return Ok(());
        }
        let mut pending = live.pending.lock().await;
        let request_id = pending.get(&approval_id).cloned().ok_or_else(|| anyhow!("approval no longer pending"))?;
        live.touch();
        live.session.respond_permission(&request_id, &decision).await?;
        pending.remove(&approval_id);
        drop(pending);
        self.inner.store.approval_resolve(approval_id, &decision)?;
        self.emit(approval.thread_id, Some(approval.turn_id), EventPayload::ApprovalResolved { approval_id, decision, answered_by })?;
        self.restore_running_after_approval(approval.thread_id, &live).await?;
        Ok(())
    }

    /// The harness's own response for normalized answers (`{"kybern_answers": [...]}`); any other decision unchanged.
    fn native_answer(&self, approval: &ApprovalRequest, decision: ApprovalDecision) -> Result<ApprovalDecision> {
        let ApprovalDecision::Submit { response } = &decision else { return Ok(decision) };
        let Some(answers) = response.get(NORMALIZED_ANSWERS_KEY) else { return Ok(decision) };
        let answers: Vec<UserInputAnswer> =
            serde_json::from_value(answers.clone()).map_err(|_| anyhow!("The answers could not be read. Answer the question again."))?;
        let response =
            encode_answers(approval, &answers, |attachment| self.attachment_path(attachment)).map_err(|message| anyhow!(message))?;
        Ok(ApprovalDecision::Submit { response })
    }

    /// Where an answer's attachment is on disk: an uploaded asset, or a file the user chose.
    fn attachment_path(&self, attachment: &UserInputAttachment) -> Option<String> {
        if !matches!(attachment.kind.as_str(), "image" | "file" | "window") {
            return None;
        }
        if let Some(id) = attachment.asset_id {
            let info = self.inner.store.asset_get(id).ok().flatten()?;
            let path = self.inner.paths.assets.join(id.to_string());
            if !path.is_file() {
                return None;
            }
            // Agents tell an image by its extension (Claude's Read does), and assets are stored by id alone: hand over a
            // hard link named `<id>.<ext>` beside it.
            let named = asset_extension(&info.name, &info.media_type).map(|ext| self.inner.paths.assets.join(format!("{id}.{ext}")));
            if let Some(named) = named.filter(|named| named.is_file() || std::fs::hard_link(&path, named).is_ok()) {
                return Some(named.to_string_lossy().into_owned());
            }
            return Some(path.to_string_lossy().into_owned());
        }
        let path = std::path::Path::new(attachment.path.as_deref()?);
        (path.is_absolute() && path.exists()).then(|| path.to_string_lossy().into_owned())
    }

    async fn restore_running_after_approval(&self, thread_id: ThreadId, live: &LiveSession) -> Result<()> {
        let daemon_waiting = live.daemon_approvals.lock().await.values().any(|entry| entry.decision.borrow().is_none());
        if live.pending.lock().await.is_empty()
            && !daemon_waiting
            && let Some(mut t) = self.inner.store.thread_get(thread_id)?
            && t.status == ThreadStatus::AwaitingApproval
        {
            t.status = ThreadStatus::Running;
            self.update_thread(t)?;
        }
        Ok(())
    }

    /// Ask the user, through a normal approval card, to let this chat control
    /// an app. A retry of the same request in the same turn reuses the card
    /// and its answer.
    async fn ask_computer_consent(
        &self,
        thread_id: ThreadId,
        turn_id: TurnId,
        live: &Arc<LiveSession>,
        request: crate::computer::ConsentRequest,
    ) -> Result<crate::computer::ConsentAnswer> {
        use crate::computer::{ConsentAnswer, Mode};
        let key = format!("{}:{:?}", request.app_id.as_deref().unwrap_or(&request.app.to_lowercase()), request.mode);
        let foreground = request.mode == Mode::Foreground;
        let input = serde_json::json!({
            "app": request.app,
            // `bundle_id` is the older name for the same value.
            "bundle_id": request.app_id,
            "app_id": request.app_id,
            "mode": if foreground { "foreground" } else { "background" },
            "action": request.first_action,
        });
        let summary = if foreground {
            format!("Let this chat use your cursor and keyboard in {}", request.app)
        } else {
            format!("Let this chat control {} in the background", request.app)
        };
        let (_, answer) =
            self.ask_daemon_approval(thread_id, turn_id, live, key, crate::computer::APPROVAL_TOOL, input, summary, None).await?;
        Ok(match answer {
            DaemonAnswer::Decided(ApprovalDecision::AllowAlways) => ConsentAnswer::Session,
            DaemonAnswer::Decided(ApprovalDecision::Submit { response })
                if response.get("scope").and_then(serde_json::Value::as_str) == Some("always") =>
            {
                ConsentAnswer::Always
            }
            DaemonAnswer::Decided(ApprovalDecision::Deny { reason }) => ConsentAnswer::Denied(reason),
            DaemonAnswer::Decided(_) => ConsentAnswer::Turn,
            DaemonAnswer::TurnEnded => ConsentAnswer::Denied(Some("the turn ended".into())),
            DaemonAnswer::Pending => ConsentAnswer::Pending,
        })
    }

    /// Open (or, for a retry with the same `key` in the same turn, reuse) a
    /// daemon-owned approval card and wait for the answer. The card resolves in
    /// the daemon, not the provider, and keeps its answer until the turn ends.
    /// A card another `operation` consumed (see [`Self::consume_daemon_approval`])
    /// is not reused. Returns the card's id with the answer.
    #[allow(clippy::too_many_arguments)]
    async fn ask_daemon_approval(
        &self,
        thread_id: ThreadId,
        turn_id: TurnId,
        live: &Arc<LiveSession>,
        key: String,
        tool_name: &str,
        input: serde_json::Value,
        summary: String,
        operation: Option<Uuid>,
    ) -> Result<(ApprovalId, DaemonAnswer)> {
        self.ask_daemon_approval_within(thread_id, turn_id, live, key, tool_name, input, summary, operation, crate::computer::CONSENT_WAIT)
            .await
    }

    /// [`Self::ask_daemon_approval`] with its own wait before the answer is `Pending`.
    #[allow(clippy::too_many_arguments)]
    async fn ask_daemon_approval_within(
        &self,
        thread_id: ThreadId,
        turn_id: TurnId,
        live: &Arc<LiveSession>,
        key: String,
        tool_name: &str,
        input: serde_json::Value,
        summary: String,
        operation: Option<Uuid>,
        wait: Duration,
    ) -> Result<(ApprovalId, DaemonAnswer)> {
        let (card, mut receiver) = self.open_daemon_card(thread_id, turn_id, live, key, tool_name, input, summary, operation).await?;
        let decision = tokio::time::timeout(wait, receiver.wait_for(Option::is_some)).await;
        let answer = match decision {
            Ok(Ok(decision)) => match decision.clone() {
                Some(decision) => DaemonAnswer::Decided(decision),
                None => DaemonAnswer::Pending,
            },
            Ok(Err(_)) => DaemonAnswer::TurnEnded,
            Err(_) => DaemonAnswer::Pending,
        };
        Ok((card, answer))
    }

    /// Raise a daemon-owned approval card (or, for a retry with the same `key`
    /// in the same turn, find the one already open) without waiting for it.
    /// The receiver holds the decision once there is one. Resolve a card the
    /// daemon answers itself with [`Self::finish_daemon_card`].
    #[allow(clippy::too_many_arguments)]
    async fn open_daemon_card(
        &self,
        thread_id: ThreadId,
        turn_id: TurnId,
        live: &Arc<LiveSession>,
        key: String,
        tool_name: &str,
        input: serde_json::Value,
        summary: String,
        operation: Option<Uuid>,
    ) -> Result<(ApprovalId, tokio::sync::watch::Receiver<Option<ApprovalDecision>>)> {
        let mut approvals = live.daemon_approvals.lock().await;
        let reusable = approvals
            .iter()
            .find(|(_, entry)| entry.turn_id == turn_id && entry.key == key && entry.consumed_by.is_none_or(|by| Some(by) == operation));
        if let Some((id, entry)) = reusable {
            return Ok((*id, entry.decision.subscribe()));
        }
        let approval = ApprovalRequest {
            id: Uuid::now_v7(),
            thread_id,
            turn_id,
            tool_call_id: None,
            tool_name: tool_name.into(),
            input,
            summary,
            suggestions: Vec::new(),
            created_at: Utc::now(),
            questions: Vec::new(),
        }
        .with_questions();
        let (sender, receiver) = tokio::sync::watch::channel(None);
        let card = approval.id;
        approvals.insert(card, DaemonApproval { turn_id, key, decision: sender, consumed_by: None });
        drop(approvals);
        if let Some(turn) = live.turn.lock().await.as_mut() {
            turn.active_messages.remove(&EventOrigin::Root);
        }
        self.inner.store.approval_insert(&approval)?;
        self.emit(thread_id, Some(turn_id), EventPayload::ApprovalRequested { approval })?;
        let mut t = self.inner.store.thread_get(thread_id)?.ok_or_else(|| anyhow!("thread vanished"))?;
        if t.status != ThreadStatus::AwaitingApproval {
            t.status = ThreadStatus::AwaitingApproval;
            self.update_thread(t)?;
        }
        Ok((card, receiver))
    }

    /// The daemon answers its own card (a sign-in that finished, a deadline
    /// that ran out). False when the user answered first: that answer stands.
    async fn finish_daemon_card(
        &self,
        thread_id: ThreadId,
        turn_id: TurnId,
        live: &LiveSession,
        card: ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<bool> {
        let Some(entry) = live.daemon_approvals.lock().await.remove(&card) else { return Ok(false) };
        if entry.decision.borrow().is_some() {
            return Ok(false);
        }
        entry.decision.send_replace(Some(decision.clone()));
        self.inner.store.approval_resolve(card, &decision)?;
        self.emit(thread_id, Some(turn_id), EventPayload::ApprovalResolved { approval_id: card, decision, answered_by: None })?;
        self.restore_running_after_approval(thread_id, live).await?;
        Ok(true)
    }

    /// Spend a card's one-time answer on `operation`. False when another
    /// operation already spent it; a retry of the same operation may spend it again.
    async fn consume_daemon_approval(&self, live: &LiveSession, card: ApprovalId, operation: Uuid) -> Result<bool> {
        let mut approvals = live.daemon_approvals.lock().await;
        let entry = approvals.get_mut(&card).ok_or_else(|| anyhow!("The turn ended before this change was made. Nothing was saved."))?;
        Ok(match entry.consumed_by {
            None => {
                entry.consumed_by = Some(operation);
                true
            }
            Some(by) => by == operation,
        })
    }

    async fn resolve_finished_requests(&self, thread_id: ThreadId, turn_id: TurnId, live: &LiveSession) -> Result<()> {
        let finished: Vec<(ApprovalId, DaemonApproval)> = {
            let mut approvals = live.daemon_approvals.lock().await;
            let ids = approvals.iter().filter(|(_, entry)| entry.turn_id == turn_id).map(|(id, _)| *id).collect::<Vec<_>>();
            ids.into_iter().filter_map(|id| approvals.remove(&id).map(|entry| (id, entry))).collect()
        };
        for (id, entry) in finished {
            if entry.decision.borrow().is_some() {
                continue;
            }
            let decision = ApprovalDecision::Deny { reason: Some("turn ended".into()) };
            entry.decision.send_replace(Some(decision.clone()));
            self.inner.store.approval_resolve(id, &decision)?;
            self.emit(thread_id, Some(turn_id), EventPayload::ApprovalResolved { approval_id: id, decision, answered_by: None })?;
        }
        let mut pending = live.pending.lock().await;
        let ids = pending.keys().copied().collect::<Vec<_>>();
        for id in ids {
            let Some((request, resolved)) = self.inner.store.approval_get(id)? else { continue };
            if request.turn_id != turn_id || resolved {
                continue;
            }
            let decision = ApprovalDecision::Deny { reason: Some("turn ended".into()) };
            if let Some(provider_id) = pending.remove(&id) {
                let _ = live.session.respond_permission(&provider_id, &decision).await;
            }
            self.inner.store.approval_resolve(id, &decision)?;
            self.emit(thread_id, Some(turn_id), EventPayload::ApprovalResolved { approval_id: id, decision, answered_by: None })?;
        }
        Ok(())
    }

    /// Per-thread lock around snapshots and everything that rewrites the thread's files under them (revert, PR checkout,
    /// worktree and delegated worktree removal). Lock order: `workspace_ops`, then the thread's admission gate, then this lock. Nothing that holds
    /// it waits on admission or `workspace_ops`: the "before" snapshot runs in the spawned turn start after admission is
    /// released, and the "after" snapshot in its own task. The turn-end path waits on it while holding no other lock.
    fn checkpoint_lock(&self, thread_id: ThreadId) -> Arc<Mutex<()>> {
        self.inner.checkpoint_locks.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).entry(thread_id).or_default().clone()
    }

    /// Snapshot the working tree for a turn. Silently skipped for non-git projects.
    async fn checkpoint(&self, thread: &Thread, turn_id: TurnId, which: &str) {
        let _checkpoint = self.checkpoint_lock(thread.id).lock_owned().await;
        self.checkpoint_locked(thread, turn_id, which).await;
    }

    /// Take a turn's "after" snapshot without holding up its end: hashing a
    /// large tree can take minutes, and clients show the turn as working
    /// until `TurnCompleted`. The lock is taken here, before the turn ends,
    /// so the next turn's "before" snapshot and a revert wait for this one.
    async fn checkpoint_after_turn(&self, thread: &Thread, turn_id: TurnId) {
        let checkpoint = self.checkpoint_lock(thread.id).lock_owned().await;
        let this = self.clone();
        let thread = thread.clone();
        tokio::spawn(async move {
            this.checkpoint_locked(&thread, turn_id, "after").await;
            drop(checkpoint);
        });
    }

    async fn checkpoint_locked(&self, thread: &Thread, turn_id: TurnId, which: &str) {
        if group_git::is_group(thread) {
            return self.group_checkpoint_locked(thread, turn_id, which).await;
        }
        let repo = Repo::new(&thread.cwd);
        if !Repo::is_repo(std::path::Path::new(&thread.cwd)).await {
            return;
        }
        let commit = match repo.snapshot(&format!("kybern checkpoint {which} for turn {turn_id}")).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(thread_id = %thread.id, %e, "checkpoint failed");
                return;
            }
        };
        let _ = repo.update_ref(&checkpoint_ref(&thread.id.to_string(), &turn_id.to_string(), which), &commit).await;
        let checkpoint = match which {
            "before" => Checkpoint {
                thread_id: thread.id,
                turn_id,
                before: commit,
                after: None,
                provider_turn_id: None,
                provider_turn_end: None,
                created_at: Utc::now(),
                repos: Vec::new(),
                incomplete: false,
            },
            _ => match self.inner.store.checkpoint_get(turn_id) {
                Ok(Some(mut c)) => {
                    c.after = Some(commit);
                    c
                }
                _ => return,
            },
        };
        if let Err(e) = self.inner.store.checkpoint_upsert(&checkpoint) {
            tracing::warn!(%e, "store checkpoint");
            return;
        }
        let _ = self.emit(thread.id, Some(turn_id), EventPayload::CheckpointUpdated { checkpoint });
    }

    /// Kick off a background title generation for threads still carrying the derived title.
    fn maybe_generate_title(&self, thread: &Thread) {
        if !self.inner.settings.get().generate_titles {
            return;
        }
        // A task's run keeps the task's title.
        if self.inner.task_threads.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).contains(&thread.id) {
            return;
        }
        let Ok((first, turns)) = self.inner.store.first_turn_message_and_count(thread.id) else { return };
        let Some(first) = first else { return };
        if turns != 1 || thread.title != title_from_message(&first) {
            return;
        }
        let this = self.clone();
        let thread = thread.clone();
        tokio::spawn(async move {
            match this.generate_title(&thread, &first).await {
                Ok(Some(title)) => {
                    if let Ok(Some(mut t)) = this.inner.store.thread_get(thread.id) {
                        t.title = title;
                        let _ = this.update_thread(t);
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::debug!(thread_id = %thread.id, %e, "title generation skipped"),
            }
        });
    }

    pub async fn generate_title(&self, thread: &Thread, first: &UserMessage) -> Result<Option<String>> {
        let settings = self.inner.settings.get();
        let mut kinds = vec![thread.provider.kind];
        if let Some(k) = settings.title_provider {
            kinds.insert(0, k);
        }
        kinds.push(ProviderKind::ClaudeCode);
        kinds.push(ProviderKind::Codex);
        let prompt = format!(
            "Write a title for a coding session that starts with the request below. Reply with the title only, with no label or preface such as \"Title:\": at most 6 words, sentence case, no quotes, no trailing period.\n\nRequest:\n{}",
            first.plain_text().chars().take(1500).collect::<String>()
        );
        let cwd = PathBuf::from(&thread.cwd);
        let mut seen = std::collections::HashSet::new();
        for kind in kinds {
            if !seen.insert(kind) {
                continue;
            }
            let Some(driver) = self.inner.drivers.get(kind) else { continue };
            let Some(env) = self.one_shot_env(thread, kind) else { continue };
            let _harness = self.inner.harness_gates[&kind].read().await;
            let binary = self.binary_for(kind);
            match driver.one_shot(&cwd, &prompt, binary.as_ref(), &env).await {
                Ok(text) => {
                    let title = clean_title(&text);
                    if !title.is_empty() {
                        return Ok(Some(title));
                    }
                }
                Err(kybern_drivers::DriverError::Unsupported(_)) => continue,
                Err(e) => tracing::debug!(%kind, %e, "title provider failed"),
            }
        }
        Ok(None)
    }

    /// The environment a one-shot helper call on behalf of `thread` runs with, or `None` when that harness
    /// must not be used for it. A helper never falls back to this machine's own login for a thread that
    /// has another account: the thread's account (a named account's own home, never the default one)
    /// is the only one its prompt may go to. A login borrowed through the broker lives only in the
    /// session's handle, so such a thread gets no model helper and keeps its plain title.
    pub(crate) fn one_shot_env(&self, thread: &Thread, kind: ProviderKind) -> Option<std::collections::BTreeMap<String, String>> {
        let borrowed = self.login_broker().is_some_and(|broker| broker.binding(thread.provider.kind, &thread.provider.instance).is_some());
        if kind == thread.provider.kind {
            return if borrowed { None } else { self.account_environment(thread).ok() };
        }
        if borrowed || thread.provider.instance != "default" {
            return None;
        }
        let settings = self.inner.settings.get();
        let provider = settings.providers.get(&kind).cloned().unwrap_or_default();
        crate::provider_accounts::environment(&provider, kind, "default").ok()
    }

    /// Ask a model for a short prompt answer using the thread's provider, falling back to others.
    async fn one_shot_any(&self, thread: &Thread, prompt: &str) -> Result<String> {
        let mut kinds = vec![thread.provider.kind, ProviderKind::ClaudeCode, ProviderKind::Codex];
        kinds.dedup();
        let cwd = PathBuf::from(&thread.cwd);
        let mut last: Option<anyhow::Error> = None;
        for kind in kinds {
            let Some(driver) = self.inner.drivers.get(kind) else { continue };
            let Some(env) = self.one_shot_env(thread, kind) else { continue };
            let _harness = self.inner.harness_gates[&kind].read().await;
            match driver.one_shot(&cwd, prompt, self.binary_for(kind).as_ref(), &env).await {
                Ok(t) if !t.trim().is_empty() => return Ok(t),
                Ok(_) => {}
                Err(kybern_drivers::DriverError::Unsupported(_)) => {}
                Err(e) => last = Some(e.into()),
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("no provider can generate text right now")))
    }

    pub async fn generate_commit_message(&self, thread: &Thread) -> Result<String> {
        let repo = Repo::new(&thread.cwd);
        let snapshot = repo.snapshot("kybern commit message").await?;
        let head = repo.head().await.unwrap_or_default();
        let diff = if head.is_empty() { String::new() } else { repo.diff(&head, &snapshot).await?.patch };
        let prompt = format!(
            "Write a git commit message for the diff below. First line: imperative, under 60 characters. Then a blank line and one to three short sentences explaining why. Reply with the message only.\n\n{}",
            diff.chars().take(20000).collect::<String>()
        );
        let text = self.one_shot_any(thread, &prompt).await?;
        Ok(text.trim().trim_matches('`').trim().to_string())
    }

    pub async fn generate_pr_text(&self, thread: &Thread, base: &str) -> Result<(String, String)> {
        let diff = crate::github::diff_against_base(std::path::Path::new(&thread.cwd), base).await.unwrap_or_default();
        let prompt = format!(
            "Write a pull request title and description for the diff below. Format exactly as:\nTITLE: <under 70 characters>\nBODY:\n<markdown with a short summary and a bullet list of changes>\n\nThread request: {}\n\nDiff:\n{}",
            thread.title,
            diff.chars().take(24000).collect::<String>()
        );
        let text = self.one_shot_any(thread, &prompt).await?;
        let mut title = thread.title.clone();
        let mut body = String::new();
        let mut in_body = false;
        for line in text.lines() {
            if let Some(t) = line.strip_prefix("TITLE:") {
                title = t.trim().to_string();
            } else if line.trim_start().starts_with("BODY:") {
                in_body = true;
            } else if in_body {
                body.push_str(line);
                body.push('\n');
            }
        }
        if body.trim().is_empty() {
            body = text;
        }
        Ok((title, body.trim().to_string()))
    }

    /// Turn uploaded image attachments into inline images so drivers can send them.
    fn resolve_attachments(&self, message: &mut UserMessage) {
        use base64::Engine;
        for part in message.parts.iter_mut() {
            if let ContentPart::Attachment { asset_id, media_type, .. } = part
                && media_type.starts_with("image/")
                && let Ok(bytes) = std::fs::read(self.inner.paths.assets.join(asset_id.to_string()))
            {
                *part =
                    ContentPart::Image { media_type: media_type.clone(), data: base64::engine::general_purpose::STANDARD.encode(bytes) };
            }
        }
    }

    pub async fn diff(&self, thread_id: ThreadId, turn_id: Option<TurnId>, include_patch: bool, path: Option<&str>) -> Result<Diff> {
        let thread = self.inner.store.thread_get(thread_id)?.ok_or_else(|| anyhow!("thread not found"))?;
        if group_git::is_group(&thread) {
            return group_git::group_diff(&self.inner.store, &thread, turn_id, include_patch, path, group_git::DiffCaps::UNLIMITED_FILES)
                .await;
        }
        let repo = Repo::new(&thread.cwd);
        if !Repo::is_repo(std::path::Path::new(&thread.cwd)).await {
            return Err(anyhow!("project is not a git repository"));
        }
        let (from, to) = match turn_id {
            Some(turn) => {
                let c = self.inner.store.checkpoint_get(turn)?.ok_or_else(|| anyhow!("no checkpoint for that turn"))?;
                let to = match c.after {
                    Some(a) => a,
                    None => repo.snapshot("kybern diff (in progress)").await?,
                };
                (c.before, to)
            }
            None => {
                let first = self
                    .inner
                    .store
                    .checkpoints_for_thread(thread_id)?
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow!("thread has no checkpoints yet"))?;
                (first.before, repo.snapshot("kybern diff (now)").await?)
            }
        };
        repo.diff_with_options(&from, &to, include_patch, path).await
    }

    /// Reset the working tree to the snapshot taken before `turn_id`.
    pub async fn revert(&self, thread_id: ThreadId, turn_id: TurnId) -> Result<(String, bool)> {
        self.ensure_not_subagent(thread_id)?;
        self.ensure_revertible(thread_id)?;
        // A finished turn's "after" snapshot may still be running, for minutes on a large tree. Wait for it without the
        // admission gate, so sends are not held back meanwhile; whatever started in that time is caught below.
        drop(self.checkpoint_lock(thread_id).lock_owned().await);
        // A group rewind rewrites several repositories: once, under the daemon-wide workspace lock.
        let group = self.inner.store.thread_get(thread_id)?.is_some_and(|t| group_git::is_group(&t));
        let _workspace = if group { Some(self.inner.workspace_ops.lock().await) } else { None };
        let admission = self.session_admission(thread_id).await;
        let _admission = admission.lock().await;
        let checkpoint = self.checkpoint_lock(thread_id).lock_owned().await;
        // Under both locks nothing can start a turn or a snapshot, so check again what the wait may have changed.
        let thread = self.ensure_revertible(thread_id).map_err(|error| {
            anyhow!("The thread started new work while the rewind waited for its last checkpoint ({error}). Rewind again when it finishes.")
        })?;
        let all = self.inner.store.checkpoints_for_thread(thread_id)?;
        let idx = all.iter().position(|c| c.turn_id == turn_id).ok_or_else(|| anyhow!("no checkpoint for that turn"))?;
        let c = all[idx].clone();
        if group_git::is_group(&thread) {
            self.group_restore(&thread, &c).await?;
        } else {
            Repo::new(&thread.cwd).restore(&c.before).await?;
        }
        drop(checkpoint);
        self.emit(thread_id, Some(turn_id), EventPayload::WorkspaceReverted { to_turn_id: turn_id, commit: c.before.clone() })?;

        if let Some(live) = self.inner.sessions.lock().await.remove(&thread_id) {
            live.mark_released();
            self.revoke_native_session(&live);
            let _ = live.session.close().await;
        }

        // Conversation rewind: the next session forks the provider conversation, keeping
        // turns before `turn_id`. If this is the first turn, start over with a fresh session.
        // Update the current record: a background title or binding change may have written it since.
        let mut t = self.inner.store.thread_get(thread_id)?.unwrap_or(thread);
        let driver_supports_fork = self.inner.drivers.get(t.provider.kind).is_some_and(|d| d.supports_fork());
        let anchors = |c: &Checkpoint| TurnAnchors { turn_id: c.provider_turn_id.clone(), previous_end: c.provider_turn_end.clone() };
        let conversation_rewound = if idx == 0 || t.provider_session_id.is_none() {
            t.provider_session_id = None;
            true
        } else if driver_supports_fork && (c.provider_turn_id.is_some() || all[idx - 1].provider_turn_end.is_some()) {
            let point = RewindPoint { drop_from: anchors(&c), keep_through: Some(anchors(&all[idx - 1])) };
            self.inner.pending_rewinds.lock().await.insert(thread_id, point);
            true
        } else {
            false
        };
        // Drop the checkpoints and events' effect of turns from `turn_id` on: keep the log
        // (history is append-only) but mark the thread so the transcript shows the cut.
        t.status = ThreadStatus::Idle;
        self.update_thread(t)?;
        Ok((c.before, conversation_rewound))
    }

    /// The thread, if nothing is running in it that a rewind would pull the files from under.
    fn ensure_revertible(&self, thread_id: ThreadId) -> Result<Thread> {
        let thread = self.inner.store.thread_get(thread_id)?.ok_or_else(|| anyhow!("thread not found"))?;
        if matches!(thread.status, ThreadStatus::Running | ThreadStatus::AwaitingApproval) {
            return Err(anyhow!("thread is busy; interrupt it first"));
        }
        if self.inner.store.runtime_tasks_for_thread(thread_id)?.iter().any(|task| task.status.is_active()) {
            return Err(anyhow!("background work is active; stop it before rewinding"));
        }
        Ok(thread)
    }

    /// `@Computer`: warm the driver, and in a chat whose process started before
    /// computer use was available, close the idle process so this turn resumes
    /// with the tools.
    /// The thread is already Running for this turn, so only the process's own
    /// work counts as busy.
    async fn prepare_for_computer(&self, thread: &Thread, message: &UserMessage) {
        if !crate::computer::ComputerUse::mentions(message)
            || !self.native_tool_restrictions(thread).is_ok_and(|restrictions| self.computer_tools_offered(thread, &restrictions))
        {
            return;
        }
        self.inner.computer.prewarm();
        let live = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(live) = sessions.get(&thread.id).cloned() else { return };
            if live.computer_tools
                || live.turn.lock().await.is_some()
                || live.tasks.lock().await.values().any(|task| task.status.is_active())
                || !live.pending.lock().await.is_empty()
            {
                return;
            }
            sessions.remove(&thread.id);
            live
        };
        live.mark_released();
        self.revoke_native_session(&live);
        if let Err(error) = live.session.close().await {
            tracing::debug!(%error, thread_id = %thread.id, "closing a session without computer tools failed");
        }
        let _ = self.emit(thread.id, None, EventPayload::ProviderSessionReleased { reason: SessionReleaseReason::Manual });
    }

    async fn ensure_session(&self, thread: &Thread) -> Result<(Arc<LiveSession>, bool)> {
        let migrated = self.migrate_cursor_model(thread.clone())?;
        let thread = &migrated;
        let expected = self.session_identity(thread)?;
        let current = self.inner.sessions.lock().await.get(&thread.id).cloned();
        if let Some(live) = current {
            let identity = self.inner.store.meta_get(&format!("live_identity:{}", thread.id))?;
            if identity.as_deref().is_none_or(|identity| identity == expected) {
                return Ok((live, true));
            }
            self.retire_or_close_session(thread.id, live, identity.unwrap_or_default()).await?;
        }
        let retained = self.inner.retained_sessions.lock().await.remove(&(thread.id, expected.clone()));
        if let Some((owner, live)) = retained {
            if owner == thread.provider && !live.is_released() {
                live.retained.store(false, Ordering::Relaxed);
                self.inner.sessions.lock().await.insert(thread.id, live.clone());
                self.inner.store.meta_set_many(&[
                    (&format!("live_fingerprint:{}", thread.id), &self.environment_fingerprint(thread)?),
                    (&format!("live_identity:{}", thread.id), &expected),
                    (&format!("live_owner:{}", thread.id), &serde_json::to_string(&thread.provider)?),
                ])?;
                return Ok((live, true));
            }
            live.mark_released();
            self.revoke_native_session(&live);
            live.session.close().await?;
        }
        let waiting = self.inner.releasing.lock().await.get(&thread.id).cloned();
        if let Some(mut waiting) = waiting {
            let _ = waiting.changed().await;
        }
        let rewind = self.inner.pending_rewinds.lock().await.remove(&thread.id);
        self.spawn_session(thread, rewind).await.map(|live| (live, false))
    }

    async fn spawn_session(&self, thread: &Thread, rewind: Option<RewindPoint>) -> Result<Arc<LiveSession>> {
        let driver = self
            .inner
            .drivers
            .get(thread.provider.kind)
            .ok_or_else(|| anyhow!("provider {} is not available in this build", thread.provider.kind))?;
        let project = self.inner.store.project_get(thread.project_id)?.ok_or_else(|| anyhow!("Project not found"))?;
        let mut provider_settings =
            crate::settings::provider_settings(&self.inner.settings.get(), thread.provider.kind, Some(&project.path));
        provider_settings.env = self.account_environment(thread)?;
        // An account that signs in through the login broker (S4) borrows its login for this session.
        let borrowed_handle = self.borrowed_login(thread, &mut provider_settings.env).await?;
        let mut profile_binding = None;
        if thread.provider.kind == ProviderKind::Omp && thread.provider.instance == "default" {
            // A resumed chat must keep the profile that owns its native session,
            // even after project defaults change or the daemon releases it at idle.
            let key = format!("omp_profile:{}", thread.id);
            let profile = match self.inner.store.meta_get(&key)? {
                Some(profile) => profile,
                None => kybern_drivers::omp_profile::resolve(&provider_settings.env)?,
            };
            provider_settings.env.insert("OMP_PROFILE".into(), profile.clone());
            profile_binding = Some((key, profile));
        }
        let session_instance_id = Uuid::now_v7();
        let restrictions = self.native_tool_restrictions(thread)?;
        if thread.coordinator_project_id.is_some() {
            self.refresh_project_map(thread.project_id).await;
            self.record_first_session_heads(thread).await;
        }
        if !is_free_chat_project(thread.project_id) {
            // The notes index states which notes may be outdated; check them off the runtime first.
            self.warm_stale_flags(thread.project_id).await;
        }
        let instructions = self.session_instructions(thread)?;
        let brief_revision = instructions.as_ref().map(|instructions| instructions.brief_revision);
        let lead_settings = instructions.as_ref().and_then(|instructions| instructions.lead_settings.clone());
        let coordinator_instructions = instructions.map(|instructions| instructions.text);
        let computer_tools = self.computer_tools_offered(thread, &restrictions);
        // Antigravity reaches the gateway through the one global `kybern` MCP entry (`kybern
        // mcp-bridge`), which reads this session's endpoint and credential from the environment
        // of each turn. Without that entry, or with the setting off, it gets no tools and no
        // gateway session; its guide is then rendered from an empty tool list so it never
        // describes a tool `agy` cannot call. The driver puts the guide at the top of the
        // conversation's first prompt either way.
        let antigravity_tools = if thread.provider.kind == ProviderKind::Antigravity && self.inner.native_tools.is_some() {
            self.antigravity_tools_ready(thread).await
        } else {
            false
        };
        let native_tool_bridge = if thread.provider.kind == ProviderKind::Antigravity && !antigravity_tools {
            let guide = self.agent_guide(&[], &restrictions);
            (guide.is_some() || coordinator_instructions.is_some()).then(|| kybern_drivers::NativeToolBridge {
                server_name: "kybern".into(),
                endpoint: None,
                authorization: None,
                coordinator_instructions: coordinator_instructions.clone(),
                guide,
                tools: Vec::new(),
                restrictions: restrictions.clone(),
            })
        } else {
            self.inner
                .native_tools
                .as_ref()
                .map(|gateway| {
                    let mut tools = crate::app_tools::native_tool_definitions();
                    // Privacy permissions are a macOS thing.
                    tools.retain(|tool| cfg!(target_os = "macos") || tool.name != PERMISSION_REQUEST_TOOL);
                    if thread.coordinator_project_id.is_some() {
                        // Only the project Coordinator is offered the git tool.
                        tools.push(crate::lead_git::tool_definition());
                    }
                    if self.computer_available(thread) {
                        tools.extend(crate::computer::ComputerUse::tool_definitions());
                    }
                    if thread.coordinator_project_id.is_some() {
                        tools.extend(lead_proposals::tool_definitions());
                    }
                    self.offer_machine_tools(thread, &mut tools);
                    let guide = self.agent_guide(&tools, &restrictions);
                    gateway.register_coordinator(thread.id, session_instance_id, tools, restrictions, coordinator_instructions, guide)
                })
                .transpose()?
        };
        // Claude Code gives up on an MCP tool call after about a minute, which would end a
        // sign-in, a permission request or a question with a deadline while the user is
        // still deciding. The daemon bounds every call itself (the longest waits are a few
        // hours), so leave the limit to it unless the user set their own. Newer Claude Code
        // also aborts a call with no progress for 5 min; the `timeout` of Kybern's own server
        // entry (`claude.rs`) lifts that for Kybern alone. `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT`
        // stays unset: it would let every other MCP server of the user hang a turn for hours.
        if thread.provider.kind == ProviderKind::ClaudeCode && !provider_settings.env.contains_key("MCP_TOOL_TIMEOUT") {
            provider_settings.env.insert("MCP_TOOL_TIMEOUT".into(), "21600000".into());
        }
        // Commands in the session can ask for a password through a private card.
        // Codex runs commands in a sandbox that can't reach the daemon outside
        // Full access, so it only gets the helper there.
        let askpass_allowed = thread.provider.kind != ProviderKind::Codex || thread.permission_mode == PermissionMode::FullAccess;
        if askpass_allowed && let Some(env) = self.askpass_env(thread.id, session_instance_id) {
            provider_settings.env.extend(env);
        }
        // Cursor and Antigravity have no system channel: the block rides in the conversation, once.
        let resume_lacks_instructions = match &thread.provider_session_id {
            Some(id) if carries_instructions_in_conversation(thread.provider.kind) && rewind.is_none() => {
                self.inner.store.meta_get(&instructions_key(id))?.is_none()
            }
            _ => false,
        };
        let instructions_unconfirmed = carries_instructions_in_conversation(thread.provider.kind)
            && (thread.provider_session_id.is_none() || resume_lacks_instructions)
            && native_tool_bridge.as_ref().is_some_and(|bridge| bridge.provider_instructions().is_some());
        // Cursor's account host reuses the daemon catalog; old entries lacking
        // native max-mode metadata deliberately fall back to one SDK discovery.
        if thread.provider.kind == ProviderKind::Cursor
            && let Some((statuses, _)) = self.inner.provider_catalogs.last_known(&thread.project_id.to_string())
            && let Some(status) = statuses.iter().find(|status| status.kind == ProviderKind::Cursor)
        {
            let settings = self.inner.settings.get();
            let provider = settings.providers.get(&ProviderKind::Cursor).cloned().unwrap_or_default();
            if thread.provider.instance == crate::provider_accounts::resolve(&provider, Some(&project.path), None) {
                provider_settings.env.insert("KYBERN_CURSOR_CATALOG".into(), serde_json::to_string(&status.models)?);
            }
        }
        let config = SessionConfig {
            cwd: PathBuf::from(&thread.cwd),
            model: thread.model.clone(),
            effort: thread.effort.clone(),
            permission_mode: thread.permission_mode,
            resume_session_id: thread.provider_session_id.clone(),
            resume_lacks_instructions,
            fork: rewind.is_some(),
            rewind,
            binary: provider_settings.binary.map(PathBuf::from),
            env: provider_settings.env.into_iter().collect(),
            native_tool_bridge,
        };
        let spawned = driver.spawn(config).await;
        // The drivers resolved the lent login's handle while spawning; nothing needs it afterwards.
        if let Some(handle) = &borrowed_handle {
            kybern_drivers::login_lend::unregister(handle);
        }
        let SpawnedSession { session, events } = match spawned {
            Ok(spawned) => spawned,
            Err(error) => {
                if let Some(gateway) = &self.inner.native_tools {
                    gateway.revoke(session_instance_id);
                }
                return Err(error.into());
            }
        };
        // A failed executable launch must not pin a profile before a session exists.
        if let Some((key, profile)) = profile_binding
            && let Err(error) = self.inner.store.meta_set(&key, &profile)
        {
            if let Some(gateway) = &self.inner.native_tools {
                gateway.revoke(session_instance_id);
            }
            let _ = session.close().await;
            return Err(error);
        }
        let live = Arc::new(LiveSession {
            session_instance_id,
            session,
            computer_tools,
            last_activity: std::sync::Mutex::new(SessionActivityTime::now()),
            released: AtomicBool::new(false),
            retained: AtomicBool::new(false),
            stop_cleanup: AtomicBool::new(false),
            turn: Mutex::new(None),
            continuation: Mutex::new(None),
            turn_ready: tokio::sync::Notify::new(),
            last_turn_id: Mutex::new(None),
            tasks: Mutex::new(HashMap::new()),
            task_aliases: Mutex::new(HashMap::new()),
            deferred_checkpoints: Mutex::new(HashSet::new()),
            pending: Mutex::new(HashMap::new()),
            daemon_approvals: Mutex::new(HashMap::new()),
            app_tool_requests: Mutex::new(HashSet::new()),
            app_tool_permits: Arc::new(Semaphore::new(crate::app_tools::MAX_CONCURRENT_REQUESTS)),
            brief_revision: Mutex::new(None),
            lead_settings: Mutex::new(None),
            context_usage: std::sync::Mutex::new(None),
            instructions_unconfirmed: AtomicBool::new(instructions_unconfirmed),
        });
        self.inner.store.meta_set_many(&[
            (&format!("live_fingerprint:{}", thread.id), &self.environment_fingerprint(thread)?),
            (&format!("live_identity:{}", thread.id), &self.session_identity(thread)?),
            (&format!("live_owner:{}", thread.id), &serde_json::to_string(&thread.provider)?),
        ])?;
        *live.brief_revision.lock().await = brief_revision;
        *live.lead_settings.lock().await = lead_settings;
        self.inner.sessions.lock().await.insert(thread.id, live.clone());
        let this = self.clone();
        let thread_id = thread.id;
        let pump_live = live.clone();
        tokio::spawn(async move { this.pump(thread_id, pump_live, events).await });
        Ok(live)
    }

    /// The askpass variables for one session, when the daemon can be reached
    /// over loopback and the bundled CLI is known.
    fn askpass_env(&self, thread_id: ThreadId, session_instance_id: Uuid) -> Option<std::collections::BTreeMap<String, String>> {
        let gateway = self.inner.native_tools.as_ref()?;
        let cli = crate::askpass::find_cli()?;
        let script = crate::askpass::install_script(&self.inner.paths.root, &cli).ok()?;
        let (url, token) = gateway.register_askpass(thread_id, session_instance_id)?;
        Some(crate::askpass::session_env(&script, &url, &token))
    }

    /// A machine that is in a network with others can run tools there (design §6.3). A thread another
    /// machine started never can: it would be a second hop.
    #[inline(never)]
    fn offer_machine_tools(&self, thread: &Thread, tools: &mut Vec<kybern_drivers::NativeToolDefinition>) {
        if !fork_machine::second_hop(thread) && self.remote_hub().is_some_and(|hub| !hub.targets().is_empty()) {
            crate::network::forward::offer_machine_args(tools);
            tools.push(crate::network::forward::machines_tool_definition());
            tools.push(crate::network::forward::add_to_machine_tool_definition());
        }
    }

    /// Names of the other machines where computer use is ready: online, not
    /// paused, and listing `computer.use.v1`. Empty for a thread another machine
    /// started, which may not reach a third one.
    fn computer_machines(&self, thread: &Thread) -> Vec<String> {
        let Some(hub) = self.remote_hub() else { return Vec::new() };
        let may_forward = !fork_machine::second_hop(thread) && thread.inbound.is_none();
        crate::network::forward::computer_ready_machines(&hub.targets(), may_forward)
    }

    /// The computer tools belong in this thread's sessions: this machine's are
    /// offerable, or a machine of the network has them ready and this thread may
    /// reach it.
    fn computer_available(&self, thread: &Thread) -> bool {
        self.inner.computer.offered_to(thread.provider.kind)
            || (crate::computer::ComputerUse::offered_for_remote(thread.provider.kind) && !self.computer_machines(thread).is_empty())
    }

    /// Whether a session spawned now would expose the computer-use tools.
    fn computer_tools_offered(&self, thread: &Thread, restrictions: &kybern_drivers::NativeToolRestrictions) -> bool {
        const PROBE: &str = "kybern_computer_act";
        self.inner.native_tools.is_some()
            && self.computer_available(thread)
            && (restrictions.allowed_tools.is_empty() || restrictions.allowed_tools.iter().any(|tool| tool == PROBE))
            && !restrictions.denied_tools.iter().any(|tool| tool == PROBE)
    }

    /// Whether an Antigravity session can use Kybern's tools: the setting is on and the
    /// `kybern` entry in `agy`'s MCP servers is present and Kybern's (re-added when it
    /// is missing or points at a command that is gone). When it cannot, the thread says why.
    async fn antigravity_tools_ready(&self, thread: &Thread) -> bool {
        let settings = self.inner.settings.get();
        if !settings.antigravity.kybern_tools {
            return false;
        }
        match crate::antigravity_mcp::turn_on(&settings).await {
            Ok(()) => true,
            Err(error) => {
                let _ = self.emit(
                    thread.id,
                    None,
                    EventPayload::ProviderNotice {
                        level: NoticeLevel::Warning,
                        text: format!("Antigravity is running without Kybern's tools. {error}"),
                        data: None,
                    },
                );
                false
            }
        }
    }

    /// The "Working in Kybern" guide for a session with these tools, or `None`
    /// when the user turned "Tell agents about Kybern" off.
    fn agent_guide(
        &self,
        tools: &[kybern_drivers::NativeToolDefinition],
        restrictions: &kybern_drivers::NativeToolRestrictions,
    ) -> Option<String> {
        if !self.inner.settings.get().tell_agents_about_kybern {
            return None;
        }
        let names = tools.iter().map(|tool| tool.name.as_str()).filter(|name| restrictions.permits(name));
        let guide = crate::agent_guide::render(&crate::agent_guide::GuideTools::from_names(names));
        tracing::debug!(version = crate::agent_guide::GUIDE_VERSION, bytes = guide.len(), "attaching the Kybern guide to a new session");
        Some(guide)
    }

    async fn persist_runtime_task_start(
        &self,
        thread_id: ThreadId,
        live: &Arc<LiveSession>,
        turn_id: Option<TurnId>,
        mut incoming: DriverRuntimeTask,
    ) -> Result<RuntimeTask> {
        let raw_id = incoming.id.clone();
        let alias = live.task_aliases.lock().await.get(&raw_id).cloned();
        if let Some(alias) = alias {
            incoming.id = alias;
        } else if !live.tasks.lock().await.contains_key(&raw_id)
            && self.inner.store.runtime_tasks_for_thread(thread_id)?.iter().any(|task| task.id == raw_id)
        {
            incoming.id = format!("native:{}:{raw_id}", live.session_instance_id);
            live.task_aliases.lock().await.insert(raw_id, incoming.id.clone());
        }
        if let Some(parent) = incoming.parent_id.as_mut() {
            let existing = live.task_aliases.lock().await.get(parent).cloned();
            if let Some(alias) = existing {
                *parent = alias;
            } else if !live.tasks.lock().await.contains_key(parent)
                && self.inner.store.runtime_tasks_for_thread(thread_id)?.iter().any(|task| task.id == *parent)
            {
                // Native child rosters can precede their parent's start frame.
                // Reserve the owning parent's alias before linking the child.
                let alias = format!("native:{}:{parent}", live.session_instance_id);
                live.task_aliases.lock().await.insert(parent.clone(), alias.clone());
                *parent = alias;
            }
        }
        let origin_turn_id = match turn_id.or(*live.last_turn_id.lock().await) {
            Some(turn_id) => turn_id,
            None => self
                .inner
                .store
                .events_for_thread(thread_id)?
                .iter()
                .rev()
                .find_map(|event| matches!(event.payload, EventPayload::TurnStarted { .. }).then_some(event.turn_id).flatten())
                .ok_or_else(|| anyhow!("provider reported a task before the thread had a turn"))?,
        };
        let now = Utc::now();
        let mut tasks = live.tasks.lock().await;
        let (task, started) = match tasks.get(&incoming.id) {
            Some(existing) => {
                let mut task = existing.clone();
                task.kind = incoming.kind;
                // Provider retries and aggregate rosters can replay a start
                // after terminal evidence. Runtime task state is monotonic.
                if task.status.is_active() || !incoming.status.is_active() {
                    task.status = incoming.status;
                }
                task.title = incoming.title;
                task.detail = incoming.detail.or(task.detail);
                task.provider_type = incoming.provider_type.or(task.provider_type);
                task.parent_id = incoming.parent_id.or(task.parent_id);
                task.tool_call_id = incoming.tool_call_id.or(task.tool_call_id);
                task.provider_thread_id = incoming.provider_thread_id.or(task.provider_thread_id);
                task.model = incoming.model.or(task.model);
                task.effort = incoming.effort.or(task.effort);
                task.backgrounded |= incoming.backgrounded;
                task.last_tool_name = incoming.last_tool_name.or(task.last_tool_name);
                task.usage = incoming.usage.or(task.usage);
                merge_task_stats(&mut task.stats, incoming.stats);
                task.capabilities = if task.status.is_active() { incoming.capabilities } else { RuntimeTaskCapabilities::default() };
                task.updated_at = now;
                if !task.status.is_active() {
                    task.completed_at.get_or_insert(now);
                }
                (task, false)
            }
            None => (
                RuntimeTask {
                    id: incoming.id,
                    thread_id,
                    origin_turn_id,
                    started_seq: 0,
                    updated_seq: 0,
                    kind: incoming.kind,
                    status: incoming.status,
                    title: incoming.title,
                    detail: incoming.detail,
                    provider_type: incoming.provider_type,
                    parent_id: incoming.parent_id,
                    tool_call_id: incoming.tool_call_id,
                    provider_thread_id: incoming.provider_thread_id,
                    model: incoming.model,
                    effort: incoming.effort,
                    backgrounded: incoming.backgrounded,
                    last_tool_name: incoming.last_tool_name,
                    usage: incoming.usage,
                    stats: incoming.stats,
                    capabilities: incoming.capabilities,
                    started_at: now,
                    updated_at: now,
                    completed_at: (!incoming.status.is_active()).then_some(now),
                },
                true,
            ),
        };
        tasks.insert(task.id.clone(), task.clone());
        drop(tasks);
        let payload = if started {
            EventPayload::RuntimeTaskStarted { task: task.clone() }
        } else if task.status.is_active() {
            EventPayload::RuntimeTaskUpdated { task: task.clone() }
        } else {
            EventPayload::RuntimeTaskCompleted { task: task.clone() }
        };
        let emitted = self.emit(thread_id, Some(task.origin_turn_id), payload)?;
        let task = match emitted.payload {
            EventPayload::RuntimeTaskStarted { task }
            | EventPayload::RuntimeTaskUpdated { task }
            | EventPayload::RuntimeTaskCompleted { task } => task,
            _ => unreachable!("runtime task emission changed payload kind"),
        };
        live.tasks.lock().await.insert(task.id.clone(), task.clone());
        if let Err(error) = self.subagent_sync(thread_id, &task, Some(live.session_instance_id)) {
            tracing::warn!(%thread_id, task_id = %task.id, %error, "could not update the subagent thread");
        }
        if !task.status.is_active() {
            self.finish_deferred_checkpoint(thread_id, live, task.origin_turn_id).await?;
        }
        Ok(task)
    }

    async fn apply_runtime_task_update(
        &self,
        thread_id: ThreadId,
        live: &Arc<LiveSession>,
        mut update: DriverRuntimeTaskUpdate,
        kind: RuntimeTaskUpdateKind,
    ) -> Result<Option<RuntimeTask>> {
        if let Some(alias) = live.task_aliases.lock().await.get(&update.id).cloned() {
            update.id = alias;
        }
        let mut tasks = live.tasks.lock().await;
        let Some(task) = tasks.get_mut(&update.id) else {
            tracing::debug!(thread_id = %thread_id, task_id = %update.id, "provider updated an unknown runtime task");
            return Ok(None);
        };
        let completed = kind == RuntimeTaskUpdateKind::Complete;
        let resumed = kind == RuntimeTaskUpdateKind::Resume;
        let next_status = match (update.status, completed) {
            (Some(status), true) if status.is_active() => RuntimeTaskStatus::Completed,
            (Some(status), _) => status,
            (None, true) => RuntimeTaskStatus::Completed,
            (None, false) => task.status,
        };
        if resumed || task.status.is_active() || !next_status.is_active() {
            task.status = next_status;
        }
        if let Some(detail) = update.detail {
            task.detail = Some(detail);
        }
        if let Some(backgrounded) = update.backgrounded {
            task.backgrounded = backgrounded;
        }
        if let Some(last_tool_name) = update.last_tool_name {
            task.last_tool_name = Some(last_tool_name);
        }
        if let Some(usage) = update.usage {
            task.usage = Some(usage);
        }
        if let Some(stats) = update.stats {
            merge_task_stats(&mut task.stats, stats);
        }
        if let Some(capabilities) = update.capabilities {
            task.capabilities = capabilities;
        }
        task.updated_at = Utc::now();
        if resumed {
            task.completed_at = None;
        } else if completed || !task.status.is_active() {
            task.completed_at = Some(task.updated_at);
            task.capabilities = RuntimeTaskCapabilities::default();
        }
        let task = task.clone();
        drop(tasks);
        let payload = if resumed {
            EventPayload::RuntimeTaskStarted { task: task.clone() }
        } else if completed || !task.status.is_active() {
            EventPayload::RuntimeTaskCompleted { task: task.clone() }
        } else {
            EventPayload::RuntimeTaskUpdated { task: task.clone() }
        };
        let emitted = self.emit(thread_id, Some(task.origin_turn_id), payload)?;
        let task = match emitted.payload {
            EventPayload::RuntimeTaskStarted { task }
            | EventPayload::RuntimeTaskUpdated { task }
            | EventPayload::RuntimeTaskCompleted { task } => task,
            _ => unreachable!("runtime task emission changed payload kind"),
        };
        live.tasks.lock().await.insert(task.id.clone(), task.clone());
        if let Err(error) = self.subagent_sync(thread_id, &task, Some(live.session_instance_id)) {
            tracing::warn!(%thread_id, task_id = %task.id, %error, "could not update the subagent thread");
        }
        if !resumed && (completed || !task.status.is_active()) {
            self.finish_deferred_checkpoint(thread_id, live, task.origin_turn_id).await?;
        }
        Ok(Some(task))
    }

    async fn finish_deferred_checkpoint(&self, thread_id: ThreadId, live: &Arc<LiveSession>, turn_id: TurnId) -> Result<()> {
        let any_active = live.tasks.lock().await.values().any(|task| task.origin_turn_id == turn_id && task.status.is_active());
        if any_active || !live.deferred_checkpoints.lock().await.remove(&turn_id) {
            return Ok(());
        }
        if let Some(thread) = self.inner.store.thread_get(thread_id)? {
            self.checkpoint_after_turn(&thread, turn_id).await;
        }
        Ok(())
    }

    async fn interrupt_runtime_tasks(&self, thread_id: ThreadId, live: &Arc<LiveSession>, detail: &str) {
        let ids = live.tasks.lock().await.values().filter(|task| task.status.is_active()).map(|task| task.id.clone()).collect::<Vec<_>>();
        for id in ids {
            let _ = self
                .apply_runtime_task_update(
                    thread_id,
                    live,
                    DriverRuntimeTaskUpdate {
                        id,
                        status: Some(RuntimeTaskStatus::Interrupted),
                        detail: Some(detail.into()),
                        backgrounded: None,
                        last_tool_name: None,
                        usage: None,
                        stats: None,
                        capabilities: None,
                    },
                    RuntimeTaskUpdateKind::Complete,
                )
                .await;
        }
    }

    /// Only call while retiring the current session behind the resume barrier.
    /// A prior broken transport may have left durable work absent from the
    /// current handle's task map. Include it in the same terminal projection.
    async fn restore_active_runtime_tasks(&self, thread_id: ThreadId, live: &Arc<LiveSession>) -> Result<()> {
        let stored = self.inner.store.runtime_tasks_for_thread(thread_id)?;
        let owners = self.inner.retained_sessions.lock().await.values().map(|(_, owner)| owner.clone()).collect::<Vec<_>>();
        let mut retained_ids = HashSet::new();
        for owner in owners {
            retained_ids.extend(owner.tasks.lock().await.keys().cloned());
        }
        let mut tasks = live.tasks.lock().await;
        for task in stored.into_iter().filter(|task| task.status.is_active() && !retained_ids.contains(&task.id)) {
            tasks.entry(task.id.clone()).or_insert(task);
        }
        Ok(())
    }

    /// Translate driver events into thread events until the provider exits.
    async fn pump(self, thread_id: ThreadId, live: Arc<LiveSession>, mut events: tokio::sync::mpsc::Receiver<DriverEvent>) {
        while let Some(ev) = events.recv().await {
            // Exited is terminal even when the public driver handle retains a
            // sender. Waiting for channel EOF would keep that handle (and its
            // background tasks) alive forever, notably with Pi/OMP.
            let exited = matches!(ev, DriverEvent::Exited { .. });
            if let Err(e) = self.handle_driver_event(thread_id, &live, ev).await {
                tracing::error!(%thread_id, error = %e, "failed to persist driver event");
            }
            if exited {
                break;
            }
        }
        self.inner.retained_sessions.lock().await.retain(|_, (_, current)| !Arc::ptr_eq(current, &live));
        // An idle session retired for an update must not remove its replacement.
        let cleanup_barrier = {
            let mut sessions = self.inner.sessions.lock().await;
            // Forced interruption owns task and turn settlement. Other exits
            // (including shutdown/archive) still need the cleanup below.
            if live.stop_cleanup.load(Ordering::Relaxed) {
                return;
            }
            if sessions.get(&thread_id).is_some_and(|current| Arc::ptr_eq(current, &live)) {
                let (done, waiting) = tokio::sync::watch::channel(());
                self.inner.releasing.lock().await.insert(thread_id, waiting);
                live.mark_released();
                sessions.remove(&thread_id);
                self.revoke_native_session(&live);
                Some(done)
            } else {
                None
            }
        };
        if cleanup_barrier.is_some()
            && let Err(error) = self.restore_active_runtime_tasks(thread_id, &live).await
        {
            tracing::error!(%thread_id, %error, "failed to recover tasks from the exited provider");
        }
        self.interrupt_runtime_tasks(thread_id, &live, "Provider exited before this work finished").await;
        let turn = live.turn.lock().await.take();
        if let Some(mut turn) = turn.filter(|t| !t.completed) {
            // A handoff turn is invisible: its rotation goes on without a handoff.
            if let Some(internal) = turn.internal.as_mut() {
                internal.finish(Err("The session exited before it wrote a handoff.".into()));
            } else {
                self.fail_exited_turn(thread_id, &live, turn.id).await;
            }
        }
        if cleanup_barrier.is_some() {
            self.inner.releasing.lock().await.remove(&thread_id);
        }
        drop(cleanup_barrier);
    }

    /// The provider exited in the middle of a user's turn.
    async fn fail_exited_turn(&self, thread_id: ThreadId, live: &Arc<LiveSession>, turn_id: TurnId) {
        let _ = self.resolve_finished_requests(thread_id, turn_id, live).await;
        let _ = self.emit(thread_id, Some(turn_id), EventPayload::TurnFailed { error: "provider exited before finishing the turn".into() });
        if let Ok(Some(mut t)) = self.inner.store.thread_get(thread_id) {
            t.status = ThreadStatus::Failed;
            let _ = self.update_thread(t);
        }
        self.delegation_turn_finished(
            thread_id,
            turn_id,
            delegation::TurnOutcome::Failed("provider exited before finishing the turn".into()),
        );
    }

    async fn owns_app_tool_turn(&self, thread_id: ThreadId, live: &Arc<LiveSession>, expected_turn: Option<TurnId>) -> Option<TurnId> {
        if live.is_released() || live.stop_cleanup.load(Ordering::Relaxed) {
            return None;
        }
        let current = self.inner.sessions.lock().await.get(&thread_id).cloned();
        if !current.as_ref().is_some_and(|current| Arc::ptr_eq(current, live)) {
            return None;
        }
        let turn = live.turn.lock().await;
        // A handoff turn has no tools: it only writes text.
        turn.as_ref()
            .filter(|turn| !turn.completed && turn.internal.is_none() && expected_turn.is_none_or(|expected| expected == turn.id))
            .map(|turn| turn.id)
    }

    async fn queue_app_tool_request(
        &self,
        thread_id: ThreadId,
        live: Arc<LiveSession>,
        request_id: String,
        name: String,
        arguments: serde_json::Value,
    ) {
        if request_id.is_empty() || request_id.len() > 128 {
            if request_id.len() <= 512 {
                reject_app_tool_request(live, request_id, "invalid app tool request id");
            }
            return;
        }
        if name.is_empty() || name.len() > 64 || !name.bytes().all(|byte| byte.is_ascii_lowercase() || byte == b'_') {
            reject_app_tool_request(live, request_id, "invalid app tool name");
            return;
        }
        if serde_json::to_vec(&arguments).map_or(true, |encoded| encoded.len() > crate::app_tools::argument_limit(&name)) {
            reject_app_tool_request(live, request_id, "tool arguments exceed the size limit for this tool");
            return;
        }
        let Some(turn_id) = self.owns_app_tool_turn(thread_id, &live, None).await else {
            reject_app_tool_request(live, request_id, "app tool request has no active owning turn");
            return;
        };
        {
            let mut requests = live.app_tool_requests.lock().await;
            if !requests.insert(request_id.clone()) {
                drop(requests);
                reject_app_tool_request(live, request_id, "duplicate app tool request id");
                return;
            }
        }
        let permit = match live.app_tool_permits.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                live.app_tool_requests.lock().await.remove(&request_id);
                reject_app_tool_request(live, request_id, "too many concurrent app tool requests");
                return;
            }
        };

        let this = self.clone();
        tokio::spawn(async move {
            let mut result = if this.owns_app_tool_turn(thread_id, &live, Some(turn_id)).await.is_none() {
                Err("app tool request is stale".to_string())
            } else {
                let timeout = if let Some(budget) = crate::secrets::call_budget(&name, &arguments)
                    .or_else(|| crate::lead_git::call_budget(&name, &arguments))
                    .or_else(|| crate::visuals::call_budget(&name, &arguments))
                    .or_else(|| crate::pick::call_budget(&name, &arguments))
                    .or_else(|| request_call_budget(&name, &arguments))
                {
                    budget
                } else if delegation::BLOCKING_TOOLS.contains(&name.as_str())
                    || (name == "kybern_thread_send" && arguments.get("wait_for_reply").and_then(serde_json::Value::as_bool) == Some(true))
                    || matches!(name.as_str(), "kybern_html_preview" | "kybern_html_publish")
                    || crate::computer::ComputerUse::is_tool(&name)
                    || agent_items::WRITE_TOOLS.contains(&name.as_str())
                {
                    Duration::from_secs(65)
                } else {
                    crate::app_tools::REQUEST_TIMEOUT
                };
                match tokio::time::timeout(
                    timeout,
                    this.execute_native_app_tool_call(thread_id, live.session_instance_id, &request_id, &name, arguments),
                )
                .await
                {
                    Ok(Ok(value)) => Ok(value),
                    Ok(Err(error)) => Err(bounded_app_tool_error(error.to_string())),
                    Err(_) => Err("app tool request timed out".to_string()),
                }
            };
            if this.owns_app_tool_turn(thread_id, &live, Some(turn_id)).await.is_none() {
                result = Err("app tool request expired because its owning turn ended".to_string());
            }
            let response = tokio::time::timeout(Duration::from_secs(5), live.session.respond_app_tool(&request_id, result)).await;
            if let Ok(Err(error)) = response {
                tracing::debug!(%thread_id, %request_id, %error, "failed to return app tool result");
            }
            live.app_tool_requests.lock().await.remove(&request_id);
            drop(permit);
        });
    }

    async fn handle_driver_event(&self, thread_id: ThreadId, live: &Arc<LiveSession>, ev: DriverEvent) -> Result<()> {
        if live.retained.load(Ordering::Relaxed) {
            return self.handle_retained_event(thread_id, live, ev).await;
        }
        match ev {
            DriverEvent::AppToolRequest { request_id, name, arguments } => {
                self.queue_app_tool_request(thread_id, live.clone(), request_id, name, arguments).await;
                Ok(())
            }
            event => self.process_driver_event(thread_id, live, event, false).await,
        }
    }

    async fn process_driver_event(&self, thread_id: ThreadId, live: &Arc<LiveSession>, ev: DriverEvent, retiring: bool) -> Result<()> {
        live.touch();
        if let DriverEvent::SubagentMessageDelivered { task_id, message_id } = &ev {
            self.acknowledge_subagent_message(thread_id, live.session_instance_id, task_id, message_id)?;
            return Ok(());
        }
        // A subagent's own prose and tool calls belong to its child thread and
        // never reach the parent's log or its turn bookkeeping.
        if live.stop_cleanup.load(Ordering::Relaxed) && !retiring {
            return Ok(());
        }
        let Some(ev) = self.subagent_take_event(thread_id, ev)? else { return Ok(()) };
        let starts_response = match &ev {
            DriverEvent::ResponseStarted => true,
            DriverEvent::TextDelta { origin, .. }
            | DriverEvent::ThinkingDelta { origin, .. }
            | DriverEvent::MessageCompleted { origin, .. }
            | DriverEvent::ImageReceived { origin, .. } => origin.is_root(),
            DriverEvent::ToolStarted(call) => call.parent_id.is_none(),
            _ => false,
        };
        let mut turn_guard = loop {
            let mut guard = live.turn.lock().await;
            if live.stop_cleanup.load(Ordering::Relaxed) && !retiring {
                return Ok(());
            }
            let mut waiting_for_start = false;
            if starts_response && guard.is_none() && !live.is_released() {
                let mut continuation = live.continuation.lock().await;
                // Serialize the decision to resume with send_with_id's decision
                // to accept user input. No await while holding the command lock.
                let _command = self.inner.commands.lock().map_err(|_| anyhow!("command lock poisoned"))?;
                if let Some(mut thread) = self.inner.store.thread_get(thread_id)? {
                    waiting_for_start = thread.status == ThreadStatus::Running;
                    if thread.provider.kind == ProviderKind::ClaudeCode
                        && thread.status == ThreadStatus::Idle
                        && let Some(mut turn) = continuation.take()
                    {
                        turn.completed = false;
                        turn.response_id = Uuid::now_v7();
                        turn.messages.clear();
                        turn.active_messages.clear();
                        turn.terminal_message_id = None;
                        self.emit(thread_id, Some(turn.id), EventPayload::TurnResumed)?;
                        thread.status = ThreadStatus::Running;
                        self.update_thread(thread)?;
                        *guard = Some(turn);
                    }
                }
            }
            // A user request is persisted before async startup installs its
            // ActiveTurn. Buffer provider output during that gap, rather than
            // reopening the old turn or emitting unscoped transcript events.
            if waiting_for_start {
                drop(guard);
                live.turn_ready.notified().await;
                continue;
            }
            break guard;
        };
        // A handoff turn keeps its text and drops everything a transcript would show.
        let internal = turn_guard.as_ref().is_some_and(|turn| turn.internal.is_some());
        let ev = if internal {
            match rotation::internal_turn_event(&mut turn_guard, ev) {
                rotation::InternalEvent::Consumed => return Ok(()),
                rotation::InternalEvent::Interrupt => {
                    // The waiting rotation interrupts the provider and goes on without a handoff.
                    tracing::warn!(%thread_id, "the Coordinator started work while writing its handoff");
                    return Ok(());
                }
                rotation::InternalEvent::Pass(DriverEvent::PermissionRequest { request_id, .. }) => {
                    drop(turn_guard);
                    let deny = ApprovalDecision::Deny { reason: Some("Kybern is collecting a handoff; tools are not available.".into()) };
                    let _ = live.session.respond_permission(&request_id, &deny).await;
                    return Ok(());
                }
                rotation::InternalEvent::Pass(ev) => ev,
            }
        } else {
            ev
        };
        let turn_id = if internal { None } else { turn_guard.as_ref().map(|t| t.id) };
        let response_event = matches!(
            &ev,
            DriverEvent::ImageReceived { .. }
                | DriverEvent::TextDelta { .. }
                | DriverEvent::ThinkingDelta { .. }
                | DriverEvent::MessageCompleted { .. }
                | DriverEvent::ToolStarted(_)
                | DriverEvent::PermissionRequest { .. }
                | DriverEvent::TurnCompleted { .. }
                | DriverEvent::TurnFailed { .. }
        );
        if response_event
            && let Some(turn) = turn_guard.as_mut()
            && !turn.first_event_observed
        {
            turn.first_event_observed = true;
            tracing::info!(
                target: "kybern::turn_startup",
                thread_id = %thread_id,
                turn_id = %turn.id,
                provider = %turn.provider,
                session_reused = turn.session_reused,
                phase = "first_provider_event",
                elapsed_ms = turn.startup_started.elapsed().as_millis() as u64,
            );
        }
        match ev {
            DriverEvent::SubagentMessageDelivered { .. } => unreachable!("handled before root turn bookkeeping"),
            // Consumed by `subagent_take_event` before this point.
            DriverEvent::SubagentPrompt { .. } | DriverEvent::SubagentOnly { .. } => {}
            DriverEvent::ResponseStarted => {}
            DriverEvent::ResponseBoundary => {
                if let Some(turn) = turn_guard.as_mut() {
                    turn.active_messages.remove(&EventOrigin::Root);
                }
            }
            DriverEvent::SessionBound { session_id, model } => {
                let mut t = self.inner.store.thread_get(thread_id)?.ok_or_else(|| anyhow!("thread vanished"))?;
                let changed = t.provider_session_id.as_deref() != Some(&session_id) || (model.is_some() && t.model != model);
                t.provider_session_id = Some(session_id.clone());
                if let Some(m) = model.clone() {
                    let admitted = self.admitted_selection(&t)?;
                    t.model = Some(m);
                    // The selection (`opus`) still describes the row's canonical model.
                    if let Some(selected) = admitted {
                        self.record_admitted(&t, &selected)?;
                    }
                }
                if let Some(mut selected) = self.stored_target(&t)?
                    && selected.provider == t.provider
                    && selected.model.is_none()
                    && model.is_some()
                {
                    selected.model = t.model.clone();
                    self.inner.store.meta_set(&format!("target:{thread_id}"), &serde_json::to_string(&selected)?)?;
                }
                // Native canonical model reports belong to this live binding and must
                // not look like a user-requested reconfiguration on the next send.
                self.inner.store.meta_set(&format!("live_identity:{thread_id}"), &self.session_identity(&t)?)?;
                if changed {
                    self.update_thread(t)?;
                }
                self.emit(thread_id, turn_id, EventPayload::ProviderSessionBound { session_id, model })?;
            }
            DriverEvent::ImageReceived { id, origin, source } => {
                if source.len() <= 70_000_000 {
                    self.emit(thread_id, turn_id, EventPayload::ImageReceived { id, origin, source })?;
                }
            }
            DriverEvent::TextDelta { message_id, origin, delta } => {
                let id = map_message_delta(&mut turn_guard, &origin, &message_id);
                self.emit(thread_id, turn_id, EventPayload::AssistantTextDelta { message_id: id, origin, delta })?;
            }
            DriverEvent::ThinkingDelta { message_id, origin, delta } => {
                let id = map_message_delta(&mut turn_guard, &origin, &message_id);
                self.emit(thread_id, turn_id, EventPayload::AssistantThinkingDelta { message_id: id, origin, delta })?;
            }
            DriverEvent::ThinkingCompleted { message_id, origin } => {
                // Close the reasoning of the message it streamed into. It never
                // opens a message: a late signal after completion has nothing to close.
                let id = turn_guard.as_ref().and_then(|turn| {
                    let active = turn.active_messages.get(&origin)?;
                    (turn.messages.get(&(origin.clone(), message_id)) == Some(active)).then_some(*active)
                });
                if let Some(id) = id {
                    self.emit(thread_id, turn_id, EventPayload::AssistantThinkingCompleted { message_id: id, origin })?;
                }
            }
            DriverEvent::AsyncQuestions(request) => {
                let events = self.inner.store.events_for_thread(thread_id)?;
                if !events.iter().any(|event| matches!(&event.payload, EventPayload::AsyncQuestionsRequested { request: previous } if previous.id == request.id)) {
                    self.emit(thread_id, turn_id, EventPayload::AsyncQuestionsRequested { request })?;
                }
            }
            DriverEvent::MessageCompleted { message_id, origin, text, thinking } => {
                let id = map_message_completion(&mut turn_guard, &origin, &message_id);
                if origin.is_root()
                    && !text.trim().is_empty()
                    && let Some(turn) = turn_guard.as_mut()
                {
                    turn.terminal_message_id = Some(id);
                }
                self.emit(thread_id, turn_id, EventPayload::AssistantMessageCompleted { message_id: id, origin, text, thinking })?;
            }
            DriverEvent::ToolStarted(call) => {
                let owner = if let Some(parent_id) = call.parent_id.as_deref() {
                    live.tasks
                        .lock()
                        .await
                        .values()
                        .find(|task| task.id == parent_id || task.tool_call_id.as_deref() == Some(parent_id))
                        .cloned()
                } else {
                    None
                };
                let origin = owner.as_ref().map_or(EventOrigin::Root, |task| EventOrigin::Agent {
                    task_id: task.id.clone(),
                    provider_thread_id: task.provider_thread_id.clone(),
                });
                self.emit(thread_id, turn_id, EventPayload::ToolCallStarted { call: call.clone(), origin })?;
                // The same call also belongs to the subagent that made it, and the
                // prompt of a launching call becomes that subagent's first message.
                if let Some(parent_id) = call.parent_id.as_deref() {
                    self.subagent_mirror_tool_started(thread_id, parent_id, &call)?;
                }
                if let Some(prompt) = json_text(&call.input, &["prompt"])
                    && generic_runtime_task(&call).is_some_and(|task| task.kind == RuntimeTaskKind::Agent)
                {
                    self.subagent_remember_prompt(thread_id, &call.id, &prompt);
                }
                if let Some(task) = owner {
                    let _ = self
                        .apply_runtime_task_update(
                            thread_id,
                            live,
                            DriverRuntimeTaskUpdate {
                                id: task.id,
                                status: Some(RuntimeTaskStatus::Running),
                                detail: None,
                                backgrounded: None,
                                last_tool_name: Some(call.name.clone()),
                                usage: None,
                                stats: None,
                                capabilities: None,
                            },
                            RuntimeTaskUpdateKind::Progress,
                        )
                        .await?;
                }
                let provider = self.inner.store.thread_get(thread_id)?.ok_or_else(|| anyhow!("thread vanished"))?.provider.kind;
                self.guard_tool_started(thread_id, provider, &call);
                if let Some(task) = generic_runtime_task_for_provider(provider, &call) {
                    self.persist_runtime_task_start(thread_id, live, turn_id, task).await?;
                }
            }
            DriverEvent::ToolOutputDelta { tool_call_id, delta } => {
                // Preview screenshots belong only to the transient tool transport.
                // A provider may echo its MCP content as a JSON output stream.
                if self.inner.store.tool_call_name(thread_id, &tool_call_id)?.as_deref().is_some_and(crate::visuals::is_preview_tool) {
                    return Ok(());
                }
                if self.subagent_holds_call(thread_id, &tool_call_id) {
                    self.subagent_mirror_tool_event(
                        thread_id,
                        &tool_call_id,
                        DriverEvent::ToolOutputDelta { tool_call_id: tool_call_id.clone(), delta: delta.clone() },
                    )?;
                }
                self.emit(thread_id, turn_id, EventPayload::ToolCallOutputDelta { tool_call_id, delta })?;
            }
            DriverEvent::ToolCompleted { tool_call_id, output, is_error } => {
                if self.subagent_holds_call(thread_id, &tool_call_id) {
                    self.subagent_mirror_tool_event(
                        thread_id,
                        &tool_call_id,
                        DriverEvent::ToolCompleted { tool_call_id: tool_call_id.clone(), output: output.clone(), is_error },
                    )?;
                }
                self.emit(
                    thread_id,
                    turn_id,
                    EventPayload::ToolCallCompleted {
                        tool_call_id: tool_call_id.clone(),
                        output: crate::visuals::persisted_output(
                            self.inner.store.tool_call_name(thread_id, &tool_call_id)?.as_deref(),
                            output.clone(),
                        ),
                        output_omitted: false,
                        stream_recoverable: false,
                        is_error,
                    },
                )?;
                let provider = self.inner.store.thread_get(thread_id)?.ok_or_else(|| anyhow!("thread vanished"))?.provider.kind;
                if provider == ProviderKind::Cursor {
                    self.guard_tool_completed(thread_id, &tool_call_id, &output);
                }
                if matches!(provider, ProviderKind::Opencode | ProviderKind::Pi | ProviderKind::Cursor) {
                    let task_id = live
                        .tasks
                        .lock()
                        .await
                        .values()
                        .find(|task| task.tool_call_id.as_deref() == Some(&tool_call_id) && task.provider_thread_id.is_none())
                        .map(|task| task.id.clone());
                    if let Some(task_id) = task_id {
                        let detached = output_indicates_running(&output);
                        let status = if is_error { RuntimeTaskStatus::Failed } else { RuntimeTaskStatus::Completed };
                        let detail = detached.then(|| "Detached; this provider does not expose ongoing lifecycle updates".to_string());
                        let _ = self
                            .apply_runtime_task_update(
                                thread_id,
                                live,
                                DriverRuntimeTaskUpdate {
                                    id: task_id,
                                    status: Some(status),
                                    detail,
                                    backgrounded: detached.then_some(true),
                                    last_tool_name: None,
                                    usage: None,
                                    stats: None,
                                    capabilities: None,
                                },
                                RuntimeTaskUpdateKind::Complete,
                            )
                            .await?;
                    }
                }
            }
            DriverEvent::RuntimeTaskStarted(task) => {
                self.persist_runtime_task_start(thread_id, live, turn_id, task).await?;
            }
            DriverEvent::RuntimeTaskResumed(update) => {
                let _ = self.apply_runtime_task_update(thread_id, live, update, RuntimeTaskUpdateKind::Resume).await?;
            }
            DriverEvent::RuntimeTaskUpdated(update) => {
                let _ = self.apply_runtime_task_update(thread_id, live, update, RuntimeTaskUpdateKind::Progress).await?;
            }
            DriverEvent::RuntimeTaskCompleted(update) => {
                let _ = self.apply_runtime_task_update(thread_id, live, update, RuntimeTaskUpdateKind::Complete).await?;
            }
            DriverEvent::PermissionRequest { request_id, tool_call_id, tool_name, input, summary, suggestions } => {
                // Full access never asks for a permission. Harnesses can still
                // request one under their bypass mode (Claude Code's built-in
                // safety checks, for one), so allow it here and record that.
                // Questions and forms keep asking.
                let thread = self.inner.store.thread_get(thread_id)?.ok_or_else(|| anyhow!("thread vanished"))?;
                if !asks_user(&tool_name) && self.allows_without_asking(&thread)? {
                    let anchor = match turn_id {
                        Some(turn_id) => Some(turn_id),
                        None => *live.last_turn_id.lock().await,
                    };
                    drop(turn_guard);
                    if let Err(error) = live.session.respond_permission(&request_id, &ApprovalDecision::AllowOnce).await {
                        tracing::warn!(%thread_id, %error, "could not allow a Full access permission request");
                        return Ok(());
                    }
                    if let Some(turn_id) = anchor {
                        let approval = ApprovalRequest {
                            id: Uuid::now_v7(),
                            thread_id,
                            turn_id,
                            tool_call_id,
                            tool_name,
                            input,
                            summary,
                            suggestions,
                            created_at: Utc::now(),
                            questions: Vec::new(),
                        };
                        self.emit(thread_id, Some(turn_id), EventPayload::ApprovalAutoAllowed { approval })?;
                    }
                    return Ok(());
                }
                let Some(turn_id) = turn_id else {
                    tracing::warn!("permission request outside a turn; denying");
                    drop(turn_guard);
                    let _ = live
                        .session
                        .respond_permission(&request_id, &ApprovalDecision::Deny { reason: Some("no active turn".into()) })
                        .await;
                    return Ok(());
                };
                if let Some(turn) = turn_guard.as_mut() {
                    // A blocking pause is a real assistant-message boundary.
                    // Resumed prose gets a new logical identity even when the
                    // provider reuses its raw item id.
                    turn.active_messages.remove(&EventOrigin::Root);
                }
                let approval = ApprovalRequest {
                    id: Uuid::now_v7(),
                    thread_id,
                    turn_id,
                    tool_call_id,
                    tool_name,
                    input,
                    summary,
                    suggestions,
                    created_at: Utc::now(),
                    questions: Vec::new(),
                }
                .with_questions();
                self.inner.store.approval_insert(&approval)?;
                live.pending.lock().await.insert(approval.id, request_id);
                let event = if approval.is_user_input() {
                    EventPayload::UserInputRequested { approval }
                } else {
                    EventPayload::ApprovalRequested { approval }
                };
                self.emit(thread_id, Some(turn_id), event)?;
                let mut t = self.inner.store.thread_get(thread_id)?.ok_or_else(|| anyhow!("thread vanished"))?;
                if t.status != ThreadStatus::AwaitingApproval {
                    t.status = ThreadStatus::AwaitingApproval;
                    self.update_thread(t)?;
                }
            }
            DriverEvent::PermissionWithdrawn { request_id } => {
                let mut pending = live.pending.lock().await;
                if let Some((approval_id, _)) = pending.iter().find(|(_, r)| **r == request_id).map(|(a, r)| (*a, r.clone())) {
                    pending.remove(&approval_id);
                    let decision = ApprovalDecision::Deny { reason: Some("withdrawn by provider".into()) };
                    self.inner.store.approval_resolve(approval_id, &decision)?;
                    self.emit(thread_id, turn_id, EventPayload::ApprovalResolved { approval_id, decision, answered_by: None })?;
                    if pending.is_empty()
                        && let Some(mut thread) = self.inner.store.thread_get(thread_id)?
                        && thread.status == ThreadStatus::AwaitingApproval
                    {
                        thread.status = ThreadStatus::Running;
                        self.update_thread(thread)?;
                    }
                }
            }
            DriverEvent::AppToolRequest { .. } => {
                unreachable!("app tool requests are dispatched outside the sequential event pump")
            }
            DriverEvent::TurnCompleted { stop_reason, usage, cost_usd, duration_ms, anchors } => {
                let Some(turn) = turn_guard.as_mut() else { return Ok(()) };
                let has_pending_tasks = if turn.provider == ProviderKind::ClaudeCode && stop_reason == StopReason::Completed {
                    live.tasks.lock().await.values().any(|task| {
                        task.origin_turn_id == turn.id
                            && task.kind != RuntimeTaskKind::Monitor
                            && !(task.kind == RuntimeTaskKind::Process && task.backgrounded)
                            && task.status.is_active()
                    })
                } else {
                    false
                };
                if has_pending_tasks {
                    // The foreground result closes the current assistant
                    // ordinal even though it does not yet settle the parent
                    // turn. Claude's task-triggered continuation is a new
                    // logical message and becomes the eventual terminal row.
                    turn.active_messages.remove(&EventOrigin::Root);
                    if let Some(pending) = turn.pending_completion.as_mut() {
                        pending.merge(stop_reason, usage, cost_usd, duration_ms, anchors);
                    } else {
                        turn.pending_completion = Some(PendingTurnCompletion { stop_reason, usage, cost_usd, duration_ms, anchors });
                    }
                    tracing::debug!(
                        thread_id = %thread_id,
                        turn_id = %turn.id,
                        "holding Claude's provisional result while background tasks finish"
                    );
                    return Ok(());
                }

                let had_pending_completion = turn.pending_completion.is_some();
                let mut completion = turn.pending_completion.take().unwrap_or(PendingTurnCompletion {
                    stop_reason,
                    usage: Usage::default(),
                    cost_usd: None,
                    duration_ms: 0,
                    anchors: TurnAnchors::default(),
                });
                completion.merge(stop_reason, usage, cost_usd, duration_ms, anchors);
                if !had_pending_completion {
                    // `completion` began empty above; merge supplied this event.
                    completion.duration_ms =
                        if completion.duration_ms == 0 { turn.started.elapsed().as_millis() as u64 } else { completion.duration_ms };
                } else {
                    // A resumed Claude turn spans the foreground result, the
                    // task wait, and the continuation. Present the wall time.
                    completion.duration_ms = completion.duration_ms.max(turn.started.elapsed().as_millis() as u64);
                }
                // Retain cumulative accounting when this settled turn is
                // reopened by a process/monitor notification later.
                turn.pending_completion = Some(completion.clone());
                let PendingTurnCompletion { stop_reason, usage, cost_usd, duration_ms, anchors } = completion;
                if (anchors.turn_id.is_some() || anchors.previous_end.is_some())
                    && let Ok(Some(mut c)) = self.inner.store.checkpoint_get(turn.id)
                {
                    c.provider_turn_id = anchors.turn_id.clone();
                    c.provider_turn_end = anchors.previous_end.clone();
                    let _ = self.inner.store.checkpoint_upsert(&c);
                }
                turn.completed = true;
                let terminal_message_id = turn.terminal_message_id;
                let turn_id = turn.id;
                // The fresh session has read its seed: the next turn goes without it.
                if let Some(seed) = turn.rotation_seed.take()
                    && let Err(error) = self.rotation_seed_sent(thread_id, &seed)
                {
                    tracing::warn!(%thread_id, %error, "could not clear the context refresh seed");
                }
                turn.messages.clear();
                turn.active_messages.clear();
                let settled = turn_guard.take();
                if settled.as_ref().is_some_and(|turn| turn.provider == ProviderKind::ClaudeCode) && stop_reason == StopReason::Completed {
                    *live.continuation.lock().await = settled;
                }
                drop(turn_guard);
                self.resolve_finished_requests(thread_id, turn_id, live).await?;
                let mut t = self.inner.store.thread_get(thread_id)?.ok_or_else(|| anyhow!("thread vanished"))?;
                self.inner.store.usage_insert(&TurnUsageRow {
                    turn_id,
                    thread_id,
                    provider: t.provider.kind,
                    model: t.model.clone(),
                    usage: usage.clone(),
                    cost_usd,
                    duration_ms,
                    at: Utc::now(),
                })?;
                let has_active_tasks =
                    live.tasks.lock().await.values().any(|task| task.origin_turn_id == turn_id && task.status.is_active());
                if has_active_tasks {
                    live.deferred_checkpoints.lock().await.insert(turn_id);
                } else {
                    self.checkpoint_after_turn(&t, turn_id).await;
                }
                self.emit(
                    thread_id,
                    Some(turn_id),
                    EventPayload::TurnCompleted { stop_reason, usage, cost_usd, duration_ms, terminal_message_id },
                )?;
                self.inner.usage.turn_finished(t.provider.kind);
                t.status = ThreadStatus::Idle;
                let t = self.update_thread(t)?;
                self.save_account_binding(&t)?;
                if live.instructions_unconfirmed.swap(false, Ordering::SeqCst)
                    && let Some(session_id) = &t.provider_session_id
                {
                    self.inner.store.meta_set(&instructions_key(session_id), "1")?;
                }
                let this = self.clone();
                tokio::spawn(async move {
                    if let Err(error) = this.apply_pending_permission(thread_id, false).await {
                        tracing::warn!(%error, "pending permission change was not applied");
                    }
                });
                self.maybe_generate_title(&t);
                self.delegation_turn_finished(thread_id, turn_id, delegation::TurnOutcome::Completed { stop_reason, terminal_message_id });
                if t.coordinator_project_id.is_some() {
                    self.rotate_if_over_budget(thread_id, live.clone());
                }
            }
            DriverEvent::TurnFailed { error } => {
                let Some(turn) = turn_guard.as_mut() else { return Ok(()) };
                // A login another machine lent was rejected (the broker rotated it): borrow again, resume and
                // send the message once more instead of failing the turn.
                if turn.internal.is_none() && !retiring && self.recover_rejected_login(thread_id, live, turn.id, &error).await {
                    *turn_guard = None;
                    return Ok(());
                }
                turn.completed = true;
                let turn_id = turn.id;
                *turn_guard = None;
                drop(turn_guard);
                self.resolve_finished_requests(thread_id, turn_id, live).await?;
                let mut t = self.inner.store.thread_get(thread_id)?.ok_or_else(|| anyhow!("thread vanished"))?;
                let has_active_tasks =
                    live.tasks.lock().await.values().any(|task| task.origin_turn_id == turn_id && task.status.is_active());
                if has_active_tasks {
                    live.deferred_checkpoints.lock().await.insert(turn_id);
                } else {
                    self.checkpoint_after_turn(&t, turn_id).await;
                }
                if self.shutting_down() {
                    tracing::info!(%thread_id, %error, "the daemon is stopping; the turn ends as interrupted");
                    self.emit(
                        thread_id,
                        Some(turn_id),
                        EventPayload::TurnCompleted {
                            stop_reason: StopReason::Interrupted,
                            usage: Usage::default(),
                            cost_usd: None,
                            duration_ms: 0,
                            terminal_message_id: None,
                        },
                    )?;
                    self.emit(
                        thread_id,
                        Some(turn_id),
                        EventPayload::ProviderNotice {
                            level: NoticeLevel::Warning,
                            text: format!(
                                "Kybern's daemon on {} stopped, so this turn was interrupted. Send a message to continue.",
                                crate::rpc::hostname()
                            ),
                            data: None,
                        },
                    )?;
                    t.status = ThreadStatus::Idle;
                    self.update_thread(t)?;
                    self.delegation_turn_finished(
                        thread_id,
                        turn_id,
                        delegation::TurnOutcome::Completed { stop_reason: StopReason::Interrupted, terminal_message_id: None },
                    );
                    return Ok(());
                }
                self.emit(thread_id, Some(turn_id), EventPayload::TurnFailed { error: error.clone() })?;
                // A failed turn may have hit a plan limit.
                self.inner.usage.turn_finished(t.provider.kind);
                t.status = ThreadStatus::Failed;
                self.update_thread(t)?;
                self.delegation_turn_finished(thread_id, turn_id, delegation::TurnOutcome::Failed(error));
            }
            DriverEvent::CommandsUpdated(commands) => {
                self.emit(thread_id, turn_id, EventPayload::ProviderCommandsUpdated { commands })?;
            }
            DriverEvent::UsageUpdated(usage) => {
                if let Some(context) = &usage.context {
                    *live.context_usage.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(context.clone());
                }
                if let Some(context) = &usage.context
                    && let Some(thread) = self.inner.store.thread_get(thread_id)?
                {
                    self.inner.store.meta_set(
                        &format!(
                            "model_context:{}:{}:{}",
                            thread.provider.kind,
                            thread.provider.instance,
                            thread.model.as_deref().unwrap_or("default")
                        ),
                        &context.window_tokens.to_string(),
                    )?;
                }
                // Plan limits are account-wide: every client's limits view
                // updates, not only this thread's.
                if let Some(limits) = usage.limits.as_deref()
                    && let Some(thread) = self.inner.store.thread_get(thread_id)?
                {
                    let admitted = self.inner.store.meta_get(&format!("live_fingerprint:{thread_id}"))?;
                    if admitted
                        .as_deref()
                        .is_none_or(|identity| self.environment_fingerprint(&thread).is_ok_and(|current| current == identity))
                    {
                        self.inner.usage.observe_account(&thread.provider, limits);
                    }
                    self.inner.store.meta_set(
                        &format!("account_usage:{}:{}", thread.provider.kind, thread.provider.instance),
                        &serde_json::to_string(&usage)?,
                    )?;
                }
                self.emit(thread_id, turn_id, EventPayload::ProviderUsageUpdated { usage })?;
            }
            DriverEvent::Notice { level, text, data } => {
                self.emit(thread_id, turn_id, EventPayload::ProviderNotice { level, text, data })?;
            }
            DriverEvent::Exited { code, error } => {
                if let Some(error) = error.filter(|_| !live.is_released()) {
                    self.emit(
                        thread_id,
                        turn_id,
                        EventPayload::ProviderNotice {
                            level: NoticeLevel::Error,
                            text: format!("provider exited{}: {error}", code.map(|c| format!(" with code {c}")).unwrap_or_default()),
                            data: None,
                        },
                    )?;
                }
            }
        }
        Ok(())
    }
}

fn reject_app_tool_request(live: Arc<LiveSession>, request_id: String, reason: &'static str) {
    tokio::spawn(async move {
        let _ = tokio::time::timeout(Duration::from_secs(2), live.session.respond_app_tool(&request_id, Err(reason.to_string()))).await;
    });
}

fn bounded_app_tool_error(mut error: String) -> String {
    const MAX_ERROR_BYTES: usize = 4096;
    if error.len() > MAX_ERROR_BYTES {
        let mut end = MAX_ERROR_BYTES;
        while !error.is_char_boundary(end) {
            end -= 1;
        }
        error.truncate(end);
    }
    error
}

fn map_message_delta(turn: &mut Option<ActiveTurn>, origin: &EventOrigin, provider_id: &str) -> MessageId {
    let Some(turn) = turn.as_mut() else { return Uuid::now_v7() };
    if let Some(active) = turn.active_messages.get(origin).copied() {
        turn.messages.insert((origin.clone(), provider_id.to_string()), active);
        return active;
    }

    // A new delta after completion opens a fresh logical message, even if the
    // provider reused a raw id from an earlier message.
    let id = Uuid::now_v7();
    turn.messages.insert((origin.clone(), provider_id.to_string()), id);
    turn.active_messages.insert(origin.clone(), id);
    id
}

fn map_message_completion(turn: &mut Option<ActiveTurn>, origin: &EventOrigin, provider_id: &str) -> MessageId {
    let Some(turn) = turn.as_mut() else { return Uuid::now_v7() };
    let key = (origin.clone(), provider_id.to_string());
    let id = turn.active_messages.remove(origin).or_else(|| turn.messages.get(&key).copied()).unwrap_or_else(Uuid::now_v7);
    turn.messages.insert(key, id);
    id
}

fn merge_task_stats(current: &mut RuntimeTaskStats, update: RuntimeTaskStats) {
    if update.token_count.is_some() {
        current.token_count = update.token_count;
    }
    if update.tool_uses.is_some() {
        current.tool_uses = update.tool_uses;
    }
    if update.duration_ms.is_some() {
        current.duration_ms = update.duration_ms;
    }
    if update.cpu_percent.is_some() {
        current.cpu_percent = update.cpu_percent;
    }
    if update.rss_kb.is_some() {
        current.rss_kb = update.rss_kb;
    }
}

// Cursor SDK cannot implement a human approval mode. New threads without an
// explicit mode use its sandboxed policy; explicit unsupported choices still
// fail at the driver boundary, never silently gain permissions.
fn default_provider_permission(provider: ProviderKind, mode: PermissionMode) -> PermissionMode {
    if provider == ProviderKind::Cursor && !matches!(mode, PermissionMode::Auto | PermissionMode::FullAccess) {
        PermissionMode::Auto
    } else {
        mode
    }
}

fn generic_runtime_task(call: &ToolCall) -> Option<DriverRuntimeTask> {
    let normalized = call.name.chars().filter(|char| char.is_ascii_alphanumeric()).flat_map(char::to_lowercase).collect::<String>();
    let title_hint = json_text(&call.input, &["title"]).unwrap_or_default();
    let normalized_title = title_hint.chars().filter(|char| char.is_ascii_alphanumeric()).flat_map(char::to_lowercase).collect::<String>();
    let explicit_background = json_bool(&call.input, &["background", "run_in_background", "runInBackground", "detach", "detached"])
        || json_false(&call.input, &["blocking"]);
    let agent_tool = matches!(
        normalized.as_str(),
        "task" | "agent" | "spawnagent" | "subagent" | "delegate" | "delegation" | "launchagent" | "callagent" | "runagent"
    ) || normalized.contains("subagent")
        || normalized.ends_with("spawnagent");
    let ambiguous_agent_tool = matches!(normalized.as_str(), "other" | "tool")
        && ["task", "agent", "subagent", "delegate", "spawnagent"].iter().any(|prefix| normalized_title.starts_with(prefix));
    let kind = if agent_tool || ambiguous_agent_tool {
        RuntimeTaskKind::Agent
    } else if matches!(normalized.as_str(), "monitor" | "monitortask" | "monitormcp") {
        RuntimeTaskKind::Monitor
    } else if explicit_background
        && matches!(normalized.as_str(), "bash" | "shell" | "execute" | "exec" | "command" | "runcommand" | "terminal")
    {
        RuntimeTaskKind::Process
    } else {
        return None;
    };
    let title = json_text(&call.input, &["description", "title", "prompt", "command", "task"])
        .map(|text| text.lines().next().unwrap_or(&text).chars().take(120).collect())
        .unwrap_or_else(|| match kind {
            RuntimeTaskKind::Agent => "Subagent".into(),
            RuntimeTaskKind::Process => "Background process".into(),
            RuntimeTaskKind::Monitor => "Monitor".into(),
        });
    Some(DriverRuntimeTask {
        id: format!("tool:{}", call.id),
        kind,
        status: RuntimeTaskStatus::Running,
        title,
        detail: None,
        provider_type: Some(call.name.clone()),
        parent_id: call.parent_id.clone(),
        tool_call_id: Some(call.id.clone()),
        provider_thread_id: None,
        model: None,
        effort: None,
        backgrounded: explicit_background,
        last_tool_name: None,
        usage: None,
        stats: RuntimeTaskStats::default(),
        capabilities: RuntimeTaskCapabilities::default(),
    })
}

fn generic_runtime_task_for_provider(provider: ProviderKind, call: &ToolCall) -> Option<DriverRuntimeTask> {
    // SDK events already carry the native task lifecycle and nested ownership.
    // Legacy Cursor ACP chats still need the conservative tool-only fallback.
    if provider == ProviderKind::Cursor && call.id.starts_with("cursor-sdk:") {
        return None;
    }
    let task = generic_runtime_task(call)?;
    match provider {
        // These harnesses expose first-class lifecycle channels. Mixing a
        // guessed tool row with the native identity duplicates batched/nested
        // children and can finish them at the wrong edge.
        ProviderKind::ClaudeCode | ProviderKind::Codex | ProviderKind::Omp => None,
        // Antigravity's subagent calls stay tool cards: no guessed task rows or child threads.
        ProviderKind::Antigravity => None,
        ProviderKind::GrokBuild => None,
        ProviderKind::Opencode if task.kind == RuntimeTaskKind::Agent => None,
        // pi and Cursor currently expose only tool-scoped observation through
        // the protocols Kybern drives. OpenCode still uses the conservative
        // path for background process/monitor tools.
        ProviderKind::Opencode | ProviderKind::Pi | ProviderKind::Cursor => Some(task),
    }
}

fn json_bool(value: &serde_json::Value, keys: &[&str]) -> bool {
    keys.iter().any(|key| value.get(*key).and_then(serde_json::Value::as_bool) == Some(true))
        || ["raw", "args", "input"].iter().any(|key| value.get(*key).is_some_and(|nested| json_bool(nested, keys)))
}

fn json_false(value: &serde_json::Value, keys: &[&str]) -> bool {
    keys.iter().any(|key| value.get(*key).and_then(serde_json::Value::as_bool) == Some(false))
        || ["raw", "args", "input"].iter().any(|key| value.get(*key).is_some_and(|nested| json_false(nested, keys)))
}

/// Whether a harness request with this tool name asks the user something:
/// a question, a form, or a plan to start implementing. Full access answers
/// permissions, never these.
fn asks_user(tool_name: &str) -> bool {
    kybern_protocol::is_user_input_tool(tool_name) || tool_name == "ExitPlanMode"
}

fn json_text(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(serde_json::Value::as_str).map(str::to_string))
        .or_else(|| ["raw", "args", "input"].iter().find_map(|key| value.get(*key).and_then(|nested| json_text(nested, keys))))
}

fn output_indicates_running(output: &serde_json::Value) -> bool {
    if let Some(object) = output.as_object() {
        if json_bool(output, &["background", "backgrounded", "running", "is_running"]) {
            return true;
        }
        if object
            .get("status")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|status| matches!(status, "running" | "in_progress" | "backgrounded" | "pending"))
        {
            return true;
        }
    }
    output
        .as_str()
        .map(str::to_ascii_lowercase)
        .is_some_and(|text| text.contains("running in background") || text.contains("background task") || text.contains("process id"))
}

fn clean_title(text: &str) -> String {
    let trim = |s: &str| {
        s.trim_matches(|c: char| c == '"' || c == '\'' || c == '*' || c == '#' || c == '`').trim_end_matches('.').trim().to_string()
    };
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let line = trim(&strip_title_preamble(&trim(line)));
    let title: String = sentence_case(&line).chars().take(80).collect();
    if title.split_whitespace().count() > 12 { String::new() } else { title }
}

/// A model that answers "Title: fix login" or "Titles the session: fix login" instead of the bare
/// title: the words before the first colon are talk about the title, not part of it.
fn strip_title_preamble(line: &str) -> String {
    let Some((head, rest)) = line.split_once(':') else { return line.to_string() };
    let words: Vec<String> = head.split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase()).collect();
    let about_the_title = words.len() <= 5
        && (words.iter().any(|w| matches!(w.as_str(), "title" | "titles" | "titled" | "heading"))
            || words.first().is_some_and(|w| matches!(w.as_str(), "here" | "suggested")));
    if about_the_title && !rest.trim().is_empty() { rest.trim().to_string() } else { line.to_string() }
}

/// The title starts with a capital. The rest stays as the model wrote it: lowercasing a Title Case
/// reply would also lowercase names (`Windows`, `Linux`), and the prompt already asks for sentence case.
fn sentence_case(title: &str) -> String {
    let mut chars = title.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// What `kybern askpass` gets back for a prompt.
pub(crate) use askpass::PromptReply;
pub(crate) use groups::{member_dir, pr_repo_dir};

/// The wait budget of the summoned-card tools (sign-in, question, permission, terminal).
pub(crate) fn request_call_budget(name: &str, arguments: &serde_json::Value) -> Option<Duration> {
    login_request::call_budget(name)
        .or_else(|| request_tools::call_budget(name, arguments))
        .or_else(|| provision::call_budget(name, arguments))
}

/// The summoned-card tools offered to agent sessions.
pub(crate) fn child_question_tool_definitions() -> Vec<kybern_drivers::NativeToolDefinition> {
    child_questions::tool_definitions()
}

pub(crate) fn request_tool_definitions() -> Vec<kybern_drivers::NativeToolDefinition> {
    let mut tools = vec![login_request::tool_definition()];
    tools.extend(request_tools::tool_definitions());
    tools
}

pub fn title_from_message(message: &UserMessage) -> String {
    let text = message.plain_text();
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if line.is_empty() {
        return DEFAULT_TITLE.to_string();
    }
    let mut title: String = line.chars().take(60).collect();
    if line.chars().count() > 60 {
        title.push('…');
    }
    title
}

fn is_compact_message(message: &UserMessage) -> bool {
    matches!(message.parts.as_slice(), [kybern_protocol::ContentPart::Text { text }] if text.trim() == "/compact")
}

/// One portable path component for a project's worktree folder: characters Windows forbids or
/// treats specially (`<>:"/\\|?*`, control characters) become '-', trailing dots and spaces go,
/// reserved device names (`CON`, `NUL`, `COM1`…) get a prefix, at most 64 characters.
pub(crate) fn worktree_label(name: &str) -> String {
    let label: String = name
        .chars()
        .map(|c| if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() { '-' } else { c })
        .take(64)
        .collect();
    let label = label.trim().trim_end_matches(['.', ' ']);
    if label.is_empty() {
        return "project".into();
    }
    let stem = label.split('.').next().unwrap_or(label).trim_end().to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
    if reserved { format!("_{label}") } else { label.to_string() }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn effort_selection_validates_against_the_model_s_own_list() {
        let fixture = Fixture::new();
        let thread = fixture.thread_with_provider(ThreadStatus::Idle, ProviderKind::Opencode);
        let model = |id: &str, efforts: Vec<&str>| ProviderModel {
            id: id.into(),
            display_name: id.into(),
            resolved_id: None,
            description: None,
            provider: None,
            efforts: efforts.into_iter().map(String::from).collect(),
            default_effort: None,
            is_default: false,
            parameters: Vec::new(),
            variants: Vec::new(),
        };
        let status = ProviderStatus {
            kind: ProviderKind::Opencode,
            display_name: "OpenCode".into(),
            available: true,
            binary_path: None,
            version: None,
            unavailable_reason: None,
            supported_permission_modes: PermissionMode::ALL.to_vec(),
            supports_fork: true,
            supports_model_switch: true,
            supports_effort_switch: true,
            supported_efforts: vec![],
            models: vec![model("p/m-with", vec!["low", "high"]), model("p/m-without", vec![])],
            instances: vec!["default".into()],
        };
        let cache_key =
            serde_json::to_string(&(Some(thread.project_id), Some(&thread.cwd), &fixture.orchestrator.inner.settings.get().providers))
                .unwrap();
        fixture.orchestrator.inner.provider_catalogs.get_or_refresh(cache_key.clone(), false, || async { vec![status.clone()] }).await;
        let instance = thread.provider.clone();
        let validate = |model: &'static str, effort: Option<&'static str>| {
            fixture.orchestrator.validate_provider_selection(thread.project_id, &instance, Some(model), effort)
        };
        assert!(validate("p/m-with", Some("low")).await.is_ok());
        assert!(validate("p/m-with", Some("xhigh")).await.is_err());
        let error = validate("p/m-without", Some("high")).await.unwrap_err().to_string();
        assert!(error.contains("has no effort levels"), "{error}");
        assert!(validate("p/m-without", None).await.is_ok());
        let legacy = status_models_with_global_list();
        fixture.orchestrator.inner.provider_catalogs.get_or_refresh(cache_key, true, || async { vec![legacy.clone()] }).await;
        assert!(validate("p/m-without", Some("high")).await.is_ok());
        assert!(validate("p/m-without", Some("low")).await.is_err());
    }

    fn status_models_with_global_list() -> ProviderStatus {
        ProviderStatus {
            kind: ProviderKind::Opencode,
            display_name: "OpenCode".into(),
            available: true,
            binary_path: None,
            version: None,
            unavailable_reason: None,
            supported_permission_modes: PermissionMode::ALL.to_vec(),
            supports_fork: true,
            supports_model_switch: true,
            supports_effort_switch: true,
            supported_efforts: vec!["high".into()],
            models: vec![ProviderModel {
                id: "p/m-without".into(),
                display_name: "p/m-without".into(),
                resolved_id: None,
                description: None,
                provider: None,
                efforts: vec![],
                default_effort: None,
                is_default: false,
                parameters: Vec::new(),
                variants: Vec::new(),
            }],
            instances: vec!["default".into()],
        }
    }

    #[test]
    fn a_generated_title_loses_talk_about_the_title_and_keeps_sentence_case() {
        use super::clean_title;
        assert_eq!(clean_title("Titles the session: check main green across OSes"), "Check main green across OSes");
        assert_eq!(clean_title("Title: **Fix login redirect**"), "Fix login redirect");
        assert_eq!(clean_title("Here's a title: fix the flaky test."), "Fix the flaky test");
        assert_eq!(clean_title("\"fix the Windows build\""), "Fix the Windows build");
        assert_eq!(clean_title("Fix iOS crash on launch"), "Fix iOS crash on launch");
        // A colon that is part of the title stays.
        assert_eq!(clean_title("Session tokens: rotate on login"), "Session tokens: rotate on login");
        assert_eq!(clean_title("Title:"), "Title:");
        assert_eq!(clean_title("one two three four five six seven eight nine ten eleven twelve thirteen"), "");
    }

    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant, SystemTime};

    use super::{
        ActiveTurn, LiveSession, Orchestrator, SessionActivityTime, generic_runtime_task, generic_runtime_task_for_provider,
        output_indicates_running,
    };
    use crate::config::Paths;
    use crate::settings::SettingsStore;
    use kybern_drivers::registry::DriverRegistry;
    use kybern_drivers::{AgentSession, DriverEvent, DriverRuntimeTask, DriverRuntimeTaskUpdate, TurnAnchors};
    use kybern_protocol::*;
    use kybern_store::Store;
    use serde_json::json;
    use tokio::sync::{Mutex, Semaphore};
    use uuid::Uuid;

    type AppToolResponse = (String, std::result::Result<serde_json::Value, String>);
    type CapturedAppToolResponses = Arc<Mutex<Vec<AppToolResponse>>>;

    #[derive(Default)]
    struct TestSession {
        closes: Arc<AtomicUsize>,
        messages: Arc<Mutex<Vec<UserMessage>>>,
        app_tool_responses: CapturedAppToolResponses,
        hang_interrupt: bool,
        broken_interrupt: bool,
        reject_permission: bool,
        stopped_tasks: Arc<Mutex<Vec<String>>>,
        /// What the session was told to do with each permission request.
        permission_responses: Arc<Mutex<Vec<ApprovalDecision>>>,
    }

    #[async_trait::async_trait]
    impl AgentSession for TestSession {
        async fn send_subagent_message(&self, _task_id: &str, _message_id: &str, _message: &UserMessage) -> kybern_drivers::Result<()> {
            Ok(())
        }

        async fn stop_runtime_task(&self, task: &RuntimeTask) -> kybern_drivers::Result<()> {
            self.stopped_tasks.lock().await.push(task.id.clone());
            Ok(())
        }

        async fn send_message(&self, _message_id: &str, message: &UserMessage) -> kybern_drivers::Result<()> {
            self.messages.lock().await.push(message.clone());
            Ok(())
        }

        async fn steer(&self, _message_id: &str, message: &UserMessage) -> kybern_drivers::Result<()> {
            self.messages.lock().await.push(message.clone());
            Ok(())
        }

        async fn interrupt(&self) -> kybern_drivers::Result<()> {
            if self.broken_interrupt {
                return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe).into());
            }
            if self.hang_interrupt {
                std::future::pending::<()>().await;
            }
            Ok(())
        }

        async fn set_permission_mode(&self, _mode: PermissionMode) -> kybern_drivers::Result<()> {
            if self.reject_permission {
                return Err(kybern_drivers::DriverError::Unsupported("fixture rejects permissions".into()));
            }
            Ok(())
        }

        async fn set_model(&self, _model: &str) -> kybern_drivers::Result<()> {
            Ok(())
        }

        async fn set_effort(&self, _effort: &str) -> kybern_drivers::Result<()> {
            Ok(())
        }

        async fn respond_permission(&self, _request_id: &str, decision: &ApprovalDecision) -> kybern_drivers::Result<()> {
            self.permission_responses.lock().await.push(decision.clone());
            Ok(())
        }

        async fn respond_app_tool(
            &self,
            request_id: &str,
            result: std::result::Result<serde_json::Value, String>,
        ) -> kybern_drivers::Result<()> {
            self.app_tool_responses.lock().await.push((request_id.to_string(), result));
            Ok(())
        }

        async fn close(&self) -> kybern_drivers::Result<()> {
            self.closes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn classifies_native_and_acp_style_subagent_tools() {
        let native = generic_runtime_task(&ToolCall {
            id: "one".into(),
            name: "spawn_agent".into(),
            input: json!({ "description": "Audit the protocol" }),
            parent_id: None,
        })
        .unwrap();
        assert_eq!(native.kind, RuntimeTaskKind::Agent);
        assert_eq!(native.title, "Audit the protocol");

        let acp = generic_runtime_task(&ToolCall {
            id: "two".into(),
            name: "other".into(),
            input: json!({ "title": "Task: inspect tests", "raw": {} }),
            parent_id: None,
        })
        .unwrap();
        assert_eq!(acp.kind, RuntimeTaskKind::Agent);
        assert_eq!(acp.title, "Task: inspect tests");
    }

    #[test]
    fn requires_an_explicit_background_signal_for_generic_shells() {
        let foreground = ToolCall { id: "one".into(), name: "shell".into(), input: json!({ "command": "cargo test" }), parent_id: None };
        assert!(generic_runtime_task(&foreground).is_none());

        let background = generic_runtime_task(&ToolCall {
            id: "two".into(),
            name: "execute".into(),
            input: json!({ "title": "Run dev server", "raw": { "blocking": false } }),
            parent_id: None,
        })
        .unwrap();
        assert_eq!(background.kind, RuntimeTaskKind::Process);
        assert!(background.backgrounded);
        assert!(!background.capabilities.stop);
    }

    #[test]
    fn cursor_legacy_model_migration_uses_only_the_cached_account_and_preserves_past_events() {
        use base64::Engine;
        let fixture = Fixture::new();
        let mut thread = fixture.thread_with_provider(ThreadStatus::Idle, ProviderKind::Cursor);
        thread.model = Some("cursor-grok-4.5-high".into());
        fixture.store.thread_upsert(&thread).unwrap();
        fixture
            .orchestrator
            .emit(thread.id, None, EventPayload::TurnFailed { error: "original old model cursor-grok-4.5-high".into() })
            .unwrap();
        let before = fixture.store.events_for_thread(thread.id).unwrap();
        // No discovery or mutation if this daemon has no cached catalog.
        assert_eq!(fixture.orchestrator.migrate_cursor_model(thread.clone()).unwrap().model, thread.model);
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({"id":"grok-4.5","params":[],"effortParam":"effort","effortValues":["low","high"]})).unwrap(),
        );
        let model: ProviderModel = serde_json::from_value(
            json!({"id":format!("cursor-model:{encoded}"),"resolved_id":"grok-4.5","display_name":"Grok 4.5","efforts":["low","high"]}),
        )
        .unwrap();
        let status = ProviderStatus {
            kind: ProviderKind::Cursor,
            display_name: "Cursor".into(),
            available: true,
            binary_path: None,
            version: None,
            unavailable_reason: None,
            supported_permission_modes: vec![PermissionMode::Auto],
            supports_fork: false,
            supports_model_switch: true,
            supports_effort_switch: true,
            supported_efforts: vec!["high".into()],
            models: vec![model],
            instances: vec!["default".into()],
        };
        fixture.orchestrator.inner.provider_catalogs.remember(&thread.project_id.to_string(), &[status]);
        let mut other_account = thread.clone();
        other_account.provider.instance = "work".into();
        assert_eq!(fixture.orchestrator.migrate_cursor_model(other_account).unwrap().model, thread.model);
        let migrated = fixture.orchestrator.migrate_cursor_model(thread).unwrap();
        assert!(migrated.model.as_deref().unwrap().starts_with("cursor-model:"));
        assert_eq!(migrated.effort.as_deref(), Some("high"));
        let after = fixture.store.events_for_thread(migrated.id).unwrap();
        assert_eq!(serde_json::to_value(&after[..before.len()]).unwrap(), serde_json::to_value(&before).unwrap());
        assert_eq!(after.len(), before.len() + 1);
        let repeated = fixture.orchestrator.migrate_cursor_model(migrated).unwrap();
        assert_eq!(fixture.store.events_for_thread(repeated.id).unwrap().len(), after.len());
    }

    #[tokio::test]
    async fn project_git_refresh_checks_all_batches_and_preserves_order() {
        let fixture = Fixture::new();
        let mut repositories = Vec::new();
        for index in 0..18 {
            let directory = fixture.root.join(format!("project-{index}"));
            std::fs::create_dir_all(&directory).unwrap();
            if index % 3 == 0 {
                assert!(tokio::process::Command::new("git").arg("init").arg("--quiet").arg(&directory).status().await.unwrap().success());
            }
            let mut project = fixture.orchestrator.add_project(directory.to_string_lossy().into_owned(), None, None).unwrap();
            if index % 3 == 0 {
                repositories.push(project.id);
            }
            // Every flag is deliberately stale, in both directions.
            project.is_git = index % 3 != 0;
            fixture.store.project_update(&project).unwrap();
        }
        let before = fixture.store.projects_list().unwrap();
        let refreshed = fixture.orchestrator.refresh_projects_git().await.unwrap();
        assert_eq!(
            before.iter().map(|project| project.id).collect::<Vec<_>>(),
            refreshed.iter().map(|project| project.id).collect::<Vec<_>>()
        );
        for project in refreshed {
            assert_eq!(project.is_git, repositories.contains(&project.id));
            assert_eq!(fixture.store.project_get(project.id).unwrap().unwrap().is_git, project.is_git);
        }
    }

    #[tokio::test]
    async fn add_project_self_heals_is_git_after_a_later_git_init() {
        let root = std::env::temp_dir().join(format!("kybern-isgit-test-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&root).unwrap();
        let paths = Paths::resolve(Some(root.join(".kyb"))).unwrap();
        let settings = SettingsStore::load(&paths.settings).unwrap();
        let store = Store::open_in_memory().unwrap();
        let (events_tx, _) = crate::bounded_broadcast::channel(32, 8 * 1024 * 1024);
        let orchestrator = Orchestrator::new(store.clone(), DriverRegistry::default(), events_tx, paths, settings);

        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let path = workspace.to_string_lossy().into_owned();

        // Registered before `git init`: the cached flag is false.
        let first = orchestrator.add_project(path.clone(), None, None).unwrap();
        assert!(!first.is_git, "fixture folder has no .git yet");

        // The user runs `git init` afterwards.
        std::fs::create_dir_all(workspace.join(".git")).unwrap();

        // Re-adding the same path re-probes, flips the flag, and persists it, so
        // edit/integration spawns are no longer permanently blocked (issue #26).
        let healed = orchestrator.add_project(path.clone(), None, None).unwrap();
        assert_eq!(healed.id, first.id, "same project record, not a duplicate");
        assert!(healed.is_git, "cached is_git self-heals to true");
        let stored = store.project_get(first.id).unwrap().unwrap();
        assert!(stored.is_git, "corrected flag is persisted to the store");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn free_chat_uses_a_hidden_neutral_workspace() {
        let root = std::env::temp_dir().join(format!("kybern-free-chat-test-{}", Uuid::now_v7()));
        let paths = Paths::resolve(Some(root.clone())).unwrap();
        let settings = SettingsStore::load(&paths.settings).unwrap();
        let store = Store::open_in_memory().unwrap();
        let (events_tx, _) = crate::bounded_broadcast::channel(32, 8 * 1024 * 1024);
        let orchestrator = Orchestrator::new(store.clone(), DriverRegistry::default(), events_tx, paths, settings);

        let thread = orchestrator
            .create_thread(methods::ThreadsCreateParams {
                pin_account: false,
                project_id: None,
                provider: ProviderInstance::default_for(ProviderKind::Codex),
                model: None,
                effort: None,
                permission_mode: None,
                use_worktree: None,
                base_branch: None,
                title: Some("Free chat".into()),
                message: None,
                thread_id: None,
                message_id: None,
                members: None,
                base_branches: None,
            })
            .await
            .unwrap();

        assert_eq!(thread.project_id, FREE_CHAT_PROJECT_ID);
        assert_eq!(PathBuf::from(&thread.cwd), root.join("free-chat"));
        assert!(PathBuf::from(&thread.cwd).is_dir());
        assert!(store.projects_list().unwrap().is_empty(), "the neutral workspace is not a user project");
        assert!(store.project_get(FREE_CHAT_PROJECT_ID).unwrap().is_some());
        assert!(store.project_delete(FREE_CHAT_PROJECT_ID).is_err());

        let rejected = orchestrator
            .create_thread(methods::ThreadsCreateParams {
                pin_account: false,
                project_id: None,
                provider: ProviderInstance::default_for(ProviderKind::Codex),
                model: None,
                effort: None,
                permission_mode: None,
                use_worktree: Some(true),
                base_branch: None,
                title: None,
                message: None,
                thread_id: None,
                message_id: None,
                members: None,
                base_branches: None,
            })
            .await;
        assert!(rejected.is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    fn turns_started(store: &Store, thread_id: ThreadId) -> usize {
        store.events_for_thread(thread_id).unwrap().iter().filter(|event| matches!(event.payload, EventPayload::TurnStarted { .. })).count()
    }

    fn client_create(fixture: &Fixture, thread_id: ThreadId, message_id: Option<MessageId>, text: &str) -> methods::ThreadsCreateParams {
        methods::ThreadsCreateParams {
            pin_account: false,
            project_id: Some(fixture.project.id),
            provider: ProviderInstance::default_for(ProviderKind::Codex),
            model: None,
            effort: None,
            permission_mode: None,
            use_worktree: Some(false),
            base_branch: None,
            title: None,
            message: Some(UserMessage::text(text)),
            thread_id: Some(thread_id),
            message_id,
            members: None,
            base_branches: None,
        }
    }

    #[tokio::test]
    async fn create_with_a_client_thread_id_is_idempotent() {
        let fixture = Fixture::new();
        let (thread_id, message_id) = (Uuid::now_v7(), Uuid::now_v7());
        let first = fixture.orchestrator.create_thread(client_create(&fixture, thread_id, Some(message_id), "hello")).await.unwrap();
        assert_eq!(first.id, thread_id, "the daemon uses the client's id");
        let repeat = fixture.orchestrator.create_thread(client_create(&fixture, thread_id, Some(message_id), "hello")).await.unwrap();
        assert_eq!(repeat.id, thread_id);
        assert_eq!(fixture.store.threads_list(None, true).unwrap().len(), 1, "never a second row");
        assert_eq!(turns_started(&fixture.store, thread_id), 1, "the accepted message is not sent again");

        // Without a message id, a repeat sends only to a thread that never started a turn.
        fixture.orchestrator.create_thread(client_create(&fixture, thread_id, None, "hello")).await.unwrap();
        assert_eq!(turns_started(&fixture.store, thread_id), 1);

        let other_message = fixture.orchestrator.create_thread(client_create(&fixture, thread_id, Some(message_id), "changed")).await;
        assert!(other_message.unwrap_err().to_string().contains("already belongs"));
        let mut elsewhere = client_create(&fixture, thread_id, Some(Uuid::now_v7()), "hello");
        elsewhere.project_id = None;
        assert!(fixture.orchestrator.create_thread(elsewhere).await.unwrap_err().to_string().contains("different thread"));
        let nil = fixture.orchestrator.create_thread(client_create(&fixture, Uuid::nil(), None, "hello")).await;
        assert!(nil.is_err());
    }

    #[tokio::test]
    async fn retrying_a_create_whose_message_failed_reuses_the_thread() {
        let fixture = Fixture::new();
        let (thread_id, message_id) = (Uuid::now_v7(), Uuid::now_v7());
        // The thread is created, then its first message is refused (the harness is updating).
        let gate = fixture.orchestrator.inner.harness_gates[&ProviderKind::Codex].clone().write_owned().await;
        let failed = fixture.orchestrator.create_thread(client_create(&fixture, thread_id, Some(message_id), "hello")).await;
        assert!(failed.unwrap_err().to_string().contains("updating"));
        drop(gate);
        assert!(fixture.store.thread_get(thread_id).unwrap().is_some());
        assert_eq!(turns_started(&fixture.store, thread_id), 0);

        let retried = fixture.orchestrator.create_thread(client_create(&fixture, thread_id, Some(message_id), "hello")).await.unwrap();
        assert_eq!(retried.id, thread_id);
        assert_eq!(fixture.store.threads_list(None, true).unwrap().len(), 1, "the retry fills the same row");
        assert_eq!(turns_started(&fixture.store, thread_id), 1);
    }

    #[test]
    fn recognizes_detached_results_without_claiming_native_lifecycle() {
        assert!(output_indicates_running(&json!({ "status": "in_progress" })));
        assert!(output_indicates_running(&json!("Process ID 42 is running in background")));
        assert!(!output_indicates_running(&json!({ "status": "completed" })));
    }

    #[test]
    fn runtime_fallback_matrix_preserves_native_harness_channels() {
        let agent =
            ToolCall { id: "agent-1".into(), name: "task".into(), input: json!({ "description": "Inspect parity" }), parent_id: None };

        assert!(generic_runtime_task_for_provider(ProviderKind::ClaudeCode, &agent).is_none());
        assert!(generic_runtime_task_for_provider(ProviderKind::Codex, &agent).is_none());
        assert!(generic_runtime_task_for_provider(ProviderKind::Opencode, &agent).is_none());
        assert!(generic_runtime_task_for_provider(ProviderKind::Omp, &agent).is_none());
        assert!(generic_runtime_task_for_provider(ProviderKind::Pi, &agent).is_some());
        assert!(generic_runtime_task_for_provider(ProviderKind::Cursor, &agent).is_some());
        let sdk_agent = ToolCall { id: "cursor-sdk:r1:tool:agent-1".into(), ..agent };
        assert!(generic_runtime_task_for_provider(ProviderKind::Cursor, &sdk_agent).is_none());
    }

    #[test]
    fn cursor_sdk_defaults_to_its_sandboxed_policy_without_changing_other_providers() {
        use super::default_provider_permission;
        assert_eq!(default_provider_permission(ProviderKind::Cursor, PermissionMode::Supervised), PermissionMode::Auto);
        assert_eq!(default_provider_permission(ProviderKind::Cursor, PermissionMode::AcceptEdits), PermissionMode::Auto);
        assert_eq!(default_provider_permission(ProviderKind::Cursor, PermissionMode::FullAccess), PermissionMode::FullAccess);
        assert_eq!(default_provider_permission(ProviderKind::ClaudeCode, PermissionMode::Supervised), PermissionMode::Supervised);
    }

    #[tokio::test]
    async fn claude_result_waits_for_background_agents_and_scopes_the_continuation() {
        assert_claude_background_continuation(RuntimeTaskKind::Agent).await;
    }

    #[tokio::test]
    async fn claude_background_process_leaves_the_composer_ready_for_normal_send() {
        assert_claude_background_continuation(RuntimeTaskKind::Process).await;
    }

    #[tokio::test]
    async fn claude_monitor_resumes_without_holding_the_foreground_open() {
        assert_claude_background_continuation(RuntimeTaskKind::Monitor).await;
    }

    async fn assert_claude_background_continuation(kind: RuntimeTaskKind) {
        let root = std::env::temp_dir().join(format!("kybern-orchestrator-test-{}", Uuid::now_v7()));
        let paths = Paths::resolve(Some(root.clone())).unwrap();
        let settings = SettingsStore::load(&paths.settings).unwrap();
        let store = Store::open_in_memory().unwrap();
        let now = chrono::Utc::now();
        let project = Project {
            repo: None,
            repos: Vec::new(),
            id: Uuid::now_v7(),
            name: "fixture".into(),
            path: root.to_string_lossy().into_owned(),
            is_git: false,
            worktrees_default: None,
            task_prefix: None,
            brief_in_all_threads: false,
            coordinator: None,
            space_id: None,
            created_at: now,
            updated_at: now,
            kind: Default::default(),
            members: Vec::new(),
            worktree_copy: Vec::new(),
            instructions_offer_dismissed: false,
        };
        store.project_insert(&project).unwrap();
        let thread = Thread {
            id: Uuid::now_v7(),
            project_id: project.id,
            title: "Fixture".into(),
            provider: ProviderInstance::default_for(ProviderKind::ClaudeCode),
            model: None,
            effort: None,
            permission_mode: PermissionMode::Supervised,
            status: ThreadStatus::Running,
            worktree: None,
            cwd: project.path.clone(),
            provider_session_id: None,
            pinned: false,
            created_at: now,
            updated_at: now,
            last_seq: 0,
            parent_thread_id: None,
            coordinator_project_id: None,
            origin: None,
            inbound: None,
            moved: None,
            subagent: None,
            delegation: None,
            members: Vec::new(),
        };
        store.thread_upsert(&thread).unwrap();
        let (events_tx, _) = crate::bounded_broadcast::channel(32, 8 * 1024 * 1024);
        let orchestrator = Orchestrator::new(store.clone(), DriverRegistry::default(), events_tx, paths, settings);
        let turn_id = Uuid::now_v7();
        orchestrator
            .emit(thread.id, Some(turn_id), EventPayload::TurnStarted { message_id: Uuid::now_v7(), message: UserMessage::text("test") })
            .unwrap();
        let task = RuntimeTask {
            id: "agent-1".into(),
            thread_id: thread.id,
            origin_turn_id: turn_id,
            started_seq: 2,
            updated_seq: 2,
            kind,
            status: RuntimeTaskStatus::Running,
            title: "Explore".into(),
            detail: None,
            provider_type: Some("local_agent".into()),
            parent_id: None,
            tool_call_id: None,
            provider_thread_id: None,
            model: None,
            effort: None,
            backgrounded: true,
            last_tool_name: None,
            usage: None,
            stats: RuntimeTaskStats::default(),
            capabilities: RuntimeTaskCapabilities::default(),
            started_at: now,
            updated_at: now,
            completed_at: None,
        };
        let live = Arc::new(LiveSession {
            session_instance_id: Uuid::now_v7(),
            session: Box::new(TestSession::default()),
            computer_tools: false,
            last_activity: std::sync::Mutex::new(SessionActivityTime::now()),
            released: AtomicBool::new(false),
            retained: AtomicBool::new(false),
            stop_cleanup: AtomicBool::new(false),
            turn: Mutex::new(Some(ActiveTurn {
                response_id: Uuid::now_v7(),
                id: turn_id,
                started: std::time::Instant::now(),
                startup_started: std::time::Instant::now(),
                provider: ProviderKind::ClaudeCode,
                session_reused: false,
                first_event_observed: false,
                messages: HashMap::new(),
                active_messages: HashMap::new(),
                terminal_message_id: None,
                completed: false,
                pending_completion: None,
                internal: None,
                rotation_seed: None,
            })),
            continuation: Mutex::new(None),
            turn_ready: tokio::sync::Notify::new(),
            last_turn_id: Mutex::new(Some(turn_id)),
            tasks: Mutex::new(HashMap::from([(task.id.clone(), task)])),
            task_aliases: Mutex::new(HashMap::new()),
            deferred_checkpoints: Mutex::new(HashSet::new()),
            pending: Mutex::new(HashMap::new()),
            daemon_approvals: Mutex::new(HashMap::new()),
            app_tool_requests: Mutex::new(HashSet::new()),
            app_tool_permits: Arc::new(Semaphore::new(crate::app_tools::MAX_CONCURRENT_REQUESTS)),
            brief_revision: Mutex::new(None),
            lead_settings: Mutex::new(None),
            context_usage: std::sync::Mutex::new(None),
            instructions_unconfirmed: AtomicBool::new(false),
        });
        orchestrator
            .handle_driver_event(
                thread.id,
                &live,
                DriverEvent::TextDelta { message_id: "provider-holding".into(), origin: EventOrigin::Root, delta: "Holding.".into() },
            )
            .await
            .unwrap();
        orchestrator
            .handle_driver_event(
                thread.id,
                &live,
                DriverEvent::MessageCompleted {
                    message_id: "provider-holding".into(),
                    origin: EventOrigin::Root,
                    text: "Holding.".into(),
                    thinking: None,
                },
            )
            .await
            .unwrap();
        let first_usage = Usage { input_tokens: 1, output_tokens: 2, cache_read_tokens: 0, cache_write_tokens: 0 };
        orchestrator
            .handle_driver_event(
                thread.id,
                &live,
                DriverEvent::TurnCompleted {
                    stop_reason: StopReason::Completed,
                    usage: first_usage,
                    cost_usd: Some(0.1),
                    duration_ms: 5,
                    anchors: TurnAnchors::default(),
                },
            )
            .await
            .unwrap();
        if kind == RuntimeTaskKind::Agent {
            assert!(live.turn.lock().await.as_ref().is_some_and(|turn| turn.pending_completion.is_some()));
            assert!(
                !store
                    .events_for_thread(thread.id)
                    .unwrap()
                    .iter()
                    .any(|event| matches!(event.payload, EventPayload::TurnCompleted { .. }))
            );
        } else {
            assert!(live.turn.lock().await.is_none(), "background services and monitors may outlive the foreground request");
            assert_eq!(store.thread_get(thread.id).unwrap().unwrap().status, ThreadStatus::Idle);
        }

        orchestrator
            .handle_driver_event(
                thread.id,
                &live,
                DriverEvent::RuntimeTaskCompleted(DriverRuntimeTaskUpdate::status("agent-1", RuntimeTaskStatus::Completed)),
            )
            .await
            .unwrap();
        if kind == RuntimeTaskKind::Agent {
            // A notification can start another wave of work. Each provisional
            // result must keep the same parent busy without emitting an alert.
            for wave in 2..=3 {
                orchestrator.handle_driver_event(thread.id, &live, DriverEvent::ResponseStarted).await.unwrap();
                let task = DriverRuntimeTask {
                    id: format!("task-{wave}"),
                    kind,
                    status: RuntimeTaskStatus::Running,
                    title: "Follow-up work".into(),
                    detail: None,
                    provider_type: None,
                    parent_id: None,
                    tool_call_id: None,
                    provider_thread_id: None,
                    model: None,
                    effort: None,
                    backgrounded: true,
                    last_tool_name: None,
                    usage: None,
                    stats: RuntimeTaskStats::default(),
                    capabilities: RuntimeTaskCapabilities::default(),
                };
                orchestrator.handle_driver_event(thread.id, &live, DriverEvent::RuntimeTaskStarted(task)).await.unwrap();
                orchestrator
                    .handle_driver_event(
                        thread.id,
                        &live,
                        DriverEvent::TurnCompleted {
                            stop_reason: StopReason::Completed,
                            usage: Usage::default(),
                            cost_usd: None,
                            duration_ms: 1,
                            anchors: TurnAnchors::default(),
                        },
                    )
                    .await
                    .unwrap();
                orchestrator
                    .handle_driver_event(
                        thread.id,
                        &live,
                        DriverEvent::RuntimeTaskCompleted(DriverRuntimeTaskUpdate::status(
                            format!("task-{wave}"),
                            RuntimeTaskStatus::Completed,
                        )),
                    )
                    .await
                    .unwrap();
                assert_eq!(store.thread_get(thread.id).unwrap().unwrap().status, ThreadStatus::Running);
                assert!(
                    !store
                        .events_for_thread(thread.id)
                        .unwrap()
                        .iter()
                        .any(|event| matches!(event.payload, EventPayload::TurnCompleted { .. }))
                );
            }
        }
        orchestrator.handle_driver_event(thread.id, &live, DriverEvent::ResponseStarted).await.unwrap();
        assert_eq!(store.thread_get(thread.id).unwrap().unwrap().status, ThreadStatus::Running);
        assert!(!orchestrator.session_parked(thread.id, &live).await.unwrap());
        let resumed =
            store.events_for_thread(thread.id).unwrap().iter().filter(|event| matches!(event.payload, EventPayload::TurnResumed)).count();
        assert_eq!(resumed, usize::from(kind != RuntimeTaskKind::Agent));
        assert!(
            orchestrator.send(thread.id, UserMessage::text("continue?")).await.is_err(),
            "manual input must not replace a running continuation"
        );
        let queued =
            methods::QueuedMessage { id: Uuid::now_v7(), thread_id: thread.id, message: UserMessage::text("after the continuation") };
        orchestrator.enqueue(queued.clone()).unwrap();
        assert!(orchestrator.drain_queues().await.unwrap());
        assert_eq!(store.queue_list(None).unwrap().len(), 1);
        orchestrator.remove_queued(thread.id, queued.id).unwrap();
        orchestrator
            .handle_driver_event(
                thread.id,
                &live,
                DriverEvent::TextDelta { message_id: "provider-fragment-1".into(), origin: EventOrigin::Root, delta: "Final ".into() },
            )
            .await
            .unwrap();
        orchestrator
            .handle_driver_event(
                thread.id,
                &live,
                DriverEvent::TextDelta { message_id: "provider-fragment-2".into(), origin: EventOrigin::Root, delta: "answer".into() },
            )
            .await
            .unwrap();
        orchestrator
            .handle_driver_event(
                thread.id,
                &live,
                DriverEvent::MessageCompleted {
                    message_id: "provider-completion".into(),
                    origin: EventOrigin::Root,
                    text: "Final answer".into(),
                    thinking: None,
                },
            )
            .await
            .unwrap();
        orchestrator
            .handle_driver_event(
                thread.id,
                &live,
                DriverEvent::TurnCompleted {
                    stop_reason: StopReason::Completed,
                    usage: Usage { input_tokens: 3, output_tokens: 4, cache_read_tokens: 0, cache_write_tokens: 0 },
                    cost_usd: Some(0.2),
                    duration_ms: 7,
                    anchors: TurnAnchors::default(),
                },
            )
            .await
            .unwrap();

        let events = store.events_for_thread(thread.id).unwrap();
        let response_events = events
            .iter()
            .filter(|event| {
                matches!(event.payload, EventPayload::AssistantTextDelta { .. } | EventPayload::AssistantMessageCompleted { .. })
            })
            .collect::<Vec<_>>();
        assert_eq!(response_events.len(), 5);
        assert!(response_events.iter().all(|event| event.turn_id == Some(turn_id)));
        let message_ids = response_events
            .iter()
            .filter_map(|event| match event.payload {
                EventPayload::AssistantTextDelta { message_id, .. } | EventPayload::AssistantMessageCompleted { message_id, .. } => {
                    Some(message_id)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(message_ids[0], message_ids[1]);
        assert!(message_ids[2..].windows(2).all(|pair| pair[0] == pair[1]));
        assert_ne!(message_ids[1], message_ids[2]);
        let completed = events
            .iter()
            .filter_map(|event| match &event.payload {
                EventPayload::TurnCompleted { usage, cost_usd, terminal_message_id, .. } => Some((usage, cost_usd, terminal_message_id)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(completed.len(), if kind == RuntimeTaskKind::Agent { 1 } else { 2 });
        let completed = &completed[completed.len() - 1..];
        assert_eq!(completed[0].0.input_tokens, 4);
        assert_eq!(completed[0].0.output_tokens, 6);
        assert!((completed[0].1.unwrap_or_default() - 0.3).abs() < f64::EPSILON);
        assert_eq!(*completed[0].2, Some(message_ids[2]));
        assert!(live.turn.lock().await.is_none());
        let projected = kybern_store::project_transcript(&events);
        assert_eq!(projected.iter().filter(|entry| matches!(entry, TranscriptEntry::TurnSummary { .. })).count(), 1);
        assert_eq!(projected.iter().filter(|entry| matches!(entry, TranscriptEntry::User { .. })).count(), 1);
        assert_eq!(store.thread_get(thread.id).unwrap().unwrap().status, ThreadStatus::Idle);

        orchestrator.inner.sessions.lock().await.insert(thread.id, live.clone());
        let follow_up = methods::QueuedMessage { id: Uuid::now_v7(), thread_id: thread.id, message: UserMessage::text("next") };
        orchestrator.enqueue(follow_up.clone()).unwrap();
        orchestrator.drain_queues().await.unwrap();
        assert!(store.queue_list(None).unwrap().is_empty());
        assert!(
            store
                .events_for_thread(thread.id)
                .unwrap()
                .iter()
                .any(|event| matches!(event.payload, EventPayload::TurnStarted { message_id, .. } if message_id == follow_up.id))
        );
        let pending = methods::QueuedMessage { id: Uuid::now_v7(), ..follow_up };
        orchestrator.enqueue(pending).unwrap();
        let mut late_completion = store.thread_get(thread.id).unwrap().unwrap();
        orchestrator.archive_thread(thread.id).await.unwrap();
        late_completion.status = ThreadStatus::Idle;
        orchestrator.update_thread(late_completion).unwrap();
        orchestrator.drain_queues().await.unwrap();
        assert_eq!(store.thread_get(thread.id).unwrap().unwrap().status, ThreadStatus::Archived);
        assert!(store.queue_list(None).unwrap().is_empty());
        orchestrator.shutdown().await;

        std::fs::remove_dir_all(root).unwrap();
    }

    struct Fixture {
        root: std::path::PathBuf,
        store: Store,
        orchestrator: Orchestrator,
        project: Project,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_drivers(DriverRegistry::default())
        }

        fn with_drivers(drivers: DriverRegistry) -> Self {
            Self::with_drivers_and_gateway(drivers, None)
        }

        fn with_drivers_and_gateway(drivers: DriverRegistry, gateway: Option<crate::native_tools_mcp::NativeToolsGateway>) -> Self {
            Self::build(drivers, gateway, false)
        }

        /// A fixture whose database is a real SQLite file (and WAL) under its data directory.
        fn on_disk() -> Self {
            Self::build(DriverRegistry::default(), None, true)
        }

        fn build(drivers: DriverRegistry, gateway: Option<crate::native_tools_mcp::NativeToolsGateway>, on_disk: bool) -> Self {
            let root = std::env::temp_dir().join(format!("kybern-orchestrator-test-{}", Uuid::now_v7()));
            let paths = Paths::resolve(Some(root.clone())).unwrap();
            let settings = SettingsStore::load(&paths.settings).unwrap();
            // Kybern tools for Antigravity are on by default, and turning them on runs this machine's
            // real `agy mcp add`. Tests start with them off; a test that needs them on stubs `agy`.
            let mut initial = settings.get();
            initial.antigravity.kybern_tools = false;
            settings.set(initial).unwrap();
            let store = if on_disk { Store::open(&root.join("state.sqlite")).unwrap() } else { Store::open_in_memory().unwrap() };
            let now = chrono::Utc::now();
            let project = Project {
                repo: None,
                repos: Vec::new(),
                id: Uuid::now_v7(),
                name: "fixture".into(),
                path: root.to_string_lossy().into_owned(),
                is_git: false,
                worktrees_default: None,
                task_prefix: None,
                brief_in_all_threads: false,
                coordinator: None,
                space_id: None,
                created_at: now,
                updated_at: now,
                kind: Default::default(),
                members: Vec::new(),
                worktree_copy: Vec::new(),
                instructions_offer_dismissed: false,
            };
            store.project_insert(&project).unwrap();
            let (events_tx, _) = crate::bounded_broadcast::channel(64, 8 * 1024 * 1024);
            let mut orchestrator = Orchestrator::new(store.clone(), drivers, events_tx, paths, settings);
            if let Some(gateway) = gateway {
                orchestrator = orchestrator.with_native_tools(gateway);
            }
            Self { root, store, orchestrator, project }
        }

        fn thread(&self, status: ThreadStatus) -> Thread {
            self.thread_with_provider(status, ProviderKind::ClaudeCode)
        }

        fn thread_with_provider(&self, status: ThreadStatus, provider: ProviderKind) -> Thread {
            let now = chrono::Utc::now();
            let thread = Thread {
                id: Uuid::now_v7(),
                project_id: self.project.id,
                title: "Fixture".into(),
                provider: ProviderInstance::default_for(provider),
                model: None,
                effort: None,
                permission_mode: PermissionMode::Supervised,
                status,
                worktree: None,
                cwd: self.project.path.clone(),
                provider_session_id: Some("provider-session".into()),
                pinned: false,
                created_at: now,
                updated_at: now,
                last_seq: 0,
                parent_thread_id: None,
                coordinator_project_id: None,
                origin: None,
                inbound: None,
                moved: None,
                subagent: None,
                delegation: None,
                members: Vec::new(),
            };
            self.store.thread_upsert(&thread).unwrap();
            thread
        }

        /// Attach a parked session whose last activity was `last_activity`.
        async fn park(&self, thread: &Thread, last_activity: Instant) -> (Arc<LiveSession>, Arc<AtomicUsize>) {
            let (live, closes, _) = self.park_recording(thread, last_activity).await;
            (live, closes)
        }

        async fn park_recording(
            &self,
            thread: &Thread,
            last_activity: Instant,
        ) -> (Arc<LiveSession>, Arc<AtomicUsize>, Arc<Mutex<Vec<UserMessage>>>) {
            let closes = Arc::new(AtomicUsize::new(0));
            let messages = Arc::new(Mutex::new(Vec::new()));
            let live = Arc::new(LiveSession {
                session_instance_id: Uuid::now_v7(),
                session: Box::new(TestSession { closes: closes.clone(), messages: messages.clone(), ..Default::default() }),
                computer_tools: false,
                last_activity: std::sync::Mutex::new(SessionActivityTime { monotonic: last_activity, wall: SystemTime::now() }),
                released: AtomicBool::new(false),
                retained: AtomicBool::new(false),
                stop_cleanup: AtomicBool::new(false),
                turn: Mutex::new(None),
                continuation: Mutex::new(None),
                turn_ready: tokio::sync::Notify::new(),
                last_turn_id: Mutex::new(None),
                tasks: Mutex::new(HashMap::new()),
                task_aliases: Mutex::new(HashMap::new()),
                deferred_checkpoints: Mutex::new(HashSet::new()),
                pending: Mutex::new(HashMap::new()),
                daemon_approvals: Mutex::new(HashMap::new()),
                app_tool_requests: Mutex::new(HashSet::new()),
                app_tool_permits: Arc::new(Semaphore::new(crate::app_tools::MAX_CONCURRENT_REQUESTS)),
                brief_revision: Mutex::new(None),
                lead_settings: Mutex::new(None),
                context_usage: std::sync::Mutex::new(None),
                instructions_unconfirmed: AtomicBool::new(false),
            });
            self.orchestrator.inner.sessions.lock().await.insert(thread.id, live.clone());
            (live, closes, messages)
        }

        async fn active_app_tool_session(&self, thread: &Thread) -> (Arc<LiveSession>, CapturedAppToolResponses) {
            let responses = Arc::new(Mutex::new(Vec::new()));
            let live = self.active_session(thread, TestSession { app_tool_responses: responses.clone(), ..Default::default() }).await;
            (live, responses)
        }

        /// A live session around `session` with an active turn.
        async fn active_session(&self, thread: &Thread, session: TestSession) -> Arc<LiveSession> {
            let live = Arc::new(LiveSession {
                session_instance_id: Uuid::now_v7(),
                session: Box::new(session),
                computer_tools: false,
                last_activity: std::sync::Mutex::new(SessionActivityTime::now()),
                released: AtomicBool::new(false),
                retained: AtomicBool::new(false),
                stop_cleanup: AtomicBool::new(false),
                turn: Mutex::new(None),
                continuation: Mutex::new(None),
                turn_ready: tokio::sync::Notify::new(),
                last_turn_id: Mutex::new(None),
                tasks: Mutex::new(HashMap::new()),
                task_aliases: Mutex::new(HashMap::new()),
                deferred_checkpoints: Mutex::new(HashSet::new()),
                pending: Mutex::new(HashMap::new()),
                daemon_approvals: Mutex::new(HashMap::new()),
                app_tool_requests: Mutex::new(HashSet::new()),
                app_tool_permits: Arc::new(Semaphore::new(crate::app_tools::MAX_CONCURRENT_REQUESTS)),
                brief_revision: Mutex::new(None),
                lead_settings: Mutex::new(None),
                context_usage: std::sync::Mutex::new(None),
                instructions_unconfirmed: AtomicBool::new(false),
            });
            self.orchestrator.inner.sessions.lock().await.insert(thread.id, live.clone());
            self.orchestrator.send(thread.id, UserMessage::text("inspect workspace")).await.unwrap();
            live.turn_ready.notified().await;
            live
        }

        async fn has_session(&self, thread: &Thread) -> bool {
            self.orchestrator.inner.sessions.lock().await.contains_key(&thread.id)
        }

        fn release_events(&self, thread: &Thread) -> Vec<SessionReleaseReason> {
            self.store
                .events_for_thread(thread.id)
                .unwrap()
                .iter()
                .filter_map(|event| match event.payload {
                    EventPayload::ProviderSessionReleased { reason } => Some(reason),
                    _ => None,
                })
                .collect()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn completed_response(stop_reason: StopReason) -> DriverEvent {
        DriverEvent::TurnCompleted {
            stop_reason,
            usage: Usage::default(),
            cost_usd: Some(0.0),
            duration_ms: 1,
            anchors: TurnAnchors::default(),
        }
    }

    #[tokio::test]
    async fn computer_consent_resolves_in_the_daemon_and_is_reused_on_retry() {
        use crate::computer::{ConsentAnswer, ConsentRequest, Mode};
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let (live, _) = fixture.active_app_tool_session(&thread).await;
        let turn_id = live.turn.lock().await.as_ref().unwrap().id;
        let request = || ConsentRequest {
            app: "Notes".into(),
            app_id: Some("com.apple.Notes".into()),
            mode: Mode::Background,
            first_action: "press @1".into(),
        };
        let asking = {
            let (orchestrator, live, thread_id) = (fixture.orchestrator.clone(), live.clone(), thread.id);
            tokio::spawn(async move { orchestrator.ask_computer_consent(thread_id, turn_id, &live, request()).await })
        };
        let approval_id = loop {
            if let Some(id) = live.daemon_approvals.lock().await.keys().next().copied() {
                break id;
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(fixture.store.thread_get(thread.id).unwrap().unwrap().status, ThreadStatus::AwaitingApproval);
        tokio::time::timeout(Duration::from_secs(2), fixture.orchestrator.respond_approval(approval_id, ApprovalDecision::AllowAlways))
            .await
            .expect("answering a daemon approval must not deadlock")
            .unwrap();
        assert_eq!(asking.await.unwrap().unwrap(), ConsentAnswer::Session);
        assert_eq!(fixture.store.thread_get(thread.id).unwrap().unwrap().status, ThreadStatus::Running);
        // A retry in the same turn reuses the answer instead of opening a new card.
        let retry =
            tokio::time::timeout(Duration::from_secs(2), fixture.orchestrator.ask_computer_consent(thread.id, turn_id, &live, request()))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(retry, ConsentAnswer::Session);
        assert_eq!(live.daemon_approvals.lock().await.len(), 1);
        assert!(fixture.orchestrator.respond_approval(approval_id, ApprovalDecision::AllowOnce).await.is_err());
        fixture.orchestrator.resolve_finished_requests(thread.id, turn_id, &live).await.unwrap();
        assert!(live.daemon_approvals.lock().await.is_empty());
    }

    impl Fixture {
        /// A thread in `mode` with an active turn, ready for native tool calls.
        async fn tool_thread(&self, mode: PermissionMode) -> (Thread, Arc<LiveSession>) {
            let mut thread = self.thread(ThreadStatus::Idle);
            thread.permission_mode = mode;
            self.store.thread_upsert(&thread).unwrap();
            let (live, _) = self.active_app_tool_session(&thread).await;
            (thread, live)
        }

        async fn tool(
            &self,
            thread: &Thread,
            live: &LiveSession,
            call: &str,
            name: &str,
            args: serde_json::Value,
        ) -> anyhow::Result<serde_json::Value> {
            self.orchestrator.execute_native_app_tool_call(thread.id, live.session_instance_id, call, name, args).await
        }
    }

    #[tokio::test]
    async fn agents_edit_only_their_own_notes_and_tasks_and_never_close_tasks() {
        let fixture = Fixture::new();
        let (thread, live) = fixture.tool_thread(PermissionMode::FullAccess).await;
        let notes_changed = fixture.orchestrator.subscribe_notes();
        let mut tasks_changed = fixture.orchestrator.subscribe_tasks();

        // The user's note: append only.
        let user_note = fixture
            .orchestrator
            .note_create(methods::NotesCreateParams {
                scope: methods::NoteScope::Project,
                project_id: Some(fixture.project.id),
                title: Some("Plan".into()),
                body: Some("Ship it".into()),
                project_role: None,
                category: None,
            })
            .unwrap();
        let error = fixture
            .tool(&thread, &live, "u1", "kybern_note_update", json!({"note_id": user_note.summary.id, "body": "mine now"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("only append"), "{error}");
        let appended = fixture
            .tool(&thread, &live, "a1", "kybern_note_append", json!({"note_id": user_note.summary.id, "text": "Found the bug."}))
            .await
            .unwrap();
        assert_eq!((appended["action"].clone(), appended["agent_editable"].clone()), (json!("appended"), json!(false)));
        assert_eq!(fixture.store.note_get(user_note.summary.id).unwrap().unwrap().body, "Ship it\n\nFound the bug.");
        // Without a note id the thread's own note is created and appended to.
        fixture.tool(&thread, &live, "a2", "kybern_note_append", json!({"text": "First"})).await.unwrap();
        fixture.tool(&thread, &live, "a3", "kybern_note_append", json!({"text": "Second"})).await.unwrap();
        assert_eq!(fixture.store.note_for_thread(thread.id).unwrap().unwrap().body, "First\n\nSecond");

        // An agent's note: fully editable, attributed, linked.
        let created = fixture.tool(&thread, &live, "c1", "kybern_note_create", json!({"title": "Findings", "body": "one"})).await.unwrap();
        let note_id: Uuid = serde_json::from_value(created["id"].clone()).unwrap();
        assert_eq!(created["link"], json!(format!("kybern://note/{note_id}")));
        assert_eq!(created["markdown"], json!(format!("[Findings](kybern://note/{note_id})")));
        assert_eq!(created["created_by_thread"], json!(thread.id));
        assert_eq!(created["scope"], json!("project"));
        // Replacing text needs the revision it was based on.
        let error =
            fixture.tool(&thread, &live, "u2-blind", "kybern_note_update", json!({"note_id": note_id, "body": "blind"})).await.unwrap_err();
        assert!(error.to_string().contains("Pass expected_revision") && error.to_string().contains("kybern_note_read"), "{error}");
        let revision = created["revision"].as_i64().unwrap();
        let error = fixture
            .tool(
                &thread,
                &live,
                "u2-stale",
                "kybern_note_update",
                json!({"note_id": note_id, "body": "stale", "expected_revision": revision - 1}),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("changed since you read it"), "{error}");
        assert_eq!(fixture.store.note_get(note_id).unwrap().unwrap().body, "one", "a rejected update writes nothing");
        fixture
            .tool(
                &thread,
                &live,
                "u2",
                "kybern_note_update",
                json!({"note_id": note_id, "title": "Findings v2", "body": "two", "expected_revision": revision}),
            )
            .await
            .unwrap();
        let note = fixture.store.note_get(note_id).unwrap().unwrap();
        assert_eq!((note.summary.title.as_str(), note.body.as_str()), ("Findings v2", "two"));
        assert!(notes_changed.len() >= 4, "every write is published");

        // The user's task: append, check, priority; no title, no close, no review without a run.
        let user_task = fixture
            .orchestrator
            .task_item_create(methods::TaskItemsCreateParams {
                scope: methods::TaskScope::Project,
                project_id: Some(fixture.project.id),
                title: "Fix login".into(),
                body: Some("Redirect.\n\n- [ ] Lands on home\n- [ ] Has a test\n".into()),
                status: Some(methods::TaskStatus::Todo),
                priority: None,
                note_ids: None,
                source: None,
            })
            .unwrap();
        while tasks_changed.try_recv().is_ok() {}
        let error =
            fixture.tool(&thread, &live, "t1", "kybern_task_update", json!({"task": user_task.key, "title": "Mine"})).await.unwrap_err();
        assert!(error.to_string().contains("cannot replace its title"), "{error}");
        for status in ["done", "canceled"] {
            let error = fixture
                .tool(&thread, &live, status, "kybern_task_update", json!({"task": user_task.key, "status": status}))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("Only the user closes a task"), "{error}");
        }
        let error = fixture
            .tool(&thread, &live, "t2", "kybern_task_update", json!({"task": user_task.key, "status": "needs_review"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Claim it with kybern_task_claim first"), "{error}");
        let updated = fixture
            .tool(
                &thread,
                &live,
                "t3",
                "kybern_task_update",
                json!({"task": user_task.key, "append": "Safari only.", "check": [1], "priority": 2}),
            )
            .await
            .unwrap();
        assert_eq!((updated["criteria_done"].clone(), updated["priority"].clone()), (json!(1), json!(2)));
        assert_eq!(updated["markdown"], json!(format!("[{} {}](kybern://task/{})", user_task.key, user_task.title, user_task.id)));
        let task = fixture.store.task_item_get(user_task.id).unwrap().unwrap();
        assert_eq!(task.body, "Redirect.\n\nSafari only.\n\n- [x] Lands on home\n- [ ] Has a test\n");
        assert!(tasks_changed.try_recv().is_ok(), "the update is published");

        // Claiming links this chat as the run; then it may hand the task over for review.
        let claimed = fixture.tool(&thread, &live, "k1", "kybern_task_claim", json!({"task": user_task.key})).await.unwrap();
        assert_eq!((claimed["action"].clone(), claimed["status"].clone()), (json!("claimed"), json!("running")));
        let task = fixture.store.task_item_get(user_task.id).unwrap().unwrap();
        assert_eq!(task.runs.last().map(|run| run.thread_id), Some(thread.id));
        let context = fixture.tool(&thread, &live, "ctx", "kybern_thread_context", json!({})).await.unwrap();
        assert_eq!(context["task"]["key"], json!(user_task.key), "the context names the task this chat runs");
        let reviewed = fixture
            .tool(
                &thread,
                &live,
                "t4",
                "kybern_task_update",
                json!({"task": format!("kybern://task/{}", user_task.id), "status": "needs_review"}),
            )
            .await
            .unwrap();
        assert_eq!(reviewed["status"], json!("needs_review"));
        // Another chat cannot claim a task whose run is live.
        let (other, other_live) = fixture.tool_thread(PermissionMode::FullAccess).await;
        let second = fixture
            .orchestrator
            .task_item_create(methods::TaskItemsCreateParams {
                scope: methods::TaskScope::Global,
                project_id: None,
                title: "Busy".into(),
                body: None,
                status: None,
                priority: None,
                note_ids: None,
                source: None,
            })
            .unwrap();
        fixture.tool(&other, &other_live, "k2", "kybern_task_claim", json!({"task": second.key})).await.unwrap();
        let error = fixture.tool(&thread, &live, "k3", "kybern_task_claim", json!({"task": second.key})).await.unwrap_err();
        assert!(error.to_string().contains("already has a run in progress in another chat"), "{error}");

        // An agent's task lands in the Inbox, attributed, and is fully editable.
        let filed = fixture
            .tool(
                &thread,
                &live,
                "n1",
                "kybern_task_create",
                json!({"title": "Flaky test", "description": "Seen twice.", "criteria": ["Passes 20 runs"], "priority": 3}),
            )
            .await
            .unwrap();
        assert_eq!((filed["status"].clone(), filed["created_by_thread"].clone()), (json!("inbox"), json!(thread.id)));
        let filed_id: Uuid = serde_json::from_value(filed["id"].clone()).unwrap();
        assert_eq!(fixture.store.task_item_get(filed_id).unwrap().unwrap().body, "Seen twice.\n\n- [ ] Passes 20 runs\n");
        fixture.tool(&thread, &live, "n2", "kybern_task_update", json!({"task": filed["key"], "title": "Flaky login test"})).await.unwrap();
        // A retried call returns the first result instead of filing twice.
        let retried = fixture
            .tool(
                &thread,
                &live,
                "n1",
                "kybern_task_create",
                json!({"title": "Flaky test", "description": "Seen twice.", "criteria": ["Passes 20 runs"], "priority": 3}),
            )
            .await
            .unwrap();
        assert_eq!(retried["id"], filed["id"]);
        let reads = fixture.tool(&thread, &live, "r1", "kybern_tasks_list", json!({"query": "flaky"})).await.unwrap();
        assert_eq!(reads["tasks"].as_array().unwrap().len(), 1);
        let read = fixture.tool(&thread, &live, "r2", "kybern_task_read", json!({"task": user_task.key})).await.unwrap();
        assert_eq!(read["criteria"][0], json!({"number": 1, "text": "Lands on home", "checked": true}));
        let search = fixture.tool(&thread, &live, "r3", "kybern_notes_search", json!({"query": "findings"})).await.unwrap();
        assert_eq!(search["notes"][0]["id"], json!(note_id));

        // The daemon stops a turn after ten filed tasks.
        for index in 1..super::agent_items::MAX_TASK_CREATES_PER_TURN {
            fixture
                .tool(&thread, &live, &format!("cap{index}"), "kybern_task_create", json!({"title": format!("Follow-up {index}")}))
                .await
                .unwrap();
        }
        let error = fixture.tool(&thread, &live, "cap-over", "kybern_task_create", json!({"title": "One too many"})).await.unwrap_err();
        assert!(error.to_string().contains("ask the user"), "{error}");
    }

    #[tokio::test]
    async fn supervised_note_and_task_writes_ask_and_remember_allow_for_this_thread() {
        let fixture = Fixture::new();
        let (thread, live) = fixture.tool_thread(PermissionMode::Supervised).await;
        // Reads never ask.
        fixture.tool(&thread, &live, "list", "kybern_tasks_list", json!({})).await.unwrap();
        assert!(live.daemon_approvals.lock().await.is_empty());

        let creating = {
            let (orchestrator, thread_id, session) = (fixture.orchestrator.clone(), thread.id, live.session_instance_id);
            tokio::spawn(async move {
                orchestrator
                    .execute_native_app_tool_call(thread_id, session, "c1", "kybern_task_create", json!({"title": "Fix flaky login test"}))
                    .await
            })
        };
        let approval_id = loop {
            if let Some(id) = live.daemon_approvals.lock().await.keys().next().copied() {
                break id;
            }
            tokio::task::yield_now().await;
        };
        let (request, _) = fixture.store.approval_get(approval_id).unwrap().unwrap();
        assert_eq!(request.tool_name, "kybern_notes_tasks");
        assert_eq!(request.summary, "Create task 'Fix flaky login test' in fixture");
        assert_eq!((request.input["action"].clone(), request.input["tool"].clone()), (json!("create_task"), json!("kybern_task_create")));
        assert!(fixture.store.task_items_list().unwrap().is_empty(), "nothing is filed before the user answers");
        fixture.orchestrator.respond_approval(approval_id, ApprovalDecision::AllowAlways).await.unwrap();
        let created = creating.await.unwrap().unwrap();
        assert_eq!(created["title"], json!("Fix flaky login test"));
        // "Allow for this thread" covers later writes without another card.
        fixture.tool(&thread, &live, "c2", "kybern_note_append", json!({"text": "noted"})).await.unwrap();
        assert_eq!(live.daemon_approvals.lock().await.len(), 1);

        // A denial in another supervised thread is reported and writes nothing.
        let (other, other_live) = fixture.tool_thread(PermissionMode::AcceptEdits).await;
        let denied = {
            let (orchestrator, thread_id, session) = (fixture.orchestrator.clone(), other.id, other_live.session_instance_id);
            tokio::spawn(async move {
                orchestrator
                    .execute_native_app_tool_call(thread_id, session, "d1", "kybern_note_create", json!({"title": "Nope", "body": ""}))
                    .await
            })
        };
        let approval_id = loop {
            if let Some(id) = other_live.daemon_approvals.lock().await.keys().next().copied() {
                break id;
            }
            tokio::task::yield_now().await;
        };
        fixture.orchestrator.respond_approval(approval_id, ApprovalDecision::Deny { reason: None }).await.unwrap();
        let error = denied.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("The user declined"), "{error}");
        assert!(fixture.store.notes_list().unwrap().iter().all(|note| note.title != "Nope"));
    }

    impl Fixture {
        fn user_task(&self, title: &str, status: methods::TaskStatus) -> methods::TaskItem {
            let task = self
                .orchestrator
                .task_item_create(methods::TaskItemsCreateParams {
                    scope: methods::TaskScope::Project,
                    project_id: Some(self.project.id),
                    title: title.into(),
                    body: None,
                    status: Some(methods::TaskStatus::Todo),
                    priority: None,
                    note_ids: None,
                    source: None,
                })
                .unwrap();
            if status != methods::TaskStatus::Todo {
                self.store.task_item_set_status(task.id, status, None).unwrap();
            }
            self.store.task_item_get(task.id).unwrap().unwrap()
        }

        /// The first daemon approval card of `live` that is not in `seen`.
        async fn next_card(&self, live: &LiveSession, seen: &[ApprovalId]) -> ApprovalId {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(id) = live.daemon_approvals.lock().await.keys().find(|id| !seen.contains(id)).copied() {
                        break id;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("an approval card opens")
        }

        fn spawn_tool(
            &self,
            thread: &Thread,
            live: &LiveSession,
            call: &str,
            name: &'static str,
            args: serde_json::Value,
        ) -> tokio::task::JoinHandle<anyhow::Result<serde_json::Value>> {
            let (orchestrator, thread_id, session, call) =
                (self.orchestrator.clone(), thread.id, live.session_instance_id, call.to_owned());
            tokio::spawn(async move { orchestrator.execute_native_app_tool_call(thread_id, session, &call, name, args).await })
        }
    }

    #[tokio::test]
    async fn agent_status_changes_never_reopen_pull_back_or_hand_over_a_stale_run() {
        use methods::TaskStatus;
        let fixture = Fixture::new();
        let (thread, live) = fixture.tool_thread(PermissionMode::FullAccess).await;
        // Inbox and To do only move between each other.
        let inbox = fixture.user_task("Triage", TaskStatus::Inbox);
        let moved = fixture.tool(&thread, &live, "m1", "kybern_task_update", json!({"task": inbox.key, "status": "todo"})).await.unwrap();
        assert_eq!(moved["status"], json!("todo"));
        // Closed, running and in-review tasks are never moved back.
        for status in [TaskStatus::Done, TaskStatus::Canceled, TaskStatus::Running, TaskStatus::NeedsReview] {
            let task = fixture.user_task("Settled", status);
            for target in ["inbox", "todo"] {
                let error = fixture
                    .tool(
                        &thread,
                        &live,
                        &format!("{status:?}-{target}"),
                        "kybern_task_update",
                        json!({"task": task.key, "status": target}),
                    )
                    .await
                    .unwrap_err();
                assert!(error.to_string().contains("only between Inbox and To do"), "{status:?} -> {target}: {error}");
                assert_eq!(fixture.store.task_item_get(task.id).unwrap().unwrap().status, status);
            }
        }

        // The handover needs the task Running...
        let task = fixture.user_task("Fix login", TaskStatus::Todo);
        fixture.tool(&thread, &live, "k1", "kybern_task_claim", json!({"task": task.key})).await.unwrap();
        fixture.store.task_item_set_status(task.id, TaskStatus::Todo, None).unwrap();
        let error = fixture
            .tool(&thread, &live, "h1", "kybern_task_update", json!({"task": task.key, "status": "needs_review"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not Running"), "{error}");
        assert_eq!(fixture.store.task_item_get(task.id).unwrap().unwrap().status, TaskStatus::Todo);

        // ...and this chat's run to be the latest one.
        let (newer, _) = fixture.tool_thread(PermissionMode::FullAccess).await;
        fixture.store.task_run_start(task.id, newer.id, &newer.provider, None, &[]).unwrap();
        let error = fixture
            .tool(&thread, &live, "h2", "kybern_task_update", json!({"task": task.key, "status": "needs_review"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("newer run"), "{error}");
        assert_eq!(fixture.store.task_item_get(task.id).unwrap().unwrap().status, TaskStatus::Running);
    }

    #[tokio::test]
    async fn one_chat_claims_several_tasks_and_hands_them_over_together_or_one_by_one() {
        let fixture = Fixture::new();
        let (thread, live) = fixture.tool_thread(PermissionMode::FullAccess).await;
        let (a, b, c) = (
            fixture.user_task("First", methods::TaskStatus::Todo),
            fixture.user_task("Second", methods::TaskStatus::Todo),
            fixture.user_task("Third", methods::TaskStatus::Todo),
        );
        let status = |task: &methods::TaskItem| fixture.store.task_item_get(task.id).unwrap().unwrap().status;

        // A list claims every task, and the chat is the run of each.
        let claimed = fixture.tool(&thread, &live, "m1", "kybern_task_claim", json!({"tasks": [a.key, b.key]})).await.unwrap();
        assert_eq!(claimed["claimed"].as_array().unwrap().len(), 2);
        for task in [&a, &b] {
            let current = fixture.store.task_item_get(task.id).unwrap().unwrap();
            assert_eq!((current.status, current.runs.last().map(|run| run.thread_id)), (methods::TaskStatus::Running, Some(thread.id)));
        }
        // A later claim in the same chat adds a third task; claiming again is idempotent.
        fixture.tool(&thread, &live, "m2", "kybern_task_claim", json!({"task": c.key})).await.unwrap();
        fixture.tool(&thread, &live, "m3", "kybern_task_claim", json!({"tasks": [c.key, a.key]})).await.unwrap();
        assert_eq!(fixture.store.task_runs_for_thread(thread.id).unwrap().len(), 3);
        assert_eq!(fixture.store.task_item_get(a.id).unwrap().unwrap().runs.len(), 1, "no second run");

        // One task is handed over early; claiming another reopens only the claimed one.
        let reviewed =
            fixture.tool(&thread, &live, "m4", "kybern_task_update", json!({"task": a.key, "status": "needs_review"})).await.unwrap();
        assert_eq!(reviewed["status"], json!("needs_review"));
        assert_eq!(
            (status(&a), status(&b), status(&c)),
            (methods::TaskStatus::NeedsReview, methods::TaskStatus::Running, methods::TaskStatus::Running)
        );
        fixture.tool(&thread, &live, "m5", "kybern_task_update", json!({"task": b.key, "status": "needs_review"})).await.unwrap();
        fixture.tool(&thread, &live, "m6", "kybern_task_claim", json!({"task": c.key})).await.unwrap();
        assert_eq!(
            (status(&a), status(&b), status(&c)),
            (methods::TaskStatus::NeedsReview, methods::TaskStatus::NeedsReview, methods::TaskStatus::Running)
        );
        // Reopening one task leaves the others' handover alone; reclaiming it comes back Running.
        fixture.tool(&thread, &live, "m7", "kybern_task_claim", json!({"task": b.key})).await.unwrap();
        assert_eq!((status(&a), status(&b)), (methods::TaskStatus::NeedsReview, methods::TaskStatus::Running));

        // The turn's end moves the tasks still Running, and only those, to Needs review.
        let turn = live.turn.lock().await.as_ref().map(|turn| turn.id);
        fixture
            .orchestrator
            .emit(
                thread.id,
                turn,
                EventPayload::TurnCompleted {
                    stop_reason: StopReason::Completed,
                    usage: Usage::default(),
                    cost_usd: None,
                    duration_ms: 1,
                    terminal_message_id: None,
                },
            )
            .unwrap();
        for task in [&a, &b, &c] {
            assert_eq!(status(task), methods::TaskStatus::NeedsReview, "{}", task.key);
        }
    }

    #[tokio::test]
    async fn a_list_claim_is_all_or_none_and_refuses_closed_or_busy_tasks() {
        let fixture = Fixture::new();
        let (thread, live) = fixture.tool_thread(PermissionMode::FullAccess).await;
        let (other, other_live) = fixture.tool_thread(PermissionMode::FullAccess).await;
        let (a, busy, closed) = (
            fixture.user_task("Free", methods::TaskStatus::Todo),
            fixture.user_task("Busy", methods::TaskStatus::Todo),
            fixture.user_task("Closed", methods::TaskStatus::Done),
        );
        fixture.tool(&other, &other_live, "o1", "kybern_task_claim", json!({"task": busy.key})).await.unwrap();

        let error = fixture.tool(&thread, &live, "l1", "kybern_task_claim", json!({"tasks": [a.key, busy.key]})).await.unwrap_err();
        assert!(error.to_string().contains("already has a run in progress in another chat"), "{error}");
        let error = fixture.tool(&thread, &live, "l2", "kybern_task_claim", json!({"tasks": [a.key, closed.key]})).await.unwrap_err();
        assert!(error.to_string().contains("closed"), "{error}");
        let error = fixture.tool(&thread, &live, "l3", "kybern_task_claim", json!({"task": a.key, "tasks": [a.key]})).await.unwrap_err();
        assert!(error.to_string().contains("either task or tasks"), "{error}");
        let keys: Vec<String> = (0..11).map(|_| a.key.clone()).collect();
        let error = fixture.tool(&thread, &live, "l4", "kybern_task_claim", json!({"tasks": keys})).await.unwrap_err();
        assert!(error.to_string().contains("at most 10"), "{error}");
        let free = fixture.store.task_item_get(a.id).unwrap().unwrap();
        assert!(free.runs.is_empty() && free.status == methods::TaskStatus::Todo, "nothing was claimed");
        assert!(fixture.store.task_runs_for_thread(thread.id).unwrap().is_empty());
    }

    #[tokio::test]
    async fn parallel_task_creates_stop_at_the_cap_even_while_cards_are_open() {
        let fixture = Fixture::new();
        let cap = super::agent_items::MAX_TASK_CREATES_PER_TURN as usize;
        // Full access: parallel calls race straight to the store.
        let (thread, live) = fixture.tool_thread(PermissionMode::FullAccess).await;
        let calls: Vec<_> = (0..cap + 5)
            .map(|index| {
                fixture.spawn_tool(&thread, &live, &format!("p{index}"), "kybern_task_create", json!({"title": format!("Task {index}")}))
            })
            .collect();
        let mut filed = 0;
        for call in calls {
            match call.await.unwrap() {
                Ok(_) => filed += 1,
                Err(error) => assert!(error.to_string().contains("the most allowed"), "{error}"),
            }
        }
        assert_eq!(filed, cap);
        assert_eq!(fixture.store.task_items_list().unwrap().len(), cap);

        // Supervised: creates waiting on a card hold their slot, so no eleventh card opens.
        let (thread, live) = fixture.tool_thread(PermissionMode::Supervised).await;
        let calls: Vec<_> = (0..cap + 2)
            .map(|index| {
                fixture.spawn_tool(&thread, &live, &format!("s{index}"), "kybern_task_create", json!({"title": format!("Card {index}")}))
            })
            .collect();
        let mut cards = Vec::new();
        while cards.len() < cap {
            cards.push(fixture.next_card(&live, &cards).await);
        }
        for card in &cards {
            fixture.orchestrator.respond_approval(*card, ApprovalDecision::AllowOnce).await.unwrap();
        }
        let mut filed = 0;
        for call in calls {
            if call.await.unwrap().is_ok() {
                filed += 1;
            }
        }
        assert_eq!(filed, cap);
        assert_eq!(live.daemon_approvals.lock().await.len(), cap, "the calls over the cap never asked");
    }

    #[tokio::test]
    async fn allow_once_covers_one_write_and_only_its_retry() {
        let fixture = Fixture::new();
        let (thread, live) = fixture.tool_thread(PermissionMode::Supervised).await;
        let append = json!({"text": "Same line"});
        let first = fixture.spawn_tool(&thread, &live, "a1", "kybern_note_append", append.clone());
        let card = fixture.next_card(&live, &[]).await;
        fixture.orchestrator.respond_approval(card, ApprovalDecision::AllowOnce).await.unwrap();
        let written = first.await.unwrap().unwrap();
        // A retry of the same call returns its receipt without asking.
        let retried = fixture.tool(&thread, &live, "a1", "kybern_note_append", append.clone()).await.unwrap();
        assert_eq!(retried, written);
        assert_eq!(live.daemon_approvals.lock().await.len(), 1);
        // An identical second write needs its own answer.
        let second = fixture.spawn_tool(&thread, &live, "a2", "kybern_note_append", append.clone());
        let second_card = fixture.next_card(&live, &[card]).await;
        assert!(!second.is_finished(), "the second write waits for the user");
        fixture.orchestrator.respond_approval(second_card, ApprovalDecision::AllowOnce).await.unwrap();
        second.await.unwrap().unwrap();
        assert_eq!(fixture.store.note_for_thread(thread.id).unwrap().unwrap().body, "Same line\n\nSame line");

        // Two identical calls waiting on one card: the answer covers one of them.
        let both = [
            fixture.spawn_tool(&thread, &live, "b1", "kybern_note_append", json!({"text": "Twin"})),
            fixture.spawn_tool(&thread, &live, "b2", "kybern_note_append", json!({"text": "Twin"})),
        ];
        let shared = fixture.next_card(&live, &[card, second_card]).await;
        // Let both calls reach the card before it is answered.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        fixture.orchestrator.respond_approval(shared, ApprovalDecision::AllowOnce).await.unwrap();
        let again = fixture.next_card(&live, &[card, second_card, shared]).await;
        fixture.orchestrator.respond_approval(again, ApprovalDecision::AllowOnce).await.unwrap();
        for call in both {
            call.await.unwrap().unwrap();
        }
        assert_eq!(fixture.store.note_for_thread(thread.id).unwrap().unwrap().body, "Same line\n\nSame line\n\nTwin\n\nTwin");
        assert_eq!(live.daemon_approvals.lock().await.len(), 4);
    }

    #[tokio::test]
    async fn thread_context_starts_catalog_refresh_without_waiting_for_probes() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let (live, _) = fixture.active_app_tool_session(&thread).await;
        let first = fixture
            .orchestrator
            .execute_native_app_tool_call(thread.id, live.session_instance_id, "context-before-catalog", "kybern_thread_context", json!({}))
            .await
            .unwrap();
        assert_eq!(first["provider_catalog_state"], "loading");
        assert_eq!(first["providers"].as_array().unwrap().len(), ProviderKind::ALL.len());

        let cached = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let value = fixture
                    .orchestrator
                    .execute_native_app_tool_call(
                        thread.id,
                        live.session_instance_id,
                        "context-after-catalog",
                        "kybern_thread_context",
                        json!({}),
                    )
                    .await
                    .unwrap();
                if value["provider_catalog_state"] == "cached" {
                    break value;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(cached["providers"].as_array().unwrap().len(), ProviderKind::ALL.len());
        assert!(cached["providers"].as_array().unwrap().iter().all(|provider| provider["models"].is_array()));
    }

    /// Records the bridge each spawned session is given.
    struct BridgeCapture {
        kind: ProviderKind,
        bridges: Arc<std::sync::Mutex<Vec<Option<kybern_drivers::NativeToolBridge>>>>,
    }

    #[async_trait::async_trait]
    impl kybern_drivers::AgentDriver for BridgeCapture {
        fn kind(&self) -> ProviderKind {
            self.kind
        }

        async fn probe(&self, _binary: Option<&PathBuf>) -> ProviderStatus {
            ProviderStatus {
                kind: self.kind,
                display_name: "Capture".into(),
                available: true,
                binary_path: None,
                version: None,
                unavailable_reason: None,
                supported_permission_modes: PermissionMode::ALL.to_vec(),
                supports_fork: false,
                supports_model_switch: false,
                supports_effort_switch: false,
                supported_efforts: vec![],
                models: vec![],
                instances: vec![],
            }
        }

        async fn spawn(&self, config: kybern_drivers::SessionConfig) -> kybern_drivers::Result<kybern_drivers::SpawnedSession> {
            self.bridges.lock().unwrap().push(config.native_tool_bridge);
            let (_tx, events) = tokio::sync::mpsc::channel(1);
            Ok(kybern_drivers::SpawnedSession { session: Box::new(TestSession::default()), events })
        }
    }

    fn capture_fixture(kind: ProviderKind) -> (Fixture, Arc<std::sync::Mutex<Vec<Option<kybern_drivers::NativeToolBridge>>>>) {
        let bridges = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut drivers = DriverRegistry::default();
        drivers.register(Arc::new(BridgeCapture { kind, bridges: bridges.clone() }));
        let gateway = crate::native_tools_mcp::NativeToolsGateway::default();
        (Fixture::with_drivers_and_gateway(drivers, Some(gateway)), bridges)
    }

    #[tokio::test]
    async fn every_new_session_receives_the_guide_for_its_own_tools_unless_turned_off() {
        let (fixture, bridges) = capture_fixture(ProviderKind::Codex);
        let thread = fixture.thread_with_provider(ThreadStatus::Idle, ProviderKind::Codex);
        let spawn = || async {
            let live = fixture.orchestrator.spawn_session(&thread, None).await.unwrap();
            fixture.orchestrator.revoke_native_session(&live);
            bridges.lock().unwrap().last().cloned().flatten().expect("bridge")
        };

        let first = spawn().await;
        let guide = first.guide().expect("an ordinary thread gets the guide").to_owned();
        assert_eq!(first.provider_instructions().as_deref(), Some(guide.as_str()), "no coordinator role on an ordinary thread");
        assert!(guide.starts_with("# Working in Kybern") && guide.contains("## Notes and tasks"));
        assert!(!guide.contains("The user's Mac"), "Codex has no Kybern computer tools");
        assert_eq!(spawn().await.guide(), Some(guide.as_str()), "respawn and resume keep the prefix byte-identical");

        let mut settings = fixture.orchestrator.inner.settings.get();
        settings.tell_agents_about_kybern = false;
        fixture.orchestrator.inner.settings.set(settings).unwrap();
        let off = spawn().await;
        assert_eq!(off.guide(), None);
        assert_eq!(off.provider_instructions(), None);
        assert!(off.tools().next().is_some(), "turning the guide off leaves the tools alone");
    }

    #[tokio::test]
    async fn preview_open_tool_reports_status_notifies_clients_and_archive_revokes_tickets() {
        let fixture = Fixture::new();
        // The fixture project is the data directory, which previews never serve.
        let work = std::env::temp_dir().join(format!("kybern-preview-tool-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&work).unwrap();
        let work = work.canonicalize().unwrap();
        std::fs::write(work.join("mock.html"), "<p>mock</p>").unwrap();
        let mut thread = fixture.thread_with_provider(ThreadStatus::Idle, ProviderKind::Pi);
        thread.cwd = work.to_string_lossy().into_owned();
        fixture.orchestrator.inner.store.thread_upsert(&thread).unwrap();
        let (live, _) = fixture.active_app_tool_session(&thread).await;
        let call = |id: &'static str, args: serde_json::Value| {
            fixture.orchestrator.execute_native_app_tool_call(thread.id, live.session_instance_id, id, "kybern_preview_open", args)
        };
        // Nobody is listening yet.
        assert_eq!(call("p0", json!({"target": "mock.html"})).await.unwrap()["status"], "no_client");
        let mut clients = fixture.orchestrator.subscribe_preview_requests();
        let shown = call("p1", json!({"target": "mock.html", "title": "Mock"})).await.unwrap();
        assert_eq!(shown["status"], "shown");
        let note = clients.try_recv().unwrap();
        assert_eq!((note.thread_id, note.title.as_deref(), note.requested_by_agent), (thread.id, Some("Mock"), true));
        assert!(note.target.ends_with("mock.html") && std::path::Path::new(&note.target).is_absolute());
        assert_eq!(call("p2", json!({"target": "http://localhost:5173"})).await.unwrap()["status"], "shown");
        assert_eq!(call("p3", json!({"target": "https://example.com/docs"})).await.unwrap()["status"], "opens_in_browser");
        let error = call("p4", json!({"target": "mock.html", "thread_id": Uuid::now_v7()})).await.unwrap_err();
        assert!(error.to_string().contains("bound to the current thread"), "{error}");
        assert!(call("p5", json!({"target": "mock.html", "allow_folder": true})).await.is_err(), "agents cannot grant folders");
        // Archiving a thread revokes its preview tickets.
        let ticket =
            fixture.orchestrator.inner.previews.mint(crate::previews::tickets::TicketKind::Files { root: work.clone() }, thread.id, None);
        assert!(fixture.orchestrator.inner.previews.lookup(&ticket).is_some());
        fixture.orchestrator.archive_thread(thread.id).await.unwrap();
        assert!(fixture.orchestrator.inner.previews.lookup(&ticket).is_none());
        let _ = std::fs::remove_dir_all(&work);
    }

    #[tokio::test]
    async fn app_tool_request_roundtrips_through_the_event_pump() {
        let fixture = Fixture::new();
        std::fs::write(fixture.root.join("bridge.txt"), "through the daemon").unwrap();
        let thread = fixture.thread_with_provider(ThreadStatus::Idle, ProviderKind::Pi);
        let (live, responses) = fixture.active_app_tool_session(&thread).await;
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let pump = tokio::spawn(fixture.orchestrator.clone().pump(thread.id, live, rx));

        tx.send(DriverEvent::AppToolRequest {
            request_id: "request-1".into(),
            name: "kybern_read_file".into(),
            arguments: json!({ "path": "bridge.txt" }),
        })
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !responses.lock().await.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let response = responses.lock().await.pop().unwrap();
        assert_eq!(response.0, "request-1");
        assert_eq!(response.1.unwrap()["content"], "through the daemon");
        drop(tx);
        pump.abort();
    }

    #[tokio::test]
    async fn app_tool_request_from_a_replaced_session_is_rejected() {
        let fixture = Fixture::new();
        let thread = fixture.thread_with_provider(ThreadStatus::Idle, ProviderKind::Pi);
        let (stale, responses) = fixture.active_app_tool_session(&thread).await;
        let (replacement, _) = fixture.park(&thread, Instant::now()).await;
        assert!(!Arc::ptr_eq(&stale, &replacement));

        fixture
            .orchestrator
            .queue_app_tool_request(thread.id, stale, "stale-request".into(), "kybern_thread_context".into(), json!({}))
            .await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !responses.lock().await.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let (_, response) = responses.lock().await.pop().unwrap();
        assert!(response.unwrap_err().contains("no active owning turn"));
    }

    #[tokio::test]
    async fn app_tool_requests_have_a_per_session_concurrency_cap() {
        let fixture = Fixture::new();
        let thread = fixture.thread_with_provider(ThreadStatus::Idle, ProviderKind::Pi);
        let (live, responses) = fixture.active_app_tool_session(&thread).await;
        let mut permits = Vec::new();
        for _ in 0..crate::app_tools::MAX_CONCURRENT_REQUESTS {
            permits.push(live.app_tool_permits.clone().acquire_owned().await.unwrap());
        }

        fixture.orchestrator.queue_app_tool_request(thread.id, live, "over-cap".into(), "kybern_thread_context".into(), json!({})).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !responses.lock().await.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let (_, response) = responses.lock().await.pop().unwrap();
        let error = response.unwrap_err();
        assert!(error.contains("too many concurrent"), "unexpected rejection: {error}");
        drop(permits);
    }

    #[tokio::test]
    async fn provider_exit_reaps_idle_background_work_without_waiting_for_sender_drop() {
        for code in [0, 1] {
            let fixture = Fixture::new();
            let thread = fixture.thread_with_provider(ThreadStatus::Idle, ProviderKind::Omp);
            let (live, _) = fixture.park(&thread, Instant::now()).await;
            fixture.orchestrator.send(thread.id, UserMessage::text("start background work")).await.unwrap();
            live.turn_ready.notified().await;
            let task = generic_runtime_task(&ToolCall {
                id: "bg_5".into(),
                name: "Bash".into(),
                input: json!({"command":"sleep 30", "run_in_background":true}),
                parent_id: None,
            })
            .unwrap();
            fixture.orchestrator.handle_driver_event(thread.id, &live, DriverEvent::RuntimeTaskStarted(task)).await.unwrap();
            // OMP's terminal result can leave provider-owned work alive.
            fixture.orchestrator.handle_driver_event(thread.id, &live, completed_response(StopReason::Completed)).await.unwrap();
            assert!(live.turn.lock().await.is_none());
            assert!(fixture.store.runtime_tasks_for_thread(thread.id).unwrap().iter().any(|task| task.status.is_active()));
            if code != 0 {
                live.tasks.lock().await.clear();
            }

            // Pi/OMP retain the sender in the public session handle even after
            // the reader emits Exited. EOF on this channel will never arrive.
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            tx.send(DriverEvent::Exited { code: Some(code), error: (code != 0).then(|| "process died".into()) }).await.unwrap();
            tokio::time::timeout(Duration::from_secs(1), fixture.orchestrator.clone().pump(thread.id, live, rx))
                .await
                .expect("Exited must retire the session even while its sender is retained");
            assert!(!fixture.has_session(&thread).await);
            let tasks = fixture.store.runtime_tasks_for_thread(thread.id).unwrap();
            assert!(tasks.iter().all(|task| task.status == RuntimeTaskStatus::Interrupted && task.completed_at.is_some()));
            assert_eq!(fixture.store.thread_get(thread.id).unwrap().unwrap().status, ThreadStatus::Idle);
            drop(tx);
        }
    }

    #[tokio::test]
    async fn idle_interrupt_reconciles_unattached_tasks_on_broken_or_stalled_transport() {
        for hangs in [false, true] {
            let fixture = Fixture::new();
            let thread = fixture.thread_with_provider(ThreadStatus::Idle, ProviderKind::Omp);
            let (mut live, closes) = fixture.park(&thread, Instant::now()).await;
            fixture.orchestrator.inner.sessions.lock().await.remove(&thread.id);
            Arc::get_mut(&mut live).unwrap().session =
                Box::new(TestSession { closes: closes.clone(), hang_interrupt: hangs, broken_interrupt: !hangs, ..Default::default() });
            fixture.orchestrator.inner.sessions.lock().await.insert(thread.id, live.clone());
            fixture.orchestrator.send(thread.id, UserMessage::text("background process")).await.unwrap();
            live.turn_ready.notified().await;
            fixture.orchestrator.handle_driver_event(thread.id, &live, completed_response(StopReason::Completed)).await.unwrap();
            let task = generic_runtime_task(&ToolCall {
                id: "bg_5".into(),
                name: "Bash".into(),
                input: json!({"command":"sleep 30", "run_in_background":true}),
                parent_id: None,
            })
            .unwrap();
            fixture.orchestrator.handle_driver_event(thread.id, &live, DriverEvent::RuntimeTaskStarted(task)).await.unwrap();
            live.tasks.lock().await.clear();
            fixture.orchestrator.interrupt_with_grace(thread.id, Duration::from_millis(20)).await.unwrap();
            assert_eq!(closes.load(Ordering::SeqCst), 1);
            assert!(!fixture.has_session(&thread).await);
            assert_eq!(fixture.store.thread_get(thread.id).unwrap().unwrap().status, ThreadStatus::Idle);
            let tasks = fixture.store.runtime_tasks_for_thread(thread.id).unwrap();
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0].status, RuntimeTaskStatus::Interrupted);
            assert!(tasks[0].completed_at.is_some());
            assert!(fixture.orchestrator.inner.releasing.lock().await.is_empty());
        }
    }

    #[tokio::test]
    async fn interrupt_ack_without_a_result_must_settle_the_turn() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let (live, closes) = fixture.park(&thread, Instant::now()).await;
        fixture.orchestrator.send(thread.id, UserMessage::text("wait for task output")).await.unwrap();
        live.turn_ready.notified().await;
        // The provider acknowledges interrupt but never emits a terminal event.
        fixture.orchestrator.interrupt_with_grace(thread.id, Duration::from_millis(50)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.store.thread_get(thread.id).unwrap().unwrap().status != ThreadStatus::Idle {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("an acknowledged stop must not leave the thread stuck running");
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        assert!(live.turn.lock().await.is_none());
        assert!(!fixture.has_session(&thread).await);
        assert!(
            fixture
                .store
                .events_for_thread(thread.id)
                .unwrap()
                .iter()
                .any(|event| { matches!(event.payload, EventPayload::TurnCompleted { stop_reason: StopReason::Interrupted, .. }) })
        );
    }

    #[tokio::test]
    async fn a_turn_ends_before_its_after_checkpoint_and_the_next_snapshot_waits_for_it() {
        let fixture = Fixture::new();
        let git =
            |args: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(&fixture.root).args(args).status().unwrap().success());
        git(&["init", "-q"]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "init"]);
        let thread = fixture.thread(ThreadStatus::Idle);
        let (live, _) = fixture.park(&thread, Instant::now()).await;
        let mut events = fixture.orchestrator.inner.events.subscribe();
        fixture.orchestrator.send(thread.id, UserMessage::text("edit a file")).await.unwrap();
        live.turn_ready.notified().await;
        let turn_id = live.turn.lock().await.as_ref().unwrap().id;
        // The before snapshot is taken once the turn is ready, and announced after it is stored.
        // Wait for that, however long git takes on a loaded runner, not against a short clock.
        while fixture.store.checkpoint_get(turn_id).unwrap().is_none() {
            match tokio::time::timeout(Duration::from_secs(60), events.recv()).await.expect("the before checkpoint is announced") {
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(error) => panic!("thread events ended: {error:?}"),
            }
        }
        std::fs::write(fixture.root.join("edited.txt"), "agent output\n").unwrap();

        // Hold the thread's checkpoint lock, as a slow snapshot of a large tree would.
        let slow = fixture.orchestrator.checkpoint_lock(thread.id).lock_owned().await;
        let ending = {
            let (orchestrator, live) = (fixture.orchestrator.clone(), live.clone());
            tokio::spawn(async move { orchestrator.handle_driver_event(thread.id, &live, completed_response(StopReason::Completed)).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        // The turn has not ended while an earlier snapshot runs; the after
        // snapshot is queued behind it, never skipped.
        assert!(!ending.is_finished());
        drop(slow);
        tokio::time::timeout(Duration::from_secs(2), ending).await.unwrap().unwrap().unwrap();
        let ended = fixture.store.events_for_thread(thread.id).unwrap();
        let completed = ended.iter().find(|event| matches!(event.payload, EventPayload::TurnCompleted { .. })).expect("turn ended").seq;
        assert_eq!(fixture.store.thread_get(thread.id).unwrap().unwrap().status, ThreadStatus::Idle);

        // A revert waits for the after snapshot, which lands after the turn ended.
        fixture.orchestrator.revert(thread.id, turn_id).await.unwrap();
        let events = fixture.store.events_for_thread(thread.id).unwrap();
        let after = events
            .iter()
            .find(|event| matches!(&event.payload, EventPayload::CheckpointUpdated { checkpoint } if checkpoint.after.is_some()))
            .expect("the after checkpoint is recorded");
        assert!(after.seq > completed, "the checkpoint follows the turn's end");
        let checkpoint = fixture.store.checkpoint_get(turn_id).unwrap().unwrap();
        let shown = std::process::Command::new("git")
            .arg("-C")
            .arg(&fixture.root)
            .args(["show", &format!("{}:edited.txt", checkpoint.after.unwrap())])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&shown.stdout), "agent output\n");
        assert!(!fixture.root.join("edited.txt").exists(), "the revert restored the snapshot taken before the turn");
    }

    #[tokio::test]
    async fn review_revert_waiting_on_after_snapshot_races_a_new_send() {
        let fixture = Fixture::new();
        let git =
            |args: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(&fixture.root).args(args).status().unwrap().success());
        git(&["init", "-q"]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "init"]);
        let thread = fixture.thread(ThreadStatus::Idle);
        let (live, _) = fixture.park(&thread, Instant::now()).await;
        let mut events = fixture.orchestrator.inner.events.subscribe();
        fixture.orchestrator.send(thread.id, UserMessage::text("edit a file")).await.unwrap();
        live.turn_ready.notified().await;
        let turn_id = live.turn.lock().await.as_ref().unwrap().id;
        // Wait for the before checkpoint's announcement, not against a short clock.
        while fixture.store.checkpoint_get(turn_id).unwrap().is_none() {
            match tokio::time::timeout(Duration::from_secs(60), events.recv()).await.expect("the before checkpoint is announced") {
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(error) => panic!("thread events ended: {error:?}"),
            }
        }
        fixture.orchestrator.handle_driver_event(thread.id, &live, completed_response(StopReason::Completed)).await.unwrap();
        // The after snapshot of a large tree is still running.
        let slow = fixture.orchestrator.checkpoint_lock(thread.id).lock_owned().await;
        let reverting = {
            let orchestrator = fixture.orchestrator.clone();
            tokio::spawn(async move { orchestrator.revert(thread.id, turn_id).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Thread is Idle, so a new message (user, queue or another thread) starts a turn.
        let next = fixture.orchestrator.send(thread.id, UserMessage::text("next")).await;
        eprintln!("send during revert wait: {:?}", next.as_ref().map(|r| r.0));
        let status_before = fixture.store.thread_get(thread.id).unwrap().unwrap().status;
        drop(slow);
        let reverted = tokio::time::timeout(Duration::from_secs(5), reverting).await.unwrap().unwrap();
        eprintln!("revert result: {:?}", reverted.as_ref().map(|r| r.1));
        let status_after = fixture.store.thread_get(thread.id).unwrap().unwrap().status;
        eprintln!("status while next turn runs: before revert finished {status_before:?}, after {status_after:?}");
        assert!(next.is_err() || reverted.is_err(), "a revert must not proceed under a turn that started while it waited");
        // The send was not held back by the wait, and the failed revert left the new turn's record alone.
        assert!(next.is_ok() && status_after == ThreadStatus::Running, "{status_after:?}");
    }

    #[tokio::test]
    async fn interrupt_without_ack_survives_rpc_cancellation() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let (mut live, closes) = fixture.park(&thread, Instant::now()).await;
        fixture.orchestrator.inner.sessions.lock().await.remove(&thread.id);
        Arc::get_mut(&mut live).unwrap().session =
            Box::new(TestSession { closes: closes.clone(), hang_interrupt: true, ..Default::default() });
        fixture.orchestrator.inner.sessions.lock().await.insert(thread.id, live.clone());
        fixture.orchestrator.send(thread.id, UserMessage::text("stuck provider")).await.unwrap();
        live.turn_ready.notified().await;
        let orchestrator = fixture.orchestrator.clone();
        let request = tokio::spawn(async move { orchestrator.interrupt_with_grace(thread.id, Duration::from_millis(50)).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        request.abort();
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.store.thread_get(thread.id).unwrap().unwrap().status != ThreadStatus::Idle {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        assert!(!fixture.has_session(&thread).await);
    }

    #[tokio::test]
    async fn interrupt_deadline_does_not_close_a_new_continuation() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let (live, closes) = fixture.park(&thread, Instant::now()).await;
        fixture.orchestrator.send(thread.id, UserMessage::text("monitor")).await.unwrap();
        live.turn_ready.notified().await;
        let old_response = live.turn.lock().await.as_ref().unwrap().response_id;
        let orchestrator = fixture.orchestrator.clone();
        let request = tokio::spawn(async move { orchestrator.interrupt_with_grace(thread.id, Duration::from_millis(50)).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        fixture.orchestrator.handle_driver_event(thread.id, &live, completed_response(StopReason::Completed)).await.unwrap();
        fixture.orchestrator.handle_driver_event(thread.id, &live, DriverEvent::ResponseStarted).await.unwrap();
        assert_ne!(live.turn.lock().await.as_ref().unwrap().response_id, old_response);
        request.await.unwrap().unwrap();
        // Also exercise a watchdog already entering its final claim.
        fixture.orchestrator.force_interrupt(thread.id, &live, Some(old_response)).await.unwrap();
        assert_eq!(closes.load(Ordering::SeqCst), 0);
        assert!(fixture.has_session(&thread).await);
        fixture.orchestrator.handle_driver_event(thread.id, &live, completed_response(StopReason::Interrupted)).await.unwrap();
        fixture.orchestrator.shutdown().await;
    }

    #[tokio::test]
    async fn interrupt_settles_a_provisional_result_after_tasks_stop() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let (live, closes) = fixture.park(&thread, Instant::now()).await;
        fixture.orchestrator.send(thread.id, UserMessage::text("wait for background work")).await.unwrap();
        live.turn_ready.notified().await;
        let task = generic_runtime_task(&ToolCall {
            id: "task-1".into(),
            name: "task".into(),
            input: json!({"description":"Explore"}),
            parent_id: None,
        })
        .unwrap();
        fixture.orchestrator.handle_driver_event(thread.id, &live, DriverEvent::RuntimeTaskStarted(task)).await.unwrap();
        fixture
            .orchestrator
            .handle_driver_event(
                thread.id,
                &live,
                DriverEvent::TurnCompleted {
                    stop_reason: StopReason::Completed,
                    usage: Usage { input_tokens: 42, ..Usage::default() },
                    cost_usd: Some(0.2),
                    duration_ms: 20,
                    anchors: TurnAnchors::default(),
                },
            )
            .await
            .unwrap();
        assert!(live.turn.lock().await.as_ref().unwrap().pending_completion.is_some());
        fixture
            .orchestrator
            .handle_driver_event(
                thread.id,
                &live,
                DriverEvent::RuntimeTaskCompleted(DriverRuntimeTaskUpdate::status("tool:task-1", RuntimeTaskStatus::Stopped)),
            )
            .await
            .unwrap();
        assert!(live.tasks.lock().await.values().all(|task| task.status == RuntimeTaskStatus::Stopped));
        assert_eq!(fixture.store.thread_get(thread.id).unwrap().unwrap().status, ThreadStatus::Running);
        fixture.orchestrator.interrupt_with_grace(thread.id, Duration::from_millis(50)).await.unwrap();
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.store.thread_get(thread.id).unwrap().unwrap().status, ThreadStatus::Idle);
        let events = fixture.store.events_for_thread(thread.id).unwrap();
        assert!(events.iter().any(|event| matches!(&event.payload,
            EventPayload::TurnCompleted { stop_reason: StopReason::Interrupted, usage, cost_usd: Some(cost), .. }
            if usage.input_tokens == 42 && (*cost - 0.2).abs() < 1e-10
        )));
        let count = events.len();
        fixture.orchestrator.handle_driver_event(thread.id, &live, DriverEvent::ResponseStarted).await.unwrap();
        fixture.orchestrator.handle_driver_event(thread.id, &live, completed_response(StopReason::Completed)).await.unwrap();
        assert_eq!(fixture.store.events_for_thread(thread.id).unwrap().len(), count, "retired output cannot reopen the turn");
    }

    #[tokio::test]
    async fn explicit_runtime_task_resume_reactivates_a_completed_agent() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let (live, _) = fixture.park(&thread, Instant::now()).await;
        fixture.orchestrator.send(thread.id, UserMessage::text("delegate")).await.unwrap();
        live.turn_ready.notified().await;
        let task = DriverRuntimeTask {
            id: "agent-1".into(),
            kind: RuntimeTaskKind::Agent,
            status: RuntimeTaskStatus::Running,
            title: "Subagent".into(),
            detail: None,
            provider_type: Some("sub_agent".into()),
            parent_id: None,
            tool_call_id: None,
            provider_thread_id: Some("agent-1".into()),
            model: None,
            effort: None,
            backgrounded: true,
            last_tool_name: None,
            usage: None,
            stats: RuntimeTaskStats::default(),
            capabilities: RuntimeTaskCapabilities { stop: true, background: false },
        };
        fixture.orchestrator.handle_driver_event(thread.id, &live, DriverEvent::RuntimeTaskStarted(task)).await.unwrap();
        fixture
            .orchestrator
            .handle_driver_event(
                thread.id,
                &live,
                DriverEvent::RuntimeTaskCompleted(DriverRuntimeTaskUpdate::status("agent-1", RuntimeTaskStatus::Completed)),
            )
            .await
            .unwrap();
        assert_eq!(fixture.store.runtime_tasks_for_thread(thread.id).unwrap()[0].status, RuntimeTaskStatus::Completed);

        fixture
            .orchestrator
            .handle_driver_event(
                thread.id,
                &live,
                DriverEvent::RuntimeTaskResumed(DriverRuntimeTaskUpdate {
                    id: "agent-1".into(),
                    status: Some(RuntimeTaskStatus::Running),
                    detail: None,
                    backgrounded: None,
                    last_tool_name: None,
                    usage: None,
                    stats: None,
                    capabilities: Some(RuntimeTaskCapabilities { stop: true, background: false }),
                }),
            )
            .await
            .unwrap();
        let resumed = fixture.store.runtime_tasks_for_thread(thread.id).unwrap();
        assert_eq!(resumed[0].status, RuntimeTaskStatus::Running);
        assert!(resumed[0].completed_at.is_none());
        assert!(matches!(
            fixture.store.events_for_thread(thread.id).unwrap().last().unwrap().payload,
            EventPayload::RuntimeTaskStarted { .. }
        ));
    }

    #[tokio::test]
    async fn omp_background_job_timeout_after_its_turn_completes_the_task_as_failed() {
        let fixture = Fixture::new();
        let thread = fixture.thread_with_provider(ThreadStatus::Idle, ProviderKind::Omp);
        let (live, _) = fixture.park(&thread, Instant::now()).await;
        fixture.orchestrator.send(thread.id, UserMessage::text("run the slow command")).await.unwrap();
        live.turn_ready.notified().await;
        let task = DriverRuntimeTask {
            id: "tool:call-1".into(),
            kind: RuntimeTaskKind::Process,
            status: RuntimeTaskStatus::Running,
            title: "sleep 30".into(),
            detail: None,
            provider_type: Some("bash".into()),
            parent_id: None,
            tool_call_id: Some("call-1".into()),
            provider_thread_id: Some("bg_2".into()),
            model: None,
            effort: None,
            backgrounded: true,
            last_tool_name: None,
            usage: None,
            stats: RuntimeTaskStats::default(),
            capabilities: RuntimeTaskCapabilities::default(),
        };
        fixture.orchestrator.handle_driver_event(thread.id, &live, DriverEvent::RuntimeTaskStarted(task)).await.unwrap();
        // Background work outlives the turn that started it.
        fixture.orchestrator.handle_driver_event(thread.id, &live, completed_response(StopReason::Completed)).await.unwrap();
        assert!(fixture.store.runtime_tasks_for_thread(thread.id).unwrap()[0].status.is_active());

        fixture
            .orchestrator
            .handle_driver_event(
                thread.id,
                &live,
                DriverEvent::RuntimeTaskCompleted(DriverRuntimeTaskUpdate {
                    detail: Some("Command timed out after 20 seconds".into()),
                    ..DriverRuntimeTaskUpdate::status("tool:call-1", RuntimeTaskStatus::Failed)
                }),
            )
            .await
            .unwrap();
        let tasks = fixture.store.runtime_tasks_for_thread(thread.id).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, RuntimeTaskStatus::Failed);
        assert_eq!(tasks[0].title, "sleep 30");
        assert!(tasks[0].completed_at.is_some());
        assert!(tasks.iter().all(|task| !task.status.is_active()));
        assert!(matches!(
            &fixture.store.events_for_thread(thread.id).unwrap().last().unwrap().payload,
            EventPayload::RuntimeTaskCompleted { task } if task.status == RuntimeTaskStatus::Failed
        ));
    }

    #[tokio::test]
    async fn interrupt_fallback_resolves_approvals_and_active_tasks() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let (live, _) = fixture.park(&thread, Instant::now()).await;
        fixture.orchestrator.send(thread.id, UserMessage::text("background work awaiting permission")).await.unwrap();
        live.turn_ready.notified().await;
        let task = generic_runtime_task(&ToolCall {
            id: "task-1".into(),
            name: "task".into(),
            input: json!({"description":"Explore"}),
            parent_id: None,
        })
        .unwrap();
        fixture.orchestrator.handle_driver_event(thread.id, &live, DriverEvent::RuntimeTaskStarted(task)).await.unwrap();
        fixture
            .orchestrator
            .handle_driver_event(
                thread.id,
                &live,
                DriverEvent::PermissionRequest {
                    request_id: "permission-1".into(),
                    tool_call_id: Some("tool-1".into()),
                    tool_name: "Bash".into(),
                    input: json!({"command":"sleep 30"}),
                    summary: "Allow this command".into(),
                    suggestions: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(fixture.store.thread_get(thread.id).unwrap().unwrap().status, ThreadStatus::AwaitingApproval);
        let approval = *live.pending.lock().await.keys().next().unwrap();
        fixture.orchestrator.interrupt_with_grace(thread.id, Duration::from_millis(50)).await.unwrap();
        assert!(live.pending.lock().await.is_empty());
        assert!(fixture.store.approval_get(approval).unwrap().unwrap().1);
        assert!(live.tasks.lock().await.values().all(|task| task.status == RuntimeTaskStatus::Interrupted));
        assert_eq!(fixture.store.thread_get(thread.id).unwrap().unwrap().status, ThreadStatus::Idle);
        // The old pump must not report this deliberately closed turn as failed.
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(tx);
        fixture.orchestrator.clone().pump(thread.id, live, rx).await;
        assert!(
            !fixture
                .store
                .events_for_thread(thread.id)
                .unwrap()
                .iter()
                .any(|event| matches!(event.payload, EventPayload::TurnFailed { .. }))
        );
    }

    #[tokio::test]
    async fn steering_retries_preserve_the_running_turn_and_deliver_once() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let (live, _, messages) = fixture.park_recording(&thread, Instant::now()).await;
        let (turn_id, _) = fixture.orchestrator.send(thread.id, UserMessage::text("initial request")).await.unwrap();
        live.turn_ready.notified().await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while messages.lock().await.is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        let prompt = methods::QueuedMessage { id: Uuid::now_v7(), thread_id: thread.id, message: UserMessage::text("Steer this turn") };
        let (first, retry) = tokio::join!(fixture.orchestrator.steer(prompt.clone()), fixture.orchestrator.steer(prompt.clone()));
        assert_eq!(first.unwrap().turn_id, turn_id);
        assert_eq!(retry.unwrap().message_id, prompt.id);
        assert_eq!(messages.lock().await.len(), 2);
        let events = fixture.store.events_for_thread(thread.id).unwrap();
        assert_eq!(events.iter().filter(|ev| matches!(ev.payload, EventPayload::TurnStarted { .. })).count(), 1);
        assert_eq!(events.iter().filter(|ev| matches!(ev.payload, EventPayload::MessageSteered { .. })).count(), 1);
        let transcript = kybern_store::project_transcript(&events);
        assert_eq!(transcript.iter().filter(|entry| matches!(entry, TranscriptEntry::User { .. })).count(), 2);
        fixture.orchestrator.handle_driver_event(thread.id, &live, completed_response(StopReason::Completed)).await.unwrap();
        assert_eq!(fixture.orchestrator.steer(prompt.clone()).await.unwrap().turn_id, turn_id, "retry after completion is acknowledged");
        assert!(fixture.orchestrator.steer(methods::QueuedMessage { message: UserMessage::text("different"), ..prompt }).await.is_err());
        assert!(
            fixture
                .orchestrator
                .steer(methods::QueuedMessage { id: Uuid::now_v7(), thread_id: thread.id, message: UserMessage::text("late") })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn queued_edits_keep_order_and_dispatch_the_latest_saved_prompt() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let (live, _, messages) = fixture.park_recording(&thread, Instant::now()).await;
        let first = methods::QueuedMessage { id: Uuid::now_v7(), thread_id: thread.id, message: UserMessage::text("old prompt") };
        let second = methods::QueuedMessage { id: Uuid::now_v7(), message: UserMessage::text("second"), ..first.clone() };
        fixture.orchestrator.enqueue(first.clone()).unwrap();
        fixture.orchestrator.enqueue(second.clone()).unwrap();
        let edited = methods::QueuedMessage { message: UserMessage::text("edited prompt"), ..first.clone() };
        fixture.orchestrator.update_queued(edited.clone()).unwrap();
        let queue = fixture.store.queue_list(Some(thread.id)).unwrap();
        assert_eq!(queue.iter().map(|item| item.id).collect::<Vec<_>>(), vec![first.id, second.id]);
        // Model a worker that selected the item immediately before it was edited.
        fixture.orchestrator.send_with_id(thread.id, first.id, first.message, true, false).await.unwrap();
        live.turn_ready.notified().await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while messages.lock().await.is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(messages.lock().await[0].plain_text(), "edited prompt");
        assert!(fixture.store.events_for_thread(thread.id).unwrap().iter().any(|event| {
            matches!(&event.payload, EventPayload::TurnStarted { message, .. } if message.plain_text() == "edited prompt")
        }));
        assert!(fixture.orchestrator.update_queued(edited).is_err());
        assert_eq!(fixture.store.queue_list(Some(thread.id)).unwrap()[0].id, second.id);
    }

    #[tokio::test]
    async fn notes_are_per_thread_and_conflicting_saves_preserve_the_saved_text() {
        let fixture = Fixture::new();
        let a = fixture.thread(ThreadStatus::Running);
        let b = fixture.thread(ThreadStatus::Idle);
        let params =
            methods::ThreadNotesSetParams { thread_id: a.id, text: "Remember to test mobile\n☑ Notes".into(), expected_revision: 0 };
        let saved = fixture.orchestrator.set_notes(params.clone()).unwrap();
        assert_eq!(saved.revision, 1);
        assert_eq!(fixture.orchestrator.set_notes(params.clone()).unwrap(), saved, "retry is idempotent");
        assert!(fixture.orchestrator.set_notes(methods::ThreadNotesSetParams { text: "stale edit".into(), ..params }).is_err());
        assert_eq!(fixture.store.thread_notes(a.id).unwrap(), saved);
        assert_eq!(fixture.store.thread_notes(b.id).unwrap(), methods::ThreadNotes::default());
        let events = fixture.store.events_for_thread(a.id).unwrap();
        assert!(events.is_empty(), "notes are stored in the notes table, not as thread events");
        assert!(kybern_store::project_transcript(&events).is_empty(), "notes are not prompts");
        let cleared = fixture
            .orchestrator
            .set_notes(methods::ThreadNotesSetParams { thread_id: a.id, text: String::new(), expected_revision: 1 })
            .unwrap();
        assert_eq!(cleared.text, "");
        assert_eq!(cleared.revision, 2);
        assert!(
            fixture
                .orchestrator
                .set_notes(methods::ThreadNotesSetParams { thread_id: a.id, text: "x".repeat(512 * 1024 + 1), expected_revision: 2 })
                .is_err()
        );
    }

    #[tokio::test]
    async fn legacy_notepad_shims_the_thread_note_and_clearing_a_missing_note_saves_nothing() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let cleared = fixture
            .orchestrator
            .set_notes(methods::ThreadNotesSetParams { thread_id: thread.id, text: String::new(), expected_revision: 0 })
            .unwrap();
        assert_eq!(cleared, methods::ThreadNotes::default());
        assert!(fixture.store.note_for_thread(thread.id).unwrap().is_none());

        let saved = fixture
            .orchestrator
            .set_notes(methods::ThreadNotesSetParams { thread_id: thread.id, text: "legacy text".into(), expected_revision: 0 })
            .unwrap();
        let note = fixture.store.note_for_thread(thread.id).unwrap().unwrap();
        assert_eq!(
            (note.body.as_str(), note.summary.revision, note.summary.scope),
            ("legacy text", saved.revision, methods::NoteScope::Thread)
        );
        assert_eq!(note.summary.title, thread.title);
        assert!(
            fixture
                .orchestrator
                .set_notes(methods::ThreadNotesSetParams { thread_id: Uuid::now_v7(), text: "x".into(), expected_revision: 0 })
                .is_err()
        );
    }

    #[tokio::test]
    async fn note_changes_are_published_and_unchanged_saves_stay_quiet() {
        let fixture = Fixture::new();
        let mut changes = fixture.orchestrator.subscribe_notes();
        let note = fixture
            .orchestrator
            .note_create(methods::NotesCreateParams {
                scope: methods::NoteScope::Project,
                project_id: Some(fixture.project.id),
                title: Some("Plan".into()),
                body: Some("- [ ] ship".into()),
                project_role: None,
                category: None,
            })
            .unwrap();
        assert_eq!(changes.try_recv().unwrap().note.unwrap().id, note.summary.id);

        let update = |body: &str, expected_revision| methods::NotesUpdateParams {
            id: Some(note.summary.id),
            thread_id: None,
            expected_revision,
            title: None,
            body: Some(body.into()),
            category: None,
            clear_category: false,
        };
        fixture.orchestrator.note_update(update("- [ ] ship", 1)).unwrap();
        assert!(changes.try_recv().is_err(), "nothing changed, nothing published");
        let edited = fixture.orchestrator.note_update(update("- [x] ship", 1)).unwrap();
        assert_eq!(changes.try_recv().unwrap().note.unwrap().checklist, methods::NoteChecklist { done: 1, total: 1 });
        assert!(fixture.orchestrator.note_update(update("stale", 1)).is_err());
        assert!(changes.try_recv().is_err(), "a rejected save is not published");

        let both = methods::NotesUpdateParams { thread_id: Some(Uuid::now_v7()), ..update("x", edited.summary.revision) };
        assert!(fixture.orchestrator.note_update(both).is_err(), "exactly one of id and thread_id");
        fixture.orchestrator.note_delete(note.summary.id).unwrap();
        assert!(changes.try_recv().unwrap().note.unwrap().deleted_at.is_some());
        fixture.orchestrator.note_purge(note.summary.id).unwrap();
        assert_eq!(changes.try_recv().unwrap().purged_id, Some(note.summary.id));
    }

    #[tokio::test]
    async fn removing_a_project_moves_its_notes_to_recently_deleted_and_publishes_them() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let page = fixture
            .orchestrator
            .note_create(methods::NotesCreateParams {
                scope: methods::NoteScope::Project,
                project_id: Some(fixture.project.id),
                title: Some("Plan".into()),
                body: None,
                project_role: None,
                category: None,
            })
            .unwrap();
        fixture
            .orchestrator
            .set_notes(methods::ThreadNotesSetParams { thread_id: thread.id, text: "thread note".into(), expected_revision: 0 })
            .unwrap();
        let mut changes = fixture.orchestrator.subscribe_notes();
        fixture.orchestrator.remove_project(fixture.project.id).unwrap();
        let mut deleted = [changes.try_recv().unwrap().note.unwrap(), changes.try_recv().unwrap().note.unwrap()];
        deleted.sort_by_key(|note| note.scope as u8);
        assert!(deleted.iter().all(|note| note.deleted_at.is_some()));
        assert_eq!(deleted[0].origin.as_deref(), Some("fixture"));
        assert_eq!(deleted[1].origin.as_deref(), Some(format!("fixture › {}", thread.title).as_str()));
        assert!(fixture.store.project_get(fixture.project.id).unwrap().is_none());
        assert!(fixture.store.note_get(page.summary.id).unwrap().is_some(), "the note outlives its project");
        assert!(fixture.orchestrator.remove_project(FREE_CHAT_PROJECT_ID).is_err());
    }

    #[tokio::test]
    async fn user_startup_wins_over_a_late_continuation_without_orphaning_output() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let (live, _) = fixture.park(&thread, Instant::now()).await;
        let (first, _) = fixture.orchestrator.send(thread.id, UserMessage::text("first")).await.unwrap();
        live.turn_ready.notified().await;
        fixture.orchestrator.handle_driver_event(thread.id, &live, completed_response(StopReason::Completed)).await.unwrap();
        assert_eq!(live.continuation.lock().await.as_ref().unwrap().id, first);

        // Hold only session lookup: accepting user input is synchronous, but
        // start_turn cannot yet install the newly reserved turn.
        let sessions = fixture.orchestrator.inner.sessions.lock().await;
        let (second, _) = fixture.orchestrator.send(thread.id, UserMessage::text("second")).await.unwrap();
        let orchestrator = fixture.orchestrator.clone();
        let response_live = live.clone();
        let response = tokio::spawn(async move {
            orchestrator
                .handle_driver_event(
                    thread.id,
                    &response_live,
                    DriverEvent::TextDelta {
                        message_id: "late-response".into(),
                        origin: EventOrigin::Root,
                        delta: "Reading results".into(),
                    },
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!response.is_finished());
        drop(sessions);
        tokio::time::timeout(Duration::from_secs(2), response).await.unwrap().unwrap().unwrap();
        let events = fixture.store.events_for_thread(thread.id).unwrap();
        assert!(!events.iter().any(|event| matches!(event.payload, EventPayload::TurnResumed)));
        assert!(
            events.iter().any(|event| event.turn_id == Some(second) && matches!(event.payload, EventPayload::AssistantTextDelta { .. }))
        );
        assert_eq!(live.turn.lock().await.as_ref().unwrap().id, second);

        // A native automatic result before the queued user is consumed only
        // closes the root message boundary; it does not finish the user turn.
        fixture.orchestrator.handle_driver_event(thread.id, &live, DriverEvent::ResponseBoundary).await.unwrap();
        assert_eq!(live.turn.lock().await.as_ref().unwrap().id, second);
        assert!(live.turn.lock().await.as_ref().unwrap().active_messages.is_empty());
        fixture.orchestrator.handle_driver_event(thread.id, &live, completed_response(StopReason::Completed)).await.unwrap();
        assert_eq!(live.continuation.lock().await.as_ref().unwrap().id, second);
        fixture.orchestrator.shutdown().await;
    }

    #[tokio::test]
    async fn repeated_continuations_settle_and_interrupt_without_phantom_restarts() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let (live, _) = fixture.park(&thread, Instant::now()).await;
        let (turn_id, _) = fixture.orchestrator.send(thread.id, UserMessage::text("monitor")).await.unwrap();
        live.turn_ready.notified().await;
        fixture.orchestrator.handle_driver_event(thread.id, &live, completed_response(StopReason::Completed)).await.unwrap();
        for _ in 0..2 {
            fixture.orchestrator.handle_driver_event(thread.id, &live, DriverEvent::ResponseStarted).await.unwrap();
            assert_eq!(live.turn.lock().await.as_ref().unwrap().id, turn_id);
            fixture.orchestrator.handle_driver_event(thread.id, &live, completed_response(StopReason::Completed)).await.unwrap();
        }
        fixture.orchestrator.handle_driver_event(thread.id, &live, DriverEvent::ResponseStarted).await.unwrap();
        fixture.orchestrator.handle_driver_event(thread.id, &live, completed_response(StopReason::Interrupted)).await.unwrap();
        assert!(live.turn.lock().await.is_none());
        assert!(live.continuation.lock().await.is_none());
        fixture.orchestrator.handle_driver_event(thread.id, &live, DriverEvent::ResponseStarted).await.unwrap();
        assert!(live.turn.lock().await.is_none());
        let entries = kybern_store::project_transcript(&fixture.store.events_for_thread(thread.id).unwrap());
        assert_eq!(entries.iter().filter(|entry| matches!(entry, TranscriptEntry::TurnSummary { .. })).count(), 1);
        assert!(entries.iter().any(|entry| matches!(entry, TranscriptEntry::TurnSummary { stop_reason: StopReason::Interrupted, .. })));
        fixture.orchestrator.shutdown().await;
    }

    fn policy(session_idle_minutes: u32, max_idle_sessions: u32) -> BackgroundSettings {
        BackgroundSettings { session_idle_minutes, max_idle_sessions, ..BackgroundSettings::default() }
    }

    const MINUTE: Duration = Duration::from_secs(60);

    #[tokio::test]
    async fn every_provider_turn_receives_only_the_original_user_message() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let (live, _, captured) = fixture.park_recording(&thread, Instant::now()).await;
        let messages = [
            UserMessage::text("hi"),
            UserMessage::text("hi 3"),
            UserMessage {
                parts: vec![
                    ContentPart::Text { text: "Review this screenshot".into() },
                    ContentPart::Image { media_type: "image/png".into(), data: "test".into() },
                ],
            },
        ];
        for (index, message) in messages.iter().enumerate() {
            fixture.orchestrator.send(thread.id, message.clone()).await.unwrap();
            let delivered = tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if let Some(message) = captured.lock().await.get(index).cloned() {
                        break message;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(serde_json::to_value(delivered).unwrap(), serde_json::to_value(message).unwrap());
            fixture.orchestrator.handle_driver_event(thread.id, &live, completed_response(StopReason::Completed)).await.unwrap();
        }
        let events = fixture.store.events_for_thread(thread.id).unwrap();
        let stored: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.payload {
                EventPayload::TurnStarted { message, .. } => Some(message),
                _ => None,
            })
            .collect();
        assert_eq!(serde_json::to_value(stored).unwrap(), serde_json::to_value(messages).unwrap());
        fixture.orchestrator.shutdown().await;
    }

    #[tokio::test]
    async fn async_answers_are_explicit_durable_and_do_not_replace_the_active_turn() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Running);
        let (live, _) = fixture.park(&thread, Instant::now()).await;
        let turn_id = Uuid::now_v7();
        *live.turn.lock().await = Some(ActiveTurn {
            response_id: Uuid::now_v7(),
            id: turn_id,
            started: Instant::now(),
            startup_started: Instant::now(),
            provider: ProviderKind::Codex,
            session_reused: false,
            first_event_observed: false,
            messages: HashMap::new(),
            active_messages: HashMap::new(),
            terminal_message_id: None,
            completed: false,
            pending_completion: None,
            internal: None,
            rotation_seed: None,
        });
        fixture
            .orchestrator
            .emit(
                thread.id,
                Some(turn_id),
                EventPayload::TurnStarted { message_id: Uuid::now_v7(), message: UserMessage::text("original prompt") },
            )
            .unwrap();
        let request = AsyncQuestionRequest {
            id: "question".into(),
            questions: vec![AsyncQuestion { title: "Window?".into(), options: vec!["Yes".into(), "No".into()] }],
        };
        fixture.orchestrator.handle_driver_event(thread.id, &live, DriverEvent::AsyncQuestions(request.clone())).await.unwrap();
        fixture.orchestrator.handle_driver_event(thread.id, &live, DriverEvent::AsyncQuestions(request)).await.unwrap();
        assert_eq!(fixture.store.thread_get(thread.id).unwrap().unwrap().status, ThreadStatus::Running);
        assert!(fixture.store.approvals_pending(Some(thread.id)).unwrap().is_empty());
        let mut params = methods::ThreadsAnswerParams { thread_id: thread.id, request_id: "question".into(), answers: vec![] };
        assert!(fixture.orchestrator.answer_questions(params.clone()).await.unwrap_err().to_string().contains("each question"));
        assert_eq!(kybern_store::project_pending_questions(&fixture.store.events_for_thread(thread.id).unwrap()).len(), 1);
        params.answers = vec!["No, keep this window".into()];
        fixture.orchestrator.answer_questions(params.clone()).await.unwrap();
        fixture.orchestrator.answer_questions(params.clone()).await.unwrap();
        params.answers = vec!["Yes".into()];
        assert!(fixture.orchestrator.answer_questions(params).await.unwrap_err().to_string().contains("already been answered"));
        let events = fixture.store.events_for_thread(thread.id).unwrap();
        assert_eq!(events.iter().filter(|e| matches!(e.payload, EventPayload::AsyncQuestionsAnswered { .. })).count(), 1);
        assert_eq!(events.iter().filter(|e| matches!(e.payload, EventPayload::TurnStarted { .. })).count(), 1);
        assert!(kybern_store::project_pending_questions(&events).is_empty());
        assert_eq!(live.turn.lock().await.as_ref().unwrap().id, turn_id);
        assert_eq!(
            kybern_store::project_transcript(&events).iter().filter(|entry| matches!(entry, TranscriptEntry::User { .. })).count(),
            2
        );
    }

    #[tokio::test]
    async fn async_answer_during_startup_stays_pending_and_does_not_start_another_turn() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Running);
        let (live, _) = fixture.park(&thread, Instant::now()).await;
        fixture
            .orchestrator
            .handle_driver_event(
                thread.id,
                &live,
                DriverEvent::AsyncQuestions(AsyncQuestionRequest {
                    id: "question".into(),
                    questions: vec![AsyncQuestion { title: "Continue?".into(), options: vec![] }],
                }),
            )
            .await
            .unwrap();
        let result = fixture
            .orchestrator
            .answer_questions(methods::ThreadsAnswerParams {
                thread_id: thread.id,
                request_id: "question".into(),
                answers: vec!["Yes".into()],
            })
            .await;
        assert!(result.unwrap_err().to_string().contains("Submit your answer again"));
        let events = fixture.store.events_for_thread(thread.id).unwrap();
        assert_eq!(kybern_store::project_pending_questions(&events).len(), 1);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.payload, EventPayload::AsyncQuestionsAnswered { .. } | EventPayload::TurnStarted { .. }))
        );
    }

    #[tokio::test]
    async fn async_question_survives_completion_and_idle_answer_starts_one_turn() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let (live, _) = fixture.park(&thread, Instant::now()).await;
        let request = AsyncQuestionRequest {
            id: "question".into(),
            questions: vec![AsyncQuestion { title: "Constraints?".into(), options: vec![] }],
        };
        fixture.orchestrator.handle_driver_event(thread.id, &live, DriverEvent::AsyncQuestions(request)).await.unwrap();
        let params =
            methods::ThreadsAnswerParams { thread_id: thread.id, request_id: "question".into(), answers: vec!["Use scratch data".into()] };
        fixture.orchestrator.answer_questions(params.clone()).await.unwrap();
        fixture.orchestrator.answer_questions(params).await.unwrap();
        let events = fixture.store.events_for_thread(thread.id).unwrap();
        assert!(kybern_store::project_pending_questions(&events).is_empty());
        assert_eq!(events.iter().filter(|e| matches!(e.payload, EventPayload::TurnStarted { .. })).count(), 1);
        assert_eq!(
            kybern_store::project_transcript(&events).iter().filter(|entry| matches!(entry, TranscriptEntry::User { .. })).count(),
            1
        );
    }

    #[tokio::test]
    async fn compaction_rejects_busy_unsupported_and_empty_conversations_without_writing_a_turn() {
        let fixture = Fixture::new();
        let mut thread = fixture.thread(ThreadStatus::Running);
        assert!(fixture.orchestrator.send(thread.id, UserMessage::text("/compact")).await.unwrap_err().to_string().contains("busy"));
        thread.status = ThreadStatus::Idle;
        fixture.store.thread_upsert(&thread).unwrap();
        assert!(
            fixture.orchestrator.send(thread.id, UserMessage::text("/compact")).await.unwrap_err().to_string().contains("does not expose")
        );
        thread.id = Uuid::now_v7();
        thread.provider = ProviderInstance::default_for(ProviderKind::Codex);
        thread.provider_session_id = None;
        fixture.store.thread_upsert(&thread).unwrap();
        assert!(
            fixture.orchestrator.send(thread.id, UserMessage::text("/compact")).await.unwrap_err().to_string().contains("Send a message")
        );
        assert!(fixture.store.events_for_thread(thread.id).unwrap().is_empty());
    }

    #[tokio::test]
    async fn manual_release_preserves_history_and_refuses_active_sessions() {
        let fixture = Fixture::new();
        let mut thread = fixture.thread(ThreadStatus::Running);
        let (_, closes) = fixture.park(&thread, Instant::now()).await;
        assert!(fixture.orchestrator.release_session(thread.id).await.is_err());
        assert_eq!(closes.load(Ordering::SeqCst), 0);
        thread.status = ThreadStatus::Idle;
        fixture.store.thread_upsert(&thread).unwrap();
        fixture.orchestrator.release_session(thread.id).await.unwrap();
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.store.thread_get(thread.id).unwrap().unwrap().provider_session_id, thread.provider_session_id);
        assert!(!fixture.has_session(&thread).await);
        assert!(fixture.orchestrator.release_session(thread.id).await.unwrap_err().to_string().contains("no live agent"));
    }

    #[tokio::test]
    async fn resume_waits_for_the_previous_writer_to_close() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let (done, waiting) = tokio::sync::watch::channel(());
        fixture.orchestrator.inner.releasing.lock().await.insert(thread.id, waiting);
        let orchestrator = fixture.orchestrator.clone();
        let resume = tokio::spawn(async move { orchestrator.ensure_session(&thread).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!resume.is_finished(), "must not attempt a second writer during release");
        drop(done);
        let result = tokio::time::timeout(Duration::from_secs(1), resume).await.unwrap().unwrap();
        // This fixture has no drivers: getting this error proves spawn only ran
        // after the old writer's close barrier completed.
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn idle_sessions_are_released_after_the_policy_window() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let started = Instant::now();
        let (_, closes) = fixture.park(&thread, started).await;

        let early =
            fixture.orchestrator.release_idle_sessions_at(&policy(10, 0), false, started + 9 * MINUTE, SystemTime::now()).await.unwrap();
        assert!(early.is_empty());
        assert!(fixture.has_session(&thread).await);

        let released =
            fixture.orchestrator.release_idle_sessions_at(&policy(10, 0), false, started + 10 * MINUTE, SystemTime::now()).await.unwrap();
        assert_eq!(released, vec![super::SessionRelease { thread_id: thread.id, reason: SessionReleaseReason::Idle }]);
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        assert!(!fixture.has_session(&thread).await);
        assert_eq!(fixture.release_events(&thread), vec![SessionReleaseReason::Idle]);
        // The thread itself is untouched: it resumes from the provider session on the next send.
        let stored = fixture.store.thread_get(thread.id).unwrap().unwrap();
        assert_eq!(stored.status, ThreadStatus::Idle);
        assert_eq!(stored.provider_session_id.as_deref(), Some("provider-session"));
    }

    #[tokio::test]
    async fn on_battery_every_parked_session_goes_after_a_short_grace() {
        let fixture = Fixture::new();
        let fresh = fixture.thread(ThreadStatus::Idle);
        let older = fixture.thread(ThreadStatus::Idle);
        let started = Instant::now();
        fixture.park(&fresh, started).await;
        let (_, closes) = fixture.park(&older, started - BackgroundSettings::BATTERY_SESSION_IDLE).await;

        // The generous policy would keep both; on battery the older one goes now.
        let released = fixture.orchestrator.release_idle_sessions_at(&policy(30, 4), true, started, SystemTime::now()).await.unwrap();
        assert_eq!(released, vec![super::SessionRelease { thread_id: older.id, reason: SessionReleaseReason::Power }]);
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        assert!(fixture.has_session(&fresh).await, "a session inside the grace period is kept");
        assert_eq!(fixture.release_events(&older), vec![SessionReleaseReason::Power]);

        let released = fixture
            .orchestrator
            .release_idle_sessions_at(&policy(30, 4), true, started + BackgroundSettings::BATTERY_SESSION_IDLE, SystemTime::now())
            .await
            .unwrap();
        assert_eq!(released.len(), 1);
        assert!(!fixture.has_session(&fresh).await);
    }

    #[tokio::test]
    async fn sleeping_past_the_idle_deadline_releases_sessions_below_the_warm_cap() {
        let fixture = Fixture::new();
        let first = fixture.thread(ThreadStatus::Idle);
        let second = fixture.thread(ThreadStatus::Idle);
        let started = Instant::now();
        let (_, first_closes) = fixture.park(&first, started).await;
        let (_, second_closes) = fixture.park(&second, started).await;

        let before_deadline = SystemTime::now() + 9 * MINUTE;
        assert!(fixture.orchestrator.release_idle_sessions_at(&policy(10, 4), false, started, before_deadline).await.unwrap().is_empty());

        // macOS can suspend the monotonic clock while wall time advances.
        // Neither agent has accumulated ten minutes of awake time.
        let after_sleep = SystemTime::now() + 60 * MINUTE;
        let released = fixture.orchestrator.release_idle_sessions_at(&policy(10, 4), false, started + MINUTE, after_sleep).await.unwrap();
        assert_eq!(released.len(), 2, "the first sweep after an hour asleep must close both idle processes");
        assert_eq!(first_closes.load(Ordering::SeqCst), 1);
        assert_eq!(second_closes.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.release_events(&first), vec![SessionReleaseReason::Idle]);
        assert_eq!(fixture.release_events(&second), vec![SessionReleaseReason::Idle]);
    }

    #[tokio::test]
    async fn reopening_and_subscribing_do_not_extend_an_idle_session() {
        use axum::{Router, routing::get};
        use kybern_client::{Client, Endpoint};
        use kybern_protocol::methods::*;

        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let started = Instant::now() - 11 * MINUTE;
        let (live, closes) = fixture.park(&thread, started).await;
        let last_activity = live.last_activity();
        let paths = Paths::resolve(Some(fixture.root.clone())).unwrap();
        let mut state = crate::state::AppState::initialize(&paths).unwrap();
        let inner = Arc::get_mut(&mut state.inner).unwrap();
        inner.store = fixture.store.clone();
        inner.orchestrator = fixture.orchestrator.clone();
        // Authentication uses the same in-memory store as the fixture.
        let bootstrap = crate::auth::ensure_bootstrap(&inner.store, &paths.token_file).unwrap();
        inner.bootstrap_token = bootstrap.token;
        inner.bootstrap_token_id = bootstrap.id;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = Endpoint { url: format!("ws://{}/ws", listener.local_addr().unwrap()), token: state.bootstrap_token.clone() };
        let app = Router::new().route("/ws", get(crate::ws::upgrade)).with_state(state);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = Client::connect(&endpoint).await.unwrap();
        client.call::<ThreadsGet>(ThreadsGetParams { thread_id: thread.id, ..Default::default() }).await.unwrap();
        client
            .call::<EventsSubscribe>(EventsSubscribeParams { thread_id: Some(thread.id), after_seq: None, include_tool_output: None })
            .await
            .unwrap();
        client.call::<DaemonActivityMethod>(Empty {}).await.unwrap();
        server.abort();

        assert_eq!(live.last_activity(), last_activity, "reading or reconnecting must not refresh the process lifetime");
        let released = fixture.orchestrator.release_idle_sessions(&policy(10, 4), false).await.unwrap();
        assert_eq!(released.len(), 1);
        assert_eq!(closes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn battery_idle_grace_counts_sleep_time() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let started = Instant::now();
        let (live, closes) = fixture.park(&thread, started).await;
        let wall = live.last_activity().wall;
        let before = fixture
            .orchestrator
            .release_idle_sessions_at(&policy(30, 4), true, started, wall + MINUTE - Duration::from_secs(1))
            .await
            .unwrap();
        assert!(before.is_empty());
        let released = fixture.orchestrator.release_idle_sessions_at(&policy(30, 4), true, started, wall + MINUTE).await.unwrap();
        assert_eq!(released, vec![super::SessionRelease { thread_id: thread.id, reason: SessionReleaseReason::Power }]);
        assert_eq!(closes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn backward_wall_clock_changes_do_not_extend_the_awake_idle_limit() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let started = Instant::now();
        let (live, closes) = fixture.park(&thread, started).await;
        let wall = live.last_activity().wall - 60 * MINUTE;
        let early = fixture.orchestrator.release_idle_sessions_at(&policy(10, 4), false, started + 9 * MINUTE, wall).await.unwrap();
        assert!(early.is_empty());
        let released = fixture.orchestrator.release_idle_sessions_at(&policy(10, 4), false, started + 10 * MINUTE, wall).await.unwrap();
        assert_eq!(released.len(), 1);
        assert_eq!(closes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn activity_during_idle_candidate_selection_keeps_the_session() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let started = Instant::now() - 11 * MINUTE;
        let (live, closes) = fixture.park(&thread, started).await;
        let policy = policy(10, 4);
        let turn = live.turn.lock().await;
        let mut sweep = std::pin::pin!(fixture.orchestrator.release_idle_sessions(&policy, false));
        // Stop after snapshotting the map, before checking whether it is parked.
        assert!(futures::poll!(&mut sweep).is_pending());
        let sessions = fixture.orchestrator.inner.sessions.lock().await;
        drop(turn);
        // Finish candidate selection, but block the claim of that candidate.
        assert!(futures::poll!(&mut sweep).is_pending());
        live.touch();
        drop(sessions);
        assert!(sweep.await.unwrap().is_empty());
        assert_eq!(closes.load(Ordering::SeqCst), 0);
        assert!(fixture.has_session(&thread).await);
    }

    #[tokio::test]
    async fn an_agent_that_fails_because_the_daemon_is_stopping_ends_its_turn_as_interrupted() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let (live, _) = fixture.park(&thread, Instant::now()).await;
        fixture.orchestrator.send(thread.id, UserMessage::text("long work")).await.unwrap();
        live.turn_ready.notified().await;
        fixture.orchestrator.begin_shutdown();
        let raw = "[ede_diagnostic] result_type=user stop_reason=tool_use".to_string();
        fixture.orchestrator.handle_driver_event(thread.id, &live, DriverEvent::TurnFailed { error: raw }).await.unwrap();
        let events = fixture.store.events_for_thread(thread.id).unwrap();
        assert!(!events.iter().any(|event| matches!(event.payload, EventPayload::TurnFailed { .. })), "no raw failure");
        assert!(
            events.iter().any(|event| matches!(event.payload, EventPayload::TurnCompleted { stop_reason: StopReason::Interrupted, .. }))
        );
        assert!(events.iter().any(|event| matches!(&event.payload, EventPayload::ProviderNotice { text, .. } if text.contains("stopped, so this turn was interrupted"))));
        assert_eq!(fixture.store.thread_get(thread.id).unwrap().unwrap().status, ThreadStatus::Idle);
    }

    #[tokio::test]
    async fn failed_threads_release_like_idle_ones() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Failed);
        let started = Instant::now();
        let (_, closes) = fixture.park(&thread, started).await;
        let released =
            fixture.orchestrator.release_idle_sessions_at(&policy(1, 0), false, started + MINUTE, SystemTime::now()).await.unwrap();
        assert_eq!(released.len(), 1);
        assert_eq!(closes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn sessions_with_work_in_flight_are_never_released() {
        let fixture = Fixture::new();
        let started = Instant::now();
        let later = started + 60 * MINUTE;

        let running = fixture.thread(ThreadStatus::Running);
        let (_, running_closes) = fixture.park(&running, started).await;

        let approving = fixture.thread(ThreadStatus::AwaitingApproval);
        let (_, approving_closes) = fixture.park(&approving, started).await;

        let with_turn = fixture.thread(ThreadStatus::Idle);
        let (turn_live, turn_closes) = fixture.park(&with_turn, started).await;
        *turn_live.turn.lock().await = Some(ActiveTurn {
            response_id: Uuid::now_v7(),
            id: Uuid::now_v7(),
            started,
            startup_started: started,
            provider: ProviderKind::ClaudeCode,
            session_reused: false,
            first_event_observed: false,
            messages: HashMap::new(),
            active_messages: HashMap::new(),
            terminal_message_id: None,
            completed: false,
            pending_completion: None,
            internal: None,
            rotation_seed: None,
        });

        let with_task = fixture.thread(ThreadStatus::Idle);
        let (task_live, task_closes) = fixture.park(&with_task, started).await;
        let now = chrono::Utc::now();
        task_live.tasks.lock().await.insert(
            "agent-1".into(),
            RuntimeTask {
                id: "agent-1".into(),
                thread_id: with_task.id,
                origin_turn_id: Uuid::now_v7(),
                started_seq: 1,
                updated_seq: 1,
                kind: RuntimeTaskKind::Agent,
                status: RuntimeTaskStatus::Running,
                title: "Explore".into(),
                detail: None,
                provider_type: None,
                parent_id: None,
                tool_call_id: None,
                provider_thread_id: None,
                model: None,
                effort: None,
                backgrounded: true,
                last_tool_name: None,
                usage: None,
                stats: RuntimeTaskStats::default(),
                capabilities: RuntimeTaskCapabilities::default(),
                started_at: now,
                updated_at: now,
                completed_at: None,
            },
        );

        let with_approval = fixture.thread(ThreadStatus::Idle);
        let (approval_live, approval_closes) = fixture.park(&with_approval, started).await;
        approval_live.pending.lock().await.insert(Uuid::now_v7(), "req-1".into());

        for (monotonic, wall) in [(later, SystemTime::now()), (started, SystemTime::now() + 60 * MINUTE)] {
            let released = fixture.orchestrator.release_idle_sessions_at(&policy(1, 1), false, monotonic, wall).await.unwrap();
            assert!(released.is_empty(), "neither awake time nor sleep can expire active work");
        }
        for closes in [running_closes, approving_closes, turn_closes, task_closes, approval_closes] {
            assert_eq!(closes.load(Ordering::SeqCst), 0);
        }
        assert_eq!(fixture.orchestrator.inner.sessions.lock().await.len(), 5);
        let activity = fixture.orchestrator.session_activity().await.unwrap();
        assert_eq!(activity, super::SessionActivity { live: 5, idle: 0 });
    }

    #[tokio::test]
    async fn warm_cap_releases_the_least_recently_used_sessions_first() {
        let fixture = Fixture::new();
        let started = Instant::now();
        let oldest = fixture.thread(ThreadStatus::Idle);
        let middle = fixture.thread(ThreadStatus::Idle);
        let newest = fixture.thread(ThreadStatus::Idle);
        let (_, oldest_closes) = fixture.park(&oldest, started).await;
        let (_, middle_closes) = fixture.park(&middle, started + MINUTE).await;
        let (_, newest_closes) = fixture.park(&newest, started + 2 * MINUTE).await;

        let released =
            fixture.orchestrator.release_idle_sessions_at(&policy(30, 2), false, started + 3 * MINUTE, SystemTime::now()).await.unwrap();
        assert_eq!(released, vec![super::SessionRelease { thread_id: oldest.id, reason: SessionReleaseReason::Capacity }]);
        assert_eq!(oldest_closes.load(Ordering::SeqCst), 1);
        assert_eq!(middle_closes.load(Ordering::SeqCst), 0);
        assert_eq!(newest_closes.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.release_events(&oldest), vec![SessionReleaseReason::Capacity]);

        // Expired sessions do not count against the cap; both rules apply in one pass.
        let released =
            fixture.orchestrator.release_idle_sessions_at(&policy(2, 1), false, started + 4 * MINUTE, SystemTime::now()).await.unwrap();
        let mut reasons: Vec<_> = released.iter().map(|r| (r.thread_id, r.reason)).collect();
        reasons.sort_by_key(|(thread_id, _)| *thread_id);
        let mut expected = vec![(middle.id, SessionReleaseReason::Idle), (newest.id, SessionReleaseReason::Idle)];
        expected.sort_by_key(|(thread_id, _)| *thread_id);
        assert_eq!(reasons, expected);
        assert!(fixture.orchestrator.inner.sessions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn activity_resets_the_idle_window_and_zero_disables_the_limits() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let started = Instant::now();
        let (live, closes) = fixture.park(&thread, started).await;

        live.last_activity.lock().unwrap().wall -= 60 * MINUTE;
        live.touch();
        let released =
            fixture.orchestrator.release_idle_sessions_at(&policy(10, 0), false, started + 10 * MINUTE, SystemTime::now()).await.unwrap();
        assert!(released.is_empty(), "a touch just now keeps the session warm");

        let far = started + 24 * 60 * MINUTE;
        let released =
            fixture.orchestrator.release_idle_sessions_at(&policy(0, 0), false, far, SystemTime::now() + 24 * 60 * MINUTE).await.unwrap();
        assert!(released.is_empty(), "zero disables both limits");
        assert_eq!(closes.load(Ordering::SeqCst), 0);
        assert!(fixture.has_session(&thread).await);
    }

    #[tokio::test]
    async fn released_processes_do_not_report_their_exit_as_a_failure() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let (live, _) = fixture.park(&thread, Instant::now()).await;
        live.mark_released();
        fixture
            .orchestrator
            .handle_driver_event(thread.id, &live, DriverEvent::Exited { code: Some(1), error: Some("exit code 1".into()) })
            .await
            .unwrap();
        let notices = fixture
            .store
            .events_for_thread(thread.id)
            .unwrap()
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::ProviderNotice { .. }))
            .count();
        assert_eq!(notices, 0);
    }

    #[tokio::test]
    async fn queue_worker_is_woken_and_told_when_to_check_back() {
        let fixture = Fixture::new();
        let running = fixture.thread(ThreadStatus::Running);
        let failed = fixture.thread(ThreadStatus::Failed);

        let orchestrator = fixture.orchestrator.clone();
        let woken = tokio::spawn(async move { orchestrator.queue_changed().await });
        fixture
            .orchestrator
            .enqueue(methods::QueuedMessage { id: Uuid::now_v7(), thread_id: failed.id, message: UserMessage::text("later") })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), woken).await.expect("enqueue wakes the queue worker").unwrap();

        assert!(!fixture.orchestrator.drain_queues().await.unwrap(), "a failed thread keeps its queue without polling");

        fixture
            .orchestrator
            .enqueue(methods::QueuedMessage { id: Uuid::now_v7(), thread_id: running.id, message: UserMessage::text("next") })
            .unwrap();
        assert!(fixture.orchestrator.drain_queues().await.unwrap(), "a busy thread's follow-up dispatches soon");
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn omp_processes_inherit_project_profiles_and_resume_with_the_original_profile() {
        let fixture = Fixture::with_drivers(DriverRegistry::with_defaults());
        let binary = fixture.root.join("fake-omp");
        let capture = fixture.root.join("launches.jsonl");
        kybern_protocol::process::write_executable(
            &binary,
            r#"#!/usr/bin/env python3
import json, os, sys
with open(os.environ['PROFILE_CAPTURE'], 'a') as f:
    f.write(json.dumps({'profile': os.environ.get('OMP_PROFILE'), 'cwd': os.getcwd(), 'args': sys.argv[1:]}) + '\n')
print(json.dumps({'type': 'ready'}), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    data = {'sessionId': 'provider-session', 'isStreaming': False} if request['type'] == 'get_state' else {}
    print(json.dumps({'id': request.get('id'), 'type': 'response', 'command': request['type'], 'success': True, 'data': data}), flush=True)
"#,
        )
        .unwrap();
        let settings_store = &fixture.orchestrator.inner.settings;
        let mut settings = settings_store.get();
        let provider = settings.providers.entry(ProviderKind::Omp).or_default();
        provider.binary = Some(binary.display().to_string());
        provider.env.insert("PROFILE_CAPTURE".into(), capture.display().to_string());
        provider.env.insert("OMP_PROFILE".into(), "global".into());
        provider.project_profiles.insert(fixture.project.path.clone(), "work".into());
        settings_store.set(settings).unwrap();
        let mut thread = fixture.thread_with_provider(ThreadStatus::Idle, ProviderKind::Omp);
        let worktree = fixture.root.join("worktree");
        std::fs::create_dir(&worktree).unwrap();
        thread.cwd = worktree.display().to_string();
        thread.provider_session_id = None;
        let mut expected = vec!["work", "work", "new-profile"];
        for index in 0..3 {
            if index == 1 {
                thread.provider_session_id = Some("provider-session".into());
                let mut settings = settings_store.get();
                settings
                    .providers
                    .get_mut(&ProviderKind::Omp)
                    .unwrap()
                    .project_profiles
                    .insert(fixture.project.path.clone(), "new-profile".into());
                settings_store.set(settings).unwrap();
            }
            if index == 2 {
                thread = fixture.thread_with_provider(ThreadStatus::Idle, ProviderKind::Omp);
                thread.provider_session_id = None;
            }
            let live = fixture.orchestrator.spawn_session(&thread, None).await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if std::fs::read_to_string(&capture).unwrap_or_default().lines().count() > index {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            live.session.close().await.unwrap();
            let value: serde_json::Value =
                serde_json::from_str(std::fs::read_to_string(&capture).unwrap().lines().last().unwrap()).unwrap();
            assert_eq!(value["profile"], expected.remove(0));
            assert_eq!(std::fs::canonicalize(value["cwd"].as_str().unwrap()).unwrap(), std::fs::canonicalize(&thread.cwd).unwrap());
            assert_eq!(value["args"].as_array().unwrap().iter().any(|arg| arg == "--resume"), index == 1);
        }
    }

    fn agent_task(id: &str, tool_call_id: Option<&str>, parent_id: Option<&str>) -> DriverRuntimeTask {
        DriverRuntimeTask {
            id: id.into(),
            kind: RuntimeTaskKind::Agent,
            status: RuntimeTaskStatus::Running,
            title: format!("Task {id}"),
            detail: None,
            provider_type: Some("Explore".into()),
            parent_id: parent_id.map(str::to_string),
            tool_call_id: tool_call_id.map(str::to_string),
            provider_thread_id: None,
            model: Some("claude-haiku".into()),
            effort: None,
            backgrounded: false,
            last_tool_name: None,
            usage: None,
            stats: RuntimeTaskStats::default(),
            capabilities: RuntimeTaskCapabilities { stop: true, background: true },
        }
    }

    fn agent_text(task_id: &str, id: &str, text: &str) -> DriverEvent {
        DriverEvent::MessageCompleted {
            message_id: id.into(),
            origin: EventOrigin::Agent { task_id: task_id.into(), provider_thread_id: None },
            text: text.into(),
            thinking: None,
        }
    }

    fn child_of(fixture: &Fixture, root: ThreadId, task_id: &str) -> Thread {
        fixture.store.subagent_thread_find(root, task_id).unwrap().expect("a child thread for the task")
    }

    async fn subagent_fixture() -> (Fixture, Thread, Arc<LiveSession>) {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let (live, _) = fixture.park(&thread, Instant::now()).await;
        fixture.orchestrator.send(thread.id, UserMessage::text("delegate")).await.unwrap();
        live.turn_ready.notified().await;
        (fixture, thread, live)
    }

    #[tokio::test]
    async fn a_subagent_becomes_a_read_only_thread_fed_by_its_own_output() {
        let (fixture, thread, live) = subagent_fixture().await;
        let o = &fixture.orchestrator;
        let launch = ToolCall { id: "toolu_1".into(), name: "Agent".into(), input: json!({"prompt":"Read note.txt"}), parent_id: None };
        o.handle_driver_event(thread.id, &live, DriverEvent::ToolStarted(launch)).await.unwrap();
        o.handle_driver_event(thread.id, &live, DriverEvent::SubagentPrompt { task_id: "task-1".into(), prompt: "Read note.txt".into() })
            .await
            .unwrap();
        o.handle_driver_event(thread.id, &live, DriverEvent::RuntimeTaskStarted(agent_task("task-1", Some("toolu_1"), None)))
            .await
            .unwrap();
        let child = child_of(&fixture, thread.id, "task-1");
        assert_eq!(child.parent_thread_id, Some(thread.id));
        assert_eq!(child.status, ThreadStatus::Running);
        let info = child.subagent.clone().unwrap();
        assert_eq!(
            (info.root_thread_id, info.status, info.tool_call_id.as_deref()),
            (thread.id, RuntimeTaskStatus::Running, Some("toolu_1"))
        );
        assert_eq!(info.agent_type.as_deref(), Some("Explore"));
        assert!(info.transcript && info.capabilities.stop);

        // The subagent's tool call stays nested in the parent and is mirrored into the child.
        let read = ToolCall {
            id: "toolu_2".into(),
            name: "Read".into(),
            input: json!({"file_path":"note.txt"}),
            parent_id: Some("toolu_1".into()),
        };
        o.handle_driver_event(thread.id, &live, DriverEvent::ToolStarted(read)).await.unwrap();
        o.handle_driver_event(
            thread.id,
            &live,
            DriverEvent::ToolCompleted { tool_call_id: "toolu_2".into(), output: json!({"content":"hi"}), is_error: false },
        )
        .await
        .unwrap();
        o.handle_driver_event(thread.id, &live, agent_text("toolu_1", "msg-1", "The note says hello.\nMore detail.")).await.unwrap();
        o.handle_driver_event(
            thread.id,
            &live,
            DriverEvent::RuntimeTaskCompleted(DriverRuntimeTaskUpdate::status("task-1", RuntimeTaskStatus::Completed)),
        )
        .await
        .unwrap();

        let parent_events = fixture.store.events_for_thread(thread.id).unwrap();
        assert!(
            !parent_events.iter().any(|event| matches!(&event.payload, EventPayload::AssistantMessageCompleted { .. })),
            "child prose never reaches the parent transcript"
        );
        assert!(parent_events.iter().any(|event| matches!(&event.payload,
            EventPayload::ToolCallStarted { call, origin: EventOrigin::Agent { .. } } if call.id == "toolu_2" && call.parent_id.as_deref() == Some("toolu_1"))));
        assert!(
            parent_events.iter().any(|event| matches!(&event.payload, EventPayload::RuntimeTaskStarted { task } if task.id == "task-1"))
        );

        let events = fixture.store.events_for_thread(child.id).unwrap();
        let kinds: Vec<_> = events.iter().map(|event| event.payload.kind()).collect();
        assert_eq!(
            kinds,
            [
                "thread_created",
                "turn_started",
                "tool_call_started",
                "thread_updated",
                "tool_call_completed",
                "assistant_message_completed",
                "turn_completed",
                "thread_updated"
            ]
        );
        assert!(matches!(&events[1].payload, EventPayload::TurnStarted { message, .. } if message.plain_text() == "Read note.txt"));
        assert!(matches!(&events[2].payload,
            EventPayload::ToolCallStarted { call, origin: EventOrigin::Root } if call.parent_id.is_none() && call.name == "Read"));
        let message_id = match &events[5].payload {
            EventPayload::AssistantMessageCompleted { message_id, origin: EventOrigin::Root, .. } => *message_id,
            other => panic!("unexpected {other:?}"),
        };
        assert!(matches!(&events[6].payload,
            EventPayload::TurnCompleted { stop_reason: StopReason::Completed, terminal_message_id: Some(id), .. } if *id == message_id));
        let done = child_of(&fixture, thread.id, "task-1");
        assert_eq!(done.status, ThreadStatus::Idle);
        let info = done.subagent.unwrap();
        assert_eq!(info.result.as_deref(), Some("The note says hello."));
        assert!(info.progress.is_none() && info.completed_at.is_some());

        // The transcript projects like any other thread.
        let transcript = fixture.store.project_transcript_through(child.id, done.last_seq).unwrap();
        assert!(
            transcript
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::Assistant { text, .. } if text.starts_with("The note says hello")))
        );

        // Read-only.
        let rejected = o.send(child.id, UserMessage::text("hi")).await.unwrap_err().to_string();
        assert!(rejected.contains("read-only"), "{rejected}");
        assert!(
            o.update_thread_fields(methods::ThreadsUpdateParams {
                thread_id: child.id,
                title: Some("x".into()),
                pinned: None,
                permission_mode: None,
                model: None,
                effort: None
            })
            .is_err()
        );
        // Lists leave them to callers that ask.
        assert_eq!(fixture.store.subagent_children(thread.id).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn subagent_output_waits_for_its_task_and_nested_agents_hang_below_their_parent() {
        let (fixture, thread, live) = subagent_fixture().await;
        let o = &fixture.orchestrator;
        // Output that arrives before the task starts is held, then delivered.
        o.handle_driver_event(thread.id, &live, agent_text("toolu_1", "early", "Early words")).await.unwrap();
        o.handle_driver_event(thread.id, &live, DriverEvent::RuntimeTaskStarted(agent_task("task-1", Some("toolu_1"), None)))
            .await
            .unwrap();
        let first = child_of(&fixture, thread.id, "task-1");
        assert!(fixture.store.events_for_thread(first.id).unwrap().iter().any(|event| matches!(&event.payload,
            EventPayload::AssistantMessageCompleted { text, .. } if text == "Early words")));

        // A tool call the first subagent makes launches a second one below it.
        let launch = ToolCall { id: "toolu_nested".into(), name: "Agent".into(), input: json!({}), parent_id: Some("toolu_1".into()) };
        o.handle_driver_event(thread.id, &live, DriverEvent::ToolStarted(launch)).await.unwrap();
        o.handle_driver_event(thread.id, &live, DriverEvent::RuntimeTaskStarted(agent_task("task-2", Some("toolu_nested"), None)))
            .await
            .unwrap();
        let second = child_of(&fixture, thread.id, "task-2");
        assert_eq!(second.parent_thread_id, Some(first.id));
        assert_eq!(second.subagent.as_ref().unwrap().root_thread_id, thread.id);
        // The first subagent's own thread shows the launch row.
        assert!(fixture.store.runtime_tasks_for_thread(first.id).unwrap().iter().any(|task| task.id == "task-2"));

        // Archiving the parent settles and archives the whole subtree.
        o.archive_thread(thread.id).await.unwrap();
        for id in [first.id, second.id] {
            assert_eq!(fixture.store.thread_get(id).unwrap().unwrap().status, ThreadStatus::Archived);
            assert!(fixture.store.events_for_thread(id).unwrap().iter().any(|event| matches!(event.payload, EventPayload::ThreadArchived)));
        }
    }

    #[tokio::test]
    async fn codex_style_subagent_output_goes_only_to_its_thread_and_the_prompt_may_arrive_late() {
        let fixture = Fixture::new();
        let thread = fixture.thread_with_provider(ThreadStatus::Idle, ProviderKind::Codex);
        let (live, _) = fixture.park(&thread, Instant::now()).await;
        let o = &fixture.orchestrator;
        o.send(thread.id, UserMessage::text("delegate")).await.unwrap();
        live.turn_ready.notified().await;
        let mut task = agent_task("child-thread", None, None);
        task.provider_thread_id = Some("child-thread".into());
        o.handle_driver_event(thread.id, &live, DriverEvent::RuntimeTaskStarted(task)).await.unwrap();
        let child = child_of(&fixture, thread.id, "child-thread");
        let wrap = |event| DriverEvent::SubagentOnly { task_id: "child-thread".into(), event: Box::new(event) };
        // The turn has not started: its first message is still unknown.
        assert!(
            !fixture
                .store
                .events_for_thread(child.id)
                .unwrap()
                .iter()
                .any(|event| matches!(event.payload, EventPayload::TurnStarted { .. }))
        );
        o.handle_driver_event(
            thread.id,
            &live,
            DriverEvent::SubagentPrompt { task_id: "child-thread".into(), prompt: "Review the diff".into() },
        )
        .await
        .unwrap();
        o.handle_driver_event(
            thread.id,
            &live,
            wrap(DriverEvent::ToolStarted(ToolCall {
                id: "cmd-1".into(),
                name: "shell".into(),
                input: json!({"command":"git diff"}),
                parent_id: None,
            })),
        )
        .await
        .unwrap();
        o.handle_driver_event(thread.id, &live, wrap(DriverEvent::ToolOutputDelta { tool_call_id: "cmd-1".into(), delta: "+line".into() }))
            .await
            .unwrap();
        o.handle_driver_event(
            thread.id,
            &live,
            wrap(DriverEvent::ToolCompleted { tool_call_id: "cmd-1".into(), output: json!({}), is_error: false }),
        )
        .await
        .unwrap();
        o.handle_driver_event(
            thread.id,
            &live,
            wrap(DriverEvent::ThinkingDelta { message_id: "r1".into(), origin: EventOrigin::Root, delta: "hmm".into() }),
        )
        .await
        .unwrap();
        o.handle_driver_event(
            thread.id,
            &live,
            wrap(DriverEvent::ThinkingCompleted { message_id: "r1".into(), origin: EventOrigin::Root }),
        )
        .await
        .unwrap();
        o.handle_driver_event(
            thread.id,
            &live,
            wrap(DriverEvent::TextDelta { message_id: "m1".into(), origin: EventOrigin::Root, delta: "Looks".into() }),
        )
        .await
        .unwrap();
        o.handle_driver_event(
            thread.id,
            &live,
            wrap(DriverEvent::MessageCompleted {
                message_id: "m1".into(),
                origin: EventOrigin::Root,
                text: "Looks fine".into(),
                thinking: None,
            }),
        )
        .await
        .unwrap();

        let events = fixture.store.events_for_thread(child.id).unwrap();
        let kinds: Vec<_> = events.iter().map(|event| event.payload.kind()).collect();
        assert_eq!(
            kinds,
            [
                "thread_created",
                "turn_started",
                "tool_call_started",
                "tool_call_output_delta",
                "tool_call_completed",
                "assistant_thinking_delta",
                "assistant_thinking_completed",
                "assistant_text_delta",
                "assistant_message_completed"
            ]
        );
        // Reasoning, text and the final message share one logical message.
        let ids: HashSet<_> = events
            .iter()
            .filter_map(|event| match &event.payload {
                EventPayload::AssistantThinkingDelta { message_id, .. }
                | EventPayload::AssistantTextDelta { message_id, .. }
                | EventPayload::AssistantMessageCompleted { message_id, .. } => Some(*message_id),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 1);
        let parent = fixture.store.events_for_thread(thread.id).unwrap();
        assert!(!parent.iter().any(|event| matches!(
            &event.payload,
            EventPayload::ToolCallStarted { .. } | EventPayload::ToolCallOutputDelta { .. } | EventPayload::AssistantTextDelta { .. }
        )));
    }

    #[tokio::test]
    async fn stopping_a_subagent_thread_stops_its_task_in_the_root_session() {
        let fixture = Fixture::new();
        let thread = fixture.thread(ThreadStatus::Idle);
        let stopped = Arc::new(Mutex::new(Vec::new()));
        let live = Arc::new(LiveSession {
            session_instance_id: Uuid::now_v7(),
            session: Box::new(TestSession { stopped_tasks: stopped.clone(), ..Default::default() }),
            computer_tools: false,
            last_activity: std::sync::Mutex::new(SessionActivityTime::now()),
            released: AtomicBool::new(false),
            retained: AtomicBool::new(false),
            stop_cleanup: AtomicBool::new(false),
            turn: Mutex::new(None),
            continuation: Mutex::new(None),
            turn_ready: tokio::sync::Notify::new(),
            last_turn_id: Mutex::new(None),
            tasks: Mutex::new(HashMap::new()),
            task_aliases: Mutex::new(HashMap::new()),
            deferred_checkpoints: Mutex::new(HashSet::new()),
            pending: Mutex::new(HashMap::new()),
            daemon_approvals: Mutex::new(HashMap::new()),
            app_tool_requests: Mutex::new(HashSet::new()),
            app_tool_permits: Arc::new(Semaphore::new(crate::app_tools::MAX_CONCURRENT_REQUESTS)),
            brief_revision: Mutex::new(None),
            lead_settings: Mutex::new(None),
            context_usage: std::sync::Mutex::new(None),
            instructions_unconfirmed: AtomicBool::new(false),
        });
        fixture.orchestrator.inner.sessions.lock().await.insert(thread.id, live.clone());
        fixture.orchestrator.send(thread.id, UserMessage::text("delegate")).await.unwrap();
        live.turn_ready.notified().await;
        let o = &fixture.orchestrator;
        o.handle_driver_event(thread.id, &live, DriverEvent::RuntimeTaskStarted(agent_task("task-1", Some("toolu_1"), None)))
            .await
            .unwrap();
        let child = child_of(&fixture, thread.id, "task-1");
        // Interrupting the child thread (or tasks.stop with its id) stops that subagent only.
        o.stop_thread(child.id, "test").await.unwrap();
        assert_eq!(*stopped.lock().await, ["task-1"]);
        assert_eq!(child_of(&fixture, thread.id, "task-1").subagent.unwrap().status, RuntimeTaskStatus::Stopping);
        o.handle_driver_event(
            thread.id,
            &live,
            DriverEvent::RuntimeTaskCompleted(DriverRuntimeTaskUpdate::status("task-1", RuntimeTaskStatus::Stopped)),
        )
        .await
        .unwrap();
        let done = child_of(&fixture, thread.id, "task-1");
        assert_eq!(done.status, ThreadStatus::Idle);
        assert!(
            fixture
                .store
                .events_for_thread(child.id)
                .unwrap()
                .iter()
                .any(|event| matches!(event.payload, EventPayload::TurnCompleted { stop_reason: StopReason::Interrupted, .. }))
        );
    }

    #[tokio::test]
    async fn a_daemon_restart_settles_running_subagent_threads() {
        let (fixture, thread, live) = subagent_fixture().await;
        fixture
            .orchestrator
            .handle_driver_event(thread.id, &live, DriverEvent::RuntimeTaskStarted(agent_task("task-1", None, None)))
            .await
            .unwrap();
        let child = child_of(&fixture, thread.id, "task-1");
        assert_eq!(child.status, ThreadStatus::Running);
        fixture.orchestrator.recover_after_restart().await.unwrap();
        let after = child_of(&fixture, thread.id, "task-1");
        assert_eq!(after.status, ThreadStatus::Idle);
        assert_eq!(after.subagent.unwrap().status, RuntimeTaskStatus::Interrupted);
        let kinds: Vec<_> = fixture.store.events_for_thread(child.id).unwrap().iter().map(|event| event.payload.kind()).collect();
        assert_eq!(kinds.iter().filter(|kind| **kind == "turn_completed").count(), 1);
        assert!(!kinds.contains(&"turn_failed"));
    }

    include!("orchestrator/antigravity_tests.rs");
    include!("orchestrator/delegation_tests.rs");
    include!("orchestrator/remote_delegation_tests.rs");
    include!("orchestrator/provision_tests.rs");
    include!("orchestrator/inbound_tests.rs");
    include!("orchestrator/fork_tests.rs");
    include!("orchestrator/move_tests.rs");
    include!("orchestrator/group_transfer_tests.rs");
    include!("orchestrator/federated_tools_tests.rs");
    include!("orchestrator/child_questions_tests.rs");
    include!("orchestrator/secret_tests.rs");
    include!("orchestrator/request_tests.rs");
    include!("orchestrator/lead_git_tests.rs");
    include!("orchestrator/group_git_tests.rs");
    include!("orchestrator/group_pr_tests.rs");
    include!("orchestrator/capabilities_tests.rs");
    include!("orchestrator/lead_tests.rs");
    include!("orchestrator/change_report_tests.rs");
    include!("orchestrator/lead_setup_tests.rs");
    include!("orchestrator/messaging_tests.rs");
    include!("orchestrator/account_tests.rs");
    include!("orchestrator/worktree_tests.rs");
    include!("orchestrator/subagent_messaging_tests.rs");
    include!("orchestrator/full_access_tests.rs");
    include!("orchestrator/groups_tests.rs");
    include!("orchestrator/project_map_tests.rs");
    include!("orchestrator/rotation_tests.rs");
}
