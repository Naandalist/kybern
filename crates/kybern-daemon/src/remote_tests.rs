// End-to-end boundaries: real HTTP, WebSocket authentication, replay and revocation.
use axum::{Router, routing::get};
use futures::{SinkExt, StreamExt};
use kybern_client::{Client, Endpoint};
use kybern_protocol::{methods::*, *};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;

struct Host {
    state: crate::state::AppState,
    url: String,
    root: PathBuf,
    /// The server side runs on the stack the daemon's own runtime gives its threads: an
    /// unoptimized build's RPC frames do not fit the 2 MiB of a test thread, and a test must not
    /// depend on how close to the edge the dispatch happens to be.
    server: Option<tokio::runtime::Runtime>,
}
impl Host {
    async fn start() -> Self {
        let root = std::env::temp_dir().join(format!("kybern-remote-test-{}", Uuid::now_v7()));
        let paths = crate::config::Paths::resolve(Some(root.clone())).unwrap();
        let state = crate::state::AppState::initialize(&paths).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().merge(crate::http::routes()).route("/ws", get(crate::ws::upgrade)).with_state(state.clone());
        let server = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_stack_size(crate::RUNTIME_THREAD_STACK)
            .build()
            .unwrap();
        server.spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, app).await.unwrap()
        });
        Self { state, url, root, server: Some(server) }
    }
    async fn client(&self) -> Client {
        let client = Client::connect(&Endpoint {
            url: format!("{}/ws", self.url.replace("http:", "ws:")),
            token: self.state.bootstrap_token.clone(),
        })
        .await
        .unwrap();
        // The WebSocket upgrade can finish before run() installs its notification receivers.
        // A round trip makes this client ready before another client changes shared state.
        client.call::<DaemonInfoMethod>(Empty {}).await.unwrap();
        client
    }
    fn thread(&self) -> Thread {
        let project = self.state.orchestrator.add_project(self.root.to_string_lossy().into_owned(), Some("Fixture".into()), None).unwrap();
        let now = chrono::Utc::now();
        let thread = Thread {
            id: Uuid::now_v7(),
            project_id: project.id,
            title: "Test".into(),
            provider: ProviderInstance::default_for(ProviderKind::ClaudeCode),
            model: None,
            effort: None,
            permission_mode: PermissionMode::Supervised,
            status: ThreadStatus::Idle,
            worktree: None,
            cwd: project.path,
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
        self.state.store.thread_upsert(&thread).unwrap();
        thread
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.shutdown_background();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn a_device_token_that_starts_like_a_machine_grant_still_connects() {
    let host = Host::start().await;
    // One device token in 4096 begins `kyb_m_`, the machine-grant prefix.
    let token = format!("kyb_m_{}", "q".repeat(37));
    host.state.store.token_insert(Uuid::now_v7(), &crate::auth::hash(&token), "lookalike", &Scope::ALL).unwrap();
    let endpoint = Endpoint { url: format!("{}/ws", host.url.replace("http:", "ws:")), token };
    let client = Client::connect(&endpoint).await.unwrap();
    client.call::<ThreadsList>(ThreadsListParams::default()).await.unwrap();
}

#[tokio::test]
async fn a_device_named_bootstrap_cannot_pair_or_act_as_the_local_app() {
    let host = Host::start().await;
    let http = reqwest::Client::new();
    // Pairing refuses the bootstrap token's name in any case, and leaves the code for a retry.
    let (code, _) = host.state.pairing.create(None).unwrap();
    for name in ["bootstrap", " BootStrap "] {
        let response = http
            .post(format!("{}/pair", host.url))
            .json(&PairRequest { code: code.clone(), device_name: Some(name.into()) })
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST, "{name}");
    }
    assert!(host.state.pairing.create(Some("Bootstrap".into())).is_err());
    let paired: PairResponse = http
        .post(format!("{}/pair", host.url))
        .json(&PairRequest { code, device_name: Some("Phone".into()) })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!paired.token.is_empty());

    // A token that is labelled "bootstrap" (paired before names were checked) with every scope is
    // still not the local app: the check is by the bootstrap token's id.
    let token = crate::auth::generate();
    host.state.store.token_insert(Uuid::now_v7(), &crate::auth::hash(&token), "bootstrap", &Scope::ALL).unwrap();
    let impostor = Client::connect(&Endpoint { url: format!("{}/ws", host.url.replace("http:", "ws:")), token }).await.unwrap();
    let attach = AppUpdaterAttachParams { version: "0.0.1".into(), can_install: true, reason: None };
    let refused = impostor.call::<AppUpdaterAttach>(attach.clone()).await.unwrap_err().to_string();
    assert!(refused.contains("Only the Kybern app on this machine"), "{refused}");
    let report = AppUpdaterReportParams { phase: AppUpdatePhase::Restarting, version: Some("0.0.1".into()), message: None };
    let refused = impostor.call::<AppUpdaterReport>(report).await.unwrap_err().to_string();
    assert!(refused.contains("Only the Kybern app on this machine"), "{refused}");
    let refused = impostor.call::<DaemonShutdown>(Empty {}).await.unwrap_err().to_string();
    assert!(refused.contains("bootstrap client"), "{refused}");
    assert!(!host.state.shutdown.is_cancelled());

    // The real bootstrap token may attach.
    host.client().await.call::<AppUpdaterAttach>(attach).await.unwrap();
}

#[tokio::test]
async fn skill_picker_explicit_account_overrides_project_and_provider_defaults() {
    let host = Host::start().await;
    let thread = host.thread();
    let shared = host.root.join("home/.claude");
    std::fs::create_dir_all(shared.join("skills/review")).unwrap();
    std::fs::write(shared.join("skills/review/SKILL.md"), "---\nname: review\ndescription: Review fixture\n---\nReview").unwrap();
    let account = host.root.join("second");
    let mut settings = host.state.settings.get();
    let provider = settings.providers.entry(ProviderKind::ClaudeCode).or_default();
    provider.env.insert("HOME".into(), host.root.join("home").to_string_lossy().into_owned());
    provider.env.insert("CLAUDE_CONFIG_DIR".into(), shared.to_string_lossy().into_owned());
    provider.binary = Some(host.root.join("no-agent-binary").to_string_lossy().into_owned());
    provider.accounts.insert(
        "second".into(),
        ProviderAccount { name: "Second".into(), directory: account.to_string_lossy().into_owned(), ..Default::default() },
    );
    provider.default_account = Some("second".into());
    provider.project_accounts.insert(thread.cwd.clone(), "second".into());
    host.state.settings.set(settings).unwrap();
    let client = host.client().await;
    // An unavailable named folder must not silently discover the CLI account's
    // skills. This distinguishes explicit CLI selection from following defaults.
    for instance in [Some("default"), Some("second"), None] {
        let result = client
            .call::<SkillsList>(SkillsListParams {
                project_id: thread.project_id,
                provider: ProviderKind::ClaudeCode,
                instance: instance.map(str::to_string),
            })
            .await
            .unwrap();
        assert_eq!(result.skills.iter().any(|skill| skill.name == "review"), instance == Some("default"), "{instance:?}");
    }
    // Once available, all accounts discover the same canonical shared asset.
    std::fs::create_dir_all(&account).unwrap();
    for instance in [Some("default"), Some("second"), None] {
        let result = client
            .call::<SkillsList>(SkillsListParams {
                project_id: thread.project_id,
                provider: ProviderKind::ClaudeCode,
                instance: instance.map(str::to_string),
            })
            .await
            .unwrap();
        let skill = result.skills.iter().find(|skill| skill.name == "review").unwrap();
        assert!(PathBuf::from(&skill.path).starts_with(shared.canonicalize().unwrap()));
    }
    assert!(
        client
            .call::<SkillsList>(SkillsListParams {
                project_id: thread.project_id,
                provider: ProviderKind::ClaudeCode,
                instance: Some("missing".into())
            })
            .await
            .is_err()
    );
}

// Successful cleanup requires the lsof or /proc process-ownership backend.
#[cfg(unix)]
#[tokio::test]
async fn ordinary_worktree_cleanup_rpc_retains_thread_and_source_association() {
    let host = Host::start().await;
    let repo = host.root.join("repository");
    std::fs::create_dir(&repo).unwrap();
    for args in [vec!["init", "-q", "-b", "main"], vec!["commit", "--allow-empty", "-qm", "initial"]] {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.name=Fixture", "-c", "user.email=fixture@example.test"])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
    let project = host.state.orchestrator.add_project(repo.to_string_lossy().into_owned(), Some("Worktree fixture".into()), None).unwrap();
    let client = host.client().await;
    let thread = client
        .call::<ThreadsCreate>(ThreadsCreateParams {
            pin_account: false,
            project_id: Some(project.id),
            provider: ProviderInstance::default_for(ProviderKind::Codex),
            model: None,
            effort: None,
            permission_mode: None,
            use_worktree: Some(true),
            base_branch: None,
            title: Some("Cleanup fixture".into()),
            message: None,
            thread_id: None,
            message_id: None,
            members: None,
            base_branches: None,
        })
        .await
        .unwrap();
    let inspected = client.call::<WorktreeInspect>(WorktreeInspectParams { thread_id: thread.id }).await.unwrap();
    assert!(inspected.eligible);
    let removed =
        client.call::<WorktreeRemove>(WorktreeRemoveParams { thread_id: thread.id, force: false, delete_branch: false }).await.unwrap();
    assert!(!removed.exists);
    let retained = host.state.store.thread_get(thread.id).unwrap().unwrap();
    assert_eq!(retained.cwd, thread.cwd);
    assert_eq!(retained.worktree.as_ref().unwrap().branch, thread.worktree.as_ref().unwrap().branch);
    let events = host.state.store.events_for_thread(thread.id).unwrap();
    assert!(events.iter().any(|event| matches!(&event.payload, EventPayload::WorktreeCleaned { branch, recovery_commit } if branch == &inspected.branch && !recovery_commit.is_empty())));
    let source = kybern_git::Repo::new(&repo);
    assert!(source.rev_parse(&inspected.branch).await.is_ok(), "the branch is kept by default");
}

#[tokio::test]
async fn git_status_refreshes_a_project_registered_before_git_init() {
    let host = Host::start().await;
    let thread = host.thread();
    let client = host.client().await;
    assert!(!host.state.store.project_get(thread.project_id).unwrap().unwrap().is_git);
    let mut changes = host.state.orchestrator.subscribe_projects();

    let initialized =
        tokio::process::Command::new("git").args(["init", "-q", "-b", "main"]).current_dir(&thread.cwd).output().await.unwrap();
    assert!(initialized.status.success());
    let status = client.call::<GitStatusMethod>(GitStatusParams { thread_id: thread.id }).await.unwrap();
    assert!(status.is_git, "the environment panel discovers the repository");
    assert_eq!(status.branch.as_deref(), Some("main"));
    assert!(
        host.state.store.project_get(thread.project_id).unwrap().unwrap().is_git,
        "a live Git read must repair the flag used by project and composer controls"
    );
    let update = changes.try_recv().expect("connected clients must receive the repaired project");
    assert!(update.projects.iter().find(|project| project.id == thread.project_id).unwrap().is_git);
    let announced = next_projects_changed(&client).await;
    assert!(announced.projects.iter().find(|project| project.id == thread.project_id).unwrap().is_git);
    let projects = client.call::<ProjectsList>(ProjectsListParams::default()).await.unwrap();
    assert!(projects.projects.iter().find(|project| project.id == thread.project_id).unwrap().is_git);
    client.call::<GitStatusMethod>(GitStatusParams { thread_id: thread.id }).await.unwrap();
    assert!(matches!(changes.try_recv(), Err(tokio::sync::broadcast::error::TryRecvError::Empty)), "unchanged Git reads stay quiet");
}

#[tokio::test]
async fn projects_list_refreshes_git_without_an_existing_thread() {
    let host = Host::start().await;
    let project = host.state.orchestrator.add_project(host.root.to_string_lossy().into_owned(), None, None).unwrap();
    assert!(!project.is_git);
    let client = host.client().await;
    let initialized =
        tokio::process::Command::new("git").args(["init", "-q", "-b", "main"]).current_dir(&project.path).output().await.unwrap();
    assert!(initialized.status.success());

    let cached = client.call::<ProjectsList>(ProjectsListParams { refresh_git: Some(false) }).await.unwrap();
    assert!(!cached.projects.iter().find(|item| item.id == project.id).unwrap().is_git, "cached boot read does not wait for Git refresh");
    let list = client.call::<ProjectsList>(ProjectsListParams::default()).await.unwrap();
    assert!(list.projects.iter().find(|item| item.id == project.id).unwrap().is_git, "boot must discover repositories without a thread");
    assert!(next_projects_changed(&client).await.projects.iter().find(|item| item.id == project.id).unwrap().is_git);
}

#[tokio::test]
async fn connected_project_git_refresh_detects_init_and_removal_without_a_git_request() {
    let host = Host::start().await;
    let project = host.state.orchestrator.add_project(host.root.to_string_lossy().into_owned(), None, None).unwrap();
    let client = host.client().await;
    let initialized =
        tokio::process::Command::new("git").args(["init", "-q", "-b", "main"]).current_dir(&project.path).output().await.unwrap();
    assert!(initialized.status.success());

    let policy = host.state.settings.get().background;
    crate::maintenance::sweep(&host.state, &policy, false).await.unwrap();
    assert!(next_projects_changed(&client).await.projects.iter().find(|item| item.id == project.id).unwrap().is_git);
    std::fs::remove_dir_all(host.root.join(".git")).unwrap();
    crate::maintenance::sweep(&host.state, &policy, false).await.unwrap();
    assert!(!next_projects_changed(&client).await.projects.iter().find(|item| item.id == project.id).unwrap().is_git);
}

#[tokio::test]
async fn git_status_of_an_independent_thread_checkout_does_not_mark_the_project_as_git() {
    let host = Host::start().await;
    let mut thread = host.thread();
    let checkout = host.root.join("checkout");
    std::fs::create_dir_all(&checkout).unwrap();
    let initialized = tokio::process::Command::new("git").args(["init", "-q", "-b", "main"]).current_dir(&checkout).output().await.unwrap();
    assert!(initialized.status.success());
    thread.cwd = checkout.to_string_lossy().into_owned();
    host.state.store.thread_upsert(&thread).unwrap();
    let client = host.client().await;
    assert!(client.call::<GitStatusMethod>(GitStatusParams { thread_id: thread.id }).await.unwrap().is_git);
    assert!(!host.state.store.project_get(thread.project_id).unwrap().unwrap().is_git, "probe the project path, not the thread's checkout");
}

#[tokio::test]
async fn environments_keep_projects_and_identity_separate() {
    let a = Host::start().await;
    let b = Host::start().await;
    a.thread();
    let ca = a.client().await;
    let cb = b.client().await;
    let ia = ca.call::<DaemonInfoMethod>(Empty {}).await.unwrap();
    let ib = cb.call::<DaemonInfoMethod>(Empty {}).await.unwrap();
    assert_ne!(ia.environment_id, ib.environment_id);
    assert_eq!(ca.call::<ProjectsList>(ProjectsListParams::default()).await.unwrap().projects.len(), 1);
    assert!(cb.call::<ProjectsList>(ProjectsListParams::default()).await.unwrap().projects.is_empty());
    let reopened = kybern_store::Store::open(&a.state.paths.db).unwrap();
    assert_eq!(reopened.meta_get("environment_id").unwrap().unwrap(), ia.environment_id);
    let browsed = ca.call::<ProjectsBrowse>(ProjectsBrowseParams { path: Some(a.root.to_string_lossy().into_owned()) }).await.unwrap();
    assert_eq!(PathBuf::from(browsed.path), a.root.canonicalize().unwrap());
}

#[tokio::test]
async fn browser_ticket_is_single_use_and_revocation_closes_an_existing_socket() {
    let host = Host::start().await;
    let (code, _) = host.state.pairing.create(None).unwrap();
    let http = reqwest::Client::new();
    let paired: PairResponse = http
        .post(format!("{}/pair", host.url))
        .json(&PairRequest { code: code.clone(), device_name: Some("Test phone".into()) })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!paired.scopes.contains(&Scope::AccessWrite));
    assert!(
        http.post(format!("{}/pair", host.url))
            .json(&PairRequest { code, device_name: None })
            .send()
            .await
            .unwrap()
            .status()
            .is_client_error()
    );
    let session: serde_json::Value = http
        .post(format!("{}/session", host.url))
        .bearer_auth(&paired.token)
        .header("Origin", "tauri://localhost")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let url = format!("{}/ws?ticket={}", host.url.replace("http:", "ws:"), session["ticket"].as_str().unwrap());
    let (mut socket, _) = connect_async(&url).await.unwrap();
    assert!(connect_async(&url).await.is_err());
    socket.send(Message::Text(json!({"jsonrpc":"2.0","id":1,"method":"daemon.info","params":{}}).to_string().into())).await.unwrap();
    let message = socket.next().await.unwrap().unwrap().into_text().unwrap();
    assert!(message.contains(&host.state.environment_id));
    let token = host.state.store.tokens_list().unwrap().into_iter().find(|token| token.label == "Test phone").unwrap();
    host.client().await.call::<TokensRevoke>(TokensRevokeParams { token_id: token.id }).await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(3), socket.next()).await.unwrap();
    assert!(closed.is_none() || closed.is_some_and(|frame| matches!(frame, Err(_) | Ok(Message::Close(_)))));
    assert_eq!(
        http.post(format!("{}/session", host.url)).bearer_auth(paired.token).send().await.unwrap().status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn subscription_ack_precedes_all_replayed_events() {
    let host = Host::start().await;
    let thread = host.thread();
    for _ in 0..600 {
        host.state.store.event_append(thread.id, None, EventPayload::ThreadUpdated { thread: thread.clone() }).unwrap();
    }
    let principal = crate::auth::authenticate(&host.state.store, &host.state.bootstrap_token).unwrap().unwrap();
    let ticket = host.state.tickets.create(principal).unwrap();
    let (mut socket, _) = connect_async(format!("{}/ws?ticket={ticket}", host.url.replace("http:", "ws:"))).await.unwrap();
    socket
        .send(Message::Text(json!({"jsonrpc":"2.0","id":7,"method":"events.subscribe","params":{"after_seq":0}}).to_string().into()))
        .await
        .unwrap();
    let first: serde_json::Value = serde_json::from_str(&socket.next().await.unwrap().unwrap().into_text().unwrap()).unwrap();
    assert_eq!(first["id"], 7);
    let subscription = first["result"]["subscription_id"].clone();
    let live = host.state.store.event_append(thread.id, None, EventPayload::ThreadUpdated { thread }).unwrap();
    host.state.events.send(live).unwrap();
    assert_eq!(first["result"]["replay_ready"], true);
    for seq in 1..=601 {
        if seq == 601 {
            let message = tokio::time::timeout(Duration::from_secs(3), socket.next()).await.unwrap().unwrap().unwrap().into_text().unwrap();
            let ready: serde_json::Value = serde_json::from_str(&message).unwrap();
            assert_eq!(ready["method"], EVENTS_READY_NOTIFICATION);
            assert_eq!(ready["params"]["subscription_id"], subscription);
            assert_eq!(ready["params"]["head_seq"], 600);
        }
        let message = tokio::time::timeout(Duration::from_secs(3), socket.next()).await.unwrap().unwrap().unwrap().into_text().unwrap();
        let frame: serde_json::Value = serde_json::from_str(&message).unwrap();
        assert_eq!(frame["params"]["subscription_id"], subscription);
        assert_eq!(frame["params"]["event"]["seq"], seq);
    }
}

#[tokio::test]
async fn compact_event_subscription_defers_large_tool_output_to_exact_rpc() {
    let host = Host::start().await;
    let thread = host.thread();
    let turn = Uuid::now_v7();
    let tool_call_id = "compact:é😀";
    let start = host
        .state
        .store
        .event_append(
            thread.id,
            Some(turn),
            EventPayload::ToolCallStarted {
                call: ToolCall { id: tool_call_id.into(), name: "Read".into(), input: json!({}), parent_id: None },
                origin: EventOrigin::Root,
            },
        )
        .unwrap();
    host.state
        .store
        .event_append(
            thread.id,
            Some(turn),
            EventPayload::ToolCallOutputDelta { tool_call_id: tool_call_id.into(), delta: "stream é😀\n".repeat(4) },
        )
        .unwrap();
    let output = json!({ "stdout": "exact é😀\n".repeat(1000) });
    let completion = host
        .state
        .store
        .event_append(
            thread.id,
            Some(turn),
            EventPayload::ToolCallCompleted {
                tool_call_id: tool_call_id.into(),
                output: output.clone(),
                output_omitted: false,
                stream_recoverable: false,
                is_error: true,
            },
        )
        .unwrap();
    let late_delta = host
        .state
        .store
        .event_append(
            thread.id,
            Some(turn),
            EventPayload::ToolCallOutputDelta { tool_call_id: tool_call_id.into(), delta: "late stream\n".into() },
        )
        .unwrap();

    let compact = host.client().await;
    compact
        .call::<EventsSubscribe>(EventsSubscribeParams { thread_id: Some(thread.id), after_seq: Some(0), include_tool_output: Some(false) })
        .await
        .unwrap();
    let compact_event = {
        let mut notifications = compact.notifications.lock().await;
        loop {
            let notification = notifications.next().await.expect("compact replay notification");
            if notification.method != EVENT_NOTIFICATION {
                continue;
            }
            let event: EventNotification = serde_json::from_value(notification.params).unwrap();
            if event.event.seq == completion.seq {
                break event.event;
            }
        }
    };
    match compact_event.payload {
        EventPayload::ToolCallCompleted { output: received, output_omitted, is_error, .. } => {
            assert_eq!(received, Value::Null);
            assert!(output_omitted);
            assert!(is_error);
        }
        other => panic!("unexpected compact payload: {other:?}"),
    }
    let exact = compact
        .call::<ThreadsToolOutput>(ThreadsToolOutputParams {
            thread_id: thread.id,
            tool_call_id: tool_call_id.into(),
            start_seq: Some(start.seq),
            through_seq: Some(late_delta.seq),
            include_tool_stream: Some(true),
        })
        .await
        .unwrap();
    assert_eq!(exact.output, output);
    assert_eq!(exact.stream.as_deref(), Some("stream é😀\nstream é😀\nstream é😀\nstream é😀\nlate stream\n"));
    assert!(!exact.stream_omitted);
    assert!(exact.is_error);

    let full = host.client().await;
    full.call::<EventsSubscribe>(EventsSubscribeParams { thread_id: Some(thread.id), after_seq: Some(0), include_tool_output: None })
        .await
        .unwrap();
    let full_event = {
        let mut notifications = full.notifications.lock().await;
        loop {
            let notification = notifications.next().await.expect("full replay notification");
            if notification.method != EVENT_NOTIFICATION {
                continue;
            }
            let event: EventNotification = serde_json::from_value(notification.params).unwrap();
            if event.event.seq == completion.seq {
                break event.event;
            }
        }
    };
    match full_event.payload {
        EventPayload::ToolCallCompleted { output: received, output_omitted, .. } => {
            assert_eq!(received, output);
            assert!(!output_omitted);
        }
        other => panic!("unexpected full payload: {other:?}"),
    }
}

#[tokio::test]
async fn queue_survives_restart_and_consumption_is_atomic_with_turn_started() {
    let host = Host::start().await;
    let thread = host.thread();
    let queued = QueuedMessage { id: Uuid::now_v7(), thread_id: thread.id, message: UserMessage::text("follow-up") };
    host.state.orchestrator.enqueue(queued.clone()).unwrap();
    host.state.orchestrator.enqueue(queued.clone()).unwrap();
    let reopened = kybern_store::Store::open(&host.state.paths.db).unwrap();
    assert_eq!(reopened.queue_list(None).unwrap().len(), 1);
    assert_eq!(reopened.events_for_thread(thread.id).unwrap().len(), 1);
    reopened
        .event_append(thread.id, Some(Uuid::now_v7()), EventPayload::TurnStarted { message_id: queued.id, message: queued.message.clone() })
        .unwrap();
    assert!(reopened.queue_list(None).unwrap().is_empty());
    host.state.orchestrator.enqueue(queued.clone()).unwrap();
    assert!(reopened.queue_list(None).unwrap().is_empty());
    assert!(host.state.orchestrator.remove_queued(thread.id, queued.id).is_err());
    let canceled = QueuedMessage { id: Uuid::now_v7(), ..queued };
    host.state.orchestrator.enqueue(canceled.clone()).unwrap();
    host.state.orchestrator.remove_queued(thread.id, canceled.id).unwrap();
    host.state.orchestrator.enqueue(canceled).unwrap();
    assert!(reopened.queue_list(None).unwrap().is_empty());
}

#[tokio::test]
async fn notes_sync_between_clients_and_survive_reopening_the_store() {
    let host = Host::start().await;
    let thread = host.thread();
    let desktop = host.client().await;
    let phone = host.client().await;
    let empty = phone.call::<ThreadNotesGet>(ThreadsInterruptParams { thread_id: thread.id }).await.unwrap();
    assert_eq!(empty, ThreadNotes::default());
    let notes = desktop
        .call::<ThreadNotesSet>(ThreadNotesSetParams {
            thread_id: thread.id,
            text: "Desktop note\n☑ Test phone".into(),
            expected_revision: 0,
        })
        .await
        .unwrap();
    assert_eq!(phone.call::<ThreadNotesGet>(ThreadsInterruptParams { thread_id: thread.id }).await.unwrap(), notes);
    assert!(
        phone
            .call::<ThreadNotesSet>(ThreadNotesSetParams { thread_id: thread.id, text: "Stale phone edit".into(), expected_revision: 0 })
            .await
            .is_err()
    );
    let snapshot = phone
        .call::<ThreadsGet>(ThreadsGetParams { thread_id: thread.id, transcript_limit: Some(60), ..Default::default() })
        .await
        .unwrap();
    assert_eq!(snapshot.notes, notes);
    assert!(snapshot.transcript.is_empty());
    let reopened = kybern_store::Store::open(&host.state.paths.db).unwrap();
    assert_eq!(reopened.thread_notes(thread.id).unwrap(), notes);
    assert!(reopened.events_for_thread(thread.id).unwrap().is_empty(), "notes no longer write thread events");
    assert_eq!(reopened.note_for_thread(thread.id).unwrap().unwrap().body, "Desktop note\n☑ Test phone");
}

async fn next_notes_changed(client: &Client) -> NotesChangedNotification {
    let mut notifications = client.notifications.lock().await;
    loop {
        let notification =
            tokio::time::timeout(Duration::from_secs(3), futures::StreamExt::next(&mut *notifications)).await.unwrap().unwrap();
        if notification.method == NOTES_CHANGED_NOTIFICATION {
            return serde_json::from_value(notification.params).unwrap();
        }
    }
}

#[tokio::test]
async fn notes_round_trip_over_rpc_and_conflicts_use_the_conflict_code() {
    let host = Host::start().await;
    let thread = host.thread();
    let desktop = host.client().await;
    let phone = host.client().await;
    let created = desktop
        .call::<NotesCreate>(NotesCreateParams {
            scope: NoteScope::Project,
            project_id: Some(thread.project_id),
            title: Some("Release plan".into()),
            body: Some("# Plan\n- [x] tag\n- [ ] ship".into()),
            project_role: None,
            category: None,
        })
        .await
        .unwrap();
    assert_eq!((created.summary.revision, created.summary.preview.as_str()), (1, "Plan tag ship"));
    assert_eq!(created.summary.checklist, NoteChecklist { done: 1, total: 2 });

    let announced = next_notes_changed(&phone).await;
    assert_eq!(announced.note.as_ref().map(|note| note.id), Some(created.summary.id));
    assert_eq!(phone.call::<NotesList>(NotesListParams::default()).await.unwrap().notes, vec![created.summary.clone()]);
    let fetched = phone.call::<NotesGet>(NotesGetParams { id: Some(created.summary.id), thread_id: None }).await.unwrap();
    assert_eq!(fetched.note, Some(created.clone()));
    assert!(phone.call::<NotesGet>(NotesGetParams { id: None, thread_id: Some(thread.id) }).await.unwrap().note.is_none());

    let update = |body: &str, expected_revision| NotesUpdateParams {
        id: Some(created.summary.id),
        thread_id: None,
        expected_revision,
        title: None,
        body: Some(body.into()),
        category: None,
        clear_category: false,
    };
    let edited = desktop.call::<NotesUpdate>(update("desktop edit", 1)).await.unwrap();
    assert_eq!(edited.summary.revision, 2);
    assert_eq!(next_notes_changed(&phone).await.note.unwrap().revision, 2);

    // The phone still holds revision 1: the daemon answers with the CONFLICT code.
    let conflict = phone.call_raw(NotesUpdate::NAME, serde_json::to_value(update("phone edit", 1)).unwrap()).await.unwrap_err().to_string();
    assert_eq!(conflict, format!("This note changed on another device. Choose which version to keep. (code {})", codes::CONFLICT));
    assert_eq!(
        desktop.call::<NotesGet>(NotesGetParams { id: Some(created.summary.id), thread_id: None }).await.unwrap().note.unwrap().body,
        "desktop edit"
    );

    let pinned = desktop.call::<NotesPin>(NotesPinParams { id: created.summary.id, pinned: true }).await.unwrap();
    assert!(pinned.pinned);
    assert!(next_notes_changed(&phone).await.note.unwrap().pinned);
    let moved =
        desktop.call::<NotesMove>(NotesMoveParams { id: created.summary.id, scope: NoteScope::Global, project_id: None }).await.unwrap();
    assert_eq!((moved.scope, moved.project_id), (NoteScope::Global, None));
    next_notes_changed(&phone).await;

    let hits = phone.call::<NotesSearch>(NotesSearchParams { query: "DESKTOP".into(), limit: None, machine: None }).await.unwrap().results;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, created.summary.id);

    let deleted = desktop.call::<NotesDelete>(NotesIdParams { id: created.summary.id }).await.unwrap();
    assert!(deleted.deleted_at.is_some());
    assert!(next_notes_changed(&phone).await.note.unwrap().deleted_at.is_some());
    let edit_deleted = phone.call_raw(NotesUpdate::NAME, serde_json::to_value(update("late", 2)).unwrap()).await.unwrap_err().to_string();
    assert!(edit_deleted.contains("Restore this note before editing it."), "{edit_deleted}");
    assert!(
        phone
            .call::<NotesSearch>(NotesSearchParams { query: "desktop".into(), limit: None, machine: None })
            .await
            .unwrap()
            .results
            .is_empty()
    );
    assert!(phone.call_raw(NotesPurge::NAME, json!({"id": Uuid::now_v7()})).await.unwrap_err().to_string().contains("code -32003"));
    desktop.call::<NotesRestore>(NotesIdParams { id: created.summary.id }).await.unwrap();
    assert!(next_notes_changed(&phone).await.note.unwrap().deleted_at.is_none());
    desktop.call::<NotesDelete>(NotesIdParams { id: created.summary.id }).await.unwrap();
    next_notes_changed(&phone).await;
    desktop.call::<NotesPurge>(NotesIdParams { id: created.summary.id }).await.unwrap();
    assert_eq!(next_notes_changed(&phone).await.purged_id, Some(created.summary.id));
    assert!(phone.call::<NotesList>(NotesListParams::default()).await.unwrap().notes.is_empty());

    // The same thread note through both APIs, and the legacy field on threads.get.
    let thread_note = desktop
        .call::<NotesUpdate>(NotesUpdateParams {
            id: None,
            thread_id: Some(thread.id),
            expected_revision: 0,
            title: None,
            body: Some("on the thread".into()),
            category: None,
            clear_category: false,
        })
        .await
        .unwrap();
    assert_eq!((thread_note.summary.scope, thread_note.summary.title.as_str()), (NoteScope::Thread, "Test"));
    assert_eq!(
        phone.call::<ThreadNotesGet>(ThreadsInterruptParams { thread_id: thread.id }).await.unwrap(),
        ThreadNotes { text: "on the thread".into(), revision: 1 }
    );
    let legacy = phone
        .call::<ThreadNotesSet>(ThreadNotesSetParams { thread_id: thread.id, text: "from an old client".into(), expected_revision: 1 })
        .await
        .unwrap();
    assert_eq!(legacy.revision, 2);
    assert_eq!(
        desktop.call::<NotesGet>(NotesGetParams { id: None, thread_id: Some(thread.id) }).await.unwrap().note.unwrap().body,
        "from an old client"
    );
    let snapshot = phone
        .call::<ThreadsGet>(ThreadsGetParams { thread_id: thread.id, transcript_limit: Some(10), ..Default::default() })
        .await
        .unwrap();
    assert_eq!(snapshot.notes, legacy);
}

#[tokio::test]
async fn removing_a_project_over_rpc_keeps_its_notes_in_recently_deleted() {
    let host = Host::start().await;
    let thread = host.thread();
    let client = host.client().await;
    let watcher = host.client().await;
    let page = client
        .call::<NotesCreate>(NotesCreateParams {
            scope: NoteScope::Project,
            project_id: Some(thread.project_id),
            title: Some("Plan".into()),
            body: None,
            project_role: None,
            category: None,
        })
        .await
        .unwrap();
    next_notes_changed(&watcher).await;
    client
        .call::<NotesUpdate>(NotesUpdateParams {
            id: None,
            thread_id: Some(thread.id),
            expected_revision: 0,
            title: None,
            body: Some("remember".into()),
            category: None,
            clear_category: false,
        })
        .await
        .unwrap();
    next_notes_changed(&watcher).await;

    client.call::<ProjectsRemove>(ProjectsRemoveParams { project_id: thread.project_id }).await.unwrap();
    let first = next_notes_changed(&watcher).await.note.unwrap();
    let second = next_notes_changed(&watcher).await.note.unwrap();
    assert!(first.deleted_at.is_some() && second.deleted_at.is_some());
    let listed = client.call::<NotesList>(NotesListParams::default()).await.unwrap().notes;
    assert_eq!(listed.len(), 2);
    assert!(
        listed.iter().all(|note| note.deleted_at.is_some() && note.origin.as_deref().is_some_and(|origin| origin.starts_with("Fixture")))
    );
    let thread_note = listed.iter().find(|note| note.scope == NoteScope::Thread).unwrap();
    assert_eq!(thread_note.origin.as_deref(), Some("Fixture › Test"));
    assert_eq!(thread_note.title, "Test");

    let restored = client.call::<NotesRestore>(NotesIdParams { id: page.summary.id }).await.unwrap();
    assert_eq!((restored.scope, restored.project_id), (NoteScope::Global, None), "the project is gone, so the note becomes global");
}

#[tokio::test]
async fn note_notifications_reach_only_clients_that_can_read_orchestration() {
    let host = Host::start().await;
    let connect_with = |scopes: &[Scope]| {
        let token = crate::auth::generate();
        host.state.store.token_insert(Uuid::now_v7(), &crate::auth::hash(&token), "scoped", scopes).unwrap();
        async { Client::connect(&Endpoint { url: format!("{}/ws", host.url.replace("http:", "ws:")), token }).await.unwrap() }
    };
    let reader = connect_with(&[Scope::OrchestrationRead]).await;
    let blind = connect_with(&[Scope::ReviewWrite]).await;
    let writer = host.client().await;
    writer
        .call::<NotesCreate>(NotesCreateParams {
            scope: NoteScope::Global,
            project_id: None,
            title: Some("Hello".into()),
            body: None,
            project_role: None,
            category: None,
        })
        .await
        .unwrap();
    assert_eq!(next_notes_changed(&reader).await.note.unwrap().title, "Hello");
    // A client without the read scope must not hear about notes, and cannot list or write them.
    let mut notifications = blind.notifications.lock().await;
    assert!(tokio::time::timeout(Duration::from_millis(300), futures::StreamExt::next(&mut *notifications)).await.is_err());
    drop(notifications);
    assert!(blind.call::<NotesList>(NotesListParams::default()).await.unwrap_err().to_string().contains("missing scope"));
    assert!(reader.call::<NotesList>(NotesListParams::default()).await.is_ok());
    assert!(
        reader
            .call::<NotesCreate>(NotesCreateParams {
                scope: NoteScope::Global,
                project_id: None,
                title: None,
                body: None,
                project_role: None,
                category: None,
            })
            .await
            .unwrap_err()
            .to_string()
            .contains("missing scope"),
        "writing needs the operate scope"
    );
}

async fn next_tasks_changed(client: &Client) -> TaskItemsChangedNotification {
    let mut notifications = client.notifications.lock().await;
    loop {
        let notification =
            tokio::time::timeout(Duration::from_secs(3), futures::StreamExt::next(&mut *notifications)).await.unwrap().unwrap();
        if notification.method == TASK_ITEMS_CHANGED_NOTIFICATION {
            return serde_json::from_value(notification.params).unwrap();
        }
    }
}

async fn next_projects_changed(client: &Client) -> ProjectsChangedNotification {
    let mut notifications = client.notifications.lock().await;
    loop {
        let notification =
            tokio::time::timeout(Duration::from_secs(3), futures::StreamExt::next(&mut *notifications)).await.unwrap().unwrap();
        if notification.method == PROJECTS_CHANGED_NOTIFICATION {
            return serde_json::from_value(notification.params).unwrap();
        }
    }
}

#[tokio::test]
async fn project_changes_reach_every_client() {
    let host = Host::start().await;
    let desktop = host.client().await;
    let phone = host.client().await;
    let folder = host.root.join("shop");
    std::fs::create_dir_all(&folder).unwrap();

    let added = desktop
        .call::<ProjectsAdd>(ProjectsAddParams {
            path: folder.to_string_lossy().into_owned(),
            name: Some("Shop".into()),
            space_id: None,
            kind: None,
            members: None,
        })
        .await
        .unwrap();
    let announced = next_projects_changed(&phone).await;
    assert!(announced.projects.iter().any(|project| project.id == added.id && project.name == "Shop"), "the new project is announced");
    // Adding the same folder again changes nothing, so nothing is announced for it.
    desktop
        .call::<ProjectsAdd>(ProjectsAddParams {
            path: folder.to_string_lossy().into_owned(),
            name: None,
            space_id: None,
            kind: None,
            members: None,
        })
        .await
        .unwrap();

    desktop
        .call::<ProjectsUpdate>(ProjectsUpdateParams {
            project_id: added.id,
            name: Some("Storefront".into()),
            worktrees_default: None,
            task_prefix: None,
            brief_in_all_threads: None,
            coordinator: None,
            space_id: None,
            members: None,
            worktree_copy: None,
            instructions_offer_dismissed: None,
        })
        .await
        .unwrap();
    let renamed = next_projects_changed(&phone).await;
    assert_eq!(renamed.projects.iter().find(|project| project.id == added.id).map(|project| project.name.as_str()), Some("Storefront"));

    desktop.call::<ProjectsRemove>(ProjectsRemoveParams { project_id: added.id }).await.unwrap();
    let removed = next_projects_changed(&phone).await;
    assert!(removed.projects.iter().all(|project| project.id != added.id), "the removed project is gone from the list");
    // The acting client hears about its own changes too, so every window stays in step.
    assert!(next_projects_changed(&desktop).await.projects.iter().any(|project| project.id == added.id));
}

fn task_create(project_id: Option<ProjectId>, title: &str) -> TaskItemsCreateParams {
    TaskItemsCreateParams {
        scope: if project_id.is_some() { TaskScope::Project } else { TaskScope::Global },
        project_id,
        title: title.into(),
        body: None,
        status: None,
        priority: None,
        note_ids: None,
        source: None,
    }
}

fn task_update(id: TaskItemId) -> TaskItemsUpdateParams {
    TaskItemsUpdateParams {
        id,
        expected_revision: None,
        title: None,
        body: None,
        status: None,
        priority: None,
        scope: None,
        project_id: None,
        note_ids: None,
        pending_followup: None,
        before_id: None,
    }
}

#[tokio::test]
async fn tasks_round_trip_over_rpc_with_notifications_and_error_codes() {
    let host = Host::start().await;
    host.thread();
    let desktop = host.client().await;
    let phone = host.client().await;

    let project = desktop.call::<ProjectsList>(ProjectsListParams::default()).await.unwrap().projects.remove(0);
    assert_eq!(project.task_prefix.as_deref(), Some("FIX"), "derived from the project name when it was added");
    let created = desktop.call::<TaskItemsCreate>(task_create(Some(project.id), "Fix login")).await.unwrap();
    assert_eq!(
        (created.key.as_str(), created.status, created.revision, created.scope),
        ("FIX-1", TaskStatus::Inbox, 1, TaskScope::Project)
    );
    assert_eq!(next_tasks_changed(&phone).await.task, Some(created.clone()));
    let global = desktop.call::<TaskItemsCreate>(task_create(None, "Loose end")).await.unwrap();
    assert_eq!(global.key, "TSK-1");
    next_tasks_changed(&phone).await;

    let listed = phone.call::<TaskItemsList>(TaskItemsListParams::default()).await.unwrap().tasks;
    assert_eq!(listed.iter().map(|task| task.key.as_str()).collect::<Vec<_>>(), ["FIX-1", "TSK-1"]);
    let by_key = phone.call::<TaskItemsGet>(TaskItemsGetParams { id: None, key: Some("fix-1".into()) }).await.unwrap();
    assert_eq!(by_key.task, Some(created.clone()));
    assert!(phone.call::<TaskItemsGet>(TaskItemsGetParams { id: Some(Uuid::now_v7()), key: None }).await.unwrap().task.is_none());

    let edited = desktop
        .call::<TaskItemsUpdate>(TaskItemsUpdateParams {
            body: Some("details".into()),
            expected_revision: Some(1),
            ..task_update(created.id)
        })
        .await
        .unwrap();
    assert_eq!(edited.revision, 2);
    assert_eq!(next_tasks_changed(&phone).await.task.unwrap().body, "details");
    let stale = TaskItemsUpdateParams { body: Some("phone edit".into()), expected_revision: Some(1), ..task_update(created.id) };
    let conflict = phone.call_raw(TaskItemsUpdate::NAME, serde_json::to_value(stale).unwrap()).await.unwrap_err().to_string();
    assert_eq!(conflict, format!("This task changed on another device. Choose which version to keep. (code {})", codes::CONFLICT));

    let todo = phone
        .call::<TaskItemsUpdate>(TaskItemsUpdateParams { status: Some(TaskStatus::Todo), priority: Some(2), ..task_update(created.id) })
        .await
        .unwrap();
    assert_eq!((todo.status, todo.priority, todo.revision), (TaskStatus::Todo, 2, 2));
    assert_eq!(next_tasks_changed(&phone).await.task.unwrap().status, TaskStatus::Todo);
    let running = phone.call_raw(TaskItemsUpdate::NAME, json!({"id": created.id, "status": "running"})).await.unwrap_err().to_string();
    assert!(running.contains(&format!("code {}", codes::INVALID_PARAMS)), "{running}");
    let unknown = phone.call_raw(TaskItemsUpdate::NAME, json!({"id": Uuid::now_v7(), "priority": 1})).await.unwrap_err().to_string();
    assert!(unknown.contains(&format!("code {}", codes::NOT_FOUND)), "{unknown}");

    // Sending validates before starting anything; a follow-up with no run is saved for the next one.
    let empty_prompt = TaskItemsSendParams {
        id: created.id,
        provider: ProviderInstance::default_for(ProviderKind::ClaudeCode),
        model: None,
        effort: None,
        permission_mode: None,
        use_worktree: None,
        base_branch: None,
        project_id: None,
        prompt: Some(" ".into()),
        message: None,
        note_ids: None,
        thread_id: None,
        message_id: None,
    };
    let rejected = phone.call_raw(TaskItemsSend::NAME, serde_json::to_value(empty_prompt).unwrap()).await.unwrap_err().to_string();
    assert!(rejected.contains(&format!("code {}", codes::INVALID_PARAMS)), "{rejected}");
    let saved = phone.call::<TaskItemsFollowup>(TaskItemsFollowupParams::text(created.id, "also tests")).await.unwrap();
    assert_eq!((saved.sent_to, saved.task.pending_followup.as_deref()), (None, Some("also tests")));
    assert_eq!(next_tasks_changed(&phone).await.task.unwrap().pending_followup.as_deref(), Some("also tests"));

    desktop.call::<TaskItemsDelete>(TaskItemsIdParams { id: created.id }).await.unwrap();
    assert_eq!(next_tasks_changed(&phone).await.deleted_id, Some(created.id));
    assert_eq!(phone.call::<TaskItemsList>(TaskItemsListParams::default()).await.unwrap().tasks.len(), 1);
    let restored = desktop.call::<TaskItemsRestore>(TaskItemsIdParams { id: created.id }).await.unwrap();
    assert_eq!((restored.key.as_str(), restored.pending_followup.as_deref()), ("FIX-1", Some("also tests")));
    assert_eq!(next_tasks_changed(&phone).await.task.map(|task| task.id), Some(created.id));
}

#[tokio::test]
async fn a_follow_up_over_rpc_reopens_the_run_and_every_broadcast_keeps_task_and_run_in_step() {
    let host = Host::start().await;
    let mut thread = host.thread();
    let desktop = host.client().await;
    let phone = host.client().await;
    let task = desktop.call::<TaskItemsCreate>(task_create(Some(thread.project_id), "Talk")).await.unwrap();
    next_tasks_changed(&phone).await;

    // Before any run, follow-ups are saved: trimmed, one blank line apart.
    let call = |text: &str| desktop.call::<TaskItemsFollowup>(TaskItemsFollowupParams::text(task.id, text));
    assert_eq!(call("  first \n").await.unwrap().task.pending_followup.as_deref(), Some("first"));
    assert_eq!(call("\nsecond\n\n").await.unwrap().task.pending_followup.as_deref(), Some("first\n\nsecond"));
    next_tasks_changed(&phone).await;
    next_tasks_changed(&phone).await;

    // A run on the thread, driven by hand: the agent is out of the picture.
    host.state.store.task_run_start(task.id, thread.id, &thread.provider, None, &[]).unwrap();
    host.state.orchestrator.track_task_thread(thread.id);
    thread.status = ThreadStatus::Running;
    host.state.store.thread_upsert(&thread).unwrap();
    let emit = |payload| host.state.orchestrator.emit(thread.id, Some(Uuid::nil()), payload).unwrap();
    let completed = || EventPayload::TurnCompleted {
        stop_reason: StopReason::Completed,
        usage: Usage::default(),
        cost_usd: None,
        duration_ms: 1,
        terminal_message_id: None,
    };
    let started = || EventPayload::TurnStarted { message_id: Uuid::now_v7(), message: UserMessage::text("go") };
    let in_step = |task: &TaskItem| match task.status {
        TaskStatus::Running => assert!(task.runs[0].state.is_live(), "running next to {:?}", task.runs[0].state),
        TaskStatus::NeedsReview => assert!(!task.runs[0].state.is_live(), "in review next to {:?}", task.runs[0].state),
        _ => {}
    };

    emit(completed());
    let reviewed = next_tasks_changed(&phone).await.task.unwrap();
    in_step(&reviewed);
    assert_eq!((reviewed.status, reviewed.runs[0].state), (TaskStatus::NeedsReview, TaskRunState::Completed));
    let first_end = reviewed.runs[0].ended_at.expect("ended");
    tokio::time::sleep(Duration::from_millis(5)).await;

    // The thread is busy, so the follow-up is queued on it; when the queued turn starts, the run reopens.
    let queued = desktop.call::<TaskItemsFollowup>(TaskItemsFollowupParams::text(task.id, "while you work")).await.unwrap();
    assert_eq!(queued.sent_to, Some(thread.id));
    assert_eq!(host.state.store.queue_list(Some(thread.id)).unwrap().len(), 1);
    emit(started());
    let reopened = next_tasks_changed(&phone).await.task.unwrap();
    in_step(&reopened);
    assert_eq!((reopened.status, reopened.runs[0].state, reopened.runs[0].ended_at), (TaskStatus::Running, TaskRunState::Running, None));
    assert_eq!(phone.call::<TaskItemsGet>(TaskItemsGetParams { id: Some(task.id), key: None }).await.unwrap().task, Some(reopened));

    emit(completed());
    let again = next_tasks_changed(&phone).await.task.unwrap();
    in_step(&again);
    assert_eq!((again.status, again.runs[0].state), (TaskStatus::NeedsReview, TaskRunState::Completed));
    assert!(again.runs[0].ended_at.unwrap() > first_end, "the end time moves to the new turn");
}

#[tokio::test]
async fn project_task_prefixes_are_assigned_edited_and_validated_over_rpc() {
    let host = Host::start().await;
    let client = host.client().await;
    let one = host.root.join("one");
    let two = host.root.join("two");
    std::fs::create_dir_all(&one).unwrap();
    std::fs::create_dir_all(&two).unwrap();
    let a = client
        .call::<ProjectsAdd>(ProjectsAddParams {
            path: one.to_string_lossy().into_owned(),
            name: Some("kybern-mobile".into()),
            space_id: None,
            kind: None,
            members: None,
        })
        .await
        .unwrap();
    let b = client
        .call::<ProjectsAdd>(ProjectsAddParams {
            path: two.to_string_lossy().into_owned(),
            name: Some("kybern-mobile".into()),
            space_id: None,
            kind: None,
            members: None,
        })
        .await
        .unwrap();
    assert_eq!((a.task_prefix.as_deref(), b.task_prefix.as_deref()), (Some("KYM"), Some("KYMA")));

    let update = |project_id, prefix: &str| ProjectsUpdateParams {
        project_id,
        name: None,
        worktrees_default: None,
        task_prefix: Some(prefix.into()),
        brief_in_all_threads: None,
        coordinator: None,
        space_id: None,
        members: None,
        worktree_copy: None,
        instructions_offer_dismissed: None,
    };
    let renamed = client.call::<ProjectsUpdate>(update(a.id, "mob")).await.unwrap();
    assert_eq!(renamed.task_prefix.as_deref(), Some("MOB"));
    let key = client.call::<TaskItemsCreate>(task_create(Some(a.id), "First")).await.unwrap().key;
    assert_eq!(key, "MOB-1");
    for bad in ["x", "TOOLONG", "A1B", "TSK"] {
        let error = client.call_raw(ProjectsUpdate::NAME, serde_json::to_value(update(a.id, bad)).unwrap()).await.unwrap_err().to_string();
        assert!(error.contains(&format!("code {}", codes::INVALID_PARAMS)), "{bad}: {error}");
    }
    let taken = client.call_raw(ProjectsUpdate::NAME, serde_json::to_value(update(b.id, "MOB")).unwrap()).await.unwrap_err().to_string();
    assert!(taken.contains("Another project already uses MOB"), "{taken}");
    let untouched = client
        .call::<ProjectsUpdate>(ProjectsUpdateParams {
            project_id: b.id,
            name: Some("Renamed".into()),
            worktrees_default: None,
            task_prefix: None,
            brief_in_all_threads: None,
            coordinator: None,
            space_id: None,
            members: None,
            worktree_copy: None,
            instructions_offer_dismissed: None,
        })
        .await
        .unwrap();
    assert_eq!((untouched.name.as_str(), untouched.task_prefix.as_deref()), ("Renamed", Some("KYMA")), "a rename leaves the prefix alone");
    assert!(
        client
            .call::<ProjectsList>(ProjectsListParams::default())
            .await
            .unwrap()
            .projects
            .iter()
            .all(|project| project.task_prefix.is_some())
    );
}

#[tokio::test]
async fn removing_a_project_over_rpc_hides_its_tasks_and_task_notifications_respect_scopes() {
    let host = Host::start().await;
    let thread = host.thread();
    let client = host.client().await;
    let watcher = host.client().await;
    let task = client.call::<TaskItemsCreate>(task_create(Some(thread.project_id), "Project work")).await.unwrap();
    let global = client.call::<TaskItemsCreate>(task_create(None, "Global work")).await.unwrap();
    next_tasks_changed(&watcher).await;
    next_tasks_changed(&watcher).await;

    client.call::<ProjectsRemove>(ProjectsRemoveParams { project_id: thread.project_id }).await.unwrap();
    assert_eq!(next_tasks_changed(&watcher).await.deleted_id, Some(task.id));
    let remaining = client.call::<TaskItemsList>(TaskItemsListParams::default()).await.unwrap().tasks;
    assert_eq!(remaining.iter().map(|task| task.id).collect::<Vec<_>>(), vec![global.id]);
    let restored = client.call::<TaskItemsRestore>(TaskItemsIdParams { id: task.id }).await.unwrap();
    assert_eq!((restored.scope, restored.project_id, restored.key.as_str()), (TaskScope::Global, None, "FIX-1"));

    let connect_with = |scopes: &[Scope]| {
        let token = crate::auth::generate();
        host.state.store.token_insert(Uuid::now_v7(), &crate::auth::hash(&token), "scoped", scopes).unwrap();
        async { Client::connect(&Endpoint { url: format!("{}/ws", host.url.replace("http:", "ws:")), token }).await.unwrap() }
    };
    let reader = connect_with(&[Scope::OrchestrationRead]).await;
    let blind = connect_with(&[Scope::ReviewWrite]).await;
    client.call::<TaskItemsCreate>(task_create(None, "Hello")).await.unwrap();
    assert_eq!(next_tasks_changed(&reader).await.task.unwrap().title, "Hello");
    let mut notifications = blind.notifications.lock().await;
    assert!(tokio::time::timeout(Duration::from_millis(300), futures::StreamExt::next(&mut *notifications)).await.is_err());
    drop(notifications);
    assert!(blind.call::<TaskItemsList>(TaskItemsListParams::default()).await.unwrap_err().to_string().contains("missing scope"));
    assert!(reader.call::<TaskItemsList>(TaskItemsListParams::default()).await.is_ok());
    assert!(reader.call::<TaskItemsCreate>(task_create(None, "No")).await.unwrap_err().to_string().contains("missing scope"));
}

#[cfg(unix)]
#[tokio::test]
async fn reconnect_reattaches_terminal_and_closed_identity_cannot_spawn_again() {
    let host = Host::start().await;
    let id = Uuid::now_v7();
    let cwd = host.root.to_string_lossy().into_owned();
    let create = || host.state.terminals.create(Some(id), None, cwd.clone(), 80, 24, Some(vec!["/bin/sh".into()])).unwrap();
    let first = create();
    let second = create();
    assert!(std::sync::Arc::ptr_eq(&first, &second));
    first.write(b"printf 'reconnected-terminal\\n'\n").unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let (_, bytes) = second.subscribe_output(true);
            if String::from_utf8_lossy(&bytes).contains("reconnected-terminal") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    host.state.terminals.close(id).unwrap();
    assert!(host.state.terminals.create(Some(id), None, cwd, 80, 24, Some(vec!["/bin/sh".into()])).is_err());
}

#[tokio::test]
async fn interrupted_host_recovery_ends_the_turn_as_interrupted_and_drains_follow_ups() {
    let host = Host::start().await;
    let mut thread = host.thread();
    thread.status = ThreadStatus::Running;
    host.state.store.thread_upsert(&thread).unwrap();
    host.state
        .orchestrator
        .enqueue(QueuedMessage { id: Uuid::now_v7(), thread_id: thread.id, message: UserMessage::text("wait for the interrupted turn") })
        .unwrap();
    host.state.orchestrator.recover_after_restart().await.unwrap();
    // The cut-off turn is interrupted (not failed) and the thread is idle, so
    // the follow-up is dispatched instead of waiting for the user.
    let events = host.state.store.events_for_thread(thread.id).unwrap();
    assert!(events.iter().any(|event| matches!(event.payload, EventPayload::TurnCompleted { stop_reason: StopReason::Interrupted, .. })));
    assert!(!events.iter().any(|event| matches!(event.payload, EventPayload::TurnFailed { .. })));
    host.state.orchestrator.drain_queues().await.unwrap();
    assert!(host.state.store.queue_list(Some(thread.id)).unwrap().is_empty());
    assert!(
        host.state
            .store
            .events_for_thread(thread.id)
            .unwrap()
            .iter()
            .any(|event| matches!(&event.payload, EventPayload::TurnStarted { message, .. } if message == &UserMessage::text("wait for the interrupted turn")))
    );
}

#[tokio::test]
async fn response_images_require_auth_and_stay_on_the_owning_host() {
    let host = Host::start().await;
    let other = Host::start().await;
    let thread = host.thread();
    let http = reqwest::Client::new();
    let url = format!("{}/threads/{}/image", host.url, thread.id);
    let png = b"\x89PNG\r\n\x1a\nfixture";
    std::fs::write(host.root.join("image with spaces.png"), png).unwrap();
    std::fs::write(host.root.join("not-an-image.png"), b"<html>no</html>").unwrap();
    assert_eq!(
        http.get(&url).query(&[("path", "image with spaces.png")]).send().await.unwrap().status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        http.get(&url).bearer_auth(&other.state.bootstrap_token).query(&[("path", "image with spaces.png")]).send().await.unwrap().status(),
        reqwest::StatusCode::FORBIDDEN
    );
    let response =
        http.get(&url).bearer_auth(&host.state.bootstrap_token).query(&[("path", "image with spaces.png")]).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "image/png");
    assert_eq!(response.bytes().await.unwrap().as_ref(), png);
    let response = http.get(&url).bearer_auth(&host.state.bootstrap_token).query(&[("path", "not-an-image.png")]).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNSUPPORTED_MEDIA_TYPE);
    // Another daemon's token still reads nothing, wherever the image is.
    let outside = outside_folder().join("outside.png");
    std::fs::write(&outside, png).unwrap();
    let outside = outside.to_string_lossy().to_string();
    assert_eq!(
        http.get(&url).bearer_auth(&other.state.bootstrap_token).query(&[("path", outside.as_str())]).send().await.unwrap().status(),
        reqwest::StatusCode::FORBIDDEN
    );
    assert_eq!(http.get(&url).query(&[("path", outside.as_str())]).send().await.unwrap().status(), reqwest::StatusCode::UNAUTHORIZED);
    let response = http.get(&url).bearer_auth(&host.state.bootstrap_token).query(&[("path", outside.as_str())]).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, host.root.join("linked.png")).unwrap();
        assert_eq!(
            http.get(&url).bearer_auth(&host.state.bootstrap_token).query(&[("path", "linked.png")]).send().await.unwrap().status(),
            reqwest::StatusCode::OK
        );
    }
}

/// A folder outside the test daemon's own folders.
fn outside_folder() -> std::path::PathBuf {
    let folder = std::env::temp_dir().join(format!("kybern-outside-test-{}", Uuid::now_v7()));
    std::fs::create_dir_all(&folder).unwrap();
    folder
}

#[tokio::test]
async fn response_images_render_from_any_folder_and_only_real_images_are_served() {
    let host = Host::start().await;
    let other = Host::start().await;
    let thread = host.thread();
    let http = reqwest::Client::new();
    let url = format!("{}/threads/{}/image", host.url, thread.id);
    let get = |path: String| {
        let request = http.get(&url).bearer_auth(&host.state.bootstrap_token).query(&[("path", path)]);
        async move { request.send().await.unwrap().status() }
    };
    let png = b"\x89PNG\r\n\x1a\nfixture";
    // Agents and tools such as Playwright write screenshots into dot-folders of the workspace.
    std::fs::create_dir_all(host.root.join(".screenshots/times")).unwrap();
    std::fs::write(host.root.join(".screenshots/times/trip-1280.png"), png).unwrap();
    std::fs::write(host.root.join(".screenshots/times/Buchungsformular 7 h spät.png"), png).unwrap();
    assert_eq!(get(".screenshots/times/trip-1280.png".into()).await, reqwest::StatusCode::OK);
    assert_eq!(get("./.screenshots/times/trip-1280.png".into()).await, reqwest::StatusCode::OK);
    assert_eq!(get(".screenshots/times/Buchungsformular%207%20h%20sp%C3%A4t.png".into()).await, reqwest::StatusCode::OK);
    assert_eq!(get(host.root.join(".screenshots/times/trip-1280.png").to_string_lossy().into()).await, reqwest::StatusCode::OK);
    assert_eq!(get(".screenshots/missing.png".into()).await, reqwest::StatusCode::NOT_FOUND);
    // Any folder serves its images; a file that is not an image by its content never is, whatever its name.
    std::fs::create_dir_all(host.root.join(".git")).unwrap();
    std::fs::write(host.root.join(".git/shot.png"), png).unwrap();
    assert_eq!(get(".git/shot.png".into()).await, reqwest::StatusCode::OK);
    std::fs::write(host.root.join(".env"), b"SECRET=1").unwrap();
    assert_eq!(get(".env".into()).await, reqwest::StatusCode::UNSUPPORTED_MEDIA_TYPE);
    std::fs::write(host.root.join(".screenshots/env.png"), b"SECRET=1").unwrap();
    assert_eq!(get(".screenshots/env.png".into()).await, reqwest::StatusCode::UNSUPPORTED_MEDIA_TYPE);
    // Outside the folder (another checkout, a worktree, another daemon's folder): served without being named first.
    let elsewhere = outside_folder();
    let shot = elsewhere.join("outside shot.png");
    std::fs::write(&shot, png).unwrap();
    assert_eq!(get(shot.to_string_lossy().into()).await, reqwest::StatusCode::OK);
    #[cfg(unix)]
    assert_eq!(get(format!("file://{}", shot.to_string_lossy().replace(' ', "%20"))).await, reqwest::StatusCode::OK);
    let data = other.root.join("data.png");
    std::fs::write(&data, png).unwrap();
    assert_eq!(get(data.to_string_lossy().into()).await, reqwest::StatusCode::OK);
    assert_eq!(get(elsewhere.join("missing.png").to_string_lossy().into()).await, reqwest::StatusCode::NOT_FOUND);
    assert_eq!(get(thread.cwd.clone()).await, reqwest::StatusCode::NOT_FOUND, "a folder is no image");
}

#[tokio::test]
async fn harness_updates_wait_for_turns_and_block_new_sends_during_installation() {
    let host = Host::start().await;
    let mut thread = host.thread();
    for status in [ThreadStatus::Running, ThreadStatus::AwaitingApproval] {
        thread.status = status;
        host.state.store.thread_upsert(&thread).unwrap();
        assert!(host.state.orchestrator.idle_harness_for_update(thread.provider.kind).await.unwrap().is_none());
    }
    thread.status = ThreadStatus::Idle;
    host.state.store.thread_upsert(&thread).unwrap();
    let guard = host.state.orchestrator.idle_harness_for_update(thread.provider.kind).await.unwrap().unwrap();
    let error = host.state.orchestrator.send(thread.id, UserMessage::text("do not launch a real CLI")).await.unwrap_err();
    assert!(error.to_string().contains("updating"));
    assert!(host.state.store.events_for_thread(thread.id).unwrap().is_empty());
    assert!(host.state.orchestrator.idle_harness_for_update(thread.provider.kind).await.unwrap().is_none());
    drop(guard);
    let now = chrono::Utc::now();
    let turn_id = Uuid::now_v7();
    let mut task = RuntimeTask {
        id: "background".into(),
        thread_id: thread.id,
        origin_turn_id: turn_id,
        started_seq: 1,
        updated_seq: 1,
        kind: RuntimeTaskKind::Agent,
        status: RuntimeTaskStatus::Running,
        title: "Background agent".into(),
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
    };
    host.state.store.event_append(thread.id, Some(turn_id), EventPayload::RuntimeTaskStarted { task: task.clone() }).unwrap();
    assert!(host.state.orchestrator.idle_harness_for_update(thread.provider.kind).await.unwrap().is_none());
    task.status = RuntimeTaskStatus::Completed;
    host.state.store.event_append(thread.id, Some(turn_id), EventPayload::RuntimeTaskCompleted { task }).unwrap();
    assert!(host.state.orchestrator.idle_harness_for_update(thread.provider.kind).await.unwrap().is_some());
}

#[tokio::test]
async fn custom_harnesses_are_not_mutated_and_update_results_survive_reconnect() {
    let host = Host::start().await;
    let mut settings = host.state.settings.get();
    settings
        .providers
        .insert(ProviderKind::ClaudeCode, ProviderSettings { binary: Some("/custom/pinned-claude".into()), ..Default::default() });
    host.state.settings.set(settings).unwrap();
    let client = host.client().await;
    client.call::<HarnessUpdatesRun>(HarnessUpdateParams { kind: ProviderKind::ClaudeCode }).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let records = client.call::<HarnessUpdatesList>(Empty {}).await.unwrap().updates;
            if records.iter().any(|record| record.kind == ProviderKind::ClaudeCode && record.status == HarnessUpdateStatus::Unsupported) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let reloaded = crate::harness_updates::HarnessUpdates::new(&host.state.store).unwrap().list();
    let record = reloaded.iter().find(|record| record.kind == ProviderKind::ClaudeCode).unwrap();
    assert_eq!(record.status, HarnessUpdateStatus::Unsupported);
    assert!(record.message.contains("Custom executable"));
}

#[tokio::test]
async fn byte_lag_can_replay_every_durable_event_including_oversized_payload() {
    let host = Host::start().await;
    let thread = host.thread();
    let mut slow = host.state.events.subscribe();
    let event = host
        .state
        .store
        .event_append(
            thread.id,
            None,
            EventPayload::AssistantTextDelta { message_id: Uuid::now_v7(), origin: Default::default(), delta: "x".repeat(9 * 1024 * 1024) },
        )
        .unwrap();
    host.state.events.send(event.clone()).unwrap();
    assert!(matches!(slow.recv().await, Err(tokio::sync::broadcast::error::RecvError::Lagged(_))));
    let replay = host.state.store.events_after_bounded(Some(thread.id), 0, 500, 1024).unwrap();
    assert_eq!(replay.len(), 1);
    assert_eq!(replay[0].seq, event.seq);
    assert_eq!(serde_json::to_value(&replay[0]).unwrap(), serde_json::to_value(&event).unwrap());
    for n in 0..5 {
        host.state
            .store
            .event_append(
                thread.id,
                None,
                EventPayload::AssistantTextDelta {
                    message_id: Uuid::now_v7(),
                    origin: Default::default(),
                    delta: format!("{n}{}", "y".repeat(900)),
                },
            )
            .unwrap();
    }
    let mut cursor = event.seq;
    let mut recovered = 0;
    loop {
        let batch = host.state.store.events_after_bounded(Some(thread.id), cursor, 500, 1200).unwrap();
        if batch.is_empty() {
            break;
        }
        assert_eq!(batch.len(), 1);
        cursor = batch[0].seq;
        recovered += 1;
    }
    assert_eq!(recovered, 5);
}

// Shared unchanged with the before worktree. vmmap samples THIS test process only.
#[tokio::test]
#[ignore = "manual native memory comparison"]
async fn memory_broadcast_fixture() {
    fn sample(stage: &str) {
        let result = std::process::Command::new("/usr/bin/vmmap").args(["-summary", &std::process::id().to_string()]).output().unwrap();
        let output = String::from_utf8_lossy(&result.stdout);
        let footprint = output.lines().find(|line| line.starts_with("Physical footprint:")).unwrap_or("unavailable");
        println!("{}", serde_json::json!({"sample": stage, "footprint": footprint}));
    }
    let host = Host::start().await;
    let thread = host.thread();
    let mut slow = host.state.events.subscribe();
    sample("startup");
    for n in 0..2048 {
        let event = host
            .state
            .store
            .event_append(
                thread.id,
                None,
                EventPayload::AssistantTextDelta {
                    message_id: Uuid::now_v7(),
                    origin: Default::default(),
                    delta: format!("{n:08}{}", "x".repeat(32760)),
                },
            )
            .unwrap();
        host.state.events.send(event).unwrap();
    }
    sample("slow-subscriber");
    let mut retained = 0;
    loop {
        match slow.try_recv() {
            Ok(_) => retained += 1,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
    let mut cursor = 0;
    let mut recovered = 0;
    loop {
        let batch = host.state.store.events_after(Some(thread.id), cursor, 32).unwrap();
        if batch.is_empty() {
            break;
        }
        for event in batch {
            assert!(event.seq > cursor);
            cursor = event.seq;
            recovered += 1;
        }
    }
    assert_eq!(recovered, 2048);
    println!("{}", serde_json::json!({"retainedEvents": retained, "recoveredEvents": recovered}));
    sample("recovered");
}

#[tokio::test]
async fn previews_share_auth_and_path_checks_and_originals_remain_exact() {
    let host = Host::start().await;
    let thread = host.thread();
    let http = reqwest::Client::new();
    let url = format!("{}/threads/{}/image", host.url, thread.id);
    for format in [image::ImageFormat::Png, image::ImageFormat::Jpeg, image::ImageFormat::Gif, image::ImageFormat::WebP] {
        let image = image::DynamicImage::new_rgb8(1800, 600);
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, format).unwrap();
        let name = format!("preview.{}", format.extensions_str()[0]);
        std::fs::write(host.root.join(&name), bytes.get_ref()).unwrap();
        let query = [("path", name.as_str()), ("preview", "true")];
        assert_eq!(http.get(&url).query(&query).send().await.unwrap().status(), reqwest::StatusCode::UNAUTHORIZED);
        let response = http.get(&url).bearer_auth(&host.state.bootstrap_token).query(&query).send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "image/png");
        let preview = image::load_from_memory(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(preview.width(), 560);
        assert!(preview.height() <= 352);
        let original = http.get(&url).bearer_auth(&host.state.bootstrap_token).query(&[("path", name)]).send().await.unwrap();
        assert_eq!(original.bytes().await.unwrap().as_ref(), bytes.get_ref());
    }
    // A file outside is judged by its content like any other: this one is not an image.
    let outside = outside_folder().join("outside.png");
    std::fs::write(&outside, b"image").unwrap();
    let refused = http
        .get(&url)
        .bearer_auth(&host.state.bootstrap_token)
        .query(&[("path", outside.to_str().unwrap()), ("preview", "true")])
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn artifacts_paginate_receipts_and_confine_isolated_previews() {
    let host = Host::start().await;
    let thread = host.thread();
    let client = host.client().await;
    let file = host.root.join("demo.html");
    std::fs::write(&file, "<button onclick=\"this.textContent='Clicked'\">Click</button>").unwrap();
    let mut seqs = Vec::new();
    for index in 0..3 {
        let id = format!("artifact-{index}");
        let event = host
            .state
            .store
            .event_append(
                thread.id,
                None,
                EventPayload::ToolCallStarted {
                    call: ToolCall { id: id.clone(), name: "Artifact".into(), input: json!({"file_path":"demo.html"}), parent_id: None },
                    origin: EventOrigin::Root,
                },
            )
            .unwrap();
        seqs.push(event.seq);
        if index != 2 {
            host.state
                .store
                .event_append(
                    thread.id,
                    None,
                    EventPayload::ToolCallCompleted {
                        tool_call_id: id,
                        output: json!({"url":"https://claude.ai/public/artifacts/example"}),
                        output_omitted: false,
                        stream_recoverable: false,
                        is_error: index == 1,
                    },
                )
                .unwrap();
        }
    }
    // Replayed completion receipts must not duplicate gallery rows or consume its page limit.
    host.state
        .store
        .event_append(
            thread.id,
            None,
            EventPayload::ToolCallCompleted {
                tool_call_id: "artifact-1".into(),
                output: json!({"url":"https://claude.ai/public/artifacts/example"}),
                output_omitted: false,
                stream_recoverable: false,
                is_error: true,
            },
        )
        .unwrap();
    let page = client.call::<ArtifactsList>(ArtifactsListParams { thread_id: thread.id, before_seq: None, limit: 2 }).await.unwrap();
    assert_eq!(page.artifacts.len(), 2);
    assert_eq!(page.artifacts[0].seq, seqs[2]);
    assert!(page.artifacts[0].output.is_none());
    assert!(page.artifacts[1].is_error);
    let older = client
        .call::<ArtifactsList>(ArtifactsListParams { thread_id: thread.id, before_seq: page.next_before_seq, limit: 2 })
        .await
        .unwrap();
    assert_eq!(older.artifacts.len(), 1);
    assert_eq!(older.artifacts[0].seq, seqs[0]);
    assert!(older.next_before_seq.is_none());
    let source =
        client.call::<ArtifactRead>(ArtifactReadParams { thread_id: thread.id, path: file.to_string_lossy().into_owned() }).await.unwrap();
    assert!(source.content.contains("Clicked"));
    for path in ["../outside.html", "/etc/passwd", "state.sqlite"] {
        assert!(client.call::<ArtifactRead>(ArtifactReadParams { thread_id: thread.id, path: path.into() }).await.is_err());
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/etc/passwd", host.root.join("escape.html")).unwrap();
        assert!(client.call::<ArtifactRead>(ArtifactReadParams { thread_id: thread.id, path: "escape.html".into() }).await.is_err());
    }
    let preview = client.call::<ArtifactPreview>(ArtifactReadParams { thread_id: thread.id, path: "demo.html".into() }).await.unwrap();
    let url = format!("{}/artifact-preview/{}", host.url, preview.ticket);
    let http = reqwest::Client::new();
    let response = http.get(&url).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let policy = response.headers()["content-security-policy"].to_str().unwrap();
    assert!(policy.contains("sandbox allow-scripts"));
    assert!(policy.contains("connect-src 'none'"));
    assert!(!policy.contains("allow-same-origin"));
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(response.text().await.unwrap().contains("Clicked"));
    assert_eq!(http.get(url).send().await.unwrap().status(), 404);
}

#[tokio::test]
async fn integration_mutations_require_operation_scopes_before_provider_execution() {
    let host = Host::start().await;
    let thread = host.thread();
    let token = "read-only-fixture-token";
    host.state.store.token_insert(Uuid::now_v7(), &crate::auth::hash(token), "read-only", &[Scope::OrchestrationRead]).unwrap();
    let client = Client::connect(&Endpoint { url: format!("{}/ws", host.url.replace("http:", "ws:")), token: token.into() }).await.unwrap();
    let change = client
        .call::<IntegrationChange>(IntegrationChangeParams {
            project_id: thread.project_id,
            provider: ProviderKind::ClaudeCode,
            id: "fixture@mock".into(),
            kind: IntegrationKind::Plugin,
            scope: None,
            action: IntegrationAction::Install,
        })
        .await
        .unwrap_err();
    assert!(change.to_string().contains("scope"), "{change}");
    let login = client.call::<IntegrationLogin>(IntegrationLoginParams { thread_id: thread.id, name: "fixture".into() }).await.unwrap_err();
    assert!(login.to_string().contains("scope"), "{login}");
}

#[tokio::test]
async fn cached_history_pages_match_full_history_and_keep_their_sequence_barrier() {
    let host = Host::start().await;
    let thread = host.thread();
    for index in 0..240 {
        host.state
            .store
            .event_append(
                thread.id,
                Some(thread.id),
                EventPayload::ProviderNotice {
                    data: None,
                    level: NoticeLevel::Info,
                    text: format!("Entry {index}: {}", "readable tool output ".repeat(100)),
                },
            )
            .unwrap();
    }
    let client = host.client().await;
    let params = ThreadsGetParams { thread_id: thread.id, transcript_limit: Some(60), ..Default::default() };
    let recent = client.call::<ThreadsGet>(params.clone()).await.unwrap();
    assert_eq!(recent.transcript.len(), 60);
    let barrier = recent.thread.last_seq;
    let full =
        client.call::<ThreadsGet>(ThreadsGetParams { transcript_limit: None, through_seq: Some(barrier), ..params.clone() }).await.unwrap();
    host.state
        .store
        .event_append(
            thread.id,
            Some(thread.id),
            EventPayload::ProviderNotice { data: None, level: NoticeLevel::Info, text: "New live entry".into() },
        )
        .unwrap();
    let mut collected = recent.transcript;
    let mut cursor = recent.next_before_seq;
    while let Some(before_seq) = cursor {
        let older = client
            .call::<ThreadsGet>(ThreadsGetParams { before_seq: Some(before_seq), through_seq: Some(barrier), ..params.clone() })
            .await
            .unwrap();
        assert_eq!(older.thread.last_seq, barrier);
        cursor = older.next_before_seq;
        let mut entries = older.transcript;
        entries.append(&mut collected);
        collected = entries;
    }
    assert_eq!(serde_json::to_value(collected).unwrap(), serde_json::to_value(full.transcript).unwrap());
    let current = client.call::<ThreadsGet>(params).await.unwrap();
    assert!(current.thread.last_seq > barrier);
    assert!(matches!(current.transcript.last(), Some(TranscriptEntry::Notice { text, .. }) if text == "New live entry"));
}

/// Shaped like a delegating thread: a prompt, then hundreds of subagent tool
/// rows in the same turn. Row pages of 120 ended inside the turn and added
/// nothing a reader could see; whole-turn pages bring each prompt with its work.
#[tokio::test]
async fn whole_turn_history_pages_add_whole_turns_and_leave_thread_state_out() {
    let host = Host::start().await;
    let thread = host.thread();
    let notice = |turn: Uuid, text: String| {
        host.state
            .store
            .event_append(thread.id, Some(turn), EventPayload::ProviderNotice { data: None, level: NoticeLevel::Info, text })
            .unwrap();
    };
    let turns: Vec<Uuid> = (0..3).map(|_| Uuid::now_v7()).collect();
    for (index, turn) in turns.iter().enumerate() {
        let rows = if index == 1 { 900 } else { 5 };
        for row in 0..rows {
            notice(*turn, format!("turn {index} row {row}"));
        }
    }
    host.state
        .store
        .event_append(
            thread.id,
            Some(turns[2]),
            EventPayload::ProviderCommandsUpdated {
                commands: vec![ProviderCommand { name: "review".into(), description: "Review the change".into() }],
            },
        )
        .unwrap();
    let client = host.client().await;
    let params =
        ThreadsGetParams { thread_id: thread.id, transcript_limit: Some(3), include_tool_output: Some(false), ..Default::default() };
    let recent = client.call::<ThreadsGet>(params.clone()).await.unwrap();
    assert_eq!(recent.transcript.len(), 3);
    assert_eq!(recent.provider_commands.len(), 1);
    let barrier = recent.thread.last_seq;
    let full =
        client.call::<ThreadsGet>(ThreadsGetParams { transcript_limit: None, through_seq: Some(barrier), ..params.clone() }).await.unwrap();

    let page_turns = |page: &[TranscriptEntry]| {
        let mut ids: Vec<Uuid> = page
            .iter()
            .map(|entry| match entry {
                TranscriptEntry::Notice { turn_id, .. } => *turn_id,
                other => panic!("unexpected row {other:?}"),
            })
            .collect();
        ids.dedup();
        ids
    };
    let mut collected = recent.transcript;
    let mut cursor = recent.next_before_seq;
    let mut pages = Vec::new();
    while let Some(before_seq) = cursor {
        let older = client
            .call::<ThreadsGet>(ThreadsGetParams {
                before_seq: Some(before_seq),
                through_seq: Some(barrier),
                whole_turns: Some(true),
                transcript_only: Some(true),
                ..params.clone()
            })
            .await
            .unwrap();
        assert_eq!(older.thread.last_seq, barrier);
        assert!(older.provider_commands.is_empty() && older.runtime_tasks.is_empty() && older.pending_approvals.is_empty());
        pages.push(page_turns(&older.transcript));
        cursor = older.next_before_seq;
        let mut entries = older.transcript;
        entries.append(&mut collected);
        collected = entries;
    }
    // The first older page finishes the newest turn and reaches into the
    // 900-row turn, so it brings that whole turn too; then the oldest turn.
    assert_eq!(pages, [vec![turns[1], turns[2]], vec![turns[0]]]);
    assert_eq!(serde_json::to_value(collected).unwrap(), serde_json::to_value(full.transcript).unwrap());

    // Without the new fields an older page keeps the legacy shape.
    let legacy = client
        .call::<ThreadsGet>(ThreadsGetParams { before_seq: recent.next_before_seq, through_seq: Some(barrier), ..params })
        .await
        .unwrap();
    assert_eq!(legacy.transcript.len(), 3);
    assert_eq!(legacy.provider_commands.len(), 1);
}

/// Local wire workload; not a physical-device, WAN, CPU or energy benchmark.
#[tokio::test]
#[ignore]
async fn measure_remote_history_pages() {
    let host = Host::start().await;
    let thread = host.thread();
    for index in 0..2000 {
        host.state
            .store
            .event_append(
                thread.id,
                Some(thread.id),
                EventPayload::ProviderNotice {
                    data: None,
                    level: NoticeLevel::Info,
                    text: format!("Entry {index}: {}", "readable output ".repeat(400)),
                },
            )
            .unwrap();
    }
    let client = host.client().await;
    for (name, limit) in [
        ("recent-cold", Some(60)),
        ("recent-warm", Some(60)),
        ("earlier-warm", Some(120)),
        ("full-warm", None),
        ("recent-warm-repeat", Some(60)),
    ] {
        let start = std::time::Instant::now();
        let result = client
            .call::<ThreadsGet>(ThreadsGetParams { thread_id: thread.id, transcript_limit: limit, ..Default::default() })
            .await
            .unwrap();
        let elapsed = start.elapsed();
        eprintln!(
            "{}",
            json!({"workload": name, "rpc_ms": elapsed.as_secs_f64() * 1000.0, "response_bytes": serde_json::to_vec(&result).unwrap().len(), "entries": result.transcript.len()})
        );
    }
}

#[tokio::test]
async fn chat_file_links_read_the_connected_thread_workspace() {
    let host = Host::start().await;
    let mut thread = host.thread();
    let worktree = host.root.join("worktree");
    std::fs::create_dir_all(worktree.join("docs")).unwrap();
    std::fs::write(host.root.join("2026-09-10-hermes-prompt.md"), "wrong project copy").unwrap();
    let content = "# Hermes prompt\n\nRead this from the conversation workspace.";
    std::fs::write(worktree.join("2026-09-10-hermes-prompt.md"), content).unwrap();
    std::fs::write(worktree.join("docs/My File.ts"), "const connected = true;\n").unwrap();
    std::fs::write(worktree.join("binary.bin"), b"\0binary").unwrap();
    std::fs::write(worktree.join(".env"), "PRIVATE=fixture").unwrap();
    thread.cwd = worktree.to_string_lossy().into_owned();
    host.state.store.thread_upsert(&thread).unwrap();
    let client = host.client().await;
    for path in [
        "2026-09-10-hermes-prompt.md".to_string(),
        "./docs/../2026-09-10-hermes-prompt.md".into(),
        worktree.join("2026-09-10-hermes-prompt.md").to_string_lossy().into_owned(),
    ] {
        let file = client
            .call::<ThreadFileRead>(ThreadFileReadParams { thread_id: thread.id, path, max_bytes: 128000, if_version: None })
            .await
            .unwrap();
        assert_eq!(file.content, content);
        assert!(!file.truncated);
    }
    let code = client
        .call::<ThreadFileRead>(ThreadFileReadParams {
            thread_id: thread.id,
            path: "docs/My File.ts".into(),
            max_bytes: 5,
            if_version: None,
        })
        .await
        .unwrap();
    assert_eq!(code.content, "const");
    assert!(code.truncated);
    assert!(
        client
            .call::<ThreadFileRead>(ThreadFileReadParams {
                thread_id: thread.id,
                path: "binary.bin".into(),
                max_bytes: 128000,
                if_version: None
            })
            .await
            .unwrap()
            .binary
    );
    for path in ["../2026-09-10-hermes-prompt.md", ".env", "missing.md"] {
        assert!(
            client
                .call::<ThreadFileRead>(ThreadFileReadParams {
                    thread_id: thread.id,
                    path: path.into(),
                    max_bytes: 128000,
                    if_version: None
                })
                .await
                .is_err()
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(host.root.join("2026-09-10-hermes-prompt.md"), worktree.join("escape.md")).unwrap();
        assert!(
            client
                .call::<ThreadFileRead>(ThreadFileReadParams {
                    thread_id: thread.id,
                    path: "escape.md".into(),
                    max_bytes: 128000,
                    if_version: None
                })
                .await
                .is_err()
        );
    }
    // Saving goes through the same workspace; a stale version is a conflict.
    let code = client
        .call::<ThreadFileRead>(ThreadFileReadParams {
            thread_id: thread.id,
            path: "docs/My File.ts".into(),
            max_bytes: 128000,
            if_version: None,
        })
        .await
        .unwrap();
    let saved = client
        .call::<ThreadFileWrite>(ThreadFileWriteParams {
            thread_id: thread.id,
            path: "docs/My File.ts".into(),
            content: "const connected = false;\n".into(),
            expected_version: code.version.clone(),
        })
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(worktree.join("docs/My File.ts")).unwrap(), "const connected = false;\n");
    let stale = client
        .call::<ThreadFileWrite>(ThreadFileWriteParams {
            thread_id: thread.id,
            path: "docs/My File.ts".into(),
            content: "lost".into(),
            expected_version: code.version,
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(stale.contains(&format!("code {}", codes::CONFLICT)), "{stale}");
    for path in [".env", "../2026-09-10-hermes-prompt.md"] {
        assert!(
            client
                .call::<ThreadFileWrite>(ThreadFileWriteParams {
                    thread_id: thread.id,
                    path: path.into(),
                    content: "x".into(),
                    expected_version: None
                })
                .await
                .is_err()
        );
    }
    let unchanged = client
        .call::<ThreadFileRead>(ThreadFileReadParams {
            thread_id: thread.id,
            path: "docs/My File.ts".into(),
            max_bytes: 128000,
            if_version: Some(saved.version),
        })
        .await
        .unwrap();
    assert!(unchanged.unchanged);
    let other = Host::start().await;
    assert!(
        other
            .client()
            .await
            .call::<ThreadFileRead>(ThreadFileReadParams {
                thread_id: thread.id,
                path: "2026-09-10-hermes-prompt.md".into(),
                max_bytes: 128000,
                if_version: None,
            })
            .await
            .is_err()
    );
}

#[tokio::test]
async fn tool_hydration_preserves_snapshot_and_reused_call_identity_over_real_rpc() {
    let host = Host::start().await;
    let thread = host.thread();
    let store = &host.state.store;
    let turn = TurnId::new_v4();
    let append = |payload| store.event_append(thread.id, Some(turn), payload).unwrap();
    append(EventPayload::TurnStarted { message_id: MessageId::new_v4(), message: UserMessage::text("fixture") });
    let start = || EventPayload::ToolCallStarted {
        call: ToolCall { id: "reused:é😀".into(), name: "Read".into(), input: json!({}), parent_id: None },
        origin: EventOrigin::Root,
    };
    let first = append(start()).seq;
    let a = json!({"stdout": "first é😀\n".repeat(1000)});
    let barrier = append(EventPayload::ToolCallCompleted {
        tool_call_id: "reused:é😀".into(),
        output: a.clone(),
        output_omitted: false,
        stream_recoverable: false,
        is_error: true,
    })
    .seq;
    let corrected = json!({"stdout": "corrected é😀\n".repeat(1000)});
    append(EventPayload::ToolCallCompleted {
        tool_call_id: "reused:é😀".into(),
        output: corrected.clone(),
        output_omitted: false,
        stream_recoverable: false,
        is_error: false,
    });
    let second = append(start()).seq;
    let b = json!({"stdout": "second é😀\n".repeat(1000)});
    append(EventPayload::ToolCallCompleted {
        tool_call_id: "reused:é😀".into(),
        output: b.clone(),
        output_omitted: false,
        stream_recoverable: false,
        is_error: false,
    });
    let client = host.client().await;
    let params = ThreadsGetParams { thread_id: thread.id, transcript_limit: Some(60), through_seq: Some(barrier), ..Default::default() };
    let historical = client.call::<ThreadsGet>(params.clone()).await.unwrap();
    assert!(
        matches!(historical.transcript.last(), Some(TranscriptEntry::ToolCall { output: Some(value), is_error: true, output_omitted: false, .. }) if value == &a)
    );
    let omitted = client.call::<ThreadsGet>(ThreadsGetParams { include_tool_output: Some(false), ..params }).await.unwrap();
    assert!(matches!(omitted.transcript.last(), Some(TranscriptEntry::ToolCall { output: None, output_omitted: true, .. })));
    let exact = ThreadsToolOutputParams {
        thread_id: thread.id,
        tool_call_id: "reused:é😀".into(),
        start_seq: Some(first),
        through_seq: Some(barrier),
        include_tool_stream: None,
    };
    let result = client.call::<ThreadsToolOutput>(exact.clone()).await.unwrap();
    assert_eq!(result.output, a);
    assert!(result.is_error);
    let current = client.call::<ThreadsGet>(ThreadsGetParams { thread_id: thread.id, ..Default::default() }).await.unwrap();
    let outputs: Vec<_> = current
        .transcript
        .iter()
        .filter_map(|row| match row {
            TranscriptEntry::ToolCall { output, .. } => output.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(outputs, [corrected, b]);
    assert!(client.call::<ThreadsToolOutput>(ThreadsToolOutputParams { start_seq: Some(second), ..exact.clone() }).await.is_err());
    assert!(client.call::<ThreadsToolOutput>(ThreadsToolOutputParams { start_seq: Some(-1), ..exact.clone() }).await.is_err());
    assert!(client.call::<ThreadsToolOutput>(ThreadsToolOutputParams { thread_id: host.thread().id, ..exact.clone() }).await.is_err());
    // Reopening the real migrated database preserves lookup semantics.
    let reopened = kybern_store::Store::open(&host.state.paths.db).unwrap();
    assert_eq!(reopened.tool_call_output_through(thread.id, &exact.tool_call_id, Some(first), barrier).unwrap(), Some((a, true)));
    store.project_delete(thread.project_id).unwrap();
    assert!(client.call::<ThreadsToolOutput>(exact).await.is_err(), "deleted content is not resurrected");
}

#[tokio::test]
async fn previews_open_serve_grant_and_close_over_rpc_and_http() {
    let host = Host::start().await;
    let mut thread = host.thread();
    let client = host.client().await;
    // The fixture project is the data directory itself, which is never served:
    // its pages ask for a folder that cannot be granted.
    std::fs::write(host.root.join("in-data.html"), "<p>x</p>").unwrap();
    let in_data = client
        .call::<PreviewOpen>(PreviewOpenParams { thread_id: thread.id, target: "in-data.html".into(), allow_folder: true, proxy: false })
        .await;
    assert!(in_data.is_err(), "the data directory (daemon.token) is never a preview root");
    // Work in a project folder outside the data directory from here on.
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target").join(format!("preview-e2e-project-{}", Uuid::now_v7()));
    std::fs::create_dir_all(&project).unwrap();
    let project = project.canonicalize().unwrap();
    thread.cwd = project.to_string_lossy().into_owned();
    host.state.store.thread_upsert(&thread).unwrap();
    let http = reqwest::Client::new();
    let get = |path: String| {
        let http = http.clone();
        let url = format!("{}{path}", host.url);
        async move { http.get(url).send().await.unwrap() }
    };
    let open = |target: &str, allow_folder: bool| {
        client.call::<PreviewOpen>(PreviewOpenParams { thread_id: thread.id, target: target.into(), allow_folder, proxy: false })
    };

    // Inside the project: no card, page plus relative assets load.
    std::fs::create_dir_all(project.join("mock")).unwrap();
    std::fs::write(project.join("mock/index.html"), "<html><head><link rel=stylesheet href=style.css></head><body>hi</body></html>")
        .unwrap();
    std::fs::write(project.join("mock/style.css"), "body{color:red}").unwrap();
    std::fs::write(project.join(".env"), "SECRET=1").unwrap();
    let inside = open("mock/index.html", false).await.unwrap();
    assert!(inside.needs_permission.is_none());
    let ticket = inside.ticket.clone().unwrap();
    let page_path = inside.path.clone().unwrap();
    assert_eq!(page_path, format!("/preview-files/{ticket}/mock/index.html"));
    let page = get(page_path.clone()).await;
    assert_eq!(page.status(), 200);
    assert!(page.headers()["content-security-policy"].to_str().unwrap().contains("sandbox allow-scripts allow-forms allow-modals"));
    assert!(page.text().await.unwrap().contains("kybern-preview"), "bridge injected");
    let css = get(format!("/preview-files/{ticket}/mock/style.css")).await;
    assert_eq!(css.headers()["content-type"], "text/css; charset=utf-8");
    assert_eq!(get(format!("/preview-files/{ticket}/.env")).await.status(), 404);
    assert_eq!(get(format!("/preview-files/{ticket}/mock/%2e%2e/.env")).await.status(), 404);
    assert_eq!(get("/preview-files/not-a-ticket/mock/index.html".into()).await.status(), 404);

    // Closing revokes, and closing again is fine.
    client.call::<PreviewClose>(PreviewCloseParams { ticket: ticket.clone() }).await.unwrap();
    client.call::<PreviewClose>(PreviewCloseParams { ticket: ticket.clone() }).await.unwrap();
    assert_eq!(get(page_path).await.status(), 404);

    // Servers and external addresses are classified without tickets.
    match open("localhost:5173", false).await.unwrap().target {
        PreviewTargetInfo::Server { port, .. } => assert_eq!(port, 5173),
        other => panic!("{other:?}"),
    }
    assert!(matches!(open("https://example.com", false).await.unwrap().target, PreviewTargetInfo::External { .. }));
    for bad in ["javascript:alert(1)", "missing.html", "./mock/style.css"] {
        assert!(open(bad, false).await.is_err(), "{bad}");
    }

    // Proxy tickets exist only for live loopback listeners.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let live = format!("localhost:{}", listener.local_addr().unwrap().port());
    let proxied = client
        .call::<PreviewOpen>(PreviewOpenParams { thread_id: thread.id, target: live, allow_folder: false, proxy: true })
        .await
        .unwrap();
    assert_eq!(proxied.path.unwrap(), format!("/preview-proxy/{}/", proxied.ticket.unwrap()));
    drop(listener);
    // Never a proxy into the daemon's own API.
    let own = format!("localhost:{}", host.state.port.load(std::sync::atomic::Ordering::Relaxed));
    let own = client.call::<PreviewOpen>(PreviewOpenParams { thread_id: thread.id, target: own, allow_folder: false, proxy: true }).await;
    assert!(own.is_err(), "proxying the daemon itself must be refused");
    let closed = client
        .call::<PreviewOpen>(PreviewOpenParams { thread_id: thread.id, target: "localhost:1".into(), allow_folder: false, proxy: true })
        .await;
    assert!(closed.is_err());

    // Outside the project: a card first, then a persisted grant.
    // Under the build's target folder, not the system temp folder: macOS's `/private/var` is a system
    // folder that previews (rightly) refuse to grant.
    let outside = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target").join(format!("preview-e2e-{}", Uuid::now_v7()));
    std::fs::create_dir_all(outside.join("deeper")).unwrap();
    let outside = outside.canonicalize().unwrap();
    std::fs::write(outside.join("page.html"), "<p>outside</p>").unwrap();
    std::fs::write(outside.join("deeper/other.html"), "<p>deeper</p>").unwrap();
    let page = outside.join("page.html").to_string_lossy().into_owned();
    let asked = open(&page, false).await.unwrap();
    assert!(asked.ticket.is_none() && asked.path.is_none());
    assert_eq!(asked.needs_permission, Some(PreviewFolderRequest { folder: outside.to_string_lossy().into_owned(), grantable: true }));
    assert!(host.state.settings.get().preview_allowed_folders.is_empty());
    let granted = open(&page, true).await.unwrap();
    assert_eq!(get(granted.path.unwrap()).await.status(), 200);
    assert_eq!(host.state.settings.get().preview_allowed_folders, vec![outside.to_string_lossy().into_owned()]);
    // Survives a restart, covers descendants, shows no card again.
    let reloaded = crate::settings::SettingsStore::load(&host.state.paths.settings).unwrap();
    assert_eq!(reloaded.get().preview_allowed_folders, vec![outside.to_string_lossy().into_owned()]);
    assert!(open(&page, false).await.unwrap().needs_permission.is_none());
    let nested = open(&outside.join("deeper/other.html").to_string_lossy(), false).await.unwrap();
    assert!(nested.needs_permission.is_none() && nested.ticket.is_some());
    // Non-grantable folders stay refused even when the user said yes.
    let refused = open("/etc/hosts", true).await.unwrap_err().to_string();
    assert!(!refused.is_empty());
    let _ = std::fs::remove_dir_all(&project);
    let _ = std::fs::remove_dir_all(&outside);
}

#[tokio::test]
async fn the_coordinator_and_project_brief_work_over_rpc() {
    let host = Host::start().await;
    let project_id = host.thread().project_id;
    let client = host.client().await;
    assert!(client.call::<CoordinatorGet>(CoordinatorGetParams { project_id }).await.unwrap().is_none());

    let open = |brief: Option<&str>| CoordinatorOpenParams {
        project_id,
        provider: ProviderInstance::default_for(ProviderKind::ClaudeCode),
        model: None,
        effort: None,
        permission_mode: None,
        opening_message: None,
        initial_brief: brief.map(str::to_owned),
        thread_id: None,
        message_id: None,
    };
    // The user's brief exists before the Coordinator does, so no setup interview starts a real agent.
    client
        .call::<NotesCreate>(NotesCreateParams {
            scope: NoteScope::Project,
            project_id: Some(project_id),
            title: None,
            body: Some("Ship it carefully.".into()),
            project_role: Some(ProjectNoteRole::Brief),
            category: None,
        })
        .await
        .unwrap();
    let opened = client.call::<CoordinatorOpen>(open(None)).await.unwrap();
    assert!(opened.created && opened.thread.coordinator_project_id == Some(project_id));
    let again = client.call::<CoordinatorOpen>(open(None)).await.unwrap();
    assert!(!again.created && again.thread.id == opened.thread.id);
    assert_eq!(client.call::<CoordinatorGet>(CoordinatorGetParams { project_id }).await.unwrap().unwrap().id, opened.thread.id);

    // The brief is a pinned project note with a role; the user edits it through the notes RPCs.
    let notes = client.call::<NotesList>(NotesListParams::default()).await.unwrap().notes;
    let brief = notes.iter().find(|note| note.project_role == Some(ProjectNoteRole::Brief)).expect("the brief note");
    assert!(brief.pinned && brief.project_id == Some(project_id) && brief.created_by_thread.is_none());
    let second = client
        .call::<NotesCreate>(NotesCreateParams {
            scope: NoteScope::Project,
            project_id: Some(project_id),
            title: None,
            body: Some("Another".into()),
            project_role: Some(ProjectNoteRole::Brief),
            category: None,
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(second.contains("already has that note"), "{second}");
    let edited = client
        .call::<NotesUpdate>(NotesUpdateParams {
            id: Some(brief.id),
            thread_id: None,
            expected_revision: brief.revision,
            title: None,
            body: Some("Ship it carefully, and propose first.".into()),
            category: None,
            clear_category: false,
        })
        .await
        .unwrap();
    assert_eq!(edited.summary.project_role, Some(ProjectNoteRole::Brief));
    let delete = client.call::<NotesDelete>(NotesIdParams { id: brief.id }).await.unwrap_err().to_string();
    assert!(delete.contains("cannot be deleted"), "{delete}");

    // The switch that shares the brief with every thread.
    let update = |on| ProjectsUpdateParams {
        project_id,
        name: None,
        worktrees_default: None,
        task_prefix: None,
        brief_in_all_threads: on,
        coordinator: None,
        space_id: None,
        members: None,
        worktree_copy: None,
        instructions_offer_dismissed: None,
    };
    assert!(!client.call::<ProjectsUpdate>(update(None)).await.unwrap().brief_in_all_threads);
    assert!(client.call::<ProjectsUpdate>(update(Some(true))).await.unwrap().brief_in_all_threads);
    assert!(client.call::<ProjectsUpdate>(update(None)).await.unwrap().brief_in_all_threads, "absent leaves the switch alone");
    assert!(!client.call::<ProjectsUpdate>(update(Some(false))).await.unwrap().brief_in_all_threads);

    // Archiving the Coordinator frees the slot; the old collaboration methods are gone.
    client.call::<ThreadsArchive>(ThreadsArchiveParams { thread_id: opened.thread.id }).await.unwrap();
    assert!(client.call::<CoordinatorGet>(CoordinatorGetParams { project_id }).await.unwrap().is_none());
    let gone =
        client.call_raw("collaboration.coordinator.get", serde_json::json!({"project_id": project_id})).await.unwrap_err().to_string();
    assert!(gone.contains("-32601") || gone.to_lowercase().contains("method"), "{gone}");
}

#[tokio::test]
async fn project_coordinator_overrides_are_set_kept_and_cleared_over_rpc() {
    let host = Host::start().await;
    let project_id = host.thread().project_id;
    let client = host.client().await;
    let update = |extra: Value| {
        let mut params = json!({ "project_id": project_id });
        params.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let client = &client;
        async move { client.call_raw(ProjectsUpdate::NAME, params).await.map(|value| serde_json::from_value::<Project>(value).unwrap()) }
    };
    assert!(update(json!({})).await.unwrap().coordinator.is_none());

    let set = update(json!({ "coordinator": {
        "lead_autonomy": "propose_first",
        "lead_max_children": 3,
        "role_defaults": { "review": { "provider": { "kind": "codex", "instance": "default" }, "model": "m" } }
    } }))
    .await
    .unwrap();
    let overrides = set.coordinator.clone().expect("the override");
    assert_eq!((overrides.lead_autonomy, overrides.lead_max_children), (Some(LeadAutonomy::ProposeFirst), Some(3)));
    assert_eq!(overrides.role_defaults[&DelegationRole::Review].model.as_deref(), Some("m"));
    assert_eq!(overrides.lead_workspace, None);

    // Absent leaves it; an object replaces it as a whole; null removes it.
    assert_eq!(update(json!({ "name": "Renamed" })).await.unwrap().coordinator, set.coordinator);
    let replaced = update(json!({ "coordinator": { "lead_workspace": "worktree" } })).await.unwrap().coordinator.unwrap();
    assert_eq!((replaced.lead_workspace, replaced.lead_autonomy), (Some(DelegationWorkspace::Worktree), None));
    assert!(update(json!({ "coordinator": null })).await.unwrap().coordinator.is_none());
    assert!(
        client
            .call::<ProjectsList>(ProjectsListParams::default())
            .await
            .unwrap()
            .projects
            .iter()
            .all(|project| project.coordinator.is_none())
    );

    // Overrides the daemon could not honor are refused with a way forward.
    for bad in [json!({ "lead_max_children": 99 }), json!({ "role_defaults": { "general": { "model": "m" } } })] {
        let error = update(json!({ "coordinator": bad })).await.unwrap_err().to_string();
        assert!(error.contains(&format!("code {}", codes::INVALID_PARAMS)), "{error}");
    }
    // Global defaults are validated the same way.
    let mut settings = client.call::<SettingsGet>(Empty {}).await.unwrap();
    settings.orchestration.role_defaults.insert(DelegationRole::General, RoleDefault { model: Some("m".into()), ..Default::default() });
    assert!(client.call::<SettingsUpdate>(SettingsUpdateParams { settings }).await.is_err());
}

/// A folder in the home folder for one test, removed when it ends.
struct HomeFolder(std::path::PathBuf);

impl Drop for HomeFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ADE-122: "if it shows an image then show the image regardless of path". Absolute and `~/` references outside the
// thread's folder render; what is refused is a file that is not an image, a folder, and a path with nothing there.
#[tokio::test]
async fn the_image_route_serves_images_by_content_from_any_path() {
    let host = Host::start().await;
    let thread = host.thread();
    let http = reqwest::Client::new();
    let url = format!("{}/threads/{}/image", host.url, thread.id);
    let get = |path: String| {
        let request = http.get(&url).bearer_auth(&host.state.bootstrap_token).query(&[("path", path)]);
        async move {
            let response = request.send().await.unwrap();
            let media = response.headers().get("content-type").and_then(|value| value.to_str().ok()).map(str::to_string);
            (response.status(), media)
        }
    };
    let png = b"\x89PNG\r\n\x1a\nfixture";
    // Absolute, in another checkout: no message names it.
    let elsewhere = outside_folder();
    std::fs::write(elsewhere.join("shot.png"), png).unwrap();
    assert_eq!(get(elsewhere.join("shot.png").to_string_lossy().into()).await, (reqwest::StatusCode::OK, Some("image/png".into())));
    // A JPEG named `.png` is served as what it is.
    std::fs::write(elsewhere.join("photo.png"), [0xff, 0xd8, 0xff, 0xe0, 0, 0]).unwrap();
    assert_eq!(get(elsewhere.join("photo.png").to_string_lossy().into()).await.1.as_deref(), Some("image/jpeg"));
    // `~/`, in the home folder.
    let home = directories::BaseDirs::new().unwrap().home_dir().to_path_buf();
    let folder = HomeFolder(home.join(format!(".kybern-image-route-test-{}", Uuid::now_v7())));
    std::fs::create_dir_all(&folder.0).unwrap();
    std::fs::write(folder.0.join("home shot.png"), png).unwrap();
    let name = folder.0.file_name().unwrap().to_string_lossy().to_string();
    assert_eq!(get(format!("~/{name}/home shot.png")).await.0, reqwest::StatusCode::OK);
    // Not an image by content: refused, outside or not.
    std::fs::write(elsewhere.join("notes.png"), b"SECRET=1").unwrap();
    assert_eq!(get(elsewhere.join("notes.png").to_string_lossy().into()).await.0, reqwest::StatusCode::UNSUPPORTED_MEDIA_TYPE);
    std::fs::write(folder.0.join("config"), b"token = 1").unwrap();
    assert_eq!(get(format!("~/{name}/config")).await.0, reqwest::StatusCode::UNSUPPORTED_MEDIA_TYPE);
    // Missing, or a folder: 404.
    assert_eq!(get(elsewhere.join("missing.png").to_string_lossy().into()).await.0, reqwest::StatusCode::NOT_FOUND);
    assert_eq!(get("../no-such-folder/x.png".into()).await.0, reqwest::StatusCode::NOT_FOUND);
    assert_eq!(get(elsewhere.to_string_lossy().into()).await.0, reqwest::StatusCode::NOT_FOUND);
    // A thread whose folder is gone (a removed worktree) still shows an absolute reference.
    let mut gone = thread.clone();
    gone.id = Uuid::now_v7();
    gone.cwd = elsewhere.join("removed-worktree").to_string_lossy().to_string();
    host.state.store.thread_upsert(&gone).unwrap();
    let response = http
        .get(format!("{}/threads/{}/image", host.url, gone.id))
        .bearer_auth(&host.state.bootstrap_token)
        .query(&[("path", elsewhere.join("shot.png").to_string_lossy().as_ref())])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
}

// ---- spaces ----

async fn next_spaces_changed(client: &Client) -> SpacesChangedNotification {
    let mut notifications = client.notifications.lock().await;
    loop {
        let notification =
            tokio::time::timeout(Duration::from_secs(3), futures::StreamExt::next(&mut *notifications)).await.unwrap().unwrap();
        if notification.method == SPACES_CHANGED_NOTIFICATION {
            return serde_json::from_value(notification.params).unwrap();
        }
    }
}

fn add_params(path: &std::path::Path, space_id: Option<SpaceId>) -> ProjectsAddParams {
    ProjectsAddParams { path: path.to_string_lossy().into_owned(), name: None, space_id, kind: None, members: None }
}

fn space_create(name: &str) -> SpacesCreateParams {
    SpacesCreateParams { name: name.into(), color: None, logo: None }
}

fn space_update(space_id: SpaceId) -> SpacesUpdateParams {
    SpacesUpdateParams { space_id, name: None, color: None, logo: None }
}

async fn rpc_error<M: Method>(client: &Client, params: M::Params) -> String {
    client.call_raw(M::NAME, serde_json::to_value(params).unwrap()).await.unwrap_err().to_string()
}

impl Host {
    /// Store raw bytes as an uploaded asset, as `POST /assets` would.
    fn asset(&self, name: &str, media_type: &str, bytes: &[u8]) -> AssetId {
        let id = Uuid::now_v7();
        std::fs::create_dir_all(&self.state.paths.assets).unwrap();
        std::fs::write(self.state.paths.assets.join(id.to_string()), bytes).unwrap();
        let info =
            AssetInfo { id, name: name.into(), media_type: media_type.into(), size: bytes.len() as u64, created_at: chrono::Utc::now() };
        self.state.store.asset_insert(&info).unwrap();
        id
    }
}

fn encoded(format: image::ImageFormat, width: u32, height: u32) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(width, height, image::Rgb([200, 30, 30])))
        .write_to(&mut out, format)
        .unwrap();
    out.into_inner()
}

#[tokio::test]
async fn spaces_are_created_updated_and_announced() {
    let host = Host::start().await;
    let desktop = host.client().await;
    let phone = host.client().await;

    let initial = desktop.call::<SpacesList>(Empty {}).await.unwrap().spaces;
    assert_eq!(initial.len(), 1);
    assert_eq!(initial[0].name, "Personal");
    // A completed WebSocket handshake does not mean the server's connection task
    // has registered its notification receivers yet. An RPC response on this
    // client is the readiness barrier before another client changes the spaces.
    let phone_initial = phone.call::<SpacesList>(Empty {}).await.unwrap().spaces;
    assert_eq!(phone_initial[0].id, initial[0].id);

    let work = desktop.call::<SpacesCreate>(space_create("  Work  ")).await.unwrap();
    assert_eq!((work.name.as_str(), work.position, work.logo.clone()), ("Work", 1, SpaceLogo::Initials));
    assert_eq!(work.color, SpaceColor::Blue, "the first swatch no space uses");
    let announced = next_spaces_changed(&phone).await;
    assert_eq!(announced.spaces.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Personal", "Work"]);

    let updated = desktop
        .call::<SpacesUpdate>(SpacesUpdateParams {
            name: Some("Office".into()),
            color: Some(SpaceColor::Amber),
            logo: Some(SpaceLogo::Icon { icon: "briefcase".into() }),
            ..space_update(work.id)
        })
        .await
        .unwrap();
    assert_eq!(
        (updated.name.as_str(), updated.color, updated.logo),
        ("Office", SpaceColor::Amber, SpaceLogo::Icon { icon: "briefcase".into() })
    );
    assert_eq!(next_spaces_changed(&phone).await.spaces[1].name, "Office");

    let duplicate = rpc_error::<SpacesCreate>(&desktop, space_create("PERSONAL")).await;
    assert!(duplicate.contains("You already have a space named PERSONAL."), "{duplicate}");
    let empty = rpc_error::<SpacesCreate>(&desktop, space_create("   ")).await;
    assert!(empty.contains("Give the space a name."), "{empty}");
    let long = rpc_error::<SpacesCreate>(&desktop, space_create(&"x".repeat(33))).await;
    assert!(long.contains("Space names can be up to 32 characters."), "{long}");
    let bad_icon = rpc_error::<SpacesUpdate>(
        &desktop,
        SpacesUpdateParams { logo: Some(SpaceLogo::Icon { icon: "Not Valid".into() }), ..space_update(work.id) },
    )
    .await;
    assert!(bad_icon.contains("Icons are named with lowercase letters, numbers and dashes."), "{bad_icon}");
    let unknown = rpc_error::<SpacesUpdate>(&desktop, space_update(Uuid::now_v7())).await;
    assert!(unknown.contains("not found: space"), "{unknown}");
    // Renaming a space to its own name in another case is allowed.
    desktop.call::<SpacesUpdate>(SpacesUpdateParams { name: Some("OFFICE".into()), ..space_update(work.id) }).await.unwrap();

    let reordered = desktop.call::<SpacesReorder>(SpacesReorderParams { space_ids: vec![work.id, initial[0].id] }).await.unwrap();
    assert_eq!(reordered.spaces.iter().map(|s| s.id).collect::<Vec<_>>(), [work.id, initial[0].id]);
    let partial = rpc_error::<SpacesReorder>(&desktop, SpacesReorderParams { space_ids: vec![work.id] }).await;
    assert!(partial.contains("List every space exactly once to reorder them."), "{partial}");
}

#[tokio::test]
async fn projects_are_added_to_and_moved_between_spaces() {
    let host = Host::start().await;
    let client = host.client().await;
    let personal = client.call::<SpacesList>(Empty {}).await.unwrap().spaces[0].id;
    let work = client.call::<SpacesCreate>(space_create("Work")).await.unwrap();
    let folder = |name: &str| {
        let path = host.root.join(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    };

    let default = client.call::<ProjectsAdd>(add_params(&folder("a"), None)).await.unwrap();
    assert_eq!(default.space_id, Some(personal), "no space lands in the first one");
    let placed = client.call::<ProjectsAdd>(add_params(&folder("b"), Some(work.id))).await.unwrap();
    assert_eq!(placed.space_id, Some(work.id));
    let unknown = rpc_error::<ProjectsAdd>(&client, add_params(&folder("c"), Some(Uuid::now_v7()))).await;
    assert!(unknown.contains("not found: space"), "{unknown}");
    let again = client.call::<ProjectsAdd>(add_params(&folder("b"), Some(personal))).await.unwrap();
    assert_eq!(again.space_id, Some(work.id), "an existing path is not moved");

    let _ = next_projects_changed(&client).await;
    let moved = client
        .call::<ProjectsUpdate>(ProjectsUpdateParams {
            project_id: default.id,
            name: None,
            worktrees_default: None,
            task_prefix: None,
            brief_in_all_threads: None,
            coordinator: None,
            space_id: Some(work.id),
            members: None,
            worktree_copy: None,
            instructions_offer_dismissed: None,
        })
        .await
        .unwrap();
    assert_eq!(moved.space_id, Some(work.id));
    let mut announced = next_projects_changed(&client).await;
    while announced.projects.iter().find(|p| p.id == default.id).and_then(|p| p.space_id) != Some(work.id) {
        announced = next_projects_changed(&client).await;
    }

    let free_chat = rpc_error::<ProjectsUpdate>(
        &client,
        ProjectsUpdateParams {
            project_id: FREE_CHAT_PROJECT_ID,
            name: None,
            worktrees_default: None,
            task_prefix: None,
            brief_in_all_threads: None,
            coordinator: None,
            space_id: Some(work.id),
            members: None,
            worktree_copy: None,
            instructions_offer_dismissed: None,
        },
    )
    .await;
    assert!(free_chat.contains("Free chat isn't in a space."), "{free_chat}");
}

#[tokio::test]
async fn deleting_a_space_moves_its_projects_and_announces_both_lists() {
    let host = Host::start().await;
    let client = host.client().await;
    let personal = client.call::<SpacesList>(Empty {}).await.unwrap().spaces[0].id;
    let work = client.call::<SpacesCreate>(space_create("Work")).await.unwrap();
    let folder = host.root.join("w");
    std::fs::create_dir_all(&folder).unwrap();
    let project = client.call::<ProjectsAdd>(add_params(&folder, Some(work.id))).await.unwrap();

    let result = client.call::<SpacesDelete>(SpacesDeleteParams { space_id: work.id }).await.unwrap();
    assert_eq!((result.moved_to, result.moved_projects), (personal, 1));
    // Both lists arrive in the same tick, in either order.
    let (mut spaces, mut projects) = (None, None);
    {
        let mut notifications = client.notifications.lock().await;
        while spaces.is_none() || projects.is_none() {
            let notification =
                tokio::time::timeout(Duration::from_secs(3), futures::StreamExt::next(&mut *notifications)).await.unwrap().unwrap();
            if notification.method == SPACES_CHANGED_NOTIFICATION {
                let changed: SpacesChangedNotification = serde_json::from_value(notification.params).unwrap();
                spaces = Some(changed).filter(|c| c.spaces.len() == 1);
            } else if notification.method == PROJECTS_CHANGED_NOTIFICATION {
                let changed: ProjectsChangedNotification = serde_json::from_value(notification.params).unwrap();
                projects = Some(changed).filter(|c| c.projects.iter().any(|p| p.id == project.id && p.space_id == Some(personal)));
            }
        }
    }
    assert_eq!(spaces.unwrap().spaces[0].id, personal);
    let last = rpc_error::<SpacesDelete>(&client, SpacesDeleteParams { space_id: personal }).await;
    assert!(last.contains("You can't delete your only space."), "{last}");
}

#[tokio::test]
async fn the_default_space_cannot_be_deleted_but_can_be_renamed() {
    let host = Host::start().await;
    let client = host.client().await;
    let personal = client.call::<SpacesList>(Empty {}).await.unwrap().spaces[0].clone();
    assert!(personal.is_default);
    let work = client.call::<SpacesCreate>(space_create("Work")).await.unwrap();
    assert!(!work.is_default);

    let refused = rpc_error::<SpacesDelete>(&client, SpacesDeleteParams { space_id: personal.id }).await;
    assert!(refused.contains("Your default space can't be deleted."), "{refused}");
    assert_eq!(client.call::<SpacesList>(Empty {}).await.unwrap().spaces.len(), 2);

    let renamed = client.call::<SpacesUpdate>(SpacesUpdateParams { name: Some("Me".into()), ..space_update(personal.id) }).await.unwrap();
    assert!((renamed.name.as_str(), renamed.is_default) == ("Me", true));
    client.call::<SpacesDelete>(SpacesDeleteParams { space_id: work.id }).await.unwrap();
}

#[tokio::test]
async fn space_logos_are_derived_into_square_assets_and_validated() {
    let host = Host::start().await;
    let client = host.client().await;
    let space = client.call::<SpacesCreate>(space_create("Pictures")).await.unwrap();
    let asset_count = || std::fs::read_dir(&host.state.paths.assets).map(|d| d.count()).unwrap_or(0);

    let upload = host.asset("logo.png", "image/png", &encoded(image::ImageFormat::Png, 300, 200));
    let saved = client
        .call::<SpacesUpdate>(SpacesUpdateParams { logo: Some(SpaceLogo::Image { asset_id: upload }), ..space_update(space.id) })
        .await
        .unwrap();
    let SpaceLogo::Image { asset_id: derived } = saved.logo.clone() else { panic!("expected an image logo, got {:?}", saved.logo) };
    assert_ne!(derived, upload, "the logo is a new asset");
    let info = host.state.store.asset_get(derived).unwrap().unwrap();
    assert_eq!((info.name.as_str(), info.media_type.as_str()), ("space-logo.png", "image/png"));
    let bytes = std::fs::read(host.state.paths.assets.join(derived.to_string())).unwrap();
    let decoded = image::load_from_memory(&bytes).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (128, 128));

    // Saving the stored logo again, or the same edit, derives nothing new.
    let before = asset_count();
    let again = client
        .call::<SpacesUpdate>(SpacesUpdateParams { logo: Some(SpaceLogo::Image { asset_id: derived }), ..space_update(space.id) })
        .await
        .unwrap();
    assert_eq!(again.logo, saved.logo);
    assert_eq!(asset_count(), before);

    let message = "Space logos must be PNG, JPEG or WebP images up to 2 MB.";
    let gif = host.asset("logo.gif", "image/gif", &encoded(image::ImageFormat::Gif, 40, 40));
    let big = host.asset("big.png", "image/png", &vec![0u8; 3 * 1024 * 1024]);
    let text = host.asset("note.txt", "text/plain", b"not an image");
    for rejected in [gif, big, text, Uuid::now_v7()] {
        let error = rpc_error::<SpacesUpdate>(
            &client,
            SpacesUpdateParams { logo: Some(SpaceLogo::Image { asset_id: rejected }), ..space_update(space.id) },
        )
        .await;
        assert!(error.contains(message), "{error}");
    }
    let unchanged = client.call::<SpacesList>(Empty {}).await.unwrap().spaces.into_iter().find(|s| s.id == space.id).unwrap();
    assert_eq!(unchanged.logo, saved.logo, "a rejected logo keeps the previous one");
}

#[tokio::test]
async fn trusted_setup_enables_only_unconfigured_and_preserves_off_after_restart() {
    let host = Host::start().await;
    let client = host.client().await;
    let before = client.call::<NetworkStatusMethod>(Empty {}).await.unwrap();
    assert_eq!(before.configured, Some(false));
    let automatic = || NetworkSetEnabledParams { enabled: true, only_if_unconfigured: true };
    let on = client.call::<NetworkSetEnabled>(automatic()).await.unwrap();
    assert!(on.enabled);
    assert_eq!(on.configured, Some(true));
    let off = client.call::<NetworkSetEnabled>(NetworkSetEnabledParams { enabled: false, only_if_unconfigured: false }).await.unwrap();
    assert!(!off.enabled);
    assert!(!client.call::<NetworkSetEnabled>(automatic()).await.unwrap().enabled);
    let persisted = crate::settings::SettingsStore::load(&host.state.settings.dir().join("settings.json")).unwrap();
    assert!(persisted.get().network.configured);
    assert!(!persisted.get().network.enabled);
    assert!(client.call::<NetworkSetEnabled>(NetworkSetEnabledParams { enabled: false, only_if_unconfigured: true }).await.is_err());
}
