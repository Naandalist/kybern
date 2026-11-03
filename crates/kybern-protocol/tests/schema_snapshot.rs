//! The wire schema is a contract with the desktop and mobile clients. Any
//! change here must be deliberate: run `cargo insta review` (or
//! `INSTA_UPDATE=always cargo test -p kybern-protocol`) after changing types.

use kybern_protocol::methods::*;
use kybern_protocol::*;
use schemars::schema_for;

#[test]
fn method_registry_is_stable() {
    let names: Vec<&str> = METHODS.iter().map(|m| m.name).collect();
    insta::assert_yaml_snapshot!(names);
}

#[test]
fn method_scopes_are_stable() {
    let scopes: Vec<(String, Option<String>)> = METHODS.iter().map(|m| (m.name.to_string(), m.scope.map(|s| s.to_string()))).collect();
    insta::assert_yaml_snapshot!(scopes);
}

#[test]
fn event_schema_is_stable() {
    insta::assert_json_snapshot!(schema_for!(ThreadEvent));
}

#[test]
fn thread_schema_is_stable() {
    insta::assert_json_snapshot!(schema_for!(Thread));
}

#[test]
fn transcript_schema_is_stable() {
    insta::assert_json_snapshot!(schema_for!(TranscriptEntry));
}

#[test]
fn settings_schema_is_stable() {
    insta::assert_json_snapshot!(schema_for!(Settings));
}

#[test]
fn projects_list_can_return_the_index_without_git_probes() {
    insta::assert_json_snapshot!("projects_list_params", schema_for!(ProjectsListParams));
    let legacy: ProjectsListParams = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!(legacy.refresh_git, None);
    assert_eq!(serde_json::to_value(legacy).unwrap(), serde_json::json!({}));
    let cached: ProjectsListParams = serde_json::from_value(serde_json::json!({ "refresh_git": false })).unwrap();
    assert_eq!(cached.refresh_git, Some(false));
}

#[test]
fn wire_examples_roundtrip() {
    let ev = ThreadEvent {
        seq: 7,
        thread_id: uuid::Uuid::nil(),
        turn_id: Some(uuid::Uuid::nil()),
        at: chrono::DateTime::parse_from_rfc3339("2026-09-02T00:00:00Z").unwrap().into(),
        payload: EventPayload::ApprovalResolved {
            approval_id: uuid::Uuid::nil(),
            decision: ApprovalDecision::Deny { reason: Some("no".into()) },
            answered_by: None,
        },
    };
    let json = serde_json::to_value(&ev).unwrap();
    assert_eq!(json["kind"], "approval_resolved");
    assert_eq!(json["decision"]["decision"], "deny");
    let back: ThreadEvent = serde_json::from_value(json).unwrap();
    assert_eq!(back.seq, 7);

    let part: ContentPart = serde_json::from_str(r#"{"type":"file_mention","path":"src/main.rs"}"#).unwrap();
    assert_eq!(part, ContentPart::FileMention { path: "src/main.rs".into() });
    let skill: ContentPart = serde_json::from_str(r#"{"type":"skill","name":"better-ui","path":"/skills/better-ui/SKILL.md"}"#).unwrap();
    assert_eq!(skill, ContentPart::Skill { name: "better-ui".into(), path: "/skills/better-ui/SKILL.md".into() });
    assert_eq!(serde_json::to_value(ProviderKind::ClaudeCode).unwrap(), "claude-code");
    assert_eq!(serde_json::to_value(PermissionMode::AcceptEdits).unwrap(), "accept-edits");

    let legacy_diff_params: ThreadsDiffParams = serde_json::from_value(serde_json::json!({ "thread_id": uuid::Uuid::nil() })).unwrap();
    assert!(legacy_diff_params.include_patch);
}

#[test]
fn replay_ready_schema_is_stable() {
    insta::assert_json_snapshot!("events_subscribe_result", schema_for!(EventsSubscribeResult));
    insta::assert_json_snapshot!("events_ready_notification", schema_for!(EventsReadyNotification));
    let legacy: EventsSubscribeResult =
        serde_json::from_value(serde_json::json!({"subscription_id": uuid::Uuid::nil(), "head_seq": 7})).unwrap();
    assert!(!legacy.replay_ready);
}

#[test]
fn thread_file_read_schema_is_stable() {
    insta::assert_json_snapshot!(schema_for!(ThreadFileReadParams));
}

#[test]
fn thread_files_resolve_schema_is_stable() {
    insta::assert_json_snapshot!(schema_for!(ThreadFilesResolveParams));
    insta::assert_json_snapshot!(schema_for!(ThreadFilesResolveResult));
}

#[test]
fn notes_wire_shape_is_stable() {
    insta::assert_json_snapshot!("note_schema", schema_for!(Note));
    insta::assert_json_snapshot!("notes_changed_notification", schema_for!(NotesChangedNotification));
    let note = Note {
        summary: NoteSummary {
            id: uuid::Uuid::nil(),
            scope: NoteScope::Thread,
            project_id: Some(uuid::Uuid::nil()),
            thread_id: None,
            title: "Plan".into(),
            preview: "Ship it".into(),
            checklist: NoteChecklist { done: 1, total: 3 },
            pinned: false,
            revision: 2,
            created_at: chrono::DateTime::parse_from_rfc3339("2026-10-05T00:00:00Z").unwrap().into(),
            updated_at: chrono::DateTime::parse_from_rfc3339("2026-10-05T00:00:00Z").unwrap().into(),
            deleted_at: None,
            origin: None,
            created_by_thread: None,
            project_role: None,
            category: None,
            stale: None,
            undated: false,
        },
        body: "- [x] Ship it".into(),
    };
    let json = serde_json::to_value(&note).unwrap();
    assert_eq!(json["scope"], "thread");
    assert_eq!(json["body"], "- [x] Ship it", "body sits beside the summary fields");
    assert_eq!(json["checklist"], serde_json::json!({"done": 1, "total": 3}));
    assert!(
        json.get("thread_id").is_none()
            && json.get("deleted_at").is_none()
            && json.get("created_by_thread").is_none()
            && json.get("project_role").is_none(),
        "absent options are omitted"
    );
    assert_eq!(serde_json::from_value::<Note>(json).unwrap(), note);
    let mut brief = note.clone();
    brief.summary.scope = NoteScope::Project;
    brief.summary.project_role = Some(ProjectNoteRole::Brief);
    let json = serde_json::to_value(&brief).unwrap();
    assert_eq!(json["project_role"], "brief");
    assert_eq!(serde_json::from_value::<Note>(json).unwrap(), brief);
    let update: NotesUpdateParams =
        serde_json::from_value(serde_json::json!({"thread_id": uuid::Uuid::nil(), "expected_revision": 0})).unwrap();
    assert!(update.id.is_none() && update.body.is_none());
}

#[test]
fn task_items_wire_shape_is_stable() {
    insta::assert_json_snapshot!("task_item_schema", schema_for!(TaskItem));
    insta::assert_json_snapshot!("task_items_changed_notification", schema_for!(TaskItemsChangedNotification));
    insta::assert_json_snapshot!("task_items_followup_params", schema_for!(TaskItemsFollowupParams));
    let at: chrono::DateTime<chrono::Utc> = chrono::DateTime::parse_from_rfc3339("2026-10-05T00:00:00Z").unwrap().into();
    let task = TaskItem {
        id: uuid::Uuid::nil(),
        key: "ADE-14".into(),
        scope: TaskScope::Project,
        project_id: Some(uuid::Uuid::nil()),
        title: "Fix login".into(),
        body: "Redirect after sign in".into(),
        status: TaskStatus::NeedsReview,
        priority: 2,
        rank: 1024.0,
        note_ids: vec![uuid::Uuid::nil()],
        source_note_id: None,
        pending_followup: None,
        runs: vec![TaskRun {
            thread_id: uuid::Uuid::nil(),
            number: 1,
            provider: ProviderInstance::default_for(ProviderKind::ClaudeCode),
            model: None,
            started_at: at,
            ended_at: Some(at),
            state: TaskRunState::Completed,
            activity: None,
            diff: Some(TaskRunDiff { added: 12, removed: 3, files: 2 }),
            notes: vec![TaskRunNote { note_id: uuid::Uuid::nil(), revision: 4 }],
        }],
        revision: 1,
        created_at: at,
        updated_at: at,
        status_changed_at: at,
        created_by_thread: None,
    };
    let json = serde_json::to_value(&task).unwrap();
    assert_eq!(json["status"], "needs_review");
    assert_eq!(json["scope"], "project");
    assert_eq!(json["priority"], 2);
    assert_eq!(json["runs"][0]["state"], "completed");
    assert_eq!(json["runs"][0]["diff"], serde_json::json!({"added": 12, "removed": 3, "files": 2}));
    assert_eq!(json["runs"][0]["notes"][0]["revision"], 4);
    assert!(
        json.get("source_note_id").is_none() && json.get("pending_followup").is_none() && json.get("created_by_thread").is_none(),
        "absent options are omitted"
    );
    assert!(json["runs"][0].get("model").is_none() && json["runs"][0].get("activity").is_none());
    assert_eq!(serde_json::from_value::<TaskItem>(json).unwrap(), task);
    let update: TaskItemsUpdateParams = serde_json::from_value(serde_json::json!({"id": uuid::Uuid::nil()})).unwrap();
    assert!(update.status.is_none() && update.before_id.is_none());
    assert_eq!(serde_json::to_value(TaskStatus::Todo).unwrap(), "todo");
    assert_eq!(TaskItemsList::NAME, "tasks.items.list");
    assert_eq!(TASK_ITEMS_CHANGED_NOTIFICATION, "tasks.items.changed");
    let agent_task = TaskItem { created_by_thread: Some(uuid::Uuid::nil()), ..task };
    let json = serde_json::to_value(&agent_task).unwrap();
    assert_eq!(json["created_by_thread"], serde_json::json!(uuid::Uuid::nil()));
    assert_eq!(serde_json::from_value::<TaskItem>(json).unwrap(), agent_task);
    // The prompt-only send that mobile and the CLI use still parses; a full message is additive.
    let send: TaskItemsSendParams = serde_json::from_value(serde_json::json!({
        "id": uuid::Uuid::nil(), "provider": {"kind": "codex", "instance": "default"}, "prompt": "Fix it"
    }))
    .unwrap();
    assert_eq!((send.prompt.as_deref(), send.message.is_none()), (Some("Fix it"), true));
    let send: TaskItemsSendParams = serde_json::from_value(serde_json::json!({
        "id": uuid::Uuid::nil(), "provider": {"kind": "codex", "instance": "default"},
        "message": {"parts": [{"type": "text", "text": "Fix it"}]}
    }))
    .unwrap();
    assert!(send.prompt.is_none() && send.message.is_some_and(|message| message.parts.len() == 1));
    // The batch send defaults to separate runs; combined is named, and a result lists both lists.
    let batch: TaskItemsSendBatchParams = serde_json::from_value(serde_json::json!({
        "ids": [uuid::Uuid::nil()], "provider": {"kind": "codex", "instance": "default"}, "prompt": "Fix them"
    }))
    .unwrap();
    assert_eq!((batch.ids.len(), batch.mode), (1, TaskBatchMode::Separate));
    assert_eq!(serde_json::to_value(TaskBatchMode::Combined).unwrap(), "combined");
    assert_eq!(TaskItemsSendBatch::NAME, "tasks.items.send_batch");
    let result: TaskItemsSendBatchResult =
        serde_json::from_value(serde_json::json!({"started": [], "skipped": [{"id": uuid::Uuid::nil(), "reason": "Already running"}]}))
            .unwrap();
    assert_eq!(result.skipped[0].reason, "Already running");
}

#[test]
fn projects_changed_notification_is_stable() {
    insta::assert_json_snapshot!("projects_changed_notification", schema_for!(ProjectsChangedNotification));
}

#[test]
fn space_schema_is_stable() {
    insta::assert_json_snapshot!("space", schema_for!(Space));
}

#[test]
fn spaces_changed_notification_is_stable() {
    insta::assert_json_snapshot!("spaces_changed_notification", schema_for!(SpacesChangedNotification));
}

#[test]
fn sessions_scan_and_import_schemas_are_stable() {
    insta::assert_json_snapshot!("sessions_scan_changed_notification", schema_for!(SessionsScanChanged));
    insta::assert_json_snapshot!("sessions_import_progress_notification", schema_for!(SessionsImportProgress));
    insta::assert_json_snapshot!("sessions_import_finished_notification", schema_for!(SessionsImportFinished));
    insta::assert_json_snapshot!("sessions_import_all_params", schema_for!(SessionsImportAllParams));
}

#[test]
fn an_import_root_omitting_the_limit_means_fifty_and_null_means_all() {
    let omitted: SessionsImportRoot = serde_json::from_str(r#"{"root":"/r"}"#).unwrap();
    assert_eq!(omitted.limit, Some(50));
    let all: SessionsImportRoot = serde_json::from_str(r#"{"root":"/r","limit":null}"#).unwrap();
    assert_eq!(all.limit, None);
}

#[test]
fn group_project_wire_shape_is_stable() {
    insta::assert_json_snapshot!("projects_scan_repos_result", schema_for!(ProjectsScanReposResult));
    insta::assert_json_snapshot!("projects_members_status_result", schema_for!(ProjectsMembersStatusResult));
    insta::assert_json_snapshot!("threads_create_params", schema_for!(ThreadsCreateParams));
    insta::assert_json_snapshot!("git_status", schema_for!(GitStatus));
    insta::assert_json_snapshot!("pr_create_group_result", schema_for!(PrCreateGroupResult));
    insta::assert_json_snapshot!("threads_workspace_progress", schema_for!(ThreadsWorkspaceProgress));
    assert_eq!(THREADS_WORKSPACE_PROGRESS_NOTIFICATION, "threads.workspace_progress");

    // A single project from an older daemon has none of the group fields, and
    // a single project still serializes without them.
    let at = "2026-10-24T00:00:00Z";
    let single: Project = serde_json::from_value(serde_json::json!({
        "id": uuid::Uuid::nil(), "name": "kybern", "path": "/work/kybern", "is_git": true,
        "brief_in_all_threads": false, "created_at": at, "updated_at": at,
    }))
    .unwrap();
    assert_eq!(single.kind, ProjectKind::Single);
    let wire = serde_json::to_value(&single).unwrap();
    for key in ["kind", "members", "worktree_copy", "instructions_offer_dismissed"] {
        assert!(wire.get(key).is_none(), "{key} leaked into a single project");
    }

    let group: Project = serde_json::from_value(serde_json::json!({
        "id": uuid::Uuid::nil(), "name": "acme", "path": "/work/acme", "is_git": false,
        "brief_in_all_threads": false, "created_at": at, "updated_at": at,
        "kind": "group",
        "members": [
            { "path": "api", "name": "api", "status": "ok" },
            { "path": "web", "name": "web", "status": "not_git" },
        ],
        "worktree_copy": [".env", "docker-compose.yml"],
    }))
    .unwrap();
    assert_eq!(group.kind, ProjectKind::Group);
    assert_eq!(group.members[1].status, MemberStatus::NotGit);
    assert_eq!(serde_json::to_value(&group).unwrap()["kind"], "group");

    let event = EventPayload::OutsideRepoChanged {
        repos: vec![OutsideRepoChange {
            repo: "infra".into(),
            name: "infra".into(),
            source_path: "/work/acme/infra".into(),
            dirty_files: 2,
            head_moved: false,
        }],
    };
    let wire = serde_json::to_value(&event).unwrap();
    assert_eq!(wire["kind"], "outside_repo_changed");
    assert!(wire["repos"][0].get("head_moved").is_none());

    let progress: ThreadsWorkspaceProgress =
        serde_json::from_value(serde_json::json!({ "thread_id": uuid::Uuid::nil(), "done": 1, "total": 3, "current": "web" })).unwrap();
    assert_eq!(progress.current.as_deref(), Some("web"));
    let pr: PrCreateGroupParams = serde_json::from_value(serde_json::json!({ "thread_id": uuid::Uuid::nil() })).unwrap();
    assert!(pr.commit_first && !pr.draft && pr.repos.is_none());
    assert_eq!(PrCreateGroup::NAME, "github.pr.create_group");
}

#[test]
fn network_schemas_are_stable() {
    insta::assert_json_snapshot!("network_status", schema_for!(NetworkStatus));
    insta::assert_json_snapshot!("network_op_body", schema_for!(OpBody));
    insta::assert_json_snapshot!("network_op_envelope", schema_for!(OpEnvelope));
    insta::assert_json_snapshot!("network_machine_card", schema_for!(MachineCard));
    insta::assert_json_snapshot!("network_found_machine", schema_for!(FoundMachine));
    insta::assert_json_snapshot!("network_route", schema_for!(Route));
    insta::assert_json_snapshot!("network_peer_presence", schema_for!(PeerPresence));
    insta::assert_json_snapshot!("network_origin", schema_for!(Origin));
    insta::assert_json_snapshot!("network_move_info", schema_for!(MoveInfo));
    insta::assert_json_snapshot!("network_repo_identity", schema_for!(ProjectRepoIdentity));
    insta::assert_json_snapshot!("network_machine_info", schema_for!(MachineInfo));
    insta::assert_json_snapshot!("network_doctor_report", schema_for!(DoctorReport));
}

#[test]
fn machine_utility_schemas_are_stable() {
    insta::assert_json_snapshot!("utilities_files_send_params", schema_for!(FilesSendParams));
    insta::assert_json_snapshot!("utilities_files_push_payload", schema_for!(FilesPushPayload));
    insta::assert_json_snapshot!("utilities_peer_files_begin_params", schema_for!(PeerFilesBeginParams));
    insta::assert_json_snapshot!("utilities_peer_files_begin_result", schema_for!(PeerFilesBeginResult));
    insta::assert_json_snapshot!("utilities_peer_files_chunk_params", schema_for!(PeerFilesChunkParams));
    insta::assert_json_snapshot!("utilities_files_progress", schema_for!(FilesProgress));
    insta::assert_json_snapshot!("utilities_files_received", schema_for!(FilesReceived));
    insta::assert_json_snapshot!("utilities_port_forward", schema_for!(PortForward));
    insta::assert_json_snapshot!("utilities_peer_ports_open_params", schema_for!(PeerPortsOpenParams));
    insta::assert_json_snapshot!("utilities_peer_ports_prepare_params", schema_for!(PeerPortsPrepareParams));
    insta::assert_json_snapshot!("utilities_peer_ports_prepare_result", schema_for!(PeerPortsPrepareResult));
    insta::assert_json_snapshot!("utilities_peer_ports_release_params", schema_for!(PeerPortsReleaseParams));
}

#[test]
fn skill_sync_schemas_are_stable() {
    insta::assert_json_snapshot!("skills_inventory", schema_for!(SkillInventory));
    insta::assert_json_snapshot!("skills_network_view", schema_for!(SkillsNetworkView));
    insta::assert_json_snapshot!("skills_copy_params", schema_for!(SkillsCopyParams));
    insta::assert_json_snapshot!("skills_copy_result", schema_for!(SkillsCopyResult));
    insta::assert_json_snapshot!("skills_copy_payload", schema_for!(SkillCopyPayload));
    insta::assert_json_snapshot!("skills_landed", schema_for!(SkillLanded));
    insta::assert_json_snapshot!("skills_conflict", schema_for!(SkillConflict));
}

#[test]
fn skill_homes_name_their_folders_and_readers() {
    for home in SkillHome::ALL {
        assert_eq!(serde_json::to_value(home).unwrap(), home.as_str());
        assert_eq!(SkillHome::parse(home.as_str()), Some(home));
        assert!(home.usual_path().starts_with("~/.") && home.usual_path().ends_with("skills"));
        assert!(!home.readers().is_empty());
    }
    // Every harness reads at least one home, and the cross-agent folder reaches more than one.
    for provider in ProviderKind::ALL {
        assert!(SkillHome::ALL.iter().any(|home| home.readers().contains(&provider)), "{provider} reads no home");
    }
    assert!(SkillHome::Agents.readers().len() > 1);
    assert_eq!(
        serde_json::to_value(PushDest::Skill { name: "x".into(), homes: vec![SkillHome::Agents], hash: "h".into(), replaces: vec![] })
            .unwrap()["kind"],
        "skill"
    );
}

fn machine(id: &str) -> MachineDescriptor {
    MachineDescriptor {
        machine_id: id.into(),
        name: "satyamas".into(),
        os: "windows".into(),
        signing_key: "c2ln".into(),
        sealing_key: "c2VhbA==".into(),
        ts_node_id: Some("nABC".into()),
        ts_name: None,
        port: None,
    }
}

#[test]
fn an_operation_body_flattens_its_kind_and_keeps_unknown_kinds() {
    let body = OpBody {
        v: OP_VERSION,
        network_id: "net".into(),
        at: "2026-10-28T10:00:00Z".parse().unwrap(),
        lamport: 3,
        op: OpKind::Add { machine: machine("m2"), via: JoinVia::Found },
    };
    let json = serde_json::to_value(&body).unwrap();
    assert_eq!((json["kind"].as_str(), json["via"].as_str(), json["lamport"].as_u64()), (Some("add"), Some("found"), Some(3)));
    assert_eq!(json["machine"]["machine_id"], "m2");
    assert!(json["machine"].get("ts_name").is_none(), "absent options are omitted");
    assert_eq!(serde_json::from_value::<OpBody>(json).unwrap(), body);

    let set = OpBody { op: OpKind::Set { key: "machine.m2.paused".into(), value: serde_json::json!(true) }, ..body.clone() };
    assert_eq!(serde_json::from_str::<OpBody>(&serde_json::to_string(&set).unwrap()).unwrap(), set);
    let remove = OpBody { op: OpKind::Remove { machine_id: "m2".into(), ts_node_id: None, reason: RemoveReason::Stolen }, ..body };
    assert_eq!(serde_json::to_value(&remove).unwrap()["reason"], "stolen");

    // A kind from a newer build parses as Unknown instead of failing, so it can be kept and gossiped.
    let future =
        serde_json::json!({"v": 2, "network_id": "net", "at": "2026-10-28T10:00:00Z", "lamport": 9, "kind": "rotate_key", "extra": 1});
    let parsed: OpBody = serde_json::from_value(future).unwrap();
    assert_eq!((parsed.op, parsed.lamport), (OpKind::Unknown, 9));
}

#[test]
fn machine_scopes_are_kept_out_of_client_tokens() {
    assert!(Scope::ALL.contains(&Scope::NetworkAdmin) && Scope::ALL.contains(&Scope::NetworkRead));
    assert!(Scope::CLIENT.contains(&Scope::NetworkRead) && !Scope::CLIENT.contains(&Scope::NetworkAdmin));
    for scope in Scope::MACHINE {
        assert!(scope.is_machine() && !Scope::ALL.contains(&scope) && !Scope::CLIENT.contains(&scope), "{scope}");
        assert!(scope.as_str().starts_with("machine:"));
    }
    assert!(!Scope::MACHINE_DEFAULT.contains(&Scope::MachineClipboardAuto));
    assert_eq!(Scope::MACHINE_DEFAULT.len() + 1, Scope::MACHINE.len());
    assert_eq!(Scope::from_capability("delegate"), Some(Scope::MachineDelegate));
    assert_eq!(Scope::from_capability("clipboard_auto"), Some(Scope::MachineClipboardAuto));
    assert_eq!((Scope::from_capability("peer"), Scope::from_capability("nope")), (None, None));
    assert_eq!(serde_json::to_value(Scope::MachineClipboardAuto).unwrap(), "machine_clipboard_auto");
    assert_eq!(serde_json::to_value(Scope::NetworkRead).unwrap(), "network_read");
}

#[test]
fn every_network_method_has_a_scope_and_peer_methods_need_a_machine_scope() {
    for method in METHODS.iter().filter(|m| m.name.starts_with("network.") || m.name.starts_with("machines.")) {
        // The relay page tickets use the relay's own caller scope: a web view with a device token opens them (ADE-99).
        if method.name.starts_with("network.relay_page.") {
            assert_eq!(method.scope, Some(Scope::OrchestrationRead), "{}", method.name);
            continue;
        }
        assert!(matches!(method.scope, Some(Scope::NetworkRead | Scope::NetworkAdmin)), "{}", method.name);
    }
    let peers: Vec<_> = METHODS.iter().filter(|m| m.name.starts_with("peer.")).collect();
    // Eight from N1; later packages add theirs (R1: delegate, delegation.get/cancel, git.bundle/import).
    assert!(peers.len() >= 8);
    assert!(peers.iter().all(|m| m.scope.is_some_and(Scope::is_machine)));
    assert_eq!(NetworkStatusMethod::NAME, "network.status");
    assert_eq!(NETWORK_CHANGED_NOTIFICATION, "network.changed");
    assert_eq!(NETWORK_MACHINE_CHANGED_NOTIFICATION, "network.machine.changed");
}

#[test]
fn an_older_daemon_info_and_project_still_parse_and_the_network_defaults_to_off() {
    let old: DaemonInfo = serde_json::from_value(serde_json::json!({
        "version": "0.18.0", "protocol_version": 1, "environment_id": "e", "hostname": "h", "os": "macos", "arch": "aarch64",
        "data_dir": "/d", "scopes": ["orchestration_read"], "started_at": "2026-10-28T10:00:00Z"
    }))
    .unwrap();
    assert_eq!((old.machine_id.as_str(), old.network_protocol, old.features.len()), ("", 0, 0));
    let settings: Settings = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!(settings.network, NetworkSettings { configured: false, enabled: false, port: DEFAULT_PORT, keep_reachable: true });
    // A settings file from before the default changed, saved with the network on, keeps it on.
    let saved: Settings = serde_json::from_value(serde_json::json!({ "network": { "enabled": true } })).unwrap();
    assert_eq!(saved.network, NetworkSettings { configured: true, enabled: true, port: DEFAULT_PORT, keep_reachable: true });
    assert_eq!(NETWORK_PROTOCOL, 1);
    assert_eq!(features::ALL.len(), 27);
}

#[test]
fn outbox_and_remote_delegation_schemas_are_stable() {
    insta::assert_json_snapshot!("outbox_item", schema_for!(OutboxItem));
    insta::assert_json_snapshot!("outbox_op", schema_for!(OutboxOp));
    insta::assert_json_snapshot!("outbox_remote_child", schema_for!(RemoteChild));
    insta::assert_json_snapshot!("peer_delegate_params", schema_for!(PeerDelegateParams));
    insta::assert_json_snapshot!("peer_delegate_result", schema_for!(PeerDelegateResult));
    insta::assert_json_snapshot!("peer_delegation_view", schema_for!(PeerDelegationView));
    insta::assert_json_snapshot!("peer_git_bundle_params", schema_for!(PeerGitBundleParams));
    insta::assert_json_snapshot!("peer_git_import_params", schema_for!(PeerGitImportParams));
}

#[test]
fn outbox_states_and_kinds_use_the_names_the_tables_check() {
    for (state, name) in [
        (OutboxState::Queued, "queued"),
        (OutboxState::Sending, "sending"),
        (OutboxState::Delivered, "delivered"),
        (OutboxState::Failed, "failed"),
        (OutboxState::Expired, "expired"),
        (OutboxState::Cancelled, "cancelled"),
    ] {
        assert_eq!(serde_json::to_value(state).unwrap(), name);
        assert_eq!(state.as_str(), name);
    }
    for kind in [
        OutboxKind::ThreadCreate,
        OutboxKind::ThreadSend,
        OutboxKind::Delegate,
        OutboxKind::FilesPush,
        OutboxKind::SettingsPatch,
        OutboxKind::SecretsPut,
        OutboxKind::Fork,
        OutboxKind::Move,
        OutboxKind::Provision,
        OutboxKind::SkillCopy,
        OutboxKind::PluginInstall,
    ] {
        assert_eq!(serde_json::to_value(kind).unwrap(), kind.as_str());
    }
    assert!(OutboxState::Expired.is_settled() && !OutboxState::Sending.is_settled());
    assert_eq!(OUTBOX_EXPIRY_DAYS, 7);
}

#[test]
fn remote_child_states_map_to_the_status_the_tools_show() {
    assert_eq!(RemoteChildState::Pending.delegation_status(), DelegationStatus::Running);
    assert_eq!(RemoteChildState::Unreachable.delegation_status(), DelegationStatus::Failed);
    assert_eq!(RemoteChildState::Cancelled.delegation_status(), DelegationStatus::Cancelled);
    assert!(!RemoteChildState::Running.is_terminal() && RemoteChildState::Completed.is_terminal());
    assert_eq!(serde_json::to_value(RemoteChildState::Unreachable).unwrap(), "unreachable");
}

#[test]
fn plugin_declarations_and_confirmations_are_stable() {
    insta::assert_json_snapshot!("plugin_inventory", schema_for!(PluginInventory));
    insta::assert_json_snapshot!("plugins_copy_params", schema_for!(PluginsCopyParams));
    insta::assert_json_snapshot!("plugin_install_request", schema_for!(PluginInstallRequest));
}

#[test]
fn provision_member_plans_are_stable() {
    insta::assert_json_snapshot!("provision_plan", schema_for!(ProvisionPlan));
    insta::assert_json_snapshot!("provision_probe", schema_for!(PeerProvisionProbe));
    let older: PeerProvisionProbe = serde_json::from_value(serde_json::json!({
        "os": "linux", "suggested_path": "/home/me/group",
        "path_check": { "state": "free", "path": "/home/me/group", "message": "New folder" }
    }))
    .unwrap();
    assert!(older.members.is_empty(), "older peers do not report member mappings");
}

#[test]
fn network_setup_preserves_legacy_choices_and_roundtrips_new_defaults() {
    let fresh = NetworkSettings::default();
    assert!(!fresh.configured);
    assert_eq!(serde_json::from_value::<NetworkSettings>(serde_json::to_value(&fresh).unwrap()).unwrap(), fresh);
    for enabled in [false, true] {
        let old: NetworkSettings = serde_json::from_value(serde_json::json!({"enabled": enabled})).unwrap();
        assert!(old.configured, "legacy choices are never overridden automatically");
        assert_eq!(old.enabled, enabled);
    }
    let absent: Settings = serde_json::from_value(serde_json::json!({})).unwrap();
    assert!(!absent.network.configured);
    let params: NetworkSetEnabledParams = serde_json::from_value(serde_json::json!({"enabled": true})).unwrap();
    assert!(!params.only_if_unconfigured);
}

#[test]
fn spaces_sync_contract_is_stable() {
    insta::assert_json_snapshot!("spaces_place_params", schema_for!(SpacesPlaceParams));
    insta::assert_json_snapshot!("spaces_list_result", schema_for!(SpacesListResult));
    insta::assert_json_snapshot!("peer_spaces_sync_params", schema_for!(PeerSpacesSyncParams));
    insta::assert_json_snapshot!("peer_spaces_sync_result", schema_for!(PeerSpacesSyncResult));
    insta::assert_json_snapshot!("peer_space_logo_params", schema_for!(PeerSpaceLogoParams));
    insta::assert_json_snapshot!("peer_space_logo_result", schema_for!(PeerSpaceLogoResult));
}
