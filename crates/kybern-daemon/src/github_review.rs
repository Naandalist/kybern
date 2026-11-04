//! Bounded, native GitHub review reads and explicit human actions.
use std::path::Path;

use anyhow::{Result, anyhow, ensure};
use kybern_protocol::methods::*;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use crate::github::{avatar_url, parse_pr_extended, run};

const PAGE_SIZE: usize = 30;
const PATCH_BYTES: usize = 64 * 1024;

fn text(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}
fn bounded(mut value: String, limit: usize) -> (String, bool) {
    if value.len() <= limit {
        return (value, false);
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    (value, true)
}
fn validate_number(number: u64) -> Result<()> {
    ensure!(number > 0, "Choose a pull request number greater than zero.");
    Ok(())
}

/// One query for everything `gh pr view --json` lacks: which checks the rules require, what the
/// signed-in account may do, the repository's merge methods and the counts. Check contexts page
/// through `$after`.
const DETAIL_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!,$after:String){repository(owner:$owner,name:$name){viewerPermission squashMergeAllowed mergeCommitAllowed rebaseMergeAllowed pullRequest(number:$number){isDraft viewerCanUpdate viewerCanUpdateBranch viewerCanMergeAsAdmin viewerCanEnableAutoMerge viewerCanDisableAutoMerge headRepositoryOwner{login} comments{totalCount} reviewThreads{totalCount} tlComments:timelineItems(itemTypes:[ISSUE_COMMENT]){filteredCount} tlReviews:timelineItems(itemTypes:[PULL_REQUEST_REVIEW]){filteredCount} tlCommits:timelineItems(itemTypes:[PULL_REQUEST_COMMIT]){filteredCount} commits(last:1){nodes{commit{statusCheckRollup{contexts(first:100,after:$after){totalCount pageInfo{hasNextPage endCursor} nodes{__typename ... on CheckRun{name status conclusion detailsUrl startedAt completedAt isRequired(pullRequestNumber:$number) checkSuite{workflowRun{workflow{name}}}} ... on StatusContext{context state targetUrl createdAt isRequired(pullRequestNumber:$number)}}}}}}}}}}";

/// Hundreds of checks is a monorepo; stop paging there.
const MAX_CHECK_PAGES: usize = 5;

/// One page of `DETAIL_QUERY`.
struct DetailPage {
    viewer: PrViewer,
    head_owner: Option<String>,
    comment_count: Option<u32>,
    timeline_counts: Option<PrTimelineCounts>,
    /// Raw rollup contexts, for the summary.
    nodes: Vec<Value>,
    has_next: bool,
    cursor: Option<String>,
}

fn count(v: &Value, pointer: &str) -> Option<u32> {
    v.pointer(pointer)?.as_u64().map(|n| n as u32)
}

fn parse_detail_page(root: &Value) -> Option<DetailPage> {
    let repo = root.pointer("/data/repository")?;
    let pr = repo.get("pullRequest").filter(|v| v.is_object())?;
    let flag = |v: &Value, key: &str| v.get(key).and_then(Value::as_bool).unwrap_or(false);
    let mut merge_methods = Vec::new();
    for (key, name) in [("squashMergeAllowed", "squash"), ("mergeCommitAllowed", "merge"), ("rebaseMergeAllowed", "rebase")] {
        if flag(repo, key) {
            merge_methods.push(name.to_owned());
        }
    }
    let viewer = PrViewer {
        can_merge: matches!(repo.get("viewerPermission").and_then(Value::as_str), Some("WRITE" | "MAINTAIN" | "ADMIN")),
        can_merge_as_admin: flag(pr, "viewerCanMergeAsAdmin"),
        can_update_branch: flag(pr, "viewerCanUpdateBranch"),
        can_enable_auto_merge: flag(pr, "viewerCanEnableAutoMerge"),
        can_disable_auto_merge: flag(pr, "viewerCanDisableAutoMerge"),
        can_mark_ready: flag(pr, "viewerCanUpdate") && flag(pr, "isDraft"),
        merge_methods,
    };
    let comment_count = match (count(pr, "/comments/totalCount"), count(pr, "/reviewThreads/totalCount")) {
        (Some(a), Some(b)) => Some(a + b),
        _ => None,
    };
    let timeline_counts =
        match (count(pr, "/tlComments/filteredCount"), count(pr, "/tlReviews/filteredCount"), count(pr, "/tlCommits/filteredCount")) {
            (Some(comments), Some(reviews), Some(commits)) => Some(PrTimelineCounts { comments, reviews, commits }),
            _ => None,
        };
    let contexts = pr.pointer("/commits/nodes/0/commit/statusCheckRollup/contexts");
    Some(DetailPage {
        viewer,
        head_owner: pr.pointer("/headRepositoryOwner/login").and_then(Value::as_str).map(str::to_owned),
        comment_count,
        timeline_counts,
        nodes: contexts.and_then(|c| c.get("nodes")).and_then(Value::as_array).cloned().unwrap_or_default(),
        has_next: contexts.and_then(|c| c.pointer("/pageInfo/hasNextPage")).and_then(Value::as_bool).unwrap_or(false),
        cursor: contexts.and_then(|c| c.pointer("/pageInfo/endCursor")).and_then(Value::as_str).map(str::to_owned),
    })
}

fn opt_text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)
}

/// A rollup context from either source: GraphQL contexts carry requiredness, `gh pr view` ones
/// use `workflowName` and have none.
fn parse_check(v: &Value) -> PrCheck {
    let is_run = v.get("__typename").and_then(Value::as_str) == Some("CheckRun") || v.get("status").is_some();
    PrCheck {
        name: v.get("name").or_else(|| v.get("context")).and_then(Value::as_str).unwrap_or_default().to_owned(),
        status: text(v, "status"),
        conclusion: v.get("conclusion").or_else(|| v.get("state")).and_then(Value::as_str).unwrap_or_default().to_owned(),
        url: v.get("detailsUrl").or_else(|| v.get("targetUrl")).and_then(Value::as_str).unwrap_or_default().to_owned(),
        is_required: v.get("isRequired").and_then(Value::as_bool),
        workflow_name: v
            .pointer("/checkSuite/workflowRun/workflow/name")
            .and_then(Value::as_str)
            .or_else(|| v.get("workflowName").and_then(Value::as_str))
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        started_at: opt_text(v, "startedAt").or_else(|| if is_run { None } else { opt_text(v, "createdAt") }),
        completed_at: opt_text(v, "completedAt"),
    }
}

/// Every page of the detail query (first page only when it fails: the caller falls back to `gh pr view`).
async fn graph_detail(cwd: &Path, number: &str) -> Option<(DetailPage, bool)> {
    let number_field = format!("number={number}");
    let query = format!("query={DETAIL_QUERY}");
    let mut after: Option<String> = None;
    let mut first: Option<DetailPage> = None;
    let mut nodes: Vec<Value> = Vec::new();
    let mut truncated = false;
    for page in 0..MAX_CHECK_PAGES {
        let after_field = after.as_ref().map(|c| format!("after={c}"));
        let mut args =
            vec!["api", "graphql", "-F", "owner={owner}", "-F", "name={repo}", "-F", number_field.as_str(), "-f", query.as_str()];
        if let Some(field) = &after_field {
            args.extend(["-f", field.as_str()]);
        }
        let out = run(cwd, "gh", &args).await.ok()?;
        let parsed = parse_detail_page(&serde_json::from_str(&out).ok()?)?;
        nodes.extend(parsed.nodes.iter().cloned());
        let (has_next, cursor) = (parsed.has_next, parsed.cursor.clone());
        first.get_or_insert(parsed);
        match (has_next, cursor) {
            (true, Some(cursor)) if page + 1 < MAX_CHECK_PAGES => after = Some(cursor),
            (true, _) => {
                truncated = true;
                break;
            }
            _ => break,
        }
    }
    let mut page = first?;
    page.nodes = nodes;
    Some((page, truncated))
}

/// Percent-encode the few characters a branch name can hold that break a URL path.
fn encode_ref(name: &str) -> String {
    name.replace('%', "%25").replace('#', "%23").replace('?', "%3F").replace(' ', "%20")
}

/// Commits the base has that the head lacks, for a BEHIND pull request. Best effort.
async fn behind_by(cwd: &Path, base: &str, head: &str, head_owner: Option<&str>) -> Option<u32> {
    let endpoint = format!("repos/{{owner}}/{{repo}}/compare/{}...{}:{}", encode_ref(base), head_owner?, encode_ref(head));
    let out = run(cwd, "gh", &["api", &endpoint, "--jq", ".behind_by"]).await.ok()?;
    out.trim().parse().ok()
}

fn parse_people(value: &Value, key: &str, pr_url: &str) -> Vec<PrPerson> {
    value[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let login = v.get("login").and_then(Value::as_str).filter(|s| !s.is_empty())?;
            let is_bot = v.get("is_bot").and_then(Value::as_bool).unwrap_or(false);
            Some(PrPerson { login: login.to_owned(), avatar_url: avatar_url(pr_url, login, is_bot), is_bot })
        })
        .take(20)
        .collect()
}

/// `latestReviews` first, then each requested reviewer not already listed.
fn parse_reviews(value: &Value, pr_url: &str) -> Vec<PrReviewer> {
    let mut out: Vec<PrReviewer> = Vec::new();
    for v in value["latestReviews"].as_array().into_iter().flatten() {
        let login = v.pointer("/author/login").and_then(Value::as_str).unwrap_or_default();
        if login.is_empty() || out.iter().any(|r| r.login == login) {
            continue;
        }
        let is_bot = v.pointer("/author/is_bot").and_then(Value::as_bool).unwrap_or(false);
        out.push(PrReviewer { login: login.to_owned(), avatar_url: avatar_url(pr_url, login, is_bot), is_bot, state: text(v, "state") });
    }
    for v in value["reviewRequests"].as_array().into_iter().flatten() {
        let (login, is_team) = match v.get("login").and_then(Value::as_str) {
            Some(l) => (l, false),
            None => (v.get("name").and_then(Value::as_str).unwrap_or_default(), true),
        };
        if login.is_empty() || out.iter().any(|r| r.login == login) {
            continue;
        }
        let avatar = if is_team { None } else { avatar_url(pr_url, login, false) };
        out.push(PrReviewer { login: login.to_owned(), avatar_url: avatar, is_bot: false, state: "REQUESTED".into() });
    }
    out.truncate(100);
    out
}

/// The current head commit only. Action guards use this rather than `detail`, which
/// also reads reviews, labels and the comment count.
pub(crate) async fn head_sha(cwd: &Path, number: u64) -> Result<String> {
    validate_number(number)?;
    let json = run(cwd, "gh", &["pr", "view", &number.to_string(), "--json", "headRefOid"]).await?;
    let value: Value = serde_json::from_str(&json)?;
    let head = text(&value, "headRefOid");
    ensure!(!head.is_empty(), "Unable to read this pull request. Refresh and try again.");
    Ok(head)
}

/// GitHub's REST avatar URLs already carry `?v=4`; ask for a 40 px image.
fn sized_avatar(url: &str) -> String {
    let joiner = if url.contains('?') { '&' } else { '?' };
    format!("{url}{joiner}s=40")
}

pub async fn detail(cwd: &Path, number: u64) -> Result<PrDetailResult> {
    validate_number(number)?;
    let number = number.to_string();
    let view_args = [
        "pr",
        "view",
        &number,
        "--json",
        "number,title,url,state,headRefName,baseRefName,isDraft,author,updatedAt,body,headRefOid,reviewRequests,statusCheckRollup,changedFiles,createdAt,additions,deletions,reviewDecision,mergeable,mergeStateStatus,autoMergeRequest,assignees,labels,latestReviews,mergedAt,mergedBy,closedAt",
    ];
    let view = run(cwd, "gh", &view_args);
    let (json, graph) = tokio::join!(view, graph_detail(cwd, &number));
    let value: Value = serde_json::from_str(&json?)?;
    let mut pull_request = parse_pr_extended(&value).ok_or_else(|| anyhow!("Unable to read this pull request. Refresh and try again."))?;
    let reviews = parse_reviews(&value, &pull_request.url);
    let assignees = parse_people(&value, "assignees", &pull_request.url);
    let merge_state_status = pull_request.merge_state_status.clone();
    let (checks, checks_truncated, viewer, comment_count, timeline_counts, head_owner) = match graph {
        Some((page, truncated)) => {
            // GraphQL reads every context with its requiredness; the summary follows it.
            pull_request.checks_summary = Some(crate::github::summarize_checks(&page.nodes));
            let checks = page.nodes.iter().map(parse_check).collect::<Vec<_>>();
            (checks, truncated, Some(page.viewer), page.comment_count, page.timeline_counts, page.head_owner)
        }
        None => {
            let rollup = value["statusCheckRollup"].as_array();
            let truncated = rollup.is_some_and(|a| a.len() > MAX_CHECK_PAGES * 100);
            let checks = rollup.into_iter().flatten().take(MAX_CHECK_PAGES * 100).map(parse_check).collect();
            (checks, truncated, None, None, None, None)
        }
    };
    let behind_by = match merge_state_status.as_deref() {
        Some("BEHIND") => behind_by(cwd, &pull_request.base, &pull_request.head, head_owner.as_deref()).await,
        _ => None,
    };
    Ok(PrDetailResult {
        pull_request,
        body: text(&value, "body"),
        head_sha: text(&value, "headRefOid"),
        changed_files: value["changedFiles"].as_u64().unwrap_or(0) as u32,
        reviewers: value["reviewRequests"]
            .as_array()
            .into_iter()
            .flatten()
            .take(100)
            .map(|v| v.get("login").or_else(|| v.get("name")).and_then(Value::as_str).unwrap_or_default().to_owned())
            .collect(),
        checks,
        merge_state_status,
        reviews,
        comment_count,
        merged_at: value.get("mergedAt").and_then(Value::as_str).and_then(|s| s.parse().ok()),
        merged_by: value.pointer("/mergedBy/login").and_then(Value::as_str).map(str::to_owned),
        closed_at: value.get("closedAt").and_then(Value::as_str).and_then(|s| s.parse().ok()),
        assignees,
        viewer,
        behind_by,
        checks_truncated,
        timeline_counts,
    })
}

pub async fn page(cwd: &Path, p: &PrPageParams) -> Result<PrPageResult> {
    validate_number(p.number)?;
    ensure!((1..=1000).contains(&p.page), "Choose a page between 1 and 1000.");
    if matches!(p.kind, PrPageKind::Timeline) {
        return timeline_page(cwd, p).await;
    }
    if matches!(p.kind, PrPageKind::Checks) {
        let head = head_sha(cwd, p.number).await?;
        let checks_endpoint = format!("repos/{{owner}}/{{repo}}/commits/{head}/check-runs?per_page={PAGE_SIZE}&page={}", p.page);
        let status_endpoint = format!("repos/{{owner}}/{{repo}}/commits/{head}/status?per_page={PAGE_SIZE}&page={}", p.page);
        let checks_args = ["api", checks_endpoint.as_str(), "--method", "GET"];
        let status_args = ["api", status_endpoint.as_str(), "--method", "GET"];
        let (checks, statuses) = tokio::try_join!(run(cwd, "gh", &checks_args), run(cwd, "gh", &status_args))?;
        let checks: Value = serde_json::from_str(&checks)?;
        let statuses: Value = serde_json::from_str(&statuses)?;
        return parse_checks(&checks, &statuses, p.page);
    }
    let endpoint = match p.kind {
        PrPageKind::Files => format!("repos/{{owner}}/{{repo}}/pulls/{}/files", p.number),
        PrPageKind::Comments => format!("repos/{{owner}}/{{repo}}/issues/{}/comments", p.number),
        PrPageKind::Reviews => format!("repos/{{owner}}/{{repo}}/pulls/{}/reviews", p.number),
        PrPageKind::ReviewComments => format!("repos/{{owner}}/{{repo}}/pulls/{}/comments", p.number),
        PrPageKind::Checks | PrPageKind::Timeline => unreachable!("handled above"),
    };
    let endpoint = format!("{endpoint}?per_page={PAGE_SIZE}&page={}", p.page);
    let response = run(cwd, "gh", &["api", &endpoint, "--method", "GET"]).await?;
    let value: Value = serde_json::from_str(&response)?;
    parse_page(&value, p.kind, p.page)
}

const TIMELINE_PAGE: usize = 30;
const TIMELINE_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!,$first:Int!,$after:String){repository(owner:$owner,name:$name){pullRequest(number:$number){timelineItems(first:$first,after:$after,itemTypes:[ISSUE_COMMENT,PULL_REQUEST_REVIEW,PULL_REQUEST_COMMIT,HEAD_REF_FORCE_PUSHED_EVENT,REVIEW_REQUESTED_EVENT,READY_FOR_REVIEW_EVENT,CONVERT_TO_DRAFT_EVENT,MERGED_EVENT,CLOSED_EVENT,REOPENED_EVENT]){pageInfo{hasNextPage endCursor} nodes{__typename ... on IssueComment{id body url createdAt author{login avatarUrl(size:40) __typename}} ... on PullRequestReview{id state body url submittedAt createdAt author{login avatarUrl(size:40) __typename} comments(first:20){totalCount nodes{path line originalLine body url}}} ... on PullRequestCommit{id url commit{oid messageHeadline committedDate author{name user{login avatarUrl(size:40)}}}} ... on HeadRefForcePushedEvent{id createdAt actor{login avatarUrl(size:40) __typename} beforeCommit{oid} afterCommit{oid}} ... on ReviewRequestedEvent{id createdAt actor{login avatarUrl(size:40) __typename} requestedReviewer{__typename ... on User{login} ... on Bot{login} ... on Mannequin{login} ... on Team{name}}} ... on ReadyForReviewEvent{id createdAt actor{login avatarUrl(size:40) __typename}} ... on ConvertToDraftEvent{id createdAt actor{login avatarUrl(size:40) __typename}} ... on MergedEvent{id createdAt actor{login avatarUrl(size:40) __typename}} ... on ClosedEvent{id createdAt actor{login avatarUrl(size:40) __typename}} ... on ReopenedEvent{id createdAt actor{login avatarUrl(size:40) __typename}}}}}}}";

async fn timeline_page(cwd: &Path, p: &PrPageParams) -> Result<PrPageResult> {
    if let Some(cursor) = &p.cursor {
        ensure!(
            !cursor.is_empty()
                && cursor.len() <= 512
                && cursor.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'=' | b'-' | b'_')),
            "The timeline position is not valid. Reload the timeline."
        );
    }
    let number_field = format!("number={}", p.number);
    let first_field = format!("first={TIMELINE_PAGE}");
    let query = format!("query={TIMELINE_QUERY}");
    let after_field = p.cursor.as_ref().map(|c| format!("after={c}"));
    let mut args = vec![
        "api",
        "graphql",
        "-F",
        "owner={owner}",
        "-F",
        "name={repo}",
        "-F",
        number_field.as_str(),
        "-F",
        first_field.as_str(),
        "-f",
        query.as_str(),
    ];
    if let Some(field) = &after_field {
        args.extend(["-f", field.as_str()]);
    }
    let out = run(cwd, "gh", &args).await?;
    parse_timeline(&serde_json::from_str(&out)?, p.page)
}

fn actor(v: Option<&Value>) -> (String, Option<String>, bool) {
    let Some(v) = v.filter(|v| v.is_object()) else { return (String::new(), None, false) };
    let is_bot = v.get("__typename").and_then(Value::as_str) == Some("Bot");
    (text(v, "login"), opt_text(v, "avatarUrl"), is_bot)
}

fn timeline_event(v: &Value) -> Option<PrTimelineEvent> {
    let typename = v.get("__typename")?.as_str()?;
    let kind = match typename {
        "IssueComment" => PrTimelineKind::Comment,
        "PullRequestReview" => PrTimelineKind::Review,
        "PullRequestCommit" => PrTimelineKind::Commit,
        "HeadRefForcePushedEvent" => PrTimelineKind::ForcePush,
        "ReviewRequestedEvent" => PrTimelineKind::ReviewRequested,
        "ReadyForReviewEvent" => PrTimelineKind::ReadyForReview,
        "ConvertToDraftEvent" => PrTimelineKind::ConvertedToDraft,
        "MergedEvent" => PrTimelineKind::Merged,
        "ClosedEvent" => PrTimelineKind::Closed,
        "ReopenedEvent" => PrTimelineKind::Reopened,
        _ => return None,
    };
    let id = opt_text(v, "id")?;
    let (author, avatar, author_is_bot) = match kind {
        PrTimelineKind::Comment | PrTimelineKind::Review => actor(v.get("author")),
        PrTimelineKind::Commit => {
            let author = v.pointer("/commit/author");
            let user = author.and_then(|a| a.get("user")).filter(|u| u.is_object());
            match user {
                Some(u) => (text(u, "login"), opt_text(u, "avatarUrl"), false),
                None => (author.map(|a| text(a, "name")).unwrap_or_default(), None, false),
            }
        }
        _ => actor(v.get("actor")),
    };
    let created_at = match kind {
        PrTimelineKind::Commit => v.pointer("/commit/committedDate").and_then(Value::as_str).map(str::to_owned),
        PrTimelineKind::Review => opt_text(v, "submittedAt").or_else(|| opt_text(v, "createdAt")),
        _ => opt_text(v, "createdAt"),
    }?;
    let mut event = PrTimelineEvent {
        id,
        kind,
        created_at,
        author,
        avatar_url: avatar,
        author_is_bot,
        body: String::new(),
        state: None,
        url: opt_text(v, "url"),
        sha: None,
        before_sha: None,
        target: None,
        inline_comments: vec![],
        inline_count: None,
    };
    match kind {
        PrTimelineKind::Comment => event.body = bounded(text(v, "body"), 64 * 1024).0,
        PrTimelineKind::Review => {
            event.body = bounded(text(v, "body"), 64 * 1024).0;
            event.state = opt_text(v, "state");
            event.inline_count = v.pointer("/comments/totalCount").and_then(Value::as_u64).map(|n| n as u32);
            event.inline_comments = v
                .pointer("/comments/nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .take(20)
                .map(|c| PrTimelineInline {
                    path: text(c, "path"),
                    line: c.get("line").and_then(Value::as_u64).or_else(|| c.get("originalLine").and_then(Value::as_u64)).map(|n| n as u32),
                    body: bounded(text(c, "body"), 8 * 1024).0,
                    url: text(c, "url"),
                })
                .collect();
        }
        PrTimelineKind::Commit => {
            event.body = text(v.get("commit")?, "messageHeadline");
            event.sha = v.pointer("/commit/oid").and_then(Value::as_str).map(str::to_owned);
        }
        PrTimelineKind::ForcePush => {
            event.sha = v.pointer("/afterCommit/oid").and_then(Value::as_str).map(str::to_owned);
            event.before_sha = v.pointer("/beforeCommit/oid").and_then(Value::as_str).map(str::to_owned);
        }
        PrTimelineKind::ReviewRequested => {
            event.target = v.get("requestedReviewer").and_then(|r| opt_text(r, "login").or_else(|| opt_text(r, "name")));
        }
        _ => {}
    }
    Some(event)
}

/// The `timelineItems` answer: events oldest first, items of an unknown type dropped.
fn parse_timeline(root: &Value, page: u32) -> Result<PrPageResult> {
    let items = root
        .pointer("/data/repository/pullRequest/timelineItems")
        .filter(|v| v.is_object())
        .ok_or_else(|| anyhow!("Unable to read the timeline. Refresh and try again."))?;
    let has_more = items.pointer("/pageInfo/hasNextPage").and_then(Value::as_bool).unwrap_or(false);
    let next_cursor = if has_more { items.pointer("/pageInfo/endCursor").and_then(Value::as_str).map(str::to_owned) } else { None };
    Ok(PrPageResult {
        checks: vec![],
        files: vec![],
        entries: vec![],
        page,
        has_more: has_more && next_cursor.is_some(),
        timeline: items["nodes"].as_array().into_iter().flatten().take(TIMELINE_PAGE).filter_map(timeline_event).collect(),
        next_cursor,
    })
}

fn parse_page(value: &Value, kind: PrPageKind, page: u32) -> Result<PrPageResult> {
    let items = value.as_array().ok_or_else(|| anyhow!("Unable to read GitHub results. Refresh and try again."))?;
    let mut result = PrPageResult {
        checks: vec![],
        files: vec![],
        entries: vec![],
        page,
        has_more: items.len() == PAGE_SIZE,
        timeline: vec![],
        next_cursor: None,
    };
    for v in items.iter().take(PAGE_SIZE) {
        if matches!(kind, PrPageKind::Files) {
            let (patch, truncated) = bounded(text(v, "patch"), PATCH_BYTES);
            result.files.push(PrFile {
                path: text(v, "filename"),
                old_path: v["previous_filename"].as_str().map(str::to_owned),
                status: text(v, "status"),
                additions: v["additions"].as_u64().unwrap_or(0) as u32,
                deletions: v["deletions"].as_u64().unwrap_or(0) as u32,
                patch,
                patch_truncated: truncated || v.get("patch").is_none(),
                sha: opt_text(v, "sha"),
            });
        } else {
            result.entries.push(PrReviewEntry {
                id: v["id"].as_u64().unwrap_or(0),
                author: v.pointer("/user/login").and_then(Value::as_str).unwrap_or_default().to_owned(),
                body: bounded(text(v, "body"), 64 * 1024).0,
                state: text(v, "state"),
                path: v["path"].as_str().map(str::to_owned),
                line: v.get("line").or_else(|| v.get("original_line")).and_then(Value::as_u64).map(|n| n as u32),
                side: v["side"].as_str().map(str::to_owned),
                url: text(v, "html_url"),
                updated_at: v.get("updated_at").or_else(|| v.get("submitted_at")).and_then(Value::as_str).unwrap_or_default().to_owned(),
                avatar_url: v.pointer("/user/avatar_url").and_then(Value::as_str).map(sized_avatar),
                author_is_bot: v.pointer("/user/type").and_then(Value::as_str) == Some("Bot"),
            });
        }
    }
    Ok(result)
}

fn parse_checks(checks: &Value, statuses: &Value, page: u32) -> Result<PrPageResult> {
    let checks = checks["check_runs"].as_array().ok_or_else(|| anyhow!("Unable to read check runs. Refresh and try again."))?;
    let statuses = statuses["statuses"].as_array().ok_or_else(|| anyhow!("Unable to read commit statuses. Refresh and try again."))?;
    let mut result = PrPageResult {
        checks: vec![],
        files: vec![],
        entries: vec![],
        page,
        has_more: checks.len() == PAGE_SIZE || statuses.len() == PAGE_SIZE,
        timeline: vec![],
        next_cursor: None,
    };
    for value in checks.iter().take(PAGE_SIZE) {
        result.checks.push(PrCheck {
            name: text(value, "name"),
            status: text(value, "status"),
            conclusion: text(value, "conclusion"),
            url: text(value, "details_url"),
            is_required: None,
            workflow_name: None,
            started_at: opt_text(value, "started_at"),
            completed_at: opt_text(value, "completed_at"),
        });
    }
    for value in statuses.iter().take(PAGE_SIZE) {
        result.checks.push(PrCheck {
            name: text(value, "context"),
            status: text(value, "state"),
            conclusion: text(value, "state"),
            url: text(value, "target_url"),
            is_required: None,
            workflow_name: None,
            started_at: opt_text(value, "created_at"),
            completed_at: None,
        });
    }
    Ok(result)
}

async fn write_api(cwd: &Path, endpoint: &str, method: &str, value: Value) -> Result<()> {
    let mut child = crate::github::spawn(cwd, "gh", &["api", endpoint, "--method", method, "--input", "-"], std::process::Stdio::piped())?;
    child.stdin.take().ok_or_else(|| anyhow!("Unable to send the GitHub request."))?.write_all(&serde_json::to_vec(&value)?).await?;
    let output = child.wait_with_output().await?;
    ensure!(
        output.status.success(),
        "GitHub rejected this action: {}. Your draft is kept; refresh and try again.",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    if let Ok(response) = serde_json::from_slice::<Value>(&output.stdout)
        && response.get("merged").is_some_and(|v| v.as_bool() == Some(false))
    {
        return Err(anyhow!("{}. Refresh the pull request and try again.", text(&response, "message")));
    }
    Ok(())
}

/// `squash` (the default), `merge` or `rebase`.
fn merge_method(method: Option<&str>) -> Result<&'static str> {
    match method.map(str::trim).filter(|m| !m.is_empty()).unwrap_or("squash") {
        "squash" => Ok("squash"),
        "merge" => Ok("merge"),
        "rebase" => Ok("rebase"),
        other => Err(anyhow!("Choose squash, merge or rebase to merge with, not \"{other}\".")),
    }
}

/// Auto-merge waits only for what the branch rules require. When GitHub could merge now, `gh` would merge straight away.
fn ensure_auto_merge_waits(merge_state_status: &str) -> Result<()> {
    ensure!(
        !matches!(merge_state_status, "CLEAN" | "HAS_HOOKS" | "UNSTABLE"),
        "GitHub can merge this pull request now, so auto-merge has nothing to wait for. Refresh it, then merge it or wait for the checks to finish."
    );
    Ok(())
}

pub(crate) fn validate_checkout_head(expected: &str, current: &str) -> Result<()> {
    ensure!(!expected.is_empty(), "Refresh this pull request before checkout or repair.");
    ensure!(expected == current, "The pull request changed. Refresh it and check the selected findings before checkout or repair.");
    Ok(())
}

pub async fn action(cwd: &Path, p: &PrActionParams) -> Result<()> {
    validate_number(p.number)?;
    ensure!(p.body.len() <= 64 * 1024 && p.inline_comments.len() <= 100, "Keep the review below 64 KiB and 100 inline comments.");
    ensure!(
        p.body.len() + p.inline_comments.iter().map(|comment| comment.body.len()).sum::<usize>() <= 128 * 1024,
        "This review exceeds 128 KiB. Shorten your summary or submit fewer inline comments."
    );
    let pull = format!("repos/{{owner}}/{{repo}}/pulls/{}", p.number);
    match p.action {
        PrActionKind::Checkout => {
            let current = head_sha(cwd, p.number).await?;
            validate_checkout_head(&p.head_sha, &current)?;
            run(cwd, "gh", &["pr", "checkout", &p.number.to_string()]).await?;
        }
        PrActionKind::Close => write_api(cwd, &pull, "PATCH", json!({"state":"closed"})).await?,
        PrActionKind::Merge => {
            ensure!(!p.head_sha.is_empty(), "Refresh this pull request before merging.");
            let method = merge_method(p.merge_method.as_deref())?;
            if p.admin {
                // `gh` asks GitHub to bypass the rules; GitHub refuses unless the account may.
                run(
                    cwd,
                    "gh",
                    &["pr", "merge", &p.number.to_string(), "--admin", &format!("--{method}"), "--match-head-commit", &p.head_sha],
                )
                .await?;
            } else {
                write_api(cwd, &format!("{pull}/merge"), "PUT", json!({"sha":p.head_sha,"merge_method":method})).await?;
            }
        }
        PrActionKind::EnableAutoMerge => {
            ensure!(!p.head_sha.is_empty(), "Refresh this pull request before turning on auto-merge.");
            let method = merge_method(p.merge_method.as_deref())?;
            // `gh pr merge --auto` merges at once when GitHub could merge now (CLEAN, HAS_HOOKS, UNSTABLE), checks
            // still running or not: turning on auto-merge must never be an unconfirmed merge.
            let state = run(cwd, "gh", &["pr", "view", &p.number.to_string(), "--json", "mergeStateStatus"]).await?;
            ensure_auto_merge_waits(&text(&serde_json::from_str::<Value>(&state)?, "mergeStateStatus"))?;
            run(cwd, "gh", &["pr", "merge", &p.number.to_string(), "--auto", &format!("--{method}"), "--match-head-commit", &p.head_sha])
                .await?;
        }
        PrActionKind::DisableAutoMerge => {
            run(cwd, "gh", &["pr", "merge", &p.number.to_string(), "--disable-auto"]).await?;
        }
        PrActionKind::UpdateBranch => {
            ensure!(!p.head_sha.is_empty(), "Refresh this pull request before updating its branch.");
            write_api(cwd, &format!("{pull}/update-branch"), "PUT", json!({"expected_head_sha":p.head_sha})).await?;
        }
        PrActionKind::MarkReady => {
            run(cwd, "gh", &["pr", "ready", &p.number.to_string()]).await?;
        }
        PrActionKind::Comment if p.inline_comments.is_empty() => {
            ensure!(!p.body.trim().is_empty(), "Write a comment before posting.");
            write_api(cwd, &format!("repos/{{owner}}/{{repo}}/issues/{}/comments", p.number), "POST", json!({"body":p.body})).await?;
        }
        _ => {
            ensure!(!p.head_sha.is_empty(), "Refresh this pull request before submitting a review.");
            let current = head_sha(cwd, p.number).await?;
            ensure!(current == p.head_sha, "The pull request changed. Refresh it and check your draft before submitting.");
            if matches!(p.action, PrActionKind::RequestChanges) {
                ensure!(!p.body.trim().is_empty() || !p.inline_comments.is_empty(), "Describe the changes you need before submitting.");
            }
            let comments = p
                .inline_comments
                .iter()
                .map(|c| {
                    ensure!(
                        c.line > 0
                            && matches!(c.side.as_str(), "LEFT" | "RIGHT")
                            && !c.path.is_empty()
                            && !c.body.trim().is_empty()
                            && c.body.len() <= 64 * 1024,
                        "Choose a file, line, side and comment for each finding."
                    );
                    Ok(json!({"path":c.path,"line":c.line,"side":c.side,"body":c.body}))
                })
                .collect::<Result<Vec<_>>>()?;
            let event = match p.action {
                PrActionKind::Approve => "APPROVE",
                PrActionKind::RequestChanges => "REQUEST_CHANGES",
                _ => "COMMENT",
            };
            write_api(
                cwd,
                &format!("{pull}/reviews"),
                "POST",
                json!({"commit_id":p.head_sha,"event":event,"body":p.body,"comments":comments}),
            )
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pages_preserve_inline_identity_and_bound_large_patches() {
        let value = json!([{"id":7,"user":{"login":"reviewer"},"body":"fix this","path":"a.rs","original_line":12,"side":"LEFT","state":"CHANGES_REQUESTED"}]);
        let page = parse_page(&value, PrPageKind::ReviewComments, 2).unwrap();
        assert_eq!(page.entries[0].line, Some(12));
        assert_eq!(page.entries[0].side.as_deref(), Some("LEFT"));
        assert_eq!(page.page, 2);
        let value = json!([{"filename":"a.rs","patch":"é".repeat(PATCH_BYTES)}]);
        let page = parse_page(&value, PrPageKind::Files, 1).unwrap();
        assert!(page.files[0].patch_truncated);
        assert_eq!(page.files[0].patch.len(), PATCH_BYTES);
    }
    #[test]
    fn pages_carry_avatars_and_bot_flags() {
        let value = json!([{"id":1,"user":{"login":"ana","avatar_url":"https://avatars.githubusercontent.com/u/1?v=4","type":"User"}},
            {"id":2,"user":{"login":"ci","type":"Bot"}}]);
        let page = parse_page(&value, PrPageKind::Comments, 1).unwrap();
        assert_eq!(page.entries[0].avatar_url.as_deref(), Some("https://avatars.githubusercontent.com/u/1?v=4&s=40"));
        assert!(!page.entries[0].author_is_bot);
        assert!(page.entries[1].author_is_bot);
    }

    #[test]
    fn avatar_size_joins_any_query() {
        assert_eq!(sized_avatar("https://a.example/u/1?v=4"), "https://a.example/u/1?v=4&s=40");
        assert_eq!(sized_avatar("https://a.example/u/1"), "https://a.example/u/1?s=40");
    }

    #[test]
    fn reviews_merge_latest_and_requested_without_duplicates() {
        let value = json!({
            "latestReviews":[{"author":{"login":"ana"},"state":"APPROVED"},{"author":{"login":"bo"},"state":"CHANGES_REQUESTED"}],
            "reviewRequests":[{"login":"ana"},{"login":"cy"},{"name":"core-team","slug":"core-team"}],
        });
        let r = parse_reviews(&value, "https://github.com/o/r/pull/1");
        let got: Vec<_> = r.iter().map(|r| (r.login.as_str(), r.state.as_str())).collect();
        assert_eq!(got, [("ana", "APPROVED"), ("bo", "CHANGES_REQUESTED"), ("cy", "REQUESTED"), ("core-team", "REQUESTED")]);
        assert!(r[3].avatar_url.is_none());
    }

    /// Shaped like a recorded `DETAIL_QUERY` answer.
    fn detail_fixture(permission: &str, more_pages: bool) -> Value {
        let raw = r#"{"data":{"repository":{
            "viewerPermission":"@PERMISSION@","squashMergeAllowed":true,"mergeCommitAllowed":false,"rebaseMergeAllowed":true,
            "pullRequest":{
                "isDraft":true,"viewerCanUpdate":true,"viewerCanUpdateBranch":true,"viewerCanMergeAsAdmin":true,
                "viewerCanEnableAutoMerge":true,"viewerCanDisableAutoMerge":false,
                "headRepositoryOwner":{"login":"kybern"},
                "comments":{"totalCount":3},"reviewThreads":{"totalCount":1},
                "tlComments":{"filteredCount":3},"tlReviews":{"filteredCount":2},"tlCommits":{"filteredCount":9},
                "commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{
                    "totalCount":3,
                    "pageInfo":{"hasNextPage":@MORE@,"endCursor":"Ng"},
                    "nodes":[
                        {"__typename":"CheckRun","name":"Tests (1/4)","status":"COMPLETED","conclusion":"CANCELLED","detailsUrl":"https://github.com/o/r/actions/runs/1/job/2",
                         "startedAt":"2026-10-28T02:12:01Z","completedAt":"2026-10-28T02:14:50Z","isRequired":true,
                         "checkSuite":{"workflowRun":{"workflow":{"name":"CI"}}}},
                        {"__typename":"CheckRun","name":"lint","status":"IN_PROGRESS","conclusion":null,"detailsUrl":"https://github.com/o/r/actions/runs/1/job/3",
                         "startedAt":"2026-10-28T02:12:04Z","completedAt":null,"isRequired":false,"checkSuite":{"workflowRun":null}},
                        {"__typename":"StatusContext","context":"deploy/preview","state":"PENDING","targetUrl":"https://example.test/p","createdAt":"2026-10-28T02:13:00Z","isRequired":false}
                    ]}}}}]}
            }}}}"#;
        serde_json::from_str(&raw.replace("@PERMISSION@", permission).replace("@MORE@", &more_pages.to_string())).unwrap()
    }

    #[test]
    fn detail_page_reads_abilities_checks_and_counts() {
        let page = parse_detail_page(&detail_fixture("WRITE", false)).unwrap();
        assert!(page.viewer.can_merge && page.viewer.can_merge_as_admin && page.viewer.can_update_branch);
        assert!(page.viewer.can_enable_auto_merge && !page.viewer.can_disable_auto_merge);
        assert!(page.viewer.can_mark_ready, "a draft the viewer may edit can be marked ready");
        assert_eq!(page.viewer.merge_methods, ["squash", "rebase"]);
        assert_eq!(page.head_owner.as_deref(), Some("kybern"));
        assert_eq!(page.comment_count, Some(4));
        let counts = page.timeline_counts.unwrap();
        assert_eq!((counts.comments, counts.reviews, counts.commits), (3, 2, 9));
        assert!(!page.has_next);
        let checks: Vec<_> = page.nodes.iter().map(parse_check).collect();
        assert_eq!(checks.len(), 3);
        assert_eq!(checks[0].conclusion, "CANCELLED", "the raw conclusion is kept");
        assert_eq!((checks[0].is_required, checks[0].workflow_name.as_deref()), (Some(true), Some("CI")));
        assert_eq!(checks[0].started_at.as_deref(), Some("2026-10-28T02:12:01Z"));
        assert_eq!(checks[0].completed_at.as_deref(), Some("2026-10-28T02:14:50Z"));
        assert_eq!(
            (checks[1].is_required, checks[1].workflow_name.as_deref(), checks[1].completed_at.as_deref()),
            (Some(false), None, None)
        );
        assert_eq!(checks[2].name, "deploy/preview");
        assert_eq!((checks[2].status.as_str(), checks[2].conclusion.as_str()), ("", "PENDING"));
        assert_eq!(checks[2].started_at.as_deref(), Some("2026-10-28T02:13:00Z"));
        let summary = crate::github::summarize_checks(&page.nodes);
        assert_eq!((summary.failed, summary.pending, summary.passed), (1, 2, 0));
    }

    #[test]
    fn detail_page_without_write_access_cannot_merge_and_reports_more_pages() {
        let mut fixture = detail_fixture("READ", true);
        fixture["data"]["repository"]["pullRequest"]["isDraft"] = json!(false);
        let page = parse_detail_page(&fixture).unwrap();
        assert!(!page.viewer.can_merge && !page.viewer.can_mark_ready);
        assert!(page.has_next);
        assert_eq!(page.cursor.as_deref(), Some("Ng"));
        assert!(parse_detail_page(&json!({"data":{"repository":{"pullRequest":null}}})).is_none());
        assert!(parse_detail_page(&json!({"errors":[{"message":"nope"}]})).is_none());
    }

    #[test]
    fn view_rollup_checks_have_no_requiredness_but_keep_workflow_and_times() {
        let check =
            parse_check(&json!({"__typename":"CheckRun","name":"build","workflowName":"CI","status":"COMPLETED","conclusion":"TIMED_OUT",
            "detailsUrl":"u","startedAt":"2026-10-28T02:00:00Z","completedAt":"2026-10-28T02:30:00Z"}));
        assert_eq!((check.is_required, check.workflow_name.as_deref(), check.conclusion.as_str()), (None, Some("CI"), "TIMED_OUT"));
        let status = parse_check(
            &json!({"__typename":"StatusContext","context":"ci/x","state":"SUCCESS","targetUrl":"t","startedAt":"2026-10-28T02:00:00Z"}),
        );
        assert_eq!((status.name.as_str(), status.conclusion.as_str(), status.url.as_str()), ("ci/x", "SUCCESS", "t"));
    }

    #[test]
    fn people_carry_avatars_and_skip_bots() {
        let value = json!({"assignees":[{"login":"ana","is_bot":false},{"login":"renovate","is_bot":true},{"login":""}]});
        let people = parse_people(&value, "assignees", "https://github.com/o/r/pull/1");
        assert_eq!(people.len(), 2);
        assert_eq!(people[0].avatar_url.as_deref(), Some("https://github.com/ana.png?size=40"));
        assert!(people[1].is_bot && people[1].avatar_url.is_none());
    }

    #[test]
    fn refs_in_compare_urls_are_encoded() {
        assert_eq!(encode_ref("feat/a b#1?x"), "feat/a%20b%231%3Fx");
    }

    #[test]
    fn auto_merge_is_refused_when_github_would_merge_at_once() {
        for state in ["CLEAN", "HAS_HOOKS", "UNSTABLE"] {
            assert!(ensure_auto_merge_waits(state).unwrap_err().to_string().contains("nothing to wait for"), "{state}");
        }
        for state in ["BLOCKED", "BEHIND", "UNKNOWN", ""] {
            assert!(ensure_auto_merge_waits(state).is_ok(), "{state}");
        }
    }

    #[test]
    fn merge_methods_default_to_squash_and_reject_the_unknown() {
        assert_eq!(merge_method(None).unwrap(), "squash");
        assert_eq!(merge_method(Some("")).unwrap(), "squash");
        assert_eq!(merge_method(Some("rebase")).unwrap(), "rebase");
        assert_eq!(merge_method(Some("merge")).unwrap(), "merge");
        assert!(merge_method(Some("fast-forward")).unwrap_err().to_string().contains("fast-forward"));
    }

    /// Shaped like a recorded `TIMELINE_QUERY` answer.
    fn timeline_fixture() -> Value {
        serde_json::from_str(
            r#"{"data":{"repository":{"pullRequest":{"timelineItems":{
            "pageInfo":{"hasNextPage":true,"endCursor":"Y3Vyc29y"},
            "nodes":[
                {"__typename":"PullRequestCommit","id":"PURC_1","url":"https://github.com/o/r/pull/1/commits/abc",
                 "commit":{"oid":"abc","messageHeadline":"Add the thing","committedDate":"2026-10-28T02:04:25Z",
                           "author":{"name":"tidy","user":{"login":"tidy-dev","avatarUrl":"https://avatars.githubusercontent.com/u/1?s=40&v=4"}}}},
                {"__typename":"PullRequestCommit","id":"PURC_2","url":"u","commit":{"oid":"def","messageHeadline":"Fix","committedDate":"2026-10-28T02:05:00Z","author":{"name":"No Account","user":null}}},
                {"__typename":"ReviewRequestedEvent","id":"RRE_1","createdAt":"2026-10-28T02:05:59Z","actor":{"login":"tidy-dev","avatarUrl":"a","__typename":"User"},
                 "requestedReviewer":{"__typename":"User","login":"kevin"}},
                {"__typename":"ReviewRequestedEvent","id":"RRE_2","createdAt":"2026-10-28T02:06:00Z","actor":{"login":"tidy-dev","avatarUrl":"a","__typename":"User"},
                 "requestedReviewer":{"__typename":"Team","name":"core"}},
                {"__typename":"ReviewRequestedEvent","id":"RRE_3","createdAt":"2026-10-28T02:06:01Z","actor":null,"requestedReviewer":null},
                {"__typename":"IssueComment","id":"IC_1","body":"Looks good","url":"https://github.com/o/r/pull/1#issuecomment-1","createdAt":"2026-10-28T02:07:00Z",
                 "author":{"login":"ci-bot","avatarUrl":"b","__typename":"Bot"}},
                {"__typename":"PullRequestReview","id":"PRR_1","state":"CHANGES_REQUESTED","body":"Two things","url":"r","submittedAt":"2026-10-28T02:08:00Z","createdAt":"2026-10-28T02:07:30Z",
                 "author":{"login":"kevin","avatarUrl":"c","__typename":"User"},
                 "comments":{"totalCount":22,"nodes":[{"path":"a.rs","line":null,"originalLine":12,"body":"why","url":"c1"},{"path":"b.rs","line":3,"originalLine":3,"body":"nit","url":"c2"}]}},
                {"__typename":"HeadRefForcePushedEvent","id":"HRFP_1","createdAt":"2026-10-28T02:09:00Z","actor":{"login":"tidy-dev","avatarUrl":"a","__typename":"User"},
                 "beforeCommit":{"oid":"def"},"afterCommit":{"oid":"123"}},
                {"__typename":"ReadyForReviewEvent","id":"RFR_1","createdAt":"2026-10-28T02:10:00Z","actor":{"login":"tidy-dev","avatarUrl":"a","__typename":"User"}},
                {"__typename":"MergedEvent","id":"M_1","createdAt":"2026-10-28T02:11:00Z","actor":{"login":"kevin","avatarUrl":"c","__typename":"User"}},
                {"__typename":"SomethingNew","id":"X_1","createdAt":"2026-10-28T02:12:00Z"},
                {}
            ]}}}}}"#,
        )
        .unwrap()
    }

    #[test]
    fn timeline_keeps_order_kinds_and_drops_unknown_items() {
        let page = parse_timeline(&timeline_fixture(), 1).unwrap();
        let kinds: Vec<_> = page.timeline.iter().map(|e| e.kind).collect();
        use PrTimelineKind::*;
        assert_eq!(
            kinds,
            [Commit, Commit, ReviewRequested, ReviewRequested, ReviewRequested, Comment, Review, ForcePush, ReadyForReview, Merged]
        );
        assert!(page.has_more);
        assert_eq!(page.next_cursor.as_deref(), Some("Y3Vyc29y"));
        assert!(page.timeline.windows(2).all(|w| w[0].created_at <= w[1].created_at), "oldest first");
    }

    #[test]
    fn timeline_events_carry_authors_targets_and_inline_context() {
        let page = parse_timeline(&timeline_fixture(), 1).unwrap();
        let e = &page.timeline;
        assert_eq!((e[0].author.as_str(), e[0].body.as_str(), e[0].sha.as_deref()), ("tidy-dev", "Add the thing", Some("abc")));
        assert!(e[0].avatar_url.is_some());
        assert_eq!(
            (e[1].author.as_str(), e[1].avatar_url.clone()),
            ("No Account", None),
            "a commit by a non-user falls back to the git name"
        );
        assert_eq!((e[2].target.as_deref(), e[3].target.as_deref(), e[4].target.as_deref()), (Some("kevin"), Some("core"), None));
        assert_eq!(e[4].author, "");
        assert!(e[5].author_is_bot);
        assert_eq!(e[5].body, "Looks good");
        let review = &e[6];
        assert_eq!(review.state.as_deref(), Some("CHANGES_REQUESTED"));
        assert_eq!(review.created_at, "2026-10-28T02:08:00Z", "a review is dated by its submission");
        assert_eq!(review.inline_count, Some(22));
        assert_eq!(review.inline_comments.len(), 2);
        assert_eq!((review.inline_comments[0].path.as_str(), review.inline_comments[0].line), ("a.rs", Some(12)));
        assert_eq!((e[7].before_sha.as_deref(), e[7].sha.as_deref()), (Some("def"), Some("123")));
    }

    #[test]
    fn timeline_without_more_pages_has_no_cursor_and_errors_are_readable() {
        let value =
            json!({"data":{"repository":{"pullRequest":{"timelineItems":{"pageInfo":{"hasNextPage":false,"endCursor":"zz"},"nodes":[]}}}}});
        let page = parse_timeline(&value, 1).unwrap();
        assert!(!page.has_more && page.next_cursor.is_none() && page.timeline.is_empty());
        assert!(parse_timeline(&json!({"errors":[]}), 1).is_err());
    }

    #[test]
    fn files_carry_the_blob_sha() {
        let page = parse_page(
            &json!([{"filename":"a.rs","sha":"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391","patch":"@@"}, {"filename":"b.rs"}]),
            PrPageKind::Files,
            1,
        )
        .unwrap();
        assert_eq!(page.files[0].sha.as_deref(), Some("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"));
        assert!(page.files[1].sha.is_none());
    }

    #[test]
    fn exact_page_size_offers_following_page() {
        let value = json!(vec![json!({"filename":"a"}); PAGE_SIZE]);
        assert!(parse_page(&value, PrPageKind::Files, 1).unwrap().has_more);
        assert!(!parse_page(&json!([]), PrPageKind::Files, 2).unwrap().has_more);
    }

    #[test]
    fn checks_page_includes_legacy_contexts_and_offers_more_without_truncating() {
        let checks = json!({"check_runs":vec![json!({"name":"CI","status":"completed","conclusion":"success","details_url":"https://github.com/example/repo/actions/1"}); PAGE_SIZE]});
        let statuses = json!({"statuses":[{"context":"Build","state":"pending","target_url":"https://example.test/build"}]});
        let page = parse_checks(&checks, &statuses, 2).unwrap();
        assert_eq!(page.checks.len(), PAGE_SIZE + 1);
        assert!(page.has_more);
        assert_eq!(page.checks[PAGE_SIZE].name, "Build");
        assert_eq!(page.checks[PAGE_SIZE].conclusion, "pending");
    }

    #[test]
    fn paged_checks_carry_times() {
        let checks = json!({"check_runs":[{"name":"CI","status":"completed","conclusion":"success","details_url":"u","started_at":"2026-10-28T02:00:00Z","completed_at":"2026-10-28T02:01:00Z"}]});
        let page = parse_checks(&checks, &json!({"statuses":[]}), 1).unwrap();
        assert_eq!(
            (page.checks[0].started_at.as_deref(), page.checks[0].completed_at.as_deref()),
            (Some("2026-10-28T02:00:00Z"), Some("2026-10-28T02:01:00Z"))
        );
    }
}
