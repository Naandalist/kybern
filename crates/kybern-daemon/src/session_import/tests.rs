use super::*;
use kybern_protocol::{EventPayload, PermissionMode, ThreadId, ThreadStatus, methods::SessionsResumeParams};

/// A scratch HOME with Claude and pi history, a few repositories, and a daemon state
/// whose harness settings point at them. No real harness is started: scans name
/// the file-based harnesses only.
struct Fixture {
    state: AppState,
    root: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn fixture() -> Fixture {
    let root = kybern_git::simplify_path(&std::fs::canonicalize(std::env::temp_dir()).unwrap())
        .join(format!("kybern-session-import-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(root.join("home")).unwrap();
    let paths = crate::config::Paths::resolve(Some(root.join("data"))).unwrap();
    let state = AppState::initialize(&paths).unwrap();
    let mut settings = state.settings.get();
    let home = root.join("home").to_string_lossy().into_owned();
    let claude = settings.providers.entry(ProviderKind::ClaudeCode).or_default();
    claude.env.insert("HOME".into(), home.clone());
    claude.env.insert("CLAUDE_CONFIG_DIR".into(), root.join("claude").to_string_lossy().into_owned());
    let pi = settings.providers.entry(ProviderKind::Pi).or_default();
    pi.env.insert("HOME".into(), home);
    pi.env.insert("PI_CODING_AGENT_DIR".into(), root.join("pi").to_string_lossy().into_owned());
    settings
        .providers
        .entry(ProviderKind::Antigravity)
        .or_default()
        .env
        .insert("HOME".into(), root.join("home").to_string_lossy().into_owned());
    state.settings.set(settings).unwrap();
    Fixture { state, root }
}

const FILE_HARNESSES: [ProviderKind; 2] = [ProviderKind::ClaudeCode, ProviderKind::Pi];

impl Fixture {
    /// A folder; with `git`, a repository.
    fn dir(&self, relative: &str, git: bool) -> PathBuf {
        let dir = self.root.join("work").join(relative);
        std::fs::create_dir_all(&dir).unwrap();
        if git {
            std::fs::create_dir_all(dir.join(".git")).unwrap();
        }
        dir
    }

    fn claude(&self, id: &str, cwd: &Path, age: chrono::Duration) -> PathBuf {
        let at = (Utc::now() - age).to_rfc3339();
        let cwd = cwd.to_string_lossy();
        let lines = [
            serde_json::json!({"type":"user","uuid":"u","parentUuid":null,"cwd":cwd,"sessionId":id,"timestamp":at,
                "message":{"role":"user","content":format!("Question {id}")}}),
            serde_json::json!({"type":"assistant","uuid":"a","parentUuid":"u","cwd":cwd,"sessionId":id,"timestamp":at,
                "message":{"content":[{"type":"text","text":"Answer"}]}}),
        ];
        let dir = self.root.join("claude/projects/fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::write(&path, lines.iter().map(|line| line.to_string() + "\n").collect::<String>()).unwrap();
        set_age(&path, age);
        path
    }

    fn pi(&self, id: &str, cwd: &Path, age: chrono::Duration) -> PathBuf {
        let at = (Utc::now() - age).to_rfc3339();
        let lines = [
            serde_json::json!({"type":"session","id":id,"cwd":cwd.to_string_lossy()}),
            serde_json::json!({"type":"message","id":"u","parentId":null,"timestamp":at,
                "message":{"role":"user","content":[{"type":"text","text":format!("Pi question {id}")}]}}),
            serde_json::json!({"type":"message","id":"a","parentId":"u","timestamp":at,
                "message":{"role":"assistant","content":[{"type":"text","text":"Pi answer"}]}}),
        ];
        let dir = self.root.join("pi/sessions/fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::write(&path, lines.iter().map(|line| line.to_string() + "\n").collect::<String>()).unwrap();
        set_age(&path, age);
        path
    }

    /// An Antigravity conversation under `home/.gemini/antigravity-cli`. `workspace` is the folder
    /// agy recorded; without one, the first command's `Cwd` is the only trace of the folder.
    fn agy(&self, id: &str, workspace: Option<&Path>, cwd: &Path, parent: Option<&str>, age: chrono::Duration) {
        let store = self.root.join("home/.gemini/antigravity-cli");
        let logs = store.join("brain").join(id).join(".system_generated/logs");
        std::fs::create_dir_all(&logs).unwrap();
        let db = rusqlite::Connection::open(store.join("conversation_summaries.db")).unwrap();
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS conversation_summaries (conversation_id TEXT PRIMARY KEY, title TEXT, preview TEXT, step_count INTEGER,
                last_modified_time INTEGER, workspace_uris TEXT, parent_conversation_id TEXT);",
        )
        .unwrap();
        // `file:///C:/x` on Windows, `file:///x` elsewhere, as an editor sends it.
        let uris = workspace.map(|path| {
            let text = path.to_string_lossy().replace('\\', "/");
            let uri = if text.starts_with('/') { format!("file://{text}") } else { format!("file:///{text}") };
            serde_json::json!([uri]).to_string()
        });
        db.execute(
            "INSERT INTO conversation_summaries VALUES (?1, ?2, '', 3, ?3, ?4, ?5)",
            rusqlite::params![id, format!("Agy {id}"), (Utc::now() - age).timestamp_millis(), uris, parent],
        )
        .unwrap();
        let at = (Utc::now() - age).to_rfc3339();
        let steps = [
            serde_json::json!({"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":at,"content":format!("Agy question {id}")}),
            serde_json::json!({"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":at,"content":"Checking",
                "tool_calls":[{"name":"run_command","args":{"CommandLine":"\"ls\"","Cwd":serde_json::to_string(&cwd.to_string_lossy()).unwrap()}}]}),
            serde_json::json!({"step_index":2,"source":"MODEL","type":"RUN_COMMAND","status":"DONE","created_at":at,"content":"Cargo.toml"}),
            serde_json::json!({"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":at,"content":"Agy answer"}),
        ];
        std::fs::write(logs.join("transcript.jsonl"), steps.iter().map(|step| step.to_string() + "\n").collect::<String>()).unwrap();
    }

    async fn scan(&self) -> SessionsScan {
        let scan =
            self.state.session_imports.start_scan(&self.state, SessionsScanProjectsParams { providers: Some(FILE_HARNESSES.to_vec()) });
        for _ in 0..400 {
            let current = self.state.session_imports.scan_get(&scan.scan_id).unwrap();
            if current.state != SessionsJobState::Running {
                return current;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("scan never finished");
    }

    async fn import(&self, roots: Vec<SessionsImportRoot>, scan_id: Option<String>) -> SessionsImportJob {
        let job = self.state.session_imports.start_import(&self.state, SessionsImportAllParams { roots, scan_id }).unwrap();
        self.finished(&job.job_id).await
    }

    async fn finished(&self, job_id: &str) -> SessionsImportJob {
        for _ in 0..800 {
            let job = self.state.session_imports.import_get(job_id).unwrap();
            if job.state != SessionsJobState::Running {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("import never finished");
    }
}

fn set_age(path: &Path, age: chrono::Duration) {
    let at = std::time::SystemTime::now() - age.to_std().unwrap();
    std::fs::File::options().write(true).open(path).unwrap().set_modified(at).unwrap();
}

fn root_of(path: &Path) -> String {
    kybern_git::simplify_path(&std::fs::canonicalize(path).unwrap()).to_string_lossy().into_owned()
}

fn import_root(path: &Path) -> SessionsImportRoot {
    SessionsImportRoot { root: root_of(path), project_id: None, providers: Some(FILE_HARNESSES.to_vec()), limit: Some(50) }
}

fn days(count: i64) -> chrono::Duration {
    chrono::Duration::days(count)
}

// ---- repository roots ----

#[test]
fn folders_group_under_their_repository_root() {
    let f = fixture();
    let repo = f.dir("app", true);
    let sub = f.dir("app/packages/web", false);
    let plain = f.dir("notes", false);
    let mut resolver = RootResolver::new(Some(f.root.join("home")));
    let top = resolver.resolve(&repo.to_string_lossy()).unwrap();
    let nested = resolver.resolve(&sub.to_string_lossy()).unwrap();
    assert_eq!(top.root, root_of(&repo));
    assert!(top.is_git && !top.folder_missing);
    assert_eq!(nested.root, top.root, "a subfolder joins its repository");
    let notes = resolver.resolve(&plain.to_string_lossy()).unwrap();
    assert_eq!((notes.root.as_str(), notes.is_git), (root_of(&plain).as_str(), false));
    assert!(resolver.resolve("").is_none() && resolver.resolve("relative/dir").is_none());
}

#[test]
fn a_deleted_subfolder_joins_its_repo_but_a_deleted_project_stays_missing() {
    let f = fixture();
    let repo = f.dir("app", true);
    let mut resolver = RootResolver::new(Some(f.root.join("home")));
    let gone_inside = resolver.resolve(&repo.join("deleted/dir").to_string_lossy()).unwrap();
    assert_eq!(gone_inside.root, root_of(&repo));
    assert!(!gone_inside.folder_missing);
    let gone = f.root.join("work/removed-project");
    let missing = resolver.resolve(&gone.to_string_lossy()).unwrap();
    assert_eq!(missing.root, gone.to_string_lossy());
    assert!(missing.folder_missing && !missing.is_git);
}

#[test]
fn a_repository_at_home_does_not_swallow_the_folders_inside_it() {
    let f = fixture();
    let home = f.root.join("home");
    std::fs::create_dir_all(home.join(".git")).unwrap();
    std::fs::create_dir_all(home.join("projects/thing")).unwrap();
    let mut resolver = RootResolver::new(Some(home.clone()));
    let inside = resolver.resolve(&home.join("projects/thing").to_string_lossy()).unwrap();
    assert_eq!(inside.root, root_of(&home.join("projects/thing")));
    assert!(!inside.is_git);
    let at_home = resolver.resolve(&home.to_string_lossy()).unwrap();
    assert!(at_home.is_git, "the home folder itself is still its own repository");
}

#[test]
fn a_linked_worktree_joins_the_main_checkout_and_the_origin_url_is_read() {
    let f = fixture();
    let main = f.dir("main", true);
    std::fs::write(
        main.join(".git/config"),
        "[core]\n\tbare = false\n[remote \"upstream\"]\n\turl = git@x:other.git\n[remote \"origin\"]\n\turl = git@github.com:me/main.git\n",
    )
    .unwrap();
    let admin = main.join(".git/worktrees/feature");
    std::fs::create_dir_all(&admin).unwrap();
    let linked = f.dir("feature-checkout", false);
    std::fs::write(linked.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
    let mut resolver = RootResolver::new(Some(f.root.join("home")));
    let info = resolver.resolve(&linked.to_string_lossy()).unwrap();
    assert_eq!(info.root, root_of(&main));
    assert_eq!(info.git_remote.as_deref(), Some("git@github.com:me/main.git"));
}

// ---- scan ----

#[tokio::test]
async fn a_scan_groups_sessions_by_repository_and_flags_the_active_ones() {
    let f = fixture();
    let busy = f.dir("busy", true);
    let busy_sub = f.dir("busy/apps/api", false);
    let quiet = f.dir("quiet", true);
    let plain = f.dir("plain", false);
    f.claude("b1", &busy, days(1));
    f.claude("b2", &busy_sub, days(2));
    f.pi("b3", &busy, days(3));
    f.claude("q1", &quiet, days(90));
    f.claude("q2", &quiet, days(80));
    f.claude("q3", &quiet, days(70));
    f.claude("p1", &plain, days(1));
    f.claude("p2", &plain, days(1));
    f.claude("p3", &plain, days(1));
    let nowhere = f.root.join("work/vanished");
    f.claude("m1", &nowhere, days(5));
    let project = f.state.orchestrator.add_project(busy.to_string_lossy().into_owned(), None, None).unwrap();

    let mut events = f.state.session_imports.subscribe();
    let scan = f.scan().await;
    assert_eq!(scan.sessions_found, 10);
    assert_eq!(scan.sessions_without_folder, 0);
    assert!(scan.providers.iter().all(|status| status.state == ScanProviderState::Done && status.error.is_none()));
    let by_root = |path: &Path| scan.projects.iter().find(|project| project.root == root_of(path)).unwrap();

    let busy_row = by_root(&busy);
    assert_eq!(busy_row.session_count, 3, "the subfolder session joins the repository");
    assert_eq!(
        busy_row.providers.iter().map(|count| (count.provider, count.sessions)).collect::<Vec<_>>(),
        [(ProviderKind::ClaudeCode, 2), (ProviderKind::Pi, 1)]
    );
    assert!(busy_row.is_git && busy_row.suggested && busy_row.recent_count == 3);
    assert_eq!(busy_row.project_id, Some(project.id), "an existing project is recognised by its path");
    assert_eq!(busy_row.name, "busy");

    let quiet_row = by_root(&quiet);
    assert!(quiet_row.is_git && !quiet_row.suggested, "old sessions are not suggested");
    let plain_row = by_root(&plain);
    assert!(!plain_row.is_git && !plain_row.suggested, "only repositories are suggested");
    let missing_row = scan.projects.iter().find(|project| project.root == nowhere.to_string_lossy()).unwrap();
    assert!(missing_row.folder_missing && missing_row.project_id.is_none());

    let order: Vec<_> = scan.projects.iter().map(|project| project.last_activity).collect();
    assert!(order.windows(2).all(|pair| pair[0] >= pair[1]), "newest activity first");

    // Notifications deliver the same picture, ending with the finished scan.
    let mut last = None;
    while let Ok(event) = events.try_recv() {
        if let SessionsEvent::Scan(changed) = event {
            last = Some(changed.scan);
        }
    }
    assert_eq!(last.expect("scan notifications"), scan);
}

#[cfg(unix)]
#[tokio::test]
async fn one_harness_failing_is_an_error_on_its_row_not_a_failed_scan() {
    let f = fixture();
    let repo = f.dir("app", true);
    f.claude("c1", &repo, days(1));
    // A Codex that dies at startup, and an OpenCode that is not installed at all.
    let broken = f.root.join("codex-broken");
    kybern_protocol::process::write_executable(&broken, "#!/bin/sh\nexit 1\n").unwrap();
    let mut settings = f.state.settings.get();
    settings.providers.entry(ProviderKind::Codex).or_default().binary = Some(broken.to_string_lossy().into_owned());
    settings.providers.entry(ProviderKind::Opencode).or_default().binary =
        Some(f.root.join("no-such-opencode").to_string_lossy().into_owned());
    f.state.settings.set(settings).unwrap();
    let scan = f.state.session_imports.start_scan(
        &f.state,
        SessionsScanProjectsParams { providers: Some(vec![ProviderKind::ClaudeCode, ProviderKind::Codex, ProviderKind::Opencode]) },
    );
    let scan = loop {
        let current = f.state.session_imports.scan_get(&scan.scan_id).unwrap();
        if current.state != SessionsJobState::Running {
            break current;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(scan.state, SessionsJobState::Done);
    let state_of = |kind| scan.providers.iter().find(|status| status.provider == kind).unwrap();
    assert_eq!(state_of(ProviderKind::ClaudeCode).state, ScanProviderState::Done);
    assert_eq!(state_of(ProviderKind::Codex).state, ScanProviderState::Failed);
    assert!(state_of(ProviderKind::Codex).error.is_some());
    assert_eq!(state_of(ProviderKind::Opencode).state, ScanProviderState::Unavailable);
    assert_eq!(scan.projects.len(), 1, "the working harness still delivered its sessions");
}

// ---- import ----

#[tokio::test]
async fn an_import_joins_subfolders_to_one_project_keeps_real_dates_and_is_idempotent() {
    let f = fixture();
    let repo = f.dir("app", true);
    let sub = f.dir("app/packages/web", false);
    f.claude("s1", &repo, days(10));
    f.claude("s2", &sub, days(5));
    f.pi("s3", &sub, days(2));
    let before = f.state.store.projects_list().unwrap().len();

    let job = f.import(vec![import_root(&repo)], None).await;
    assert_eq!(job.state, SessionsJobState::Done);
    let project = &job.projects[0];
    assert_eq!((project.state, project.total, project.done, project.imported), (ImportProjectState::Done, 3, 3, 3));
    assert!(project.skipped.is_empty() && project.error.is_none());

    let projects = f.state.store.projects_list().unwrap();
    assert_eq!(projects.len(), before + 1, "subfolder sessions did not create projects of their own");
    let threads = f.state.store.threads_list(None, true).unwrap();
    assert_eq!(threads.len(), 3);
    assert!(threads.iter().all(|thread| thread.project_id == project.project_id.unwrap()));
    assert!(threads.iter().all(|thread| thread.status != ThreadStatus::Archived));
    let by_session = |id: &str| threads.iter().find(|thread| thread.provider_session_id.as_deref() == Some(id)).unwrap();
    for (id, age) in [("s1", 10), ("s2", 5), ("s3", 2)] {
        let age_in_days = Utc::now().signed_duration_since(by_session(id).updated_at).num_hours() as f64 / 24.0;
        assert!((age_in_days - age as f64).abs() < 0.1, "{id} keeps its own last activity, got {age_in_days} days old");
    }
    assert_eq!(by_session("s2").cwd, sub.to_string_lossy(), "a thread keeps the folder its session ran in");

    // Running it again changes nothing.
    let again = f.import(vec![import_root(&repo)], None).await;
    assert_eq!((again.imported, again.already_imported, again.skipped_count), (0, 3, 0));
    assert_eq!(f.state.store.threads_list(None, true).unwrap().len(), 3);

    // The scan reports them as imported.
    let scan = f.scan().await;
    assert_eq!(scan.projects[0].imported_count, 3);
}

#[tokio::test]
async fn the_limit_takes_the_most_recent_sessions_and_null_takes_all() {
    let f = fixture();
    let repo = f.dir("app", true);
    for (index, id) in ["a", "b", "c", "d"].into_iter().enumerate() {
        f.claude(id, &repo, days(index as i64 + 1));
    }
    let mut limited = import_root(&repo);
    limited.limit = Some(2);
    let job = f.import(vec![limited.clone()], None).await;
    assert_eq!((job.projects[0].total, job.imported), (2, 2));
    let imported: HashSet<String> =
        f.state.store.threads_list(None, true).unwrap().into_iter().filter_map(|thread| thread.provider_session_id).collect();
    assert_eq!(imported, HashSet::from(["a".to_string(), "b".to_string()]), "the two newest");

    // Same window again: nothing new. Then lift the limit: only the rest is added.
    assert_eq!(f.import(vec![limited], None).await.imported, 0);
    let mut all = import_root(&repo);
    all.limit = None;
    let job = f.import(vec![all], None).await;
    assert_eq!((job.projects[0].total, job.imported, job.already_imported), (4, 2, 2));
}

#[tokio::test]
async fn the_default_limit_is_fifty_and_null_means_all() {
    let default: SessionsImportRoot = serde_json::from_value(serde_json::json!({"root": "/r"})).unwrap();
    assert_eq!(default.limit, Some(50));
    let all: SessionsImportRoot = serde_json::from_value(serde_json::json!({"root": "/r", "limit": null})).unwrap();
    assert_eq!(all.limit, None);
}

#[tokio::test]
async fn sessions_that_cannot_be_imported_are_skipped_with_a_reason_and_the_rest_continue() {
    let f = fixture();
    let repo = f.dir("app", true);
    f.claude("fine", &repo, days(3));
    f.claude("live", &repo, chrono::Duration::seconds(5));
    let gone = f.dir("app/temporary", false);
    f.claude("orphan", &gone, days(4));
    std::fs::remove_dir_all(&gone).unwrap();
    let huge = f.claude("huge", &repo, days(6));
    std::fs::File::options().write(true).open(&huge).unwrap().set_len(65 * 1024 * 1024).unwrap();
    set_age(&huge, days(6));
    let partial = f.claude("partial", &repo, days(7));
    let mut text = std::fs::read_to_string(&partial).unwrap();
    text.push_str("{\"type\":\"assistant\",\"uuid\":\"cut");
    std::fs::write(&partial, text).unwrap();
    set_age(&partial, days(7));

    let mut root = import_root(&repo);
    root.limit = None;
    let job = f.import(vec![root], None).await;
    let project = &job.projects[0];
    assert_eq!((project.total, project.done, project.imported, project.skipped_count), (5, 5, 1, 4));
    let reason = |id: &str| project.skipped.iter().find(|skip| skip.session_id == id).map(|skip| skip.reason);
    assert_eq!(reason("live"), Some(SessionSkipReason::InUse));
    assert_eq!(reason("orphan"), Some(SessionSkipReason::FolderMissing));
    assert_eq!(reason("huge"), Some(SessionSkipReason::TooLarge));
    assert_eq!(reason("partial"), Some(SessionSkipReason::InUse), "a half-written last line means its harness is still writing");
    assert!(project.skipped.iter().all(|skip| !skip.detail.is_empty()));
    let threads = f.state.store.threads_list(None, true).unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].provider_session_id.as_deref(), Some("fine"));
}

#[tokio::test]
async fn a_folder_that_is_gone_fails_its_project_but_not_the_job() {
    let f = fixture();
    let kept = f.dir("kept", true);
    f.claude("k1", &kept, days(2));
    let gone = f.root.join("work/removed");
    f.claude("g1", &gone, days(2));
    let roots = vec![
        SessionsImportRoot {
            root: gone.to_string_lossy().into_owned(),
            project_id: None,
            providers: Some(FILE_HARNESSES.to_vec()),
            limit: Some(50),
        },
        import_root(&kept),
    ];
    let job = f.import(roots, None).await;
    assert_eq!(job.state, SessionsJobState::Done);
    assert_eq!(job.projects[0].state, ImportProjectState::Failed);
    assert_eq!(job.projects[0].skipped[0].reason, SessionSkipReason::FolderMissing);
    assert_eq!(job.projects[1].imported, 1);
}

#[tokio::test]
async fn an_import_can_reuse_a_scans_list_and_join_a_named_project() {
    let f = fixture();
    let repo = f.dir("app", true);
    let sub = f.dir("app/tools", false);
    f.claude("one", &sub, days(2));
    let scan = f.scan().await;
    // A later session appears; the scan's list is what the import uses.
    f.claude("two", &repo, days(1));
    let target = f.dir("elsewhere", true);
    let project = f.state.orchestrator.add_project(target.to_string_lossy().into_owned(), None, None).unwrap();
    let mut root = import_root(&repo);
    root.project_id = Some(project.id);
    let job = f.import(vec![root], Some(scan.scan_id)).await;
    assert_eq!(job.imported, 1);
    let thread = f.state.store.threads_list(None, true).unwrap().remove(0);
    assert_eq!(thread.provider_session_id.as_deref(), Some("one"));
    assert_eq!(thread.project_id, project.id, "an explicit project overrides the one for the root");
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_stops_a_job_before_it_imports_anything_more() {
    let f = fixture();
    let repo = f.dir("app", true);
    for index in 0..6 {
        f.claude(&format!("c{index}"), &repo, days(index + 1));
    }
    let job =
        f.state.session_imports.start_import(&f.state, SessionsImportAllParams { roots: vec![import_root(&repo)], scan_id: None }).unwrap();
    // The job task has not run yet on this single-threaded runtime, yet the daemon already counts it as work.
    assert_eq!(f.state.session_imports.running(), 1);
    let snapshot = f.state.session_imports.import_cancel(&job.job_id).unwrap();
    assert_eq!(snapshot.imported, 0);
    let done = f.finished(&job.job_id).await;
    assert_eq!(done.state, SessionsJobState::Canceled);
    assert_eq!(done.imported, 0);
    assert!(done.projects.iter().all(|project| project.state == ImportProjectState::Done));
    assert_eq!(f.state.session_imports.running(), 0, "the job no longer keeps the daemon busy");
}

#[tokio::test]
async fn progress_and_the_final_summary_arrive_as_notifications() {
    let f = fixture();
    let repo = f.dir("app", true);
    for index in 0..3 {
        f.claude(&format!("n{index}"), &repo, days(index + 1));
    }
    f.claude("late", &f.dir("app/gone", false), days(9));
    std::fs::remove_dir_all(f.root.join("work/app/gone")).unwrap();
    let mut events = f.state.session_imports.subscribe();
    let job = f.import(vec![import_root(&repo)], None).await;
    let (mut ids, mut skipped, mut finished, mut last) = (Vec::new(), Vec::new(), None, None);
    while let Ok(event) = events.try_recv() {
        match event {
            SessionsEvent::Progress(progress) => {
                assert_eq!(progress.job_id, job.job_id);
                ids.extend(progress.new_thread_ids.clone());
                skipped.extend(progress.new_skipped.clone());
                last = Some(progress);
            }
            SessionsEvent::Finished(done) => finished = Some(done.job),
            SessionsEvent::Scan(_) => {}
        }
    }
    let last = last.expect("progress");
    assert_eq!((last.state, last.total, last.done, last.imported, last.skipped_count), (ImportProjectState::Done, 4, 4, 3, 1));
    let mut expected = job.projects[0].imported_thread_ids.clone();
    ids.sort();
    expected.sort();
    assert_eq!(ids, expected, "every thread id is announced exactly once");
    assert_eq!(skipped, job.projects[0].skipped);
    assert_eq!(finished.expect("final summary"), job);
}

#[tokio::test]
async fn a_bulk_import_announces_each_thread_without_streaming_its_history() {
    let f = fixture();
    let repo = f.dir("app", true);
    for index in 0..3 {
        f.claude(&format!("h{index}"), &repo, days(index + 1));
    }
    let mut live = f.state.events.subscribe();
    let job = f.import(vec![import_root(&repo)], None).await;
    let threads = &job.projects[0].imported_thread_ids;
    assert_eq!(threads.len(), 3);
    let mut kinds: HashMap<ThreadId, Vec<&'static str>> = HashMap::new();
    while let Ok(event) = live.try_recv() {
        let kind = match event.payload {
            EventPayload::ThreadCreated { .. } => "created",
            EventPayload::SessionImported { .. } => "imported",
            _ => "history",
        };
        kinds.entry(event.thread_id).or_default().push(kind);
    }
    for id in threads {
        assert_eq!(kinds.get(id).map(Vec::as_slice), Some(&["created", "imported"][..]), "live clients hear only that the thread exists");
        let stored = f.state.store.events_for_thread(*id).unwrap();
        assert!(stored.len() > 2, "the history itself is stored for whoever opens the thread");
    }
}

// ---- the single-session resume is untouched ----

#[tokio::test]
async fn resuming_one_session_still_makes_its_own_project_and_stamps_now() {
    let f = fixture();
    let repo = f.dir("app", true);
    let sub = f.dir("app/packages/web", false);
    f.claude("solo", &sub, days(20));
    let thread = f
        .state
        .orchestrator
        .resume_external_session(SessionsResumeParams { provider: ProviderKind::ClaudeCode, session_id: "solo".into(), project_id: None })
        .await
        .unwrap();
    let project = f.state.store.project_get(thread.project_id).unwrap().unwrap();
    assert_eq!(project.path, root_of(&sub), "the existing behaviour: a project for the session's own folder");
    assert_ne!(project.path, root_of(&repo));
    assert!(Utc::now().signed_duration_since(thread.updated_at).num_minutes() < 5, "a resumed thread is stamped now");
    let again = f
        .state
        .orchestrator
        .resume_external_session(SessionsResumeParams { provider: ProviderKind::ClaudeCode, session_id: "solo".into(), project_id: None })
        .await
        .unwrap();
    assert_eq!(again.id, thread.id);
}

#[tokio::test]
async fn antigravity_conversations_group_by_repository_and_import_with_their_cards() {
    let f = fixture();
    let repo = f.dir("app", true);
    let sub = f.dir("app/packages/web", false);
    let other = f.dir("other", true);
    f.agy("g1", Some(&repo), &repo, None, days(10));
    f.agy("g2", Some(&sub), &sub, None, days(3));
    f.agy("g3", None, &sub, None, days(2));
    f.agy("g4", Some(&other), &other, None, days(1));
    f.agy("child", Some(&repo), &repo, Some("g1"), days(1));
    f.agy("live", Some(&repo), &repo, None, chrono::Duration::seconds(5));
    f.claude("c1", &repo, days(4));
    let kinds = vec![ProviderKind::Antigravity, ProviderKind::ClaudeCode];

    let scan = f.state.session_imports.start_scan(&f.state, SessionsScanProjectsParams { providers: Some(kinds.clone()) });
    let scan = loop {
        let current = f.state.session_imports.scan_get(&scan.scan_id).unwrap();
        if current.state != SessionsJobState::Running {
            break current;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(scan.sessions_found, 6, "four conversations, the one being written, and the Claude session; the sub-agent is not listed");
    assert_eq!(scan.sessions_without_folder, 0, "the Cwd of a command stands in for a missing workspace");
    assert!(scan.providers.iter().all(|status| status.state == ScanProviderState::Done && status.error.is_none()));
    let app = scan.projects.iter().find(|project| project.root == root_of(&repo)).unwrap();
    assert_eq!(app.session_count, 5);
    let count = |kind| app.providers.iter().find(|count| count.provider == kind).map(|count| count.sessions);
    assert_eq!((count(ProviderKind::Antigravity), count(ProviderKind::ClaudeCode)), (Some(4), Some(1)));

    let root = SessionsImportRoot { root: root_of(&repo), project_id: None, providers: Some(kinds.clone()), limit: None };
    let job = f.import(vec![root], Some(scan.scan_id.clone())).await;
    assert_eq!(job.state, SessionsJobState::Done);
    let project = &job.projects[0];
    assert_eq!((project.total, project.imported, project.skipped_count), (5, 4, 1));
    assert_eq!(project.skipped[0].session_id, "live");
    assert_eq!(project.skipped[0].reason, SessionSkipReason::InUse);

    let threads = f.state.store.threads_list(None, true).unwrap();
    let agy_thread = |id: &str| threads.iter().find(|thread| thread.provider_session_id.as_deref() == Some(id)).unwrap();
    assert!(threads.iter().all(|thread| thread.project_id == project.project_id.unwrap()), "the other repository was not imported");
    assert!(
        threads
            .iter()
            .all(|thread| thread.provider_session_id.as_deref() != Some("g4") && thread.provider_session_id.as_deref() != Some("child"))
    );
    let g2 = agy_thread("g2");
    assert_eq!(g2.provider.kind, ProviderKind::Antigravity);
    assert_eq!(g2.provider_session_id.as_deref(), Some("g2"), "resuming passes this id as --conversation");
    assert_eq!(g2.title, "Agy g2");
    assert_eq!(Path::new(&g2.cwd), sub, "compared as paths: Windows spells the separators one way");
    assert_eq!(Path::new(&agy_thread("g3").cwd), sub);
    assert_eq!(g2.permission_mode, PermissionMode::FullAccess, "Antigravity only runs in Full access");
    let age = Utc::now().signed_duration_since(g2.updated_at).num_hours() as f64 / 24.0;
    assert!((age - 3.0).abs() < 0.1, "keeps its own last activity, got {age} days");
    let events = f.state.store.events_for_thread(g2.id).unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(&event.payload, EventPayload::TurnStarted { message, .. } if message.plain_text() == "Agy question g2"))
    );
    assert!(
        events.iter().any(
            |event| matches!(&event.payload, EventPayload::ToolCallStarted { call, .. } if call.id == "agy:g2:2" && call.name == "Bash")
        )
    );

    // Running it again changes nothing, and the source folder is still just what the test wrote.
    let again =
        f.import(vec![SessionsImportRoot { root: root_of(&repo), project_id: None, providers: Some(kinds), limit: None }], None).await;
    assert_eq!((again.imported, again.already_imported), (0, 4));
}
