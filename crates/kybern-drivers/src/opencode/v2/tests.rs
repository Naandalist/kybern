use super::*;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn session(base: &str) -> (Arc<OpencodeSession>, mpsc::Receiver<DriverEvent>) {
    let (events, rx) = mpsc::channel(64);
    let mut command = crate::test_process::sleep();
    command.arg("30").kill_on_drop(true);
    let session = Arc::new(OpencodeSession {
        server: Api { http: reqwest::Client::new(), base: base.into(), password: "pw".into() },
        child: Mutex::new(command.spawn().unwrap()),
        events,
        state: Mutex::new(State::new("ses_root".into(), Vec::new())),
    });
    (session, rx)
}

fn event(ty: &str, data: Value) -> Value {
    json!({ "id": "evt", "type": ty, "data": data })
}

/// Accept `count` requests, answer each with `{"data":{"id":"msg_x"}}`, return them raw.
async fn mock(count: usize) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut requests = vec![];
        for _ in 0..count {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = vec![];
            loop {
                let mut chunk = [0; 4096];
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&chunk[..read]);
                let text = String::from_utf8_lossy(&bytes);
                if let Some(end) = text.find("\r\n\r\n") {
                    let length: usize = text[..end]
                        .lines()
                        .find_map(|line| line.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            requests.push(String::from_utf8(bytes).unwrap());
            let body = r#"{"data":{"id":"msg_x"}}"#;
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
        }
        requests
    });
    (base, server)
}

fn body(request: &str) -> Value {
    serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap()
}

#[tokio::test]
async fn a_root_turn_maps_v2_events_and_anchors_the_user_message() {
    let (base, server) = mock(1).await;
    let (session, mut rx) = session(&base);
    let handle = Handle(session.clone(), std::sync::Mutex::new(None), None);
    handle.send_message("kybern-id", &UserMessage::text("hello")).await.unwrap();
    let requests = server.await.unwrap();
    assert!(requests[0].starts_with("POST /api/session/ses_root/prompt "));
    assert!(requests[0].to_lowercase().contains("authorization: basic b3blbmnvzgu6chc="), "basic auth as opencode:pw");
    assert_eq!(body(&requests[0])["text"], "hello");
    session.state.lock().await.context_windows.insert("p/m".into(), 1000);
    let frames = [
        event("session.execution.started", json!({"sessionID":"ses_root"})),
        event("session.step.started", json!({"sessionID":"ses_root","model":{"providerID":"p","id":"m"},"assistantMessageID":"msg_a"})),
        event("session.reasoning.delta", json!({"sessionID":"ses_root","assistantMessageID":"msg_a","ordinal":0,"delta":"hm"})),
        event("session.reasoning.ended", json!({"sessionID":"ses_root","assistantMessageID":"msg_a","ordinal":0,"text":"hm"})),
        event("session.tool.input.started", json!({"sessionID":"ses_root","assistantMessageID":"msg_a","id":"call_1","name":"shell"})),
        event("session.tool.called", json!({"sessionID":"ses_root","assistantMessageID":"msg_a","id":"call_1","input":{"command":"ls"}})),
        event(
            "session.tool.success",
            json!({"sessionID":"ses_root","id":"call_1","content":[{"type":"text","text":"a.txt"}],"metadata":{"exit":0}}),
        ),
        event(
            "session.step.ended",
            json!({"sessionID":"ses_root","tokens":{"input":100,"output":10,"reasoning":5,"cache":{"read":50,"write":0}},"cost":0.5}),
        ),
        event("session.text.delta", json!({"sessionID":"ses_root","assistantMessageID":"msg_b","ordinal":1,"delta":"po"})),
        event("session.text.ended", json!({"sessionID":"ses_root","assistantMessageID":"msg_b","ordinal":1,"text":"pong"})),
        // Another session's events never reach this thread.
        event("session.text.ended", json!({"sessionID":"ses_other","assistantMessageID":"msg_c","ordinal":0,"text":"leak"})),
        event("session.execution.succeeded", json!({"sessionID":"ses_root"})),
    ];
    for frame in &frames {
        session.handle_event(frame).await;
    }
    let mut shape = Vec::new();
    while let Ok(event) = rx.try_recv() {
        shape.push(match event {
            DriverEvent::ThinkingDelta { message_id, delta, .. } => format!("think {message_id} {delta}"),
            DriverEvent::ThinkingCompleted { message_id, .. } => format!("thought {message_id}"),
            DriverEvent::ToolStarted(call) => format!("tool {} {} {}", call.name, call.id, call.input["command"]),
            DriverEvent::ToolCompleted { tool_call_id, output, is_error } => {
                format!("done {tool_call_id} {} {is_error}", output["content"])
            }
            DriverEvent::UsageUpdated(usage) => {
                let context = usage.context.unwrap();
                format!("context {}/{}", context.used_tokens, context.window_tokens)
            }
            DriverEvent::TextDelta { message_id, delta, .. } => format!("text {message_id} {delta}"),
            DriverEvent::MessageCompleted { message_id, text, .. } => format!("message {message_id} {text}"),
            DriverEvent::TurnCompleted { stop_reason, usage, cost_usd, anchors, .. } => format!(
                "turn {stop_reason:?} in={} out={} cache={} cost={cost_usd:?} anchor={:?}",
                usage.input_tokens, usage.output_tokens, usage.cache_read_tokens, anchors.turn_id
            ),
            other => panic!("unexpected {other:?}"),
        });
    }
    assert_eq!(
        shape,
        [
            "think msg_a hm",
            "thought msg_a",
            "tool shell call_1 \"ls\"",
            "done call_1 \"a.txt\" false",
            "context 165/1000",
            "text msg_b:1 po",
            "message msg_b:1 pong",
            "turn Completed in=100 out=15 cache=50 cost=Some(0.5) anchor=Some(\"msg_x\")",
        ]
    );
}

#[tokio::test]
async fn interrupted_and_failed_executions_end_the_turn() {
    let (session, mut rx) = session("http://127.0.0.1:9");
    session.handle_event(&event("session.execution.started", json!({"sessionID":"ses_root"}))).await;
    session.handle_event(&event("session.execution.interrupted", json!({"sessionID":"ses_root","reason":"user"}))).await;
    assert!(matches!(rx.recv().await, Some(DriverEvent::TurnCompleted { stop_reason: StopReason::Interrupted, .. })));
    session.handle_event(&event("session.execution.started", json!({"sessionID":"ses_root"}))).await;
    session
        .handle_event(&event(
            "session.execution.failed",
            json!({"sessionID":"ses_root","error":{"type":"provider.auth","message":"denied"}}),
        ))
        .await;
    assert!(matches!(rx.recv().await, Some(DriverEvent::TurnFailed { error }) if error == "provider.auth: denied"));
}

#[tokio::test]
async fn subagent_output_reaches_its_thread_and_never_the_root() {
    let (session, mut rx) = session("http://127.0.0.1:9");
    let frames = [
        event("session.tool.input.started", json!({"sessionID":"ses_root","id":"call_s","name":"subagent"})),
        event(
            "session.tool.called",
            json!({"sessionID":"ses_root","id":"call_s","input":{"agent":"general","description":"Look around","prompt":"List files"}}),
        ),
        event("session.created", json!({"sessionID":"ses_kid","parentID":"ses_root","title":"Look around (@general subagent)"})),
        event("session.tool.progress", json!({"sessionID":"ses_root","id":"call_s","metadata":{"sessionID":"ses_kid","status":"running"}})),
        event("session.tool.input.started", json!({"sessionID":"ses_kid","id":"c1","name":"glob"})),
        event("session.tool.called", json!({"sessionID":"ses_kid","id":"c1","input":{"pattern":"*"}})),
        event("session.tool.success", json!({"sessionID":"ses_kid","id":"c1","content":[{"type":"text","text":"a.txt"}]})),
        event("session.text.delta", json!({"sessionID":"ses_kid","assistantMessageID":"msg_k","ordinal":0,"delta":"One"})),
        event("session.text.ended", json!({"sessionID":"ses_kid","assistantMessageID":"msg_k","ordinal":0,"text":"One file."})),
        event("session.execution.succeeded", json!({"sessionID":"ses_kid"})),
    ];
    for frame in &frames {
        session.handle_event(frame).await;
    }
    let mut shape = Vec::new();
    while let Ok(event) = rx.try_recv() {
        shape.push(match event {
            DriverEvent::ToolStarted(call) => format!("root tool {}", call.name),
            DriverEvent::RuntimeTaskStarted(task) => {
                format!("task {} {} {:?} {:?}", task.id, task.title, task.tool_call_id, task.provider_type)
            }
            DriverEvent::RuntimeTaskUpdated(update) => format!("update {:?}", update.last_tool_name),
            DriverEvent::RuntimeTaskCompleted(update) => format!("completed {:?}", update.status),
            DriverEvent::SubagentOnly { task_id, event } => {
                assert_eq!(task_id, "opencode:ses_kid");
                match *event {
                    DriverEvent::ToolStarted(call) => format!("kid tool {}", call.name),
                    DriverEvent::ToolCompleted { tool_call_id, .. } => format!("kid done {tool_call_id}"),
                    DriverEvent::TextDelta { delta, .. } => format!("kid text {delta}"),
                    DriverEvent::MessageCompleted { text, .. } => format!("kid message {text}"),
                    other => panic!("unexpected {other:?}"),
                }
            }
            DriverEvent::TextDelta { .. } | DriverEvent::MessageCompleted { .. } => panic!("child prose leaked to the root"),
            other => panic!("unexpected {other:?}"),
        });
    }
    assert_eq!(
        shape,
        [
            "root tool subagent",
            "task opencode:ses_kid Look around None None",
            "task opencode:ses_kid Look around Some(\"call_s\") Some(\"task:general\")",
            "kid tool glob",
            "update Some(\"glob\")",
            "kid done c1",
            "kid text One",
            "kid message One file.",
            "completed Some(Completed)",
        ]
    );
}

#[tokio::test]
async fn permissions_and_forms_reply_to_their_native_endpoints() {
    let (base, server) = mock(4).await;
    let (session, mut rx) = session(&base);
    let handle = Handle(session.clone(), std::sync::Mutex::new(None), None);
    session
        .handle_event(&event(
            "permission.asked",
            json!({"id":"per_1","sessionID":"ses_root","action":"shell","resources":["echo hi"],"save":["echo *"],"source":{"type":"tool","messageID":"m","id":"call_1"}}),
        ))
        .await;
    let Some(DriverEvent::PermissionRequest { request_id, tool_call_id, tool_name, summary, suggestions, .. }) = rx.recv().await else {
        panic!("missing permission")
    };
    assert_eq!(
        (request_id.as_str(), tool_call_id.as_deref(), tool_name.as_str(), summary.as_str()),
        ("per_1", Some("call_1"), "shell", "run: echo hi")
    );
    assert_eq!(suggestions, vec![json!("echo *")]);
    handle.respond_permission("per_1", &ApprovalDecision::AllowAlways).await.unwrap();

    let form = json!({"id":"frm_1","sessionID":"ses_root","title":"Questions","metadata":{"kind":"question","tool":{"id":"call_q"}},"fields":[
        {"key":"q0","title":"Color","description":"Which color?","type":"string","options":[{"value":"red","label":"Red"},{"value":"blue","label":"Blue"}],"custom":true},
        {"key":"q1","type":"multiselect","description":"Which sizes?","options":[{"value":"s","label":"Small"},{"value":"l","label":"Large"}]},
        {"key":"q2","type":"boolean","title":"Ship it?"}
    ]});
    session.handle_event(&event("form.created", json!({ "form": form }))).await;
    let Some(DriverEvent::PermissionRequest { tool_name, input, .. }) = rx.recv().await else { panic!("missing question") };
    assert_eq!(tool_name, "opencode_question");
    assert_eq!(input["questions"][0]["question"], "Which color?");
    assert_eq!(input["questions"][0]["header"], "Color");
    assert_eq!(input["questions"][1]["multiple"], true);
    assert_eq!(input["questions"][2]["options"][0]["label"], "Yes");
    assert_eq!(input["questions"][2]["custom"], false);
    let approval: ApprovalRequest = serde_json::from_value(json!({"id":"00000000-0000-0000-0000-000000000001","thread_id":"00000000-0000-0000-0000-000000000002","turn_id":"00000000-0000-0000-0000-000000000003","tool_name":tool_name,"input":input,"summary":"Question","created_at":"2026-10-28T00:00:00Z"})).unwrap();
    let response = json!({"answers":[["Blue"],["Small","Large"],["Yes"]]});
    assert!(approval.validate_decision(&ApprovalDecision::Submit { response: response.clone() }).is_ok());
    handle.respond_permission("frm_1", &ApprovalDecision::Submit { response }).await.unwrap();

    session
        .handle_event(&event(
            "form.created",
            json!({ "form": {"id":"frm_2","sessionID":"ses_root","title":"Q","fields":[{"key":"q0","type":"string"}]} }),
        ))
        .await;
    assert!(matches!(rx.recv().await, Some(DriverEvent::PermissionRequest { .. })));
    handle.respond_permission("frm_2", &ApprovalDecision::Deny { reason: None }).await.unwrap();

    session
        .handle_event(&event("permission.asked", json!({"id":"per_2","sessionID":"ses_root","action":"edit","resources":["a.rs"]})))
        .await;
    assert!(matches!(rx.recv().await, Some(DriverEvent::PermissionRequest { summary, .. }) if summary == "edit: a.rs"));
    handle.respond_permission("per_2", &ApprovalDecision::Deny { reason: Some("not now".into()) }).await.unwrap();

    let requests = tokio::time::timeout(Duration::from_secs(2), server).await.unwrap().unwrap();
    assert!(requests[0].starts_with("POST /api/session/ses_root/permission/per_1/reply "));
    assert_eq!(body(&requests[0]), json!({"decision":"always","message":null}));
    assert!(requests[1].starts_with("POST /api/session/ses_root/form/frm_1/reply "));
    assert_eq!(body(&requests[1]), json!({"answer":{"q0":"blue","q1":["s","l"],"q2":true}}));
    assert!(requests[2].starts_with("DELETE /api/session/ses_root/form/frm_2 "));
    assert_eq!(body(&requests[3]), json!({"decision":"reject","message":"not now"}));
    assert!(session.state.lock().await.pending.is_empty());

    // A reply from another client withdraws the card.
    session.handle_event(&event("permission.asked", json!({"id":"per_3","sessionID":"ses_root","action":"shell","resources":["x"]}))).await;
    rx.recv().await.unwrap();
    session.handle_event(&event("permission.replied", json!({"sessionID":"ses_root","requestID":"per_3","reply":"once"}))).await;
    assert!(matches!(rx.recv().await, Some(DriverEvent::PermissionWithdrawn { request_id }) if request_id == "per_3"));
}

#[tokio::test]
async fn skills_commands_mode_and_model_use_v2_endpoints() {
    let (base, server) = mock(4).await;
    let (session, _rx) = session(&base);
    session.state.lock().await.commands = vec![ProviderCommand { name: "review".into(), description: "Review".into() }];
    let handle = Handle(session.clone(), std::sync::Mutex::new(None), None);
    let skill = UserMessage {
        parts: vec![ContentPart::Skill { name: "demo".into(), path: "/x/SKILL.md".into() }, ContentPart::Text { text: " go".into() }],
    };
    handle.send_message("a", &skill).await.unwrap();
    handle.send_message("b", &UserMessage::text("/review changed files")).await.unwrap();
    handle.set_permission_mode(PermissionMode::AcceptEdits).await.unwrap();
    handle.set_model("openrouter/openai/gpt-x#high").await.unwrap();
    let requests = tokio::time::timeout(Duration::from_secs(2), server).await.unwrap().unwrap();
    assert_eq!(body(&requests[0])["skills"], json!([{"id":"demo"}]));
    assert_eq!(body(&requests[0])["text"], "/demo go");
    assert!(requests[1].starts_with("POST /api/session/ses_root/command "));
    assert_eq!(body(&requests[1])["name"], "review");
    assert_eq!(body(&requests[1])["text"], "changed files");
    assert!(requests[2].starts_with("PATCH /api/session/ses_root "));
    assert_eq!(body(&requests[2])["permissions"][1], json!({"action":"shell","resource":"*","effect":"ask"}));
    assert_eq!(body(&requests[3]), json!({"model":{"providerID":"openrouter","id":"openai/gpt-x","variant":"high"}}));
}

#[test]
fn a_coordinator_keeps_only_kybern_tools_after_any_mode() {
    let mut bridge = NativeToolBridge {
        server_name: "kybern".into(),
        endpoint: Some("http://127.0.0.1:4199/native-tools/mcp".into()),
        authorization: Some("cap".into()),
        coordinator_instructions: None,
        guide: None,
        tools: vec![],
        restrictions: crate::NativeToolRestrictions {
            allowed_tools: vec![],
            denied_tools: vec![],
            require_enforcement: true,
            allow_read_only_builtins: false,
        },
    };
    let rules = restriction_rules(Some(&bridge));
    assert_eq!(rules, [json!({"action":"*","resource":"*","effect":"deny"}), json!({"action":"kybern_*","resource":"*","effect":"allow"})]);
    bridge.restrictions.allow_read_only_builtins = true;
    let rules = restriction_rules(Some(&bridge));
    assert!(rules.contains(&json!({"action":"read","resource":"*","effect":"allow"})));
    assert!(!rules.iter().any(|rule| rule["action"] == "edit" || rule["action"] == "shell"));
    bridge.restrictions.require_enforcement = false;
    assert!(restriction_rules(Some(&bridge)).is_empty());
    assert_eq!(ruleset(PermissionMode::FullAccess), [json!({"action":"*","resource":"*","effect":"allow"})]);
    assert_eq!(ruleset(PermissionMode::Auto).last().unwrap()["action"], "external_directory");
}

#[test]
fn parses_the_v2_skill_catalog() {
    let cwd = Path::new("/work/app");
    let skills = parse_skills(
        &[
            json!({"id":"opencode","name":"OpenCode","path":"/builtin/opencode.md","content":""}),
            json!({"id":"demo","name":"demo","description":" Demo ","path":"/work/app/.opencode/skills/demo/SKILL.md","content":""}),
            json!({"id":"Demo","name":"dup","path":"/x","content":""}),
            json!({"id":"tdd","name":"tdd","path":"/home/me/.config/opencode/skills/tdd/SKILL.md","content":""}),
        ],
        cwd,
    );
    let shape: Vec<_> = skills.iter().map(|s| (s.name.as_str(), s.display_name.as_deref(), s.scope)).collect();
    assert_eq!(
        shape,
        [("demo", None, SkillScope::Project), ("opencode", Some("OpenCode"), SkillScope::System), ("tdd", None, SkillScope::User)]
    );
    assert_eq!(skills[0].description.as_deref(), Some("Demo"));
}

#[tokio::test]
async fn steering_needs_a_running_turn_and_uses_native_delivery() {
    let (base, server) = mock(1).await;
    let (session, _rx) = session(&base);
    let handle = Handle(session.clone(), std::sync::Mutex::new(None), None);
    assert!(matches!(handle.steer("s", &UserMessage::text("also check tests")).await, Err(DriverError::Unsupported(_))));
    session.state.lock().await.active = true;
    handle.steer("s", &UserMessage::text("also check tests")).await.unwrap();
    let requests = server.await.unwrap();
    assert!(requests[0].starts_with("POST /api/session/ses_root/prompt "));
    assert_eq!(body(&requests[0])["delivery"], "steer");
    assert_eq!(body(&requests[0])["text"], "also check tests");
}

#[test]
fn model_reference_carries_an_effort_as_the_variant() {
    assert_eq!(model_ref("p/m", None), Some(json!({"providerID": "p", "id": "m"})));
    assert_eq!(model_ref("p/m", Some("high")), Some(json!({"providerID": "p", "id": "m", "variant": "high"})));
    assert_eq!(model_ref("p/m#low", Some("high")), Some(json!({"providerID": "p", "id": "m", "variant": "high"})));
    assert_eq!(model_ref("p/m#low", None), Some(json!({"providerID": "p", "id": "m", "variant": "low"})));
    assert_eq!(model_ref("p/m", Some(" ")), Some(json!({"providerID": "p", "id": "m"})));
    assert_eq!(model_ref("nomodel", Some("high")), None);
}

#[tokio::test]
async fn a_model_switch_carries_the_current_effort() {
    let (base, server) = mock(1).await;
    let (session, _rx) = session(&base);
    session.state.lock().await.model = Some("p/m".into());
    session.state.lock().await.effort = Some("xhigh".into());
    let handle = Handle(session.clone(), std::sync::Mutex::new(None), None);
    handle.set_model("q/n").await.unwrap();
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("POST /api/session/ses_root/model "));
    assert_eq!(body(&requests[0])["model"], json!({"providerID": "q", "id": "n", "variant": "xhigh"}));
    let state = session.state.lock().await;
    assert_eq!(state.model.as_deref(), Some("q/n"));
    assert_eq!(state.effort.as_deref(), Some("xhigh"));
}

#[tokio::test]
async fn an_effort_switch_resends_the_current_model_with_the_variant() {
    let (base, server) = mock(2).await;
    let (session, _rx) = session(&base);
    session.state.lock().await.model = Some("p/m".into());
    let handle = Handle(session.clone(), std::sync::Mutex::new(None), None);
    handle.set_effort("xhigh").await.unwrap();
    // A stored `#variant` must not shadow the switch: it is stripped, so the
    // new effort wins and the stored model drops its variant.
    session.state.lock().await.model = Some("p/m#low".into());
    handle.set_effort("high").await.unwrap();
    session.state.lock().await.model = None;
    assert!(matches!(handle.set_effort("high").await, Err(DriverError::Unsupported(_))));
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("POST /api/session/ses_root/model "));
    assert_eq!(body(&requests[0])["model"], json!({"providerID": "p", "id": "m", "variant": "xhigh"}));
    assert_eq!(body(&requests[1])["model"], json!({"providerID": "p", "id": "m", "variant": "high"}));
}
