//! Notes and tasks readable from every machine (ADE-86 M5 / S2, design §9.2).
//!
//! Each machine owns its notes and tasks and is the only one that writes them. What this adds is reading:
//! `notes.list`, `notes.search` and `tasks.items.list` take `machine: "all"` (or one machine's name or
//! id), and the agent tools `kybern_notes_search` and `kybern_tasks_list` take the same. This machine asks
//! each online member in parallel, with a 5 s budget each, and merges the answers. A machine that is
//! offline, paused, too old or too slow is left out and named in a footer ("satyamas offline; its notes are
//! not included"), so a partial list never looks complete.
//!
//! * The RPCs ask members with `peer.federated.read` (`machine:tools` on the target, read-only) and merge
//!   typed results; [`Federation`] says which ids each machine contributed.
//! * The agent tools ask with `peer.tool.call`, the same audited path as every forwarded tool, and
//!   [`merge_tool`] merges the tools' JSON rows, labelling each with its machine.
//! * Writes are never federated: a note or task is changed on the machine that holds it, by naming that
//!   machine in `machine` (§6.3) or from a client connected to it. `machine: "all"` on a write is refused
//!   with a message that says so.

use std::time::Duration;

use kybern_protocol::methods::*;
use kybern_protocol::*;
use serde_json::{Value, json};

use crate::orchestrator::remote_delegation::{CallFailure, MemberView, RemoteHub};
use crate::state::AppState;

/// How long one machine has to answer a federated read.
pub const MEMBER_BUDGET: Duration = Duration::from_secs(5);

/// The agent tools that can read every machine.
pub fn is_federated_read(tool: &str) -> bool {
    matches!(tool, "kybern_notes_search" | "kybern_tasks_list")
}

/// Whether the arguments ask for every machine.
pub fn wants_all(arguments: &Value) -> bool {
    arguments.get("machine").and_then(Value::as_str).is_some_and(|machine| machine.trim().eq_ignore_ascii_case(MACHINE_ALL))
}

/// Why `machine: "all"` does not fit this tool.
pub fn refuse_all(tool: &str) -> String {
    match tool {
        "kybern_note_read" | "kybern_task_read" => {
            "Read one note or task from the machine that holds it: pass that machine's name, which every row of a search with machine \"all\" carries in `machine`.".into()
        }
        "kybern_note_create" | "kybern_note_append" | "kybern_note_update" | "kybern_task_create" | "kybern_task_update" => {
            "A note or task is written on the machine that holds it. Name that machine in `machine` (kybern_machines lists them), or leave `machine` out to write here.".into()
        }
        _ => format!("{tool} cannot run on every machine. Name one machine, or leave out `machine`."),
    }
}

// ---- the target side ----

/// `peer.federated.read`: this machine's notes or tasks, for another member.
pub async fn serve_read(state: &AppState, read: PeerFederatedReadParams) -> Result<PeerFederatedReadResult, RpcError> {
    let orchestrator = state.orchestrator.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<PeerFederatedReadResult> {
        let mut result = PeerFederatedReadResult::default();
        match read {
            PeerFederatedReadParams::NotesList => {
                result.notes_list = Some(NotesListResult {
                    notes: orchestrator.notes_list()?,
                    history_since: orchestrator.notes_history_since()?,
                    federation: None,
                });
            }
            PeerFederatedReadParams::NotesSearch { query, limit } => {
                result.notes_search = Some(NotesSearchResult { results: orchestrator.notes_search(&query, limit)?, federation: None });
            }
            PeerFederatedReadParams::TaskItemsList => {
                result.task_items = Some(TaskItemsListResult { tasks: orchestrator.task_items_list()?, federation: None });
            }
        }
        Ok(result)
    })
    .await
    .map_err(RpcError::internal)?
    .map_err(RpcError::internal)
}

// ---- who to ask ----

/// What the caller wants read: every other machine, or one.
enum Wanted {
    All,
    One(String),
}

fn wanted(machine: &str) -> Wanted {
    if machine.trim().eq_ignore_ascii_case(MACHINE_ALL) { Wanted::All } else { Wanted::One(machine.trim().to_string()) }
}

/// The other members to ask, and those that cannot be asked, for a read that needs `feature`.
struct Asking {
    ask: Vec<MemberView>,
    skipped: Vec<FederationSkipped>,
}

fn who_to_ask(hub: &dyn RemoteHub, wanted: &Wanted) -> Result<Asking, RpcError> {
    let members = hub.members();
    let mut others: Vec<MemberView> = members.into_iter().filter(|member| !member.is_self).collect();
    others.sort_by_key(|member| member.name.to_lowercase());
    let chosen: Vec<MemberView> = match wanted {
        Wanted::All => others,
        Wanted::One(query) => {
            let target = hub.resolve(query).map_err(|reason| RpcError::new(codes::NOT_FOUND, reason))?;
            others.into_iter().filter(|member| member.id == target.id).collect()
        }
    };
    let mut asking = Asking { ask: Vec::new(), skipped: Vec::new() };
    for member in chosen {
        let skip = |reason| FederationSkipped { machine_id: member.id.clone(), machine_name: member.name.clone(), reason };
        match member.status {
            "paused" => asking.skipped.push(skip(SkipReason::Paused)),
            "online" if !member.features.is_empty() && !member.features.iter().any(|f| f == features::NOTES_FEDERATED_V1) => {
                asking.skipped.push(skip(SkipReason::Unsupported));
            }
            "online" => asking.ask.push(member),
            _ => asking.skipped.push(skip(SkipReason::Offline)),
        }
    }
    Ok(asking)
}

fn skip_reason(failure: &CallFailure) -> SkipReason {
    match failure {
        CallFailure::Offline(_) | CallFailure::Gone(_) => SkipReason::Offline,
        CallFailure::Remote(error) if error.code == codes::METHOD_NOT_FOUND => SkipReason::Unsupported,
        CallFailure::Remote(error) if error.code == codes::FORBIDDEN => SkipReason::Refused,
        CallFailure::Remote(_) => SkipReason::Failed,
    }
}

/// "satyamas offline; its notes are not included." Nothing when no machine was left out.
pub fn footer(skipped: &[FederationSkipped], noun: &str) -> Option<String> {
    if skipped.is_empty() {
        return None;
    }
    let phrase = |item: &FederationSkipped| {
        let why = match item.reason {
            SkipReason::Offline => "offline",
            SkipReason::Paused => "paused",
            SkipReason::Unsupported => "needs a newer Kybern",
            SkipReason::Refused => "has sharing turned off",
            SkipReason::TimedOut => "did not answer in time",
            SkipReason::Failed => "could not be read",
        };
        format!("{} {why}", item.machine_name)
    };
    let list = skipped.iter().map(phrase).collect::<Vec<_>>().join(", ");
    let pronoun = if skipped.len() == 1 { "its" } else { "their" };
    Some(format!("{list}; {pronoun} {noun} are not included."))
}

/// Ask every machine in `asking` the same read, in parallel, each within [`MEMBER_BUDGET`].
async fn ask_all(hub: &dyn RemoteHub, asking: &mut Asking, read: &PeerFederatedReadParams) -> Vec<(MemberView, PeerFederatedReadResult)> {
    let params = serde_json::to_value(read).unwrap_or(Value::Null);
    let calls = asking.ask.iter().map(|member| {
        let params = params.clone();
        async move {
            let call = hub.call(&member.id, PeerFederatedRead::NAME, params);
            match tokio::time::timeout(MEMBER_BUDGET, call).await {
                Err(_) => Err(SkipReason::TimedOut),
                Ok(Err(failure)) => Err(skip_reason(&failure)),
                Ok(Ok(value)) => serde_json::from_value::<PeerFederatedReadResult>(value).map_err(|_| SkipReason::Failed),
            }
        }
    });
    let answers = futures::future::join_all(calls).await;
    let mut taken = Vec::new();
    for (member, answer) in std::mem::take(&mut asking.ask).into_iter().zip(answers) {
        match answer {
            Ok(result) => taken.push((member, result)),
            Err(reason) => asking.skipped.push(FederationSkipped { machine_id: member.id, machine_name: member.name, reason }),
        }
    }
    taken
}

/// A single machine that was asked for by name but could not answer is an error, not a footer.
fn named_unreachable(wanted: &Wanted, asking: &Asking) -> Result<(), RpcError> {
    if let (Wanted::One(_), Some(skipped)) = (wanted, asking.skipped.first()) {
        let why = match skipped.reason {
            SkipReason::Offline => "is offline",
            SkipReason::Paused => "is paused in your network",
            SkipReason::Unsupported => "needs a newer Kybern to share its notes and tasks",
            SkipReason::Refused => "does not let this machine read its notes and tasks (its Tools setting is off)",
            SkipReason::TimedOut => "did not answer in time",
            SkipReason::Failed => "could not be read",
        };
        let message = format!("{} {why}.", skipped.machine_name);
        let error = RpcError::new(codes::NOT_AVAILABLE, message.clone());
        return Err(match skipped.reason {
            SkipReason::Offline | SkipReason::TimedOut => {
                error.with_problem(&crate::problem::machine_offline(&skipped.machine_name, message))
            }
            _ => error,
        });
    }
    Ok(())
}

struct Plan {
    hub: std::sync::Arc<dyn RemoteHub>,
    asking: Asking,
    wanted: Wanted,
}

fn plan(state: &AppState, machine: &str) -> Result<Plan, RpcError> {
    let hub = state
        .orchestrator
        .remote_hub()
        .cloned()
        .ok_or_else(|| RpcError::new(codes::NOT_AVAILABLE, "the machine network is not available on this machine"))?;
    let wanted = wanted(machine);
    // Naming this machine reads only here.
    if let Wanted::One(query) = &wanted
        && (query == &hub.self_id() || query.eq_ignore_ascii_case(&hub.self_name()))
    {
        return Ok(Plan { hub, asking: Asking { ask: Vec::new(), skipped: Vec::new() }, wanted: Wanted::All });
    }
    let asking = who_to_ask(&*hub, &wanted)?;
    named_unreachable(&wanted, &asking)?;
    Ok(Plan { hub, asking, wanted })
}

/// The sections of a merged answer, this machine first (when it is part of the answer) and the others by
/// name; the order the items were merged in.
fn finish_federation(mut sections: Vec<FederationSection>, skipped: Vec<FederationSkipped>, noun: &str) -> Federation {
    sections.sort_by(|a, b| b.is_self.cmp(&a.is_self).then_with(|| a.machine_name.to_lowercase().cmp(&b.machine_name.to_lowercase())));
    let footer = footer(&skipped, noun);
    Federation { sections, skipped, footer }
}

// ---- the RPC side ----

/// `notes.list` with `machine`: `local` is this machine's answer.
pub async fn notes_list(state: &AppState, machine: Option<String>, local: NotesListResult) -> Result<NotesListResult, RpcError> {
    let Some(machine) = machine.filter(|machine| !machine.trim().is_empty()) else { return Ok(local) };
    let Plan { hub, mut asking, wanted } = plan(state, &machine)?;
    let answers = ask_all(&*hub, &mut asking, &PeerFederatedReadParams::NotesList).await;
    let include_self = matches!(wanted, Wanted::All);
    let mut sections = Vec::new();
    let mut notes = Vec::new();
    let mut history_since = None;
    if include_self {
        sections.push(FederationSection {
            machine_id: hub.self_id(),
            machine_name: hub.self_name(),
            is_self: true,
            ids: local.notes.iter().map(|note| note.id).collect(),
        });
        history_since = local.history_since;
        notes.extend(local.notes);
    }
    for (member, result) in answers {
        let Some(list) = result.notes_list else { continue };
        sections.push(FederationSection {
            machine_id: member.id,
            machine_name: member.name,
            is_self: false,
            ids: list.notes.iter().map(|note| note.id).collect(),
        });
        history_since = match (history_since, list.history_since) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        notes.extend(list.notes);
    }
    let federation = finish_federation(sections, asking.skipped, "notes");
    Ok(NotesListResult { notes, history_since, federation: Some(federation) })
}

/// `notes.search` with `machine`.
pub async fn notes_search(
    state: &AppState,
    machine: Option<String>,
    query: &str,
    limit: Option<u32>,
    local: NotesSearchResult,
) -> Result<NotesSearchResult, RpcError> {
    let Some(machine) = machine.filter(|machine| !machine.trim().is_empty()) else { return Ok(local) };
    let Plan { hub, mut asking, wanted } = plan(state, &machine)?;
    let read = PeerFederatedReadParams::NotesSearch { query: query.to_string(), limit };
    let answers = ask_all(&*hub, &mut asking, &read).await;
    let cap = limit.unwrap_or(50).clamp(1, 200) as usize;
    let mut sections = Vec::new();
    let mut parts: Vec<Vec<NoteSearchHit>> = Vec::new();
    if matches!(wanted, Wanted::All) {
        sections.push(FederationSection { machine_id: hub.self_id(), machine_name: hub.self_name(), is_self: true, ids: Vec::new() });
        parts.push(local.results);
    }
    for (member, result) in answers {
        let Some(search) = result.notes_search else { continue };
        sections.push(FederationSection { machine_id: member.id, machine_name: member.name, is_self: false, ids: Vec::new() });
        parts.push(search.results);
    }
    // Each machine ranked its own hits; take them in turns so a full local list cannot hide the others.
    let results = interleave(parts.iter().map(|part| part.as_slice()).collect(), cap);
    for (section, part) in sections.iter_mut().zip(&parts) {
        section.ids = part.iter().filter(|hit| results.iter().any(|kept| kept.id == hit.id)).map(|hit| hit.id).collect();
    }
    let federation = finish_federation(sections, asking.skipped, "notes");
    Ok(NotesSearchResult { results, federation: Some(federation) })
}

/// `tasks.items.list` with `machine`.
pub async fn task_items_list(
    state: &AppState,
    machine: Option<String>,
    local: TaskItemsListResult,
) -> Result<TaskItemsListResult, RpcError> {
    let Some(machine) = machine.filter(|machine| !machine.trim().is_empty()) else { return Ok(local) };
    let Plan { hub, mut asking, wanted } = plan(state, &machine)?;
    let answers = ask_all(&*hub, &mut asking, &PeerFederatedReadParams::TaskItemsList).await;
    let mut sections = Vec::new();
    let mut tasks = Vec::new();
    if matches!(wanted, Wanted::All) {
        sections.push(FederationSection {
            machine_id: hub.self_id(),
            machine_name: hub.self_name(),
            is_self: true,
            ids: local.tasks.iter().map(|task| task.id).collect(),
        });
        tasks.extend(local.tasks);
    }
    for (member, result) in answers {
        let Some(list) = result.task_items else { continue };
        sections.push(FederationSection {
            machine_id: member.id,
            machine_name: member.name,
            is_self: false,
            ids: list.tasks.iter().map(|task| task.id).collect(),
        });
        tasks.extend(list.tasks);
    }
    let federation = finish_federation(sections, asking.skipped, "tasks");
    Ok(TaskItemsListResult { tasks, federation: Some(federation) })
}

/// Up to `cap` items, one from each part in turn, each part keeping its own order.
fn interleave<T: Clone>(parts: Vec<&[T]>, cap: usize) -> Vec<T> {
    let mut taken = Vec::new();
    let longest = parts.iter().map(|part| part.len()).max().unwrap_or(0);
    'rounds: for round in 0..longest {
        for part in &parts {
            if let Some(item) = part.get(round) {
                if taken.len() == cap {
                    break 'rounds;
                }
                taken.push(item.clone());
            }
        }
    }
    taken
}

// ---- the agent-tool side ----

/// One machine's answer to a tool call, ready to merge.
pub struct ToolPart {
    pub machine_id: String,
    pub machine_name: String,
    pub is_self: bool,
    pub value: Value,
}

/// Merge `kybern_notes_search` / `kybern_tasks_list` results from several machines. Every row gets the
/// `machine` and `machine_id` it came from, machines are taken in turns up to `limit`, and the machines
/// that could not be read are named in `footer`.
pub fn merge_tool(tool: &str, mut parts: Vec<ToolPart>, skipped: &[FederationSkipped], limit: usize) -> Value {
    let (key, noun) = if tool == "kybern_notes_search" { ("notes", "notes") } else { ("tasks", "tasks") };
    parts.sort_by(|a, b| b.is_self.cmp(&a.is_self).then_with(|| a.machine_name.to_lowercase().cmp(&b.machine_name.to_lowercase())));
    let rows: Vec<Vec<Value>> = parts
        .iter()
        .map(|part| {
            let mut rows = part.value.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
            for row in &mut rows {
                if let Some(object) = row.as_object_mut() {
                    object.insert("machine".into(), json!(part.machine_name));
                    object.insert("machine_id".into(), json!(part.machine_id));
                }
            }
            rows
        })
        .collect();
    let total: usize = rows.iter().map(Vec::len).sum();
    let merged = interleave(rows.iter().map(Vec::as_slice).collect(), limit);
    let truncated = merged.len() < total || parts.iter().any(|part| part.value.get("truncated").and_then(Value::as_bool) == Some(true));
    let machines: Vec<Value> = parts
        .iter()
        .zip(&rows)
        .map(|(part, rows)| json!({ "machine": part.machine_name, "id": part.machine_id, "count": rows.len() }))
        .collect();
    let mut result = json!({
        key: merged,
        "truncated": truncated,
        "machines": machines,
        "hint": "Each row's `machine` is the machine that holds it. Read or change a row there by passing that name in `machine`.",
    });
    if !skipped.is_empty() {
        result["skipped"] =
            json!(skipped.iter().map(|item| json!({ "machine": item.machine_name, "reason": item.reason })).collect::<Vec<_>>());
        result["footer"] = json!(footer(skipped, noun));
    }
    result
}

/// The machines a tool call with `machine: "all"` asks, and those it cannot.
pub fn tool_targets(hub: &dyn RemoteHub) -> Result<(Vec<MemberView>, Vec<FederationSkipped>), RpcError> {
    who_to_ask(hub, &Wanted::All).map(|asking| (asking.ask, asking.skipped))
}

/// A tool call that did not answer, as a footer entry.
pub fn tool_skip(member: &MemberView, failure: &CallFailure) -> FederationSkipped {
    FederationSkipped { machine_id: member.id.clone(), machine_name: member.name.clone(), reason: skip_reason(failure) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skipped(name: &str, reason: SkipReason) -> FederationSkipped {
        FederationSkipped { machine_id: format!("id-{name}"), machine_name: name.into(), reason }
    }

    #[test]
    fn the_footer_names_what_was_left_out() {
        assert_eq!(footer(&[], "notes"), None);
        assert_eq!(footer(&[skipped("satyamas", SkipReason::Offline)], "notes").unwrap(), "satyamas offline; its notes are not included.");
        assert_eq!(
            footer(&[skipped("a", SkipReason::Offline), skipped("b", SkipReason::TimedOut)], "tasks").unwrap(),
            "a offline, b did not answer in time; their tasks are not included."
        );
        assert!(footer(&[skipped("old", SkipReason::Unsupported)], "notes").unwrap().contains("needs a newer Kybern"));
    }

    #[test]
    fn machines_are_taken_in_turns_up_to_the_limit() {
        let a = [1, 2, 3, 4];
        let b = [10, 20];
        assert_eq!(interleave(vec![&a[..], &b[..]], 5), vec![1, 10, 2, 20, 3]);
        assert_eq!(interleave(vec![&a[..], &b[..]], 100), vec![1, 10, 2, 20, 3, 4]);
        assert_eq!(interleave::<i32>(vec![], 3), Vec::<i32>::new());
    }

    #[test]
    fn a_merged_tool_result_labels_rows_and_names_the_machines_it_could_not_read() {
        let part = |name: &str, is_self: bool, titles: &[&str]| ToolPart {
            machine_id: format!("id-{name}"),
            machine_name: name.into(),
            is_self,
            value: json!({ "notes": titles.iter().map(|title| json!({ "id": title, "title": title })).collect::<Vec<_>>() }),
        };
        let merged = merge_tool(
            "kybern_notes_search",
            vec![part("zeta", false, &["z1"]), part("mac", true, &["m1", "m2"]), part("alpha", false, &["a1"])],
            &[skipped("satyamas", SkipReason::Offline)],
            10,
        );
        let titles: Vec<&str> = merged["notes"].as_array().unwrap().iter().map(|row| row["title"].as_str().unwrap()).collect();
        assert_eq!(titles, ["m1", "a1", "z1", "m2"], "this machine first, then by name, in turns");
        assert_eq!(merged["notes"][0]["machine"], "mac");
        assert_eq!(merged["notes"][1]["machine"], "alpha");
        assert_eq!(merged["footer"], "satyamas offline; its notes are not included.");
        assert_eq!(merged["machines"][1]["count"], 1);
        assert_eq!(merged["truncated"], false);
        let capped = merge_tool("kybern_notes_search", vec![part("mac", true, &["m1", "m2", "m3"]), part("b", false, &["b1"])], &[], 2);
        assert_eq!(capped["truncated"], true);
        assert!(capped.get("footer").is_none());
    }

    #[test]
    fn all_means_every_machine_only_for_the_two_reads() {
        assert!(is_federated_read("kybern_notes_search") && is_federated_read("kybern_tasks_list"));
        assert!(!is_federated_read("kybern_note_read") && !is_federated_read("kybern_note_create"));
        assert!(wants_all(&json!({"machine": " ALL "})) && !wants_all(&json!({"machine": "satyamas"})) && !wants_all(&json!({})));
        assert!(refuse_all("kybern_note_create").contains("written on the machine that holds it"));
        assert!(refuse_all("kybern_task_read").contains("machine"));
        assert!(refuse_all("kybern_run_in_terminal").contains("cannot run on every machine"));
    }
}
