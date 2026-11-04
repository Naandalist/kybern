//! Git plumbing used by the daemon. Everything shells out to `git`, which is
//! the one dependency every user of a coding agent already has.
//!
//! Checkpoints are commits that are never on a branch: a snapshot of the
//! whole working tree (tracked and untracked, ignored files excluded) written
//! with a temporary index and referenced from `refs/kybern/<thread>/<turn>/<before|after>`.
//! Diffs are between two such commits, so untracked files show up and the
//! user's real index is never touched. Untracked files and directories of
//! `SNAPSHOT_SKIP_BYTES` or more are left out (named in the commit message)
//! and kept when a snapshot is restored.

use kybern_protocol::process::{NoWindow, SpawnError};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use tokio::process::Command;

pub const AUTHOR_NAME: &str = "kybern";
pub const AUTHOR_EMAIL: &str = "checkpoint@kybern.local";
/// Keep a single diff response bounded so JSON parsing and rendering cannot
/// monopolize a desktop client's main thread. Metadata is never truncated.
pub const MAX_PATCH_BYTES: usize = 1024 * 1024;
const MB: u64 = 1024 * 1024;
/// Untracked files and directories this large are left out of snapshots:
/// hashing them on every turn held turns open for minutes and copied them
/// into the object store.
pub const SNAPSHOT_SKIP_BYTES: u64 = 100 * MB;
/// The last snapshot's index, kept in the git dir as the next one's stat cache.
const SNAPSHOT_INDEX: &str = "kybern-snapshot-index";
/// Commit-message line naming a path a snapshot left out.
const SKIPPED_TRAILER: &str = "Kybern-Skipped: ";

/// What git says about a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoState {
    Repo,
    NotRepo,
    /// Git refuses the folder because another user owns it; the message tells the person what to run.
    Unsafe(String),
}

/// The message for git's "dubious ownership" refusal, or `None` for any other stderr. Kybern never
/// writes the user's global git config itself.
pub fn dubious_ownership(stderr: &str, path: &Path) -> Option<String> {
    if !stderr.contains("dubious ownership") {
        return None;
    }
    // git names the folder as it sees it: "repository at 'D:/work/repo'".
    let named = stderr.split("repository at '").nth(1).and_then(|rest| rest.split('\'').next()).filter(|name| !name.is_empty());
    let shown = named.map(str::to_string).unwrap_or_else(|| simplify_path(path).to_string_lossy().replace('\\', "/"));
    Some(format!("Git refuses this folder because another user owns it. Run: git config --global --add safe.directory {shown}"))
}

/// `path` without the `\\?\` prefix that `canonicalize` adds on Windows (`\\?\D:\x` becomes `D:\x`,
/// `\\?\UNC\host\share` becomes `\\host\share`), which git and people do not expect. Other paths
/// come back unchanged.
pub fn simplify_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        let bytes = rest.as_bytes();
        // Only a drive path: `\\?\Volume{..}` and device paths keep their prefix.
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes.get(2).is_none_or(|b| *b == b'\\') {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

/// An untracked path too large to snapshot.
#[derive(Debug, Clone)]
struct SkippedPath {
    path: String,
    /// Size counted up to the limit.
    bytes: u64,
}

/// Every local branch tip, the branch checked out here and the remote's
/// default branch, read with one `for-each-ref`.
#[derive(Debug, Clone, Default)]
pub struct RefTips {
    /// Branch name to its tip commit.
    pub branches: std::collections::BTreeMap<String, String>,
    /// The branch checked out in this working tree (`None` when detached).
    pub current: Option<String>,
    /// The remote's default branch as the clone recorded it (`origin/HEAD`).
    pub origin_default: Option<String>,
}

/// One local branch from `for-each-ref`.
#[derive(Debug, Clone)]
pub struct BranchRef {
    pub name: String,
    pub upstream: Option<String>,
    /// Committer time of the tip, unix seconds.
    pub committed_at: i64,
}

pub struct Repo {
    /// Directory git commands run in (the project root or a worktree).
    pub workdir: PathBuf,
    /// Extra environment for every git command (tests isolate the user's
    /// global configuration with it).
    env: Vec<(String, String)>,
}

/// What a git command printed, whatever its exit status.
#[derive(Debug, Clone)]
pub struct GitOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// How `Repo::merge_no_ff` ended. A conflicting merge is aborted, so the
/// working tree is back to how it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    Merged { commit: String },
    AlreadyUpToDate,
    Conflict { paths: Vec<String> },
}

/// What `Repo::merge_tree_check` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeCheck {
    Clean,
    Conflicts(Vec<String>),
    /// This git cannot check without merging (older than 2.38).
    Unknown,
}

/// One entry of `git status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    /// Two-letter porcelain code, such as ` M`, `A ` or `??`.
    pub code: String,
    pub path: String,
}

/// Changes of the working tree against HEAD (or the index).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingDiff {
    pub files: Vec<WorkingDiffFile>,
    pub patch: String,
    pub patch_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingDiffFile {
    pub path: String,
    pub additions: u32,
    pub deletions: u32,
    pub binary: bool,
}

/// One commit of `git log`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub hash: String,
    pub author: String,
    /// Author date, strict ISO 8601.
    pub date: String,
    pub subject: String,
}

/// One path of a `--name-status -M` listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathChange {
    pub status: FileStatus,
    /// The new path for a rename or copy.
    pub path: String,
    /// The old path of a rename or copy.
    pub old_path: Option<String>,
}

/// A commit with the paths it changed, from `Repo::log_range`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntryWithFiles {
    pub entry: LogEntry,
    pub files: Vec<PathChange>,
}

pub use kybern_protocol::{Diff, DiffStat, FileChange, FileStatus};

/// Git could not be started (not installed, not on PATH, not allowed to run), as a typed error
/// the daemon turns into "Install Git" rather than a failed command.
fn spawn_error(error: std::io::Error) -> SpawnError {
    SpawnError::new("git", error)
}

impl Repo {
    pub fn new(workdir: impl Into<PathBuf>) -> Self {
        Self { workdir: workdir.into(), env: Vec::new() }
    }

    /// The same repository with extra environment variables on every command.
    pub fn with_env(mut self, env: impl IntoIterator<Item = (String, String)>) -> Self {
        self.env.extend(env);
        self
    }

    async fn git(&self, args: &[&str]) -> Result<String> {
        self.git_env(args, &[]).await
    }

    async fn git_env(&self, args: &[&str], env: &[(&str, &str)]) -> Result<String> {
        let mut cmd = Command::new("git").no_window();
        cmd.arg("-C").arg(&self.workdir).args(args).stdin(Stdio::null());
        cmd.envs(self.env.iter().map(|(k, v)| (k, v)));
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().await.map_err(spawn_error)?;
        if !out.status.success() {
            return Err(anyhow!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    }

    /// Run a Git command where a non-zero status is meaningful output rather
    /// than a transport failure (for example `update-index --really-refresh`).
    async fn git_env_allow_nonzero(&self, args: &[&str], env: &[(&str, &str)]) -> Result<()> {
        let mut cmd = Command::new("git").no_window();
        cmd.arg("-C").arg(&self.workdir).args(args).stdin(Stdio::null());
        cmd.envs(self.env.iter().map(|(k, v)| (k, v)));
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().await.map_err(spawn_error)?;
        Ok(())
    }

    /// Like [`Repo::is_repo`], but says why git refused a folder it would otherwise open: a daemon
    /// running as another user (a scheduled task on Windows) meets git's ownership check.
    pub async fn probe(path: &Path) -> RepoState {
        let out = Command::new("git").no_window().arg("-C").arg(path).args(["rev-parse", "--is-inside-work-tree"]).output().await;
        match out {
            Ok(out) if out.status.success() => RepoState::Repo,
            Ok(out) => match dubious_ownership(&String::from_utf8_lossy(&out.stderr), path) {
                Some(message) => RepoState::Unsafe(message),
                None => RepoState::NotRepo,
            },
            Err(_) => RepoState::NotRepo,
        }
    }

    pub async fn is_repo(path: &Path) -> bool {
        Command::new("git")
            .no_window()
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "--is-inside-work-tree"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Whether git ignores `path` (relative to the workdir). A tracked file is
    /// not ignored: its changes would still be committed.
    pub async fn is_ignored(&self, path: &str) -> Result<bool> {
        let out = Command::new("git")
            .no_window()
            .arg("-C")
            .arg(&self.workdir)
            .args(["check-ignore", "-q", "--"])
            .arg(path)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(spawn_error)?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(anyhow!("git check-ignore failed: {}", String::from_utf8_lossy(&out.stderr).trim())),
        }
    }

    /// Whether git tracks `path` (in the index), so writing a secret to it would put the secret in commits.
    pub async fn is_tracked(&self, path: &str) -> Result<bool> {
        let out = Command::new("git")
            .no_window()
            .arg("-C")
            .arg(&self.workdir)
            .args(["ls-files", "--error-unmatch", "--"])
            .arg(path)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await
            .map_err(spawn_error)?;
        Ok(out.status.success())
    }

    pub async fn toplevel(&self) -> Result<PathBuf> {
        Ok(PathBuf::from(self.git(&["rev-parse", "--show-toplevel"]).await?))
    }

    pub async fn git_dir(&self) -> Result<PathBuf> {
        let d = self.git(&["rev-parse", "--absolute-git-dir"]).await?;
        Ok(PathBuf::from(d))
    }

    async fn index_path(&self) -> Result<PathBuf> {
        let path = PathBuf::from(self.git(&["rev-parse", "--git-path", "index"]).await?);
        if path.is_absolute() { Ok(path) } else { Ok(self.workdir.join(path)) }
    }

    pub async fn head(&self) -> Option<String> {
        self.git(&["rev-parse", "--verify", "HEAD"]).await.ok()
    }

    pub async fn current_branch(&self) -> Option<String> {
        self.git(&["symbolic-ref", "--short", "-q", "HEAD"]).await.ok().filter(|s| !s.is_empty())
    }

    /// Local branches, most recently committed first.
    pub async fn branches(&self) -> Result<Vec<BranchRef>> {
        let out = self
            .git(&[
                "for-each-ref",
                "--sort=-committerdate",
                "--format=%(refname:short)%09%(upstream:short)%09%(committerdate:unix)",
                "refs/heads",
            ])
            .await?;
        Ok(out
            .lines()
            .filter_map(|line| {
                let mut it = line.split('\t');
                let name = it.next()?.trim().to_string();
                if name.is_empty() {
                    return None;
                }
                let upstream = it.next().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
                let committed_at = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
                Some(BranchRef { name, upstream, committed_at })
            })
            .collect())
    }

    /// Remote-tracking branches (`origin/feature`), most recently committed
    /// first. Each remote's `HEAD` alias is left out.
    pub async fn remote_branches(&self) -> Result<Vec<BranchRef>> {
        let out =
            self.git(&["for-each-ref", "--sort=-committerdate", "--format=%(refname)%09%(committerdate:unix)", "refs/remotes"]).await?;
        Ok(out
            .lines()
            .filter_map(|line| {
                let mut it = line.split('\t');
                let name = it.next()?.trim().strip_prefix("refs/remotes/")?.to_string();
                if name.is_empty() || name.ends_with("/HEAD") {
                    return None;
                }
                let committed_at = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
                Some(BranchRef { name, upstream: None, committed_at })
            })
            .collect())
    }

    /// Fetch every remote and prune branches deleted there. Never prompts: a
    /// remote that needs credentials fails instead.
    pub async fn fetch_all(&self) -> Result<()> {
        self.git_env(&["fetch", "--all", "--prune", "--quiet"], &[("GIT_TERMINAL_PROMPT", "0")]).await.map(|_| ())
    }

    /// Check out a branch in this working tree. A remote-tracking branch
    /// (`origin/feature`) without a local one gets a local branch tracking it.
    pub async fn switch(&self, branch: &str) -> Result<()> {
        if self.rev_parse(&format!("refs/heads/{branch}")).await.is_err() && self.rev_parse(&format!("refs/remotes/{branch}")).await.is_ok()
        {
            return self.git(&["switch", "--track", branch]).await.map(|_| ());
        }
        self.git(&["switch", branch]).await.map(|_| ())
    }

    /// Snapshot the working tree into a dangling commit and return its hash.
    ///
    /// The temporary index is seeded from the last snapshot's index (kept in
    /// the git dir, one per folder of the repository), so untracked files
    /// that did not change are not hashed again; the user's index seeds the
    /// first one. A snapshot only re-reads the working tree under its own
    /// folder, so in a subfolder the entries outside it are copied from the
    /// user's index, as a fresh seed would have them. Untracked files and
    /// directories of `SNAPSHOT_SKIP_BYTES` or more are left out and named in
    /// the commit message, and `restore` keeps them.
    pub async fn snapshot(&self, message: &str) -> Result<String> {
        self.snapshot_excluding(message, &[]).await
    }

    /// `snapshot`, leaving out `exclude` (paths relative to the workdir, such as
    /// the nested repositories of a group folder). Each exclusion set keeps its
    /// own stat-cache index.
    pub async fn snapshot_excluding(&self, message: &str, exclude: &[String]) -> Result<String> {
        let (git_dir, location) = tokio::join!(self.git_dir(), self.git(&["rev-parse", "--show-toplevel", "--show-prefix"]));
        let git_dir = git_dir?;
        let location = location?;
        let mut lines = location.lines();
        let toplevel = PathBuf::from(lines.next().unwrap_or_default());
        let prefix = lines.next().unwrap_or_default().to_string();
        let key = if exclude.is_empty() { prefix.clone() } else { format!("{prefix}\0{}", exclude.join("\0")) };
        let kept = git_dir.join(snapshot_index_name(&key));
        if tokio::fs::try_exists(&kept).await.unwrap_or(false) {
            match self.snapshot_seeded(message, &git_dir, &kept, true, &toplevel, &prefix, exclude).await {
                Ok(commit) => return Ok(commit),
                Err(error) => {
                    // A kept index must never fail every later checkpoint.
                    tracing::warn!(workdir = %self.workdir.display(), %error, "snapshot from the kept index failed, retrying from the user's index");
                    let _ = tokio::fs::remove_file(&kept).await;
                }
            }
        }
        self.snapshot_seeded(message, &git_dir, &kept, false, &toplevel, &prefix, exclude).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn snapshot_seeded(
        &self,
        message: &str,
        git_dir: &Path,
        persistent: &Path,
        reuse: bool,
        toplevel: &Path,
        prefix: &str,
        exclude: &[String],
    ) -> Result<String> {
        let (working_index, head) = tokio::join!(self.index_path(), self.head());
        let index = git_dir.join(format!("kybern-index-{}", uuid::Uuid::new_v4().simple()));
        let index_str = index.to_string_lossy().to_string();
        let env = [("GIT_INDEX_FILE", index_str.as_str())];
        let result = async {
            let mut from_snapshot_index = false;
            let mut seed_metadata = None;
            let kept = reuse.then(|| persistent.to_path_buf());
            for (seed, is_snapshot_index) in [(kept, true), (working_index.ok(), false)] {
                let Some(seed) = seed else { continue };
                match tokio::fs::metadata(&seed).await {
                    Ok(metadata) => {
                        // Reuse the seed's stat cache instead of re-hashing the
                        // whole worktree.
                        tokio::fs::copy(&seed, &index).await.with_context(|| format!("copy git index {}", seed.display()))?;
                        seed_metadata = Some(metadata);
                        from_snapshot_index = is_snapshot_index;
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error).with_context(|| format!("stat git index {}", seed.display())),
                }
            }

            if seed_metadata.is_none()
                && let Some(head) = head.as_deref()
            {
                self.git_env(&["read-tree", head], &env).await?;
            }
            if let Some(metadata) = seed_metadata {
                // Verify racily-clean entries without throwing away the copied
                // stat cache. A non-zero status only means paths need updating.
                self.git_env_allow_nonzero(&["update-index", "--really-refresh"], &env).await?;
                restore_file_times(&index, &metadata)?;
            }
            if from_snapshot_index {
                if !prefix.is_empty() {
                    let top = Repo { workdir: toplevel.to_path_buf(), env: self.env.clone() };
                    top.copy_user_entries_outside(prefix, &env).await?;
                }
                self.match_tracked_ignored_files(&env).await?;
            }
            let skipped = self.large_untracked(exclude).await?;
            let mut excludes = skipped.iter().map(|entry| format!(":(exclude,literal){}", entry.path)).collect::<Vec<_>>();
            excludes.extend(exclude_pathspecs(exclude));
            if !skipped.is_empty() {
                // An earlier snapshot may still hold a path that has grown too large.
                let literal = skipped.iter().map(|entry| format!(":(literal){}", entry.path)).collect::<Vec<_>>();
                let mut rm = vec!["rm", "--cached", "-r", "-f", "-q", "--ignore-unmatch", "--"];
                rm.extend(literal.iter().map(String::as_str));
                self.git_env(&rm, &env).await?;
            }
            let mut add = vec!["add", "-A", "--", "."];
            add.extend(excludes.iter().map(String::as_str));
            self.git_env(&add, &env).await?;
            let tree = self.git_env(&["write-tree"], &env).await?;
            let mut message = message.to_string();
            if !skipped.is_empty() {
                message.push('\n');
                for entry in &skipped {
                    message.push_str(&format!("\n{SKIPPED_TRAILER}{} ({} MB or more, not snapshotted)", entry.path, entry.bytes / MB));
                }
                tracing::info!(
                    workdir = %self.workdir.display(),
                    skipped = ?skipped.iter().map(|entry| entry.path.as_str()).collect::<Vec<_>>(),
                    "left large untracked paths out of the snapshot"
                );
            }
            let mut args = vec!["commit-tree".to_string(), tree, "-m".into(), message];
            if let Some(head) = head {
                args.push("-p".into());
                args.push(head);
            }
            let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
            let commit = self
                .git_env(
                    &args_ref,
                    &[
                        ("GIT_AUTHOR_NAME", AUTHOR_NAME),
                        ("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL),
                        ("GIT_COMMITTER_NAME", AUTHOR_NAME),
                        ("GIT_COMMITTER_EMAIL", AUTHOR_EMAIL),
                    ],
                )
                .await?;
            // Keep this index as the next snapshot's stat cache. Concurrent
            // snapshots each rename a complete index, so the last one wins.
            tokio::fs::rename(&index, persistent).await.with_context(|| format!("keep snapshot index {}", persistent.display()))?;
            Ok(commit)
        }
        .await;
        let _ = tokio::fs::remove_file(&index).await;
        let mut lock = index.as_os_str().to_os_string();
        lock.push(".lock");
        let _ = tokio::fs::remove_file(PathBuf::from(lock)).await;
        result
    }

    /// Make the snapshot index track the same ignored files as the user's
    /// index: an earlier snapshot may hold a file that is ignored now, and
    /// the user may have force-added an ignored file since.
    async fn match_tracked_ignored_files(&self, env: &[(&str, &str)]) -> Result<()> {
        let list = ["ls-files", "-z", "-c", "-i", "--exclude-standard"];
        let (snapshot, user) = tokio::join!(self.git_env(&list, env), self.git(&list));
        let snapshot = snapshot?;
        let user = user?;
        let snapshot = snapshot.split('\0').filter(|path| !path.is_empty()).collect::<std::collections::HashSet<_>>();
        let user = user.split('\0').filter(|path| !path.is_empty()).collect::<std::collections::HashSet<_>>();
        let stale = snapshot.difference(&user).map(|path| format!(":(literal){path}")).collect::<Vec<_>>();
        if !stale.is_empty() {
            let mut rm = vec!["rm", "--cached", "-f", "-q", "--ignore-unmatch", "--"];
            rm.extend(stale.iter().map(String::as_str));
            self.git_env(&rm, env).await?;
        }
        // A tracked file deleted from disk cannot be force-added; leaving it
        // out records the deletion, as `add -A` does for every other file.
        let forced = user
            .difference(&snapshot)
            .filter(|path| self.workdir.join(path).symlink_metadata().is_ok())
            .map(|path| format!(":(literal){path}"))
            .collect::<Vec<_>>();
        if !forced.is_empty() {
            let mut add = vec!["add", "-f", "--"];
            add.extend(forced.iter().map(String::as_str));
            self.git_env(&add, env).await?;
        }
        Ok(())
    }

    /// Replace the snapshot index's entries outside `prefix` (a folder of
    /// the repository, ending in `/`) with the user's index's: a snapshot
    /// re-reads only its own folder, so a kept index would otherwise freeze
    /// everything else. Runs at the top level, where index paths are full.
    async fn copy_user_entries_outside(&self, prefix: &str, env: &[(&str, &str)]) -> Result<()> {
        let list = ["ls-files", "-s", "-z", "--full-name"];
        let (snapshot, user) = tokio::join!(self.git_env(&list, env), self.git(&list));
        let (snapshot, user) = (snapshot?, user?);
        // `<mode> <object> <stage>\t<path>` records, grouped by path.
        let outside = |listing: &str| {
            let mut entries = std::collections::BTreeMap::<String, Vec<String>>::new();
            for record in listing.split('\0').filter(|record| !record.is_empty()) {
                if let Some((_, path)) = record.split_once('\t')
                    && !path.starts_with(prefix)
                {
                    entries.entry(path.to_string()).or_default().push(record.to_string());
                }
            }
            entries
        };
        let (snapshot, user) = (outside(&snapshot), outside(&user));
        let mut input = String::new();
        for (path, records) in &snapshot {
            if user.get(path) != Some(records) {
                // Mode 0 removes every stage of the path.
                let object = records[0].split(' ').nth(1).unwrap_or_default();
                input.push_str(&format!("0 {object}\t{path}\0"));
            }
        }
        for (path, records) in &user {
            if snapshot.get(path) != Some(records) {
                for record in records {
                    input.push_str(record);
                    input.push('\0');
                }
            }
        }
        if input.is_empty() {
            return Ok(());
        }
        let mut cmd = Command::new("git").no_window();
        cmd.arg("-C").arg(&self.workdir).args(["update-index", "-z", "--index-info"]);
        cmd.envs(self.env.iter().map(|(k, v)| (k, v)));
        cmd.envs(env.iter().copied());
        cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(spawn_error)?;
        let mut stdin = child.stdin.take().context("git stdin")?;
        tokio::io::AsyncWriteExt::write_all(&mut stdin, input.as_bytes()).await.context("write git index info")?;
        drop(stdin);
        let out = child.wait_with_output().await.map_err(spawn_error)?;
        if !out.status.success() {
            return Err(anyhow!("git update-index --index-info failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(())
    }

    /// Untracked files and wholly untracked directories (as `git status`
    /// collapses them) of `SNAPSHOT_SKIP_BYTES` or more. Sizing stops at the
    /// limit, so a huge directory costs about as much as a small one.
    async fn large_untracked(&self, exclude: &[String]) -> Result<Vec<SkippedPath>> {
        let excludes = exclude_pathspecs(exclude);
        let mut args = vec!["ls-files", "-z", "-o", "--directory", "--exclude-standard"];
        if !excludes.is_empty() {
            args.extend(["--", "."]);
            args.extend(excludes.iter().map(String::as_str));
        }
        let out = self.git(&args).await?;
        let workdir = self.workdir.clone();
        let entries = out.split('\0').filter(|path| !path.is_empty()).map(str::to_string).collect::<Vec<_>>();
        tokio::task::spawn_blocking(move || {
            entries
                .into_iter()
                .filter_map(|entry| {
                    let path = entry.trim_end_matches('/').to_string();
                    let bytes = size_up_to(&workdir.join(&path), SNAPSHOT_SKIP_BYTES);
                    (bytes >= SNAPSHOT_SKIP_BYTES).then_some(SkippedPath { path, bytes })
                })
                .collect()
        })
        .await
        .context("size untracked files")
    }

    /// Paths a snapshot left out (see `snapshot`), from its commit message.
    pub async fn snapshot_skipped(&self, commit: &str) -> Result<Vec<String>> {
        let message = self.git(&["log", "-1", "--format=%B", commit]).await?;
        Ok(message
            .lines()
            .filter_map(|line| line.strip_prefix(SKIPPED_TRAILER))
            .map(|rest| rest.rsplit_once(" (").map_or(rest, |(path, _)| path).to_string())
            .collect())
    }

    pub async fn update_ref(&self, name: &str, commit: &str) -> Result<()> {
        self.git(&["update-ref", name, commit]).await.map(|_| ())
    }

    pub async fn delete_ref(&self, name: &str) -> Result<()> {
        let _ = self.git(&["update-ref", "-d", name]).await;
        Ok(())
    }

    /// Reset the working tree and index to exactly the snapshot. Untracked,
    /// non-ignored files that are not in the snapshot are removed.
    /// Paths the snapshot left out for their size, and untracked paths that
    /// are that large now, are kept.
    pub async fn restore(&self, commit: &str) -> Result<()> {
        let (skipped, large) = tokio::join!(self.snapshot_skipped(commit), self.large_untracked(&[]));
        let mut keep = skipped?;
        keep.extend(large?.into_iter().map(|entry| entry.path));
        self.git(&["read-tree", "--reset", "-u", commit]).await?;
        let excludes = keep.iter().map(|path| format!(":(exclude,literal){path}")).collect::<Vec<_>>();
        let mut clean = vec!["clean", "-fd", "--", "."];
        clean.extend(excludes.iter().map(String::as_str));
        self.git(&clean).await?;
        Ok(())
    }

    /// `restore`, leaving `exclude` (paths relative to the workdir) untouched.
    /// `read-tree --reset` cannot exclude, so this restores through pathspecs.
    pub async fn restore_excluding(&self, commit: &str, exclude: &[String]) -> Result<()> {
        if exclude.is_empty() {
            return self.restore(commit).await;
        }
        let (skipped, large) = tokio::join!(self.snapshot_skipped(commit), self.large_untracked(exclude));
        let mut keep = skipped?;
        keep.extend(large?.into_iter().map(|entry| entry.path));
        let mut excludes = keep.iter().map(|path| format!(":(exclude,literal){path}")).collect::<Vec<_>>();
        excludes.extend(exclude_pathspecs(exclude));
        let source = format!("--source={commit}");
        let mut args = vec!["restore", source.as_str(), "--staged", "--worktree", "--", "."];
        args.extend(excludes.iter().map(String::as_str));
        self.git(&args).await?;
        let mut clean = vec!["clean", "-fd", "--", "."];
        clean.extend(excludes.iter().map(String::as_str));
        self.git(&clean).await?;
        Ok(())
    }

    pub async fn diff(&self, from: &str, to: &str) -> Result<Diff> {
        self.diff_with_options(from, to, true, None).await
    }

    /// Diff two snapshots, optionally omitting the unified patch or limiting it
    /// to one repository-relative path. Metadata is always returned.
    pub async fn diff_with_options(&self, from: &str, to: &str, include_patch: bool, path: Option<&str>) -> Result<Diff> {
        self.diff_prefixed(from, to, include_patch, path, None).await
    }

    /// `diff_with_options` for a repository inside a larger folder: with
    /// `prefix` (`api`), file paths and patch headers are relative to that
    /// folder (`api/src/x.rs`). `path` stays relative to this repository.
    pub async fn diff_prefixed(&self, from: &str, to: &str, include_patch: bool, path: Option<&str>, prefix: Option<&str>) -> Result<Diff> {
        let prefix = prefix.map(|p| p.trim_matches('/')).filter(|p| !p.is_empty() && *p != ".");
        let src_prefix = prefix.map(|p| format!("--src-prefix=a/{p}/"));
        let dst_prefix = prefix.map(|p| format!("--dst-prefix=b/{p}/"));
        let args = |mode: &'static str| {
            let mut args = vec!["diff", mode, "-M"];
            if mode == "--no-color"
                && let (Some(src), Some(dst)) = (&src_prefix, &dst_prefix)
            {
                args.extend([src.as_str(), dst.as_str()]);
            }
            args.extend([from, to]);
            if let Some(path) = path {
                args.extend(["--", path]);
            }
            args
        };
        let numstat_args = args("--numstat");
        let status_args = args("--name-status");
        let (numstat, status) = tokio::try_join!(self.git(&numstat_args), self.git(&status_args))?;
        let mut patch = if include_patch { self.git(&args("--no-color")).await? } else { String::new() };
        let patch_truncated = truncate_utf8(&mut patch, MAX_PATCH_BYTES);

        let mut files: Vec<FileChange> = parse_name_status(&status)
            .into_iter()
            .map(|change| FileChange {
                path: change.path,
                old_path: change.old_path,
                status: change.status,
                additions: 0,
                deletions: 0,
                binary: false,
                repo: None,
            })
            .collect();
        for line in numstat.lines() {
            let mut parts = line.split('\t');
            let add = parts.next().unwrap_or("-");
            let del = parts.next().unwrap_or("-");
            let rest = parts.next().unwrap_or("");
            // Renames appear as "old => new" or "{a => b}/x" in numstat; match on the new path.
            let path = rest.split(" => ").last().unwrap_or(rest).replace(['{', '}'], "");
            if let Some(f) = files.iter_mut().find(|f| f.path == path || f.path.ends_with(&path)) {
                if add == "-" {
                    f.binary = true;
                } else {
                    f.additions = add.parse().unwrap_or(0);
                    f.deletions = del.parse().unwrap_or(0);
                }
            }
        }
        if let Some(prefix) = prefix {
            for file in &mut files {
                file.path = format!("{prefix}/{}", file.path);
                if let Some(old) = file.old_path.as_mut() {
                    *old = format!("{prefix}/{old}");
                }
            }
        }
        Ok(Diff { from: from.into(), to: to.into(), files, patch, patch_truncated, repos: Vec::new() })
    }

    // ---- state queries used by delegated-worktree cleanup ----

    /// Whether the working tree has no changes, counting untracked files
    /// (ignored files do not count).
    pub async fn is_clean(&self) -> Result<bool> {
        Ok(self.git(&["status", "--porcelain"]).await?.is_empty())
    }

    /// Branch tips, the current branch and `origin/HEAD` in one git call that
    /// gives up after `timeout`.
    pub async fn ref_tips(&self, timeout: std::time::Duration) -> Result<RefTips> {
        let out = self
            .output_with(
                &["for-each-ref", "--format=%(HEAD)%00%(refname)%00%(objectname)%00%(symref)", "refs/heads", "refs/remotes/origin/HEAD"],
                &[],
                Some(timeout),
            )
            .await?;
        if !out.success {
            return Err(anyhow!("git for-each-ref failed: {}", out.stderr));
        }
        let mut tips = RefTips::default();
        for line in out.stdout.lines() {
            let mut fields = line.split('\0');
            let (Some(head), Some(name), Some(sha)) = (fields.next(), fields.next(), fields.next()) else { continue };
            let symref = fields.next().unwrap_or_default();
            if name == "refs/remotes/origin/HEAD" {
                tips.origin_default = symref.strip_prefix("refs/remotes/origin/").filter(|name| !name.is_empty()).map(str::to_owned);
            } else if let Some(branch) = name.strip_prefix("refs/heads/") {
                if head == "*" {
                    tips.current = Some(branch.to_owned());
                }
                tips.branches.insert(branch.to_owned(), sha.to_owned());
            }
        }
        Ok(tips)
    }

    /// `status_porcelain` for a read that runs beside the user's own git: it
    /// never takes `index.lock` to refresh the index (`GIT_OPTIONAL_LOCKS=0`)
    /// and gives up after `timeout`.
    pub async fn status_porcelain_unlocked(&self, exclude: &[String], timeout: std::time::Duration) -> Result<Vec<String>> {
        let excludes = exclude_pathspecs(exclude);
        let mut args = vec!["status", "--porcelain", "--", "."];
        args.extend(excludes.iter().map(String::as_str));
        let out = self.output_with(&args, &[("GIT_OPTIONAL_LOCKS", "0")], Some(timeout)).await?;
        if !out.success {
            return Err(anyhow!("git status failed: {}", out.stderr));
        }
        Ok(out.stdout.lines().filter(|line| !line.is_empty()).map(str::to_string).collect())
    }

    /// `git status --porcelain` lines, leaving out `exclude` (paths relative to
    /// the workdir, such as the nested repositories of a group folder).
    pub async fn status_porcelain(&self, exclude: &[String]) -> Result<Vec<String>> {
        let excludes = exclude_pathspecs(exclude);
        let mut args = vec!["status", "--porcelain", "--", "."];
        args.extend(excludes.iter().map(String::as_str));
        Ok(self.git(&args).await?.lines().filter(|line| !line.is_empty()).map(str::to_string).collect())
    }

    /// Whether merging `theirs` into `ours` would conflict, without touching
    /// the working tree (`git merge-tree --write-tree`, git 2.38 or newer).
    pub async fn merge_tree_check(&self, ours: &str, theirs: &str) -> Result<MergeCheck> {
        let out = self.output(&["merge-tree", "--write-tree", "--name-only", "--no-messages", ours, theirs]).await?;
        if out.success {
            return Ok(MergeCheck::Clean);
        }
        // Exit status 1 with a tree id first: the merge conflicts.
        let mut lines = out.stdout.lines();
        let tree = lines.next().unwrap_or_default();
        if !tree.is_empty() && tree.chars().all(|c| c.is_ascii_hexdigit()) {
            let mut paths: Vec<String> = lines.filter(|line| !line.is_empty()).map(str::to_string).collect();
            paths.dedup();
            return Ok(MergeCheck::Conflicts(paths));
        }
        if out.stderr.contains("usage:") || out.stderr.contains("unknown option") {
            return Ok(MergeCheck::Unknown);
        }
        Err(anyhow!("git merge-tree failed: {}", out.stderr))
    }

    /// Whether `ancestor` is reachable from `descendant` (`git merge-base
    /// --is-ancestor`). An unknown revision is an error, not `false`.
    pub async fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let out = Command::new("git")
            .no_window()
            .arg("-C")
            .arg(&self.workdir)
            .args(["merge-base", "--is-ancestor", ancestor, descendant])
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(spawn_error)?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(anyhow!("git merge-base --is-ancestor failed: {}", String::from_utf8_lossy(&out.stderr).trim())),
        }
    }

    /// The best common ancestor of two commits (`git merge-base`).
    pub async fn merge_base(&self, left: &str, right: &str) -> Result<String> {
        self.git(&["merge-base", left, right]).await
    }

    /// Resolve a revision to a full commit id.
    pub async fn rev_parse(&self, revision: &str) -> Result<String> {
        self.git(&["rev-parse", "--verify", &format!("{revision}^{{commit}}")]).await
    }

    /// Delete a local branch. Without `force` git refuses unless it is merged.
    pub async fn delete_branch(&self, branch: &str, force: bool) -> Result<()> {
        self.git(&["branch", if force { "-D" } else { "-d" }, "--", branch]).await.map(|_| ())
    }

    /// Files and lines changed from `base` to `head` (binary files count as a file).
    pub async fn diffstat(&self, base: &str, head: &str) -> Result<DiffStat> {
        let numstat = self.git(&["diff", "--numstat", "-M", base, head]).await?;
        let mut stat = DiffStat::default();
        for line in numstat.lines() {
            let mut parts = line.split('\t');
            let (add, del) = (parts.next().unwrap_or("-"), parts.next().unwrap_or("-"));
            stat.files += 1;
            stat.additions += add.parse::<u32>().unwrap_or(0);
            stat.deletions += del.parse::<u32>().unwrap_or(0);
        }
        Ok(stat)
    }

    // ---- operations behind the Coordinator's git tool ----

    /// Run git and return what it printed, whether or not it succeeded.
    pub async fn output(&self, args: &[&str]) -> Result<GitOutput> {
        self.output_with(args, &[], None).await
    }

    /// Run git with extra environment and an optional time limit, and return what it printed.
    pub async fn output_env(&self, args: &[&str], env: &[(&str, &str)], timeout: Option<std::time::Duration>) -> Result<GitOutput> {
        self.output_with(args, env, timeout).await
    }

    async fn output_with(&self, args: &[&str], env: &[(&str, &str)], timeout: Option<std::time::Duration>) -> Result<GitOutput> {
        let mut cmd = Command::new("git").no_window();
        cmd.arg("-C").arg(&self.workdir).args(args).stdin(Stdio::null()).kill_on_drop(true);
        cmd.envs(self.env.iter().map(|(k, v)| (k, v)));
        // Never wait for a person at a prompt or an editor.
        cmd.env("GIT_TERMINAL_PROMPT", "0").env("GIT_EDITOR", "true");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let run = cmd.output();
        let out = match timeout {
            Some(limit) => tokio::time::timeout(limit, run).await.map_err(|_| anyhow!("git {} timed out", args.join(" ")))?,
            None => run.await,
        }
        .map_err(spawn_error)?;
        Ok(GitOutput {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim_end().to_string(),
        })
    }

    /// Changed files with their porcelain codes, untracked files listed one by one.
    pub async fn status_entries(&self) -> Result<Vec<StatusEntry>> {
        let out = self.git(&["status", "--porcelain=v1", "-z", "--untracked-files=all"]).await?;
        let mut entries = Vec::new();
        let mut parts = out.split('\0').filter(|part| !part.is_empty());
        while let Some(part) = parts.next() {
            if part.len() < 4 {
                continue;
            }
            let (code, path) = part.split_at(2);
            if code.contains('R') || code.contains('C') {
                // A rename or copy is followed by its original path.
                parts.next();
            }
            entries.push(StatusEntry { code: code.to_string(), path: path.trim_start().to_string() });
        }
        Ok(entries)
    }

    /// The newest `limit` commits of the current branch; empty before the first commit.
    pub async fn log(&self, limit: usize) -> Result<Vec<LogEntry>> {
        if self.head().await.is_none() {
            return Ok(Vec::new());
        }
        let count = limit.max(1).to_string();
        let out = self.git(&["log", "-n", &count, "--format=%H%x1f%an%x1f%aI%x1f%s"]).await?;
        Ok(out
            .lines()
            .filter_map(|line| {
                let mut fields = line.split('\x1f');
                Some(LogEntry {
                    hash: fields.next()?.to_string(),
                    author: fields.next()?.to_string(),
                    date: fields.next()?.to_string(),
                    subject: fields.next().unwrap_or("").to_string(),
                })
            })
            .collect())
    }

    /// Commits reachable from `to` but not from `from` (`from..to`; an empty
    /// `from` means all of `to`'s history), newest first and at most `limit`, each with the paths it changed (renames
    /// detected). Merge commits list no paths. `from` must be an ancestor of
    /// `to` for the range to mean "what changed since"; check `is_ancestor`
    /// first when history may have been rewritten.
    pub async fn log_range(&self, from: &str, to: &str, limit: usize) -> Result<Vec<LogEntryWithFiles>> {
        const START: &str = "\x1e";
        let count = limit.max(1).to_string();
        let range = if from.is_empty() { to.to_string() } else { format!("{from}..{to}") };
        let out =
            self.git(&["log", "-n", &count, "-M", "--name-status", &format!("--format={START}%H%x1f%an%x1f%aI%x1f%s"), &range]).await?;
        let mut entries = Vec::new();
        for block in out.split(START).filter(|block| !block.trim().is_empty()) {
            let (header, files) = block.split_once('\n').unwrap_or((block, ""));
            let mut fields = header.split('\x1f');
            let (Some(hash), Some(author), Some(date)) = (fields.next(), fields.next(), fields.next()) else {
                continue;
            };
            let entry = LogEntry {
                hash: hash.to_string(),
                author: author.to_string(),
                date: date.to_string(),
                subject: fields.next().unwrap_or("").to_string(),
            };
            entries.push(LogEntryWithFiles { entry, files: parse_name_status(files) });
        }
        Ok(entries)
    }

    /// How many commits `from..to` holds (`rev-list --count`).
    pub async fn count_range(&self, from: &str, to: &str) -> Result<usize> {
        let range = if from.is_empty() { to.to_string() } else { format!("{from}..{to}") };
        Ok(self.git(&["rev-list", "--count", &range]).await?.trim().parse().unwrap_or(0))
    }

    /// Paths that differ between two revisions (`diff --name-status -M`).
    pub async fn changed_paths(&self, from: &str, to: &str) -> Result<Vec<PathChange>> {
        Ok(parse_name_status(&self.git(&["diff", "--name-status", "-M", from, to]).await?))
    }

    /// Uncommitted changes to tracked files (or only the staged ones) with a
    /// unified patch cut at `max_bytes`. Untracked files are in `status_entries`.
    pub async fn diff_uncommitted(&self, staged: bool, path: Option<&str>, max_bytes: usize) -> Result<WorkingDiff> {
        let mut base: Vec<&str> = vec!["diff"];
        if staged || self.head().await.is_none() {
            base.push("--cached");
        } else {
            base.push("HEAD");
        }
        let with = |mode: &'static str| {
            let mut args = base.clone();
            args.insert(1, mode);
            args.push("--");
            if let Some(path) = path {
                args.push(path);
            }
            args
        };
        let numstat = self.git(&with("--numstat")).await?;
        let mut patch = self.git(&with("--no-color")).await?;
        let patch_truncated = truncate_utf8(&mut patch, max_bytes);
        let files = numstat
            .lines()
            .filter_map(|line| {
                let mut parts = line.split('\t');
                let (add, del, path) = (parts.next()?, parts.next()?, parts.next()?);
                Some(WorkingDiffFile {
                    path: path.to_string(),
                    additions: add.parse().unwrap_or(0),
                    deletions: del.parse().unwrap_or(0),
                    binary: add == "-",
                })
            })
            .collect();
        Ok(WorkingDiff { files, patch, patch_truncated })
    }

    /// A configured value, or `None` when it is unset or empty.
    pub async fn config_get(&self, key: &str) -> Option<String> {
        let out = self.output(&["config", "--get", key]).await.ok()?;
        Some(out.stdout.trim().to_string()).filter(|value| out.success && !value.is_empty())
    }

    /// The configured commit identity `(name, email)`; either is `None` when unset.
    pub async fn identity(&self) -> (Option<String>, Option<String>) {
        tokio::join!(self.config_get("user.name"), self.config_get("user.email"))
    }

    /// The remote's default branch as the clone recorded it (`origin/HEAD`).
    pub async fn default_branch(&self) -> Option<String> {
        let out = self.git(&["symbolic-ref", "--short", "-q", "refs/remotes/origin/HEAD"]).await.ok()?;
        out.strip_prefix("origin/").map(str::to_string).filter(|name| !name.is_empty())
    }

    /// Whether `name` is the branch work must not be committed to directly:
    /// `main`, `master` or the remote's recorded default.
    pub async fn is_default_branch(&self, name: &str) -> bool {
        matches!(name, "main" | "master") || self.default_branch().await.as_deref() == Some(name)
    }

    /// Whether `name` is an acceptable branch name.
    pub async fn is_valid_branch_name(&self, name: &str) -> bool {
        !name.starts_with('-') && self.output(&["check-ref-format", "--branch", name]).await.is_ok_and(|out| out.success)
    }

    /// Create `name` (forked from `from`, HEAD when `None`) and switch to it.
    pub async fn create_branch(&self, name: &str, from: Option<&str>) -> Result<()> {
        let mut args = vec!["switch", "-c", name];
        if let Some(from) = from {
            args.push(from);
        }
        self.git(&args).await.map(|_| ())
    }

    /// Stage and commit. With `paths`, only those paths are committed (a new
    /// file is added first); otherwise every tracked and untracked change is.
    /// Uses the configured identity and returns the new commit.
    pub async fn commit(&self, message: &str, paths: Option<&[String]>) -> Result<String> {
        match paths {
            Some(paths) if !paths.is_empty() => {
                let mut add = vec!["add", "-A", "--"];
                add.extend(paths.iter().map(String::as_str));
                self.git(&add).await?;
                let mut commit = vec!["commit", "-q", "-m", message, "--"];
                commit.extend(paths.iter().map(String::as_str));
                self.git(&commit).await?;
            }
            _ => {
                self.git(&["add", "-A", "--", "."]).await?;
                self.git(&["commit", "-q", "-m", message]).await?;
            }
        }
        self.git(&["rev-parse", "HEAD"]).await
    }

    /// The branch's upstream (`origin/feature`), if it has one.
    pub async fn upstream(&self, branch: &str) -> Option<String> {
        self.git(&["rev-parse", "--abbrev-ref", &format!("{branch}@{{upstream}}")]).await.ok().filter(|value| !value.is_empty())
    }

    /// The remote a branch pushes to: its upstream's remote, else `origin`,
    /// else the only remote. `None` when there is no remote.
    pub async fn push_remote(&self, branch: &str) -> Option<String> {
        let remotes: Vec<String> = self.git(&["remote"]).await.ok()?.lines().map(str::to_string).collect();
        if let Some(remote) = self.config_get(&format!("branch.{branch}.remote")).await
            && remotes.contains(&remote)
        {
            return Some(remote);
        }
        match remotes.as_slice() {
            [] => None,
            [only] => Some(only.clone()),
            many => many.iter().find(|remote| remote.as_str() == "origin").cloned(),
        }
    }

    /// Commits of `branch` that its upstream (or, without one, any ref of
    /// `remote`) does not have.
    pub async fn commits_ahead(&self, branch: &str, remote: &str) -> Result<u32> {
        let count = match self.upstream(branch).await {
            Some(upstream) => self.git(&["rev-list", "--count", &format!("{upstream}..{branch}")]).await?,
            None => self.git(&["rev-list", "--count", branch, "--not", &format!("--remotes={remote}")]).await?,
        };
        Ok(count.trim().parse().unwrap_or(0))
    }

    /// Commits `(ahead, behind)` the upstream; `None` without one.
    pub async fn ahead_behind(&self, branch: &str) -> Option<(u32, u32)> {
        let upstream = self.upstream(branch).await?;
        let out = self.git(&["rev-list", "--left-right", "--count", &format!("{upstream}...{branch}")]).await.ok()?;
        let mut parts = out.split_whitespace();
        let behind = parts.next()?.parse().ok()?;
        let ahead = parts.next()?.parse().ok()?;
        Some((ahead, behind))
    }

    /// Merge `branch` into the current branch with a merge commit. A merge that
    /// conflicts is aborted and reports the conflicting paths.
    pub async fn merge_no_ff(&self, branch: &str, message: &str) -> Result<MergeOutcome> {
        let out = self.output(&["merge", "--no-ff", "--no-edit", "-m", message, branch]).await?;
        if out.success {
            if out.stdout.contains("Already up to date") {
                return Ok(MergeOutcome::AlreadyUpToDate);
            }
            return Ok(MergeOutcome::Merged { commit: self.git(&["rev-parse", "HEAD"]).await? });
        }
        let unmerged = self.output(&["diff", "--name-only", "--diff-filter=U"]).await?;
        let paths: Vec<String> = unmerged.stdout.lines().map(str::to_string).filter(|path| !path.is_empty()).collect();
        if self.rev_parse("MERGE_HEAD").await.is_ok() {
            self.output(&["merge", "--abort"]).await?;
        }
        if paths.is_empty() {
            let detail = if out.stderr.is_empty() { out.stdout } else { out.stderr };
            return Err(anyhow!("git merge failed: {detail}"));
        }
        Ok(MergeOutcome::Conflict { paths })
    }

    /// `git push -u remote branch`. Never forced; a rejected push is an error.
    pub async fn push_upstream(&self, remote: &str, branch: &str) -> Result<String> {
        let out = self.output_with(&["push", "-u", remote, branch], &[], Some(std::time::Duration::from_secs(120))).await?;
        if !out.success {
            return Err(anyhow!("git push failed: {}", out.stderr));
        }
        Ok(if out.stderr.is_empty() { out.stdout } else { out.stderr })
    }

    // ---- worktrees ----

    /// Add a worktree on a new branch, forked from `start` (HEAD when `None`).
    pub async fn worktree_add(&self, path: &Path, branch: &str, start: Option<&str>) -> Result<()> {
        let p = path.to_string_lossy().to_string();
        // A remote-tracking start (`origin/feature`) must not become the new branch's upstream.
        let mut args = vec!["worktree", "add", "--no-track", "-b", branch, &p];
        if let Some(start) = start {
            args.push(start);
        }
        self.git(&args).await.map(|_| ())
    }

    /// Restore a managed worktree on its retained branch.
    pub async fn worktree_add_existing(&self, path: &Path, branch: &str) -> Result<()> {
        self.git(&["worktree", "add", &path.to_string_lossy(), branch]).await.map(|_| ())
    }

    /// Forget worktrees whose directory no longer exists (`git worktree prune`).
    pub async fn worktree_prune(&self) -> Result<()> {
        self.git(&["worktree", "prune"]).await.map(|_| ())
    }

    pub async fn worktree_remove(&self, path: &Path, force: bool) -> Result<()> {
        let p = path.to_string_lossy().to_string();
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(&p);
        self.git(&args).await.map(|_| ())
    }
}

/// The kept snapshot index of the repository folder `prefix` (empty at the
/// top level). Each folder keeps its own: they cover different paths.
fn snapshot_index_name(prefix: &str) -> String {
    if prefix.is_empty() {
        return SNAPSHOT_INDEX.to_string();
    }
    // FNV-1a: stable across builds, unlike the standard hasher.
    let hash = prefix.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3));
    format!("{SNAPSHOT_INDEX}-{hash:016x}")
}

fn restore_file_times(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    let modified = metadata.modified().context("read git index modified time")?;
    let accessed = metadata.accessed().unwrap_or(modified);
    let times = std::fs::FileTimes::new().set_accessed(accessed).set_modified(modified);
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("open temporary git index {}", path.display()))?
        .set_times(times)
        .with_context(|| format!("restore temporary git index times {}", path.display()))
}

/// Bytes in a file or directory tree (symlinks not followed), counted until
/// `limit` is reached.
fn size_up_to(path: &Path, limit: u64) -> u64 {
    let Ok(metadata) = std::fs::symlink_metadata(path) else { return 0 };
    if !metadata.is_dir() {
        return metadata.len();
    }
    let mut total = 0;
    let mut pending = vec![path.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else { continue };
            if metadata.is_dir() {
                pending.push(entry.path());
            } else {
                total += metadata.len();
                if total >= limit {
                    return total;
                }
            }
        }
    }
    total
}

fn truncate_utf8(value: &mut String, max_bytes: usize) -> bool {
    if value.len() <= max_bytes {
        return false;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    true
}

/// Parse `--name-status` lines (`M\tpath`, `R100\told\tnew`). Blank lines
/// and lines without a path are skipped.
fn parse_name_status(text: &str) -> Vec<PathChange> {
    let mut changes = Vec::new();
    for line in text.lines() {
        let mut parts = line.split('\t');
        let code = parts.next().unwrap_or("");
        let a = parts.next().unwrap_or("").to_string();
        let b = parts.next().map(str::to_string);
        if code.is_empty() || a.is_empty() {
            continue;
        }
        let (status, path, old_path) = match code.chars().next() {
            Some('A') => (FileStatus::Added, a, None),
            Some('M') => (FileStatus::Modified, a, None),
            Some('D') => (FileStatus::Deleted, a, None),
            Some('R') => (FileStatus::Renamed, b.clone().unwrap_or_default(), Some(a)),
            Some('C') => (FileStatus::Copied, b.clone().unwrap_or_default(), Some(a)),
            Some('T') => (FileStatus::TypeChanged, a, None),
            _ => (FileStatus::Unknown, a, None),
        };
        changes.push(PathChange { status, path, old_path });
    }
    changes
}

/// `:(exclude,literal)<path>` pathspecs for paths relative to the workdir.
pub fn exclude_pathspecs(paths: &[String]) -> Vec<String> {
    paths.iter().map(|path| format!(":(exclude,literal){}", path.trim_end_matches('/'))).collect()
}

pub fn checkpoint_ref(thread_id: &str, turn_id: &str, which: &str) -> String {
    format!("refs/kybern/{thread_id}/{turn_id}/{which}")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn init_repo() -> (tempfile::TempDir, Repo) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::new(dir.path());
        repo.git(&["init", "-q"]).await.unwrap();
        repo.git(&["config", "core.autocrlf", "false"]).await.unwrap();
        repo.git(&["config", "user.email", "t@t"]).await.unwrap();
        repo.git(&["config", "user.name", "t"]).await.unwrap();
        (dir, repo)
    }

    #[tokio::test]
    async fn ref_tips_reads_every_branch_the_current_one_and_origin_head_in_one_call() {
        let (dir, repo) = init_repo().await;
        repo.git(&["symbolic-ref", "HEAD", "refs/heads/trunk"]).await.unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        repo.git(&["add", "."]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "init"]).await.unwrap();
        let first = repo.head().await.unwrap();
        repo.git(&["branch", "kybern/child"]).await.unwrap();
        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        repo.git(&["commit", "-qam", "second"]).await.unwrap();
        let second = repo.head().await.unwrap();
        let timeout = std::time::Duration::from_secs(5);

        let tips = repo.ref_tips(timeout).await.unwrap();
        assert_eq!(tips.branches.get("trunk"), Some(&second));
        assert_eq!(tips.branches.get("kybern/child"), Some(&first));
        assert_eq!((tips.current.as_deref(), tips.origin_default.as_deref()), (Some("trunk"), None));

        repo.git(&["update-ref", "refs/remotes/origin/trunk", &second]).await.unwrap();
        repo.git(&["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/trunk"]).await.unwrap();
        repo.git(&["checkout", "-q", "--detach"]).await.unwrap();
        let tips = repo.ref_tips(timeout).await.unwrap();
        assert_eq!((tips.current.as_deref(), tips.origin_default.as_deref()), (None, Some("trunk")));
        assert_eq!(tips.branches.len(), 2, "remote refs are not branches: {:?}", tips.branches);
        assert_eq!(repo.merge_base("trunk", "kybern/child").await.unwrap(), first);

        // Status that leaves the index alone still sees the change, and a hung git gives up.
        std::fs::write(dir.path().join("a.txt"), "three\n").unwrap();
        assert_eq!(repo.status_porcelain_unlocked(&[], timeout).await.unwrap(), [" M a.txt"]);
        // A file-system monitor hook that sleeps holds `git status`; a timeout of a
        // few nanoseconds instead raced a fast git on a busy runner.
        let hook = if cfg!(windows) { "powershell.exe -NoProfile -Command \"Start-Sleep -Seconds 10\"" } else { "sleep 10; :" };
        repo.git(&["config", "core.fsmonitor", hook]).await.unwrap();
        let error = repo.status_porcelain_unlocked(&[], std::time::Duration::from_millis(300)).await.unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
    }

    #[tokio::test]
    async fn snapshot_diff_restore_roundtrip() {
        let (dir, repo) = init_repo().await;
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        repo.git(&["add", "."]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "init"]).await.unwrap();

        let before = repo.snapshot("before").await.unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.path().join("new.txt"), "hi\n").unwrap();
        let after = repo.snapshot("after").await.unwrap();

        let d = repo.diff(&before, &after).await.unwrap();
        assert_eq!(d.files.len(), 2);
        let a = d.files.iter().find(|f| f.path == "a.txt").unwrap();
        assert_eq!(a.status, FileStatus::Modified);
        assert_eq!(a.additions, 1);
        let n = d.files.iter().find(|f| f.path == "new.txt").unwrap();
        assert_eq!(n.status, FileStatus::Added);
        assert!(d.patch.contains("+two"));

        // The user's real index is untouched by snapshots.
        let status = repo.git(&["status", "--porcelain"]).await.unwrap();
        assert!(status.contains("?? new.txt"), "{status}");

        repo.restore(&before).await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "one\n");
        assert!(!dir.path().join("new.txt").exists());
    }

    #[tokio::test]
    async fn diff_can_skip_patch_and_scope_to_path() {
        let (dir, repo) = init_repo().await;
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "one\n").unwrap();
        repo.git(&["add", "."]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "init"]).await.unwrap();

        let before = repo.snapshot("before").await.unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "one\nthree\n").unwrap();
        let after = repo.snapshot("after").await.unwrap();

        let summary = repo.diff_with_options(&before, &after, false, None).await.unwrap();
        assert_eq!(summary.files.len(), 2);
        assert!(summary.patch.is_empty());

        let file = repo.diff_with_options(&before, &after, true, Some("a.txt")).await.unwrap();
        assert_eq!(file.files.len(), 1);
        assert_eq!(file.files[0].path, "a.txt");
        assert!(file.patch.contains("+two"));
        assert!(!file.patch.contains("b.txt"));
        assert!(!file.patch_truncated);
    }

    #[tokio::test]
    async fn oversized_patch_is_transport_bounded_without_losing_stats() {
        let (dir, repo) = init_repo().await;
        std::fs::write(dir.path().join("large.txt"), "before\n").unwrap();
        repo.git(&["add", "."]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "init"]).await.unwrap();

        let before = repo.snapshot("before").await.unwrap();
        let contents = (0..30_000).map(|line| format!("{line:05} {}\n", "x".repeat(48))).collect::<String>();
        std::fs::write(dir.path().join("large.txt"), contents).unwrap();
        let after = repo.snapshot("after").await.unwrap();

        let diff = repo.diff_with_options(&before, &after, true, Some("large.txt")).await.unwrap();
        assert_eq!(diff.patch.len(), MAX_PATCH_BYTES);
        assert!(diff.patch_truncated);
        assert_eq!(diff.files[0].additions, 30_000);
    }

    fn sparse_file(path: &Path, bytes: u64) {
        std::fs::File::create(path).unwrap().set_len(bytes).unwrap();
    }

    #[tokio::test]
    async fn snapshot_index_is_kept_and_follows_ignore_changes() {
        let (dir, repo) = init_repo().await;
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        repo.git(&["add", "."]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "init"]).await.unwrap();
        std::fs::write(dir.path().join("notes.scratch"), "untracked\n").unwrap();
        let first = repo.snapshot("first").await.unwrap();
        assert!(repo.git_dir().await.unwrap().join(SNAPSHOT_INDEX).exists());
        assert_eq!(repo.git(&["show", &format!("{first}:notes.scratch")]).await.unwrap(), "untracked");

        // Unchanged files give the same tree from the kept index.
        let again = repo.snapshot("again").await.unwrap();
        let tree = |commit: &str| format!("{commit}^{{tree}}");
        assert_eq!(repo.git(&["rev-parse", &tree(&first)]).await.unwrap(), repo.git(&["rev-parse", &tree(&again)]).await.unwrap());

        // A file ignored since the last snapshot leaves it, and a force-added
        // ignored file joins it.
        std::fs::write(dir.path().join(".git/info/exclude"), "*.scratch\n").unwrap();
        std::fs::write(dir.path().join("kept.scratch"), "forced\n").unwrap();
        repo.git(&["add", "-f", "kept.scratch"]).await.unwrap();
        let later = repo.snapshot("later").await.unwrap();
        let files = repo.git(&["ls-tree", "-r", "--name-only", &later]).await.unwrap();
        assert_eq!(files.lines().collect::<Vec<_>>(), ["a.txt", "kept.scratch"], "{files}");
    }

    #[tokio::test]
    async fn subfolder_snapshots_keep_their_own_index_and_take_outside_entries_from_the_user_index() {
        let (dir, root) = init_repo().await;
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("outside.txt"), "v1\n").unwrap();
        std::fs::write(dir.path().join("gone.txt"), "x\n").unwrap();
        std::fs::write(dir.path().join("sub/in.txt"), "a\n").unwrap();
        root.git(&["add", "."]).await.unwrap();
        root.git(&["commit", "-q", "-m", "init"]).await.unwrap();
        let sub = Repo::new(dir.path().join("sub"));
        let git_dir = root.git_dir().await.unwrap();

        std::fs::write(dir.path().join("sub/new.txt"), "untracked\n").unwrap();
        sub.snapshot("first").await.unwrap();
        let kept = git_dir.join(snapshot_index_name("sub/"));
        assert!(kept.exists() && !git_dir.join(SNAPSHOT_INDEX).exists(), "a subfolder keeps its own index");

        // The user stages a change, adds a file and removes one outside the
        // folder; unstaged edits outside it stay out, as with a fresh seed.
        std::fs::write(dir.path().join("outside.txt"), "v2\n").unwrap();
        std::fs::write(dir.path().join("added.txt"), "new\n").unwrap();
        root.git(&["add", "outside.txt", "added.txt"]).await.unwrap();
        root.git(&["rm", "-q", "gone.txt"]).await.unwrap();
        std::fs::write(dir.path().join("outside.txt"), "v3 unstaged\n").unwrap();
        std::fs::write(dir.path().join("sub/in.txt"), "b\n").unwrap();
        let second = sub.snapshot("second").await.unwrap();
        let at = |path: &str| format!("{second}:{path}");
        assert_eq!(root.git(&["show", &at("outside.txt")]).await.unwrap(), "v2");
        assert_eq!(root.git(&["show", &at("added.txt")]).await.unwrap(), "new");
        assert_eq!(root.git(&["show", &at("sub/in.txt")]).await.unwrap(), "b");
        assert_eq!(root.git(&["show", &at("sub/new.txt")]).await.unwrap(), "untracked");
        assert!(root.git(&["show", &at("gone.txt")]).await.is_err());

        // A root snapshot sees the working tree everywhere.
        let full = root.snapshot("root").await.unwrap();
        assert_eq!(root.git(&["show", &format!("{full}:outside.txt")]).await.unwrap(), "v3 unstaged");
        assert!(git_dir.join(SNAPSHOT_INDEX).exists());
    }

    #[tokio::test]
    async fn a_broken_kept_index_falls_back_to_a_fresh_seed() {
        let (dir, repo) = init_repo().await;
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        repo.git(&["add", "."]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "init"]).await.unwrap();
        repo.snapshot("first").await.unwrap();
        let kept = repo.git_dir().await.unwrap().join(SNAPSHOT_INDEX);
        std::fs::write(&kept, "not an index").unwrap();
        let second = repo.snapshot("second").await.unwrap();
        assert_eq!(repo.git(&["show", &format!("{second}:a.txt")]).await.unwrap(), "one");
        // The next snapshot reuses the index the fallback kept.
        assert_ne!(std::fs::read(&kept).unwrap(), b"not an index");
        repo.snapshot("third").await.unwrap();
    }

    #[tokio::test]
    async fn large_untracked_paths_are_left_out_named_and_kept_on_restore() {
        let (dir, repo) = init_repo().await;
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        repo.git(&["add", "."]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "init"]).await.unwrap();
        let big = dir.path().join("previews");
        std::fs::create_dir_all(big.join("run")).unwrap();
        sparse_file(&big.join("run/one.mov"), SNAPSHOT_SKIP_BYTES / 2);
        sparse_file(&big.join("two.mov"), SNAPSHOT_SKIP_BYTES / 2);
        std::fs::write(dir.path().join("grows.bin"), "small").unwrap();
        let before = repo.snapshot("before").await.unwrap();
        let files = repo.git(&["ls-tree", "-r", "--name-only", &before]).await.unwrap();
        assert_eq!(files.lines().collect::<Vec<_>>(), ["a.txt", "grows.bin"], "{files}");
        assert_eq!(repo.snapshot_skipped(&before).await.unwrap(), ["previews"]);

        // A file that grows past the limit leaves the next snapshot.
        sparse_file(&dir.path().join("grows.bin"), SNAPSHOT_SKIP_BYTES);
        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        let after = repo.snapshot("after").await.unwrap();
        let mut skipped = repo.snapshot_skipped(&after).await.unwrap();
        skipped.sort();
        assert_eq!(skipped, ["grows.bin", "previews"]);
        let message = repo.git(&["log", "-1", "--format=%B", &after]).await.unwrap();
        assert!(message.contains("Kybern-Skipped: previews (100 MB or more, not snapshotted)"), "{message}");

        // Restoring keeps what was left out, and large untracked files made
        // since, instead of cleaning them away; snapshotted files come back.
        sparse_file(&dir.path().join("fresh.mov"), SNAPSHOT_SKIP_BYTES);
        std::fs::write(dir.path().join("fresh.txt"), "small").unwrap();
        repo.restore(&before).await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "one\n");
        assert_eq!(std::fs::read_to_string(dir.path().join("grows.bin")).unwrap(), "small");
        assert!(big.join("run/one.mov").exists() && big.join("two.mov").exists());
        assert!(dir.path().join("fresh.mov").exists());
        assert!(!dir.path().join("fresh.txt").exists());
    }

    /// Times snapshots of a real repository: `KYBERN_SNAPSHOT_BENCH=<repo>
    /// cargo test -p kybern-git snapshot_bench -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn snapshot_bench() {
        let Ok(path) = std::env::var("KYBERN_SNAPSHOT_BENCH") else { return };
        let repo = Repo::new(&path);
        let objects = |label: &str| {
            let out = std::process::Command::new("du").args(["-sk", ".git/objects"]).current_dir(&path).output().unwrap();
            eprintln!("{label}: .git/objects {}", String::from_utf8_lossy(&out.stdout).trim());
        };
        objects("start");
        for run in ["first", "unchanged", "one edit"] {
            if run == "one edit" {
                std::fs::write(std::path::Path::new(&path).join("bench-edit.txt"), uuid::Uuid::new_v4().to_string()).unwrap();
            }
            let started = std::time::Instant::now();
            let commit = repo.snapshot("bench").await.unwrap();
            eprintln!("{run}: {} ms ({commit})", started.elapsed().as_millis());
        }
        objects("end");
    }

    #[tokio::test]
    async fn snapshot_works_without_head() {
        let (dir, repo) = init_repo().await;
        std::fs::write(dir.path().join("x"), "1").unwrap();
        let c = repo.snapshot("s").await.unwrap();
        assert_eq!(c.len(), 40);
    }

    #[tokio::test]
    async fn snapshot_preserves_staged_and_unstaged_index_state() {
        let (dir, repo) = init_repo().await;
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "committed\n").unwrap();
        repo.git(&["add", "."]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "init"]).await.unwrap();

        std::fs::write(&path, "staged value\n").unwrap();
        repo.git(&["add", "a.txt"]).await.unwrap();
        std::fs::write(&path, "working value\n").unwrap();
        let status_before = repo.git(&["status", "--porcelain=v1"]).await.unwrap();

        let snapshot = repo.snapshot("snapshot").await.unwrap();

        assert_eq!(repo.git(&["status", "--porcelain=v1"]).await.unwrap(), status_before);
        assert_eq!(repo.git(&["show", &format!("{snapshot}:a.txt")]).await.unwrap(), "working value");
    }

    #[tokio::test]
    async fn state_queries_report_clean_ancestry_and_diffstat() {
        let (dir, repo) = init_repo().await;
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        repo.git(&["add", "."]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "init"]).await.unwrap();
        let base = repo.rev_parse("HEAD").await.unwrap();
        assert!(repo.is_clean().await.unwrap());

        std::fs::write(dir.path().join("new.txt"), "hi\n").unwrap();
        assert!(!repo.is_clean().await.unwrap(), "untracked files make the tree dirty");
        repo.git(&["add", "."]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "more"]).await.unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        repo.git(&["commit", "-q", "-am", "edit"]).await.unwrap();
        let head = repo.rev_parse("HEAD").await.unwrap();

        assert!(repo.is_ancestor(&base, &head).await.unwrap());
        assert!(!repo.is_ancestor(&head, &base).await.unwrap());
        assert!(repo.is_ancestor(&head, &head).await.unwrap());
        assert!(repo.is_ancestor("does-not-exist", &head).await.is_err());
        assert_eq!(repo.diffstat(&base, &head).await.unwrap(), DiffStat { files: 2, additions: 3, deletions: 0 });
        assert_eq!(repo.diffstat(&head, &head).await.unwrap(), DiffStat::default());
    }

    #[tokio::test]
    async fn fetch_lists_new_remote_branches_and_switch_tracks_them() {
        let (upstream_dir, upstream) = init_repo().await;
        std::fs::write(upstream_dir.path().join("a.txt"), "one\n").unwrap();
        upstream.git(&["add", "."]).await.unwrap();
        upstream.git(&["commit", "-q", "-m", "init"]).await.unwrap();
        let clone_dir = tempfile::tempdir().unwrap();
        let clone_path = clone_dir.path().join("clone");
        upstream.git(&["clone", "-q", &upstream_dir.path().to_string_lossy(), &clone_path.to_string_lossy()]).await.unwrap();
        let clone = Repo::new(&clone_path);
        assert!(clone.remote_branches().await.unwrap().iter().all(|b| !b.name.ends_with("/HEAD")));

        upstream.git(&["branch", "feature/new"]).await.unwrap();
        assert!(clone.remote_branches().await.unwrap().iter().all(|b| b.name != "origin/feature/new"));
        clone.fetch_all().await.unwrap();
        assert!(clone.remote_branches().await.unwrap().iter().any(|b| b.name == "origin/feature/new"));

        let tree = clone_dir.path().join("tree");
        clone.worktree_add(&tree, "kybern/child", Some("origin/feature/new")).await.unwrap();
        assert!(
            Repo::new(&tree).git(&["rev-parse", "--abbrev-ref", "@{upstream}"]).await.is_err(),
            "the worktree's branch has no upstream"
        );

        clone.switch("origin/feature/new").await.unwrap();
        assert_eq!(clone.current_branch().await.as_deref(), Some("feature/new"));
        assert_eq!(clone.git(&["rev-parse", "--abbrev-ref", "@{upstream}"]).await.unwrap(), "origin/feature/new");
    }

    #[tokio::test]
    async fn delete_branch_refuses_unmerged_unless_forced() {
        let (dir, repo) = init_repo().await;
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        repo.git(&["add", "."]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "init"]).await.unwrap();
        repo.git(&["checkout", "-q", "-b", "feature"]).await.unwrap();
        std::fs::write(dir.path().join("b.txt"), "two\n").unwrap();
        repo.git(&["add", "."]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "feature work"]).await.unwrap();
        repo.git(&["checkout", "-q", "-"]).await.unwrap();

        assert!(repo.delete_branch("feature", false).await.is_err());
        repo.git(&["merge", "-q", "--no-ff", "-m", "merge", "feature"]).await.unwrap();
        repo.delete_branch("feature", false).await.unwrap();
        assert!(repo.branches().await.unwrap().iter().all(|branch| branch.name != "feature"));

        repo.git(&["branch", "scratch"]).await.unwrap();
        repo.delete_branch("scratch", true).await.unwrap();
    }

    #[tokio::test]
    async fn worktree_seeded_from_a_snapshot_is_clean_and_carries_uncommitted_files() {
        let (dir, repo) = init_repo().await;
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        repo.git(&["add", "."]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "init"]).await.unwrap();
        std::fs::write(dir.path().join("wip.txt"), "uncommitted\n").unwrap();
        let snapshot = repo.snapshot("base").await.unwrap();

        let parent = tempfile::tempdir().unwrap();
        let worktree = parent.path().join("child");
        repo.worktree_add(&worktree, "kybern/child", Some(&snapshot)).await.unwrap();
        let child = Repo::new(&worktree);
        assert_eq!(std::fs::read_to_string(worktree.join("wip.txt")).unwrap(), "uncommitted\n");
        assert!(child.is_clean().await.unwrap());
        assert_eq!(child.rev_parse("HEAD").await.unwrap(), snapshot);
        // No commits beyond the base yet: the branch tip is an ancestor of the base.
        assert!(repo.is_ancestor("kybern/child", &snapshot).await.unwrap());
        std::fs::write(worktree.join("scratch"), "x").unwrap();
        assert!(!child.is_clean().await.unwrap());
        repo.worktree_remove(&worktree, true).await.unwrap();
        // The snapshot is a dangling commit, so `-d` sees an unmerged branch.
        assert!(repo.delete_branch("kybern/child", false).await.is_err());
        repo.delete_branch("kybern/child", true).await.unwrap();
    }

    #[tokio::test]
    async fn worktree_prune_forgets_a_deleted_worktree_so_its_branch_can_be_judged() {
        let (dir, repo) = init_repo().await;
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        repo.git(&["add", "."]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "init"]).await.unwrap();
        let wt = tempfile::tempdir().unwrap();
        let path = wt.path().join("child");
        repo.worktree_add(&path, "kybern/x", None).await.unwrap();
        std::fs::remove_dir_all(&path).unwrap();
        assert!(repo.git(&["worktree", "list", "--porcelain"]).await.unwrap().contains("kybern/x"));
        repo.worktree_prune().await.unwrap();
        assert!(!repo.git(&["worktree", "list", "--porcelain"]).await.unwrap().contains("kybern/x"));
        // The branch survives and, with no commits of its own, is an ancestor of HEAD.
        assert!(repo.is_ancestor("kybern/x", "HEAD").await.unwrap());
        repo.delete_branch("kybern/x", false).await.unwrap();
    }

    async fn committed_repo() -> (tempfile::TempDir, Repo) {
        let (dir, repo) = init_repo().await;
        repo.git(&["symbolic-ref", "HEAD", "refs/heads/main"]).await.unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        repo.git(&["add", "."]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "init"]).await.unwrap();
        (dir, repo)
    }

    #[tokio::test]
    async fn status_log_and_diff_describe_the_working_tree() {
        let (dir, repo) = committed_repo().await;
        assert!(repo.status_entries().await.unwrap().is_empty());
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/new.txt"), "n\n").unwrap();
        let entries = repo.status_entries().await.unwrap();
        assert!(entries.contains(&StatusEntry { code: " M".into(), path: "a.txt".into() }), "{entries:?}");
        assert!(entries.contains(&StatusEntry { code: "??".into(), path: "sub/new.txt".into() }), "{entries:?}");

        let diff = repo.diff_uncommitted(false, None, 1024).await.unwrap();
        assert_eq!(diff.files, vec![WorkingDiffFile { path: "a.txt".into(), additions: 1, deletions: 0, binary: false }]);
        assert!(diff.patch.contains("+two"));
        assert!(repo.diff_uncommitted(true, None, 1024).await.unwrap().files.is_empty(), "nothing is staged");
        let tiny = repo.diff_uncommitted(false, Some("a.txt"), 10).await.unwrap();
        assert!(tiny.patch_truncated && tiny.patch.len() <= 10);

        let log = repo.log(5).await.unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!((log[0].subject.as_str(), log[0].author.as_str()), ("init", "t"));
        let (_empty_dir, empty) = init_repo().await;
        assert!(empty.log(5).await.unwrap().is_empty(), "no commits yet");
    }

    #[tokio::test]
    async fn commit_takes_only_the_named_paths_and_uses_the_configured_identity() {
        let (dir, repo) = committed_repo().await;
        std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "new\n").unwrap();
        std::fs::write(dir.path().join("c.txt"), "other\n").unwrap();
        let first = repo.commit("only b", Some(&["b.txt".to_string()])).await.unwrap();
        assert_eq!(repo.git(&["show", "--name-only", "--format=%an <%ae>", &first]).await.unwrap(), "t <t@t>\n\nb.txt");
        let left: Vec<_> = repo.status_entries().await.unwrap().into_iter().map(|entry| entry.path).collect();
        assert_eq!(left, vec!["a.txt", "c.txt"]);
        repo.commit("everything else", None).await.unwrap();
        assert!(repo.status_entries().await.unwrap().is_empty());
        assert!(repo.commit("nothing", None).await.is_err());
    }

    #[tokio::test]
    async fn identity_reads_the_configured_values_and_notices_when_missing() {
        let (dir, repo) = committed_repo().await;
        assert_eq!(repo.identity().await, (Some("t".into()), Some("t@t".into())));
        let home = tempfile::tempdir().unwrap();
        let isolated = Repo::new(dir.path()).with_env([
            ("HOME".to_string(), home.path().to_string_lossy().into_owned()),
            ("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string()),
            ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
        ]);
        repo.git(&["config", "--unset", "user.name"]).await.unwrap();
        repo.git(&["config", "--unset", "user.email"]).await.unwrap();
        assert_eq!(isolated.identity().await, (None, None));
    }

    #[tokio::test]
    async fn branch_helpers_validate_names_and_spot_the_default_branch() {
        let (_dir, repo) = committed_repo().await;
        assert!(repo.is_valid_branch_name("feature/x").await);
        for bad in ["-x", "a b", "a..b", "", "x.lock"] {
            assert!(!repo.is_valid_branch_name(bad).await, "{bad:?}");
        }
        assert!(repo.is_default_branch("main").await && repo.is_default_branch("master").await);
        assert!(!repo.is_default_branch("feature").await);
        repo.create_branch("feature", None).await.unwrap();
        assert_eq!(repo.current_branch().await.as_deref(), Some("feature"));
        assert!(repo.create_branch("feature", None).await.is_err(), "it exists");
        repo.create_branch("from-main", Some("main")).await.unwrap();
        assert_eq!(repo.current_branch().await.as_deref(), Some("from-main"));
    }

    #[tokio::test]
    async fn merge_no_ff_makes_a_merge_commit_and_aborts_on_conflict() {
        let (dir, repo) = committed_repo().await;
        repo.git(&["checkout", "-q", "-b", "clean"]).await.unwrap();
        std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        repo.commit("add b", None).await.unwrap();
        repo.git(&["checkout", "-q", "-b", "clash", "main"]).await.unwrap();
        std::fs::write(dir.path().join("a.txt"), "theirs\n").unwrap();
        repo.commit("their a", None).await.unwrap();
        repo.git(&["checkout", "-q", "main"]).await.unwrap();
        std::fs::write(dir.path().join("a.txt"), "ours\n").unwrap();
        repo.commit("our a", None).await.unwrap();

        let MergeOutcome::Conflict { paths } = repo.merge_no_ff("clash", "merge clash").await.unwrap() else {
            panic!("expected a conflict")
        };
        assert_eq!(paths, vec!["a.txt"]);
        assert!(repo.rev_parse("MERGE_HEAD").await.is_err(), "the merge was aborted");
        assert!(repo.is_clean().await.unwrap());
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "ours\n");

        let MergeOutcome::Merged { commit } = repo.merge_no_ff("clean", "merge clean").await.unwrap() else { panic!("expected a merge") };
        assert_eq!(repo.git(&["rev-list", "--parents", "-n", "1", &commit]).await.unwrap().split_whitespace().count(), 3, "two parents");
        assert_eq!(repo.git(&["log", "-1", "--format=%s"]).await.unwrap(), "merge clean");
        assert_eq!(repo.merge_no_ff("clean", "again").await.unwrap(), MergeOutcome::AlreadyUpToDate);
        assert!(repo.merge_no_ff("does-not-exist", "x").await.is_err());
    }

    #[tokio::test]
    async fn push_sets_the_upstream_and_counts_commits_ahead() {
        let remote = tempfile::tempdir().unwrap();
        let bare = Repo::new(remote.path());
        bare.git(&["init", "-q", "--bare"]).await.unwrap();
        let (dir, repo) = committed_repo().await;
        assert_eq!(repo.push_remote("main").await, None, "no remote yet");
        repo.git(&["remote", "add", "origin", &remote.path().to_string_lossy()]).await.unwrap();
        assert_eq!(repo.push_remote("main").await.as_deref(), Some("origin"));
        assert_eq!(repo.commits_ahead("main", "origin").await.unwrap(), 1);
        assert_eq!(repo.upstream("main").await, None);
        repo.push_upstream("origin", "main").await.unwrap();
        assert_eq!(repo.upstream("main").await.as_deref(), Some("origin/main"));
        assert_eq!(repo.commits_ahead("main", "origin").await.unwrap(), 0);
        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        repo.commit("more", None).await.unwrap();
        assert_eq!(repo.ahead_behind("main").await, Some((1, 0)));
        assert_eq!(repo.commits_ahead("main", "origin").await.unwrap(), 1);
        // A diverged push is rejected, never forced.
        repo.git(&["reset", "-q", "--hard", "HEAD~1"]).await.unwrap();
        std::fs::write(dir.path().join("a.txt"), "diverged\n").unwrap();
        repo.commit("diverge", None).await.unwrap();
        repo.push_upstream("origin", "main").await.unwrap();
        repo.git(&["reset", "-q", "--hard", "HEAD~1"]).await.unwrap();
        std::fs::write(dir.path().join("a.txt"), "other\n").unwrap();
        repo.commit("other", None).await.unwrap();
        assert!(repo.push_upstream("origin", "main").await.is_err());
    }
}

#[cfg(test)]
mod review_adversarial {
    use super::*;

    async fn git(dir: &Path, args: &[&str]) -> String {
        let out = tokio::process::Command::new("git").current_dir(dir).args(args).output().await.unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A thread whose cwd is a subfolder of the repository: the kept snapshot
    /// index never refreshes entries outside the subfolder.
    #[tokio::test]
    async fn subfolder_cwd_snapshot_keeps_stale_outside_entries_and_restore_writes_them_back() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]).await;
        git(root, &["config", "user.email", "t@t"]).await;
        git(root, &["config", "user.name", "t"]).await;
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("outside.txt"), "v1\n").unwrap();
        std::fs::write(root.join("sub/in.txt"), "a\n").unwrap();
        git(root, &["add", "."]).await;
        git(root, &["commit", "-q", "-m", "init"]).await;

        let repo = Repo::new(root.join("sub").to_string_lossy().as_ref());
        let _first = repo.snapshot("first").await.unwrap();
        // The user commits a change outside the thread's folder.
        std::fs::write(root.join("outside.txt"), "v2\n").unwrap();
        git(root, &["commit", "-q", "-am", "outside v2"]).await;
        let second = repo.snapshot("second").await.unwrap();
        let shown = git(root, &["show", &format!("{second}:outside.txt")]).await;
        eprintln!("second snapshot outside.txt = {shown:?} (working tree and HEAD are v2)");
        std::fs::write(root.join("sub/in.txt"), "b\n").unwrap();
        repo.restore(&second).await.unwrap();
        let after = std::fs::read_to_string(root.join("outside.txt")).unwrap();
        eprintln!("outside.txt after restoring the second snapshot = {after:?}");
        assert_eq!((shown.as_str(), after.as_str()), ("v2", "v2\n"), "restore must not write back stale content outside the folder");
    }

    /// A tracked file that matches .gitignore is deleted from disk (deletion not staged).
    #[tokio::test]
    async fn deleting_a_tracked_ignored_file_breaks_later_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]).await;
        git(root, &["config", "user.email", "t@t"]).await;
        git(root, &["config", "user.name", "t"]).await;
        std::fs::write(root.join(".gitignore"), "*.local\n").unwrap();
        std::fs::write(root.join("config.local"), "x\n").unwrap();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        git(root, &["add", ".gitignore", "a.txt"]).await;
        git(root, &["add", "-f", "config.local"]).await;
        git(root, &["commit", "-q", "-m", "init"]).await;
        let repo = Repo::new(root.to_string_lossy().as_ref());
        repo.snapshot("first").await.unwrap();
        std::fs::remove_file(root.join("config.local")).unwrap();
        let second = repo.snapshot("second").await;
        eprintln!("second: {second:?}");
        let third = repo.snapshot("third").await;
        eprintln!("third: {third:?}");
        assert!(second.is_ok() && third.is_ok());
    }
}

#[cfg(test)]
mod group_tests {
    use super::*;

    async fn init_repo() -> (tempfile::TempDir, Repo) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::new(dir.path());
        repo.git(&["init", "-q"]).await.unwrap();
        repo.git(&["config", "core.autocrlf", "false"]).await.unwrap();
        repo.git(&["config", "user.email", "t@t"]).await.unwrap();
        repo.git(&["config", "user.name", "t"]).await.unwrap();
        (dir, repo)
    }

    /// A group folder that is itself a repository, with a nested member
    /// repository: snapshots, restores and status of the folder leave the
    /// member alone.
    #[tokio::test]
    async fn excluded_nested_repository_is_never_snapshotted_or_restored() {
        let (dir, outer) = init_repo().await;
        let root = dir.path();
        std::fs::write(root.join("README.md"), "outer\n").unwrap();
        outer.git(&["add", "."]).await.unwrap();
        outer.git(&["commit", "-q", "-m", "init"]).await.unwrap();
        let api = root.join("api");
        std::fs::create_dir(&api).unwrap();
        let inner = Repo::new(&api);
        inner.git(&["init", "-q"]).await.unwrap();
        std::fs::write(api.join("x.rs"), "fn x() {}\n").unwrap();
        let exclude = vec!["api".to_string()];

        assert_eq!(outer.status_porcelain(&exclude).await.unwrap(), Vec::<String>::new());
        assert_eq!(outer.status_porcelain(&[]).await.unwrap(), ["?? api/"]);
        let before = outer.snapshot_excluding("before", &exclude).await.unwrap();
        std::fs::write(root.join("README.md"), "outer two\n").unwrap();
        std::fs::write(root.join("notes.txt"), "new\n").unwrap();
        std::fs::write(api.join("x.rs"), "fn x() { 1 }\n").unwrap();
        let after = outer.snapshot_excluding("after", &exclude).await.unwrap();
        let diff = outer.diff(&before, &after).await.unwrap();
        let paths: Vec<_> = diff.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["README.md", "notes.txt"], "the nested repository stays out of the folder's snapshots");

        outer.restore_excluding(&before, &exclude).await.unwrap();
        assert_eq!(std::fs::read_to_string(root.join("README.md")).unwrap(), "outer\n");
        assert!(!root.join("notes.txt").exists());
        assert_eq!(std::fs::read_to_string(api.join("x.rs")).unwrap(), "fn x() { 1 }\n", "restore leaves the member alone");
        assert!(api.join(".git").exists());
    }

    #[tokio::test]
    async fn prefixed_diff_names_files_inside_the_group_folder() {
        let (dir, repo) = init_repo().await;
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        repo.git(&["add", "."]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "init"]).await.unwrap();
        let before = repo.snapshot("before").await.unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::rename(dir.path().join("a.txt"), dir.path().join("b.txt")).unwrap();
        let after = repo.snapshot("after").await.unwrap();
        let diff = repo.diff_prefixed(&before, &after, true, None, Some("services/api")).await.unwrap();
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].path, "services/api/b.txt");
        assert_eq!(diff.files[0].old_path.as_deref(), Some("services/api/a.txt"));
        assert_eq!(diff.files[0].additions, 1);
        assert!(diff.patch.contains("diff --git a/services/api/a.txt b/services/api/b.txt"), "{}", diff.patch);
        let only = repo.diff_prefixed(&before, &after, false, Some("b.txt"), Some("services/api")).await.unwrap();
        assert_eq!(only.files.len(), 1);
        let plain = repo.diff_prefixed(&before, &after, true, None, Some(".")).await.unwrap();
        assert_eq!(plain.files[0].path, "b.txt");
    }

    #[tokio::test]
    async fn merge_tree_check_finds_conflicts_without_touching_the_tree() {
        let (dir, repo) = init_repo().await;
        std::fs::write(dir.path().join("a.txt"), "base\n").unwrap();
        repo.git(&["add", "."]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "init"]).await.unwrap();
        let main = repo.current_branch().await.unwrap();
        repo.git(&["switch", "-q", "-c", "side"]).await.unwrap();
        std::fs::write(dir.path().join("a.txt"), "side\n").unwrap();
        repo.git(&["commit", "-q", "-am", "side"]).await.unwrap();
        repo.git(&["switch", "-q", "-c", "other", &main]).await.unwrap();
        std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        repo.git(&["add", "."]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "other"]).await.unwrap();
        assert_eq!(repo.merge_tree_check("other", "side").await.unwrap(), MergeCheck::Clean);
        repo.git(&["switch", "-q", &main]).await.unwrap();
        std::fs::write(dir.path().join("a.txt"), "main\n").unwrap();
        repo.git(&["commit", "-q", "-am", "main"]).await.unwrap();
        assert_eq!(repo.merge_tree_check("HEAD", "side").await.unwrap(), MergeCheck::Conflicts(vec!["a.txt".into()]));
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "main\n");
    }

    #[tokio::test]
    async fn log_range_lists_paths_with_renames_and_deletes() {
        let (dir, repo) = init_repo().await;
        let w = |name: &str, body: &str| std::fs::write(dir.path().join(name), body).unwrap();
        w("keep.txt", "k\n");
        w("gone.txt", "g\n");
        w("old.txt", "a long enough body to be detected as a rename\nsecond line\nthird line\n");
        repo.git(&["add", "."]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "init"]).await.unwrap();
        let base = repo.rev_parse("HEAD").await.unwrap();

        std::fs::remove_file(dir.path().join("gone.txt")).unwrap();
        repo.git(&["add", "-A"]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "Remove gone"]).await.unwrap();
        repo.git(&["mv", "old.txt", "new.txt"]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "Rename old"]).await.unwrap();
        w("keep.txt", "k\nmore\n");
        w("added.txt", "x\n");
        repo.git(&["add", "-A"]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "Modify and add"]).await.unwrap();

        let log = repo.log_range(&base, "HEAD", 10).await.unwrap();
        let subjects: Vec<_> = log.iter().map(|c| c.entry.subject.as_str()).collect();
        assert_eq!(subjects, ["Modify and add", "Rename old", "Remove gone"]);
        assert_eq!(log[2].files, [PathChange { status: FileStatus::Deleted, path: "gone.txt".into(), old_path: None }]);
        assert_eq!(log[1].files, [PathChange { status: FileStatus::Renamed, path: "new.txt".into(), old_path: Some("old.txt".into()) }]);
        assert!(log[0].files.iter().any(|f| f.status == FileStatus::Modified && f.path == "keep.txt"));
        assert!(log[0].files.iter().any(|f| f.status == FileStatus::Added && f.path == "added.txt"));
        assert_eq!(log[0].entry.author, "t");

        assert_eq!(repo.log_range(&base, "HEAD", 2).await.unwrap().len(), 2);
        assert!(repo.log_range("HEAD", "HEAD", 5).await.unwrap().is_empty());

        let changes = repo.changed_paths(&base, "HEAD").await.unwrap();
        assert!(changes.iter().any(|c| c.status == FileStatus::Deleted && c.path == "gone.txt"));
        assert!(changes.iter().any(|c| c.status == FileStatus::Renamed && c.old_path.as_deref() == Some("old.txt")));
    }

    #[tokio::test]
    async fn log_range_after_history_rewrite_is_detectable() {
        let (dir, repo) = init_repo().await;
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        repo.git(&["add", "."]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "first"]).await.unwrap();
        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        repo.git(&["commit", "-q", "-am", "second"]).await.unwrap();
        let old_head = repo.rev_parse("HEAD").await.unwrap();

        // Rewrite: replace the second commit with a different one.
        repo.git(&["reset", "-q", "--hard", "HEAD~1"]).await.unwrap();
        std::fs::write(dir.path().join("a.txt"), "three\n").unwrap();
        repo.git(&["commit", "-q", "-am", "second again"]).await.unwrap();

        assert!(!repo.is_ancestor(&old_head, "HEAD").await.unwrap());
        let recent = repo.log_range("HEAD~1", "HEAD", 5).await.unwrap();
        assert_eq!(recent.len(), 1);
        // An empty `from` lists `to`'s own history: the fallback for a rewrite.
        let tail = repo.log_range("", "HEAD", 5).await.unwrap();
        assert_eq!(tail.iter().map(|c| c.entry.subject.as_str()).collect::<Vec<_>>(), ["second again", "first"]);
        assert_eq!(repo.count_range("", "HEAD").await.unwrap(), 2);
        assert_eq!(repo.count_range("HEAD~1", "HEAD").await.unwrap(), 1);
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn the_verbatim_prefix_is_dropped_from_drive_and_unc_paths_only() {
        let plain = |text: &str| simplify_path(Path::new(text)).to_string_lossy().into_owned();
        assert_eq!(plain(r"\\?\D:\kybern\repo"), r"D:\kybern\repo");
        assert_eq!(plain(r"\\?\UNC\box\share\repo"), r"\\box\share\repo");
        assert_eq!(plain(r"\\?\Volume{1234}\repo"), r"\\?\Volume{1234}\repo");
        assert_eq!(plain(r"D:\kybern\repo"), r"D:\kybern\repo");
        assert_eq!(plain("/Users/me/repo"), "/Users/me/repo");
    }

    #[test]
    fn git_s_ownership_refusal_becomes_a_message_with_the_command_to_run() {
        let stderr = "fatal: detected dubious ownership in repository at 'D:/kybern/repo'\nTo add an exception for this directory, call:\n\n\tgit config --global --add safe.directory D:/kybern/repo\n";
        let message = dubious_ownership(stderr, Path::new(r"\\?\D:\kybern\repo")).unwrap();
        assert_eq!(
            message,
            "Git refuses this folder because another user owns it. Run: git config --global --add safe.directory D:/kybern/repo"
        );
        let unnamed = dubious_ownership("fatal: detected dubious ownership", Path::new(r"\\?\D:\x")).unwrap();
        assert!(unnamed.ends_with("safe.directory D:/x"), "{unnamed}");
        assert_eq!(dubious_ownership("fatal: not a git repository", Path::new("/x")), None);
    }
}
