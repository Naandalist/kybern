use super::*;

fn session(provider: ProviderKind) -> SavedSession {
    SavedSession {
        provider,
        id: "fixture-session".into(),
        title: "Fix the session picker".into(),
        cwd: "/tmp/project".into(),
        updated_at: "2026-09-01T10:00:00Z".parse().unwrap(),
        model: None,
        thread_id: None,
    }
}

pub(super) fn users(history: &SessionHistory) -> Vec<String> {
    history
        .events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::TurnStarted { message, .. } => Some(message.plain_text()),
            _ => None,
        })
        .collect()
}

pub(super) fn answers(history: &SessionHistory) -> Vec<&str> {
    history
        .events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::AssistantMessageCompleted { text, .. } if !text.is_empty() => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn claude_import_follows_the_selected_branch_and_does_not_invent_users_for_tool_results() {
    let records = vec![
        json!({"type":"user","uuid":"u","parentUuid":null,"message":{"role":"user","content":"Check the file"}}),
        json!({"type":"assistant","uuid":"a","parentUuid":"u","message":{"content":[{"type":"tool_use","id":"read1","name":"Read","input":{"file_path":"test.rs"}}]}}),
        json!({"type":"user","uuid":"r","parentUuid":"a","message":{"content":[{"type":"tool_result","tool_use_id":"read1","content":"file contents"}]}}),
        json!({"type":"assistant","uuid":"abandoned","parentUuid":"r","message":{"content":[{"type":"text","text":"Wrong branch"}]}}),
        json!({"type":"assistant","uuid":"final","parentUuid":"r","message":{"content":[{"type":"text","text":"Correct answer"}]}}),
        json!({"type":"assistant","uuid":"child","parentUuid":"final","isSidechain":true,"message":{"content":[{"type":"text","text":"Child output"}]}}),
    ];
    let history = claude::parse(session(ProviderKind::ClaudeCode), records);
    assert_eq!(users(&history), ["Check the file"]);
    assert_eq!(answers(&history), ["Correct answer"]);
    assert!(
        history
            .events
            .iter()
            .any(|e| matches!(&e.payload, EventPayload::ToolCallCompleted { output, is_error: false, .. } if output == "file contents"))
    );
    assert_eq!(history.events.iter().filter(|e| matches!(e.payload, EventPayload::TurnCompleted { .. })).count(), 1);
}

#[test]
fn pi_and_omp_preserve_active_branch_tool_errors_and_summary() {
    for kind in [ProviderKind::Pi, ProviderKind::Omp] {
        let records = vec![
            json!({"type":"session","id":"fixture-session","cwd":"/tmp/project"}),
            json!({"type":"message","id":"u","parentId":null,"message":{"role":"user","content":[{"type":"text","text":"Run checks"}]}}),
            json!({"type":"message","id":"a","parentId":"u","message":{"role":"assistant","content":[{"type":"toolCall","id":"t","name":"bash","arguments":{"command":"check"}}]}}),
            json!({"type":"message","id":"r","parentId":"a","message":{"role":"toolResult","toolCallId":"t","content":[{"type":"text","text":"check failed"}],"isError":true}}),
            json!({"type":"message","id":"old","parentId":"r","message":{"role":"assistant","content":[{"type":"text","text":"Discarded"}]}}),
            json!({"type":"compaction","id":"c","parentId":"r","summary":"Checks failed; fix next."}),
            json!({"type":"message","id":"f","parentId":"c","message":{"role":"assistant","content":[{"type":"text","text":"The check failed."}]}}),
        ];
        let history = pi::parse(session(kind), records);
        assert_eq!(users(&history), ["Run checks"]);
        assert_eq!(answers(&history), ["The check failed."]);
        assert!(history.events.iter().any(|e| matches!(e.payload, EventPayload::ToolCallCompleted { is_error: true, .. })));
        assert!(
            history
                .events
                .iter()
                .any(|e| matches!(&e.payload, EventPayload::ProviderNotice { text, .. } if text.contains("Checks failed; fix next.")))
        );
    }
}

#[test]
fn codex_import_keeps_user_content_command_output_and_final_identity() {
    let mut history = History::default();
    let source = session(ProviderKind::Codex);
    codex::parse_turn(
        &mut history,
        &source,
        &json!({"status":"completed","items":[
            {"type":"userMessage","content":[{"type":"text","text":"Read the answer"}]},
            {"type":"agentMessage","text":"Reading now"},
            {"id":"cmd1","type":"commandExecution","command":"cat answer.txt","status":"completed","exitCode":0,"aggregatedOutput":"42"},
            {"id":"f","type":"agentMessage","text":"The answer is 42."}
        ]}),
    );
    let history = history.finish(source);
    assert_eq!(users(&history), ["Read the answer"]);
    assert_eq!(answers(&history), ["Reading now", "The answer is 42."]);
    let final_id = history
        .events
        .iter()
        .find_map(|e| match &e.payload {
            EventPayload::AssistantMessageCompleted { message_id, text, .. } if text == "The answer is 42." => Some(*message_id),
            _ => None,
        })
        .unwrap();
    assert!(
        history
            .events
            .iter()
            .any(|e| matches!(&e.payload, EventPayload::TurnCompleted { terminal_message_id: Some(id), .. } if *id == final_id))
    );
    assert!(history.events.iter().any(|e| matches!(&e.payload, EventPayload::ToolCallCompleted { output, .. } if output == "42")));
}

#[test]
fn opencode_import_keeps_message_parts_and_model() {
    let history = opencode::parse(
        session(ProviderKind::Opencode),
        json!([
            {"info":{"role":"user"},"parts":[{"type":"text","text":"Read the file"}]},
            {"info":{"role":"assistant","providerID":"provider","modelID":"model"},"parts":[
                {"type":"tool","callID":"t","tool":"read","state":{"status":"completed","input":{"filePath":"file.txt"},"output":"contents"}},
                {"type":"text","text":"Here are the contents."}
            ]}
        ]),
    );
    assert_eq!(users(&history), ["Read the file"]);
    assert_eq!(answers(&history), ["Here are the contents."]);
    assert_eq!(history.session.model.as_deref(), Some("provider/model"));
    assert!(history.events.iter().any(|e| matches!(&e.payload, EventPayload::ToolCallCompleted { output, .. } if output == "contents")));
}

#[test]
fn opencode_v2_import_keeps_typed_messages_and_model() {
    let history = opencode::parse_v2(
        session(ProviderKind::Opencode),
        vec![
            json!({"type":"user","id":"msg_1","time":{"created":1793155003387_i64},"text":"Read the file"}),
            json!({"type":"assistant","model":{"providerID":"provider","id":"model"},"content":[
                {"type":"reasoning","text":"thinking"},
                {"type":"tool","id":"t","name":"read","state":{"status":"completed","input":{"path":"file.txt"},"content":[{"type":"text","text":"contents"}]}},
                {"type":"tool","id":"u","name":"shell","state":{"status":"error","input":{"command":"x"},"error":{"type":"aborted","message":"interrupted"}}},
                {"type":"text","text":"Here are the contents."}
            ]}),
            json!({"type":"idle","outcome":"succeeded"}),
        ],
    );
    assert_eq!(users(&history), ["Read the file"]);
    assert_eq!(answers(&history), ["Here are the contents."]);
    assert_eq!(history.session.model.as_deref(), Some("provider/model"));
    assert!(history.events.iter().any(|e| matches!(&e.payload, EventPayload::ToolCallCompleted { output, .. } if output == "contents")));
    assert!(
        history
            .events
            .iter()
            .any(|e| matches!(&e.payload, EventPayload::ToolCallCompleted { output, is_error: true, .. } if output == "interrupted"))
    );
}

#[test]
fn cursor_replay_coalesces_chunks_and_finishes_tools() {
    let updates = vec![
        json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"Check "}}),
        json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"the file"}}),
        json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Checking"}}),
        json!({"sessionUpdate":"tool_call","toolCallId":"t","title":"Read file","status":"in_progress","rawInput":{"path":"a.rs"}}),
        json!({"sessionUpdate":"tool_call_update","toolCallId":"t","status":"completed","rawOutput":"contents"}),
        json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"All "}}),
        json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"good."}}),
    ]
    .into_iter()
    .map(|update| json!({"method":"session/update","params":{"update":update}}))
    .collect();
    let history = cursor::parse(session(ProviderKind::Cursor), updates);
    assert_eq!(users(&history), ["Check the file"]);
    assert_eq!(answers(&history), ["Checking", "All good."]);
    assert!(
        history
            .events
            .iter()
            .any(|e| matches!(&e.payload, EventPayload::ToolCallCompleted { output, is_error: false, .. } if output == "contents"))
    );
}

#[test]
fn unfinished_native_tools_are_interrupted_instead_of_restarted() {
    let mut history = History::default();
    let source = session(ProviderKind::ClaudeCode);
    history.user(source.updated_at, UserMessage::text("Start work"));
    history.tool(source.updated_at, "t", "Bash", json!({"command":"sleep 999"}));
    let imported = history.finish(source);
    assert!(imported.events.iter().any(|e| matches!(e.payload, EventPayload::TurnCompleted { stop_reason: StopReason::Interrupted, .. })));
    assert!(imported.events.iter().any(|e| matches!(e.payload, EventPayload::ToolCallCompleted { is_error: true, .. })));
}

#[tokio::test]
async fn file_discovery_searches_beyond_first_page_and_respects_profile_root() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let dir = temp.path().join("agent/sessions/project");
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..55 {
        std::fs::write(
            dir.join(format!("session-{i}.jsonl")),
            format!(
                "{}\n{}\n",
                json!({"type":"session","id":format!("session-{i}"),"cwd":project}),
                json!({"type":"message","id":"u","parentId":null,"message":{"role":"user","content":format!("Task {i}")}})
            ),
        )
        .unwrap();
    }
    let context = ProbeContext {
        cwd: Some(project),
        env: [("PI_CODING_AGENT_DIR".into(), temp.path().join("agent").display().to_string()), ("OMP_PROFILE".into(), String::new())]
            .into(),
        ..Default::default()
    };
    for kind in [ProviderKind::Pi, ProviderKind::Omp] {
        let page = list(kind, &context, None, "").await.unwrap();
        assert_eq!(page.sessions.len(), PAGE_SIZE);
        let next = list(kind, &context, page.next_cursor.as_deref(), "").await.unwrap();
        assert_eq!(next.sessions.len(), 5);
        let found = list(kind, &context, None, "Task 54").await.unwrap();
        assert_eq!(found.sessions.len(), 1);
        let imported = read(kind, &context, &found.sessions[0].id).await.unwrap();
        assert_eq!(users(&imported), ["Task 54"]);
        assert!(read(kind, &context, "/etc/passwd").await.is_err());
    }
}

#[test]
fn corrupt_history_is_not_silently_imported() {
    let temp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(temp.path(), "{\"type\":\"session\"}\nbroken\n{\"type\":\"message\"}\n").unwrap();
    assert!(read_lines(temp.path()).is_err());
    std::fs::write(temp.path(), "{\"type\":\"session\"}\n{\"unfinished\"").unwrap();
    assert!(read_lines(temp.path()).is_err());
}

#[test]
fn claude_compaction_retains_earlier_history_without_a_synthetic_user() {
    let history = claude::parse(
        session(ProviderKind::ClaudeCode),
        vec![
            json!({"type":"user","uuid":"u","parentUuid":null,"message":{"content":"Original question"}}),
            json!({"type":"assistant","uuid":"a","parentUuid":"u","message":{"content":[{"type":"text","text":"Original answer"}]}}),
            json!({"type":"system","subtype":"compact_boundary","uuid":"c","parentUuid":null,"logicalParentUuid":"a"}),
            json!({"type":"user","uuid":"s","parentUuid":"c","isCompactSummary":true,"message":{"content":"Summary of earlier work"}}),
            json!({"type":"assistant","uuid":"f","parentUuid":"s","message":{"content":[{"type":"text","text":"Continued answer"}]}}),
        ],
    );
    assert_eq!(users(&history), ["Original question"]);
    assert_eq!(answers(&history), ["Original answer", "Continued answer"]);
}

#[tokio::test]
async fn a_file_reader_pages_all_folders_and_reads_by_the_path_it_listed() {
    let temp = tempfile::tempdir().unwrap();
    let claude_dir = temp.path().join("claude");
    let projects = claude_dir.join("projects/p");
    std::fs::create_dir_all(&projects).unwrap();
    for index in 0..(PAGE_SIZE + 5) {
        let id = format!("s{index}");
        let cwd = format!("/work/repo{}", index % 3);
        let lines = [
            json!({"type":"user","uuid":"u","parentUuid":null,"cwd":cwd,"sessionId":id,"message":{"content":format!("Task {index}")}}),
            json!({"type":"assistant","uuid":"a","parentUuid":"u","cwd":cwd,"sessionId":id,"message":{"content":[{"type":"text","text":"Done"}]}}),
        ];
        std::fs::write(projects.join(format!("{id}.jsonl")), lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
    }
    let context = ProbeContext {
        binary: None,
        cwd: None,
        env: [("HOME".to_string(), temp.path().display().to_string()), ("CLAUDE_CONFIG_DIR".to_string(), claude_dir.display().to_string())]
            .into_iter()
            .collect(),
    };
    let mut reader = reader(ProviderKind::ClaudeCode, &context).await.unwrap();
    let first = reader.list_page(None).await.unwrap();
    assert_eq!(first.sessions.len(), PAGE_SIZE);
    let second = reader.list_page(first.next_cursor.as_deref()).await.unwrap();
    assert_eq!(second.sessions.len(), 5);
    assert!(second.next_cursor.is_none());
    let ids: HashSet<_> = first.sessions.iter().chain(&second.sessions).map(|s| s.id.clone()).collect();
    assert_eq!(ids.len(), PAGE_SIZE + 5, "no folder filter: every session of every folder");
    let history = reader.read("s7").await.unwrap();
    assert_eq!(users(&history), ["Task 7"]);
    assert!(reader.read("missing").await.is_err());
    reader.close().await;
}

#[test]
fn read_failures_are_told_apart_by_cause() {
    use super::{ReadFailure, classify_read_error};
    let temp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(temp.path(), "{\"type\":\"session\"}\n{\"unfinished\"").unwrap();
    assert_eq!(classify_read_error(&read_lines(temp.path()).unwrap_err()), ReadFailure::InUse);
    std::fs::write(temp.path(), "{}\n").unwrap();
    temp.as_file().set_len(MAX_HISTORY_BYTES + 1).unwrap();
    assert_eq!(classify_read_error(&read_lines(temp.path()).unwrap_err()), ReadFailure::TooLarge);
    assert_eq!(classify_read_error(&DriverError::Protocol("Choose a valid OpenCode session.".into())), ReadFailure::Other);
}
