//! Git status and GitHub pull requests through the user's `gh` CLI, which
//! already holds their credentials.

use kybern_protocol::process::{NoWindow, SpawnError};
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use kybern_git::{Repo, RepoState};
use kybern_protocol::methods::*;
use kybern_protocol::rpc::{Problem, ProblemAction, ProblemKind};
use serde_json::Value;
use tokio::process::Command;

/// Spawn `program` (stdin as `stdin` says, output piped). When the bare `gh` isn't on PATH,
/// `gh_locate::refresh` looks again where a new terminal would, and the spawn is retried once.
/// A failure is a `SpawnError` that keeps its cause ("run gh: program not found"), which
/// `friendly` and `problem::problem_from` read by type.
pub(crate) fn spawn(cwd: &Path, program: &str, args: &[&str], stdin: std::process::Stdio) -> Result<tokio::process::Child> {
    let mut stdin = Some(stdin);
    let mut start = || {
        Command::new(program)
            .no_window()
            .current_dir(cwd)
            .args(args)
            .stdin(stdin.take().unwrap_or_else(std::process::Stdio::null))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
    };
    let child = match start() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && crate::gh_locate::refresh(program) => start(),
        other => other,
    };
    child.map_err(|error| SpawnError::new(program, error).into())
}

/// `args` for an error message: each one cut to a few words, so a failed `gh pr create` doesn't
/// quote the whole title and body of the pull request.
fn shown_args(args: &[&str]) -> String {
    const MAX: usize = 60;
    args.iter()
        .map(|arg| match arg.char_indices().nth(MAX) {
            Some((end, _)) => format!("{}…", &arg[..end]),
            None => arg.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) async fn run(cwd: &Path, program: &str, args: &[&str]) -> Result<String> {
    let out = spawn(cwd, program, args, std::process::Stdio::null())?.wait_with_output().await.with_context(|| format!("run {program}"))?;
    if !out.status.success() {
        return Err(anyhow!("{program} {}: {}", shown_args(args), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// `run`, with `input` written to the program's standard input.
pub(crate) async fn run_stdin(cwd: &Path, program: &str, args: &[&str], input: &str) -> Result<String> {
    let mut child = spawn(cwd, program, args, std::process::Stdio::piped())?;
    let mut stdin = child.stdin.take().context("program stdin")?;
    tokio::io::AsyncWriteExt::write_all(&mut stdin, input.as_bytes()).await.with_context(|| format!("write to {program}"))?;
    drop(stdin);
    let out = child.wait_with_output().await.with_context(|| format!("run {program}"))?;
    if !out.status.success() {
        return Err(anyhow!("{program} {}: {}", shown_args(args), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Why a `gh` call failed, as far as Kybern can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GhErrorKind {
    GhMissing,
    GhAuth,
    Forbidden,
    NotFound,
    NoRemote,
}

/// The `Problem` a client shows for a `gh` failure of `kind`; `detail` is the full error chain.
pub(crate) fn gh_problem(kind: GhErrorKind, detail: String) -> Problem {
    let action = |id: &str, label: &str, params: Value| Some(ProblemAction { id: id.into(), label: label.into(), params: Some(params) });
    let retry = Some(ProblemAction { id: "retry".into(), label: "Check again".into(), params: None });
    let (problem_kind, subject, title, hint, action, secondary) = match kind {
        GhErrorKind::GhMissing => (
            ProblemKind::ToolMissing,
            "gh",
            "Install GitHub CLI",
            "Kybern uses your own GitHub sign-in through the GitHub CLI (gh). Install it, then check again.",
            action("install_tool", "Install GitHub CLI", serde_json::json!({ "tool": "gh" })),
            retry,
        ),
        GhErrorKind::GhAuth => (
            ProblemKind::SignedOut,
            "gh",
            "Sign in to GitHub",
            "The GitHub CLI (gh) isn't signed in. Sign in, then check again.",
            action("sign_in", "Sign in to GitHub", serde_json::json!({ "provider": "gh" })),
            retry,
        ),
        GhErrorKind::Forbidden => (
            ProblemKind::Permission,
            "gh",
            "GitHub refused this",
            "The signed-in GitHub account can't do this in this repository. Check its access, then try again.",
            None,
            None,
        ),
        GhErrorKind::NotFound => (
            ProblemKind::NotFound,
            "gh",
            "GitHub couldn't find this",
            "Check that origin points at GitHub and that the signed-in account can see the repository.",
            None,
            None,
        ),
        GhErrorKind::NoRemote => (
            ProblemKind::NotFound,
            "github_remote",
            "This project isn't on GitHub",
            "Add a GitHub remote to this repository, then try again.",
            None,
            None,
        ),
    };
    Problem {
        kind: problem_kind,
        subject: Some(subject.into()),
        title: Some(title.into()),
        hint: Some(hint.into()),
        action,
        secondary,
        detail: Some(detail),
        retryable: true,
    }
}

/// The words `classify` gives a missing `gh`.
pub(crate) const GH_MISSING: &str =
    "The GitHub CLI (gh) isn't installed. Install it from cli.github.com, run gh auth login, and try again.";

/// What a `gh` failure means and the words that say what to do next, or `None` when it is none
/// of the cases Kybern knows. A known case with no client recovery has no kind.
pub(crate) fn classify(raw: &str) -> Option<(Option<GhErrorKind>, &'static str)> {
    let lower = raw.to_ascii_lowercase();
    Some(if lower.contains("program not found") || lower.contains("no such file") || lower.contains("run gh:") {
        (Some(GhErrorKind::GhMissing), GH_MISSING)
    } else if lower.contains("gh auth login") || lower.contains("not logged in") || lower.contains("http 401") {
        (Some(GhErrorKind::GhAuth), "gh isn't signed in. Run gh auth login and try again.")
    } else if lower.contains("workflow_dispatch") {
        (None, "This workflow can't be started by hand: its file has no workflow_dispatch trigger on that branch.")
    } else if lower.contains("http 403") {
        (
            Some(GhErrorKind::Forbidden),
            "GitHub refused this: the signed-in gh account can't do it in this repository. Check its access and try again.",
        )
    } else if lower.contains("http 404") {
        (
            Some(GhErrorKind::NotFound),
            "GitHub couldn't find this. Check that origin points at GitHub and that the gh account can see the repository.",
        )
    } else if lower.contains("none of the git remotes") || lower.contains("no git remotes") || lower.contains("not a git repository") {
        (Some(GhErrorKind::NoRemote), "This project has no GitHub remote. Add one, then try again.")
    } else {
        return None;
    })
}

/// `classify` for an error: a `gh` that could not be started is read from its `SpawnError` type,
/// anything else from its text (which older paths and `gh`'s own stderr only have as text).
pub(crate) fn classify_error(error: &anyhow::Error) -> Option<(Option<GhErrorKind>, &'static str)> {
    let spawn = error.chain().find_map(|cause| cause.downcast_ref::<SpawnError>());
    match spawn {
        Some(spawn) if spawn.tool() == "gh" && spawn.kind() == std::io::ErrorKind::NotFound => {
            Some((Some(GhErrorKind::GhMissing), GH_MISSING))
        }
        _ => classify(&format!("{error:#}")),
    }
}

/// `gh`'s failure in words that say what to do next; the original text follows for the details.
/// Text that is already friendly is returned as it is.
pub(crate) fn friendly(error: anyhow::Error) -> anyhow::Error {
    let raw = format!("{error:#}");
    match classify_error(&error) {
        Some((_, lead)) if !raw.starts_with(lead) => anyhow!("{lead} ({})", raw.lines().last().unwrap_or_default().trim()),
        _ => error,
    }
}

/// `friendly`'s message and the `Problem` a client recovers by, with the full error chain as its detail.
pub(crate) fn friendly_problem(error: anyhow::Error) -> (String, Option<Problem>) {
    let raw = format!("{error:#}");
    let problem = classify_error(&error).and_then(|(kind, _)| kind).map(|kind| gh_problem(kind, raw));
    (format!("{:#}", friendly(error)), problem)
}

/// Local branches of a project checkout, most recently committed first.
/// How long `git.branches { fetch }` waits for the remotes before listing what it has.
const BRANCH_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

pub async fn branches(cwd: &Path, fetch: bool, include_remote: bool) -> Result<GitBranchesResult> {
    if !Repo::is_repo(cwd).await {
        return Ok(GitBranchesResult { current: None, branches: Vec::new() });
    }
    let repo = Repo::new(cwd);
    if fetch {
        match tokio::time::timeout(BRANCH_FETCH_TIMEOUT, repo.fetch_all()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::debug!(cwd = %cwd.display(), %error, "branch list: fetch failed"),
            Err(_) => tracing::debug!(cwd = %cwd.display(), "branch list: fetch timed out"),
        }
    }
    let current = repo.current_branch().await;
    let mut branches: Vec<BranchInfo> = repo
        .branches()
        .await?
        .into_iter()
        .map(|b| BranchInfo {
            is_current: current.as_deref() == Some(b.name.as_str()),
            name: b.name,
            upstream: b.upstream,
            committed_at: b.committed_at,
            remote: false,
        })
        .collect();
    if include_remote {
        // `origin/feature` is listed only when there is no local `feature`; `git switch --track` would make that one.
        let local: std::collections::HashSet<String> = branches.iter().map(|b| b.name.clone()).collect();
        let remote = repo.remote_branches().await.unwrap_or_default();
        branches.extend(
            remote.into_iter().filter(|b| b.name.split_once('/').is_none_or(|(_, short)| !local.contains(short))).map(|b| BranchInfo {
                name: b.name,
                is_current: false,
                upstream: None,
                committed_at: b.committed_at,
                remote: true,
            }),
        );
    }
    Ok(GitBranchesResult { current, branches })
}

/// How long "gh isn't there" stands before the next ask runs `gh --version` again. A yes stands
/// for the daemon's life.
const GH_RECHECK: std::time::Duration = std::time::Duration::from_secs(30);

pub async fn gh_available() -> bool {
    static LAST: std::sync::Mutex<Option<(bool, std::time::Instant)>> = std::sync::Mutex::new(None);
    remembered(&LAST, GH_RECHECK, || async { run(&std::env::temp_dir(), "gh", &["--version"]).await.is_ok() }).await
}

/// `probe`'s answer, kept in `slot`: a yes for good, a no for `recheck`.
async fn remembered<F, Fut>(slot: &std::sync::Mutex<Option<(bool, std::time::Instant)>>, recheck: std::time::Duration, probe: F) -> bool
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let cached = *slot.lock().unwrap_or_else(|error| error.into_inner());
    if let Some((answer, at)) = cached
        && (answer || at.elapsed() < recheck)
    {
        return answer;
    }
    let answer = probe().await;
    *slot.lock().unwrap_or_else(|error| error.into_inner()) = Some((answer, std::time::Instant::now()));
    answer
}

pub async fn status(cwd: &Path) -> Result<GitStatus> {
    let state = Repo::probe(cwd).await;
    if state != RepoState::Repo {
        return Ok(GitStatus {
            is_git: false,
            issue: match state {
                RepoState::Unsafe(message) => Some(message),
                _ => None,
            },
            branch: None,
            dirty_files: 0,
            ahead: 0,
            behind: 0,
            upstream: None,
            remote_url: None,
            pull_request: None,
            repos: Vec::new(),
        });
    }
    let repo = Repo::new(cwd);
    let branch = repo.current_branch().await;
    let dirty_files = run(cwd, "git", &["status", "--porcelain"]).await.map(|s| s.lines().count() as u32).unwrap_or(0);
    let upstream = run(cwd, "git", &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]).await.ok();
    let (ahead, behind) = match &upstream {
        Some(_) => run(cwd, "git", &["rev-list", "--left-right", "--count", "@{u}...HEAD"])
            .await
            .ok()
            .and_then(|s| {
                let mut it = s.split_whitespace();
                Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
            })
            .map(|(behind, ahead)| (ahead, behind))
            .unwrap_or((0, 0)),
        None => (0, 0),
    };
    let remote_url = run(cwd, "git", &["remote", "get-url", "origin"]).await.ok();
    let pull_request = match (&branch, remote_url.as_deref().is_some_and(|u| u.contains("github.com")), gh_available().await) {
        (Some(_), true, true) => {
            run(cwd, "gh", &["pr", "view", "--json", "number,title,url,state,headRefName,baseRefName,isDraft,author,updatedAt"])
                .await
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .and_then(|v| parse_pr(&v))
        }
        _ => None,
    };
    Ok(GitStatus { is_git: true, issue: None, branch, dirty_files, ahead, behind, upstream, remote_url, pull_request, repos: Vec::new() })
}

pub(crate) fn parse_pr(v: &Value) -> Option<PullRequest> {
    Some(PullRequest {
        number: v.get("number")?.as_u64()?,
        title: v.get("title")?.as_str()?.to_string(),
        url: v.get("url")?.as_str()?.to_string(),
        state: v.get("state").and_then(|s| s.as_str()).unwrap_or("OPEN").to_string(),
        head: v.get("headRefName").and_then(|s| s.as_str()).unwrap_or("").to_string(),
        base: v.get("baseRefName").and_then(|s| s.as_str()).unwrap_or("").to_string(),
        is_draft: v.get("isDraft").and_then(|b| b.as_bool()).unwrap_or(false),
        author: v.pointer("/author/login").and_then(|s| s.as_str()).unwrap_or("").to_string(),
        updated_at: v.get("updatedAt").and_then(|s| s.as_str()).and_then(|s| s.parse().ok()).unwrap_or_else(chrono::Utc::now),
        author_avatar_url: None,
        author_is_bot: false,
        created_at: None,
        additions: None,
        deletions: None,
        review_decision: None,
        mergeable: None,
        merge_state_status: None,
        auto_merge: None,
        labels: Vec::new(),
        checks_summary: None,
        repo: None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bucket {
    Passed,
    Failed,
    Pending,
    Skipped,
}

/// Classify one `statusCheckRollup` entry (a CheckRun or a StatusContext).
pub(crate) fn check_bucket(v: &Value) -> Bucket {
    let s = |key: &str| v.get(key).and_then(Value::as_str).unwrap_or_default().to_ascii_uppercase();
    let is_run = v.get("__typename").and_then(Value::as_str) == Some("CheckRun") || v.get("status").is_some();
    if is_run {
        if s("status") != "COMPLETED" {
            return Bucket::Pending;
        }
        return match s("conclusion").as_str() {
            "SUCCESS" => Bucket::Passed,
            "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => Bucket::Failed,
            _ => Bucket::Skipped,
        };
    }
    match s("state").as_str() {
        "SUCCESS" => Bucket::Passed,
        "FAILURE" | "ERROR" => Bucket::Failed,
        "PENDING" | "EXPECTED" => Bucket::Pending,
        _ => Bucket::Skipped,
    }
}

pub(crate) fn summarize_checks(rollup: &[Value]) -> PrChecksSummary {
    let mut out = PrChecksSummary { total: rollup.len() as u32, ..Default::default() };
    for v in rollup {
        match check_bucket(v) {
            Bucket::Passed => out.passed += 1,
            Bucket::Failed => out.failed += 1,
            Bucket::Pending => out.pending += 1,
            Bucket::Skipped => out.skipped += 1,
        }
    }
    out
}

/// Avatar URL for a GraphQL actor, which `gh --json` returns without one. GitHub
/// redirects `{host}/{login}.png` to the avatar CDN (also on Enterprise hosts).
pub(crate) fn avatar_url(pr_url: &str, login: &str, is_bot: bool) -> Option<String> {
    if login.is_empty() || is_bot || login.starts_with("app/") {
        return None;
    }
    let rest = pr_url.strip_prefix("https://").or_else(|| pr_url.strip_prefix("http://"))?;
    let host = rest.split('/').next().filter(|h| !h.is_empty())?;
    Some(format!("https://{host}/{login}.png?size=40"))
}

/// `autoMergeRequest` as `gh --json` returns it; null when auto-merge is off.
pub(crate) fn parse_auto_merge(v: Option<&Value>) -> Option<PrAutoMerge> {
    let v = v.filter(|v| v.is_object())?;
    let method = v.get("mergeMethod").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("SQUASH");
    Some(PrAutoMerge {
        enabled_by: v.pointer("/enabledBy/login").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned),
        merge_method: method.to_ascii_lowercase(),
    })
}

fn parse_time(v: &Value, key: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    v.get(key).and_then(Value::as_str).and_then(|s| s.parse().ok())
}

/// `parse_pr` plus the list and detail extras.
pub(crate) fn parse_pr_extended(v: &Value) -> Option<PullRequest> {
    let mut pr = parse_pr(v)?;
    pr.author_is_bot = v.pointer("/author/is_bot").and_then(Value::as_bool).unwrap_or(false);
    pr.author_avatar_url = avatar_url(&pr.url, &pr.author, pr.author_is_bot);
    pr.created_at = parse_time(v, "createdAt");
    pr.additions = v.get("additions").and_then(Value::as_u64).map(|n| n as u32);
    pr.deletions = v.get("deletions").and_then(Value::as_u64).map(|n| n as u32);
    pr.review_decision = v.get("reviewDecision").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    pr.mergeable = v.get("mergeable").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    pr.merge_state_status = v.get("mergeStateStatus").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    pr.auto_merge = parse_auto_merge(v.get("autoMergeRequest"));
    pr.labels = v
        .get("labels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(20)
        .filter_map(|l| {
            Some(PrLabel {
                name: l.get("name")?.as_str()?.to_owned(),
                color: l.get("color").and_then(Value::as_str).unwrap_or_default().to_owned(),
            })
        })
        .collect();
    pr.checks_summary = v.get("statusCheckRollup").and_then(Value::as_array).map(|a| summarize_checks(a));
    Some(pr)
}

pub async fn commit_all(cwd: &Path, message: &str) -> Result<String> {
    run(cwd, "git", &["add", "-A", "--", "."]).await?;
    run(cwd, "git", &["commit", "-q", "-m", message]).await?;
    run(cwd, "git", &["rev-parse", "HEAD"]).await
}

pub async fn has_changes(cwd: &Path) -> bool {
    run(cwd, "git", &["status", "--porcelain"]).await.map(|s| !s.trim().is_empty()).unwrap_or(false)
}

/// `has_changes`, leaving out `exclude` (the nested members of a group folder).
pub async fn has_changes_excluding(cwd: &Path, exclude: &[String]) -> bool {
    Repo::new(cwd).status_porcelain(exclude).await.is_ok_and(|lines| !lines.is_empty())
}

/// `commit_all`, never adding `exclude` (the nested members of a group folder).
pub async fn commit_all_excluding(cwd: &Path, message: &str, exclude: &[String]) -> Result<String> {
    let excludes = kybern_git::exclude_pathspecs(exclude);
    let mut add = vec!["add", "-A", "--", "."];
    add.extend(excludes.iter().map(String::as_str));
    run(cwd, "git", &add).await?;
    run(cwd, "git", &["commit", "-q", "-m", message]).await?;
    run(cwd, "git", &["rev-parse", "HEAD"]).await
}

/// Whether the repository has any remote at all.
pub async fn has_any_remote(cwd: &Path) -> bool {
    run(cwd, "git", &["remote"]).await.is_ok_and(|names| !names.trim().is_empty())
}

/// Whether `origin` points at GitHub.
pub async fn has_github_remote(cwd: &Path) -> bool {
    run(cwd, "git", &["remote", "get-url", "origin"]).await.is_ok_and(|url| url.contains("github.com"))
}

/// Commits of HEAD that `origin/<base>` (else the local `base`) does not have.
pub async fn commits_ahead_of(cwd: &Path, base: &str) -> u32 {
    for range in [format!("origin/{base}..HEAD"), format!("{base}..HEAD")] {
        if let Ok(count) = run(cwd, "git", &["rev-list", "--count", &range]).await {
            return count.trim().parse().unwrap_or(0);
        }
    }
    0
}

/// Diff of what would go into a PR: committed changes vs the base branch plus the working tree.
pub async fn diff_against_base(cwd: &Path, base: &str) -> Result<String> {
    let merge_base =
        run(cwd, "git", &["merge-base", &format!("origin/{base}"), "HEAD"]).await.or_else(|_| Ok::<_, anyhow::Error>(base.to_string()))?;
    run(cwd, "git", &["diff", "--no-color", "--stat", "-p", &merge_base]).await
}

pub async fn default_base(cwd: &Path) -> String {
    default_base_with("gh", cwd).await
}

pub async fn default_base_with(gh: &str, cwd: &Path) -> String {
    if let Ok(s) = run(cwd, gh, &["repo", "view", "--json", "defaultBranchRef", "-q", ".defaultBranchRef.name"]).await
        && !s.is_empty()
    {
        return s;
    }
    if let Ok(s) = run(cwd, "git", &["symbolic-ref", "refs/remotes/origin/HEAD"]).await
        && let Some(b) = s.rsplit('/').next()
    {
        return b.to_string();
    }
    "main".into()
}

pub async fn push_current(cwd: &Path) -> Result<()> {
    let branch = run(cwd, "git", &["symbolic-ref", "--short", "HEAD"]).await?;
    run(cwd, "git", &["push", "-u", "origin", &branch]).await.map(|_| ())
}

pub async fn pr_create(cwd: &Path, title: &str, body: &str, base: &str, draft: bool) -> Result<PullRequest> {
    pr_create_with("gh", cwd, title, body, base, draft).await
}

pub async fn pr_create_with(gh: &str, cwd: &Path, title: &str, body: &str, base: &str, draft: bool) -> Result<PullRequest> {
    let mut args = vec!["pr", "create", "--title", title, "--body", body, "--base", base];
    if draft {
        args.push("--draft");
    }
    run(cwd, gh, &args).await?;
    let json = run(cwd, gh, &["pr", "view", "--json", "number,title,url,state,headRefName,baseRefName,isDraft,author,updatedAt"]).await?;
    parse_pr(&serde_json::from_str::<Value>(&json)?).ok_or_else(|| anyhow!("could not read the created pull request"))
}

/// Replace a pull request's description (`gh pr edit <n> --body-file -`).
pub async fn pr_edit_body(gh: &str, cwd: &Path, number: u64, body: &str) -> Result<()> {
    run_stdin(cwd, gh, &["pr", "edit", &number.to_string(), "--body-file", "-"], body).await.map(|_| ())
}

/// `owner/repo#128` from `https://github.com/owner/repo/pull/128`.
pub fn pr_reference(url: &str) -> Option<String> {
    let rest = url.split_once("github.com/")?.1;
    let mut parts = rest.trim_end_matches('/').split('/');
    let (owner, repo, kind, number) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    (kind == "pull" && !owner.is_empty() && !repo.is_empty() && number.parse::<u64>().is_ok()).then(|| format!("{owner}/{repo}#{number}"))
}

/// The block appended to each pull request of a change across repositories.
pub fn cross_link_block(chat_title: &str, references: &[String]) -> String {
    let mut block = format!("---\nPart of a change across {} repositories (Kybern chat “{chat_title}”):\n", references.len());
    for reference in references {
        block.push_str(&format!("- {reference}\n"));
    }
    block
}

pub async fn pr_list(cwd: &Path, state: &str, limit: u32) -> Result<Vec<PullRequest>> {
    pr_list_with("gh", cwd, state, limit).await
}

pub async fn pr_list_with(gh: &str, cwd: &Path, state: &str, limit: u32) -> Result<Vec<PullRequest>> {
    let limit = limit.to_string();
    let json = run(
        cwd,
        gh,
        &[
            "pr",
            "list",
            "--state",
            state,
            "--limit",
            &limit,
            "--json",
            "number,title,url,state,headRefName,baseRefName,isDraft,author,updatedAt,createdAt,additions,deletions,reviewDecision,mergeable,mergeStateStatus,autoMergeRequest,labels,statusCheckRollup",
        ],
    )
    .await?;
    let v: Value = serde_json::from_str(&json)?;
    Ok(v.as_array().map(|a| a.iter().filter_map(parse_pr_extended).collect()).unwrap_or_default())
}

/// The repository the desktop's star prompt asks about (ADE-117). Fixed: neither RPC takes a repository.
pub const KYBERN_REPOSITORY: &str = "Tresnanda/kybern";
/// How long one `gh api` call for the star prompt may take before it counts as a failure.
const STAR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// What one `gh api` call for the star prompt came back with.
enum GhAnswer {
    /// Exit 0: GitHub answered 2xx (204 for both routes).
    Ok,
    /// GitHub answered 404.
    NotFound,
    /// `gh` is not installed, or not signed in (its exit code 4, or its sign-in hint).
    Unavailable,
    /// Anything else: GitHub refused, the network failed, the call timed out.
    Failed(String),
}

async fn gh_api(gh: &str, args: &[&str]) -> GhAnswer {
    // `spawn` finds a gh installed after the daemon started, like every other gh call.
    let child = match spawn(&std::env::temp_dir(), gh, args, std::process::Stdio::null()) {
        Ok(child) => child,
        Err(error) => {
            let text = format!("{error:#}");
            return match classify(&text) {
                Some((Some(GhErrorKind::GhMissing), _)) => GhAnswer::Unavailable,
                _ => GhAnswer::Failed(text),
            };
        }
    };
    let out = match tokio::time::timeout(STAR_TIMEOUT, child.wait_with_output()).await {
        Err(_) => return GhAnswer::Failed("gh api timed out".into()),
        Ok(Err(error)) => return GhAnswer::Failed(format!("run gh: {error}")),
        Ok(Ok(out)) => out,
    };
    if out.status.success() {
        return GhAnswer::Ok;
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.status.code() == Some(4) || stderr.contains("gh auth login") {
        GhAnswer::Unavailable
    } else if stderr.contains("HTTP 404") {
        GhAnswer::NotFound
    } else {
        GhAnswer::Failed(format!("gh {}: {}", args.join(" "), stderr.trim()))
    }
}

/// `github.starred`: `gh api user/starred/<repo>` answers 204 when starred and 404 when not. A missing or
/// signed-out `gh` reads as "not starred" (the prompt still shows, and Star opens the page); any other
/// failure is an error, which the client takes as "skip the prompt this session".
pub async fn starred_with(gh: &str) -> Result<GithubStarredResult> {
    let route = format!("user/starred/{KYBERN_REPOSITORY}");
    match gh_api(gh, &["api", &route]).await {
        GhAnswer::Ok => Ok(GithubStarredResult { starred: true, can_star: true }),
        GhAnswer::NotFound => Ok(GithubStarredResult { starred: false, can_star: true }),
        GhAnswer::Unavailable => Ok(GithubStarredResult { starred: false, can_star: false }),
        GhAnswer::Failed(message) => Err(anyhow!(message)),
    }
}

/// `github.star`: `gh api -X PUT user/starred/<repo>`.
pub async fn star_with(gh: &str) -> Result<()> {
    let route = format!("user/starred/{KYBERN_REPOSITORY}");
    match gh_api(gh, &["api", "-X", "PUT", &route]).await {
        GhAnswer::Ok => Ok(()),
        GhAnswer::Unavailable => Err(anyhow!("GitHub CLI is not installed or not signed in")),
        GhAnswer::NotFound => Err(anyhow!("GitHub could not find {KYBERN_REPOSITORY}")),
        GhAnswer::Failed(message) => Err(anyhow!(message)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn buckets_cover_every_conclusion_and_state() {
        let run = |status: &str, conclusion: &str| check_bucket(&json!({"__typename":"CheckRun","status":status,"conclusion":conclusion}));
        assert_eq!(run("IN_PROGRESS", ""), Bucket::Pending);
        assert_eq!(run("QUEUED", ""), Bucket::Pending);
        assert_eq!(run("COMPLETED", "SUCCESS"), Bucket::Passed);
        for c in ["FAILURE", "TIMED_OUT", "CANCELLED", "ACTION_REQUIRED", "STARTUP_FAILURE"] {
            assert_eq!(run("COMPLETED", c), Bucket::Failed, "{c}");
        }
        for c in ["SKIPPED", "NEUTRAL", "STALE", ""] {
            assert_eq!(run("COMPLETED", c), Bucket::Skipped, "{c}");
        }
        let ctx = |state: &str| check_bucket(&json!({"__typename":"StatusContext","state":state}));
        assert_eq!(ctx("SUCCESS"), Bucket::Passed);
        assert_eq!(ctx("FAILURE"), Bucket::Failed);
        assert_eq!(ctx("ERROR"), Bucket::Failed);
        assert_eq!(ctx("PENDING"), Bucket::Pending);
        assert_eq!(ctx("EXPECTED"), Bucket::Pending);
    }

    #[test]
    fn list_entry_reduces_rollup_to_a_summary() {
        let v = json!({
            "number": 5, "title": "t", "url": "https://github.com/o/r/pull/5", "author": {"login": "ana", "is_bot": false},
            "additions": 12, "deletions": 3, "reviewDecision": "APPROVED", "mergeable": "CONFLICTING", "mergeStateStatus": "DIRTY",
            "autoMergeRequest": {"enabledBy": {"login": "kevin"}, "mergeMethod": "SQUASH"},
            "labels": [{"name": "bug", "color": "d73a4a"}],
            "statusCheckRollup": [
                {"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"},
                {"__typename":"CheckRun","status":"COMPLETED","conclusion":"FAILURE"},
                {"__typename":"CheckRun","status":"IN_PROGRESS"},
                {"__typename":"StatusContext","state":"SUCCESS"},
            ],
        });
        let pr = parse_pr_extended(&v).unwrap();
        let s = pr.checks_summary.unwrap();
        assert_eq!((s.total, s.passed, s.failed, s.pending, s.skipped), (4, 2, 1, 1, 0));
        assert_eq!(pr.additions, Some(12));
        assert_eq!(pr.mergeable.as_deref(), Some("CONFLICTING"));
        assert_eq!(pr.merge_state_status.as_deref(), Some("DIRTY"));
        let auto = pr.auto_merge.unwrap();
        assert_eq!((auto.enabled_by.as_deref(), auto.merge_method.as_str()), (Some("kevin"), "squash"));
        assert_eq!(pr.labels[0].name, "bug");
        assert_eq!(pr.author_avatar_url.as_deref(), Some("https://github.com/ana.png?size=40"));
    }

    #[test]
    fn pr_errors_get_a_problem_and_next_steps() {
        let kind = |text: &str| friendly_problem(anyhow!("{text}")).1.map(|p| (p.kind, p.subject.unwrap_or_default()));
        let (missing, problem) = friendly_problem(anyhow!("run gh: program not found"));
        assert!(missing.starts_with(GH_MISSING) && missing.ends_with("(run gh: program not found)"), "{missing}");
        let problem = problem.unwrap();
        assert_eq!((problem.kind, problem.subject.as_deref()), (ProblemKind::ToolMissing, Some("gh")));
        let action = problem.action.unwrap();
        assert_eq!((action.id.as_str(), action.params), ("install_tool", Some(serde_json::json!({"tool": "gh"}))));
        assert_eq!(problem.secondary.unwrap().id, "retry");
        assert_eq!(problem.detail.as_deref(), Some("run gh: program not found"));
        assert!(problem.retryable);
        let (auth, problem) = friendly_problem(anyhow!("gh pr list: To get started with GitHub CLI, please run:  gh auth login"));
        assert!(auth.starts_with("gh isn't signed in"), "{auth}");
        let problem = problem.unwrap();
        assert_eq!(problem.kind, ProblemKind::SignedOut);
        assert_eq!(problem.action.unwrap().params, Some(serde_json::json!({"provider": "gh"})));
        assert_eq!(kind("gh pr view 3: HTTP 403: Resource not accessible by integration"), Some((ProblemKind::Permission, "gh".into())));
        assert_eq!(kind("gh api x: HTTP 404: Not Found"), Some((ProblemKind::NotFound, "gh".into())));
        assert_eq!(
            kind("gh pr list: none of the git remotes configured for this repository point to a known GitHub host"),
            Some((ProblemKind::NotFound, "github_remote".into()))
        );
        assert_eq!(friendly_problem(anyhow!("gh pr list: boom")), ("gh pr list: boom".to_string(), None));
        // Friendly text passes through again unchanged, so a second mapping never nests it.
        assert_eq!(friendly_problem(anyhow!("{missing}")).0, missing);
    }

    #[tokio::test]
    async fn a_spawn_failure_keeps_its_cause() {
        let error = run(&std::env::temp_dir(), "kybern-no-such-program-7f3a", &["--version"]).await.unwrap_err();
        let text = format!("{error:#}");
        assert!(text.starts_with("run kybern-no-such-program-7f3a: "), "{text}");
        assert!(text.len() > "run kybern-no-such-program-7f3a: ".len(), "{text}");
    }

    #[tokio::test]
    async fn a_missing_gh_is_asked_again_after_the_recheck_time() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let slot = std::sync::Mutex::new(None);
        let probes = AtomicU32::new(0);
        let ask = |answer: bool| {
            let probes = &probes;
            move || async move {
                probes.fetch_add(1, Ordering::SeqCst);
                answer
            }
        };
        let long = std::time::Duration::from_secs(3600);
        assert!(!remembered(&slot, long, ask(false)).await);
        assert!(!remembered(&slot, long, ask(true)).await, "a fresh no stands");
        assert_eq!(probes.load(Ordering::SeqCst), 1);
        // Once the no is older than the recheck time, gh is asked again, and then the yes stands for good.
        assert!(remembered(&slot, std::time::Duration::ZERO, ask(true)).await);
        assert!(remembered(&slot, std::time::Duration::ZERO, ask(false)).await);
        assert_eq!(probes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn auto_merge_is_absent_when_null() {
        assert!(parse_auto_merge(Some(&json!(null))).is_none());
        assert!(parse_auto_merge(None).is_none());
        let rebase = parse_auto_merge(Some(&json!({"mergeMethod":"REBASE"}))).unwrap();
        assert_eq!((rebase.enabled_by, rebase.merge_method.as_str()), (None, "rebase"));
    }

    #[test]
    fn avatar_urls_skip_bots_and_follow_the_host() {
        assert_eq!(avatar_url("https://github.com/o/r/pull/1", "ana", false).as_deref(), Some("https://github.com/ana.png?size=40"));
        assert_eq!(
            avatar_url("https://ghe.example.com/o/r/pull/1", "ana", false).as_deref(),
            Some("https://ghe.example.com/ana.png?size=40")
        );
        assert!(avatar_url("https://github.com/o/r/pull/1", "dependabot", true).is_none());
        assert!(avatar_url("https://github.com/o/r/pull/1", "app/renovate", false).is_none());
        assert!(avatar_url("https://github.com/o/r/pull/1", "", false).is_none());
    }
}

#[cfg(test)]
mod group_link_tests {
    use super::*;

    #[test]
    fn cross_link_block_names_every_pull_request() {
        assert_eq!(pr_reference("https://github.com/acme/api/pull/128").as_deref(), Some("acme/api#128"));
        assert_eq!(pr_reference("https://github.com/acme/api/issues/3"), None);
        assert_eq!(
            cross_link_block("Add SSO to the admin console", &["acme/api#128".into(), "acme/web#77".into()]),
            "---\nPart of a change across 2 repositories (Kybern chat “Add SSO to the admin console”):\n- acme/api#128\n- acme/web#77\n"
        );
    }
}

// The fake `gh` is a shell script; nothing here reaches GitHub.
#[cfg(all(test, unix))]
mod star_tests {
    use super::*;

    /// A fake `gh` that logs its arguments, then exits with `code` and prints `stderr`.
    fn fake(code: u8, stderr: &str) -> (Cleanup, String, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("kybern-star-{}", uuid::Uuid::now_v7()));
        let log = dir.join("gh.log");
        let body = format!("#!/bin/sh\necho \"$@\" >> \"{}\"\necho '{stderr}' >&2\nexit {code}\n", log.display());
        let gh = crate::test_support::script(&dir, "gh", &body, "");
        (Cleanup(dir), gh.to_string_lossy().into_owned(), log)
    }

    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn starred_reads_204_and_404_and_treats_a_missing_or_signed_out_gh_as_not_starred() {
        let (_dir, gh, log) = fake(0, "");
        let yes = starred_with(&gh).await.unwrap();
        assert_eq!((yes.starred, yes.can_star), (true, true));
        assert_eq!(std::fs::read_to_string(&log).unwrap().trim(), "api user/starred/Tresnanda/kybern");

        let (_dir, gh, _) = fake(1, "gh: Not Found (HTTP 404)");
        let no = starred_with(&gh).await.unwrap();
        assert_eq!((no.starred, no.can_star), (false, true));

        let (_dir, gh, _) = fake(4, "To get started with GitHub CLI, please run:  gh auth login");
        let signed_out = starred_with(&gh).await.unwrap();
        assert_eq!((signed_out.starred, signed_out.can_star), (false, false));

        let missing = starred_with("/nonexistent/kybern-test/gh").await.unwrap();
        assert_eq!((missing.starred, missing.can_star), (false, false));

        let (_dir, gh, _) = fake(1, "gh: Server Error (HTTP 502)");
        assert!(starred_with(&gh).await.is_err(), "another failure is an error: the client skips this session");
    }

    #[tokio::test]
    async fn star_puts_only_kyberns_repository_and_reports_failures() {
        let (_dir, gh, log) = fake(0, "");
        star_with(&gh).await.unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap().trim(), "api -X PUT user/starred/Tresnanda/kybern");

        let (_dir, gh, _) = fake(4, "gh auth login");
        assert!(star_with(&gh).await.unwrap_err().to_string().contains("not signed in"));
        assert!(star_with("/nonexistent/kybern-test/gh").await.is_err());
        let (_dir, gh, _) = fake(1, "gh: Forbidden (HTTP 403)");
        assert!(star_with(&gh).await.unwrap_err().to_string().contains("403"));
    }
}
