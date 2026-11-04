//! Status, branches, commits and pull requests of group threads: each runs on
//! every member repository, primary first.
use super::group_git::{self, RepoTarget};
use super::*;
use crate::github;
use anyhow::ensure;
use futures::{StreamExt, stream};
use kybern_protocol::methods::{GitStatus, PullRequest, RepoGitStatus};

/// Member status probes at a time.
const PARALLEL: usize = 4;

fn no_repository() -> GitStatus {
    GitStatus {
        is_git: false,
        branch: None,
        dirty_files: 0,
        ahead: 0,
        behind: 0,
        upstream: None,
        remote_url: None,
        pull_request: None,
        issue: None,
        repos: Vec::new(),
    }
}

/// A model's `TITLE:`/`BODY:` reply, with `fallback` as the title when it has none.
fn parse_pr_text(text: &str, fallback: &str) -> (String, String) {
    let mut title = fallback.to_string();
    let mut body = String::new();
    let mut in_body = false;
    for line in text.lines() {
        if let Some(t) = line.strip_prefix("TITLE:") {
            title = t.trim().to_string();
        } else if line.trim_start().starts_with("BODY:") {
            in_body = true;
        } else if in_body {
            body.push_str(line);
            body.push('\n');
        }
    }
    if body.trim().is_empty() {
        body = text.to_string();
    }
    (title, body.trim().to_string())
}

/// What to show for a failed `gh`/`git` call: its stderr, without the command line.
fn failure_reason(error: &anyhow::Error) -> String {
    let text = format!("{error:#}");
    let reason = text.split_once(": ").map_or(text.as_str(), |(_, rest)| rest).trim();
    if reason.is_empty() {
        return "gh exited with an error. Run gh auth status in that repository to see why.".to_string();
    }
    reason.chars().take(300).collect()
}

/// `gh`'s refusal for a repository with no GitHub remote it knows.
fn not_on_github(reason: &str) -> bool {
    reason.contains("no git remotes found") || reason.contains("known GitHub host")
}

impl Orchestrator {
    /// `github.pr.list` of a group project: every member folder, a few at a time.
    /// A member that can't be listed (no remote, missing folder, `gh` error) gets
    /// its reason in `repos` and never fails the others.
    pub async fn group_pr_list(&self, project: &Project, state: &str, limit: u32, gh: &str) -> methods::PrListResult {
        let work: Vec<(ProjectMember, Result<PathBuf, String>)> =
            project.members.iter().map(|member| (member.clone(), super::groups::pr_repo_dir(project, Some(&member.path)))).collect();
        let rows: Vec<(methods::PrListRepo, Vec<PullRequest>)> = stream::iter(work)
            .map(|(member, dir)| async move {
                // `gh` picks the GitHub remote itself (any name, Enterprise hosts): only a
                // member with no remote at all is skipped before it runs.
                let listed = match dir {
                    Err(reason) => Err((reason, false)),
                    Ok(dir) if !github::has_any_remote(&dir).await => Err(("No GitHub remote".to_string(), true)),
                    Ok(dir) => github::pr_list_with(gh, &dir, state, limit).await.map_err(|error| {
                        let reason = failure_reason(&error);
                        if not_on_github(&reason) { ("No GitHub remote".to_string(), true) } else { (reason, false) }
                    }),
                };
                // A `gh` failure gets `friendly`'s words and the kind clients recover by.
                let listed = listed.map_err(|(reason, skipped)| match github::friendly_problem(anyhow::anyhow!("{reason}")) {
                    (words, Some(problem)) if !skipped => (words, skipped, Some(problem)),
                    _ => (reason, skipped, None),
                });
                let (pull_requests, error, skipped, problem) = match listed {
                    Ok(mut prs) => {
                        for pr in &mut prs {
                            pr.repo = Some(member.path.clone());
                        }
                        (prs, None, false, None)
                    }
                    Err((reason, skipped, kind)) => (Vec::new(), Some(reason), skipped, kind),
                };
                (methods::PrListRepo { repo: member.path, name: member.name, error, skipped, problem }, pull_requests)
            })
            .buffered(PARALLEL)
            .collect()
            .await;
        let (repos, lists): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
        methods::PrListResult { pull_requests: lists.into_iter().flatten().collect(), repos }
    }

    /// `git.status` of a group thread: the fields are the primary's, `repos`
    /// has every member in order, deselected and missing ones included.
    pub async fn group_git_status(&self, thread: &Thread) -> Result<GitStatus> {
        let cwd = PathBuf::from(&thread.cwd);
        // The other members are nested in the group folder, not its changes.
        let nested: Vec<String> = thread.members.iter().filter(|m| m.path != ".").map(|m| m.path.clone()).collect();
        // Owned per member: a future borrowing from the closure is not `Send` for every lifetime.
        let work: Vec<(ThreadMember, PathBuf, Vec<String>)> =
            thread.members.iter().map(|m| (m.clone(), cwd.clone(), nested.clone())).collect();
        let rows: Vec<RepoGitStatus> = stream::iter(work)
            .map(|(member, cwd, nested)| async move {
                let dir = if member.included && !member.missing {
                    group_git::member_dir(&cwd, &member.path)
                } else {
                    PathBuf::from(&member.source_path)
                };
                let status = if !dir.exists() {
                    MemberStatus::Missing
                } else if !dir.join(".git").exists() {
                    MemberStatus::NotGit
                } else {
                    MemberStatus::Ok
                };
                let git = match status {
                    MemberStatus::Ok => match github::status(&dir).await {
                        Ok(mut git) => {
                            if member.path == "."
                                && let Ok(lines) = Repo::new(&dir).status_porcelain(&nested).await
                            {
                                git.dirty_files = lines.len() as u32;
                            }
                            git
                        }
                        Err(error) => {
                            tracing::debug!(repo = member.path, %error, "member status failed");
                            no_repository()
                        }
                    },
                    _ => no_repository(),
                };
                let mut git = git;
                // The review dock opens this pull request by member (`github.pr.detail` needs `repo`).
                if let Some(pr) = git.pull_request.as_mut() {
                    pr.repo = Some(member.path.clone());
                }
                RepoGitStatus { repo: member.path.clone(), name: member.name.clone(), included: member.included, status, git }
            })
            .buffered(PARALLEL)
            .collect()
            .await;
        let mut top =
            rows.iter().find(|row| row.included && row.status == MemberStatus::Ok).map(|row| row.git.clone()).unwrap_or_else(no_repository);
        top.repos = rows;
        Ok(top)
    }

    /// `git.branches` of one member of a group project (the primary by default).
    pub async fn group_branches(
        &self,
        project: &Project,
        repo: Option<&str>,
        fetch: bool,
        include_remote: bool,
    ) -> Result<methods::GitBranchesResult> {
        let member = match repo {
            Some(repo) => {
                project.members.iter().find(|m| m.path == repo).ok_or_else(|| anyhow!("{repo} isn't a repository of {}.", project.name))?
            }
            None => project.members.first().ok_or_else(|| anyhow!("{} has no repositories yet. Add one in its settings.", project.name))?,
        };
        github::branches(&group_git::member_dir(Path::new(&project.path), &member.path), fetch, include_remote).await
    }

    /// Whether any member of a group thread has something to commit.
    pub async fn group_has_changes(&self, thread: &Thread) -> bool {
        for target in group_git::targets(thread) {
            if target.present() && github::has_changes_excluding(&target.dir, &target.exclude).await {
                return true;
            }
        }
        false
    }

    /// `git.commit` of a group thread: every member with changes gets a commit,
    /// with `message` or one written from that member's own diff.
    pub async fn group_commit(&self, thread: &Thread, message: Option<String>) -> Result<methods::GitCommitResult> {
        let mut commits = Vec::new();
        for target in group_git::targets(thread) {
            if !target.present() || !github::has_changes_excluding(&target.dir, &target.exclude).await {
                continue;
            }
            let message = match &message {
                Some(message) => message.clone(),
                None => self.generate_member_commit_message(thread, &target).await?,
            };
            let commit = github::commit_all_excluding(&target.dir, &message, &target.exclude)
                .await
                .map_err(|e| anyhow!("Couldn't commit {}: {e}", target.name))?;
            commits.push(methods::RepoCommit { repo: target.repo_path().to_string(), commit, message });
        }
        ensure!(!commits.is_empty(), "nothing to commit");
        Ok(methods::GitCommitResult { commit: commits[0].commit.clone(), message: commits[0].message.clone(), commits })
    }

    /// `generate_commit_message` for one member, from that member's changes only.
    pub(super) async fn generate_member_commit_message(&self, thread: &Thread, target: &RepoTarget) -> Result<String> {
        let repo = target.git();
        let snapshot = repo.snapshot_excluding("kybern commit message", &target.exclude).await?;
        let head = repo.head().await.unwrap_or_default();
        let diff = if head.is_empty() { String::new() } else { repo.diff(&head, &snapshot).await?.patch };
        let prompt = format!(
            "Write a git commit message for the diff below, from the repository {}. First line: imperative, under 60 characters. Then a blank line and one to three short sentences explaining why. Reply with the message only.\n\n{}",
            target.name,
            diff.chars().take(20000).collect::<String>()
        );
        let text = self.one_shot_any(thread, &prompt).await?;
        Ok(text.trim().trim_matches('`').trim().to_string())
    }

    /// `generate_pr_text` for one member, naming the other repositories of the change.
    async fn generate_member_pr_text(
        &self,
        thread: &Thread,
        target: &RepoTarget,
        base: &str,
        names: &[String],
    ) -> Result<(String, String)> {
        let diff = github::diff_against_base(&target.dir, base).await.unwrap_or_default();
        let others: Vec<&str> = names.iter().map(String::as_str).filter(|name| *name != target.name).collect();
        let context = if others.is_empty() {
            String::new()
        } else {
            format!(
                "This pull request is part of a change across repositories; the others get their own pull requests: {}. Describe only the changes in {}.\n\n",
                others.join(", "),
                target.name
            )
        };
        let prompt = format!(
            "Write a pull request title and description for the diff below, from the repository {}. Format exactly as:\nTITLE: <under 70 characters>\nBODY:\n<markdown with a short summary and a bullet list of changes>\n\n{context}Thread request: {}\n\nDiff:\n{}",
            target.name,
            thread.title,
            diff.chars().take(24000).collect::<String>()
        );
        let text = self.one_shot_any(thread, &prompt).await?;
        Ok(parse_pr_text(&text, &thread.title))
    }

    /// `github.pr.create_group`: one pull request per changed member with a
    /// GitHub remote, one after another, then cross-linked. A member that fails
    /// is skipped with the reason; the others are never undone.
    pub async fn pr_create_group(&self, p: methods::PrCreateGroupParams, gh: &str) -> Result<methods::PrCreateGroupResult> {
        let thread = self.inner.store.thread_get(p.thread_id)?.ok_or_else(|| anyhow!("thread not found"))?;
        ensure!(group_git::is_group(&thread), "This chat works in one repository. Use Create pull request (github.pr.create).");
        let all = group_git::targets(&thread);
        let chosen: Vec<RepoTarget> = match &p.repos {
            Some(repos) => {
                for repo in repos {
                    ensure!(all.iter().any(|t| t.repo_path() == repo), "{repo} isn't a repository of this chat.");
                }
                all.into_iter().filter(|t| repos.iter().any(|repo| repo == t.repo_path())).collect()
            }
            None => all,
        };
        // Only repositories this chat changed: since its first checkpoint, so a
        // member's earlier work in progress or unpushed commits never ride along.
        let changed: Vec<String> =
            match group_git::group_diff(&self.inner.store, &thread, None, false, None, group_git::DiffCaps::UNLIMITED_FILES).await {
                Ok(diff) => diff.repos.into_iter().filter(|r| r.files > 0).map(|r| r.repo).collect(),
                Err(error) => {
                    tracing::debug!(thread_id = %thread.id, %error, "no group diff for pull requests");
                    Vec::new()
                }
            };
        let mut skipped = Vec::new();
        let chosen: Vec<RepoTarget> = chosen
            .into_iter()
            .filter(|t| {
                let keep = changed.iter().any(|repo| repo == t.repo_path());
                if !keep && p.repos.is_some() {
                    skipped.push(methods::RejectedFolder { path: t.repo_path().to_string(), reason: "This chat didn't change it".into() });
                }
                keep
            })
            .collect();
        ensure!(!chosen.is_empty() || !skipped.is_empty(), "This chat hasn't changed any repository yet.");
        let names: Vec<String> = chosen.iter().map(|t| t.name.clone()).collect();
        let mut created: Vec<(RepoTarget, PullRequest, String)> = Vec::new();
        for target in chosen {
            match self.member_pr(&thread, &target, &names, &p, gh).await {
                Ok((pr, body)) => created.push((target, pr, body)),
                Err(reason) => skipped.push(methods::RejectedFolder { path: target.repo_path().to_string(), reason }),
            }
        }
        if created.len() >= 2 {
            let references: Vec<String> =
                created.iter().map(|(_, pr, _)| github::pr_reference(&pr.url).unwrap_or_else(|| pr.url.clone())).collect();
            let block = github::cross_link_block(&thread.title, &references);
            for (target, pr, body) in &created {
                let body = if body.trim().is_empty() { block.clone() } else { format!("{}\n\n{block}", body.trim_end()) };
                if let Err(error) = github::pr_edit_body(gh, &target.dir, pr.number, &body).await {
                    tracing::warn!(repo = target.repo_path(), number = pr.number, %error, "could not cross-link the pull request");
                }
            }
        }
        Ok(methods::PrCreateGroupResult {
            pull_requests: created
                .into_iter()
                .map(|(target, pull_request, _)| methods::RepoPullRequest { repo: target.repo_path().to_string(), pull_request })
                .collect(),
            skipped,
        })
    }

    /// One member's pull request and the body it was opened with, or the
    /// sentence explaining why it was skipped.
    async fn member_pr(
        &self,
        thread: &Thread,
        target: &RepoTarget,
        names: &[String],
        p: &methods::PrCreateGroupParams,
        gh: &str,
    ) -> std::result::Result<(PullRequest, String), String> {
        let dir = &target.dir;
        if !target.present() {
            return Err(format!("{} is missing or is no longer a git repository", target.name));
        }
        if !github::has_github_remote(dir).await {
            return Err("No GitHub remote".into());
        }
        let dirty = github::has_changes_excluding(dir, &target.exclude).await;
        let base = github::default_base_with(gh, dir).await;
        // Never commit to or push the base branch: the pull request needs its own.
        if target.git().current_branch().await.as_deref() == Some(base.as_str()) {
            return Err(format!("{} is on {base}. Create a branch first", target.name));
        }
        if !(p.commit_first && dirty) && github::commits_ahead_of(dir, &base).await == 0 {
            return Err("No changes".into());
        }
        if p.commit_first && dirty {
            let message = match self.generate_member_commit_message(thread, target).await {
                Ok(message) if !message.is_empty() => message,
                _ => thread.title.clone(),
            };
            github::commit_all_excluding(dir, &message, &target.exclude).await.map_err(|e| format!("Couldn't commit: {e}"))?;
        }
        github::push_current(dir).await.map_err(|e| format!("Couldn't push: {e}"))?;
        let (title, body) = self.generate_member_pr_text(thread, target, &base, names).await.unwrap_or_else(|error| {
            tracing::debug!(repo = target.repo_path(), %error, "pull request text not generated");
            (thread.title.clone(), String::new())
        });
        let pr = github::pr_create_with(gh, dir, &title, &body, &base, p.draft).await.map_err(|e| e.to_string())?;
        Ok((pr, body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_text_falls_back_to_the_chat_title() {
        assert_eq!(parse_pr_text("TITLE: Add SSO\nBODY:\n- one\n", "Chat"), ("Add SSO".into(), "- one".into()));
        assert_eq!(parse_pr_text("just words", "Chat"), ("Chat".into(), "just words".into()));
    }
}
