// Group thread status, commits and pull requests. Included into
// `orchestrator::tests` for its fixture and the `git_in` helper.

fn pr_init_member(dir: &std::path::Path, file: &str) {
    std::fs::create_dir_all(dir).unwrap();
    git_in(dir, &["init", "-q"]);
    git_in(dir, &["config", "core.autocrlf", "false"]);
    git_in(dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git_in(dir, &["config", "user.name", "Group Test"]);
    git_in(dir, &["config", "user.email", "group@example.com"]);
    git_in(dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join(file), "one\n").unwrap();
    git_in(dir, &["add", "."]);
    git_in(dir, &["commit", "-q", "-m", "init"]);
}

fn group_member(folder: &std::path::Path, path: &str, included: bool, missing: bool) -> ThreadMember {
    ThreadMember {
        path: path.into(),
        name: if path == "." { "acme".into() } else { path.into() },
        included: included && !missing,
        source_path: super::group_git::member_dir(folder, path).to_string_lossy().into_owned(),
        worktree: None,
        base_branch: None,
        base_commit: None,
        worktree_state: None,
        missing,
    }
}

/// A local-mode group thread in `<root>/acme`: the folder itself is a repository
/// holding `api` and `web`, `infra` is deselected and `docs` is missing.
fn group_with_folder_repo(fixture: &Fixture) -> (std::path::PathBuf, Thread) {
    let folder = fixture.root.join("acme");
    pr_init_member(&folder, "README.md");
    pr_init_member(&folder.join("api"), "a.rs");
    pr_init_member(&folder.join("web"), "w.ts");
    pr_init_member(&folder.join("infra"), "main.tf");
    let mut thread = fixture.thread(ThreadStatus::Idle);
    thread.cwd = folder.to_string_lossy().into_owned();
    thread.members = vec![
        group_member(&folder, ".", true, false),
        group_member(&folder, "api", true, false),
        group_member(&folder, "web", true, false),
        group_member(&folder, "infra", false, false),
        group_member(&folder, "docs", false, true),
    ];
    fixture.store.thread_upsert(&thread).unwrap();
    (folder, thread)
}

#[tokio::test]
async fn group_status_lists_every_member_and_ignores_nested_members_in_the_folder() {
    let fixture = Fixture::new();
    let (folder, thread) = group_with_folder_repo(&fixture);
    std::fs::write(folder.join("api/new.rs"), "x\n").unwrap();
    let status = fixture.orchestrator.group_git_status(&thread).await.unwrap();
    assert!(status.is_git);
    assert_eq!(status.branch.as_deref(), Some("main"));
    assert_eq!(status.dirty_files, 0, "nested members are not the group folder's changes");
    let rows: Vec<_> = status.repos.iter().map(|r| (r.repo.as_str(), r.included, r.status, r.git.dirty_files)).collect();
    assert_eq!(
        rows,
        [
            (".", true, MemberStatus::Ok, 0),
            ("api", true, MemberStatus::Ok, 1),
            ("web", true, MemberStatus::Ok, 0),
            ("infra", false, MemberStatus::Ok, 0),
            ("docs", false, MemberStatus::Missing, 0),
        ]
    );
    assert_eq!(status.repos[3].git.branch.as_deref(), Some("main"));
    assert!(status.repos.iter().all(|r| r.git.repos.is_empty()));
    assert!(!status.repos[4].git.is_git);
}

#[tokio::test]
async fn group_commit_fans_out_and_never_adds_nested_members_to_the_folder() {
    let fixture = Fixture::new();
    let (folder, thread) = group_with_folder_repo(&fixture);
    std::fs::write(folder.join("README.md"), "two\n").unwrap();
    std::fs::write(folder.join("api/new.rs"), "x\n").unwrap();
    assert!(fixture.orchestrator.group_has_changes(&thread).await);
    let result = fixture.orchestrator.group_commit(&thread, Some("Update docs".into())).await.unwrap();
    let repos: Vec<_> = result.commits.iter().map(|c| c.repo.as_str()).collect();
    assert_eq!(repos, [".", "api"]);
    assert_eq!(result.commit, result.commits[0].commit);
    assert_eq!(git_in(&folder, &["show", "--name-only", "--format=", "HEAD"]), "README.md");
    assert_eq!(git_in(&folder.join("api"), &["log", "-1", "--format=%s"]), "Update docs");
    assert_eq!(git_in(&folder.join("web"), &["rev-list", "--count", "HEAD"]), "1");
    assert!(!fixture.orchestrator.group_has_changes(&thread).await);
    let error = fixture.orchestrator.group_commit(&thread, Some("Again".into())).await.unwrap_err();
    assert_eq!(error.to_string(), "nothing to commit");
}

#[test]
fn group_branches_of_an_unknown_member_name_the_project() {
    let fixture = Fixture::new();
    let mut project = fixture.project.clone();
    project.kind = ProjectKind::Group;
    project.members = vec![ProjectMember { path: "api".into(), name: "api".into(), status: MemberStatus::Ok }];
    let error = futures::executor::block_on(fixture.orchestrator.group_branches(&project, Some("web"), false, false)).unwrap_err();
    assert_eq!(error.to_string(), "web isn't a repository of fixture.");
}

/// A fake `gh` for group pull requests: `api` opens #128, `web` #77; edits
/// write the new body next to the log.
#[cfg(unix)]
fn fake_group_gh(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    std::fs::create_dir_all(dir).unwrap();
    let (log, script) = (dir.join("gh.log"), dir.join("gh"));
    let body = r#"#!/bin/sh
name=$(basename "$PWD")
echo "$name $@" >> "LOG"
case "$1 $2" in
  --version*) echo "gh version 2.0.0" ;;
  "repo view") echo main ;;
  "pr create") echo "https://github.com/acme/$name/pull/1" ;;
  "pr view")
    case "$name" in api) n=128 ;; web) n=77 ;; *) n=5 ;; esac
    echo "{\"number\":$n,\"title\":\"t\",\"url\":\"https://github.com/acme/$name/pull/$n\",\"state\":\"OPEN\",\"headRefName\":\"feat\",\"baseRefName\":\"main\",\"isDraft\":false,\"author\":{\"login\":\"me\"},\"updatedAt\":\"2026-01-01T00:00:00Z\"}" ;;
  "pr edit") cat > "DIR/edit-$name-$3" ;;
esac
"#
    .replace("LOG", &log.to_string_lossy())
    .replace("DIR", &dir.to_string_lossy());
    kybern_protocol::process::write_executable(&script, body).unwrap();
    (script, log)
}

#[cfg(unix)]
#[tokio::test]
async fn group_pull_requests_skip_repos_without_a_remote_and_cross_link_the_rest() {
    let fixture = Fixture::new();
    let folder = fixture.root.join("acme");
    // `ops` has the user's own work in progress and is on main; `site` is on main.
    let repos = [("api", true), ("web", true), ("infra", false), ("ops", true), ("site", true)];
    for (name, remote) in repos {
        let dir = folder.join(name);
        pr_init_member(&dir, "a.txt");
        if remote {
            let bare = fixture.root.join("github.com/acme").join(format!("{name}.git"));
            std::fs::create_dir_all(&bare).unwrap();
            git_in(&bare, &["init", "-q", "--bare"]);
            git_in(&dir, &["remote", "add", "origin", &bare.to_string_lossy()]);
            git_in(&dir, &["push", "-q", "origin", "main"]);
        }
        if !matches!(name, "ops" | "site") {
            git_in(&dir, &["switch", "-q", "-c", "feat"]);
        }
    }
    std::fs::write(folder.join("ops/a.txt"), "user wip\n").unwrap();
    git_in(&folder.join("ops"), &["commit", "-qam", "unpushed"]);
    std::fs::write(folder.join("ops/a.txt"), "more user wip\n").unwrap();
    let mut thread = fixture.thread(ThreadStatus::Idle);
    thread.title = "Add SSO to the admin console".into();
    thread.cwd = folder.to_string_lossy().into_owned();
    thread.members = repos.iter().map(|(path, _)| group_member(&folder, path, true, false)).collect();
    fixture.store.thread_upsert(&thread).unwrap();
    // The chat's turn changes api, web, infra and site, never ops.
    let turn = Uuid::now_v7();
    fixture.orchestrator.checkpoint(&thread, turn, "before").await;
    for name in ["api", "web", "infra", "site"] {
        std::fs::write(folder.join(name).join("a.txt"), "two\n").unwrap();
    }
    fixture.orchestrator.checkpoint(&thread, turn, "after").await;
    let (gh, log) = fake_group_gh(&fixture.root.join("gh"));

    let result = fixture
        .orchestrator
        .pr_create_group(
            methods::PrCreateGroupParams { thread_id: thread.id, repos: None, draft: false, commit_first: true },
            &gh.to_string_lossy(),
        )
        .await
        .unwrap();
    let created: Vec<_> = result.pull_requests.iter().map(|p| (p.repo.as_str(), p.pull_request.number)).collect();
    assert_eq!(created, [("api", 128), ("web", 77)]);
    let skipped: Vec<_> = result.skipped.iter().map(|s| (s.path.as_str(), s.reason.as_str())).collect();
    assert_eq!(skipped, [("infra", "No GitHub remote"), ("site", "site is on main. Create a branch first")]);
    assert_eq!(git_in(&folder.join("ops"), &["status", "--porcelain"]), "M a.txt", "an untouched repository is never committed");
    let ops = fixture.root.join("github.com/acme/ops.git");
    assert_eq!(git_in(&ops, &["rev-parse", "main"]), git_in(&folder.join("ops"), &["rev-parse", "HEAD~1"]), "nor pushed");
    assert_eq!(git_in(&folder.join("site"), &["status", "--porcelain"]), "M a.txt", "the base branch is never committed to");
    for name in ["api", "web"] {
        assert_eq!(git_in(&folder.join(name), &["status", "--porcelain"]), "", "{name} was committed first");
        let bare = fixture.root.join("github.com/acme").join(format!("{name}.git"));
        assert_eq!(git_in(&bare, &["rev-parse", "feat"]), git_in(&folder.join(name), &["rev-parse", "HEAD"]), "{name} was pushed");
    }
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.contains("api pr edit 128 --body-file -"), "{calls}");
    assert!(calls.contains("web pr edit 77 --body-file -"), "{calls}");
    assert!(!calls.lines().any(|line| line.starts_with("infra pr") || line.starts_with("ops ") || line.starts_with("site pr")), "{calls}");
    let body = std::fs::read_to_string(fixture.root.join("gh/edit-api-128")).unwrap();
    assert_eq!(
        body,
        "---\nPart of a change across 2 repositories (Kybern chat \u{201c}Add SSO to the admin console\u{201d}):\n- acme/api#128\n- acme/web#77\n"
    );
    let unknown = fixture
        .orchestrator
        .pr_create_group(
            methods::PrCreateGroupParams { thread_id: thread.id, repos: Some(vec!["docs".into()]), draft: false, commit_first: true },
            &gh.to_string_lossy(),
        )
        .await
        .unwrap_err();
    assert_eq!(unknown.to_string(), "docs isn't a repository of this chat.");
    let untouched = fixture
        .orchestrator
        .pr_create_group(
            methods::PrCreateGroupParams { thread_id: thread.id, repos: Some(vec!["ops".into()]), draft: false, commit_first: true },
            &gh.to_string_lossy(),
        )
        .await
        .unwrap();
    assert!(untouched.pull_requests.is_empty());
    assert_eq!(untouched.skipped[0].reason, "This chat didn't change it");
}

/// A fake `gh` for listing: `api` has #3 and #9, `web` fails like a signed-out `gh`,
/// `lab` has only a remote `gh` doesn't know (GitLab), `quiet` fails with no stderr.
#[cfg(unix)]
fn fake_list_gh(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let script = dir.join("gh");
    let body = r#"#!/bin/sh
name=$(basename "$PWD")
case "$name" in
  web) echo "HTTP 401: Bad credentials" >&2; exit 1 ;;
  lab) echo "none of the git remotes configured for this repository point to a known GitHub host" >&2; exit 1 ;;
  quiet) exit 1 ;;
  api) echo '[{"number":3,"title":"Add SSO","url":"https://github.com/acme/api/pull/3","state":"OPEN","headRefName":"sso","baseRefName":"main","isDraft":false,"author":{"login":"me"},"updatedAt":"2026-01-02T00:00:00Z"},{"number":9,"title":"Fix","url":"https://github.com/acme/api/pull/9","state":"OPEN","headRefName":"fix","baseRefName":"main","isDraft":true,"author":{"login":"me"},"updatedAt":"2026-01-01T00:00:00Z"}]' ;;
  *) echo '[{"number":1,"title":"Docs","url":"https://github.com/acme/x/pull/1","state":"OPEN","headRefName":"d","baseRefName":"main","isDraft":false,"author":{"login":"me"},"updatedAt":"2026-01-01T00:00:00Z"}]' ;;
esac
"#;
    kybern_protocol::process::write_executable(&script, body).unwrap();
    script
}

#[cfg(unix)]
#[tokio::test]
async fn group_pull_request_list_tags_members_and_keeps_going_past_a_failing_one() {
    let fixture = Fixture::new();
    let folder = fixture.root.join("acme");
    // api answers, ops answers through a remote named `upstream`, web's `gh` fails,
    // infra has no remote, lab is on GitLab, quiet fails silently, docs was deleted.
    for (name, remote) in
        [("api", Some("origin")), ("web", Some("origin")), ("infra", None), ("ops", Some("upstream")), ("lab", Some("origin")), ("quiet", Some("origin"))]
    {
        let dir = folder.join(name);
        pr_init_member(&dir, "a.txt");
        if let Some(remote) = remote {
            let host = if name == "lab" { "gitlab.com" } else { "github.com" };
            git_in(&dir, &["remote", "add", remote, &format!("https://{host}/acme/{name}.git")]);
        }
    }
    let mut project = fixture.project.clone();
    project.kind = ProjectKind::Group;
    project.path = folder.to_string_lossy().into_owned();
    project.members = ["api", "web", "infra", "ops", "lab", "quiet", "docs"]
        .into_iter()
        .map(|path| ProjectMember { path: path.into(), name: path.into(), status: MemberStatus::Ok })
        .collect();
    let gh = fake_list_gh(&fixture.root.join("bin"));

    let result = fixture.orchestrator.group_pr_list(&project, "open", 30, &gh.to_string_lossy()).await;
    let listed: Vec<_> = result.pull_requests.iter().map(|p| (p.repo.as_deref().unwrap(), p.number)).collect();
    assert_eq!(listed, [("api", 3), ("api", 9), ("ops", 1)], "member order, each tagged with its member");
    let rows: Vec<_> = result.repos.iter().map(|r| (r.repo.as_str(), r.error.as_deref(), r.skipped)).collect();
    assert_eq!(
        rows,
        [
            ("api", None, false),
            ("web", Some("gh isn't signed in. Run gh auth login and try again. (HTTP 401: Bad credentials)"), false),
            ("infra", Some("No GitHub remote"), true),
            ("ops", None, false),
            ("lab", Some("No GitHub remote"), true),
            ("quiet", Some("gh exited with an error. Run gh auth status in that repository to see why."), false),
            ("docs", Some("docs is missing from fixture. Restore the folder or remove it in the project's settings."), false),
        ]
    );
    // A known gh failure also says how to fix it; others have no problem.
    let kinds: Vec<_> = result.repos.iter().map(|r| (r.repo.as_str(), r.problem.as_ref().map(|p| p.kind))).collect();
    assert_eq!(kinds[1], ("web", Some(kybern_protocol::rpc::ProblemKind::SignedOut)));
    assert!(kinds.iter().filter(|(repo, _)| *repo != "web").all(|(_, kind)| kind.is_none()), "{kinds:?}");
    // The wire form leaves the new fields out for single projects.
    let single = methods::PrListResult { pull_requests: Vec::new(), repos: Vec::new() };
    assert_eq!(serde_json::to_value(&single).unwrap(), serde_json::json!({"pull_requests": []}));

    // A single repository lists exactly as before: untagged, no member rows.
    let api = folder.join("api");
    let prs = crate::github::pr_list_with(&gh.to_string_lossy(), &api, "open", 30).await.unwrap();
    assert_eq!(prs.iter().map(|p| p.number).collect::<Vec<_>>(), [3, 9]);
    assert!(prs.iter().all(|p| p.repo.is_none()));
}

#[test]
fn pull_request_calls_run_in_the_chosen_member_folder() {
    let fixture = Fixture::new();
    let single = fixture.project.clone();
    assert_eq!(super::groups::pr_repo_dir(&single, None).unwrap(), std::path::PathBuf::from(&single.path));
    assert!(super::groups::pr_repo_dir(&single, Some("api")).is_err(), "a single project takes no repo");

    let folder = fixture.root.join("acme");
    pr_init_member(&folder.join("api"), "a.rs");
    std::fs::create_dir_all(folder.join("notes")).unwrap();
    let mut group = single;
    group.kind = ProjectKind::Group;
    group.path = folder.to_string_lossy().into_owned();
    group.members = ["api", "notes", "gone"]
        .into_iter()
        .map(|path| ProjectMember { path: path.into(), name: path.into(), status: MemberStatus::Ok })
        .collect();
    assert_eq!(super::groups::pr_repo_dir(&group, Some("api")).unwrap(), folder.join("api"));
    // No default member: a client from before member tags must not act on the primary's #N.
    assert!(super::groups::pr_repo_dir(&group, None).unwrap_err().contains("needs its repository"));
    assert_eq!(super::groups::pr_repo_dir(&group, Some("web")).unwrap_err(), "web isn't a repository of fixture.");
    // Only exact member paths resolve: nothing outside the group folder.
    for escape in ["../api", "api/..", "/etc", folder.join("api").to_str().unwrap(), "", "."] {
        assert!(super::groups::pr_repo_dir(&group, Some(escape)).is_err(), "{escape:?} must not resolve");
    }
    assert_eq!(
        super::groups::pr_repo_dir(&group, Some("notes")).unwrap_err(),
        "notes is no longer a git repository. Restore its .git folder or remove it in the project's settings."
    );
    assert_eq!(
        super::groups::pr_repo_dir(&group, Some("gone")).unwrap_err(),
        "gone is missing from fixture. Restore the folder or remove it in the project's settings."
    );
    group.members.clear();
    assert_eq!(super::groups::pr_repo_dir(&group, None).unwrap_err(), "fixture has no repositories yet. Add one in its settings.");
}
