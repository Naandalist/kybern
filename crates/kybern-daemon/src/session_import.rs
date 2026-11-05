//! First run: find the user's past chats in every installed harness, group them
//! by repository, and import the ones they pick.
//!
//! Both jobs run on the daemon, so they continue when the client window closes.
//! A scan lists sessions of every harness account concurrently and pushes the
//! whole picture as `sessions.scan.changed` whenever it grows; a harness that
//! fails or times out is an error on its own row, never a failed scan. An import
//! job reads each selected session through one reader per harness account
//! (`kybern_drivers::sessions::SessionReader`: one Codex app-server, one
//! `opencode serve`, one Cursor sidecar for the whole job, not one process per
//! session) and adopts it as a thread of the repository's project. It creates no
//! turns and sends no prompt; a session it cannot read is skipped with a reason.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use kybern_drivers::sessions::{ReadFailure, SessionReader, classify_read_error};
use kybern_drivers::{DriverError, ProbeContext};
use kybern_protocol::methods::*;
use kybern_protocol::{Project, ProjectId, ProviderKind, ThreadId};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::state::AppState;

/// Finished scans and jobs stay readable this long.
const RETAIN: Duration = Duration::from_secs(30 * 60);
/// Longest one harness account may take to list its sessions.
const TARGET_TIMEOUT: Duration = Duration::from_secs(180);
/// Longest one session may take to read.
const READ_TIMEOUT: Duration = Duration::from_secs(120);
/// A file-based session written this recently is still open in its harness.
const LIVE_WINDOW_SECS: i64 = 60;
const RECENT_DAYS: i64 = 30;
const SUGGEST_MIN_RECENT: u32 = 3;
const MAX_PAGES: usize = 2000;
const SKIP_LIST_CAP: usize = 200;
/// Least time between two notifications about the same scan or project.
const EMIT_INTERVAL: Duration = Duration::from_millis(250);

type Key = (ProviderKind, String, String);

/// What the connection layer forwards to clients.
#[derive(Debug, Clone)]
pub enum SessionsEvent {
    Scan(SessionsScanChanged),
    Progress(SessionsImportProgress),
    Finished(SessionsImportFinished),
}

pub struct SessionImports {
    shared: Arc<Shared>,
}

impl Default for SessionImports {
    fn default() -> Self {
        Self {
            shared: Arc::new(Shared {
                scans: Mutex::new(HashMap::new()),
                jobs: Mutex::new(HashMap::new()),
                events: broadcast::channel(256).0,
                running: AtomicUsize::new(0),
            }),
        }
    }
}

struct Shared {
    scans: Mutex<HashMap<String, ScanEntry>>,
    jobs: Mutex<HashMap<String, JobEntry>>,
    events: broadcast::Sender<SessionsEvent>,
    /// Scans and jobs still working; the idle-exit sweep treats them as activity.
    running: AtomicUsize,
}

struct ScanEntry {
    run: Arc<ScanRun>,
    cancel: CancellationToken,
    ended: Option<Instant>,
}

struct JobEntry {
    job: SessionsImportJob,
    cancel: CancellationToken,
    ended: Option<Instant>,
}

/// Holds the `running` count for as long as a task lives.
struct Running(Arc<Shared>);

impl Running {
    fn new(shared: &Arc<Shared>) -> Self {
        shared.running.fetch_add(1, Ordering::SeqCst);
        Self(shared.clone())
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.0.running.fetch_sub(1, Ordering::SeqCst);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl SessionImports {
    pub fn subscribe(&self) -> broadcast::Receiver<SessionsEvent> {
        self.shared.events.subscribe()
    }

    /// Scans and import jobs that have not finished.
    pub fn running(&self) -> usize {
        self.shared.running.load(Ordering::SeqCst)
    }

    fn prune(&self) {
        let alive = |ended: &Option<Instant>| ended.is_none_or(|at| at.elapsed() < RETAIN);
        lock(&self.shared.scans).retain(|_, scan| alive(&scan.ended));
        lock(&self.shared.jobs).retain(|_, job| alive(&job.ended));
    }

    // ---- scan ----

    pub fn start_scan(&self, state: &AppState, params: SessionsScanProjectsParams) -> SessionsScan {
        self.prune();
        let scan_id = uuid::Uuid::now_v7().to_string();
        let targets = targets(state, params.providers.as_deref());
        let run = Arc::new(ScanRun::new(state, Some((scan_id.clone(), self.shared.events.clone())), &targets));
        let cancel = CancellationToken::new();
        lock(&self.shared.scans).insert(scan_id.clone(), ScanEntry { run: run.clone(), cancel: cancel.clone(), ended: None });
        let snapshot = run.snapshot();
        let (state, shared) = (state.clone(), self.shared.clone());
        // Counted before the task is scheduled, so the daemon never looks idle between start and first poll.
        let running = Running::new(&shared);
        tokio::spawn(async move {
            let _running = running;
            run.execute(&state, targets, &cancel).await;
            if let Some(entry) = lock(&shared.scans).get_mut(&scan_id) {
                entry.ended = Some(Instant::now());
            }
        });
        snapshot
    }

    pub fn scan_get(&self, scan_id: &str) -> Option<SessionsScan> {
        lock(&self.shared.scans).get(scan_id).map(|entry| entry.run.snapshot())
    }

    pub fn scan_cancel(&self, scan_id: &str) -> Option<SessionsScan> {
        let entry = lock(&self.shared.scans);
        let entry = entry.get(scan_id)?;
        entry.cancel.cancel();
        Some(entry.run.snapshot())
    }

    // ---- import ----

    pub fn start_import(&self, state: &AppState, params: SessionsImportAllParams) -> Result<SessionsImportJob> {
        if params.roots.is_empty() {
            return Err(anyhow!("Choose at least one project to import."));
        }
        if params.roots.len() > 500 {
            return Err(anyhow!("Choose at most 500 projects at a time."));
        }
        self.prune();
        let job_id = uuid::Uuid::now_v7().to_string();
        let projects = params
            .roots
            .iter()
            .map(|root| SessionsImportProject {
                root: root.root.clone(),
                state: ImportProjectState::Queued,
                project_id: root.project_id,
                total: 0,
                done: 0,
                imported: 0,
                already_imported: 0,
                skipped_count: 0,
                skipped: Vec::new(),
                imported_thread_ids: Vec::new(),
                error: None,
            })
            .collect();
        let job = SessionsImportJob {
            job_id: job_id.clone(),
            state: SessionsJobState::Running,
            started_at: Utc::now(),
            finished_at: None,
            projects,
            imported: 0,
            already_imported: 0,
            skipped_count: 0,
        };
        let cancel = CancellationToken::new();
        lock(&self.shared.jobs).insert(job_id.clone(), JobEntry { job: job.clone(), cancel: cancel.clone(), ended: None });
        let cached =
            params.scan_id.as_deref().and_then(|id| lock(&self.shared.scans).get(id).and_then(|scan| scan.run.finished_sessions()));
        let (state, shared) = (state.clone(), self.shared.clone());
        let running = Running::new(&shared);
        tokio::spawn(async move {
            let _running = running;
            let mut runner = JobRunner { state, shared: shared.clone(), job_id: job_id.clone(), cancel, pending: HashMap::new() };
            runner.run(params.roots, cached).await;
            if let Some(entry) = lock(&shared.jobs).get_mut(&job_id) {
                entry.ended = Some(Instant::now());
            }
        });
        Ok(job)
    }

    pub fn import_get(&self, job_id: &str) -> Option<SessionsImportJob> {
        lock(&self.shared.jobs).get(job_id).map(|entry| entry.job.clone())
    }

    pub fn import_cancel(&self, job_id: &str) -> Option<SessionsImportJob> {
        let jobs = lock(&self.shared.jobs);
        let entry = jobs.get(job_id)?;
        entry.cancel.cancel();
        Some(entry.job.clone())
    }
}

// ---- harness accounts to read ----

/// One harness account whose saved sessions are read.
#[derive(Clone)]
struct Target {
    kind: ProviderKind,
    instance: String,
    context: ProbeContext,
    /// OMP profile the sessions were found under, recorded on imported threads.
    profile: Option<String>,
}

enum TargetSlot {
    Ready(Target),
    Broken { kind: ProviderKind, instance: String, error: String },
}

/// Every harness account worth reading: the CLI account of each harness plus each
/// named Kybern account. Project-specific overrides do not apply to a scan.
fn targets(state: &AppState, only: Option<&[ProviderKind]>) -> Vec<TargetSlot> {
    let settings = state.settings.get();
    let mut out = Vec::new();
    for kind in ProviderKind::ALL {
        if only.is_some_and(|only| !only.contains(&kind)) || state.drivers.get(kind).is_none() {
            continue;
        }
        let provider = settings.providers.get(&kind).cloned().unwrap_or_default();
        let instances = std::iter::once("default".to_string()).chain(provider.accounts.keys().cloned());
        for instance in instances {
            let slot = match crate::provider_accounts::environment(&provider, kind, &instance) {
                Ok(env) => {
                    let profile = if kind == ProviderKind::Omp { kybern_drivers::omp_profile::resolve(&env).ok() } else { None };
                    TargetSlot::Ready(Target {
                        kind,
                        instance,
                        context: ProbeContext { binary: provider.binary.clone().map(PathBuf::from), cwd: None, env },
                        profile,
                    })
                }
                Err(error) => TargetSlot::Broken { kind, instance, error: error.to_string() },
            };
            out.push(slot);
        }
    }
    out
}

fn describe(error: &DriverError) -> String {
    match error {
        DriverError::BinaryNotFound(name) => format!("{name} is not installed on this computer."),
        DriverError::VersionTooOld { found, required } => {
            format!("Version {found} cannot read saved sessions. Update it to {required} or newer.")
        }
        DriverError::ProcessExited(message) | DriverError::Protocol(message) | DriverError::Unsupported(message) => message.clone(),
        other => other.to_string(),
    }
}

// ---- repository roots ----

#[derive(Clone)]
struct RootInfo {
    root: String,
    is_git: bool,
    git_remote: Option<String>,
    folder_missing: bool,
}

/// Maps a session's folder to its repository root. Pure file-system probing, no
/// processes: it walks up looking for `.git`, follows a linked worktree to its
/// main checkout, and never treats the home folder or `/` as the repository of a
/// folder inside it (a dotfiles repo would swallow everything).
struct RootResolver {
    home: Option<PathBuf>,
    cache: HashMap<String, Option<RootInfo>>,
    repos: HashMap<PathBuf, RootInfo>,
}

impl RootResolver {
    fn new(home: Option<PathBuf>) -> Self {
        let home = home.map(|home| home.canonicalize().unwrap_or(home));
        Self { home, cache: HashMap::new(), repos: HashMap::new() }
    }

    fn from_env() -> Self {
        Self::new(std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from))
    }

    fn resolve(&mut self, cwd: &str) -> Option<RootInfo> {
        if let Some(known) = self.cache.get(cwd) {
            return known.clone();
        }
        let info = self.probe(cwd);
        self.cache.insert(cwd.to_string(), info.clone());
        info
    }

    fn probe(&mut self, cwd: &str) -> Option<RootInfo> {
        let path = Path::new(cwd);
        if cwd.is_empty() || !path.is_absolute() {
            return None;
        }
        let exists = path.is_dir();
        let start = path.ancestors().find(|ancestor| ancestor.is_dir());
        if let Some(start) = start.map(|start| start.canonicalize().unwrap_or_else(|_| start.to_path_buf())) {
            let own = exists.then_some(start.as_path());
            for dir in start.ancestors() {
                let boundary = self.home.as_deref() == Some(dir) || dir.parent().is_none();
                if boundary && own != Some(dir) {
                    break;
                }
                if dir.join(".git").exists() {
                    return Some(self.repo(dir));
                }
            }
        }
        let root = if exists { path.canonicalize().unwrap_or_else(|_| path.to_path_buf()) } else { path.to_path_buf() };
        Some(RootInfo {
            root: kybern_git::simplify_path(&root).to_string_lossy().into_owned(),
            is_git: false,
            git_remote: None,
            folder_missing: !exists,
        })
    }

    fn repo(&mut self, dir: &Path) -> RootInfo {
        let main = main_checkout(dir).unwrap_or_else(|| dir.to_path_buf());
        let main = main.canonicalize().unwrap_or(main);
        self.repos
            .entry(main.clone())
            .or_insert_with(|| RootInfo {
                root: kybern_git::simplify_path(&main).to_string_lossy().into_owned(),
                is_git: true,
                git_remote: origin_url(&main),
                folder_missing: false,
            })
            .clone()
    }
}

/// The main checkout of a linked git worktree (`.git` is a file naming
/// `<main>/.git/worktrees/<name>`); `None` for an ordinary repository or a submodule.
fn main_checkout(dir: &Path) -> Option<PathBuf> {
    let git = dir.join(".git");
    if !git.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&git).ok()?;
    let target = Path::new(text.lines().find_map(|line| line.strip_prefix("gitdir:"))?.trim());
    let target = if target.is_absolute() { target.to_path_buf() } else { dir.join(target) };
    let worktrees = target.parent()?;
    if worktrees.file_name()? != "worktrees" {
        return None;
    }
    let git_dir = worktrees.parent()?;
    let main = git_dir.parent()?;
    (git_dir.file_name()? == ".git" && git_dir.is_dir()).then(|| main.to_path_buf())
}

/// `origin`'s URL from `<root>/.git/config`, without running git.
fn origin_url(root: &Path) -> Option<String> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(root.join(".git/config")).ok()?.take(256 * 1024).read_to_string(&mut text).ok()?;
    let mut in_origin = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_origin = line.replace(' ', "") == "[remote\"origin\"]";
        } else if in_origin && let Some(rest) = line.strip_prefix("url") {
            let url = rest.trim_start().strip_prefix('=')?.trim();
            return (!url.is_empty()).then(|| url.to_string());
        }
    }
    None
}

// ---- scan ----

/// A listed session, with the account it was found in and the repository root it belongs to.
#[derive(Clone)]
struct Found {
    session: kybern_protocol::methods::SavedSession,
    instance: String,
    root: String,
}

struct Group {
    info: RootInfo,
    project_id: Option<ProjectId>,
    counts: BTreeMap<ProviderKind, (u32, u32)>,
    sessions: u32,
    imported: u32,
    recent: u32,
    last_activity: DateTime<Utc>,
}

struct ScanState {
    scan_id: String,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    state: SessionsJobState,
    providers: Vec<ScanProviderStatus>,
    groups: HashMap<String, Group>,
    resolver: RootResolver,
    imported: HashSet<Key>,
    project_ids: HashMap<String, ProjectId>,
    seen: HashSet<Key>,
    found: Vec<Found>,
    total: u32,
    without_folder: u32,
    last_emit: Option<Instant>,
    done: bool,
}

/// One scan's shared state. Targets run concurrently and add pages as they arrive.
struct ScanRun {
    state: Mutex<ScanState>,
    events: Option<broadcast::Sender<SessionsEvent>>,
}

impl ScanRun {
    fn new(state: &AppState, events: Option<(String, broadcast::Sender<SessionsEvent>)>, targets: &[TargetSlot]) -> Self {
        let (scan_id, events) = match events {
            Some((id, events)) => (id, Some(events)),
            None => (String::new(), None),
        };
        let imported = state
            .store
            .threads_list(None, true)
            .map(|threads| {
                threads
                    .into_iter()
                    .filter_map(|thread| Some((thread.provider.kind, thread.provider.instance, thread.provider_session_id?)))
                    .collect()
            })
            .unwrap_or_default();
        let project_ids = state
            .store
            .projects_list()
            .map(|projects| {
                projects
                    .into_iter()
                    .map(|project| (kybern_git::simplify_path(Path::new(&project.path)).to_string_lossy().into_owned(), project.id))
                    .collect()
            })
            .unwrap_or_default();
        let providers = targets
            .iter()
            .map(|slot| match slot {
                TargetSlot::Ready(target) => ScanProviderStatus {
                    provider: target.kind,
                    instance: target.instance.clone(),
                    state: ScanProviderState::Pending,
                    sessions: 0,
                    error: None,
                },
                TargetSlot::Broken { kind, instance, error } => ScanProviderStatus {
                    provider: *kind,
                    instance: instance.clone(),
                    state: ScanProviderState::Failed,
                    sessions: 0,
                    error: Some(error.clone()),
                },
            })
            .collect();
        Self {
            state: Mutex::new(ScanState {
                scan_id,
                started_at: Utc::now(),
                finished_at: None,
                state: SessionsJobState::Running,
                providers,
                groups: HashMap::new(),
                resolver: RootResolver::from_env(),
                imported,
                project_ids,
                seen: HashSet::new(),
                found: Vec::new(),
                total: 0,
                without_folder: 0,
                last_emit: None,
                done: false,
            }),
            events,
        }
    }

    fn snapshot(&self) -> SessionsScan {
        lock(&self.state).snapshot()
    }

    /// The listed sessions, once the scan ended without being canceled.
    fn finished_sessions(&self) -> Option<Arc<Vec<Found>>> {
        let state = lock(&self.state);
        (state.done && state.state == SessionsJobState::Done).then(|| Arc::new(state.found.clone()))
    }

    fn emit(&self, state: &mut ScanState, force: bool) {
        let Some(events) = &self.events else { return };
        if !force && state.last_emit.is_some_and(|at| at.elapsed() < EMIT_INTERVAL) {
            return;
        }
        state.last_emit = Some(Instant::now());
        // No receiver just means no client is connected.
        let _ = events.send(SessionsEvent::Scan(SessionsScanChanged { scan: state.snapshot() }));
    }

    fn set_provider(&self, index: usize, change: impl FnOnce(&mut ScanProviderStatus)) {
        let mut state = lock(&self.state);
        change(&mut state.providers[index]);
        self.emit(&mut state, true);
    }

    fn add_page(&self, index: usize, sessions: Vec<kybern_protocol::methods::SavedSession>) {
        let mut state = lock(&self.state);
        let instance = state.providers[index].instance.clone();
        let added = sessions.into_iter().map(|session| state.add(session, &instance)).filter(|added| *added).count();
        state.providers[index].sessions += added as u32;
        self.emit(&mut state, false);
    }

    async fn execute(&self, state: &AppState, targets: Vec<TargetSlot>, cancel: &CancellationToken) {
        let work = targets.into_iter().enumerate().filter_map(|(index, slot)| match slot {
            TargetSlot::Ready(target) => Some(self.scan_target(state, index, target, cancel)),
            TargetSlot::Broken { .. } => None,
        });
        futures::future::join_all(work).await;
        let mut scan = lock(&self.state);
        scan.state = if cancel.is_cancelled() { SessionsJobState::Canceled } else { SessionsJobState::Done };
        scan.finished_at = Some(Utc::now());
        scan.done = true;
        self.emit(&mut scan, true);
    }

    async fn scan_target(&self, state: &AppState, index: usize, target: Target, cancel: &CancellationToken) {
        self.set_provider(index, |status| status.state = ScanProviderState::Scanning);
        let result = tokio::time::timeout(TARGET_TIMEOUT, self.list_target(state, index, &target, cancel)).await;
        let (outcome, error) = match result {
            Ok(Ok(())) => (ScanProviderState::Done, None),
            Ok(Err(DriverError::BinaryNotFound(name))) => {
                (ScanProviderState::Unavailable, Some(format!("{name} is not installed on this computer.")))
            }
            Ok(Err(error)) => (ScanProviderState::Failed, Some(describe(&error))),
            Err(_) => (
                ScanProviderState::Failed,
                Some(format!(
                    "{} took longer than {} seconds to list its sessions. Open it once to check it works, then scan again.",
                    target.kind.display_name(),
                    TARGET_TIMEOUT.as_secs()
                )),
            ),
        };
        self.set_provider(index, |status| {
            status.state = outcome;
            status.error = error;
        });
    }

    async fn list_target(&self, state: &AppState, index: usize, target: &Target, cancel: &CancellationToken) -> kybern_drivers::Result<()> {
        // Codex and OpenCode run a helper binary; without it there is nothing to ask.
        if matches!(target.kind, ProviderKind::Codex | ProviderKind::Opencode) {
            kybern_drivers::binary::resolve(target.kind, target.context.binary.as_ref())?;
        }
        let driver = state.drivers.get(target.kind).ok_or_else(|| DriverError::Unsupported("This harness is unavailable.".into()))?;
        let mut reader = driver.open_session_reader(&target.context).await?;
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::new();
        let mut outcome = Ok(());
        for _ in 0..MAX_PAGES {
            if cancel.is_cancelled() {
                break;
            }
            match reader.list_page(cursor.as_deref()).await {
                Ok(page) => {
                    self.add_page(index, page.sessions);
                    cursor = page.next_cursor;
                    if cursor.as_ref().is_none_or(|next| !seen.insert(next.clone())) {
                        break;
                    }
                }
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
            }
        }
        reader.close().await;
        outcome
    }
}

impl ScanState {
    /// Add one listed session. False when it was already counted or has no usable folder.
    fn add(&mut self, session: kybern_protocol::methods::SavedSession, instance: &str) -> bool {
        let key = (session.provider, instance.to_string(), session.id.clone());
        if !self.seen.insert(key.clone()) {
            return false;
        }
        self.total += 1;
        let Some(info) = self.resolver.resolve(&session.cwd) else {
            self.without_folder += 1;
            return true;
        };
        let imported = self.imported.contains(&key);
        let project_id = self.project_ids.get(&info.root).copied();
        let recent = session.updated_at > Utc::now() - chrono::Duration::days(RECENT_DAYS);
        let group = self.groups.entry(info.root.clone()).or_insert_with(|| Group {
            info: info.clone(),
            project_id,
            counts: BTreeMap::new(),
            sessions: 0,
            imported: 0,
            recent: 0,
            last_activity: session.updated_at,
        });
        let count = group.counts.entry(session.provider).or_default();
        count.0 += 1;
        count.1 += u32::from(imported);
        group.sessions += 1;
        group.imported += u32::from(imported);
        group.recent += u32::from(recent);
        group.last_activity = group.last_activity.max(session.updated_at);
        self.found.push(Found { session, instance: instance.to_string(), root: info.root });
        true
    }

    fn snapshot(&self) -> SessionsScan {
        let mut projects: Vec<ScannedProject> = self
            .groups
            .values()
            .map(|group| ScannedProject {
                root: group.info.root.clone(),
                name: Path::new(&group.info.root)
                    .file_name()
                    .map_or_else(|| group.info.root.clone(), |name| name.to_string_lossy().into_owned()),
                is_git: group.info.is_git,
                git_remote: group.info.git_remote.clone(),
                project_id: group.project_id,
                providers: group
                    .counts
                    .iter()
                    .map(|(provider, (sessions, imported))| ScannedProviderCount {
                        provider: *provider,
                        sessions: *sessions,
                        imported: *imported,
                    })
                    .collect(),
                session_count: group.sessions,
                imported_count: group.imported,
                recent_count: group.recent,
                last_activity: group.last_activity,
                folder_missing: group.info.folder_missing,
                suggested: group.info.is_git && group.recent >= SUGGEST_MIN_RECENT,
            })
            .collect();
        projects.sort_by(|a, b| b.last_activity.cmp(&a.last_activity).then_with(|| a.root.cmp(&b.root)));
        SessionsScan {
            scan_id: self.scan_id.clone(),
            state: self.state,
            started_at: self.started_at,
            finished_at: self.finished_at,
            providers: self.providers.clone(),
            projects,
            sessions_found: self.total,
            sessions_without_folder: self.without_folder,
        }
    }
}

// ---- import ----

/// One reader per harness account, opened on first use and kept for the job.
enum Slot {
    Ready { reader: Box<dyn SessionReader>, _gate: Option<tokio::sync::OwnedRwLockReadGuard<()>> },
    Failed(String),
}

#[derive(Default)]
struct Pending {
    thread_ids: Vec<ThreadId>,
    skipped: Vec<SessionSkip>,
    last_emit: Option<Instant>,
}

struct JobRunner {
    state: AppState,
    shared: Arc<Shared>,
    job_id: String,
    cancel: CancellationToken,
    /// Changes not yet announced, by project root.
    pending: HashMap<String, Pending>,
}

enum Outcome {
    Imported(ThreadId),
    Already,
    Skipped(SessionSkip),
}

impl JobRunner {
    fn update<R>(&self, change: impl FnOnce(&mut SessionsImportJob) -> R) -> Option<R> {
        lock(&self.shared.jobs).get_mut(&self.job_id).map(|entry| change(&mut entry.job))
    }

    /// Tell clients where project `index` stands, with what changed since the last time.
    fn announce(&mut self, index: usize, force: bool) {
        let Some(project) = self.update(|job| job.projects[index].clone()) else { return };
        let pending = self.pending.entry(project.root.clone()).or_default();
        if !force && pending.last_emit.is_some_and(|at| at.elapsed() < EMIT_INTERVAL) {
            return;
        }
        pending.last_emit = Some(Instant::now());
        let progress = SessionsImportProgress {
            job_id: self.job_id.clone(),
            root: project.root,
            state: project.state,
            project_id: project.project_id,
            total: project.total,
            done: project.done,
            imported: project.imported,
            already_imported: project.already_imported,
            skipped_count: project.skipped_count,
            new_thread_ids: std::mem::take(&mut pending.thread_ids),
            new_skipped: std::mem::take(&mut pending.skipped),
            error: project.error,
        };
        let _ = self.shared.events.send(SessionsEvent::Progress(progress));
    }

    fn record(&mut self, index: usize, outcome: Outcome) {
        let root = self.update(|job| job.projects[index].root.clone()).unwrap_or_default();
        let pending = self.pending.entry(root).or_default();
        match &outcome {
            Outcome::Imported(id) => pending.thread_ids.push(*id),
            Outcome::Skipped(skip) => pending.skipped.push(skip.clone()),
            Outcome::Already => {}
        }
        self.update(|job| {
            let project = &mut job.projects[index];
            project.done += 1;
            match outcome {
                Outcome::Imported(id) => {
                    project.imported += 1;
                    project.imported_thread_ids.push(id);
                    job.imported += 1;
                }
                Outcome::Already => {
                    project.already_imported += 1;
                    job.already_imported += 1;
                }
                Outcome::Skipped(skip) => {
                    project.skipped_count += 1;
                    if project.skipped.len() < SKIP_LIST_CAP {
                        project.skipped.push(skip);
                    }
                    job.skipped_count += 1;
                }
            }
        });
        self.announce(index, false);
    }

    async fn run(&mut self, roots: Vec<SessionsImportRoot>, cached: Option<Arc<Vec<Found>>>) {
        let mut resolver = RootResolver::from_env();
        let found = match cached {
            Some(found) => found,
            None => Arc::new(self.list(&roots).await),
        };
        let settings = self.state.settings.get();
        let mut existing: HashSet<Key> = self
            .state
            .store
            .threads_list(None, true)
            .map(|threads| {
                threads
                    .into_iter()
                    .filter_map(|thread| Some((thread.provider.kind, thread.provider.instance, thread.provider_session_id?)))
                    .collect()
            })
            .unwrap_or_default();
        let targets: HashMap<(ProviderKind, String), Target> = targets(&self.state, None)
            .into_iter()
            .filter_map(|slot| match slot {
                TargetSlot::Ready(target) => Some(((target.kind, target.instance.clone()), target)),
                TargetSlot::Broken { .. } => None,
            })
            .collect();
        let mut pool: HashMap<(ProviderKind, String), Slot> = HashMap::new();

        for (index, entry) in roots.iter().enumerate() {
            if self.cancel.is_cancelled() {
                break;
            }
            let root = resolver.resolve(&entry.root).map_or_else(|| entry.root.clone(), |info| info.root);
            let mut sessions: Vec<&Found> = found
                .iter()
                .filter(|item| item.root == root && entry.providers.as_ref().is_none_or(|only| only.contains(&item.session.provider)))
                .collect();
            sessions.sort_by_key(|item| std::cmp::Reverse(item.session.updated_at));
            if let Some(limit) = entry.limit {
                sessions.truncate(limit as usize);
            }
            self.update(|job| {
                let project = &mut job.projects[index];
                project.state = ImportProjectState::Running;
                project.total = sessions.len() as u32;
            });
            self.announce(index, true);

            let project = match self.project_for(entry, &root) {
                Ok(project) => project,
                Err(error) => {
                    // Without a project nothing can be imported; say so once per session.
                    self.update(|job| {
                        let project = &mut job.projects[index];
                        project.state = ImportProjectState::Failed;
                        project.error = Some(error.to_string());
                    });
                    for item in &sessions {
                        let skip = skip_of(item, SessionSkipReason::FolderMissing, error.to_string());
                        self.record(index, Outcome::Skipped(skip));
                    }
                    self.announce(index, true);
                    continue;
                }
            };
            self.update(|job| job.projects[index].project_id = Some(project.id));

            for item in sessions {
                if self.cancel.is_cancelled() {
                    break;
                }
                let outcome = self.import_one(&mut pool, &targets, &project, item, &mut existing, &settings).await;
                self.record(index, outcome);
            }
            self.update(|job| job.projects[index].state = ImportProjectState::Done);
            self.announce(index, true);
        }

        for (_, slot) in pool {
            if let Slot::Ready { reader, .. } = slot {
                reader.close().await;
            }
        }
        let canceled = self.cancel.is_cancelled();
        let summary = self.update(|job| {
            job.state = if canceled { SessionsJobState::Canceled } else { SessionsJobState::Done };
            job.finished_at = Some(Utc::now());
            for project in &mut job.projects {
                if project.state == ImportProjectState::Queued || project.state == ImportProjectState::Running {
                    project.state = ImportProjectState::Done;
                }
            }
            job.clone()
        });
        if let Some(job) = summary {
            let _ = self.shared.events.send(SessionsEvent::Finished(SessionsImportFinished { job }));
        }
    }

    /// List every session of the harnesses the roots ask for.
    async fn list(&self, roots: &[SessionsImportRoot]) -> Vec<Found> {
        let only: Option<Vec<ProviderKind>> = if roots.iter().any(|root| root.providers.is_none()) {
            None
        } else {
            let mut kinds: Vec<ProviderKind> = roots.iter().flat_map(|root| root.providers.iter().flatten().copied()).collect();
            kinds.sort();
            kinds.dedup();
            Some(kinds)
        };
        let targets = targets(&self.state, only.as_deref());
        let run = ScanRun::new(&self.state, None, &targets);
        run.execute(&self.state, targets, &self.cancel).await;
        lock(&run.state).found.clone()
    }

    /// The project the root's sessions join: the one the caller named, else the
    /// one at `root`, created when it does not exist yet.
    fn project_for(&self, entry: &SessionsImportRoot, root: &str) -> Result<Project> {
        if let Some(id) = entry.project_id {
            return self.state.store.project_get(id)?.ok_or_else(|| anyhow!("That project no longer exists. Choose it again."));
        }
        if !Path::new(root).is_dir() {
            return Err(anyhow!("The folder {root} no longer exists. Restore it or skip this project."));
        }
        self.state.orchestrator.add_project(root.to_string(), None, None)
    }

    async fn import_one(
        &self,
        pool: &mut HashMap<(ProviderKind, String), Slot>,
        targets: &HashMap<(ProviderKind, String), Target>,
        project: &Project,
        item: &Found,
        existing: &mut HashSet<Key>,
        settings: &kybern_protocol::Settings,
    ) -> Outcome {
        let session = &item.session;
        let key = (session.provider, item.instance.clone(), session.id.clone());
        if existing.contains(&key) {
            return Outcome::Already;
        }
        // Grok Build lists no saved sessions (`sessions::list` is empty), so none reaches this check.
        let file_based =
            matches!(session.provider, ProviderKind::ClaudeCode | ProviderKind::Pi | ProviderKind::Omp | ProviderKind::Antigravity);
        if file_based && Utc::now().signed_duration_since(session.updated_at).num_seconds() < LIVE_WINDOW_SECS {
            return Outcome::Skipped(skip_of(
                item,
                SessionSkipReason::InUse,
                format!("{} is still using this session. Close it there, then import again.", session.provider.display_name()),
            ));
        }
        let cwd = Path::new(&session.cwd);
        if !cwd.is_absolute() || !cwd.is_dir() {
            return Outcome::Skipped(skip_of(
                item,
                SessionSkipReason::FolderMissing,
                format!("The session's folder {} no longer exists. Restore it to import this session.", session.cwd),
            ));
        }
        let pool_key = (session.provider, item.instance.clone());
        if !pool.contains_key(&pool_key) {
            let slot = self.open(targets.get(&pool_key)).await;
            pool.insert(pool_key.clone(), slot);
        }
        let reader = match pool.get_mut(&pool_key) {
            Some(Slot::Ready { reader, .. }) => reader,
            Some(Slot::Failed(error)) => return Outcome::Skipped(skip_of(item, SessionSkipReason::HarnessUnavailable, error.clone())),
            None => unreachable!("slot was just opened"),
        };
        let history = match tokio::time::timeout(READ_TIMEOUT, reader.read(&session.id)).await {
            Ok(Ok(history)) => history,
            Ok(Err(error)) => {
                let reason = match classify_read_error(&error) {
                    ReadFailure::InUse => SessionSkipReason::InUse,
                    ReadFailure::TooLarge => SessionSkipReason::TooLarge,
                    ReadFailure::Other => SessionSkipReason::ReadFailed,
                };
                // A helper that died is restarted for the next session.
                if matches!(error, DriverError::ProcessExited(_) | DriverError::Io(_)) {
                    pool.remove(&pool_key);
                }
                return Outcome::Skipped(skip_of(item, reason, describe(&error)));
            }
            Err(_) => {
                pool.remove(&pool_key);
                return Outcome::Skipped(skip_of(
                    item,
                    SessionSkipReason::ReadFailed,
                    format!(
                        "Reading this session took longer than {} seconds. Open it in its harness to check it, then import again.",
                        READ_TIMEOUT.as_secs()
                    ),
                ));
            }
        };
        if history.session.id != session.id || history.session.provider != session.provider {
            return Outcome::Skipped(skip_of(
                item,
                SessionSkipReason::ReadFailed,
                "The harness returned a different session. Scan again and retry.".into(),
            ));
        }
        let updated_at = history.session.updated_at;
        let profile = targets.get(&pool_key).and_then(|target| target.profile.clone());
        let adopt = crate::orchestrator::AdoptSession {
            project: project.clone(),
            instance: item.instance.clone(),
            history,
            imported_profile: profile,
            updated_at,
            announce_history: false,
        };
        match self.state.orchestrator.adopt_session_history(adopt, settings).await {
            Ok((thread, true)) => {
                existing.insert(key);
                Outcome::Imported(thread.id)
            }
            Ok((_, false)) => {
                existing.insert(key);
                Outcome::Already
            }
            Err(error) => Outcome::Skipped(skip_of(item, SessionSkipReason::ReadFailed, format!("{error}"))),
        }
    }

    async fn open(&self, target: Option<&Target>) -> Slot {
        let Some(target) = target else {
            return Slot::Failed("This account is no longer set up. Open Settings to add it again.".into());
        };
        // A helper process holds back harness updates while it reads; file readers need no hold.
        let gate = if matches!(target.kind, ProviderKind::Codex | ProviderKind::Opencode | ProviderKind::Cursor) {
            self.state.orchestrator.harness_read_gate(target.kind).await
        } else {
            None
        };
        let Some(driver) = self.state.drivers.get(target.kind) else {
            return Slot::Failed("This harness is unavailable.".into());
        };
        match driver.open_session_reader(&target.context).await {
            Ok(reader) => Slot::Ready { reader, _gate: gate },
            Err(error) => Slot::Failed(describe(&error)),
        }
    }
}

fn skip_of(item: &Found, reason: SessionSkipReason, detail: String) -> SessionSkip {
    SessionSkip { provider: item.session.provider, session_id: item.session.id.clone(), title: item.session.title.clone(), reason, detail }
}

#[cfg(test)]
mod tests;
