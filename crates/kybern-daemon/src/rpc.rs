//! Method dispatch. Every handler parses typed params, checks nothing about
//! auth (the connection layer already did), and returns a typed result.

use kybern_protocol::methods::*;
use kybern_protocol::process::NoWindow;
use kybern_protocol::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::state::AppState;
use crate::ws::ConnectionCtx;

/// Every RPC method's handler. The match below is one large future (each arm's await state is
/// laid out inline), so it lives on the heap: callers and tests keep a small stack frame.
pub async fn dispatch(state: &AppState, ctx: &ConnectionCtx, method: &str, params: Value) -> Result<Value, RpcError> {
    // Repo identity refresh publishes projects.changed. Resolve a newly identified clone before clients read its next projection.
    if matches!(method, ProjectsList::NAME | MachinesInfo::NAME) {
        crate::network::spaces::reconcile(state).await;
    }
    if matches!(method, NetworkAdd::NAME | NetworkJoin::NAME | NetworkMerge::NAME)
        || (method == NetworkSetEnabled::NAME && params.get("enabled").and_then(Value::as_bool) == Some(true))
    {
        let _guard = state.network.spaces_lock.lock().await;
        crate::network::spaces::prefer_local(state).map_err(RpcError::internal)?;
    }
    // Serialize user writes with replica ingestion; capture only fields touched by the mutation.
    let replicated = matches!(method, SpacesCreate::NAME | SpacesUpdate::NAME | SpacesDelete::NAME | SpacesReorder::NAME)
        || (method == ProjectsUpdate::NAME && params.get("space_id").is_some());
    if replicated {
        let _guard = state.network.spaces_lock.lock().await;
        crate::network::spaces::prepare(state).map_err(RpcError::internal)?;
        let before = state.store.spaces_list().map_err(RpcError::internal)?;
        let project = if method == ProjectsUpdate::NAME {
            params.get("project_id").cloned().and_then(|v| serde_json::from_value(v).ok())
        } else {
            None
        };
        let result = Box::pin(dispatch_method(state, ctx, method, params)).await?;
        crate::network::spaces::record(state, &before, project).map_err(RpcError::internal)?;
        return Ok(result);
    }
    Box::pin(dispatch_method(state, ctx, method, params)).await
}

/// Every RPC method's handler, in slices. One match over every method is one frame of well over
/// a megabyte in an unoptimized build (nothing is shared between arms), and a test thread has
/// two. Each part answers the methods it knows, and leaves the params in `slot` for the next part
/// when the method is not its own; the last part reports an unknown method.
async fn dispatch_method(state: &AppState, ctx: &ConnectionCtx, method: &str, params: Value) -> Result<Value, RpcError> {
    let mut slot = Some(params);
    macro_rules! try_parts {
        ($($part:ident),+ ; $last:ident) => {{
            $(
                let result = $part(state, ctx, method, &mut slot).await;
                if slot.is_none() {
                    return result;
                }
            )+
            $last(state, ctx, method, &mut slot).await
        }};
    }
    try_parts!(dispatch_part_1, dispatch_part_2, dispatch_part_3, dispatch_part_4, dispatch_part_5, dispatch_part_6, dispatch_part_7, dispatch_part_8, dispatch_part_9, dispatch_part_10, dispatch_part_11, dispatch_part_12, dispatch_part_13, dispatch_part_14, dispatch_part_15, dispatch_part_utilities, dispatch_part_16; dispatch_part_17)
}

async fn dispatch_part_1(state: &AppState, ctx: &ConnectionCtx, method: &str, slot: &mut Option<Value>) -> Result<Value, RpcError> {
    let params = slot.take().unwrap_or_default();
    match method {
        DaemonInfoMethod::NAME => {
            let info = DaemonInfo {
                version: env!("CARGO_PKG_VERSION").to_string(),
                protocol_version: PROTOCOL_VERSION,
                environment_id: state.environment_id.clone(),
                hostname: hostname(),
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
                data_dir: state.paths.root.display().to_string(),
                scopes: ctx.principal.scopes.clone(),
                started_at: state.started_at,
                machine_id: state.environment_id.clone(),
                network_protocol: NETWORK_PROTOCOL,
                features: state.network.features(),
            };
            ok(info)
        }
        DaemonActivityMethod::NAME => ok(crate::maintenance::activity(state).await.map_err(internal)?),
        DaemonShutdown::NAME => {
            if !crate::auth::is_bootstrap(state, &ctx.principal) {
                return Err(RpcError::new(codes::FORBIDDEN, "daemon shutdown requires the local bootstrap client"));
            }
            let shutdown = state.shutdown.clone();
            tokio::spawn(async move {
                // Let the response reach the desktop before closing every socket.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                shutdown.cancel();
            });
            ok(Empty {})
        }
        SessionsList::NAME => {
            let p: SessionsListParams = parse(params)?;
            let cwd = match p.project_id {
                Some(id) => Some(std::path::PathBuf::from(
                    state.store.project_get(id).map_err(internal)?.ok_or_else(|| RpcError::not_found("project"))?.path,
                )),
                None => None,
            };
            let snapshot = state.settings.get();
            let raw_provider = snapshot.providers.get(&p.provider).cloned().unwrap_or_default();
            let instance = crate::provider_accounts::resolve(&raw_provider, cwd.as_deref().and_then(|path| path.to_str()), None);
            let settings = crate::settings::provider_settings(&snapshot, p.provider, cwd.as_deref().and_then(|p| p.to_str()));
            let context = kybern_drivers::ProbeContext { binary: settings.binary.map(std::path::PathBuf::from), cwd, env: settings.env };
            let driver = state.drivers.get(p.provider).ok_or_else(|| RpcError::not_found("harness"))?;
            let mut result =
                driver.list_sessions(&context, p.cursor.as_deref(), p.query.as_deref().unwrap_or("")).await.map_err(|e| bad(e.into()))?;
            let threads = state.store.threads_list(None, true).map_err(internal)?;
            for session in &mut result.sessions {
                session.thread_id = threads
                    .iter()
                    .find(|thread| {
                        thread.provider.kind == session.provider
                            && thread.provider.instance == instance
                            && thread.provider_session_id.as_deref() == Some(&session.id)
                    })
                    .map(|thread| thread.id);
            }
            ok(result)
        }
        SessionsResume::NAME => {
            let p: SessionsResumeParams = parse(params)?;
            ok(state.orchestrator.resume_external_session(p).await.map_err(bad)?)
        }
        SessionsScanProjects::NAME => ok(state.session_imports.start_scan(state, parse_or_default(params)?)),
        SessionsScanGet::NAME => {
            let p: SessionsScanRef = parse(params)?;
            ok(state.session_imports.scan_get(&p.scan_id).ok_or_else(|| RpcError::not_found("scan"))?)
        }
        SessionsScanCancel::NAME => {
            let p: SessionsScanRef = parse(params)?;
            ok(state.session_imports.scan_cancel(&p.scan_id).ok_or_else(|| RpcError::not_found("scan"))?)
        }
        SessionsImportAll::NAME => ok(state.session_imports.start_import(state, parse(params)?).map_err(bad)?),
        SessionsImportGet::NAME => {
            let p: SessionsImportRef = parse(params)?;
            ok(state.session_imports.import_get(&p.job_id).ok_or_else(|| RpcError::not_found("import job"))?)
        }
        _ => {
            *slot = Some(params);
            Err(RpcError::method_not_found(method))
        }
    }
}

async fn dispatch_part_2(state: &AppState, ctx: &ConnectionCtx, method: &str, slot: &mut Option<Value>) -> Result<Value, RpcError> {
    let params = slot.take().unwrap_or_default();
    match method {
        SessionsImportCancel::NAME => {
            let p: SessionsImportRef = parse(params)?;
            ok(state.session_imports.import_cancel(&p.job_id).ok_or_else(|| RpcError::not_found("import job"))?)
        }
        ProvidersList::NAME => {
            let p: ProvidersListParams = parse_or_default(params)?;
            let cwd = p
                .project_id
                .map(|project_id| {
                    state
                        .store
                        .project_get(project_id)
                        .map_err(internal)?
                        .map(|project| std::path::PathBuf::from(project.path))
                        .ok_or_else(|| RpcError::not_found("project"))
                })
                .transpose()?;
            let providers = crate::network::machine_info::provider_statuses(state, p.project_id, cwd, p.force_refresh).await;
            ok(ProvidersListResult { providers })
        }
        AccountsCreate::NAME => ok(crate::provider_accounts::create(state, parse(params)?).map_err(bad)?),
        AccountsSignIn::NAME => ok(crate::provider_accounts::sign_in(state, parse(params)?).map_err(provider_err)?),
        AccountsCatalog::NAME => ok(crate::provider_accounts::catalog(state, parse(params)?).await.map_err(provider_err)?),
        AccountsUsage::NAME => ok(crate::provider_accounts::usage(state, parse(params)?).await.map_err(provider_err)?),
        AccountsList::NAME => ok(crate::provider_accounts::list(state, parse_or_default(params)?).await.map_err(bad)?),
        AccountsLoginStart::NAME => ok(state.account_logins.start(state, parse(params)?).await.map_err(bad)?),
        AccountsLoginGet::NAME => {
            let p: AccountLoginIdParams = parse(params)?;
            let login = state.account_logins.get(&p.id).map_err(bad)?;
            ok(if ctx.principal.has(Scope::AccessWrite) { login } else { crate::account_login::for_observer(login) })
        }
        AccountsLoginInput::NAME => {
            let p: AccountLoginInputParams = parse(params)?;
            ok(state.account_logins.input(&p.id, p.code).await.map_err(bad)?)
        }
        _ => {
            *slot = Some(params);
            Err(RpcError::method_not_found(method))
        }
    }
}

async fn dispatch_part_3(state: &AppState, ctx: &ConnectionCtx, method: &str, slot: &mut Option<Value>) -> Result<Value, RpcError> {
    let params = slot.take().unwrap_or_default();
    match method {
        AccountsLoginCancel::NAME => {
            let p: AccountLoginIdParams = parse(params)?;
            ok(state.account_logins.cancel(state, &p.id).await.map_err(bad)?)
        }
        AccountsLoginFinish::NAME => ok(state.account_logins.finish(state, parse(params)?).await.map_err(bad)?),
        AccountsUpdate::NAME => ok(crate::provider_accounts::update(state, parse(params)?).await.map_err(bad)?),
        AccountsSignOut::NAME => ok(crate::provider_accounts::sign_out(state, parse(params)?).await.map_err(bad)?),
        AccountsRemove::NAME => {
            crate::provider_accounts::remove(state, parse(params)?).await.map_err(bad)?;
            ok(Empty {})
        }
        ThreadsTargetGet::NAME => {
            let p: ThreadsInterruptParams = parse(params)?;
            ok(state.orchestrator.thread_target(p.thread_id).map_err(bad)?)
        }
        ThreadsTargetSet::NAME => ok(state.orchestrator.set_thread_target(parse(params)?).await.map_err(bad)?),
        ThreadsPermissionsApply::NAME => {
            let p: ThreadsInterruptParams = parse(params)?;
            ok(state.orchestrator.apply_pending_permission(p.thread_id, true).await.map_err(provider_err)?)
        }
        ThreadsSwitchContinue::NAME => ok(state.orchestrator.switch_continue(parse(params)?).await.map_err(provider_err)?),
        HarnessUpdatesList::NAME => ok(HarnessUpdatesResult { updates: state.harness_updates.list() }),
        HarnessUpdatesRun::NAME => {
            let p: HarnessUpdateParams = parse(params)?;
            let record = state.harness_updates.request(&state.store, p.kind);
            tokio::spawn(crate::harness_updates::tick(state.clone()));
            ok(record)
        }
        DaemonUpdateStatusMethod::NAME => ok(state.daemon_updates.get()),
        DaemonUpdateCheck::NAME => ok(crate::self_update::check(state).await),
        DaemonUpdateRun::NAME => {
            // The desktop app's daemon updates by updating the app (ADE-125), the way "Update all machines" does.
            if state.desktop_managed.load(std::sync::atomic::Ordering::SeqCst) {
                crate::network::updates::start_self(state, None).await;
                return ok(state.daemon_updates.get());
            }
            let record = state.daemon_updates.request(&state.store);
            tokio::spawn(crate::self_update::tick(state.clone()));
            ok(record)
        }
        AppUpdaterAttach::NAME => {
            if !crate::auth::is_bootstrap(state, &ctx.principal) {
                return Err(RpcError::new(codes::FORBIDDEN, "Only the Kybern app on this machine can update it"));
            }
            let p: AppUpdaterAttachParams = parse(params)?;
            state.app_updates.attach(crate::app_update::Attached {
                connection: ctx.id,
                version: p.version,
                can_install: p.can_install,
                reason: p.reason,
            });
            ok(Empty {})
        }
        AppUpdaterReport::NAME => {
            if !crate::auth::is_bootstrap(state, &ctx.principal) {
                return Err(RpcError::new(codes::FORBIDDEN, "Only the Kybern app on this machine can update it"));
            }
            let p: AppUpdaterReportParams = parse(params)?;
            crate::app_update::report(state, ctx.id, p);
            ok(Empty {})
        }
        ProjectsList::NAME => {
            let p: ProjectsListParams = parse_or_default(params)?;
            let projects = if p.refresh_git == Some(false) {
                state.store.projects_list().map_err(internal)?
            } else {
                state.orchestrator.refresh_projects_git().await.map_err(internal)?
            };
            ok(ProjectsListResult { projects })
        }
        ProjectsBrowse::NAME => {
            let p: ProjectsBrowseParams = parse_or_default(params)?;
            ok(crate::files::browse_directories(p.path).await.map_err(bad)?)
        }
        ProjectsProvisionPlan::NAME => {
            let p: ProjectsProvisionPlanParams = parse(params)?;
            ok(state.orchestrator.provision_plan(p.project_id, &p.machine, p.path).await?)
        }
        ProjectsProvision::NAME => ok(crate::network::provision::start(state, parse(params)?).await?),
        ProjectsProvisionStatusMethod::NAME => {
            let p: ProjectsProvisionStatusParams = parse(params)?;
            ok(crate::network::provision::status(state, p.operation_id)?)
        }
        ProjectsAdd::NAME => {
            let p: ProjectsAddParams = parse(params)?;
            match p.kind.unwrap_or_default() {
                ProjectKind::Group => {
                    let members = p.members.ok_or_else(|| RpcError::new(codes::INVALID_PARAMS, "Select at least 2 repositories."))?;
                    ok(state.orchestrator.add_group_project(p.path, p.name, p.space_id, members)?)
                }
                ProjectKind::Single => {
                    if p.members.is_some() {
                        return Err(RpcError::new(codes::INVALID_PARAMS, "Only group projects have repositories."));
                    }
                    ok(state.orchestrator.add_project(p.path, p.name, p.space_id).map_err(bad)?)
                }
            }
        }
        ProjectsScanRepos::NAME => {
            let p: ProjectsScanReposParams = parse(params)?;
            ok(state.orchestrator.scan_repos(p).await?)
        }
        ProjectsMembersStatus::NAME => {
            let p: ProjectsMembersStatusParams = parse(params)?;
            ok(state.orchestrator.members_status(p.project_id).await?)
        }
        ProjectsMap::NAME => ok(state.orchestrator.projects_map(parse(params)?).await.map_err(bad)?),
        ProjectsInstructionsCreate::NAME => {
            let p: ProjectsInstructionsCreateParams = parse(params)?;
            ok(state.orchestrator.create_group_instructions(p.project_id)?)
        }
        _ => {
            *slot = Some(params);
            Err(RpcError::method_not_found(method))
        }
    }
}

async fn dispatch_part_4(state: &AppState, _ctx: &ConnectionCtx, method: &str, slot: &mut Option<Value>) -> Result<Value, RpcError> {
    let params = slot.take().unwrap_or_default();
    match method {
        ProjectsUpdate::NAME => {
            let p: ProjectsUpdateParams = parse(params)?;
            if let Some(space_id) = p.space_id {
                // Before the other fields, so a failed move leaves the project as it was.
                state.orchestrator.move_project(p.project_id, space_id)?;
            }
            let mut project = state.store.project_get(p.project_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("project"))?;
            if let Some(n) = p.name {
                let n = n.trim();
                if n.is_empty() || n.chars().count() > 100 {
                    return Err(RpcError::new(codes::INVALID_PARAMS, "Use a project name of 1 to 100 characters."));
                }
                project.name = n.to_string();
            }
            if let Some(w) = p.worktrees_default {
                project.worktrees_default = w;
            }
            if let Some(brief_everywhere) = p.brief_in_all_threads {
                project.brief_in_all_threads = brief_everywhere;
            }
            if let Some(coordinator) = p.coordinator {
                if let Some(overrides) = &coordinator {
                    overrides.validate().map_err(|error| RpcError::new(codes::INVALID_PARAMS, error))?;
                }
                project.coordinator = coordinator.filter(|overrides| !overrides.is_empty());
            }
            // Validated before anything is saved, so a rejected member list leaves the other edits unsaved too.
            let members =
                state.orchestrator.apply_group_update(&mut project, p.members, p.worktree_copy, p.instructions_offer_dismissed)?;
            project.updated_at = chrono::Utc::now();
            if let Some(prefix) = &p.task_prefix {
                // Validated first, so a bad prefix leaves the other edits unsaved too.
                state.store.project_set_task_prefix(p.project_id, prefix).map_err(task_err)?;
            }
            state.store.project_update(&project).map_err(internal)?;
            if let Some(members) = members {
                state.store.project_set_members(p.project_id, &members).map_err(internal)?;
            }
            state.orchestrator.publish_projects();
            ok(state.store.project_get(p.project_id).map_err(internal)?.unwrap_or(project))
        }
        SpacesPlace::NAME => ok(crate::network::spaces::place(state, parse(params)?).await?),
        SpacesList::NAME => ok(crate::network::spaces::list(state)?),
        SpacesCreate::NAME => {
            let p: SpacesCreateParams = parse(params)?;
            ok(state.orchestrator.create_space(p.name, p.color, p.logo).await?)
        }
        SpacesUpdate::NAME => {
            let p: SpacesUpdateParams = parse(params)?;
            ok(state.orchestrator.update_space(p.space_id, p.name, p.color, p.logo).await?)
        }
        SpacesDelete::NAME => {
            let p: SpacesDeleteParams = parse(params)?;
            ok(state.orchestrator.delete_space(p.space_id)?)
        }
        SpacesReorder::NAME => {
            let p: SpacesReorderParams = parse(params)?;
            state.orchestrator.reorder_spaces(&p.space_ids)?;
            ok(crate::network::spaces::list(state)?)
        }
        ProjectsRemove::NAME => {
            let p: ProjectsRemoveParams = parse(params)?;
            state.orchestrator.remove_project(p.project_id).map_err(note_err)?;
            ok(Empty {})
        }
        _ => {
            *slot = Some(params);
            Err(RpcError::method_not_found(method))
        }
    }
}

async fn dispatch_part_5(state: &AppState, _ctx: &ConnectionCtx, method: &str, slot: &mut Option<Value>) -> Result<Value, RpcError> {
    let params = slot.take().unwrap_or_default();
    match method {
        ThreadsList::NAME => {
            let p: ThreadsListParams = parse_or_default(params)?;
            let mut threads = state.store.threads_list(p.project_id, p.include_archived).map_err(internal)?;
            // Counted before the children are filtered out: a parent whose delegated children still run must say so, or a
            // client that starts while they work never lists them.
            let mut running_children: std::collections::HashMap<ThreadId, u32> = std::collections::HashMap::new();
            for thread in &threads {
                if let Some(delegation) = &thread.delegation
                    && delegation.status == DelegationStatus::Running
                    && thread.status != ThreadStatus::Archived
                {
                    *running_children.entry(delegation.parent_thread_id).or_default() += 1;
                }
            }
            // Children on other machines count too: a client reads their records (`delegations.remote_list`) for these parents.
            for child in state.store.remote_children_active().map_err(internal)? {
                if !child.state.is_terminal() {
                    *running_children.entry(child.parent_thread_id).or_default() += 1;
                }
            }
            // Subagent and delegated threads are opt-in: older clients must never list one as a normal thread.
            if let Some(parent) = p.parent_thread_id {
                threads
                    .retain(|thread| (thread.subagent.is_some() || thread.delegation.is_some()) && thread.parent_thread_id == Some(parent));
            } else if !p.include_subagents {
                // A child another machine delegated here has no parent on this machine, so it lists as a thread of its own;
                // clients nest it under the thread that started it (`Thread::origin`).
                threads.retain(|thread| (thread.subagent.is_none() && thread.delegation.is_none()) || thread.is_remote_child());
            }
            // The hidden threads that record other machines' tool calls are opt-in too.
            if !p.include_inbound {
                threads.retain(|thread| thread.inbound.is_none());
            }
            let (own_pending, child_pending) = pending_request_kinds(&state.store).map_err(internal)?;
            let mut activity: Vec<ThreadActivitySummary> = threads
                .iter()
                .map(|thread| {
                    let tasks = state.store.runtime_tasks_for_thread(thread.id).map_err(internal)?;
                    let mut summary = kybern_store::project_thread_activity(thread.id, &tasks);
                    summary.active_children = running_children.get(&thread.id).copied().unwrap_or(0);
                    summary.pending_request = own_pending.get(&thread.id).copied();
                    summary.pending_child_request = child_pending.get(&thread.id).copied();
                    Ok(summary)
                })
                .collect::<Result<Vec<_>, RpcError>>()?
                .into_iter()
                .filter(|summary| {
                    summary.state.is_some()
                        || summary.active_children > 0
                        || summary.pending_request.is_some()
                        || summary.pending_child_request.is_some()
                })
                .collect();
            // Delegated agents are left out of the list, but their rows say what they wait on: add their kinds alone.
            let mut waiting: Vec<_> = own_pending.iter().filter(|(id, _)| !threads.iter().any(|thread| thread.id == **id)).collect();
            waiting.sort_by_key(|(id, _)| **id);
            activity.extend(waiting.into_iter().map(|(id, kind)| ThreadActivitySummary {
                thread_id: *id,
                state: None,
                active_agents: 0,
                active_processes: 0,
                active_monitors: 0,
                active_children: 0,
                pending_request: Some(*kind),
                pending_child_request: child_pending.get(id).copied(),
            }));
            ok(ThreadsListResult { threads, activity })
        }
        ThreadsSearch::NAME => {
            let p: ThreadsSearchParams = parse_or_default(params)?;
            let project_id = (!p.all_projects).then_some(p.project_id).flatten();
            let preferred_project_id = p.all_projects.then_some(p.project_id).flatten();
            let page = state
                .store
                .search_thread_history(
                    project_id,
                    preferred_project_id,
                    p.query.as_deref(),
                    p.include_archived,
                    p.cursor.as_deref(),
                    p.limit,
                )
                .map_err(bad)?;
            ok(ThreadsSearchResult { threads: page.threads, next_cursor: page.next_cursor })
        }
        ThreadsRead::NAME => {
            let p: ThreadsReadParams = parse(params)?;
            let page = state
                .store
                .read_thread_history(p.thread_id, p.before_seq, p.through_seq, p.limit, p.message_seq, p.text_offset)
                .map_err(bad)?;
            let messages = page
                .messages
                .into_iter()
                .map(|message| ThreadReadMessage {
                    seq: message.seq,
                    turn_id: message.turn_id,
                    role: message.role,
                    text: message.text,
                    created_at: message.created_at,
                    attribution: message.attribution,
                    text_offset: message.text_offset,
                    next_text_offset: message.next_text_offset,
                    text_truncated: message.text_truncated,
                })
                .collect();
            ok(ThreadsReadResult { thread: page.thread, messages, next_before_seq: page.next_before_seq, through_seq: page.through_seq })
        }
        _ => {
            *slot = Some(params);
            Err(RpcError::method_not_found(method))
        }
    }
}

async fn dispatch_part_6(state: &AppState, _ctx: &ConnectionCtx, method: &str, slot: &mut Option<Value>) -> Result<Value, RpcError> {
    let params = slot.take().unwrap_or_default();
    match method {
        ThreadsCreate::NAME => {
            let p: ThreadsCreateParams = parse(params)?;
            ok(state.orchestrator.create_thread(p).await.map_err(bad)?)
        }
        ThreadsGet::NAME => {
            let p: ThreadsGetParams = parse(params)?;
            if p.transcript_limit.is_some_and(|limit| !(1..=500).contains(&limit))
                || (p.before_seq.is_some() && p.transcript_limit.is_none())
                || p.before_seq.is_some_and(|seq| seq < 0)
                || p.through_seq.is_some_and(|seq| seq < 0)
            {
                return Err(RpcError::invalid_params("use transcript_limit 1..500 and nonnegative history cursors"));
            }
            let mut thread = state.store.thread_get(p.thread_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("thread"))?;
            thread = state.orchestrator.migrate_cursor_model(thread).map_err(internal)?;
            let through_seq = p.through_seq.map_or(thread.last_seq, |seq| seq.min(thread.last_seq));
            thread.last_seq = through_seq;
            let id = p.thread_id;
            let projection = state.thread_projections.transcript(state.store.clone(), id, through_seq).await.map_err(internal)?;
            let (mut transcript, next_before_seq) = kybern_store::transcript_page_ref_with(
                projection.transcript.rows(),
                p.transcript_limit,
                p.before_seq,
                p.whole_turns.unwrap_or(false),
            );
            if p.defer_tool_stream.unwrap_or(false) {
                projection.transcript.mark_tool_streams_omitted(&mut transcript);
            }
            if p.include_tool_output.unwrap_or(true) {
                state.store.hydrate_tool_outputs_through(id, &mut transcript, through_seq).map_err(internal)?;
            }
            // An older page only extends the transcript the client already
            // holds. Its runtime tasks alone were 1.1 MB of a 1.2 MB page.
            if p.transcript_only.unwrap_or(false) && p.before_seq.is_some() {
                return ok(ThreadsGetResult {
                    notes: Default::default(),
                    thread,
                    transcript,
                    next_before_seq,
                    pending_approvals: Vec::new(),
                    runtime_tasks: Vec::new(),
                    provider_usage: Default::default(),
                    provider_commands: Vec::new(),
                    pending_questions: Vec::new(),
                });
            }
            let side = state.thread_projections.side_state(&projection, state.store.clone(), id, through_seq).await.map_err(internal)?;
            let pending_approvals = state.store.approvals_pending(Some(id)).map_err(internal)?;
            ok(ThreadsGetResult {
                notes: state.store.thread_notes(thread.id).map_err(internal)?,
                thread,
                transcript,
                next_before_seq,
                pending_approvals,
                runtime_tasks: side.runtime_tasks.clone(),
                provider_usage: side.provider_usage.clone(),
                provider_commands: side.provider_commands.clone(),
                pending_questions: side.pending_questions.clone(),
            })
        }
        ThreadsRecoverOmpAnswer::NAME => {
            let p: ThreadsRecoverOmpAnswerParams = parse(params)?;
            ok(state.orchestrator.recover_omp_answer(p).await.map_err(bad)?)
        }
        ThreadsToolOutput::NAME => {
            let p: ThreadsToolOutputParams = parse(params)?;
            if p.start_seq.is_some_and(|seq| seq < 0) || p.through_seq.is_some_and(|seq| seq < 0) {
                return Err(RpcError::invalid_params("use nonnegative tool and snapshot sequences"));
            }
            let thread = state.store.thread_get(p.thread_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("thread"))?;
            let through_seq = p.through_seq.unwrap_or(thread.last_seq).min(thread.last_seq);
            let include_tool_stream = p.include_tool_stream.unwrap_or(false);
            let result = state
                .store
                .tool_call_result_through(p.thread_id, &p.tool_call_id, p.start_seq, through_seq, include_tool_stream)
                .map_err(internal)?
                .ok_or_else(|| RpcError::not_found("tool output"))?;
            ok(ThreadsToolOutputResult {
                output: result.output,
                is_error: result.is_error,
                stream: result.stream,
                stream_omitted: result.stream_omitted,
            })
        }
        _ => {
            *slot = Some(params);
            Err(RpcError::method_not_found(method))
        }
    }
}

async fn dispatch_part_7(state: &AppState, _ctx: &ConnectionCtx, method: &str, slot: &mut Option<Value>) -> Result<Value, RpcError> {
    let params = slot.take().unwrap_or_default();
    match method {
        ThreadsUpdate::NAME => {
            let p: ThreadsUpdateParams = parse(params)?;
            let thread = state.orchestrator.update_session_fields(p).await.map_err(provider_err)?;
            ok(thread)
        }
        ThreadsArchive::NAME => {
            let p: ThreadsArchiveParams = parse(params)?;
            state.orchestrator.archive_thread(p.thread_id).await.map_err(bad)?;
            ok(Empty {})
        }
        ThreadsSend::NAME => {
            let p: ThreadsSendParams = parse(params)?;
            let sent = state.orchestrator.send_client_message(p).await.map_err(|e| {
                let msg = e.to_string();
                if msg.contains("busy") { RpcError::new(codes::THREAD_BUSY, msg) } else { bad(e) }
            })?;
            ok(sent)
        }
        SubagentsSend::NAME => ok(state.orchestrator.send_subagent_message(parse(params)?).await.map_err(bad)?),
        SubagentsMessages::NAME => {
            let p: ThreadsInterruptParams = parse(params)?;
            ok(state.orchestrator.subagent_messages(p.thread_id).await.map_err(bad)?)
        }
        SubagentsSendToParent::NAME => ok(state.orchestrator.send_subagent_message_to_parent(parse(params)?).await.map_err(bad)?),
        ThreadsSteer::NAME => ok(state.orchestrator.steer(parse(params)?).await.map_err(bad)?),
        ThreadNotesGet::NAME => {
            let p: ThreadsInterruptParams = parse(params)?;
            state.store.thread_get(p.thread_id).map_err(internal)?.ok_or_else(|| bad(anyhow::anyhow!("Thread not found.")))?;
            ok(state.store.thread_notes(p.thread_id).map_err(internal)?)
        }
        ThreadNotesSet::NAME => ok(state.orchestrator.set_notes(parse(params)?).map_err(note_err)?),
        NotesList::NAME => {
            let orchestrator = state.orchestrator.clone();
            let (notes, history_since) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                Ok((orchestrator.notes_list()?, orchestrator.notes_history_since()?))
            })
            .await
            .map_err(internal)?
            .map_err(note_err)?;
            let p: NotesListParams = parse_or_default(params)?;
            ok(crate::network::federated::notes_list(state, p.machine, NotesListResult { notes, history_since, federation: None }).await?)
        }
        NotesGet::NAME => {
            let (orchestrator, params) = (state.orchestrator.clone(), parse_or_default(params)?);
            let note = tokio::task::spawn_blocking(move || orchestrator.note_get(params)).await.map_err(internal)?.map_err(note_err)?;
            ok(NotesGetResult { note })
        }
        NotesCreate::NAME => ok(state.orchestrator.note_create(parse(params)?).map_err(note_err)?),
        NotesUpdate::NAME => ok(state.orchestrator.note_update(parse(params)?).map_err(note_err)?),
        NotesPin::NAME => {
            let p: NotesPinParams = parse(params)?;
            ok(state.orchestrator.note_pin(p.id, p.pinned).map_err(note_err)?)
        }
        NotesMove::NAME => ok(state.orchestrator.note_move(parse(params)?).map_err(note_err)?),
        NotesDelete::NAME => ok(state.orchestrator.note_delete(parse::<NotesIdParams>(params)?.id).map_err(note_err)?),
        NotesRestore::NAME => ok(state.orchestrator.note_restore(parse::<NotesIdParams>(params)?.id).map_err(note_err)?),
        NotesPurge::NAME => {
            state.orchestrator.note_purge(parse::<NotesIdParams>(params)?.id).map_err(note_err)?;
            ok(Empty {})
        }
        NotesSearch::NAME => {
            let p: NotesSearchParams = parse(params)?;
            let (p_machine, p_query, p_limit) = (p.machine.clone(), p.query.clone(), p.limit);
            let orchestrator = state.orchestrator.clone();
            let results = tokio::task::spawn_blocking(move || orchestrator.notes_search(&p.query, p.limit))
                .await
                .map_err(internal)?
                .map_err(note_err)?;
            ok(crate::network::federated::notes_search(
                state,
                p_machine,
                &p_query,
                p_limit,
                NotesSearchResult { results, federation: None },
            )
            .await?)
        }
        _ => {
            *slot = Some(params);
            Err(RpcError::method_not_found(method))
        }
    }
}

async fn dispatch_part_8(state: &AppState, ctx: &ConnectionCtx, method: &str, slot: &mut Option<Value>) -> Result<Value, RpcError> {
    let params = slot.take().unwrap_or_default();
    match method {
        TaskItemsList::NAME => {
            let p: TaskItemsListParams = parse_or_default(params)?;
            let orchestrator = state.orchestrator.clone();
            let tasks = tokio::task::spawn_blocking(move || orchestrator.task_items_list()).await.map_err(internal)?.map_err(task_err)?;
            ok(crate::network::federated::task_items_list(state, p.machine, TaskItemsListResult { tasks, federation: None }).await?)
        }
        TaskItemsGet::NAME => {
            ok(TaskItemsGetResult { task: state.orchestrator.task_item_get(parse_or_default(params)?).map_err(task_err)? })
        }
        TaskItemsCreate::NAME => ok(state.orchestrator.task_item_create(parse(params)?).map_err(task_err)?),
        TaskItemsUpdate::NAME => ok(state.orchestrator.task_item_update(parse(params)?).map_err(task_err)?),
        TaskItemsDelete::NAME => {
            state.orchestrator.task_item_delete(parse::<TaskItemsIdParams>(params)?.id).map_err(task_err)?;
            ok(Empty {})
        }
        TaskItemsRestore::NAME => ok(state.orchestrator.task_item_restore(parse::<TaskItemsIdParams>(params)?.id).map_err(task_err)?),
        TaskItemsSend::NAME => ok(state.orchestrator.task_item_send(parse(params)?).await.map_err(task_err)?),
        TaskItemsSendBatch::NAME => ok(state.orchestrator.task_items_send_batch(parse(params)?).await.map_err(task_err)?),
        TaskItemsFollowup::NAME => ok(state.orchestrator.task_item_followup(parse(params)?).await.map_err(task_err)?),
        QueueUpdate::NAME => {
            state.orchestrator.update_queued(parse(params)?).map_err(bad)?;
            ok(Empty {})
        }
        QueueAdd::NAME => {
            state.orchestrator.enqueue(parse(params)?).map_err(bad)?;
            ok(Empty {})
        }
        QueueList::NAME => {
            let p: QueueListParams = parse(params)?;
            ok(QueueListResult { messages: state.store.queue_list(p.thread_id).map_err(internal)? })
        }
        QueueRemove::NAME => {
            let p: QueueRemoveParams = parse(params)?;
            state.orchestrator.remove_queued(p.thread_id, p.id).map_err(bad)?;
            ok(Empty {})
        }
        ThreadsRelease::NAME => {
            let p: ThreadsInterruptParams = parse(params)?;
            state.orchestrator.release_session(p.thread_id).await.map_err(bad)?;
            ok(Empty {})
        }
        ThreadsAnswer::NAME => {
            state.orchestrator.answer_questions(parse(params)?).await.map_err(bad)?;
            ok(Empty {})
        }
        ThreadsCompact::NAME => {
            let p: ThreadsInterruptParams = parse(params)?;
            let (turn_id, message_id) = state.orchestrator.send(p.thread_id, UserMessage::text("/compact")).await.map_err(bad)?;
            ok(ThreadsSendResult { turn_id, message_id, queued: false })
        }
        ThreadsInterrupt::NAME => {
            let p: ThreadsInterruptParams = parse(params)?;
            let cause = format!("threads.interrupt from client {:?} (connection {})", ctx.principal.label, ctx.id);
            state.orchestrator.stop_thread(p.thread_id, &cause).await.map_err(bad)?;
            ok(Empty {})
        }
        ThreadMessagesList::NAME => {
            let p: ThreadMessagesListParams = parse(params)?;
            let messages = state.store.thread_message_list_for_thread(p.thread_id, p.states.as_deref(), 200).map_err(internal)?;
            ok(ThreadMessagesListResult { messages })
        }
        ThreadMessagesDeliver::NAME => {
            let p: ThreadMessageIdParams = parse(params)?;
            ok(state.orchestrator.thread_message_deliver(p.message_id).await.map_err(bad)?)
        }
        ThreadMessagesDismiss::NAME => {
            let p: ThreadMessageIdParams = parse(params)?;
            ok(state.orchestrator.thread_message_dismiss(p.message_id).await.map_err(bad)?)
        }
        DelegationsWorktreeRemove::NAME => {
            let p: DelegationsWorktreeRemoveParams = parse(params)?;
            ok(state.orchestrator.delegation_worktree_remove(p.thread_id, p.force).await.map_err(bad)?)
        }
        DelegationsRemoteList::NAME => {
            let p: DelegationsRemoteListParams = parse(params)?;
            ok(DelegationsRemoteListResult { children: state.orchestrator.remote_children_list(p.parent_thread_id).map_err(bad)? })
        }
        CoordinatorGet::NAME => {
            let p: CoordinatorGetParams = parse(params)?;
            ok(state.orchestrator.coordinator_get(p.project_id).map_err(bad)?)
        }
        CoordinatorOpen::NAME => ok(state.orchestrator.coordinator_open(parse(params)?).await.map_err(bad)?),
        NotesTidyOffer::NAME => {
            let orchestrator = state.orchestrator.clone();
            let params: NotesTidyParams = parse(params)?;
            ok(tokio::task::spawn_blocking(move || orchestrator.notes_tidy_offer(params)).await.map_err(internal)?.map_err(bad)?)
        }
        _ => {
            *slot = Some(params);
            Err(RpcError::method_not_found(method))
        }
    }
}

async fn dispatch_part_9(state: &AppState, _ctx: &ConnectionCtx, method: &str, slot: &mut Option<Value>) -> Result<Value, RpcError> {
    let params = slot.take().unwrap_or_default();
    match method {
        NotesTidySnooze::NAME => {
            let orchestrator = state.orchestrator.clone();
            let params: NotesTidyParams = parse(params)?;
            ok(tokio::task::spawn_blocking(move || orchestrator.notes_tidy_snooze(params)).await.map_err(internal)?.map_err(bad)?)
        }
        NotesTidyStart::NAME => ok(state.orchestrator.notes_tidy_start(parse(params)?).await.map_err(bad)?),
        CoordinatorSetup::NAME => ok(state.orchestrator.coordinator_setup(parse(params)?).await.map_err(bad)?),
        CoordinatorSwitchHarness::NAME => ok(state.orchestrator.coordinator_switch_harness(parse(params)?).await.map_err(bad)?),
        CoordinatorRefreshContext::NAME => ok(state.orchestrator.coordinator_refresh_context(parse(params)?).await.map_err(bad)?),
        CoordinatorEditHandoff::NAME => {
            state.orchestrator.coordinator_edit_handoff(parse(params)?).await.map_err(bad)?;
            ok(Empty {})
        }
        TasksList::NAME => {
            let p: TasksListParams = parse(params)?;
            let mut tasks = state.store.runtime_tasks_for_thread(p.thread_id).map_err(internal)?;
            if !p.include_completed {
                tasks.retain(|task| task.status.is_active());
            }
            ok(TasksListResult { tasks })
        }
        TaskStop::NAME => {
            let p: TaskControlParams = parse(params)?;
            ok(state.orchestrator.stop_runtime_task(p.thread_id, &p.task_id).await.map_err(provider_err)?)
        }
        TaskBackground::NAME => {
            let p: TaskControlParams = parse(params)?;
            ok(state.orchestrator.background_runtime_task(p.thread_id, &p.task_id).await.map_err(provider_err)?)
        }
        ThreadsRegenerateTitle::NAME => {
            let p: ThreadsRegenerateTitleParams = parse(params)?;
            let thread = state.store.thread_get(p.thread_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("thread"))?;
            let events = state.store.events_for_thread(p.thread_id).map_err(internal)?;
            let first = events.iter().find_map(|e| match &e.payload {
                EventPayload::TurnStarted { message, .. } => Some(message.clone()),
                _ => None,
            });
            let generated = match &first {
                Some(m) => state.orchestrator.generate_title(&thread, m).await.map_err(provider_err)?,
                None => None,
            };
            let title = generated
                .or_else(|| first.map(|m| crate::orchestrator::title_from_message(&m)))
                .unwrap_or_else(|| crate::orchestrator::DEFAULT_TITLE.into());
            let thread = state
                .orchestrator
                .update_thread_fields(ThreadsUpdateParams {
                    thread_id: p.thread_id,
                    title: Some(title),
                    pinned: None,
                    permission_mode: None,
                    model: None,
                    effort: None,
                })
                .map_err(bad)?;
            ok(thread)
        }
        ThreadsCheckpoints::NAME => {
            let p: ThreadsCheckpointsParams = parse(params)?;
            ok(ThreadsCheckpointsResult { checkpoints: state.store.checkpoints_for_thread(p.thread_id).map_err(internal)? })
        }
        ThreadsDiff::NAME => {
            let p: ThreadsDiffParams = parse(params)?;
            ok(state.orchestrator.diff(p.thread_id, p.turn_id, p.include_patch, p.path.as_deref()).await.map_err(bad)?)
        }
        ThreadsRevert::NAME => {
            let p: ThreadsRevertParams = parse(params)?;
            let (commit, conversation_rewound) = state.orchestrator.revert(p.thread_id, p.turn_id).await.map_err(bad)?;
            ok(ThreadsRevertResult { commit, conversation_rewound })
        }
        TerminalsCreate::NAME => {
            let p: TerminalsCreateParams = parse(params)?;
            let cwd = match (p.cwd, p.thread_id) {
                (Some(c), _) => c,
                (None, Some(t)) => state.store.thread_get(t).map_err(internal)?.ok_or_else(|| RpcError::not_found("thread"))?.cwd,
                (None, None) => return Err(RpcError::invalid_params("thread_id or cwd is required")),
            };
            let t = state.terminals.create(p.terminal_id, p.thread_id, cwd, p.cols, p.rows, p.command).map_err(internal)?;
            ok(t.info())
        }
        _ => {
            *slot = Some(params);
            Err(RpcError::method_not_found(method))
        }
    }
}

async fn dispatch_part_10(state: &AppState, ctx: &ConnectionCtx, method: &str, slot: &mut Option<Value>) -> Result<Value, RpcError> {
    let params = slot.take().unwrap_or_default();
    match method {
        TerminalsList::NAME => {
            let p: TerminalsListParams = parse_or_default(params)?;
            ok(TerminalsListResult { terminals: state.terminals.list(p.thread_id) })
        }
        TerminalsInput::NAME => {
            use base64::Engine;
            let p: TerminalsInputParams = parse(params)?;
            let t = state.terminals.get(p.terminal_id).ok_or_else(|| RpcError::not_found("terminal"))?;
            let bytes = base64::engine::general_purpose::STANDARD.decode(&p.data).map_err(RpcError::invalid_params)?;
            t.write(&bytes).map_err(internal)?;
            ok(Empty {})
        }
        TerminalsResize::NAME => {
            let p: TerminalsResizeParams = parse(params)?;
            let t = state.terminals.get(p.terminal_id).ok_or_else(|| RpcError::not_found("terminal"))?;
            t.resize(p.cols, p.rows).map_err(internal)?;
            ok(Empty {})
        }
        TerminalsClose::NAME => {
            let p: TerminalsCloseParams = parse(params)?;
            ctx.unsubscribe_terminal(p.terminal_id).await;
            state.terminals.close(p.terminal_id).map_err(bad)?;
            ok(Empty {})
        }
        TerminalsSubscribe::NAME => {
            let p: TerminalsSubscribeParams = parse(params)?;
            let t = state.terminals.get(p.terminal_id).ok_or_else(|| RpcError::not_found("terminal"))?;
            ctx.subscribe_terminal(t, p.replay).await;
            ok(Empty {})
        }
        TerminalsUnsubscribe::NAME => {
            let p: TerminalsCloseParams = parse(params)?;
            ctx.unsubscribe_terminal(p.terminal_id).await;
            ok(Empty {})
        }
        SettingsGet::NAME => ok(state.settings.get()),
        ComputerStatusGet::NAME => ok(crate::computer::status(state.orchestrator.computer()).await),
        ComputerFrameGet::NAME => {
            let p: ComputerFrameParams = parse(params)?;
            let (frame, paused) = state.orchestrator.computer().frame(p.thread_id, p.after);
            ok(ComputerFrameResult { frame, paused })
        }
        ComputerSetup::NAME => {
            let p: ComputerSetupParams = parse(params)?;
            ok(crate::computer::setup(state.orchestrator.computer(), p.action).await.map_err(internal)?)
        }
        CursorStatusGet::NAME => ok(crate::cursor_setup::status(state).await),
        CursorSetup::NAME => {
            let p: CursorSetupParams = parse(params)?;
            ok(crate::cursor_setup::setup(state, p.action).await.map_err(bad)?)
        }
        ComputerNotesList::NAME => ok(ComputerNotesResult { notes: state.orchestrator.computer().notes().list() }),
        ComputerNoteSet::NAME => {
            let p: ComputerNoteSetParams = parse(params)?;
            let notes = state.orchestrator.computer().notes();
            let app = match p.app {
                Some(app) => Some(app),
                None if notes.get(&p.bundle_id).is_none() => crate::computer::app_name(&p.bundle_id).await,
                None => None,
            };
            notes.set(&p.bundle_id, app.as_deref(), &p.text).map_err(bad)?;
            ok(ComputerNotesResult { notes: notes.list() })
        }
        SecretsTarget::NAME => {
            let p: SecretsTargetParams = parse(params)?;
            let cwd = secrets_workspace(state, &p.workspace)?;
            ok(crate::secrets::check_pasted(std::path::Path::new(&cwd), p.file.as_deref(), &p.env_key).await.map_err(bad)?)
        }
        SecretsStore::NAME => {
            let p: SecretsStoreParams = parse(params)?;
            let cwd = secrets_workspace(state, &p.workspace)?;
            ok(crate::secrets::store_pasted(std::path::Path::new(&cwd), p.file.as_deref(), &p.env_key, p.value, p.add_to_gitignore)
                .await
                .map_err(bad)?)
        }
        _ => {
            *slot = Some(params);
            Err(RpcError::method_not_found(method))
        }
    }
}

async fn dispatch_part_11(state: &AppState, ctx: &ConnectionCtx, method: &str, slot: &mut Option<Value>) -> Result<Value, RpcError> {
    let params = slot.take().unwrap_or_default();
    match method {
        SettingsUpdate::NAME => {
            let p: SettingsUpdateParams = parse(params)?;
            // Antigravity's MCP list is machine-wide: change it only when the switch flips, and refuse
            // the switch when someone else's `kybern` entry holds the name. A missing `agy` is not an error.
            let was_on = state.settings.get().antigravity.kybern_tools;
            let wants = p.settings.antigravity.kybern_tools;
            if wants && !was_on {
                match crate::antigravity_mcp::turn_on(&p.settings).await {
                    Err(crate::antigravity_mcp::Failure::Foreign) => return Err(RpcError::invalid_params(crate::antigravity_mcp::FOREIGN)),
                    Err(error) => tracing::warn!(%error, "could not add Kybern's Antigravity MCP entry"),
                    Ok(()) => {}
                }
            }
            let updated = state.settings.set(p.settings).map_err(internal)?;
            if was_on
                && !wants
                && let Err(error) = crate::antigravity_mcp::turn_off(&updated).await
            {
                tracing::warn!(%error, "could not remove Kybern's Antigravity MCP entry");
            }
            state.orchestrator.usage().settings_changed();
            ok(updated)
        }
        SettingsNetworkViewMethod::NAME => ok(crate::settings_share::network_view(state).await),
        SettingsShare::NAME => ok(crate::settings_share::share(state, parse(params)?).await?),
        PluginsInventory::NAME => ok(crate::plugins::inventory(state).await?),
        PluginsNetworkViewMethod::NAME => ok(crate::network::plugin_sync::network_view(state).await?),
        PluginsCopy::NAME => ok(crate::network::plugin_sync::copy(state, parse(params)?).await?),
        SkillsInventoryMethod::NAME => ok(crate::network::skill_sync::inventory(state).await?),
        SkillsNetworkViewMethod::NAME => ok(crate::network::skill_sync::network_view(state).await?),
        SkillsCopy::NAME => ok(crate::network::skill_sync::copy(state, parse(params)?).await?),
        UsageSummary::NAME => {
            let p: UsageSummaryParams = parse_or_default(params)?;
            let rows = state.store.usage_summary(p.since, p.group_by).map_err(internal)?;
            let mut total = UsageRow { key: "total".into(), turns: 0, usage: Usage::default(), cost_usd: 0.0 };
            for r in &rows {
                total.turns += r.turns;
                total.usage.add(&r.usage);
                total.cost_usd += r.cost_usd;
            }
            ok(UsageSummaryResult { rows, total })
        }
        UsageLimits::NAME => {
            let p: UsageLimitsParams = parse_or_default(params)?;
            ok(state.orchestrator.usage().limits(p).await)
        }
        PairingCreate::NAME => {
            let p: PairingCreateParams = parse_or_default(params)?;
            let endpoints = crate::access::endpoints(state).await;
            let (code, expires_at) = state.pairing.create(p.label).map_err(bad)?;
            ok(PairingCreateResult { code, expires_at, endpoints })
        }
        ExposureGet::NAME => ok(crate::access::exposure(state).await),
        ExposureSet::NAME => {
            let p: ExposureSetParams = parse(params)?;
            ok(crate::access::set_exposure(state, p.tailscale).await.map_err(|e| internal(format!("{e:#}")))?)
        }
        TokensList::NAME => ok(TokensListResult { tokens: state.store.tokens_list().map_err(internal)? }),
        TokensRevoke::NAME => {
            let p: TokensRevokeParams = parse(params)?;
            if p.token_id == ctx.principal.token_id {
                return Err(RpcError::invalid_params("a token cannot revoke itself"));
            }
            let device = state.store.token_device(p.token_id).map_err(internal)?;
            state.store.token_revoke(p.token_id).map_err(internal)?;
            let _ = state.revoked_tokens.send(p.token_id);
            state.previews.revoke_principal(p.token_id);
            state.network.pages.revoke_principal(p.token_id);
            // A device is revoked on every machine, not only the one it was revoked on (§3.4).
            if let Some(device) = device {
                crate::network::client_grant::revoke_device_everywhere(state, &device).await;
            }
            ok(Empty {})
        }
        FilesSearch::NAME => {
            let p: FilesSearchParams = parse(params)?;
            let project = state.store.project_get(p.project_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("project"))?;
            let root = std::path::PathBuf::from(&project.path);
            let files = state.file_indexes.list(&root).await.map_err(internal)?;
            let total = files.len() as u32;
            let files = crate::files::rank(&files, &p.query, p.limit as usize);
            ok(FilesSearchResult { files, total })
        }
        FilesList::NAME => {
            let p: FilesListParams = parse(params)?;
            let project = state.store.project_get(p.project_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("project"))?;
            let entries = crate::files::list_dir(std::path::Path::new(&project.path), &p.path).await.map_err(bad)?;
            ok(FilesListResult { entries })
        }
        FilesRead::NAME => {
            let p: FilesReadParams = parse(params)?;
            let project = state.store.project_get(p.project_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("project"))?;
            ok(crate::files::read_file(std::path::Path::new(&project.path), &p.path, p.max_bytes, p.if_version.as_deref())
                .await
                .map_err(bad)?)
        }
        FilesWrite::NAME => {
            let p: FilesWriteParams = parse(params)?;
            let project = state.store.project_get(p.project_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("project"))?;
            ok(crate::files::write_file(std::path::Path::new(&project.path), &p.path, p.content, p.expected_version.as_deref())
                .await
                .map_err(write_error)?)
        }
        _ => {
            *slot = Some(params);
            Err(RpcError::method_not_found(method))
        }
    }
}

async fn dispatch_part_12(state: &AppState, ctx: &ConnectionCtx, method: &str, slot: &mut Option<Value>) -> Result<Value, RpcError> {
    let params = slot.take().unwrap_or_default();
    match method {
        ThreadFileRead::NAME => {
            let p: ThreadFileReadParams = parse(params)?;
            let thread = state.store.thread_get(p.thread_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("thread"))?;
            ok(crate::files::read_thread_file(
                &state.file_indexes,
                std::path::Path::new(&thread.cwd),
                &p.path,
                p.max_bytes,
                p.if_version.as_deref(),
            )
            .await
            .map_err(bad)?)
        }
        ThreadFileWrite::NAME => {
            let p: ThreadFileWriteParams = parse(params)?;
            let thread = state.store.thread_get(p.thread_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("thread"))?;
            ok(crate::files::write_thread_file(std::path::Path::new(&thread.cwd), &p.path, p.content, p.expected_version.as_deref())
                .await
                .map_err(write_error)?)
        }
        ThreadFilesResolve::NAME => {
            let p: ThreadFilesResolveParams = parse(params)?;
            if p.paths.len() > crate::files::MAX_RESOLVE_PATHS {
                return Err(RpcError::invalid_params(format!("Send at most {} paths at a time.", crate::files::MAX_RESOLVE_PATHS)));
            }
            let thread = state.store.thread_get(p.thread_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("thread"))?;
            let paths = crate::files::resolve_thread_files(&state.file_indexes, std::path::Path::new(&thread.cwd), &p.paths)
                .await
                .map_err(internal)?;
            ok(ThreadFilesResolveResult { paths })
        }
        HtmlPublish::NAME => {
            let p: HtmlPublishParams = parse(params)?;
            let turn = state
                .store
                .visual_latest_turn(p.thread_id)
                .map_err(internal)?
                .ok_or_else(|| RpcError::invalid_params("Send a message in this thread before publishing a visual."))?;
            if p.await_answer == Some(true) || p.answer_schema.is_some() {
                return Err(RpcError::invalid_params(
                    "Only an agent's kybern_html_publish can wait for an answer. Publish without await_answer.",
                ));
            }
            let event = crate::visuals::publish(&state.store, &state.paths, p.thread_id, turn, &p.html, &p.title, p.height, None)
                .await
                .map_err(bad)?;
            let EventPayload::HtmlPublished { visual } = &event.payload else { unreachable!() };
            let result = HtmlPublishResult { visual: visual.clone() };
            let _ = state.events.send(event);
            ok(result)
        }
        HtmlPreview::NAME => {
            let p: HtmlPreviewParams = parse(params)?;
            state.store.thread_get(p.thread_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("thread"))?;
            ok(crate::visuals::preview(&state.paths, p).await.map_err(bad)?)
        }
        PreviewOpen::NAME => {
            let p: PreviewOpenParams = parse(params)?;
            let thread = state.store.thread_get(p.thread_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("thread"))?;
            ok(crate::previews::open(state, Some(ctx.principal.token_id), &thread, &p).await.map_err(preview_error)?)
        }
        PreviewServerOpen::NAME => {
            let p: PreviewServerOpenParams = parse(params)?;
            ok(crate::previews::open_server(state, ctx.principal.token_id, &p).await.map_err(preview_error)?)
        }
        PreviewClose::NAME => {
            let p: PreviewCloseParams = parse(params)?;
            state.previews.revoke(&p.ticket);
            ok(Empty {})
        }
        HtmlRead::NAME => ok(crate::visuals::read(&state.store, parse(params)?).map_err(bad)?),
        HtmlSnapshot::NAME => {
            let p: HtmlSnapshotParams = parse(params)?;
            state.store.thread_get(p.thread_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("thread"))?;
            ok(crate::visuals::snapshot(&state.store, &state.paths, p).await.map_err(bad)?)
        }
        HtmlFrame::NAME => ok(crate::visuals::issue(&state.store, parse(params)?).map_err(bad)?),
        HtmlRevoke::NAME => {
            crate::visuals::revoke(parse(params)?);
            ok(Empty {})
        }
        _ => {
            *slot = Some(params);
            Err(RpcError::method_not_found(method))
        }
    }
}

async fn dispatch_part_13(state: &AppState, _ctx: &ConnectionCtx, method: &str, slot: &mut Option<Value>) -> Result<Value, RpcError> {
    let params = slot.take().unwrap_or_default();
    match method {
        PreviewProbe::NAME => {
            let p: PreviewProbeParams = parse(params)?;
            match crate::previews::probe::probe(&p.url).await {
                Ok(result) => ok(result),
                Err(_) => Err(RpcError::new(codes::INVALID_PARAMS, "Preview addresses must be on this computer or its private network.")
                    .with_data(serde_json::json!({ "reason": "invalid_address" }))),
            }
        }
        PreviewServersList::NAME => {
            let p: PreviewServersListParams = parse_or_default(params)?;
            ok(crate::previews::discovery::servers_list(state, p.thread_id).await.map_err(bad)?)
        }
        ArtifactsList::NAME => {
            let p: ArtifactsListParams = parse(params)?;
            let _thread = state.store.thread_get(p.thread_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("thread"))?;
            let limit = p.limit.clamp(1, 100) as usize;
            let mut artifacts = state.store.artifact_calls(p.thread_id, p.before_seq, limit as u32 + 1).map_err(internal)?;
            let more = artifacts.len() > limit;
            artifacts.truncate(limit);
            let next_before_seq = more.then(|| artifacts.last().unwrap().seq);
            ok(ArtifactsListResult { artifacts, next_before_seq })
        }
        ArtifactRead::NAME => ok(crate::artifacts::read(state, parse(params)?).await.map_err(bad)?),
        ArtifactPreview::NAME => ok(ArtifactPreviewResult { ticket: crate::artifacts::issue(state, parse(params)?).await.map_err(bad)? }),
        IntegrationsList::NAME => ok(crate::integrations::list(state, parse(params)?).await.map_err(bad)?),
        IntegrationChange::NAME => ok(crate::integrations::change(state, parse(params)?).await.map_err(bad)?),
        IntegrationLogin::NAME => ok(crate::integrations::login(state, parse(params)?).await.map_err(bad)?),
        SkillsList::NAME => {
            let p: SkillsListParams = parse(params)?;
            let project = state.store.project_get(p.project_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("project"))?;
            let cwd = std::path::Path::new(&project.path);
            let settings = state.settings.get();
            let mut provider_settings = settings.providers.get(&p.provider).cloned().unwrap_or_default();
            crate::provider_assets::prepare(&provider_settings, p.provider).map_err(bad)?;
            if p.provider == ProviderKind::Omp
                && let Some(profile) = provider_settings.project_profiles.get(&project.path).cloned()
            {
                provider_settings.env.insert("OMP_PROFILE".into(), profile);
            }
            let instance = crate::provider_accounts::resolve(&provider_settings, cwd.to_str(), p.instance.as_deref());
            provider_settings.env = crate::provider_accounts::environment(&provider_settings, p.provider, &instance).map_err(bad)?;
            let binary = provider_settings.binary.as_ref().map(std::path::PathBuf::from);
            let skills = match p.provider {
                ProviderKind::ClaudeCode => {
                    let mut skills = crate::skills::list(cwd, p.provider, &provider_settings.env).await.map_err(internal)?;
                    let context = kybern_drivers::ProbeContext {
                        binary: binary.clone(),
                        cwd: Some(cwd.to_path_buf()),
                        env: provider_settings.env.clone(),
                    };
                    if let Ok(roots) = kybern_drivers::claude_integrations::skill_roots(&context).await {
                        skills.extend(crate::skills::plugin_skills(roots).await.map_err(internal)?);
                    }
                    skills
                }
                ProviderKind::Codex => match kybern_drivers::codex::discover_skills(cwd, binary.as_ref(), &provider_settings.env).await {
                    Some(skills) => skills,
                    None => crate::skills::list(cwd, p.provider, &provider_settings.env).await.map_err(internal)?,
                },
                ProviderKind::Opencode => {
                    match kybern_drivers::opencode::discover_skills(cwd, binary.as_ref(), &provider_settings.env).await {
                        Some(skills) => skills,
                        None => crate::skills::list(cwd, p.provider, &provider_settings.env).await.map_err(internal)?,
                    }
                }
                _ => crate::skills::list(cwd, p.provider, &provider_settings.env).await.map_err(internal)?,
            };
            let mut skills = skills;
            if state.orchestrator.computer().offered_to(p.provider) {
                skills.insert(0, crate::computer::ComputerUse::mention_skill());
            }
            ok(SkillsListResult { skills })
        }
        GitStatusMethod::NAME => {
            let p: GitStatusParams = parse(params)?;
            let t = state.store.thread_get(p.thread_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("thread"))?;
            state.orchestrator.refresh_project_git(t.project_id).await.map_err(internal)?;
            if !t.members.is_empty() {
                return ok(state.orchestrator.group_git_status(&t).await.map_err(internal)?);
            }
            ok(crate::github::status(std::path::Path::new(&t.cwd)).await.map_err(internal)?)
        }
        _ => {
            *slot = Some(params);
            Err(RpcError::method_not_found(method))
        }
    }
}

async fn dispatch_part_14(state: &AppState, _ctx: &ConnectionCtx, method: &str, slot: &mut Option<Value>) -> Result<Value, RpcError> {
    let params = slot.take().unwrap_or_default();
    match method {
        GitBranches::NAME => {
            let p: GitBranchesParams = parse(params)?;
            let project = state.store.project_get(p.project_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("project"))?;
            state.orchestrator.refresh_project_git(project.id).await.map_err(internal)?;
            if project.kind == ProjectKind::Group {
                return ok(state
                    .orchestrator
                    .group_branches(&project, p.repo.as_deref(), p.fetch, p.include_remote)
                    .await
                    .map_err(RpcError::invalid_params)?);
            }
            ok(crate::github::branches(std::path::Path::new(&project.path), p.fetch, p.include_remote).await.map_err(internal)?)
        }
        GitCommit::NAME => {
            let p: GitCommitParams = parse(params)?;
            let t = state.store.thread_get(p.thread_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("thread"))?;
            if !t.members.is_empty() {
                if !state.orchestrator.group_has_changes(&t).await {
                    return Err(RpcError::invalid_params("nothing to commit"));
                }
                return ok(state.orchestrator.group_commit(&t, p.message).await.map_err(provider_err)?);
            }
            let cwd = std::path::PathBuf::from(&t.cwd);
            if !crate::github::has_changes(&cwd).await {
                return Err(RpcError::invalid_params("nothing to commit"));
            }
            let message = match p.message {
                Some(m) => m,
                None => state.orchestrator.generate_commit_message(&t).await.map_err(provider_err)?,
            };
            let commit = crate::github::commit_all(&cwd, &message).await.map_err(bad)?;
            ok(GitCommitResult { commit, message, commits: Vec::new() })
        }
        PrCreate::NAME => {
            let p: PrCreateParams = parse(params)?;
            let t = state.store.thread_get(p.thread_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("thread"))?;
            if !t.members.is_empty() {
                return Err(RpcError::invalid_params(
                    "This chat spans several repositories. Use Create pull requests (github.pr.create_group).",
                ));
            }
            let cwd = std::path::PathBuf::from(&t.cwd);
            if !crate::github::gh_available().await {
                return Err(RpcError::new(codes::PROVIDER_UNAVAILABLE, "GitHub CLI (gh) is not installed or not logged in"));
            }
            if p.commit_first && crate::github::has_changes(&cwd).await {
                let message = state.orchestrator.generate_commit_message(&t).await.map_err(provider_err)?;
                crate::github::commit_all(&cwd, &message).await.map_err(bad)?;
            }
            let base = match p.base {
                Some(b) => b,
                None => crate::github::default_base(&cwd).await,
            };
            crate::github::push_current(&cwd).await.map_err(bad)?;
            let (title, body) = match (p.title, p.body) {
                (Some(t), Some(b)) => (t, b),
                (title, body) => {
                    let (gt, gb) = state.orchestrator.generate_pr_text(&t, &base).await.map_err(provider_err)?;
                    (title.unwrap_or(gt), body.unwrap_or(gb))
                }
            };
            ok(crate::github::pr_create(&cwd, &title, &body, &base, p.draft).await.map_err(bad)?)
        }
        PrCreateGroup::NAME => {
            let p: PrCreateGroupParams = parse(params)?;
            if !crate::github::gh_available().await {
                return Err(RpcError::new(codes::PROVIDER_UNAVAILABLE, "GitHub CLI (gh) is not installed or not logged in"));
            }
            ok(state.orchestrator.pr_create_group(p, "gh").await.map_err(bad)?)
        }
        PrList::NAME => {
            let p: PrListParams = parse(params)?;
            let project = state.store.project_get(p.project_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("project"))?;
            if project.kind == ProjectKind::Group {
                return ok(state.orchestrator.group_pr_list(&project, &p.state, p.limit, "gh").await);
            }
            ok(PrListResult {
                pull_requests: crate::github::pr_list(std::path::Path::new(&project.path), &p.state, p.limit).await.map_err(bad)?,
                repos: Vec::new(),
            })
        }
        _ => {
            *slot = Some(params);
            Err(RpcError::method_not_found(method))
        }
    }
}

async fn dispatch_part_15(state: &AppState, ctx: &ConnectionCtx, method: &str, slot: &mut Option<Value>) -> Result<Value, RpcError> {
    let params = slot.take().unwrap_or_default();
    match method {
        PrDetail::NAME => {
            let p: PrDetailParams = parse(params)?;
            let project = state.store.project_get(p.project_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("project"))?;
            let dir = pr_dir(&project, p.repo.as_deref())?;
            let mut detail = crate::github_review::detail(&dir, p.number).await.map_err(bad)?;
            // Group calls always name their member (`pr_dir`); single projects stay untagged.
            detail.pull_request.repo = p.repo;
            ok(detail)
        }
        PrPage::NAME => {
            let p: PrPageParams = parse(params)?;
            let project = state.store.project_get(p.project_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("project"))?;
            let dir = pr_dir(&project, p.repo.as_deref())?;
            ok(crate::github_review::page(&dir, &p).await.map_err(bad)?)
        }
        PrAction::NAME => {
            let p: PrActionParams = parse(params)?;
            let project = state.store.project_get(p.project_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("project"))?;
            let dir = pr_dir(&project, p.repo.as_deref())?;
            if matches!(p.action, PrActionKind::Checkout) {
                if p.repo.is_some() {
                    return Err(RpcError::invalid_params(
                        "A pull request of a group project can't be checked out into a chat yet. Open that repository as its own project.",
                    ));
                }
                state.orchestrator.pr_checkout(&p).await.map_err(bad)?;
            } else {
                crate::github_review::action(&dir, &p).await.map_err(bad)?;
            }
            ok(Empty {})
        }
        ActionsRuns::NAME
        | ActionsRun::NAME
        | ActionsLog::NAME
        | ActionsWorkflows::NAME
        | ActionsWorkflow::NAME
        | ActionsAction::NAME
        | ActionsDispatch::NAME => actions_call(state, "gh", method, params).await,
        GithubStarred::NAME => {
            let _: GithubStarredParams = parse(params)?;
            ok(crate::github::starred_with("gh").await.map_err(bad)?)
        }
        GithubStar::NAME => {
            let _: GithubStarParams = parse(params)?;
            crate::github::star_with("gh").await.map_err(bad)?;
            ok(Empty {})
        }
        WorktreeInspect::NAME => {
            let p: WorktreeInspectParams = parse(params)?;
            ok(state.orchestrator.worktree_inspect(p.thread_id).await.map_err(bad)?)
        }
        WorktreeRemove::NAME => {
            let p: WorktreeRemoveParams = parse(params)?;
            ok(state.orchestrator.worktree_remove(p).await.map_err(bad)?)
        }
        ApprovalsRespond::NAME => {
            let p: ApprovalsRespondParams = parse(params)?;
            state.orchestrator.respond_approval(p.approval_id, p.decision).await.map_err(bad)?;
            ok(Empty {})
        }
        RequestsProgress::NAME => {
            let p: RequestsProgressParams = parse_or_default(params)?;
            let items = state.orchestrator.requests().list(p.thread_id);
            let items =
                if ctx.principal.has(Scope::AccessWrite) { items } else { items.into_iter().map(crate::ws::redact_progress).collect() };
            ok(RequestsProgressResult { items })
        }
        RequestsAct::NAME => {
            let p: RequestsActParams = parse(params)?;
            let act = crate::requests::RequestAct { action: p.action, value: p.value, can_write: ctx.principal.has(Scope::AccessWrite) };
            state.orchestrator.requests().act(p.approval_id, act).map_err(bad)?;
            ok(Empty {})
        }
        ApprovalsList::NAME => {
            let p: ApprovalsListParams = parse_or_default(params)?;
            ok(ApprovalsListResult { approvals: state.store.approvals_pending(p.thread_id).map_err(internal)? })
        }
        // events.subscribe is handled by ws.rs, which acknowledges the
        // subscription before replay and serializes replay with live delivery.
        EventsUnsubscribe::NAME => {
            let p: EventsUnsubscribeParams = parse(params)?;
            ctx.unsubscribe(p.subscription_id).await;
            ok(Empty {})
        }
        EventsRange::NAME => {
            let p: EventsRangeParams = parse(params)?;
            let limit = p.limit.clamp(1, 5000);
            let mut events = state.store.events_after(Some(p.thread_id), p.after_seq, limit + 1).map_err(internal)?;
            let has_more = events.len() > limit as usize;
            events.truncate(limit as usize);
            ok(EventsRangeResult { events, has_more })
        }
        _ => {
            *slot = Some(params);
            Err(RpcError::method_not_found(method))
        }
    }
}

/// The machine utilities (M4): sending files, forwarding ports. Its own slice so a lane that adds methods
/// elsewhere does not touch it.
async fn dispatch_part_utilities(
    state: &AppState,
    _ctx: &ConnectionCtx,
    method: &str,
    slot: &mut Option<Value>,
) -> Result<Value, RpcError> {
    let params = slot.take().unwrap_or_default();
    match method {
        FilesSend::NAME => ok(crate::network::files_push::send(state, parse(params)?)?),
        ClipboardSet::NAME => ok(crate::network::clipboard::handle_set(state, parse(params)?).await?),
        ClipboardGet::NAME => ok(crate::network::clipboard::handle_get(state, parse_or_default(params)?).await?),
        ClipboardStatusMethod::NAME => ok(crate::network::clipboard::status(state)),
        PortsForward::NAME => ok(crate::network::ports::forward(state, parse(params)?).await?),
        PortsList::NAME => ok(crate::network::ports::list(state)),
        PortsStop::NAME => {
            let p: PortsStopParams = parse(params)?;
            ok(crate::network::ports::stop(state, p.id)?)
        }
        _ => {
            *slot = Some(params);
            Err(RpcError::method_not_found(method))
        }
    }
}

async fn dispatch_part_16(state: &AppState, ctx: &ConnectionCtx, method: &str, slot: &mut Option<Value>) -> Result<Value, RpcError> {
    let params = slot.take().unwrap_or_default();
    match method {
        // Remote delegation's outbox (R1).
        OutboxList::NAME => ok(crate::network::outbox::list(state, parse_or_default(params)?)?),
        OutboxRetry::NAME => ok(crate::network::outbox::retry(state, parse(params)?)?),
        OutboxCancel::NAME => ok(crate::network::outbox::cancel(state, parse(params)?)?),
        OutboxEnqueue::NAME => {
            let p: OutboxEnqueueParams = parse(params)?;
            if p.operation.kind == OutboxKind::Fork {
                ok(crate::orchestrator::fork_machine::serve_fork_enqueue(state, p).await?)
            } else if p.operation.kind == OutboxKind::Move {
                ok(crate::orchestrator::move_thread::serve_move_enqueue(state, p).await?)
            } else {
                ok(crate::network::outbox::enqueue(state, p)?)
            }
        }
        // Fork to a machine (F1): the hand-off note to edit, and the fork itself.
        ThreadsForkNote::NAME => ok(state.orchestrator.fork_note(parse(params)?).await?),
        ThreadsForkToMachine::NAME => ok(crate::orchestrator::fork_machine::serve_fork_to_machine(state, params).await?),
        // Continue on another machine (F2): queue the move, and give up one whose outcome is unknown.
        ThreadsMoveToMachine::NAME => ok(crate::orchestrator::move_thread::serve_move_to_machine(state, params).await?),
        ThreadsMoveAbandon::NAME => ok(state.orchestrator.move_abandon(parse(params)?).await?),
        // The machine network (N4). The logic is in `network/`; these arms parse and answer.
        NetworkStatusMethod::NAME => ok(network_status(state).await?),
        NetworkSetEnabled::NAME => {
            let p: NetworkSetEnabledParams = parse(params)?;
            crate::network::manage::set_enabled_guarded(state, p.enabled, p.only_if_unconfigured).await.map_err(JoinError::into_rpc)?;
            ok(network_status(state).await?)
        }
        NetworkAdd::NAME => {
            let p: NetworkFoundParams = parse(params)?;
            ok(crate::network::join::add_found(state, &p.found_id).await.map_err(JoinError::into_rpc)?)
        }
        NetworkCodeCreate::NAME => ok(crate::network::join::create_code(state).await.map_err(JoinError::into_rpc)?),
        NetworkJoin::NAME => {
            let p: NetworkJoinParams = parse(params)?;
            ok(crate::network::join::join_by_code(state, &p.code, &p.address, p.migrate).await.map_err(JoinError::into_rpc)?)
        }
        NetworkRemove::NAME => {
            let p: NetworkRemoveParams = parse(params)?;
            crate::network::removal::remove(state, &p.machine_id, p.stolen, &p.also_remove).await.map_err(JoinError::into_rpc)?;
            ok(network_status(state).await?)
        }
        NetworkPause::NAME => {
            let p: NetworkPauseParams = parse(params)?;
            ok(crate::network::manage::pause(state, &p.machine_id, p.paused).await.map_err(JoinError::into_rpc)?)
        }
        NetworkRename::NAME => {
            let p: NetworkRenameParams = parse(params)?;
            ok(crate::network::manage::rename(state, &p.machine_id, &p.name).await.map_err(JoinError::into_rpc)?)
        }
        NetworkMerge::NAME => {
            let p: NetworkFoundParams = parse(params)?;
            crate::network::join::merge(state, &p.found_id).await.map_err(JoinError::into_rpc)?;
            ok(network_status(state).await?)
        }
        NetworkRejoin::NAME => {
            crate::network::manage::rejoin(state).await.map_err(JoinError::into_rpc)?;
            ok(network_status(state).await?)
        }
        NetworkClientGrant::NAME => {
            let p: NetworkClientGrantParams = parse(params)?;
            ok(crate::network::client_grant::grant(state, &ctx.principal, p).await?)
        }
        NetworkRelayPageOpen::NAME => {
            let p: NetworkRelayPageOpenParams = parse(params)?;
            ok(crate::network::relay_pages::open(state, &ctx.principal, p)?)
        }
        NetworkRelayPageClose::NAME => {
            let p: NetworkRelayPageCloseParams = parse(params)?;
            crate::network::relay_pages::close(state, &ctx.principal, &p.ticket);
            ok(Empty {})
        }
        NetworkRoutes::NAME => {
            let p: NetworkRoutesParams = parse(params)?;
            ok(NetworkRoutesResult { routes: crate::network::client_grant::routes(state, &p.machine_id).await })
        }
        NetworkAllowSet::NAME => {
            let p: NetworkAllowSetParams = parse(params)?;
            ok(crate::network::manage::allow(state, &p.machine_id, &p.capability, p.allowed).await.map_err(JoinError::into_rpc)?)
        }
        NetworkUpdateAll::NAME => ok(crate::network::updates::update_all(state).await),
        NetworkFollowUpdatesSet::NAME => {
            let p: NetworkFollowUpdatesParams = parse(params)?;
            ok(crate::network::updates::set_follow(state, p.enabled).await.map_err(JoinError::into_rpc)?)
        }
        NetworkKeepReachableSet::NAME => {
            let p: NetworkKeepReachableParams = parse(params)?;
            crate::login_service::set_enabled(state, p.enabled)
                .await
                .map_err(|error| RpcError::internal(format!("Kybern could not save the setting: {error:#}")))?;
            ok(crate::network::status::network_status(state)
                .await
                .ok_or_else(|| RpcError::internal("The machine network is unavailable on this machine."))?)
        }
        NetworkDoctor::NAME => {
            let p: NetworkDoctorParams = parse_or_default(params)?;
            ok(crate::network::doctor::report(state, p.machine_id.as_deref()).await)
        }
        MachinesInfo::NAME => ok(crate::network::machine_info::info(state).await),
        // Daemon-to-daemon methods are answered on a session (`network::peers::dispatch`), never here.
        PeerOpsSync::NAME
        | PeerOpsPush::NAME
        | PeerPresenceMethod::NAME
        | PeerClientTokenMint::NAME
        | PeerDeviceRevoke::NAME
        | PeerRelayOpen::NAME
        | PeerRelayData::NAME
        | PeerRelayClose::NAME
        | PeerUpdateStart::NAME => Err(RpcError::new(codes::NOT_AVAILABLE, format!("{method} is only available between machines."))),
        _ => {
            *slot = Some(params);
            Err(RpcError::method_not_found(method))
        }
    }
}

/// The login broker and pushed keys (S4). The logic is in `network/broker.rs`; these arms parse and answer.
async fn dispatch_part_17(state: &AppState, _ctx: &ConnectionCtx, method: &str, slot: &mut Option<Value>) -> Result<Value, RpcError> {
    let params = slot.take().unwrap_or_default();
    let broker = || {
        state
            .orchestrator
            .login_broker()
            .cloned()
            .ok_or_else(|| RpcError::new(codes::NOT_AVAILABLE, "The machine network is unavailable on this machine."))
    };
    match method {
        LoginBrokerStatusMethod::NAME => ok(broker()?.status(state).await),
        LoginBrokerBind::NAME => ok(broker()?.bind(state, parse(params)?).await?),
        LoginBrokerSet::NAME => ok(crate::network::broker::set_broker(state, parse(params)?).await?),
        SecretsPush::NAME => ok(crate::network::broker::push_secret(state, parse(params)?).await?),
        // Daemon-to-daemon methods are answered on a session (`network::peers::dispatch`), never here.
        PeerLoginBorrow::NAME
        | PeerLoginPush::NAME
        | PeerLoginRelease::NAME
        | PeerLoginStatusMethod::NAME
        | PeerLoginReleaseAll::NAME
        | PeerLoginUsage::NAME
        | PeerSecretsPut::NAME => Err(RpcError::new(codes::NOT_AVAILABLE, format!("{method} is only available between machines."))),
        _ => Err(RpcError::method_not_found(method)),
    }
}

/// The `github.actions.*` methods (ADE-97), run with the `gh` program named here so tests can fake it.
pub(crate) async fn actions_call(state: &AppState, gh: &str, method: &str, params: Value) -> Result<Value, RpcError> {
    use crate::github_actions as actions;
    /// The project folder (or group member) a call names.
    fn dir(state: &AppState, project_id: ProjectId, repo: Option<&str>) -> Result<std::path::PathBuf, RpcError> {
        let project = state.store.project_get(project_id).map_err(internal)?.ok_or_else(|| RpcError::not_found("project"))?;
        pr_dir(&project, repo)
    }
    match method {
        ActionsRuns::NAME => {
            let p: ActionsRunsParams = parse(params)?;
            ok(actions::runs(gh, &dir(state, p.project_id, p.repo.as_deref())?, &p).await.map_err(bad)?)
        }
        ActionsRun::NAME => {
            let p: ActionsRunParams = parse(params)?;
            ok(actions::run_detail(gh, &dir(state, p.project_id, p.repo.as_deref())?, p.run_id, p.attempt).await.map_err(bad)?)
        }
        ActionsLog::NAME => {
            let p: ActionsLogParams = parse(params)?;
            let cache = state.paths.root.join("cache").join("actions-logs");
            ok(actions::log(gh, &dir(state, p.project_id, p.repo.as_deref())?, &cache, &p).await.map_err(bad)?)
        }
        ActionsWorkflows::NAME => {
            let p: ActionsWorkflowsParams = parse(params)?;
            ok(actions::workflows(gh, &dir(state, p.project_id, p.repo.as_deref())?).await.map_err(bad)?)
        }
        ActionsWorkflow::NAME => {
            let p: ActionsWorkflowParams = parse(params)?;
            ok(actions::workflow(gh, &dir(state, p.project_id, p.repo.as_deref())?, &p).await.map_err(bad)?)
        }
        ActionsAction::NAME => {
            let p: ActionsActionParams = parse(params)?;
            actions::action(gh, &dir(state, p.project_id, p.repo.as_deref())?, &p).await.map_err(bad)?;
            ok(Empty {})
        }
        ActionsDispatch::NAME => {
            let p: ActionsDispatchParams = parse(params)?;
            actions::dispatch(gh, &dir(state, p.project_id, p.repo.as_deref())?, &p).await.map_err(bad)?;
            ok(Empty {})
        }
        _ => Err(RpcError::method_not_found(method)),
    }
}

fn ok<T: Serialize>(v: T) -> Result<Value, RpcError> {
    serde_json::to_value(v).map_err(internal)
}

use crate::network::join::JoinError;

async fn network_status(state: &AppState) -> Result<NetworkStatus, RpcError> {
    crate::network::status::network_status(state)
        .await
        .ok_or_else(|| RpcError::new(codes::NOT_AVAILABLE, "The machine network is unavailable on this machine."))
}

fn parse<T: DeserializeOwned>(v: Value) -> Result<T, RpcError> {
    serde_json::from_value(v).map_err(RpcError::invalid_params)
}

fn parse_or_default<T: DeserializeOwned + Default>(v: Value) -> Result<T, RpcError> {
    if v.is_null() { Ok(T::default()) } else { parse(v) }
}

/// The folder a pasted secret is stored under: the thread's workspace, or the project's folder.
fn secrets_workspace(state: &AppState, workspace: &SecretsWorkspace) -> Result<String, RpcError> {
    match (workspace.thread_id, workspace.project_id) {
        (Some(id), None) => Ok(state.store.thread_get(id).map_err(internal)?.ok_or_else(|| RpcError::not_found("thread"))?.cwd),
        (None, Some(id)) => Ok(state.store.project_get(id).map_err(internal)?.ok_or_else(|| RpcError::not_found("project"))?.path),
        _ => Err(RpcError::invalid_params("pass either thread_id or project_id")),
    }
}

/// The folder a pull request call runs in: the project, or the member `repo` of a group project.
fn pr_dir(project: &Project, repo: Option<&str>) -> Result<std::path::PathBuf, RpcError> {
    crate::orchestrator::pr_repo_dir(project, repo).map_err(RpcError::invalid_params)
}

fn internal(e: impl std::fmt::Display) -> RpcError {
    RpcError::internal(e)
}

/// Preview failures keep a stable machine code in `data.code`.
fn preview_error(error: crate::previews::PreviewError) -> RpcError {
    let code = match error.code {
        "not_found" => codes::NOT_FOUND,
        "internal_error" => codes::INTERNAL_ERROR,
        _ => codes::INVALID_PARAMS,
    };
    RpcError::new(code, error.message).with_data(serde_json::json!({ "code": error.code }))
}

/// A save that lost the race with another writer is a conflict, so clients can
/// offer Reload instead of showing a plain error.
fn write_error(e: anyhow::Error) -> RpcError {
    if e.is::<crate::files::WriteConflict>() { RpcError::new(codes::CONFLICT, e.to_string()) } else { bad(e) }
}

/// User-facing failures from the orchestrator: not found, busy, bad input.
fn bad(e: anyhow::Error) -> RpcError {
    let msg = e.to_string();
    if msg.contains("not found") {
        RpcError::new(codes::NOT_FOUND, msg)
    } else if msg.contains("not available") || msg.contains("not found:") {
        RpcError::new(codes::PROVIDER_UNAVAILABLE, msg)
    } else {
        RpcError::new(codes::INVALID_PARAMS, msg)
    }
}

/// Notes failures: a stale revision is a CONFLICT the client can resolve, the rest
/// say what to do next. Anything unexpected is an internal error.
fn note_err(e: anyhow::Error) -> RpcError {
    use kybern_store::NoteError;
    match e.downcast_ref::<NoteError>() {
        Some(NoteError::Conflict) => RpcError::new(codes::CONFLICT, e.to_string()),
        Some(NoteError::NotFound(_)) => RpcError::new(codes::NOT_FOUND, e.to_string()),
        Some(NoteError::Deleted | NoteError::Invalid(_)) => RpcError::new(codes::INVALID_PARAMS, e.to_string()),
        None => internal(e),
    }
}

/// Task failures: a stale revision is a CONFLICT the client can resolve, the rest
/// say what to do next. Notes and threads reached through a task keep their own
/// codes. Anything unexpected is an internal error.
fn task_err(e: anyhow::Error) -> RpcError {
    use kybern_store::TaskError;
    match e.downcast_ref::<TaskError>() {
        Some(TaskError::Conflict) => RpcError::new(codes::CONFLICT, e.to_string()),
        Some(TaskError::NotFound(_)) => RpcError::new(codes::NOT_FOUND, e.to_string()),
        Some(TaskError::Invalid(_)) => RpcError::new(codes::INVALID_PARAMS, e.to_string()),
        None if e.downcast_ref::<kybern_store::NoteError>().is_some() => note_err(e),
        // Starting the agent can fail for provider reasons (unknown model, no binary).
        None => bad(e),
    }
}

fn provider_err(e: anyhow::Error) -> RpcError {
    RpcError::new(codes::PROVIDER_ERROR, e.to_string())
}

pub(crate) fn hostname() -> String {
    std::process::Command::new("hostname")
        .no_window()
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

type PendingKinds = std::collections::HashMap<ThreadId, PendingRequestKind>;

/// Each thread's oldest waiting request kind, and for each delegating ancestor what its
/// delegated descendants wait on (a question outranks an approval).
fn pending_request_kinds(store: &kybern_store::Store) -> anyhow::Result<(PendingKinds, PendingKinds)> {
    let mut own = PendingKinds::new();
    let mut below = PendingKinds::new();
    for approval in store.approvals_pending(None)? {
        let kind = approval.pending_kind();
        own.entry(approval.thread_id).or_insert(kind);
        let mut cursor = store.thread_get(approval.thread_id)?;
        for _ in 0..16 {
            let Some(thread) = cursor.filter(|thread| thread.delegation.is_some()) else { break };
            let Some(parent) = thread.parent_thread_id else { break };
            let entry = below.entry(parent).or_insert(kind);
            *entry = (*entry).max(kind);
            cursor = store.thread_get(parent)?;
        }
    }
    Ok((own, below))
}
