//! Actionable errors: what a failure means for the person using Kybern, as the `Problem` an RPC
//! error carries in `data.problem`.
//!
//! `problem_from` reads typed errors first (a program that could not start, a harness that isn't
//! installed, a machine that is offline, a store conflict, a refused file) and falls back to
//! `internal` with the whole error chain as the detail. Clients write the words from `kind` and
//! `subject`; the daemon's `title`/`hint` are only their fallback.

use kybern_drivers::DriverError;
use kybern_protocol::process::SpawnError;
use kybern_protocol::rpc::{Problem, ProblemAction, ProblemKind, RpcError, codes};
use std::io::ErrorKind;

use crate::orchestrator::remote_delegation::CallFailure;

/// The `Problem` for `error`: its typed meaning when Kybern knows one, else `internal`.
pub(crate) fn problem_from(error: &anyhow::Error) -> Problem {
    typed(error).unwrap_or_else(|| Problem::new(ProblemKind::Internal, detail(error)))
}

/// The full error chain ("run git: program not found"), never just the outermost context.
pub(crate) fn detail(error: &anyhow::Error) -> String {
    format!("{error:#}")
}

/// An RPC error with `code`, the error chain as its message (or plain words for a missing
/// program) and `problem_from` as its `data.problem`.
pub(crate) fn rpc_error(code: i32, error: &anyhow::Error) -> RpcError {
    let problem = problem_from(error);
    let message = match (problem.kind, problem.subject.as_deref()) {
        (ProblemKind::ToolMissing, Some(tool)) => format!("{} ({})", tool_missing_sentence(tool), detail(error)),
        _ => detail(error),
    };
    RpcError::new(code, message).with_problem(&problem)
}

/// The meaning of `error` when one of its causes says it, or `None`.
pub(crate) fn typed(error: &anyhow::Error) -> Option<Problem> {
    let detail = detail(error);
    for cause in error.chain() {
        if let Some(spawn) = cause.downcast_ref::<SpawnError>() {
            return Some(from_spawn(spawn, detail));
        }
        if let Some(problem) = cause.downcast_ref::<DriverError>().and_then(|driver| from_driver(driver, &detail)) {
            return Some(problem);
        }
        if let Some(call) = cause.downcast_ref::<CallFailure>() {
            return from_call(call, detail);
        }
        if let Some(problem) = from_store(cause, &detail) {
            return Some(problem);
        }
        if cause.is::<crate::files::WriteConflict>() {
            return Some(Problem::new(ProblemKind::Conflict, detail));
        }
        if let Some(problem) = cause.downcast_ref::<std::io::Error>().and_then(|io| from_io(io, &detail)) {
            return Some(problem);
        }
    }
    // `gh`'s own stderr is text; its sign-in failure is the one worth a button.
    let lower = detail.to_ascii_lowercase();
    if lower.contains("gh auth login") || lower.contains("not logged in to any github hosts") {
        return Some(crate::github::gh_problem(crate::github::GhErrorKind::GhAuth, detail));
    }
    None
}

fn from_spawn(spawn: &SpawnError, detail: String) -> Problem {
    let tool = spawn.tool();
    match spawn.kind() {
        ErrorKind::NotFound if tool == "gh" => crate::github::gh_problem(crate::github::GhErrorKind::GhMissing, detail),
        ErrorKind::NotFound => tool_missing(&tool, detail),
        ErrorKind::PermissionDenied => Problem::new(ProblemKind::Permission, detail).with_subject(tool.clone()).with_copy(
            format!("{} isn't allowed to run", tool_label(&tool)),
            "This computer blocked Kybern from starting it. Check its permissions, then try again.",
        ),
        _ => Problem::new(ProblemKind::Internal, detail).with_subject(tool),
    }
}

fn from_driver(error: &DriverError, detail: &str) -> Option<Problem> {
    match error {
        // `BinaryNotFound` carries a binary name, a path, or (for Cursor's runtime) a whole sentence.
        DriverError::BinaryNotFound(name) if is_program_name(name) => {
            Some(tool_missing(&kybern_protocol::process::tool_name(name), detail.to_string()))
        }
        DriverError::BinaryNotFound(_) => Some(Problem::new(ProblemKind::ToolMissing, detail)),
        DriverError::VersionTooOld { .. } => Some(
            Problem::new(ProblemKind::UpdateNeeded, detail)
                .with_copy("Update this agent", "Its installed version is older than Kybern needs. Update it, then try again."),
        ),
        DriverError::Io(io) => from_io(io, detail),
        _ => None,
    }
}

/// Another machine of the network can't be reached. `machine` is its name, which clients show.
pub(crate) fn machine_offline(machine: &str, detail: impl Into<String>) -> Problem {
    offline(detail).with_subject(machine)
}

fn offline(detail: impl Into<String>) -> Problem {
    Problem::new(ProblemKind::Offline, detail)
        .with_action(ProblemAction::retry("Try again"))
        .with_secondary(ProblemAction::new("open_machines", "Machine settings"))
}

fn from_call(failure: &CallFailure, detail: String) -> Option<Problem> {
    match failure {
        CallFailure::Offline(_) | CallFailure::Gone(_) => Some(offline(detail)),
        // The other machine's own problem, when it sent one.
        CallFailure::Remote(remote) => remote.problem(),
    }
}

fn from_store(cause: &(dyn std::error::Error + 'static), detail: &str) -> Option<Problem> {
    use kybern_store::{NoteError, TaskError};
    let kind = match (cause.downcast_ref::<NoteError>(), cause.downcast_ref::<TaskError>()) {
        (Some(NoteError::Conflict), _) | (_, Some(TaskError::Conflict)) => ProblemKind::Conflict,
        (Some(NoteError::NotFound(_)), _) | (_, Some(TaskError::NotFound(_))) => ProblemKind::NotFound,
        (Some(NoteError::Deleted | NoteError::Invalid(_)), _) | (_, Some(TaskError::Invalid(_))) => ProblemKind::InvalidInput,
        _ => return None,
    };
    Some(Problem::new(kind, detail))
}

fn from_io(error: &std::io::Error, detail: &str) -> Option<Problem> {
    match error.kind() {
        ErrorKind::PermissionDenied => Some(Problem::new(ProblemKind::Permission, detail).with_copy(
            "Kybern isn't allowed to do this",
            "This computer refused access. Check the file or folder's permissions, then try again.",
        )),
        ErrorKind::NotFound => Some(Problem::new(ProblemKind::NotFound, detail)),
        _ => None,
    }
}

/// `tool` isn't installed or can't be found: install it, then check again.
fn tool_missing(tool: &str, detail: String) -> Problem {
    let label = tool_label(tool);
    Problem::new(ProblemKind::ToolMissing, detail)
        .with_subject(tool)
        .with_copy(format!("Install {label}"), tool_missing_sentence(tool))
        .with_action(ProblemAction::new("install_tool", format!("Install {label}")).with_params(serde_json::json!({ "tool": tool })))
        .with_secondary(ProblemAction::retry("Check again"))
}

fn tool_missing_sentence(tool: &str) -> String {
    format!("{} isn't installed on this computer, or Kybern can't find it. Install it, then try again.", tool_label(tool))
}

/// How people name `tool`.
fn tool_label(tool: &str) -> String {
    match tool {
        "gh" => "GitHub CLI".into(),
        "git" => "Git".into(),
        "tailscale" => "Tailscale".into(),
        "node" | "npm" | "npx" => "Node.js".into(),
        "claude" => "Claude Code".into(),
        "codex" => "Codex".into(),
        "opencode" => "OpenCode".into(),
        "cursor-agent" | "cursor" => "Cursor".into(),
        "agy" | "antigravity" => "Antigravity".into(),
        "grok" => "Grok Build".into(),
        "pi" => "Pi".into(),
        "omp" => "Oh My Pi".into(),
        other => other.into(),
    }
}

/// A binary name or path rather than a sentence.
fn is_program_name(text: &str) -> bool {
    (!text.is_empty() && !text.contains(' ')) || std::path::Path::new(text).is_absolute()
}

/// The JSON-RPC code that fits a typed problem better than the caller's text guess, if any.
pub(crate) fn code_for(kind: ProblemKind) -> Option<i32> {
    match kind {
        ProblemKind::ToolMissing | ProblemKind::SignedOut => Some(codes::PROVIDER_UNAVAILABLE),
        ProblemKind::NotFound => Some(codes::NOT_FOUND),
        ProblemKind::Conflict => Some(codes::CONFLICT),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn(program: &str, kind: ErrorKind) -> anyhow::Error {
        anyhow::Error::new(SpawnError::new(program, std::io::Error::new(kind, "program not found")))
    }

    #[test]
    fn a_program_that_cannot_start_is_a_missing_tool_with_its_name() {
        let error = spawn("git", ErrorKind::NotFound).context("checkpoint");
        let problem = problem_from(&error);
        assert_eq!((problem.kind, problem.subject.as_deref()), (ProblemKind::ToolMissing, Some("git")));
        let action = problem.action.unwrap();
        assert_eq!((action.id.as_str(), action.label.as_str()), ("install_tool", "Install Git"));
        assert_eq!(action.params, Some(serde_json::json!({ "tool": "git" })));
        assert_eq!(problem.secondary.unwrap().id, "retry");
        // The cause survives every context around it.
        assert_eq!(problem.detail.as_deref(), Some("checkpoint: run git: program not found"));
        assert!(problem.retryable);
        // A full Windows path names the same tool.
        assert_eq!(problem_from(&spawn(r"C:\Tools\Tailscale\tailscale.exe", ErrorKind::NotFound)).subject.as_deref(), Some("tailscale"));
    }

    #[test]
    fn a_missing_gh_keeps_its_own_copy_and_sign_in_is_signed_out() {
        let missing = problem_from(&spawn("gh", ErrorKind::NotFound));
        assert_eq!((missing.kind, missing.subject.as_deref()), (ProblemKind::ToolMissing, Some("gh")));
        assert_eq!(missing.action.unwrap().label, "Install GitHub CLI");
        // `friendly` reads the same type.
        let (message, problem) = crate::github::friendly_problem(spawn("gh", ErrorKind::NotFound).context("github.pr.list"));
        assert!(message.starts_with(crate::github::GH_MISSING), "{message}");
        assert_eq!(problem.unwrap().kind, ProblemKind::ToolMissing);

        let signed_out = problem_from(&anyhow::anyhow!("gh pr list: To get started with GitHub CLI, please run:  gh auth login"));
        assert_eq!((signed_out.kind, signed_out.subject.as_deref()), (ProblemKind::SignedOut, Some("gh")));
        assert_eq!(signed_out.action.unwrap().id, "sign_in");
    }

    #[test]
    fn permission_denied_is_permission() {
        let problem = problem_from(&spawn("git", ErrorKind::PermissionDenied));
        assert_eq!((problem.kind, problem.subject.as_deref()), (ProblemKind::Permission, Some("git")));
        let io = anyhow::Error::new(std::io::Error::from(ErrorKind::PermissionDenied)).context("write C:\\work\\a.txt");
        assert_eq!(problem_from(&io).kind, ProblemKind::Permission);
    }

    #[test]
    fn harness_errors_are_typed() {
        let missing = problem_from(&anyhow::Error::new(DriverError::BinaryNotFound("claude".into())));
        assert_eq!((missing.kind, missing.subject.as_deref()), (ProblemKind::ToolMissing, Some("claude")));
        assert_eq!(missing.action.unwrap().label, "Install Claude Code");
        let sentence = problem_from(&anyhow::Error::new(DriverError::BinaryNotFound(
            "Cursor’s SDK installs with npm, which comes with Node.js.".into(),
        )));
        assert_eq!((sentence.kind, sentence.subject), (ProblemKind::ToolMissing, None));
        let old = DriverError::VersionTooOld { found: "1.0".into(), required: "2.0".into() };
        assert_eq!(problem_from(&anyhow::Error::new(old)).kind, ProblemKind::UpdateNeeded);
    }

    #[test]
    fn an_offline_machine_is_offline() {
        let problem = problem_from(&anyhow::Error::new(CallFailure::Offline("SatyaMas is offline".into())));
        assert_eq!(problem.kind, ProblemKind::Offline);
        assert_eq!(problem.action.unwrap().id, "retry");
        assert_eq!(problem.secondary.unwrap().id, "open_machines");
    }

    #[test]
    fn store_errors_keep_their_meaning() {
        assert_eq!(problem_from(&anyhow::Error::new(kybern_store::NoteError::Conflict)).kind, ProblemKind::Conflict);
        assert_eq!(problem_from(&anyhow::Error::new(kybern_store::TaskError::NotFound("task"))).kind, ProblemKind::NotFound);
        assert_eq!(problem_from(&anyhow::Error::new(kybern_store::TaskError::Invalid("title".into()))).kind, ProblemKind::InvalidInput);
    }

    #[test]
    fn invalid_params_are_invalid_input() {
        let error = RpcError::invalid_params("missing field `thread_id`").ensure_problem();
        assert_eq!(error.problem().unwrap().kind, ProblemKind::InvalidInput);
    }

    #[test]
    fn anything_else_is_internal_with_the_whole_chain() {
        let error = anyhow::anyhow!("database is locked").context("save thread").context("threads.rename");
        let problem = problem_from(&error);
        assert_eq!(problem.kind, ProblemKind::Internal);
        assert_eq!(problem.detail.as_deref(), Some("threads.rename: save thread: database is locked"));
        let rpc = rpc_error(codes::INTERNAL_ERROR, &error);
        assert_eq!(rpc.message, "threads.rename: save thread: database is locked");
        assert_eq!(rpc.problem().unwrap().kind, ProblemKind::Internal);
    }

    #[test]
    fn a_missing_tool_reads_as_words_for_old_clients() {
        let rpc = rpc_error(codes::INVALID_PARAMS, &spawn("git", ErrorKind::NotFound));
        assert_eq!(
            rpc.message,
            "Git isn't installed on this computer, or Kybern can't find it. Install it, then try again. (run git: program not found)"
        );
    }
}
