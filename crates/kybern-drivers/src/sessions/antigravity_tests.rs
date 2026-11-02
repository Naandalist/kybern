use super::antigravity::{by_step, load_steps, parse, uri_path};
use super::tests::{answers, users};
use super::*;
use rusqlite::Connection;
use std::collections::BTreeMap;

/// A scratch HOME holding `~/.gemini/antigravity-cli` the way agy lays it out.
struct Agy {
    home: PathBuf,
}

impl Drop for Agy {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(self.root(), std::fs::Permissions::from_mode(0o755));
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

const SCHEMA: &str = "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
    CREATE TABLE conversation_summaries (conversation_id TEXT PRIMARY KEY, title TEXT, preview TEXT, step_count INTEGER,
        last_modified_time INTEGER, workspace_uris TEXT, status TEXT, source TEXT, project_id TEXT, agent_name TEXT,
        parent_conversation_id TEXT, nesting_depth INTEGER, battle_id TEXT, winning_conversation_id TEXT, not_fully_idle INTEGER,
        killed INTEGER, last_user_input_time INTEGER, last_user_input_step_index INTEGER, app_data_dir TEXT, raw_summary BLOB,
        group_id TEXT);";

impl Agy {
    fn new() -> Self {
        let home = std::env::temp_dir().join(format!("kybern-agy-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(home.join(".gemini/antigravity-cli")).unwrap();
        Self { home }
    }

    fn root(&self) -> PathBuf {
        self.home.join(".gemini/antigravity-cli")
    }

    fn context(&self) -> ProbeContext {
        ProbeContext { binary: None, cwd: None, env: [("HOME".to_string(), self.home.display().to_string())].into() }
    }

    /// The summaries database in WAL mode with checkpoints off, so rows live only in the `-wal` file
    /// for as long as the returned connection is open, as with a running agy.
    fn database(&self) -> Connection {
        let db = Connection::open(self.root().join("conversation_summaries.db")).unwrap();
        db.execute_batch(SCHEMA).unwrap();
        db
    }

    #[allow(clippy::too_many_arguments)]
    fn summary(
        &self,
        db: &Connection,
        id: &str,
        title: Option<&str>,
        preview: &str,
        workspace: Option<&str>,
        parent: Option<&str>,
        at: i64,
    ) {
        db.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, preview, last_modified_time, workspace_uris, parent_conversation_id, step_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 3)",
            rusqlite::params![id, title, preview, at, workspace.map(|uri| serde_json::json!([uri]).to_string()), parent],
        )
        .unwrap();
    }

    fn logs(&self, id: &str) -> PathBuf {
        let logs = self.root().join("brain").join(id).join(".system_generated/logs");
        std::fs::create_dir_all(&logs).unwrap();
        logs
    }

    fn transcript(&self, id: &str, rows: &[Value]) {
        std::fs::write(self.logs(id).join("transcript.jsonl"), rows.iter().map(|row| format!("{row}\n")).collect::<String>()).unwrap();
    }
}

/// Every file under `dir` with its size, modification time and bytes.
type Tree = BTreeMap<PathBuf, (u64, Option<std::time::SystemTime>, Vec<u8>)>;

fn tree(dir: &Path) -> Tree {
    let mut out = BTreeMap::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                dirs.push(entry.path());
            }
            let bytes = if meta.is_file() { std::fs::read(entry.path()).unwrap() } else { Vec::new() };
            // A directory's own timestamp is updated lazily by NTFS after the writes inside it, so only files carry one.
            let modified = if meta.is_file() { meta.modified().ok() } else { None };
            out.insert(entry.path(), (meta.len(), modified, bytes));
        }
    }
    out
}

/// The paths whose presence, size, bytes or modification time differ between two listings.
fn changed(before: &Tree, after: &Tree) -> Vec<String> {
    let paths: std::collections::BTreeSet<_> = before.keys().chain(after.keys()).collect();
    paths
        .into_iter()
        .filter(|path| before.get(*path) != after.get(*path))
        .map(|path| format!("{}: {:?} -> {:?}", path.display(), before.get(path).map(|e| (e.0, e.1)), after.get(path).map(|e| (e.0, e.1))))
        .collect()
}

fn step(index: u64, source: &str, kind: &str, content: &str) -> Value {
    json!({"step_index": index, "source": source, "type": kind, "status": "DONE", "created_at": format!("2026-10-30T10:00:{index:02}Z"),
        "content": content})
}

fn planner(index: u64, content: &str, calls: Value) -> Value {
    let mut row = step(index, "MODEL", "PLANNER_RESPONSE", content);
    row["tool_calls"] = calls;
    row
}

/// The `file://` URI an editor sends for `path`: `file:///C:/x` on Windows, `file:///x` elsewhere.
fn file_uri(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.starts_with('/') { format!("file://{text}") } else { format!("file:///{text}") }
}

/// An argument value as agy writes it: the JSON encoding of the value, as a string.
fn encoded(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy()).unwrap()
}

fn conversation_session(id: &str) -> SavedSession {
    SavedSession {
        provider: ProviderKind::Antigravity,
        id: id.into(),
        title: "Fix the bug".into(),
        cwd: "/tmp/project".into(),
        updated_at: "2026-10-30T10:00:00Z".parse().unwrap(),
        model: None,
        thread_id: None,
    }
}

fn tool_calls(history: &SessionHistory) -> Vec<(&str, &str, &Value)> {
    history
        .events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolCallStarted { call, .. } => Some((call.id.as_str(), call.name.as_str(), &call.input)),
            _ => None,
        })
        .collect()
}

fn results(history: &SessionHistory) -> Vec<(&str, &Value, bool)> {
    history
        .events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolCallCompleted { tool_call_id, output, is_error, .. } => Some((tool_call_id.as_str(), output, *is_error)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_transcript_becomes_turns_tool_cards_and_notices() {
    let truncated = step(7, "USER_EXPLICIT", "USER_INPUT", "A long question A long");
    let mut thinking = planner(
        1,
        "Looking at it.",
        json!([
            {"name": "view_file", "args": {"AbsolutePath": "\"/tmp/project/a.rs\""}},
            {"name": "run_command", "args": {"CommandLine": "\"cargo test\"", "Cwd": "\"/tmp/project\""}},
            {"name": "manage_task", "args": {"TaskName": "\"Fix\""}},
            {"name": "view_file", "args": {"AbsolutePath": "\"/home/u/.gemini/antigravity-cli/mcp/srv/tool.json\""}},
        ]),
    );
    thinking["thinking"] = json!("Plan the fix.");
    let mut failed = step(3, "MODEL", "RUN_COMMAND", "Command exited with code 2.\nerror[E0425]");
    failed["status"] = json!("DONE");
    let mut system_error = step(6, "SYSTEM", "ERROR_MESSAGE", "Model overloaded. Try again.");
    system_error["status"] = json!("ERROR");
    // Row order in the file is time order: step 5 is written twice, the rewrite last.
    let rows = [
        step(0, "USER_EXPLICIT", "USER_INPUT", "Fix the bug"),
        thinking,
        step(2, "MODEL", "VIEW_FILE", "fn main() {}"),
        failed,
        step(4, "MODEL", "GENERIC", "Task saved"),
        step(8, "MODEL", "VIEW_FILE", "schema"),
        planner(5, "A draft", json!([])),
        system_error,
        truncated,
        step(9, "SYSTEM", "SYSTEM_MESSAGE", "Ephemeral reminder"),
        step(10, "MODEL", "CONVERSATION_HISTORY", "old"),
        planner(5, "Tests fail on line 3.", json!([])),
        planner(11, "All fixed.", json!([])),
        json!({"step_index": 12, "type": "SOMETHING_NEW", "extra": {"x": 1}}),
    ];
    let steps = by_step(rows.to_vec());
    let history = parse(conversation_session("conv"), &steps);

    let tools = tool_calls(&history);
    assert_eq!(
        tools.iter().map(|(id, name, _)| (*id, *name)).collect::<Vec<_>>(),
        [("agy:conv:2", "Read"), ("agy:conv:3", "Bash"), ("agy:conv:4", "manage_task"),],
        "the MCP schema read is hidden, ids carry the result step, unmapped tools keep their name"
    );
    assert_eq!(tools[0].2["file_path"], "/tmp/project/a.rs", "JSON-encoded arguments are unquoted");
    assert_eq!(tools[1].2["command"], "cargo test");
    assert_eq!(tools[1].2["cwd"], "/tmp/project");
    assert_eq!(tools[2].2["TaskName"], "Fix");
    let done = results(&history);
    assert_eq!(done.len(), 3);
    assert_eq!(done[0], ("agy:conv:2", &json!("fn main() {}"), false));
    assert!(done[1].2, "a non-zero exit is an error");
    assert!(!done[2].2);

    let answers = answers(&history);
    assert_eq!(answers, ["Looking at it.", "Tests fail on line 3.", "All fixed."], "the last row of a rewritten step wins");
    assert!(
        history
            .events
            .iter()
            .any(|e| matches!(&e.payload, EventPayload::AssistantMessageCompleted { thinking: Some(t), .. } if t == "Plan the fix."))
    );
    assert!(history.events.iter().any(|e| matches!(&e.payload,
        EventPayload::ProviderNotice { level: NoticeLevel::Error, text, .. } if text == "Model overloaded. Try again.")));
    assert_eq!(users(&history), ["Fix the bug", "A long question A long"]);
    assert_eq!(history.events.iter().filter(|e| matches!(e.payload, EventPayload::TurnCompleted { .. })).count(), 2);
}

#[test]
fn truncated_content_is_read_from_the_full_transcript() {
    let agy = Agy::new();
    let db = agy.database();
    agy.summary(&db, "conv", Some("Long"), "", Some("file:///tmp"), None, 1_790_000_000_000);
    let mut cut = step(0, "USER_EXPLICIT", "USER_INPUT", "A long qu");
    cut["truncated_fields"] = json!(["content"]);
    agy.transcript("conv", &[cut, step(1, "MODEL", "PLANNER_RESPONSE", "ok")]);
    std::fs::write(
        agy.logs("conv").join("transcript_full.jsonl"),
        format!("{}\n", step(0, "USER_EXPLICIT", "USER_INPUT", "A long question in full")),
    )
    .unwrap();
    let history = parse(conversation_session("conv"), &load_steps(&agy.logs("conv")).unwrap());
    assert_eq!(users(&history), ["A long question in full"]);

    // Without the full text, the cut is said out loud instead of passing for the whole message.
    std::fs::remove_file(agy.logs("conv").join("transcript_full.jsonl")).unwrap();
    let history = parse(conversation_session("conv"), &load_steps(&agy.logs("conv")).unwrap());
    assert_eq!(users(&history), ["A long qu\n\n[Cut short in the original transcript.]"]);
}

#[tokio::test]
async fn the_list_skips_subagents_and_finds_each_conversations_folder() {
    let agy = Agy::new();
    let work = agy.home.join("work");
    std::fs::create_dir_all(work.join("Proj One")).unwrap();
    std::fs::create_dir_all(work.join("other")).unwrap();
    let db = agy.database();
    // The editor spelled the folder in another case and with an encoded space.
    let wrong_case = format!("{}/WORK/proj%20one", file_uri(&agy.home));
    agy.summary(&db, "has-workspace", Some("First"), "ignored preview", Some(&wrong_case), None, 1_790_000_300_000);
    agy.summary(&db, "preview-title", None, "A preview used as title", Some(&file_uri(&work.join("other"))), None, 1_790_000_200);
    agy.summary(&db, "no-workspace", Some("From a command"), "", None, None, 1_790_000_100_000);
    agy.summary(&db, "nowhere", Some("No folder at all"), "", None, None, 1_790_000_050_000);
    agy.summary(&db, "child", Some("Sub-agent"), "", Some("file:///tmp"), Some("has-workspace"), 1_790_000_400_000);
    agy.summary(&db, "no-transcript", Some("Deleted"), "", Some("file:///tmp"), None, 1_790_000_010_000);
    let hello = |index| step(index, "USER_EXPLICIT", "USER_INPUT", "Hello");
    agy.transcript("has-workspace", &[hello(0)]);
    agy.transcript("preview-title", &[hello(0)]);
    agy.transcript("child", &[hello(0)]);
    agy.transcript("nowhere", &[hello(0)]);
    agy.transcript(
        "no-workspace",
        &[
            hello(0),
            planner(1, "", json!([{"name": "run_command", "args": {"CommandLine": "\"ls\"", "Cwd": encoded(&work.join("other"))}}])),
        ],
    );

    let mut reader = reader(ProviderKind::Antigravity, &agy.context()).await.unwrap();
    let page = reader.list_page(None).await.unwrap();
    assert!(page.next_cursor.is_none());
    let listed: Vec<_> = page.sessions.iter().map(|s| (s.id.as_str(), s.title.as_str(), s.cwd.as_str())).collect();
    let proj = work.join("Proj One").display().to_string();
    let other = work.join("other").display().to_string();
    assert_eq!(
        listed,
        [
            ("has-workspace", "First", proj.as_str()),
            ("preview-title", "A preview used as title", other.as_str()),
            ("no-workspace", "From a command", other.as_str()),
            ("nowhere", "No folder at all", ""),
        ],
        "newest first, sub-agents and missing transcripts left out, seconds and milliseconds both read"
    );
    assert!(page.sessions.iter().all(|s| s.provider == ProviderKind::Antigravity && s.model.is_none()));

    let history = reader.read("has-workspace").await.unwrap();
    assert_eq!(history.session.id, "has-workspace");
    assert_eq!(users(&history), ["Hello"]);
    assert!(reader.read("child").await.is_err(), "sub-agent conversations are not imported");
    assert!(reader.read("../brain").await.is_err());
    reader.close().await;

    let filtered =
        list(ProviderKind::Antigravity, &ProbeContext { cwd: Some(work.join("other")), ..agy.context() }, None, "").await.unwrap();
    assert_eq!(filtered.sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["preview-title", "no-workspace"]);
    let queried = list(ProviderKind::Antigravity, &agy.context(), None, "command").await.unwrap();
    assert_eq!(queried.sessions.len(), 1);
}

#[tokio::test]
async fn reading_never_touches_the_antigravity_folder() {
    let agy = Agy::new();
    let db = agy.database();
    agy.summary(&db, "conv", Some("Only in the log"), "", Some("file:///tmp"), None, 1_790_000_000_000);
    agy.transcript("conv", &[step(0, "USER_EXPLICIT", "USER_INPUT", "Hello")]);
    // The row is uncheckpointed: it exists only in the -wal file while this connection is open.
    let wal = agy.root().join("conversation_summaries.db-wal");
    assert!(wal.metadata().unwrap().len() > 0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(agy.root(), std::fs::Permissions::from_mode(0o555)).unwrap();
        std::fs::set_permissions(agy.root().join("conversation_summaries.db"), std::fs::Permissions::from_mode(0o444)).unwrap();
    }
    let before = tree(&agy.root());

    let mut reader = reader(ProviderKind::Antigravity, &agy.context()).await.unwrap();
    let page = reader.list_page(None).await.unwrap();
    assert_eq!(page.sessions.len(), 1, "a row that only the WAL holds is listed");
    assert_eq!(users(&reader.read("conv").await.unwrap()), ["Hello"]);
    reader.close().await;
    list(ProviderKind::Antigravity, &agy.context(), None, "").await.unwrap();
    read(ProviderKind::Antigravity, &agy.context(), "conv").await.unwrap();

    assert_eq!(changed(&before, &tree(&agy.root())), Vec::<String>::new(), "no file was added, changed or touched");

    // Without the writer, SQLite leaves no -wal or -shm; reading must not bring them back.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(agy.root(), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    drop(db);
    let names = |dir: &Path| std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name()).collect::<HashSet<_>>();
    let before = names(&agy.root());
    assert!(!before.contains(std::ffi::OsStr::new("conversation_summaries.db-shm")));
    list(ProviderKind::Antigravity, &agy.context(), None, "").await.unwrap();
    assert_eq!(names(&agy.root()), before);
}

#[tokio::test]
async fn no_store_is_an_empty_list_and_an_unfinished_last_line_means_in_use() {
    let agy = Agy::new();
    assert!(list(ProviderKind::Antigravity, &agy.context(), None, "").await.unwrap().sessions.is_empty());
    let db = agy.database();
    agy.summary(&db, "live", Some("Live"), "", Some("file:///tmp"), None, 1_790_000_000_000);
    std::fs::write(
        agy.logs("live").join("transcript.jsonl"),
        format!("{}\n{{\"step_index\":1,\"type\":\"PLANNER_RESP", step(0, "USER_EXPLICIT", "USER_INPUT", "Hello")),
    )
    .unwrap();
    let error = read(ProviderKind::Antigravity, &agy.context(), "live").await.err().expect("an unfinished line is refused");
    assert_eq!(classify_read_error(&error), ReadFailure::InUse);
    // A damaged line in the middle is skipped, not fatal.
    std::fs::write(agy.logs("live").join("transcript.jsonl"), format!("not json\n{}\n", step(0, "USER_EXPLICIT", "USER_INPUT", "Hello")))
        .unwrap();
    assert_eq!(users(&read(ProviderKind::Antigravity, &agy.context(), "live").await.unwrap()), ["Hello"]);
}

#[test]
fn file_uris_become_paths() {
    assert_eq!(uri_path("file:///Users/me/My%20Proj").as_deref(), Some("/Users/me/My Proj"));
    assert_eq!(uri_path("file:///Users/me/proj/").as_deref(), Some("/Users/me/proj"));
    assert_eq!(uri_path("file:///Users/me/caf%C3%A9").as_deref(), Some("/Users/me/café"));
    assert_eq!(uri_path("file://localhost/tmp/x").as_deref(), Some("/tmp/x"));
    assert_eq!(uri_path("file:///C:/Users/me/x").as_deref(), Some("C:/Users/me/x"));
    assert_eq!(uri_path("file:///c%3A/Users/me").as_deref(), Some("c:/Users/me"));
    assert_eq!(uri_path("file://server/share/x").as_deref(), Some("//server/share/x"));
    assert_eq!(uri_path("file:///").as_deref(), Some("/"));
    assert_eq!(uri_path("/already/a/path").as_deref(), Some("/already/a/path"));
    assert_eq!(uri_path("C:\\Users\\me").as_deref(), Some("C:\\Users\\me"));
    assert_eq!(uri_path("vscode-remote://ssh/x"), None);
}
