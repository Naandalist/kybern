//! Integration tests against the real agent CLIs. Each test is skipped when
//! its binary is not on PATH, so CI without agents still passes. Run locally
//! with `cargo test -p kybern-drivers --test live_drivers -- --nocapture`.
//! Set KYBERN_LIVE_TESTS=1 to fail instead of skip when a binary is missing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kybern_drivers::registry::DriverRegistry;
use kybern_drivers::{DriverEvent, NativeToolBridge, NativeToolDefinition, NativeToolRestrictions, SessionConfig};
use kybern_protocol::*;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn skip_or_fail(kind: ProviderKind) -> bool {
    if which::which(kind.default_binary()).is_ok() {
        return false;
    }
    if std::env::var_os("KYBERN_LIVE_TESTS").is_some() {
        panic!("{} is not installed", kind.default_binary());
    }
    eprintln!("skipping {kind}: binary not found");
    true
}

async fn skip_or_fail_cursor() -> bool {
    let registry = DriverRegistry::with_defaults();
    let status = registry.get(ProviderKind::Cursor).unwrap().probe(None).await;
    if status.available {
        return false;
    }
    assert!(std::env::var_os("KYBERN_LIVE_TESTS").is_none(), "Cursor SDK unavailable: {:?}", status.unavailable_reason);
    eprintln!("skipping Cursor SDK: {:?}", status.unavailable_reason);
    true
}

/// `grok` is installed and signed in (`grok login` or XAI_API_KEY). Otherwise skip, or fail under KYBERN_LIVE_TESTS.
async fn grok_ready() -> bool {
    let status = DriverRegistry::with_defaults().get(ProviderKind::GrokBuild).unwrap().probe(None).await;
    let strict = std::env::var_os("KYBERN_LIVE_TESTS").is_some();
    if !status.available {
        assert!(!strict, "Grok Build unavailable: {:?}", status.unavailable_reason);
        eprintln!("skipping Grok Build: {:?}", status.unavailable_reason);
        return false;
    }
    // `grok models` only answers when signed in.
    if status.models.is_empty() && std::env::var_os("XAI_API_KEY").is_none() {
        assert!(!strict, "Grok Build is signed out");
        eprintln!("skipping Grok Build: signed out (grok models returned no models)");
        return false;
    }
    true
}

async fn run_turn(kind: ProviderKind, mode: PermissionMode) -> Vec<DriverEvent> {
    let dir = tempfile::tempdir().unwrap();
    std::process::Command::new("git").arg("-C").arg(dir.path()).arg("init").arg("-q").status().unwrap();
    let registry = DriverRegistry::with_defaults();
    let driver = registry.get(kind).expect("driver registered");
    let status = driver.probe(None).await;
    assert!(status.available, "{kind} should probe as available: {:?}", status.unavailable_reason);
    let env = if kind == ProviderKind::Cursor {
        HashMap::from([("KYBERN_CURSOR_STATE_DIR".into(), dir.path().join("sdk-state").display().to_string())])
    } else {
        HashMap::new()
    };

    let spawned = driver
        .spawn(SessionConfig {
            cwd: dir.path().to_path_buf(),
            model: None,
            effort: None,
            permission_mode: mode,
            native_tool_bridge: None,
            resume_session_id: None,
            resume_lacks_instructions: false,
            fork: false,
            rewind: None,
            binary: None,
            env: env.clone(),
        })
        .await
        .expect("spawn");
    let session = spawned.session;
    let mut events = spawned.events;
    session.send_message(&uuid::Uuid::new_v4().to_string(), &UserMessage::text("Reply with exactly the word: pong")).await.expect("send");

    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    loop {
        let ev = match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(ev)) => ev,
            Ok(None) => break,
            Err(_) => panic!("{kind}: no turn completion within 180s; events so far: {seen:?}"),
        };
        let done = matches!(ev, DriverEvent::TurnCompleted { .. } | DriverEvent::TurnFailed { .. } | DriverEvent::Exited { .. });
        if let DriverEvent::PermissionRequest { request_id, .. } = &ev {
            session.respond_permission(request_id, &ApprovalDecision::AllowOnce).await.unwrap();
        }
        seen.push(ev);
        if done {
            break;
        }
    }
    session.close().await.ok();
    if kind == ProviderKind::Cursor {
        let id = seen
            .iter()
            .find_map(|event| match event {
                DriverEvent::SessionBound { session_id, .. } => Some(session_id.clone()),
                _ => None,
            })
            .expect("SDK agent id");
        assert!(id.starts_with("cursor-sdk:"));
        let context =
            kybern_drivers::ProbeContext { cwd: Some(dir.path().into()), env: env.clone().into_iter().collect(), ..Default::default() };
        let history = driver.read_session(&context, &id).await.expect("read native SDK history");
        assert!(history.events.iter().any(|event| matches!(&event.payload, EventPayload::AssistantMessageCompleted { text, .. } if text.to_lowercase().contains("pong"))), "SDK history lost the final answer: {:?}", history.events);
        let listing = driver.list_sessions(&context, None, "").await.expect("list native SDK agents");
        assert!(listing.sessions.iter().any(|session| session.id == id));
        let resumed = driver
            .spawn(SessionConfig {
                cwd: dir.path().into(),
                model: None,
                effort: None,
                permission_mode: mode,
                native_tool_bridge: None,
                resume_session_id: Some(id.clone()),
                resume_lacks_instructions: false,
                fork: false,
                rewind: None,
                binary: None,
                env,
            })
            .await
            .expect("resume SDK agent");
        let mut events = resumed.events;
        resumed
            .session
            .send_message("resume", &UserMessage::text("Repeat the exact word from your previous final answer. Do not use tools."))
            .await
            .expect("send after resume");
        let mut replay = Vec::new();
        tokio::time::timeout(Duration::from_secs(180), async {
            while let Some(event) = events.recv().await {
                let done = matches!(event, DriverEvent::TurnCompleted { .. } | DriverEvent::TurnFailed { .. } | DriverEvent::Exited { .. });
                replay.push(event);
                if done {
                    break;
                }
            }
        })
        .await
        .expect("resumed turn completion");
        resumed.session.close().await.expect("close resumed SDK agent");
        assert_completed_with_text(kind, &replay);
        assert!(replay.iter().any(|event| matches!(event, DriverEvent::SessionBound { session_id, .. } if session_id == &id)));
    }
    seen
}

fn assert_completed_with_text(kind: ProviderKind, events: &[DriverEvent]) {
    assert!(events.iter().any(|e| matches!(e, DriverEvent::SessionBound { .. })), "{kind}: no SessionBound");
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            DriverEvent::MessageCompleted { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    let deltas: String = events
        .iter()
        .filter_map(|e| match e {
            DriverEvent::TextDelta { delta, .. } => Some(delta.clone()),
            _ => None,
        })
        .collect();
    assert!(
        text.to_lowercase().contains("pong") || deltas.to_lowercase().contains("pong"),
        "{kind}: expected pong in output, got text={text:?} deltas={deltas:?}; events={events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(e, DriverEvent::TurnCompleted { stop_reason: StopReason::Completed, .. })),
        "{kind}: turn did not complete: {events:?}"
    );
}

async fn spawn_probe_mcp() -> (String, Arc<Mutex<Vec<(Option<String>, String)>>>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let received = Arc::new(Mutex::new(Vec::new()));
    let server_received = received.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            let received = server_received.clone();
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0_u8; 4096];
                    let Ok(count) = socket.read(&mut chunk).await else { return };
                    if count == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                    let text = String::from_utf8_lossy(&bytes);
                    let Some(header_end) = text.find("\r\n\r\n") else { continue };
                    let length = text[..header_end]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= header_end + 4 + length {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&bytes);
                let header_end = request.find("\r\n\r\n").unwrap();
                let authorization = request[..header_end]
                    .lines()
                    .find_map(|line| line.split_once(':').filter(|(name, _)| name.eq_ignore_ascii_case("authorization")))
                    .map(|(_, value)| value.trim().to_string());
                let Ok(value) = serde_json::from_str::<Value>(&request[header_end + 4..]) else { return };
                let method = value.get("method").and_then(Value::as_str).unwrap_or("").to_string();
                received.lock().unwrap().push((authorization, method.clone()));
                let Some(id) = value.get("id") else {
                    let _ = socket.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                    return;
                };
                let result = match method.as_str() {
                    "initialize" => json!({
                        "protocolVersion": "2025-03-26",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "kybern-live-probe", "version": "1"}
                    }),
                    "tools/list" => json!({"tools": [{
                        "name": "kybern_thread_context",
                        "description": "Return the Kybern native bridge acceptance sentinel.",
                        "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
                    }]}),
                    "tools/call" => json!({"content": [{"type": "text", "text": "KYBERN_NATIVE_BRIDGE_OK"}]}),
                    _ => json!({}),
                };
                let response = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.len()
                );
                let _ = socket.write_all(headers.as_bytes()).await;
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    (endpoint, received, task)
}

async fn run_native_bridge_turn(kind: ProviderKind) {
    if if kind == ProviderKind::Cursor { skip_or_fail_cursor().await } else { skip_or_fail(kind) } {
        return;
    }
    if kind == ProviderKind::GrokBuild && !grok_ready().await {
        return;
    }
    let (endpoint, mcp_requests, mcp_task) = spawn_probe_mcp().await;
    let dir = tempfile::tempdir().unwrap();
    std::process::Command::new("git").arg("-C").arg(dir.path()).arg("init").arg("-q").status().unwrap();
    let strict = matches!(kind, ProviderKind::ClaudeCode | ProviderKind::Opencode | ProviderKind::Pi | ProviderKind::Omp);
    let bridge = NativeToolBridge {
        server_name: "kybern".into(),
        endpoint: Some(endpoint),
        authorization: Some("live-scoped-capability".into()),
        coordinator_instructions: Some(
            "You are the explicit Kybern test coordinator. Call kybern_thread_context exactly once before answering.".into(),
        ),
        guide: None,
        tools: vec![NativeToolDefinition {
            name: "kybern_thread_context".into(),
            description: "Return the Kybern native bridge acceptance sentinel.".into(),
            input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        }],
        restrictions: NativeToolRestrictions {
            allowed_tools: vec!["kybern_thread_context".into()],
            denied_tools: vec!["write".into(), "edit".into(), "bash".into()],
            require_enforcement: strict,
            allow_read_only_builtins: false,
        },
    };
    let driver = DriverRegistry::with_defaults().get(kind).expect("driver registered");
    let spawned = driver
        .spawn(SessionConfig {
            cwd: dir.path().into(),
            model: None,
            effort: None,
            permission_mode: if kind == ProviderKind::Pi { PermissionMode::FullAccess } else { PermissionMode::Auto },
            native_tool_bridge: Some(bridge),
            resume_session_id: None,
            resume_lacks_instructions: false,
            fork: false,
            rewind: None,
            binary: None,
            env: if kind == ProviderKind::Cursor {
                HashMap::from([("KYBERN_CURSOR_STATE_DIR".into(), dir.path().join("sdk-state").display().to_string())])
            } else {
                HashMap::new()
            },
        })
        .await
        .unwrap_or_else(|error| panic!("{kind}: native bridge spawn failed: {error}"));
    let session = spawned.session;
    let mut events = spawned.events;
    session
        .send_message(
            &uuid::Uuid::new_v4().to_string(),
            &UserMessage::text("Use the Kybern thread-context tool now. Then reply with exactly KYBERN_NATIVE_BRIDGE_OK."),
        )
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    let mut saw_app_tool = false;
    let mut completed_text = String::new();
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .unwrap_or_else(|_| panic!("{kind}: native bridge turn timed out"))
            .unwrap_or_else(|| panic!("{kind}: native bridge event stream closed"));
        match event {
            DriverEvent::AppToolRequest { request_id, name, .. } => {
                assert_eq!(name, "kybern_thread_context");
                saw_app_tool = true;
                session.respond_app_tool(&request_id, Ok(json!({"sentinel": "KYBERN_NATIVE_BRIDGE_OK"}))).await.unwrap();
            }
            DriverEvent::MessageCompleted { text, .. } => completed_text.push_str(&text),
            DriverEvent::TextDelta { delta, .. } => completed_text.push_str(&delta),
            DriverEvent::TurnCompleted { .. } => break,
            DriverEvent::TurnFailed { error } => panic!("{kind}: native bridge turn failed: {error}"),
            DriverEvent::Exited { error, .. } => panic!("{kind}: native bridge provider exited: {error:?}"),
            _ => {}
        }
    }
    session.close().await.ok();
    let requests = mcp_requests.lock().unwrap().clone();
    let saw_mcp_call = requests.iter().any(|(_, method)| method == "tools/call");
    assert!(saw_app_tool || saw_mcp_call, "{kind}: model never called the native Kybern bridge; MCP requests: {requests:?}");
    if saw_mcp_call {
        assert!(
            requests.iter().all(|(authorization, _)| authorization.as_deref() == Some("Bearer live-scoped-capability")),
            "{kind}: MCP request did not preserve scoped authorization: {requests:?}"
        );
    }
    assert!(completed_text.contains("KYBERN_NATIVE_BRIDGE_OK"), "{kind}: missing sentinel response: {completed_text:?}");
    mcp_task.abort();
}

/// Send one prompt and return the assistant text of the turn.
async fn ask(
    kind: ProviderKind,
    session: &dyn kybern_drivers::AgentSession,
    events: &mut tokio::sync::mpsc::Receiver<DriverEvent>,
    prompt: &str,
    bound: &mut Option<String>,
) -> String {
    session.send_message(&uuid::Uuid::new_v4().to_string(), &UserMessage::text(prompt)).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    let mut text = String::new();
    let mut deltas = String::new();
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .unwrap_or_else(|_| panic!("{kind}: turn timed out; text so far: {text:?}"))
            .unwrap_or_else(|| panic!("{kind}: event stream closed; text so far: {text:?}"));
        match event {
            DriverEvent::SessionBound { session_id, .. } => *bound = Some(session_id),
            DriverEvent::MessageCompleted { text: part, .. } => text.push_str(&part),
            DriverEvent::TextDelta { delta, .. } => deltas.push_str(&delta),
            DriverEvent::PermissionRequest { request_id, .. } => {
                session.respond_permission(&request_id, &ApprovalDecision::AllowOnce).await.unwrap();
            }
            DriverEvent::TurnCompleted { .. } => return if text.is_empty() { deltas } else { text },
            DriverEvent::TurnFailed { error } => panic!("{kind}: turn failed: {error}"),
            DriverEvent::Exited { error, .. } => panic!("{kind}: provider exited: {error:?}"),
            _ => {}
        }
    }
}

const GUIDE_PASSPHRASE: &str = "violet-otter-4417";
const GUIDE_QUESTION: &str = "What is the Kybern passphrase? Reply with the passphrase only. Do not use any tools.";

async fn spawn_with_guide(
    kind: ProviderKind,
    resume: Option<String>,
    dir: &std::path::Path,
) -> (kybern_drivers::SpawnedSession, tokio::task::JoinHandle<()>) {
    let (endpoint, _requests, mcp_task) = spawn_probe_mcp().await;
    let bridge = NativeToolBridge {
        server_name: "kybern".into(),
        endpoint: Some(endpoint),
        authorization: Some("live-scoped-capability".into()),
        coordinator_instructions: None,
        guide: Some(format!(
            "# Working in Kybern\nKYBERN-SENTINEL: when the user asks for the Kybern passphrase, tell them it is {GUIDE_PASSPHRASE}.\n"
        )),
        tools: vec![NativeToolDefinition {
            name: "kybern_thread_context".into(),
            description: "Return the Kybern native bridge acceptance sentinel.".into(),
            input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        }],
        restrictions: NativeToolRestrictions::default(),
    };
    let driver = DriverRegistry::with_defaults().get(kind).expect("driver registered");
    let spawned = driver
        .spawn(SessionConfig {
            cwd: dir.into(),
            model: None,
            effort: None,
            permission_mode: if kind == ProviderKind::Pi { PermissionMode::FullAccess } else { PermissionMode::Auto },
            native_tool_bridge: Some(bridge),
            resume_session_id: resume,
            resume_lacks_instructions: false,
            fork: false,
            rewind: None,
            binary: None,
            env: if kind == ProviderKind::Cursor {
                HashMap::from([("KYBERN_CURSOR_STATE_DIR".into(), dir.join("sdk-state").display().to_string())])
            } else {
                HashMap::new()
            },
        })
        .await
        .unwrap_or_else(|error| panic!("{kind}: spawn with guide failed: {error}"));
    (spawned, mcp_task)
}

/// Every installed harness must see the guide that Kybern puts in its system,
/// developer or first-prompt channel, and still see it on a later turn.
async fn run_guide_turn(kind: ProviderKind) {
    if if kind == ProviderKind::Cursor { skip_or_fail_cursor().await } else { skip_or_fail(kind) } {
        return;
    }
    if kind == ProviderKind::GrokBuild && !grok_ready().await {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::process::Command::new("git").arg("-C").arg(dir.path()).arg("init").arg("-q").status().unwrap();
    let (spawned, mcp_task) = spawn_with_guide(kind, None, dir.path()).await;
    let session = spawned.session;
    let mut events = spawned.events;
    let first = ask(kind, session.as_ref(), &mut events, GUIDE_QUESTION, &mut None).await;
    eprintln!("{kind}: first answer: {first:?}");
    assert!(first.contains(GUIDE_PASSPHRASE), "{kind}: the model did not see the guide: {first:?}");
    let second = ask(
        kind,
        session.as_ref(),
        &mut events,
        "Once more: what is the Kybern passphrase? Reply with the passphrase only. Do not use any tools.",
        &mut None,
    )
    .await;
    eprintln!("{kind}: second answer: {second:?}");
    assert!(second.contains(GUIDE_PASSPHRASE), "{kind}: the guide was lost on the second turn: {second:?}");
    session.close().await.ok();
    mcp_task.abort();
}

/// Codex drops some client-supplied developer messages when it compacts a
/// conversation; the guide must survive that, or Kybern needs `additionalContext`.
#[tokio::test]
async fn codex_keeps_the_guide_after_compaction_and_resume() {
    let kind = ProviderKind::Codex;
    if skip_or_fail(kind) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::process::Command::new("git").arg("-C").arg(dir.path()).arg("init").arg("-q").status().unwrap();
    let (spawned, mcp_task) = spawn_with_guide(kind, None, dir.path()).await;
    let session = spawned.session;
    let mut events = spawned.events;
    let mut native_id = None;
    let first = ask(kind, session.as_ref(), &mut events, "Say hello in one short sentence. Do not use any tools.", &mut native_id).await;
    eprintln!("codex: first answer: {first:?}");
    session.compact().await.expect("start compaction");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv()).await.expect("compaction timed out").expect("event stream closed");
        match event {
            DriverEvent::TurnCompleted { .. } => break,
            DriverEvent::TurnFailed { error } => panic!("compaction failed: {error}"),
            _ => {}
        }
    }
    let after = ask(kind, session.as_ref(), &mut events, GUIDE_QUESTION, &mut None).await;
    eprintln!("codex: answer after compaction: {after:?}");
    assert!(after.contains(GUIDE_PASSPHRASE), "Codex lost developerInstructions after compaction: {after:?}");
    session.close().await.ok();

    // A resumed thread receives the same developer instructions again.
    let native_id = native_id.expect("codex session id");
    let (resumed, resumed_mcp) = spawn_with_guide(kind, Some(native_id), dir.path()).await;
    let mut events = resumed.events;
    let answer = ask(kind, resumed.session.as_ref(), &mut events, GUIDE_QUESTION, &mut None).await;
    eprintln!("codex: answer after resume: {answer:?}");
    assert!(answer.contains(GUIDE_PASSPHRASE), "Codex lost developerInstructions after resume: {answer:?}");
    resumed.session.close().await.ok();
    mcp_task.abort();
    resumed_mcp.abort();
}

#[tokio::test]
async fn claude_code_receives_the_guide() {
    run_guide_turn(ProviderKind::ClaudeCode).await;
}

#[tokio::test]
async fn codex_receives_the_guide() {
    run_guide_turn(ProviderKind::Codex).await;
}

#[tokio::test]
async fn opencode_receives_the_guide() {
    run_guide_turn(ProviderKind::Opencode).await;
}

#[tokio::test]
async fn omp_receives_the_guide() {
    run_guide_turn(ProviderKind::Omp).await;
}

#[tokio::test]
async fn pi_receives_the_guide() {
    run_guide_turn(ProviderKind::Pi).await;
}

#[tokio::test]
async fn cursor_receives_the_guide_on_its_first_prompt() {
    run_guide_turn(ProviderKind::Cursor).await;
}

#[tokio::test]
async fn grok_build_receives_the_guide_on_its_first_prompt() {
    run_guide_turn(ProviderKind::GrokBuild).await;
}

#[tokio::test]
async fn installed_harnesses_round_trip_a_scoped_native_bridge_tool() {
    let selected = std::env::var("KYBERN_LIVE_PROVIDER").ok();
    for kind in ProviderKind::ALL {
        if selected.as_deref().is_some_and(|name| name != kind.as_str()) {
            continue;
        }
        // Antigravity reaches the bridge through a global `agy mcp` entry (see antigravity_mcp.rs), which a
        // scratch HOME run covers; this test has no per-session config to point it at.
        if kind == ProviderKind::Antigravity {
            continue;
        }
        run_native_bridge_turn(kind).await;
    }
}

#[tokio::test]
async fn claude_code_completes_a_turn() {
    if skip_or_fail(ProviderKind::ClaudeCode) {
        return;
    }
    let events = run_turn(ProviderKind::ClaudeCode, PermissionMode::Supervised).await;
    assert_completed_with_text(ProviderKind::ClaudeCode, &events);
}

#[tokio::test]
async fn codex_completes_a_turn() {
    if skip_or_fail(ProviderKind::Codex) {
        return;
    }
    let events = run_turn(ProviderKind::Codex, PermissionMode::Auto).await;
    assert_completed_with_text(ProviderKind::Codex, &events);
}

#[tokio::test]
async fn opencode_completes_a_turn() {
    if skip_or_fail(ProviderKind::Opencode) {
        return;
    }
    let events = run_turn(ProviderKind::Opencode, PermissionMode::Auto).await;
    assert_completed_with_text(ProviderKind::Opencode, &events);
}

#[tokio::test]
async fn omp_completes_a_turn() {
    if skip_or_fail(ProviderKind::Omp) {
        return;
    }
    let events = run_turn(ProviderKind::Omp, PermissionMode::Auto).await;
    assert_completed_with_text(ProviderKind::Omp, &events);
}

#[tokio::test]
async fn cursor_completes_a_turn() {
    if skip_or_fail_cursor().await {
        return;
    }
    let events = run_turn(ProviderKind::Cursor, PermissionMode::Auto).await;
    assert_completed_with_text(ProviderKind::Cursor, &events);
}

#[tokio::test]
async fn pi_completes_a_turn() {
    if skip_or_fail(ProviderKind::Pi) {
        return;
    }
    let events = run_turn(ProviderKind::Pi, PermissionMode::FullAccess).await;
    assert_completed_with_text(ProviderKind::Pi, &events);
}

// ---- Antigravity ----

const AGY_MODEL: &str = "gemini-3.8-flash-low";

/// `Some(driver)` when `agy` is installed and signed in; otherwise skip (or fail under KYBERN_LIVE_TESTS).
async fn antigravity_ready() -> Option<Arc<dyn kybern_drivers::AgentDriver>> {
    let driver = DriverRegistry::with_defaults().get(ProviderKind::Antigravity).expect("driver registered");
    let status = driver.probe(None).await;
    let strict = std::env::var_os("KYBERN_LIVE_TESTS").is_some();
    if !status.available {
        assert!(!strict, "Antigravity unavailable: {:?}", status.unavailable_reason);
        eprintln!("skipping Antigravity: {:?}", status.unavailable_reason);
        return None;
    }
    if status.models.is_empty() {
        // `agy models` fails when signed out.
        assert!(!strict, "Antigravity is signed out");
        eprintln!("skipping Antigravity: signed out (agy models returned no models)");
        return None;
    }
    Some(driver)
}

fn antigravity_config(dir: &std::path::Path, resume: Option<String>) -> SessionConfig {
    SessionConfig {
        cwd: dir.to_path_buf(),
        model: Some(AGY_MODEL.into()),
        effort: None,
        permission_mode: PermissionMode::FullAccess,
        native_tool_bridge: None,
        resume_session_id: resume,
        resume_lacks_instructions: false,
        fork: false,
        rewind: None,
        binary: None,
        env: HashMap::new(),
    }
}

async fn until_turn_ends(events: &mut tokio::sync::mpsc::Receiver<DriverEvent>, seconds: u64) -> Vec<DriverEvent> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        let event = match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(_) => panic!("no turn completion within {seconds}s; events so far: {seen:?}"),
        };
        let done = matches!(event, DriverEvent::TurnCompleted { .. } | DriverEvent::TurnFailed { .. } | DriverEvent::Exited { .. });
        seen.push(event);
        if done {
            break;
        }
    }
    seen
}

#[tokio::test]
async fn antigravity_streams_runs_a_tool_resumes_and_lists_models() {
    let Some(driver) = antigravity_ready().await else { return };
    let status = driver.probe(None).await;
    assert!(status.models.iter().any(|model| model.id == AGY_MODEL), "models: {:?}", status.models);
    assert_eq!(status.supported_permission_modes, vec![PermissionMode::FullAccess]);
    let dir = tempfile::tempdir().unwrap();
    let spawned = driver.spawn(antigravity_config(dir.path(), None)).await.expect("spawn");
    let (session, mut events) = (spawned.session, spawned.events);

    session
        .send_message("m1", &UserMessage::text("Run the shell command `echo hi` and then tell me its output in one short sentence."))
        .await
        .unwrap();
    let first = until_turn_ends(&mut events, 180).await;
    eprintln!("first turn: {first:#?}");
    let conversation = first
        .iter()
        .find_map(|event| match event {
            DriverEvent::SessionBound { session_id, .. } => Some(session_id.clone()),
            _ => None,
        })
        .expect("a conversation id");
    assert!(first.iter().any(|event| matches!(event, DriverEvent::TextDelta { .. })), "text streams");
    let tool = first.iter().find_map(|event| match event {
        DriverEvent::ToolStarted(call) if call.name == "Bash" => Some(call.clone()),
        _ => None,
    });
    let tool = tool.expect("one Bash tool call");
    assert!(tool.input["command"].as_str().unwrap_or("").contains("echo hi"), "{:?}", tool.input);
    assert!(
        first.iter().any(|event| matches!(event, DriverEvent::ToolCompleted { tool_call_id, output, is_error: false } if *tool_call_id == tool.id && output.as_str().unwrap_or("").contains("hi"))),
        "the tool completed with its output"
    );
    let Some(DriverEvent::TurnCompleted { stop_reason, usage, .. }) = first.last() else { panic!("{:?}", first.last()) };
    assert_eq!(*stop_reason, StopReason::Completed);
    assert!(usage.input_tokens > 0);

    // Resume: the same conversation answers from what it already knows.
    session
        .send_message(
            "m2",
            &UserMessage::text("Without using any tool: what shell command did you just run? Answer with the command only."),
        )
        .await
        .unwrap();
    let second = until_turn_ends(&mut events, 180).await;
    assert!(
        second.iter().any(|event| matches!(event, DriverEvent::SessionBound { session_id, .. } if *session_id == conversation)),
        "the second turn resumed {conversation}"
    );
    let answer = second
        .iter()
        .rev()
        .find_map(|event| match event {
            DriverEvent::MessageCompleted { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("an answer");
    assert!(answer.contains("echo"), "{answer}");
    assert!(matches!(second.last(), Some(DriverEvent::TurnCompleted { stop_reason: StopReason::Completed, .. })));
    session.close().await.unwrap();
}

/// What a resumed session says before anyone prompts it.
async fn quiet_for(events: &mut tokio::sync::mpsc::Receiver<DriverEvent>, millis: u64) -> Vec<DriverEvent> {
    let mut seen = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(millis), events.recv()).await {
        seen.push(event);
    }
    seen
}

fn grok_config(dir: &std::path::Path, resume: Option<String>) -> SessionConfig {
    SessionConfig {
        cwd: dir.to_path_buf(),
        model: None,
        effort: None,
        permission_mode: PermissionMode::FullAccess,
        native_tool_bridge: None,
        resume_session_id: resume,
        resume_lacks_instructions: false,
        fork: false,
        rewind: None,
        binary: None,
        env: HashMap::new(),
    }
}

#[tokio::test]
async fn grok_build_streams_runs_a_tool_and_lists_models() {
    if !grok_ready().await {
        return;
    }
    let driver = DriverRegistry::with_defaults().get(ProviderKind::GrokBuild).expect("driver registered");
    let status = driver.probe(None).await;
    // Only Auto and Full access work: Grok sends no permission requests Kybern could answer.
    assert_eq!(status.supported_permission_modes, vec![PermissionMode::Auto, PermissionMode::FullAccess]);
    assert!(!status.supports_model_switch, "the model is chosen at session start");
    if std::env::var_os("XAI_API_KEY").is_none() {
        assert!(!status.models.is_empty(), "`grok models` listed no models: {status:?}");
    }
    assert!(status.models.iter().all(|model| !model.id.is_empty() && !model.id.contains(' ')), "{:?}", status.models);

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "kybern-sentinel-4417 is the only word in this file.\n").unwrap();
    let spawned = driver.spawn(grok_config(dir.path(), None)).await.expect("spawn");
    let (session, mut events) = (spawned.session, spawned.events);
    session
        .send_message("m1", &UserMessage::text("Use your file tools to read notes.txt, then tell me its first word in one short sentence."))
        .await
        .unwrap();
    let first = until_turn_ends(&mut events, 240).await;
    eprintln!("first turn: {first:#?}");
    let conversation = first
        .iter()
        .find_map(|event| match event {
            DriverEvent::SessionBound { session_id, .. } => Some(session_id.clone()),
            _ => None,
        })
        .expect("a session id");
    assert!(first.iter().any(|event| matches!(event, DriverEvent::TextDelta { .. })), "text streams");
    let tool = first
        .iter()
        .find_map(|event| match event {
            DriverEvent::ToolStarted(call) => Some(call.clone()),
            _ => None,
        })
        .expect("one tool call");
    assert!(tool.name != "other" && !tool.name.is_empty(), "the tool has Grok's own name: {tool:?}");
    assert!(
        first
            .iter()
            .any(|event| matches!(event, DriverEvent::ToolCompleted { tool_call_id, is_error: false, .. } if *tool_call_id == tool.id)),
        "the tool completed"
    );
    let Some(DriverEvent::TurnCompleted { stop_reason, usage, .. }) = first.last() else { panic!("{:?}", first.last()) };
    assert_eq!(*stop_reason, StopReason::Completed);
    assert!(usage.input_tokens > 0 && usage.output_tokens > 0, "the turn reports its tokens: {usage:?}");
    session.close().await.unwrap();

    // Resume: the conversation loads without replaying its history as new output, and still answers from it.
    let resumed = driver.spawn(grok_config(dir.path(), Some(conversation.clone()))).await.expect("resume");
    let (session, mut events) = (resumed.session, resumed.events);
    let replayed = quiet_for(&mut events, 1500).await;
    assert!(
        replayed.iter().all(|event| !matches!(
            event,
            DriverEvent::TextDelta { .. }
                | DriverEvent::ThinkingDelta { .. }
                | DriverEvent::ToolStarted(_)
                | DriverEvent::ToolCompleted { .. }
        )),
        "loading a session must not replay its history: {replayed:?}"
    );
    assert!(
        replayed.iter().any(|event| matches!(event, DriverEvent::SessionBound { session_id, .. } if *session_id == conversation)),
        "the resumed session keeps its id: {replayed:?}"
    );
    session
        .send_message("m2", &UserMessage::text("Without using any tool: which file did you just read? Answer with the file name only."))
        .await
        .unwrap();
    let second = until_turn_ends(&mut events, 240).await;
    let answer: String = second
        .iter()
        .filter_map(|event| match event {
            DriverEvent::TextDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert!(answer.contains("notes"), "{answer}");
    assert!(matches!(second.last(), Some(DriverEvent::TurnCompleted { stop_reason: StopReason::Completed, .. })));
    session.close().await.unwrap();
}

#[cfg(unix)]
fn process_alive(pid: &str) -> bool {
    std::process::Command::new("/bin/kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
#[tokio::test]
async fn antigravity_interrupt_ends_the_turn_and_kills_the_tool_child() {
    let Some(driver) = antigravity_ready().await else { return };
    let dir = tempfile::tempdir().unwrap();
    // A distinctive duration so the child is easy to find among other processes.
    let marker = "sleep 31.417";
    let spawned = driver.spawn(antigravity_config(dir.path(), None)).await.expect("spawn");
    let (session, mut events) = (spawned.session, spawned.events);
    session
        .send_message("m1", &UserMessage::text(format!("Run exactly this shell command and wait for it: `{marker}`. Do nothing else.")))
        .await
        .unwrap();
    // Wait for the tool to start, then let the sleep begin.
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv()).await.expect("the tool started in time").expect("events open");
        let started = matches!(&event, DriverEvent::ToolStarted(call) if call.name == "Bash");
        seen.push(event);
        if started {
            break;
        }
    }
    tokio::time::sleep(Duration::from_secs(4)).await;
    let pids = || {
        let out = std::process::Command::new("pgrep").args(["-f", marker]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).split_whitespace().map(str::to_string).collect::<Vec<_>>()
    };
    let before = pids();
    eprintln!("sleep processes before the interrupt: {before:?}");
    assert!(!before.is_empty(), "the sleep is running: {seen:?}");
    let started = std::time::Instant::now();
    session.interrupt().await.unwrap();
    let ended = until_turn_ends(&mut events, 40).await;
    eprintln!("interrupt took {:?}: {ended:#?}", started.elapsed());
    assert!(matches!(ended.last(), Some(DriverEvent::TurnCompleted { stop_reason: StopReason::Interrupted, .. })), "{:?}", ended.last());
    tokio::time::sleep(Duration::from_millis(500)).await;
    for pid in &before {
        assert!(!process_alive(pid), "the sleep child {pid} outlived the interrupt");
    }
    // The conversation survives an interrupted turn.
    session.send_message("m2", &UserMessage::text("Reply with the single word: ready")).await.unwrap();
    let after = until_turn_ends(&mut events, 180).await;
    assert!(matches!(after.last(), Some(DriverEvent::TurnCompleted { stop_reason: StopReason::Completed, .. })), "{:?}", after.last());
    session.close().await.unwrap();
}

#[tokio::test]
async fn antigravity_one_shot_answers_a_title_prompt_without_editing() {
    let Some(driver) = antigravity_ready().await else { return };
    let dir = tempfile::tempdir().unwrap();
    let answer = driver
        .one_shot(
            dir.path(),
            "Reply with a title of at most four words for a chat about sorting arrays. Output only the title.",
            None,
            &Default::default(),
        )
        .await
        .expect("one-shot");
    eprintln!("title: {answer:?}");
    assert!(!answer.trim().is_empty());
    let created: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert!(created.is_empty(), "plan mode left the checkout alone: {created:?}");
}

#[allow(dead_code)]
fn _unused(_: PathBuf) {}
