// Remote delegation scenarios (ADE-86 R1, design §6.2, §6.4, §11.2). Included into
// `orchestrator::tests` so they share the scripted driver of `delegation_tests.rs`. Each test runs two
// real daemon states (machine "a", which delegates, and machine "b", which runs the child) on the
// in-process multi-daemon harness, with the real session handshake between them. They skip on a
// computer without the `127.x.0.n` loopback aliases (macOS: `sudo ifconfig lo0 alias 127.1.0.2`).

use crate::network::harness::{Cluster, Node, NodeSpec, WAIT, eventually as wait_until};

/// Two machines in one network, a git repository checked out on both, and a parent agent running on `a`.
struct Pair {
    cluster: Cluster,
    a: Node,
    b: Node,
    /// The agents machine b started.
    spawned: Arc<std::sync::Mutex<Vec<ScriptedChild>>>,
    /// Machine a's orchestrator, as the delegation fixtures drive it.
    fixture: Fixture,
    parent: Thread,
    live: Arc<LiveSession>,
    repo_a: PathBuf,
    repo_b: PathBuf,
}

fn scripted_drivers(spawned: &Arc<std::sync::Mutex<Vec<ScriptedChild>>>) -> DriverRegistry {
    let mut drivers = DriverRegistry::default();
    drivers.register(Arc::new(ScriptedDriver { kind: ProviderKind::ClaudeCode, spawned: spawned.clone() }));
    drivers.register(Arc::new(ScriptedDriver { kind: ProviderKind::Codex, spawned: spawned.clone() }));
    drivers
}

fn git_project(name: &str, path: &std::path::Path) -> Project {
    let now = chrono::Utc::now();
    Project {
        repo: None,
        repos: Vec::new(),
        id: Uuid::now_v7(),
        name: name.into(),
        path: path.to_string_lossy().into_owned(),
        is_git: true,
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
    }
}

/// A bare "origin" with one pushed commit and two checkouts of it: the two machines' copies.
fn two_checkouts(root: &std::path::Path) -> (PathBuf, PathBuf) {
    let origin = root.join("origin.git");
    std::fs::create_dir_all(&origin).unwrap();
    run_git(&origin, &["init", "-q", "--bare", "-b", "main"]);
    let seed = root.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    run_git(&seed, &["init", "-q", "-b", "main"]);
    run_git(&seed, &["config", "core.autocrlf", "false"]);
    // The transfer assertions compare exact bytes even on a CRLF-default machine.
    std::fs::write(seed.join(".gitattributes"), "* text eol=lf\n").unwrap();
    std::fs::write(seed.join("tracked.txt"), "one\n").unwrap();
    run_git(&seed, &["add", "."]);
    run_git(&seed, &["commit", "-q", "-m", "init"]);
    run_git(&seed, &["remote", "add", "origin", origin.to_str().unwrap()]);
    run_git(&seed, &["push", "-q", "origin", "main"]);
    let (a, b) = (root.join("checkout-a"), root.join("checkout-b"));
    for dir in [&a, &b] {
        run_git(root, &["clone", "-q", "--config", "core.autocrlf=false", origin.to_str().unwrap(), dir.to_str().unwrap()]);
    }
    (a, b)
}

/// A group project of the given member folders (each a repository) at `path`.
fn group_project(name: &str, path: &std::path::Path, members: &[&str]) -> Project {
    let mut project = git_project(name, path);
    project.is_git = false;
    project.kind = ProjectKind::Group;
    project.members =
        members.iter().map(|member| ProjectMember { path: (*member).into(), name: (*member).into(), status: MemberStatus::Ok }).collect();
    project
}

/// Two group folders, `group-a` and `group-b`, each holding checkouts of the repositories `api` and `web`
/// (b only of those in `on_b`): every repository has its own bare origin and its own root commit.
fn group_checkouts(root: &std::path::Path, on_b: &[&str]) -> (PathBuf, PathBuf) {
    let (folder_a, folder_b) = (root.join("group-a"), root.join("group-b"));
    for folder in [&folder_a, &folder_b] {
        std::fs::create_dir_all(folder).unwrap();
    }
    for member in ["api", "web"] {
        let origin = root.join(format!("origin-{member}.git"));
        std::fs::create_dir_all(&origin).unwrap();
        run_git(&origin, &["init", "-q", "--bare", "-b", "main"]);
        let seed = root.join(format!("seed-{member}"));
        std::fs::create_dir_all(&seed).unwrap();
        run_git(&seed, &["init", "-q", "-b", "main"]);
        run_git(&seed, &["config", "core.autocrlf", "false"]);
        std::fs::write(seed.join(".gitattributes"), "* text eol=lf\n").unwrap();
        std::fs::write(seed.join("tracked.txt"), format!("{member}\n")).unwrap();
        run_git(&seed, &["add", "."]);
        run_git(&seed, &["commit", "-q", "-m", &format!("init {member}")]);
        run_git(&seed, &["remote", "add", "origin", origin.to_str().unwrap()]);
        run_git(&seed, &["push", "-q", "origin", "main"]);
        run_git(root, &["clone", "-q", "--config", "core.autocrlf=false", origin.to_str().unwrap(), folder_a.join(member).to_str().unwrap()]);
        if on_b.contains(&member) {
            run_git(root, &["clone", "-q", "--config", "core.autocrlf=false", origin.to_str().unwrap(), folder_b.join(member).to_str().unwrap()]);
        }
    }
    (folder_a, folder_b)
}

impl Pair {
    async fn new() -> Option<Self> {
        Self::build(None).await
    }

    /// Like `new`, but the project is a group of two repositories, `api` and `web`: machine a has both and
    /// machine b has those in `on_b`. `repo_a` and `repo_b` are the group folders.
    async fn group(on_b: &[&str]) -> Option<Self> {
        Self::build(Some(on_b)).await
    }

    async fn build(group_on_b: Option<&[&str]>) -> Option<Self> {
        let cluster = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b")])?;
        let spawned = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (repo_a, repo_b, project_a, project_b) = match group_on_b {
            None => {
                let (repo_a, repo_b) = two_checkouts(cluster.dir.path());
                let (project_a, project_b) = (git_project("repo-a", &repo_a), git_project("repo-b", &repo_b));
                (repo_a, repo_b, project_a, project_b)
            }
            Some(on_b) => {
                let (repo_a, repo_b) = group_checkouts(cluster.dir.path(), on_b);
                let (project_a, project_b) = (group_project("acme-a", &repo_a, &["api", "web"]), group_project("acme-b", &repo_b, on_b));
                (repo_a, repo_b, project_a, project_b)
            }
        };
        let a = cluster.start_with("a", true, scripted_drivers(&Arc::new(std::sync::Mutex::new(Vec::new())))).await;
        let b = cluster.start_with("b", true, scripted_drivers(&spawned)).await;
        cluster.form(&a, &[&b], JoinVia::Auto).await;
        a.state.store.project_insert(&project_a).unwrap();
        b.state.store.project_insert(&project_b).unwrap();
        wait_until("a and b linked", WAIT, || crate::network::harness::linked(&a, &b)).await;

        let fixture = Fixture {
            root: std::env::temp_dir().join(format!("kybern-remote-fixture-{}", Uuid::now_v7())),
            store: a.state.store.clone(),
            orchestrator: a.state.orchestrator.clone(),
            project: project_a,
        };
        fixture.orchestrator.set_delegation_debounce(Duration::from_millis(60));
        let mut parent = fixture.thread(ThreadStatus::Idle);
        parent.title = "Parent on a".into();
        if group_on_b.is_some() {
            parent.members = super::group_git::snapshot_members(&fixture.project);
        }
        fixture.store.thread_upsert(&parent).unwrap();
        let (live, _, _) = fixture.park_recording(&parent, Instant::now()).await;
        fixture.orchestrator.send(parent.id, UserMessage::text("coordinate the work")).await.unwrap();
        live.turn_ready.notified().await;
        Some(Self { cluster, a, b, spawned, fixture, parent, live, repo_a, repo_b })
    }

    async fn tool(&self, name: &str, args: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        self.fixture.tool(&self.parent, &self.live, &Uuid::now_v7().to_string(), name, args).await
    }

    /// `kybern_agent_delegate` to machine b.
    async fn delegate(&self, mut args: serde_json::Value) -> serde_json::Value {
        args["machine"] = json!(self.b.id());
        self.tool("kybern_agent_delegate", args).await.unwrap()
    }

    /// The agent machine b started whose brief mentions `needle`.
    async fn agent(&self, needle: &str) -> ScriptedChild {
        eventually(|| async {
            let candidates: Vec<ScriptedChild> = self.spawned.lock().unwrap().clone();
            for candidate in candidates {
                if candidate.transcript().await.contains(needle) {
                    return Some(candidate);
                }
            }
            None
        })
        .await
    }

    /// End the parent's turn so results queue instead of steering.
    async fn end_parent_turn(&self) {
        self.fixture
            .orchestrator
            .process_driver_event(self.parent.id, &self.live, completed_response(StopReason::Completed), false)
            .await
            .unwrap();
    }

    /// Every result batch queued for the parent, flattened.
    fn results(&self) -> Vec<AgentResultItem> {
        self.a
            .state
            .store
            .queue_list(Some(self.parent.id))
            .unwrap()
            .into_iter()
            .flat_map(|queued| match queued.message.parts.as_slice() {
                [ContentPart::AgentResults { items }] => items.clone(),
                _ => Vec::new(),
            })
            .collect()
    }

    fn child_on_b(&self, task_id: Uuid) -> Option<Thread> {
        self.b.state.store.delegation_find_by_task(task_id).unwrap()
    }

    fn children_on_b(&self) -> usize {
        self.b.state.store.threads_list(None, true).unwrap().into_iter().filter(|thread| thread.delegation.is_some()).count()
    }

    fn row(&self, task_id: Uuid) -> RemoteChild {
        self.a.state.store.remote_child_get(task_id).unwrap().expect("a remote child row")
    }

    fn outbox(&self) -> Vec<OutboxItem> {
        self.a.state.store.outbox_list(None, None).unwrap()
    }

    /// Machine b leaves the network (its listener closes and its sessions end).
    async fn b_offline(&self) {
        self.b.set_network_enabled(false);
        wait_until("a loses b", WAIT, || self.a.state.network.peers.link(&self.b.id()).is_none()).await;
    }

    async fn b_online(&self) {
        self.b.set_network_enabled(true);
        wait_until("a and b linked again", WAIT, || crate::network::harness::linked(&self.a, &self.b)).await;
    }

    /// Machine b dies and starts again on the same data directory.
    async fn restart_b(&mut self) {
        self.b.state.exposure.clear_network().await;
        self.b.stop();
        self.b = self.cluster.start_with("b", true, scripted_drivers(&self.spawned)).await;
        wait_until("a and the restarted b linked", WAIT, || crate::network::harness::linked(&self.a, &self.b)).await;
    }
}

fn task_id_of(value: &serde_json::Value) -> Uuid {
    Uuid::parse_str(value["task_id"].as_str().unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delegation_runs_on_the_other_machine_and_its_result_arrives_once() {
    let Some(p) = Pair::new().await else { return };
    let operation = Uuid::now_v7();
    let started = p.delegate(json!({"task": "Fix the flaky clock test", "title": "Flaky test", "operation_id": operation})).await;
    assert_eq!(started["status"], "running");
    assert_eq!(started["remote_state"], "running", "the target acknowledged: {started}");
    let task_id = task_id_of(&started);

    // The child is an ordinary delegated thread on b, stamped with where it came from.
    let child = eventually(|| async { p.child_on_b(task_id) }).await;
    assert_eq!(child.cwd, p.repo_b.to_string_lossy(), "a shared child works in the target's own checkout");
    let origin = child.origin.clone().expect("origin");
    assert_eq!(origin.machine_id, p.a.id());
    assert_eq!(origin.thread_id, Some(p.parent.id));
    assert_eq!(origin.permission_cap, PermissionMode::Supervised);
    assert_eq!(origin.hops, vec![p.a.id()]);
    assert!(matches!(origin.kind, OriginKind::Delegation { .. }));
    assert_eq!(child.permission_mode, PermissionMode::Supervised);
    assert_eq!(child.parent_thread_id, None, "nothing on b is its parent");
    assert!(child.is_remote_child(), "b lists it as a thread of its own, under the thread on a that delegated it");
    let agent = p.agent("Fix the flaky clock test").await;
    assert!(agent.transcript().await.contains("different machine"), "the brief says it runs elsewhere");

    // a knows it as a task of the parent, and a repeated call returns the same child.
    let status = p.tool("kybern_agent_status", json!({"task_ids": [task_id]})).await.unwrap();
    assert_eq!(status["agents"][0]["remote_state"], "running");
    assert_eq!(status["agents"][0]["title"], "Flaky test");
    let again = p.delegate(json!({"task": "Fix the flaky clock test", "operation_id": operation})).await;
    assert_eq!(task_id_of(&again), task_id);
    assert_eq!(p.children_on_b(), 1);
    assert_eq!(p.outbox().len(), 1);
    assert_eq!(p.outbox()[0].state, OutboxState::Delivered);

    p.end_parent_turn().await;
    agent.finish("Fixed it: the clock was racing the test.").await;
    eventually(|| async { Some(p.results()).filter(|items| !items.is_empty()) }).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let items = p.results();
    assert_eq!(items.len(), 1, "the result arrives once: {items:?}");
    let item = &items[0];
    assert_eq!((item.task_id, item.status), (task_id, DelegationStatus::Completed));
    assert!(item.result.as_deref().unwrap().contains("clock was racing"));
    eventually(|| async { (p.row(task_id).state == RemoteChildState::Completed && p.row(task_id).info.parent_notified).then_some(()) }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_mode_and_kybern_agent_wait_return_a_remote_childs_result_without_repeating_it() {
    let Some(p) = Pair::new().await else { return };
    // `mode: wait` returns the result inline.
    let finishing = p.spawned.clone();
    let finisher = tokio::spawn(async move {
        let agent = eventually(|| async {
            let candidates: Vec<ScriptedChild> = finishing.lock().unwrap().clone();
            for candidate in candidates {
                if candidate.transcript().await.contains("Quick job") {
                    return Some(candidate);
                }
            }
            None
        })
        .await;
        agent.finish("Quick job done.").await;
    });
    let done = p.delegate(json!({"task": "Quick job", "mode": "wait", "timeout_ms": 20000})).await;
    finisher.await.unwrap();
    assert_eq!((done["status"].as_str(), done["result"].as_str()), (Some("completed"), Some("Quick job done.")), "{done}");
    // And kybern_agent_wait on a second, still running child.
    let second = task_id_of(&p.delegate(json!({"task": "Slow job"})).await);
    let timed_out = p.tool("kybern_agent_wait", json!({"task_ids": [second], "timeout_ms": 300})).await.unwrap();
    assert_eq!(timed_out["wait_timed_out"], true);
    p.agent("Slow job").await.finish("Slow job done.").await;
    let waited = p.tool("kybern_agent_wait", json!({"task_ids": [second], "timeout_ms": 20000})).await.unwrap();
    assert_eq!(waited["wait_timed_out"], false);
    assert_eq!(waited["agents"][0]["result"], "Slow job done.");
    assert_eq!(waited["agents"][0]["machine"]["id"], p.b.id());
    // Both outcomes were returned to the caller, so neither is queued again for the parent.
    p.end_parent_turn().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(p.results().is_empty(), "{:?}", p.results());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worktree_child_starts_from_the_parents_uncommitted_changes_without_pushing_them() {
    let Some(p) = Pair::new().await else { return };
    // One unpushed commit and unsaved edits, including an untracked file, on machine a.
    std::fs::write(p.repo_a.join("tracked.txt"), "one\ntwo\n").unwrap();
    run_git(&p.repo_a, &["commit", "-q", "-am", "unpushed"]);
    std::fs::write(p.repo_a.join("tracked.txt"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(p.repo_a.join("wip.txt"), "work in progress\n").unwrap();
    let remote_before = run_git(&p.repo_a, &["ls-remote", "origin"]);

    let started = p.delegate(json!({"task": "Continue my edits", "workspace": "worktree"})).await;
    let task_id = task_id_of(&started);
    assert_eq!(started["remote_state"], "running", "{started}");
    let child = eventually(|| async { p.child_on_b(task_id) }).await;
    let worktree = child.worktree.clone().expect("a worktree");
    assert!(worktree.branch.starts_with("kybern/"), "{worktree:?}");
    assert_eq!(std::fs::read_to_string(std::path::Path::new(&child.cwd).join("tracked.txt")).unwrap(), "one\ntwo\nthree\n");
    assert_eq!(std::fs::read_to_string(std::path::Path::new(&child.cwd).join("wip.txt")).unwrap(), "work in progress\n");
    let info = child.delegation.clone().unwrap();
    assert_eq!(info.workspace, DelegationWorkspace::Worktree);
    assert!(info.base_commit.is_some());

    // Nothing reached the shared remote, and a's working tree is as it was.
    assert_eq!(run_git(&p.repo_a, &["ls-remote", "origin"]), remote_before);
    assert_eq!(run_git(&p.repo_a, &["status", "--porcelain"]).lines().count(), 2);
    // The child's own commit comes back as its branch and head in the result.
    let agent = p.agent("Continue my edits").await;
    std::fs::write(std::path::Path::new(&child.cwd).join("continued.txt"), "more\n").unwrap();
    run_git(std::path::Path::new(&child.cwd), &["add", "."]);
    run_git(std::path::Path::new(&child.cwd), &["commit", "-q", "-m", "continued"]);
    p.end_parent_turn().await;
    agent.finish("Continued.").await;
    let items = eventually(|| async { Some(p.results()).filter(|items| !items.is_empty()) }).await;
    assert_eq!(items[0].branch.as_deref(), Some(worktree.branch.as_str()));
    assert!(items[0].head_commit.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_offline_target_keeps_the_child_pending_and_runs_it_when_the_machine_returns() {
    let Some(p) = Pair::new().await else { return };
    p.b_offline().await;
    let started = p.delegate(json!({"task": "Run the slow suite", "title": "Slow suite"})).await;
    let task_id = task_id_of(&started);
    assert_eq!(started["remote_state"], "pending", "{started}");
    assert!(started["note"].as_str().unwrap().contains("Waiting for"), "{started}");
    assert_eq!(started["status"], "running", "pending is not a failure");
    // Visible in the outbox, nothing on b, and the parent counts it as running.
    let items = p.outbox();
    assert_eq!((items.len(), items[0].state, items[0].kind), (1, OutboxState::Queued, OutboxKind::Delegate));
    assert_eq!(items[0].origin.as_ref().unwrap().machine_id, p.a.id());
    assert_eq!(p.children_on_b(), 0);
    let capabilities = p.tool("kybern_agent_capabilities", json!({})).await.unwrap();
    assert_eq!(capabilities["limits"]["active_children"], 1);
    let listed = crate::network::outbox::list(&p.a.state, OutboxListParams { target: Some(p.b.id()), state: Some(OutboxState::Queued) }).unwrap();
    assert_eq!(listed.items.len(), 1);

    // Clients hear about the item as it goes: sending, then delivered.
    let mut notifications = p.a.state.network.subscribe_outbox();
    p.b_online().await;
    let child = eventually(|| async { p.child_on_b(task_id) }).await;
    assert_eq!(child.origin.unwrap().machine_id, p.a.id());
    eventually(|| async { (p.row(task_id).state == RemoteChildState::Running).then_some(()) }).await;
    assert_eq!(p.outbox()[0].state, OutboxState::Delivered);
    assert_eq!(p.children_on_b(), 1);
    let mut states = Vec::new();
    while let Ok(item) = notifications.try_recv() {
        states.push(item.state);
    }
    assert!(states.contains(&OutboxState::Sending) && states.contains(&OutboxState::Delivered), "{states:?}");

    p.end_parent_turn().await;
    p.agent("Run the slow suite").await.finish("Suite passed.").await;
    let items = eventually(|| async { Some(p.results()).filter(|items| !items.is_empty()) }).await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].status, DelegationStatus::Completed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delivery_whose_acknowledgement_was_lost_does_not_start_a_second_child() {
    let Some(p) = Pair::new().await else { return };
    p.b_offline().await;
    let started = p.delegate(json!({"task": "Count the crates"})).await;
    let task_id = task_id_of(&started);
    // An earlier delivery reached b and a crashed before it recorded the answer: replay it directly.
    let item = p.outbox().remove(0);
    let first = p.b.state.orchestrator.peer_dispatch(&p.a.id(), "peer.delegate", item.payload.clone()).await.unwrap();
    assert_eq!(p.children_on_b(), 1);

    p.b_online().await;
    eventually(|| async { (p.row(task_id).state == RemoteChildState::Running).then_some(()) }).await;
    assert_eq!(p.children_on_b(), 1, "b answered the repeat with the child it already had");
    assert_eq!(p.row(task_id).remote_thread_id.unwrap().to_string(), first["thread_id"].as_str().unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_of_the_delegating_machine_before_and_during_the_follow_loses_nothing() {
    let Some(mut p) = Pair::new().await else { return };
    // Mid-follow: one child is acknowledged and running.
    let running = task_id_of(&p.delegate(json!({"task": "Running task"})).await);
    eventually(|| async { (p.row(running).state == RemoteChildState::Running).then_some(()) }).await;
    // Before anything was sent: another has a row and a queued item only, because b is away.
    p.b_offline().await;
    let early = task_id_of(&p.delegate(json!({"task": "Early task"})).await);

    // a dies and starts again on the same data directory.
    let id_a = p.a.id();
    p.a.stop();
    p.a = p.cluster.start_with("a", true, scripted_drivers(&Arc::new(std::sync::Mutex::new(Vec::new())))).await;
    assert_eq!(p.a.id(), id_a);
    assert_eq!(p.row(early).state, RemoteChildState::Pending);
    assert_eq!(p.outbox().iter().filter(|item| item.state == OutboxState::Queued).count(), 1);

    p.b_online().await;
    eventually(|| async { (p.row(early).state == RemoteChildState::Running).then_some(()) }).await;
    assert_eq!(p.children_on_b(), 2, "one child for each task, no duplicates");
    // The agents finish while a follows again: each result arrives once.
    for (needle, text) in [("Early task", "early done"), ("Running task", "running done")] {
        p.agent(needle).await.finish(text).await;
    }
    let queued = |p: &Pair| -> Vec<AgentResultItem> {
        p.a.state
            .store
            .queue_list(None)
            .unwrap()
            .into_iter()
            .flat_map(|queued| match queued.message.parts.as_slice() {
                [ContentPart::AgentResults { items }] => items.clone(),
                _ => Vec::new(),
            })
            .collect()
    };
    eventually(|| async { (queued(&p).len() >= 2).then_some(()) }).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let items = queued(&p);
    assert_eq!(items.len(), 2, "{items:?}");
    let mut results: Vec<_> = items.iter().filter_map(|item| item.result.clone()).collect();
    results.sort();
    assert_eq!(results, vec!["early done".to_string(), "running done".to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_of_the_target_mid_follow_gives_a_result_not_a_hang() {
    let Some(mut p) = Pair::new().await else { return };
    let started = p.delegate(json!({"task": "Long job"})).await;
    let task_id = task_id_of(&started);
    p.agent("Long job").await.tx.send(DriverEvent::MessageCompleted {
        message_id: Uuid::now_v7().to_string(),
        origin: EventOrigin::Root,
        text: "half way".into(),
        thinking: None,
    }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    p.restart_b().await;
    // b's recovery marks the child interrupted (it was running when b died); a gets that, once.
    p.b.state.orchestrator.recover_after_restart().await.unwrap();
    p.end_parent_turn().await;
    eventually(|| async { Some(p.results()).filter(|items| !items.is_empty()) }).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let items = p.results();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!((items[0].task_id, items[0].status), (task_id, DelegationStatus::Interrupted));
    assert_eq!(items[0].result.as_deref(), Some("half way"));
    assert_eq!(p.children_on_b(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_requested_while_the_target_is_away_lands_when_it_reconnects() {
    let Some(p) = Pair::new().await else { return };
    let started = p.delegate(json!({"task": "Never-ending job"})).await;
    let task_id = task_id_of(&started);
    let agent = p.agent("Never-ending job").await;
    p.b_offline().await;

    let cancelled = p.tool("kybern_agent_cancel", json!({"task_id": task_id})).await.unwrap();
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["stop_pending"], true, "b has not confirmed yet");
    assert_eq!(p.child_on_b(task_id).unwrap().delegation.unwrap().status, DelegationStatus::Running);
    assert!(p.row(task_id).stop_requested);

    p.b_online().await;
    eventually(|| async { (!p.row(task_id).stop_requested).then_some(()) }).await;
    let child = p.child_on_b(task_id).unwrap();
    assert_eq!(child.delegation.unwrap().status, DelegationStatus::Cancelled);
    eventually(|| async { (agent.interrupts.load(Ordering::SeqCst) >= 1).then_some(()) }).await;
    // A late "completed" from b cannot undo it: the first terminal result won.
    assert_eq!(p.row(task_id).state, RemoteChildState::Cancelled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_before_the_task_was_sent_removes_it_from_the_outbox_for_good() {
    let Some(p) = Pair::new().await else { return };
    p.b_offline().await;
    let task_id = task_id_of(&p.delegate(json!({"task": "Never sent"})).await);
    p.tool("kybern_agent_cancel", json!({"task_id": task_id})).await.unwrap();
    assert_eq!(p.outbox()[0].state, OutboxState::Cancelled);
    assert!(!p.row(task_id).stop_requested, "b never had it, so there is nothing to tell it");
    p.b_online().await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(p.children_on_b(), 0);
    assert_eq!(p.row(task_id).state, RemoteChildState::Cancelled);
    // Ended here, by this machine's clock: clients must not compare it with the target's.
    assert!(p.row(task_id).info.completed_at.is_some());
    assert!(!p.row(task_id).completed_at_from_target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_the_target_makes_its_children_unreachable_with_a_reason() {
    let Some(p) = Pair::new().await else { return };
    // One child waiting in the outbox, one already running on b.
    let running = task_id_of(&p.delegate(json!({"task": "Running on b"})).await);
    p.b_offline().await;
    let waiting = task_id_of(&p.delegate(json!({"task": "Waiting for b"})).await);

    let removal = p.a.membership().remove(&p.b.id(), RemoveReason::User).unwrap();
    p.a.state.network.publish_ops(vec![removal]);
    p.a.state.network.drop_machine(&p.b.id());
    p.end_parent_turn().await;

    eventually(|| async { (p.row(waiting).state == RemoteChildState::Unreachable).then_some(()) }).await;
    eventually(|| async { (p.row(running).state == RemoteChildState::Unreachable).then_some(()) }).await;
    for task_id in [waiting, running] {
        let error = p.row(task_id).info.error.unwrap();
        assert!(error.contains("removed from your network"), "{error}");
    }
    let item = p.outbox().into_iter().find(|item| item.operation_id == p.row(waiting).operation_id).unwrap();
    assert_eq!(item.state, OutboxState::Failed);
    let items = eventually(|| async { Some(p.results()).filter(|items| items.len() >= 2) }).await;
    assert!(items.iter().all(|item| item.status == DelegationStatus::Failed), "{items:?}");
    // And a new delegation to it is refused up front.
    let error = p.a.state.orchestrator.remote_hub().unwrap().resolve(&p.b.id()).unwrap_err();
    assert!(error.contains("no machine"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_item_that_waited_seven_days_expires_and_retry_runs_it() {
    let Some(p) = Pair::new().await else { return };
    p.b_offline().await;
    let task_id = task_id_of(&p.delegate(json!({"task": "Patient task"})).await);
    assert_eq!(p.outbox()[0].expires_at - p.outbox()[0].created_at, chrono::Duration::days(7));

    // The seven days pass.
    let mut item = p.outbox().remove(0);
    item.expires_at = chrono::Utc::now() - chrono::Duration::seconds(1);
    p.a.state.store.outbox_update(&item).unwrap();
    p.end_parent_turn().await;
    p.a.state.orchestrator.remote_hub().unwrap().kick();
    eventually(|| async { (p.outbox()[0].state == OutboxState::Expired).then_some(()) }).await;
    eventually(|| async { (p.row(task_id).state == RemoteChildState::Unreachable).then_some(()) }).await;
    assert!(p.row(task_id).info.error.unwrap().contains("7 days"));
    assert_eq!(p.outbox()[0].state, OutboxState::Expired);
    let items = eventually(|| async { Some(p.results()).filter(|items| !items.is_empty()) }).await;
    assert_eq!(items[0].status, DelegationStatus::Failed, "the parent is told");

    // Retry queues it again with a fresh seven days; the child waits for its machine again.
    let retried = crate::network::outbox::retry(&p.a.state, OutboxIdParams { id: item.id }).unwrap();
    assert_eq!(retried.state, OutboxState::Queued);
    assert!(retried.expires_at > chrono::Utc::now() + chrono::Duration::days(6));
    assert_eq!(p.row(task_id).state, RemoteChildState::Pending);
    p.b_online().await;
    let child = eventually(|| async { p.child_on_b(task_id) }).await;
    assert_eq!(child.origin.unwrap().machine_id, p.a.id());
    eventually(|| async { (p.row(task_id).state == RemoteChildState::Running).then_some(()) }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_target_takes_the_origin_from_the_session_and_the_mode_can_only_narrow() {
    let Some(p) = Pair::new().await else { return };
    let repo_identity = crate::network::git_transfer::identity_of(&p.repo_a).await;
    let params = |op: Uuid, thread: Option<Uuid>, mode: PermissionMode, cap: PermissionMode, hops: Vec<String>| PeerDelegateParams {
        operation_id: op,
        task_id: Uuid::now_v7(),
        origin: Origin {
            machine_id: "forged".into(),
            machine_name: "forged".into(),
            thread_id: thread,
            operation_id: op,
            kind: OriginKind::ToolCall,
            permission_cap: cap,
            hops,
        },
        parent_title: "P".into(),
        task: "Do a thing".into(),
        title: None,
        role: DelegationRole::General,
        provider: Some(ProviderKind::ClaudeCode),
        model: None,
        effort: None,
        permission_mode: mode,
        workspace: DelegationWorkspace::Shared,
        owns: vec![],
        repo: repo_identity.clone(),
        project: None,
        base_commit: None,
        group: None,
    };
    let a_id = p.a.id();
    let dispatch = |params: PeerDelegateParams| {
        let orchestrator = p.b.state.orchestrator.clone();
        let from = a_id.clone();
        async move { orchestrator.peer_dispatch(&from, "peer.delegate", serde_json::to_value(params).unwrap()).await }
    };

    // The cap decides: a full-access request under a supervised parent runs supervised.
    let op = Uuid::now_v7();
    let result = dispatch(params(op, Some(p.parent.id), PermissionMode::FullAccess, PermissionMode::Supervised, vec![a_id.clone()])).await.unwrap();
    let child = p.b.state.store.thread_get(result["thread_id"].as_str().unwrap().parse().unwrap()).unwrap().unwrap();
    assert_eq!(child.permission_mode, PermissionMode::Supervised);
    // The payload's machine id is ignored: the session says who called.
    assert_eq!(child.origin.as_ref().unwrap().machine_id, a_id);
    // The same parent and operation is the same child; another parent with the same key is another child.
    let again = dispatch(params(op, Some(p.parent.id), PermissionMode::Supervised, PermissionMode::Supervised, vec![a_id.clone()])).await.unwrap();
    assert_eq!(again["thread_id"], result["thread_id"]);
    let other_parent = dispatch(params(op, Some(Uuid::now_v7()), PermissionMode::Supervised, PermissionMode::Supervised, vec![a_id.clone()])).await.unwrap();
    assert_ne!(other_parent["thread_id"], result["thread_id"]);
    // Work does not hop onwards.
    let error = dispatch(params(Uuid::now_v7(), None, PermissionMode::Supervised, PermissionMode::Supervised, vec![a_id.clone(), "third".into()]))
        .await
        .unwrap_err();
    assert_eq!(error.code, codes::FORBIDDEN, "{error:?}");
    // Another machine cannot read or stop the child.
    let task_id = child.delegation.as_ref().unwrap().task_id;
    for method in ["peer.delegation.get", "peer.delegation.cancel"] {
        let error = p.b.state.orchestrator.peer_dispatch("someone-else", method, json!({"task_id": task_id})).await.unwrap_err();
        assert_eq!(error.code, codes::NOT_FOUND, "{method}");
    }
    assert!(p.b.state.orchestrator.peer_dispatch(&a_id, "peer.delegation.get", json!({"task_id": task_id})).await.is_ok());
    // A child that delegates locally hands its origin on.
    assert_eq!(super::remote_delegation::narrower(PermissionMode::Auto, PermissionMode::AcceptEdits), PermissionMode::AcceptEdits);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_target_finds_its_checkout_by_repository_and_says_so_when_it_cannot() {
    let Some(p) = Pair::new().await else { return };
    // No checkout: a clear failure that comes back as the child's error, not a hang.
    let other = p.cluster.dir.path().join("other-repo");
    init_project_repo(&other);
    std::fs::write(other.join("tracked.txt"), "a different history\n").unwrap();
    run_git(&other, &["commit", "-q", "-a", "--amend", "-m", "elsewhere"]);
    let strange = crate::network::git_transfer::identity_of(&other).await;
    let a_id = p.a.id();
    let make = |repo: RepoIdentity, project: Option<String>| PeerDelegateParams {
        operation_id: Uuid::now_v7(),
        task_id: Uuid::now_v7(),
        origin: Origin {
            machine_id: a_id.clone(),
            machine_name: "a".into(),
            thread_id: Some(p.parent.id),
            operation_id: Uuid::now_v7(),
            kind: OriginKind::ToolCall,
            permission_cap: PermissionMode::Supervised,
            hops: vec![a_id.clone()],
        },
        parent_title: "P".into(),
        task: "x".into(),
        title: None,
        role: DelegationRole::General,
        provider: Some(ProviderKind::ClaudeCode),
        model: None,
        effort: None,
        permission_mode: PermissionMode::Supervised,
        workspace: DelegationWorkspace::Shared,
        owns: vec![],
        repo,
        project,
        base_commit: None,
        group: None,
    };
    let call = |params: PeerDelegateParams| p.b.state.orchestrator.peer_dispatch(&a_id, "peer.delegate", serde_json::to_value(params).unwrap());
    let error = call(make(strange, None)).await.unwrap_err();
    assert_eq!(error.code, codes::NOT_FOUND);
    assert!(error.message.contains("no checkout of this repository"), "{}", error.message);

    // Two checkouts of the same repository: b lists them and the hint picks one.
    let second = p.cluster.dir.path().join("checkout-b2");
    run_git(p.cluster.dir.path(), &["clone", "-q", "--config", "core.autocrlf=false", p.repo_b.to_str().unwrap(), second.to_str().unwrap()]);
    p.b.state.store.project_insert(&git_project("repo-b-again", &second)).unwrap();
    let repo = crate::network::git_transfer::identity_of(&p.repo_a).await;
    let error = call(make(repo.clone(), None)).await.unwrap_err();
    assert_eq!(error.code, codes::CONFLICT);
    assert!(error.message.contains("repo-b") && error.message.contains("repo-b-again") && error.message.contains("project"), "{}", error.message);
    let chosen = call(make(repo, Some("repo-b-again".into()))).await.unwrap();
    let child = p.b.state.store.thread_get(chosen["thread_id"].as_str().unwrap().parse().unwrap()).unwrap().unwrap();
    assert_eq!(child.cwd, second.to_string_lossy());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_queued_for_an_offline_machine_arrives_when_it_returns_exactly_once() {
    let Some(p) = Pair::new().await else { return };
    p.b_offline().await;
    let project = p.b.state.store.projects_list().unwrap().remove(0);
    let enqueue = |kind: OutboxKind, payload: serde_json::Value, operation_id: Option<Uuid>| {
        crate::network::outbox::enqueue(&p.a.state, OutboxEnqueueParams { target: p.b.id(), operation: OutboxOp { kind, payload, operation_id } })
    };
    let operation = Uuid::now_v7();
    let create = json!({
        "project_id": project.id,
        "provider": {"kind": "claude-code", "instance": "default"},
        "title": "Offline send",
        "message": UserMessage::text("Queued while you were away"),
    });
    let item = enqueue(OutboxKind::ThreadCreate, create.clone(), Some(operation)).unwrap();
    assert_eq!((item.state, item.kind), (OutboxState::Queued, OutboxKind::ThreadCreate));
    assert_eq!(enqueue(OutboxKind::ThreadCreate, create, Some(operation)).unwrap().id, item.id, "a repeated enqueue is the first item");
    assert!(item.payload["thread_id"].is_string() && item.payload["message_id"].is_string(), "the ids that make a repeat harmless are fixed up front");
    assert_eq!(p.outbox().len(), 1);
    let thread_id: Uuid = item.payload["thread_id"].as_str().unwrap().parse().unwrap();

    p.b_online().await;
    eventually(|| async { (p.outbox()[0].state == OutboxState::Delivered).then_some(()) }).await;
    let created = p.b.state.store.thread_get(thread_id).unwrap().expect("the thread exists on b");
    assert_eq!((created.title.as_str(), created.origin.is_none()), ("Offline send", true), "the user started it, so it has no origin");
    let agent = p.agent("Queued while you were away").await;
    // Delivered again (as after a crash between the answer and the bookkeeping), it is the same thread and message.
    let again = p.b.state.orchestrator.peer_dispatch(&p.a.id(), "peer.thread.create", item.payload.clone()).await.unwrap();
    assert_eq!(again["id"], json!(thread_id));
    assert_eq!(p.b.state.store.threads_list(None, true).unwrap().len(), 1);
    assert_eq!(p.spawned.lock().unwrap().len(), 1);

    // A follow-up for the same thread goes the same way.
    agent.finish("Done.").await;
    eventually(|| async { (p.b.state.store.thread_get(thread_id).unwrap().unwrap().status == ThreadStatus::Idle).then_some(()) }).await;
    p.b_offline().await;
    let send = enqueue(OutboxKind::ThreadSend, json!({"thread_id": thread_id, "message": UserMessage::text("And one more thing")}), None).unwrap();
    p.b_online().await;
    eventually(|| async { (p.a.state.store.outbox_get(send.id).unwrap().unwrap().state == OutboxState::Delivered).then_some(()) }).await;
    eventually(|| async { agent.transcript().await.contains("And one more thing").then_some(()) }).await;

    // What this version cannot deliver is refused up front rather than queued to fail.
    let error = enqueue(OutboxKind::FilesPush, json!({}), None).unwrap_err();
    assert_eq!(error.code, codes::NOT_AVAILABLE);
    let error = enqueue(OutboxKind::Delegate, json!({}), None).unwrap_err();
    assert_eq!(error.code, codes::INVALID_PARAMS);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_the_parent_stops_its_remote_children_too() {
    let Some(p) = Pair::new().await else { return };
    let running = task_id_of(&p.delegate(json!({"task": "Running on b"})).await);
    let agent = p.agent("Running on b").await;
    p.b_offline().await;
    let waiting = task_id_of(&p.delegate(json!({"task": "Queued for b"})).await);

    p.fixture.orchestrator.delegation_stop(p.parent.id, false).await.ok();
    assert_eq!(p.row(running).state, RemoteChildState::Cancelled);
    assert_eq!(p.row(waiting).state, RemoteChildState::Cancelled);
    assert!(p.row(running).stop_requested, "b has to be told");
    assert!(!p.row(waiting).stop_requested, "b never had the queued one");
    p.b_online().await;
    eventually(|| async { (!p.row(running).stop_requested).then_some(()) }).await;
    assert_eq!(p.child_on_b(running).unwrap().delegation.unwrap().status, DelegationStatus::Cancelled);
    eventually(|| async { (agent.interrupts.load(Ordering::SeqCst) >= 1).then_some(()) }).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(p.children_on_b(), 1, "the queued one never started");
}

// ADE-121: clients read a's own record of the child (`delegations.remote_list`) and hear every change of it
// (`delegations.remote_changed`), so a launch row or sidebar row settles even when the client's copy of b is stale.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clients_see_a_remote_childs_record_and_hear_it_settle() {
    let Some(p) = Pair::new().await else { return };
    let mut changes = p.a.state.orchestrator.subscribe_remote_children();
    let started = p.delegate(json!({"task": "Tidy the release notes", "title": "Release notes"})).await;
    let task_id = task_id_of(&started);

    let listed = p.a.state.orchestrator.remote_children_list(p.parent.id).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!((listed[0].task_id, listed[0].target.as_str()), (task_id, p.b.id().as_str()));
    assert!(listed[0].remote_thread_id.is_some(), "the target acknowledged before the call returned");
    assert!(p.a.state.orchestrator.remote_children_list(ThreadId::now_v7()).unwrap().is_empty(), "another thread has none");

    let first = tokio::time::timeout(Duration::from_secs(5), changes.recv()).await.expect("a change is announced").unwrap();
    assert_eq!(first.task_id, task_id);

    let agent = p.agent("Tidy the release notes").await;
    p.end_parent_turn().await;
    agent.finish("Release notes are tidy.").await;
    let settled = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let change = changes.recv().await.unwrap();
            if change.task_id == task_id && change.state == RemoteChildState::Completed {
                return change;
            }
        }
    })
    .await
    .expect("the completion is announced");
    assert!(settled.info.completed_at.is_some());
    // Followed to its end: the end time is the target's own, comparable with its copy of the thread.
    assert!(settled.completed_at_from_target, "the end time came from the target");
    let listed = p.a.state.orchestrator.remote_children_list(p.parent.id).unwrap();
    assert_eq!((listed[0].state, listed[0].completed_at_from_target), (RemoteChildState::Completed, true));
}
