//! Native saved-session discovery and transcript readers. These never send a
//! prompt or replay a tool. Each reader follows its harness own storage/API.
mod antigravity;
#[cfg(test)]
mod antigravity_tests;
mod claude;
mod codex;
mod cursor;
mod cursor_acp;
mod opencode;
mod pi;
#[cfg(test)]
mod tests;

use kybern_protocol::process::NoWindow;
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use kybern_protocol::methods::{SavedSession, SessionsListResult};
use kybern_protocol::*;
use serde_json::{Value, json};
use tokio::process::Command;
use uuid::Uuid;

use crate::ndjson::{NdjsonChild, SessionLifetime};
use crate::{DriverError, ProbeContext, Result};

const PAGE_SIZE: usize = 50;
const MAX_HISTORY_BYTES: u64 = 64 * 1024 * 1024;

pub struct SessionHistory {
    pub session: SavedSession,
    pub events: Vec<ThreadEvent>,
}

pub async fn list(kind: ProviderKind, context: &ProbeContext, cursor: Option<&str>, query: &str) -> Result<SessionsListResult> {
    match kind {
        ProviderKind::ClaudeCode => claude::list(context, cursor, query).await,
        ProviderKind::Codex => codex::list(context, cursor, query).await,
        ProviderKind::Opencode => opencode::list(context, cursor, query).await,
        ProviderKind::Pi | ProviderKind::Omp => pi::list(kind, context, cursor, query).await,
        ProviderKind::Cursor => cursor::list(context, cursor, query).await,
        ProviderKind::Antigravity => antigravity::list(context, cursor, query).await,
    }
}

pub async fn read(kind: ProviderKind, context: &ProbeContext, id: &str) -> Result<SessionHistory> {
    if id.is_empty() || id.len() > 4096 {
        return Err(DriverError::Protocol("Choose a saved session to resume.".into()));
    }
    match kind {
        ProviderKind::ClaudeCode => claude::read(context, id).await,
        ProviderKind::Codex => codex::read(context, id).await,
        ProviderKind::Opencode => opencode::read(context, id).await,
        ProviderKind::Pi | ProviderKind::Omp => pi::read(kind, context, id).await,
        ProviderKind::Cursor => cursor::read(context, id).await,
        ProviderKind::Antigravity => antigravity::read(context, id).await,
    }
}

/// One harness's saved sessions, read through a single helper process (Codex
/// app-server, `opencode serve`, the Cursor sidecar or `cursor-agent acp`) kept
/// for many calls, instead of one process per `list` or `read`. File-based
/// harnesses start no process; they remember the file list and where each id lives.
/// Dropping the reader (or `close`) ends the helper.
#[async_trait::async_trait]
pub trait SessionReader: Send {
    /// One native page of sessions in every folder (`context.cwd` still filters),
    /// newest first. `None` starts at the beginning.
    async fn list_page(&mut self, cursor: Option<&str>) -> Result<SessionsListResult>;
    async fn read(&mut self, id: &str) -> Result<SessionHistory>;
    async fn close(self: Box<Self>);
}

pub async fn reader(kind: ProviderKind, context: &ProbeContext) -> Result<Box<dyn SessionReader>> {
    Ok(match kind {
        ProviderKind::ClaudeCode => Box::new(FileReader::new(kind, claude::root(context)?, context)),
        ProviderKind::Pi | ProviderKind::Omp => Box::new(FileReader::new(kind, pi::root(kind, context)?, context)),
        ProviderKind::Codex => Box::new(codex::Reader { rpc: codex::connect(context).await?, context: context.clone() }),
        ProviderKind::Opencode => Box::new(opencode::Reader::start(context).await?),
        ProviderKind::Cursor => Box::new(cursor::Reader::new(context)),
        ProviderKind::Antigravity => Box::new(antigravity::Reader::new(context)?),
    })
}

/// Claude, pi and OMP: sessions are JSONL files under one root.
struct FileReader {
    kind: ProviderKind,
    root: PathBuf,
    context: ProbeContext,
    files: Option<Arc<Vec<PathBuf>>>,
    paths: HashMap<String, PathBuf>,
}

impl FileReader {
    fn new(kind: ProviderKind, root: PathBuf, context: &ProbeContext) -> Self {
        Self { kind, root, context: context.clone(), files: None, paths: HashMap::new() }
    }
}

#[async_trait::async_trait]
impl SessionReader for FileReader {
    async fn list_page(&mut self, cursor: Option<&str>) -> Result<SessionsListResult> {
        let start = offset(cursor)?;
        let files = match &self.files {
            Some(files) => files.clone(),
            None => {
                let root = self.root.clone();
                let files =
                    Arc::new(tokio::task::spawn_blocking(move || jsonl_files(&root)).await.map_err(|e| DriverError::Other(e.into()))??);
                self.files = Some(files.clone());
                files
            }
        };
        let (kind, context) = (self.kind, self.context.clone());
        let (result, found) = tokio::task::spawn_blocking(move || {
            file_page(&files, start, &context, "", |path, records| match kind {
                ProviderKind::ClaudeCode => claude::summary(path, records),
                _ => pi::summary(kind, path, records),
            })
        })
        .await
        .map_err(|e| DriverError::Other(e.into()))?;
        self.paths.extend(found);
        Ok(result)
    }

    async fn read(&mut self, id: &str) -> Result<SessionHistory> {
        let Some(path) = self.paths.get(id).cloned() else {
            // Not listed through this reader: resolve by id the slow way.
            return read(self.kind, &self.context, id).await;
        };
        let (kind, id) = (self.kind, id.to_string());
        tokio::task::spawn_blocking(move || match kind {
            ProviderKind::ClaudeCode => claude::read_path(&path, &id),
            _ => pi::read_path(kind, &path, &id),
        })
        .await
        .map_err(|e| DriverError::Other(e.into()))?
    }

    async fn close(self: Box<Self>) {}
}

/// One page of a JSONL harness: summaries for files from `start`, stopping at
/// `PAGE_SIZE` matches. Also returns where each listed session lives.
fn file_page(
    files: &[PathBuf],
    start: usize,
    context: &ProbeContext,
    query: &str,
    summarize: impl Fn(&Path, &[Value]) -> Option<SavedSession>,
) -> (SessionsListResult, Vec<(String, PathBuf)>) {
    let mut sessions = Vec::new();
    let mut found = Vec::new();
    let mut next_cursor = None;
    for (index, path) in files.iter().enumerate().skip(start) {
        if let Ok(records) = metadata_lines(path)
            && let Some(session) = summarize(path, &records)
            && matches_cwd(&session, context)
            && matches_query(&session, query)
        {
            found.push((session.id.clone(), path.clone()));
            sessions.push(session);
        }
        if sessions.len() == PAGE_SIZE {
            next_cursor = (index + 1 < files.len()).then(|| (index + 1).to_string());
            break;
        }
    }
    (SessionsListResult { sessions, next_cursor }, found)
}

/// Why `read` refused a session, from the wording the readers use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadFailure {
    /// Its harness is still writing it.
    InUse,
    /// Over the 64 MB history limit.
    TooLarge,
    Other,
}

const STILL_WRITING: &str = "Session is still being written.";
const TOO_LARGE: &str = "64 MB";

pub fn classify_read_error(error: &DriverError) -> ReadFailure {
    let text = error.to_string();
    if text.contains(STILL_WRITING) || text.contains("Session changed while reading.") {
        ReadFailure::InUse
    } else if text.contains(TOO_LARGE) {
        ReadFailure::TooLarge
    } else {
        ReadFailure::Other
    }
}

/// Read retained explicit OMP blocks without starting the harness or importing history.
pub async fn recover_omp_message(context: &ProbeContext, session_id: &str, text: &str) -> Result<NativeOmpMessage> {
    if session_id.is_empty() || session_id.len() > 4096 || text.len() > 1024 * 1024 {
        return Err(DriverError::Protocol("Choose a bounded retained OMP message to recover.".into()));
    }
    pi::recover_message(context, session_id, text).await
}

pub struct NativeOmpMessage {
    pub entry_id: String,
    pub at: DateTime<Utc>,
    pub blocks: Vec<NativeOmpBlock>,
}

pub enum NativeOmpBlock {
    Text(String),
    Thinking(String),
    ToolCall(String),
}

fn env(context: &ProbeContext, name: &str) -> Option<String> {
    context.env.get(name).cloned().or_else(|| std::env::var(name).ok()).filter(|s| !s.is_empty())
}

fn home(context: &ProbeContext) -> Result<PathBuf> {
    env(context, "HOME")
        .or_else(|| env(context, "USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| DriverError::Protocol("Home folder is unavailable. Set the harness session directory in provider settings.".into()))
}

fn command(kind: ProviderKind, context: &ProbeContext) -> Result<Command> {
    let bin = crate::binary::resolve(kind, context.binary.as_ref())?;
    let mut cmd = crate::binary::command(bin).no_window();
    cmd.envs(&context.env).kill_on_drop(true);
    if let Some(cwd) = &context.cwd {
        cmd.current_dir(cwd);
    }
    Ok(cmd)
}

fn timestamp(value: &Value) -> Option<DateTime<Utc>> {
    value.as_str().and_then(|s| s.parse().ok()).or_else(|| {
        value.as_i64().and_then(|n| if n > 100_000_000_000 { DateTime::from_timestamp_millis(n) } else { DateTime::from_timestamp(n, 0) })
    })
}

fn title(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(160).collect()
}

fn offset(cursor: Option<&str>) -> Result<usize> {
    cursor.map(|s| s.parse().map_err(|_| DriverError::Protocol("Session list changed. Refresh and try again.".into()))).unwrap_or(Ok(0))
}

fn matches_query(session: &SavedSession, query: &str) -> bool {
    let haystack = format!("{} {} {} {}", session.title, session.cwd, session.id, session.provider.display_name()).to_lowercase();
    query.split_whitespace().all(|word| haystack.contains(&word.to_lowercase()))
}

fn matches_cwd(session: &SavedSession, context: &ProbeContext) -> bool {
    context.cwd.as_ref().is_none_or(|cwd| {
        let path = Path::new(&session.cwd);
        path == cwd || path.starts_with(cwd)
    })
}

/// Metadata-only scan: read bounded head/tail windows, never whole transcripts
/// just to populate a picker. Skip symlinks and harness subagent directories.
fn jsonl_files(root: &Path) -> Result<Vec<PathBuf>> {
    jsonl_files_bounded(root, usize::MAX)
}

fn jsonl_files_bounded(root: &Path, limit: usize) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut dirs = vec![(root.to_path_buf(), 0)];
    let mut visited = 0usize;
    while let Some((dir, depth)) = dirs.pop() {
        visited = visited.saturating_add(1);
        if visited > limit || dirs.len() > limit {
            return Err(DriverError::Protocol(
                "OMP session discovery exceeds its directory limit. Choose a narrower native history directory.".into(),
            ));
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_dir() && depth < 4 && entry.file_name() != "subagents" {
                dirs.push((entry.path(), depth + 1));
            } else if ty.is_file() && entry.path().extension().is_some_and(|ext| ext == "jsonl") {
                if files.len() >= limit {
                    return Err(DriverError::Protocol(
                        "OMP session discovery exceeds its file limit. Choose a narrower native history directory.".into(),
                    ));
                }
                files.push(entry.path());
            }
        }
    }
    files.sort_by_cached_key(|p| std::cmp::Reverse(p.metadata().and_then(|m| m.modified()).ok()));
    Ok(files)
}

fn metadata_lines(path: &Path) -> Result<Vec<Value>> {
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let mut data = Vec::new();
    (&mut file).take(128 * 1024).read_to_end(&mut data)?;
    let mut lines = data.split(|c| *c == b'\n').filter_map(|line| serde_json::from_slice(line).ok()).collect::<Vec<_>>();
    if size > 128 * 1024 {
        file.seek(SeekFrom::Start(size.saturating_sub(64 * 1024)))?;
        data.clear();
        file.read_to_end(&mut data)?;
        lines.extend(data.split(|c| *c == b'\n').skip(1).filter_map(|line| serde_json::from_slice::<Value>(line).ok()));
    }
    Ok(lines)
}

fn read_lines(path: &Path) -> Result<Vec<Value>> {
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > MAX_HISTORY_BYTES {
        return Err(DriverError::Unsupported(
            "This transcript is too large to import (64 MB limit). Fork a shorter conversation in the harness and resume that session."
                .into(),
        ));
    }
    let mut records = Vec::new();
    let mut reader = BufReader::new(file).take(MAX_HISTORY_BYTES + 1);
    let mut text = String::new();
    reader.read_to_string(&mut text)?;
    if text.len() as u64 > MAX_HISTORY_BYTES {
        return Err(DriverError::Protocol("Session changed while reading. Stop the original session and try again.".into()));
    }
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        match serde_json::from_str(line) {
            Ok(record) => records.push(record),
            Err(_) if !text.ends_with('\n') && text.ends_with(line) => {
                return Err(DriverError::Protocol(format!("{STILL_WRITING} Stop the original session and try again.")));
            }
            Err(e) => {
                return Err(DriverError::Protocol(format!(
                    "Unable to read saved history: {e}. Open the session in its harness to repair it."
                )));
            }
        }
    }
    Ok(records)
}

/// Follow the active leaf of a branching JSONL session, not abandoned siblings.
fn active_branch<'a>(records: &'a [Value], id_key: &str, parent_key: &str) -> Vec<&'a Value> {
    let index = records.iter().filter_map(|v| v.get(id_key).and_then(Value::as_str).map(|id| (id, v))).collect::<HashMap<_, _>>();
    let Some(last) = records.iter().rev().find(|v| v.get(id_key).and_then(Value::as_str).is_some() && v.get(parent_key).is_some()) else {
        return records.iter().collect();
    };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut entry = Some(last);
    while let Some(v) = entry {
        let Some(id) = v.get(id_key).and_then(Value::as_str) else { break };
        if !seen.insert(id) {
            break;
        }
        out.push(v);
        entry = v.get(parent_key).and_then(Value::as_str).and_then(|parent| index.get(parent).copied());
    }
    out.reverse();
    out
}

/// Session APIs share framing, not provider semantics. Requests from the
/// harness are rejected here: importing history must never execute a tool.
struct ReadRpc {
    child: Arc<NdjsonChild>,
    _lifetime: SessionLifetime,
    next_id: i64,
}

impl ReadRpc {
    fn spawn(cmd: Command) -> Result<Self> {
        let child = Arc::new(NdjsonChild::spawn(cmd)?);
        let lifetime = SessionLifetime::new(child.clone());
        Ok(Self { child, _lifetime: lifetime, next_id: 1 })
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<(Value, Vec<Value>)> {
        let id = self.next_id;
        self.next_id += 1;
        self.child.write(&json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})).await?;
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut notifications = Vec::new();
            let mut bytes = 0;
            loop {
                let value = self.child.lines.lock().await.recv().await
                    .ok_or_else(|| DriverError::ProcessExited("Session reader closed. Check that the harness is signed in and try again.".into()))?;
                if value.get("id").and_then(Value::as_i64) == Some(id) && value.get("method").is_none() {
                    if let Some(error) = value.get("error") {
                        return Err(DriverError::Protocol(error.get("message").and_then(Value::as_str).unwrap_or("Unable to read saved session. Update the harness and try again.").to_string()));
                    }
                    return Ok((value.get("result").cloned().unwrap_or(Value::Null), notifications));
                }
                if value.get("method").is_some() && value.get("id").is_some() {
                    self.child.write(&json!({"jsonrpc":"2.0", "id":value["id"], "error":{"code":-32601,"message":"Tools are unavailable while importing history"}})).await?;
                } else if value["method"] == "session/update" {
                    bytes += value.to_string().len() as u64;
                    if bytes > MAX_HISTORY_BYTES { return Err(DriverError::Unsupported("Session history exceeds the 64 MB import limit. Fork a shorter conversation and try again.".into())); }
                    notifications.push(value);
                }
            }
        }).await.map_err(|_| DriverError::Protocol("Reading saved sessions timed out. Check the harness and try again.".into()))?
    }
}

/// Imported turns have no checkpoints or usage billing rows. Unresolved tools
/// become interrupted so the transcript does not claim they are still running.
#[derive(Default)]
struct History {
    events: Vec<ThreadEvent>,
    turn: Option<TurnId>,
    terminal: Option<MessageId>,
    tools: HashSet<String>,
    last_at: Option<DateTime<Utc>>,
}

impl History {
    fn push(&mut self, at: DateTime<Utc>, payload: EventPayload) {
        self.last_at = Some(at);
        self.events.push(ThreadEvent { seq: 0, thread_id: Uuid::nil(), turn_id: self.turn, at, payload });
    }

    fn user(&mut self, at: DateTime<Utc>, message: UserMessage) {
        if message.parts.is_empty() {
            return;
        }
        self.end(StopReason::Completed);
        self.turn = Some(Uuid::new_v4());
        self.push(at, EventPayload::TurnStarted { message_id: Uuid::new_v4(), message });
    }

    fn assistant(&mut self, at: DateTime<Utc>, text: String, thinking: Option<String>) {
        if self.turn.is_none() {
            self.turn = Some(Uuid::new_v4());
        }
        if text.is_empty() && thinking.as_ref().is_none_or(String::is_empty) {
            return;
        }
        let message_id = Uuid::new_v4();
        if !text.trim().is_empty() {
            self.terminal = Some(message_id);
        }
        self.push(at, EventPayload::AssistantMessageCompleted { message_id, origin: EventOrigin::Root, text, thinking });
    }

    fn tool(&mut self, at: DateTime<Utc>, id: &str, name: &str, input: Value) {
        if self.turn.is_none() {
            self.turn = Some(Uuid::new_v4());
        }
        if !self.tools.insert(id.to_string()) {
            return;
        }
        self.terminal = None;
        self.push(
            at,
            EventPayload::ToolCallStarted {
                call: ToolCall { id: id.into(), name: name.into(), input, parent_id: None },
                origin: EventOrigin::Root,
            },
        );
    }

    fn result(&mut self, at: DateTime<Utc>, id: &str, output: Value, is_error: bool) {
        if !self.tools.remove(id) {
            return;
        }
        self.push(
            at,
            EventPayload::ToolCallCompleted { tool_call_id: id.into(), output, output_omitted: false, stream_recoverable: false, is_error },
        );
    }

    fn notice(&mut self, at: DateTime<Utc>, text: String) {
        self.notice_level(at, NoticeLevel::Info, text);
    }

    fn notice_level(&mut self, at: DateTime<Utc>, level: NoticeLevel, text: String) {
        self.push(at, EventPayload::ProviderNotice { level, text, data: None });
    }

    fn end(&mut self, stop: StopReason) {
        let Some(_) = self.turn else { return };
        let at = self.last_at.unwrap_or_else(Utc::now);
        let unfinished = !self.tools.is_empty();
        for id in self.tools.clone() {
            self.result(at, &id, json!("Execution ended outside Kybern. This tool was not restarted."), true);
        }
        let terminal_message_id = self.terminal.take();
        self.push(
            at,
            EventPayload::TurnCompleted {
                stop_reason: if unfinished { StopReason::Interrupted } else { stop },
                usage: Usage::default(),
                cost_usd: None,
                duration_ms: 0,
                terminal_message_id,
            },
        );
        self.turn = None;
    }

    fn finish(mut self, session: SavedSession) -> SessionHistory {
        self.end(StopReason::Completed);
        SessionHistory { session, events: self.events }
    }
}

fn content_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    content.as_array().into_iter().flatten().filter_map(|part| part.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n")
}

fn user_content(content: &Value) -> UserMessage {
    if let Some(text) = content.as_str() {
        return UserMessage::text(text);
    }
    let mut parts = Vec::new();
    for part in content.as_array().into_iter().flatten() {
        match part["type"].as_str().unwrap_or("") {
            "text" | "input_text" => {
                if let Some(text) = part["text"].as_str() {
                    parts.push(ContentPart::Text { text: text.into() });
                }
            }
            "image" => {
                let data = part.pointer("/source/data").or_else(|| part.get("data")).and_then(Value::as_str);
                let mime = part.pointer("/source/media_type").or_else(|| part.get("mimeType")).and_then(Value::as_str);
                if let (Some(data), Some(mime)) = (data, mime) {
                    parts.push(ContentPart::Image { media_type: mime.into(), data: data.into() });
                } else {
                    parts.push(ContentPart::Text { text: "[Image from the original session]".into() });
                }
            }
            "localImage" => {
                if let Some(path) = part["path"].as_str() {
                    parts.push(ContentPart::FileMention { path: path.into() });
                }
            }
            "skill" => {
                if let (Some(name), Some(path)) = (part["name"].as_str(), part["path"].as_str()) {
                    parts.push(ContentPart::Skill { name: name.into(), path: path.into() });
                }
            }
            "file" => {
                let url = part["url"].as_str().unwrap_or("");
                let mime = part["mime"].as_str().unwrap_or("");
                if mime.starts_with("image/")
                    && url.starts_with("data:")
                    && let Some((_, data)) = url.split_once(";base64,")
                {
                    parts.push(ContentPart::Image { media_type: mime.into(), data: data.into() });
                } else if let Some(path) = url.strip_prefix("file://") {
                    parts.push(ContentPart::FileMention { path: path.into() });
                } else {
                    parts.push(ContentPart::Text {
                        text: format!("[Attachment: {}]", part["filename"].as_str().unwrap_or("original session file")),
                    });
                }
            }
            "tool_result" => {}
            _ => {
                if let Some(text) = part["text"].as_str() {
                    parts.push(ContentPart::Text { text: text.into() });
                }
            }
        }
    }
    UserMessage { parts }
}
