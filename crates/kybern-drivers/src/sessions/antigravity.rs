//! Antigravity (`agy`) saved conversations, read from the CLI's own store under
//! `~/.gemini/antigravity-cli/`. Nothing there is ever written:
//!
//! - `conversation_summaries.db` (SQLite, WAL mode) lists the conversations. It is
//!   read from a private copy (the `-wal` file first, then the database) so the
//!   original never gets a `-shm` file or a checkpoint from us.
//! - `brain/<conversation>/.system_generated/logs/transcript.jsonl` holds one step
//!   per line. A rewritten step appears twice; the last row wins. Fields cut short
//!   (`truncated_fields`) are read from `transcript_full.jsonl`.
//!
//! Only main conversations are imported; sub-agent conversations (those with a
//! `parent_conversation_id`) are skipped. Tool cards go through the live driver's
//! `tools::normalize` and use its ids (`agy:<conversation>:<step>`), so imported and
//! live cards match and a resumed thread continues with `--conversation <id>`.
use super::*;
use crate::antigravity::tools;
use rusqlite::types::Value as Sql;
use rusqlite::{Connection, OpenFlags};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Component;

const STORE: &str = ".gemini/antigravity-cli";
const SUMMARIES: &str = "conversation_summaries.db";
/// How much of a transcript is searched for a folder when the summary has none.
const CWD_SCAN_BYTES: u64 = 4 * 1024 * 1024;
const TRUNCATED_NOTE: &str = "\n\n[Cut short in the original transcript.]";

pub(super) fn root(context: &ProbeContext) -> Result<PathBuf> {
    Ok(home(context)?.join(STORE))
}

pub(super) async fn list(context: &ProbeContext, cursor: Option<&str>, query: &str) -> Result<SessionsListResult> {
    let root = root(context)?;
    let context = context.clone();
    let start = offset(cursor)?;
    let query = query.to_string();
    tokio::task::spawn_blocking(move || {
        let rows = load_rows(&root)?;
        Ok(page(&root, &rows, start, &context, &query))
    })
    .await
    .map_err(|e| DriverError::Other(e.into()))?
}

pub(super) async fn read(context: &ProbeContext, id: &str) -> Result<SessionHistory> {
    let root = root(context)?;
    let id = id.to_string();
    tokio::task::spawn_blocking(move || read_row(&root, &load_rows(&root)?, &id)).await.map_err(|e| DriverError::Other(e.into()))?
}

/// One snapshot of the conversation list, kept across the pages and reads of a scan or import.
pub(super) struct Reader {
    root: PathBuf,
    context: ProbeContext,
    rows: Option<Arc<Vec<Row>>>,
}

impl Reader {
    pub(super) fn new(context: &ProbeContext) -> Result<Self> {
        Ok(Self { root: root(context)?, context: context.clone(), rows: None })
    }

    async fn rows(&mut self) -> Result<Arc<Vec<Row>>> {
        if let Some(rows) = &self.rows {
            return Ok(rows.clone());
        }
        let root = self.root.clone();
        let rows = Arc::new(tokio::task::spawn_blocking(move || load_rows(&root)).await.map_err(|e| DriverError::Other(e.into()))??);
        self.rows = Some(rows.clone());
        Ok(rows)
    }
}

#[async_trait::async_trait]
impl SessionReader for Reader {
    async fn list_page(&mut self, cursor: Option<&str>) -> Result<SessionsListResult> {
        let start = offset(cursor)?;
        let rows = self.rows().await?;
        let (root, context) = (self.root.clone(), self.context.clone());
        tokio::task::spawn_blocking(move || page(&root, &rows, start, &context, "")).await.map_err(|e| DriverError::Other(e.into()))
    }

    async fn read(&mut self, id: &str) -> Result<SessionHistory> {
        let rows = self.rows().await?;
        let (root, id) = (self.root.clone(), id.to_string());
        tokio::task::spawn_blocking(move || read_row(&root, &rows, &id)).await.map_err(|e| DriverError::Other(e.into()))?
    }

    async fn close(self: Box<Self>) {}
}

// ---- the conversation list ----

/// A main conversation from `conversation_summaries`.
struct Row {
    id: String,
    title: String,
    updated_at: DateTime<Utc>,
    /// The first workspace URI, still encoded.
    workspace: Option<String>,
}

/// Removes the private copy of the database when the read is done.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("kybern-agy-{}", Uuid::new_v4()));
        #[cfg(unix)]
        {
            let mut builder = std::fs::DirBuilder::new();
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder.create(&dir)?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A plain byte copy: the destination is writable even when the source is not.
fn copy_file(from: &Path, to: &Path) -> std::io::Result<()> {
    std::io::copy(&mut std::fs::File::open(from)?, &mut std::fs::File::create(to)?).map(drop)
}

fn db_error(error: rusqlite::Error) -> DriverError {
    DriverError::Protocol(format!("Unable to read the Antigravity conversation list: {error}. Close Antigravity and try again."))
}

fn load_rows(root: &Path) -> Result<Vec<Row>> {
    let database = root.join(SUMMARIES);
    if !database.is_file() {
        return Ok(Vec::new());
    }
    let scratch = Scratch::new()?;
    let copy = scratch.0.join(SUMMARIES);
    // The write-ahead log first: a checkpoint between the two copies then leaves the
    // database newer than the log, which replays harmlessly. The other order loses rows.
    match copy_file(&root.join(format!("{SUMMARIES}-wal")), &scratch.0.join(format!("{SUMMARIES}-wal"))) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    copy_file(&database, &copy)?;
    // Read-write is safe: this is our copy, and SQLite needs it to rebuild the shared-memory index.
    let connection =
        Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(db_error)?;
    let columns = {
        let mut statement = connection.prepare("PRAGMA table_info(conversation_summaries)").map_err(db_error)?;
        let names = statement.query_map([], |row| row.get::<_, String>(1)).map_err(db_error)?;
        names.collect::<std::result::Result<HashSet<_>, _>>().map_err(db_error)?
    };
    if !columns.contains("conversation_id") {
        return Err(DriverError::Protocol("The Antigravity conversation list has an unknown layout. Update Kybern and try again.".into()));
    }
    // Columns this version of agy lacks read as NULL.
    let wanted = ["conversation_id", "title", "preview", "last_modified_time", "workspace_uris", "parent_conversation_id"];
    let select = wanted.map(|name| if columns.contains(name) { name } else { "NULL" }).join(", ");
    let mut statement = connection.prepare(&format!("SELECT {select} FROM conversation_summaries")).map_err(db_error)?;
    let mut records = statement.query([]).map_err(db_error)?;
    let mut rows = Vec::new();
    while let Some(record) = records.next().map_err(db_error)? {
        let get = |index: usize| record.get::<_, Sql>(index).unwrap_or(Sql::Null);
        let Some(id) = sql_text(&get(0)).filter(|id| valid_id(id)) else { continue };
        if sql_text(&get(5)).is_some() {
            continue;
        }
        let transcript = logs(root, &id).join("transcript.jsonl");
        let Ok(metadata) = transcript.metadata() else { continue };
        let title = sql_text(&get(1)).or_else(|| sql_text(&get(2))).unwrap_or_default();
        let updated_at = sql_time(&get(3)).or_else(|| metadata.modified().ok().map(DateTime::<Utc>::from)).unwrap_or_else(Utc::now);
        rows.push(Row { id, title, updated_at, workspace: workspace(&get(4)) });
    }
    rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.id.cmp(&b.id)));
    Ok(rows)
}

/// Ids name a folder under `brain/`, so only the characters of a UUID-like id are accepted.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn logs(root: &Path, id: &str) -> PathBuf {
    root.join("brain").join(id).join(".system_generated/logs")
}

fn sql_text(value: &Sql) -> Option<String> {
    let text = match value {
        Sql::Text(text) => text.clone(),
        Sql::Blob(bytes) => String::from_utf8(bytes.clone()).ok()?,
        _ => return None,
    };
    Some(text.trim().to_string()).filter(|text| !text.is_empty())
}

/// An epoch in seconds, milliseconds, microseconds or nanoseconds, or a date text.
fn sql_time(value: &Sql) -> Option<DateTime<Utc>> {
    let epoch = |number: f64| {
        let seconds = if number >= 1e17 {
            number / 1e9
        } else if number >= 1e14 {
            number / 1e6
        } else if number >= 1e11 {
            number / 1e3
        } else {
            number
        };
        (seconds > 0.0).then(|| DateTime::from_timestamp(seconds.floor() as i64, (seconds.fract() * 1e9) as u32)).flatten()
    };
    match value {
        Sql::Integer(number) => epoch(*number as f64),
        Sql::Real(number) => epoch(*number),
        Sql::Text(text) => {
            let text = text.trim();
            text.parse::<DateTime<Utc>>()
                .ok()
                .or_else(|| text.parse::<f64>().ok().and_then(epoch))
                .or_else(|| chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f").ok().map(|naive| naive.and_utc()))
        }
        _ => None,
    }
}

/// The first of `workspace_uris`, a JSON array of `file://` URIs (a bare URI also reads).
fn workspace(value: &Sql) -> Option<String> {
    let text = sql_text(value)?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Array(uris)) => uris.iter().find_map(|uri| uri.as_str().map(str::to_string)),
        Ok(Value::String(uri)) => Some(uri),
        _ => Some(text),
    }
    .filter(|uri| !uri.trim().is_empty())
}

// ---- folders ----

/// The folder a `file://` URI names. Windows drive letters (`file:///C:/x`, also with an
/// encoded colon) come back as `C:/x`, and a host other than `localhost` as `//host/x`.
pub(super) fn uri_path(uri: &str) -> Option<String> {
    let uri = uri.trim();
    let Some(rest) = uri.get(..7).filter(|scheme| scheme.eq_ignore_ascii_case("file://")).map(|_| &uri[7..]) else {
        // A bare path, in either style: the summary may name a folder of another OS than this one's.
        let bytes = uri.as_bytes();
        let windows = bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/');
        return (uri.starts_with('/') || windows || Path::new(uri).is_absolute()).then(|| uri.to_string());
    };
    let (authority, path) = rest.find('/').map_or((rest, ""), |at| rest.split_at(at));
    let path = percent_decode(path);
    let bytes = path.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
        && bytes.get(3).is_none_or(|b| *b == b'/');
    let path = if drive {
        path[1..].to_string()
    } else if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
        format!("//{authority}{path}")
    } else {
        path
    };
    let trimmed = path.trim_end_matches('/');
    Some(if trimmed.is_empty() { path } else { trimmed.to_string() }).filter(|path| !path.is_empty())
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
        if bytes[at] == b'%'
            && at + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[at + 1]), hex(bytes[at + 2]))
        {
            out.push(high << 4 | low);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Gives a path the spelling the disk uses. agy records the URI the editor sent, and a
/// case-insensitive disk (macOS, Windows) accepts any case, which would split one
/// repository into several projects.
#[derive(Default)]
struct CaseResolver {
    listings: HashMap<PathBuf, Option<Vec<OsString>>>,
}

impl CaseResolver {
    fn resolve(&mut self, path: &Path) -> PathBuf {
        let mut out = PathBuf::new();
        for component in path.components() {
            let Component::Normal(name) = component else {
                // A drive letter has no case on disk; one spelling keeps `c:` and `C:` in one project.
                match component {
                    Component::Prefix(prefix) if matches!(prefix.kind(), std::path::Prefix::Disk(_)) => {
                        out.push(component.as_os_str().to_string_lossy().to_uppercase())
                    }
                    _ => out.push(component.as_os_str()),
                }
                continue;
            };
            let names = self
                .listings
                .entry(out.clone())
                .or_insert_with(|| std::fs::read_dir(&out).ok().map(|entries| entries.flatten().map(|entry| entry.file_name()).collect()));
            let lower = name.to_string_lossy().to_lowercase();
            let on_disk = names.as_ref().and_then(|names| {
                names
                    .iter()
                    .find(|candidate| *candidate == name)
                    .or_else(|| names.iter().find(|c| c.to_string_lossy().to_lowercase() == lower))
            });
            out.push(on_disk.map_or(name, |found| found.as_os_str()));
        }
        out
    }
}

/// The `Cwd` of the first shell command in the transcript, for a summary without a workspace.
fn command_cwd(transcript: &Path) -> Option<String> {
    let file = std::fs::File::open(transcript).ok()?;
    let mut text = String::new();
    BufReader::new(file).take(CWD_SCAN_BYTES).read_to_string(&mut text).ok()?;
    text.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()).find_map(|step| {
        step["tool_calls"].as_array()?.iter().filter(|call| call["name"] == "run_command").find_map(|call| {
            let args = tools::decode_args(call["args"].as_object()?);
            let (_, input) = tools::normalize("run_command", &args, |_| None);
            input["cwd"].as_str().map(str::to_string)
        })
    })
}

fn summarize(root: &Path, row: &Row, case: &mut CaseResolver) -> SavedSession {
    let cwd = row
        .workspace
        .as_deref()
        .and_then(uri_path)
        .or_else(|| command_cwd(&logs(root, &row.id).join("transcript.jsonl")))
        .map(|path| case.resolve(Path::new(&path)).to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = title(&row.title);
    SavedSession {
        provider: ProviderKind::Antigravity,
        id: row.id.clone(),
        title: if name.is_empty() { "Untitled conversation".into() } else { name },
        cwd,
        updated_at: row.updated_at,
        model: None,
        thread_id: None,
    }
}

fn page(root: &Path, rows: &[Row], start: usize, context: &ProbeContext, query: &str) -> SessionsListResult {
    let mut case = CaseResolver::default();
    let mut sessions = Vec::new();
    let mut next_cursor = None;
    for (index, row) in rows.iter().enumerate().skip(start) {
        let session = summarize(root, row, &mut case);
        if matches_cwd(&session, context) && matches_query(&session, query) {
            sessions.push(session);
        }
        if sessions.len() == PAGE_SIZE {
            next_cursor = (index + 1 < rows.len()).then(|| (index + 1).to_string());
            break;
        }
    }
    SessionsListResult { sessions, next_cursor }
}

// ---- the transcript ----

fn read_row(root: &Path, rows: &[Row], id: &str) -> Result<SessionHistory> {
    let row = valid_id(id)
        .then(|| rows.iter().find(|row| row.id == id))
        .flatten()
        .ok_or_else(|| DriverError::Protocol("Conversation was not found. Refresh the session list.".into()))?;
    let session = summarize(root, row, &mut CaseResolver::default());
    let steps = load_steps(&logs(root, id))?;
    Ok(parse(session, &steps))
}

/// Step rows of one JSONL file. A line that does not parse is skipped, except an
/// unfinished last line when `live`: agy is writing it right now.
fn read_rows(path: &Path, live: bool) -> Result<Vec<Value>> {
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > MAX_HISTORY_BYTES {
        return Err(DriverError::Unsupported(
            "This transcript is too large to import (64 MB limit). Start a shorter conversation in Antigravity and resume that one.".into(),
        ));
    }
    let mut text = String::new();
    BufReader::new(file).take(MAX_HISTORY_BYTES + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_HISTORY_BYTES {
        return Err(DriverError::Protocol("Session changed while reading. Stop the original session and try again.".into()));
    }
    let mut rows = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        match serde_json::from_str::<Value>(line) {
            Ok(row) => rows.push(row),
            Err(_) if live && !text.ends_with('\n') && text.ends_with(line) => {
                return Err(DriverError::Protocol(format!("{STILL_WRITING} Stop the original session and try again.")));
            }
            Err(_) => {}
        }
    }
    Ok(rows)
}

/// The last row per `step_index`, in step order: a rewritten step appears twice.
pub(super) fn by_step(rows: Vec<Value>) -> BTreeMap<u64, Value> {
    rows.into_iter().filter_map(|row| Some((row["step_index"].as_u64()?, row))).collect()
}

pub(super) fn load_steps(logs: &Path) -> Result<BTreeMap<u64, Value>> {
    let mut steps = by_step(read_rows(&logs.join("transcript.jsonl"), true)?);
    let cut = |step: &Value| step["truncated_fields"].as_array().is_some_and(|fields| !fields.is_empty());
    if !steps.values().any(cut) {
        return Ok(steps);
    }
    // The untruncated rows: `transcript_full.jsonl`, then whatever `chunks/` holds, later files winning.
    let mut full = BTreeMap::new();
    let mut sources = vec![logs.join("transcript_full.jsonl")];
    if let Ok(entries) = std::fs::read_dir(logs.join("chunks")) {
        let mut chunks: Vec<_> =
            entries.flatten().map(|entry| entry.path()).filter(|path| path.extension().is_some_and(|ext| ext == "jsonl")).collect();
        chunks.sort();
        sources.extend(chunks);
    }
    for source in sources {
        if let Ok(rows) = read_rows(&source, false) {
            full.extend(by_step(rows));
        }
    }
    for (index, step) in steps.iter_mut().filter(|(_, step)| cut(step)) {
        let fields = step["truncated_fields"].as_array().cloned().unwrap_or_default();
        for field in fields.iter().filter_map(Value::as_str) {
            match full.get(index).and_then(|row| row.get(field)).filter(|value| !value.is_null()) {
                Some(value) => step[field] = value.clone(),
                None if field == "content" => {
                    let text = step["content"].as_str().unwrap_or("");
                    step["content"] = Value::String(format!("{text}{TRUNCATED_NOTE}"));
                }
                None => {}
            }
        }
    }
    Ok(steps)
}

/// What a step of this type answers, by the tool that was called.
fn result_types(tool: &str) -> Option<&'static [&'static str]> {
    Some(match tool {
        "view_file" | "read_file" => &["VIEW_FILE"],
        "list_dir" => &["LIST_DIRECTORY"],
        "grep_search" => &["GREP_SEARCH"],
        "write_to_file" | "write_file" | "replace_file_content" | "multi_replace_file_content" | "edit_file" | "sed_file" => {
            &["CODE_ACTION"]
        }
        "run_command" => &["RUN_COMMAND"],
        "search_web" => &["SEARCH_WEB"],
        "read_url_content" => &["READ_URL_CONTENT"],
        _ => return None,
    })
}

/// Types of steps that are not the result of a tool call.
const NOT_RESULTS: [&str; 6] = ["USER_INPUT", "PLANNER_RESPONSE", "SYSTEM_MESSAGE", "CONVERSATION_HISTORY", "ERROR_MESSAGE", "CHECKPOINT"];

fn kind(step: &Value) -> &str {
    step["type"].as_str().unwrap_or("")
}

fn text_of<'a>(step: &'a Value, key: &str) -> &'a str {
    step[key].as_str().unwrap_or("")
}

/// The text an `error` field carries: a string, or an object with a message.
fn error_text(step: &Value) -> Option<String> {
    let error = &step["error"];
    error
        .as_str()
        .or_else(|| error["message"].as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .or_else(|| (!error.is_null() && !error.is_string() && error["message"].is_null()).then(|| error.to_string()))
}

/// The result step of the `call_index`th call of the planner step at `position`: the first
/// unclaimed step before the next turn of the conversation that fits the tool.
fn result_of(ordered: &[(&u64, &Value)], position: usize, tool: &str, claimed: &HashSet<u64>) -> Option<usize> {
    let window = ordered[position + 1..].iter().take_while(|(_, step)| !matches!(kind(step), "USER_INPUT" | "PLANNER_RESPONSE"));
    let candidates: Vec<usize> = window
        .enumerate()
        .filter(|(_, (index, step))| !claimed.contains(index) && !NOT_RESULTS.contains(&kind(step)))
        .map(|(offset, _)| position + 1 + offset)
        .collect();
    let wanted = result_types(tool);
    let known = |step: &Value| result_types_known(kind(step));
    candidates
        .iter()
        .copied()
        .find(|at| wanted.is_some_and(|types| types.contains(&kind(ordered[*at].1))))
        .or_else(|| candidates.iter().copied().find(|at| kind(ordered[*at].1) == "GENERIC"))
        .or_else(|| if wanted.is_none() { candidates.iter().copied().find(|at| !known(ordered[*at].1)) } else { None })
}

fn result_types_known(kind: &str) -> bool {
    ["VIEW_FILE", "LIST_DIRECTORY", "GREP_SEARCH", "CODE_ACTION", "RUN_COMMAND", "SEARCH_WEB", "READ_URL_CONTENT"].contains(&kind)
}

pub(super) fn parse(session: SavedSession, steps: &BTreeMap<u64, Value>) -> SessionHistory {
    let ordered: Vec<(&u64, &Value)> = steps.iter().collect();
    let mut history = History::default();
    let mut claimed = HashSet::new();
    let mut at = session.updated_at;
    for (position, (index, step)) in ordered.iter().enumerate() {
        at = timestamp(&step["created_at"]).unwrap_or(at);
        match kind(step) {
            "USER_INPUT" => {
                let text = text_of(step, "content");
                if !text.trim().is_empty() {
                    history.user(at, UserMessage::text(text));
                }
            }
            "PLANNER_RESPONSE" => {
                let thinking = Some(text_of(step, "thinking").to_string()).filter(|thinking| !thinking.trim().is_empty());
                history.assistant(at, text_of(step, "content").to_string(), thinking);
                for (n, call) in step["tool_calls"].as_array().into_iter().flatten().enumerate() {
                    let Some(name) = call["name"].as_str().filter(|name| !name.is_empty()) else { continue };
                    let args = call["args"].as_object().map(tools::decode_args).unwrap_or(Value::Null);
                    let (kybern_name, input) = tools::normalize(name, &args, |_| None);
                    let hidden = tools::reads_own_mcp_schema(&kybern_name, &input);
                    let result = result_of(&ordered, position, name, &claimed);
                    let id = match result {
                        Some(at) => format!("agy:{}:{}", session.id, ordered[at].0),
                        None => format!("agy:{}:{}.{n}", session.id, index),
                    };
                    if let Some(at) = result {
                        claimed.insert(*ordered[at].0);
                    }
                    if hidden {
                        continue;
                    }
                    history.tool(at, &id, &kybern_name, input);
                    let Some(result) = result.map(|at| ordered[at].1) else { continue };
                    // A step still running never finished; it ends the turn as interrupted.
                    if text_of(result, "status") == "RUNNING" {
                        continue;
                    }
                    let output = Some(text_of(result, "content").to_string())
                        .filter(|output| !output.is_empty())
                        .or_else(|| error_text(result))
                        .unwrap_or_default();
                    let is_error = text_of(result, "status") == "ERROR" || error_text(result).is_some() || tools::exited_nonzero(&output);
                    let done = timestamp(&result["created_at"]).unwrap_or(at);
                    history.result(done, &id, Value::String(output), is_error);
                }
                if text_of(step, "status") == "ERROR"
                    && let Some(error) = error_text(step)
                {
                    history.notice_level(at, NoticeLevel::Error, error);
                }
            }
            "ERROR_MESSAGE" => {
                let text = Some(text_of(step, "content").to_string()).filter(|text| !text.trim().is_empty()).or_else(|| error_text(step));
                if let Some(text) = text {
                    history.notice_level(at, NoticeLevel::Error, text);
                }
            }
            _ => {}
        }
    }
    history.finish(session)
}
