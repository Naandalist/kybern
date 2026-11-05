//! Grok Build driver: ACP over stdio (`grok agent stdio`).
//!
//! Uses the official `agent-client-protocol` crate, same as the Cursor ACP path.

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ClientCapabilities, ContentBlock, HttpHeader, Implementation, InitializeRequest, LoadSessionRequest, McpServer,
    McpServerHttp, Meta, NewSessionRequest, PromptRequest, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    StopReason as AcpStop, TextContent, ToolKind,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo, on_receive_notification};
use async_trait::async_trait;
use kybern_protocol::process::NoWindow;
use kybern_protocol::*;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::binary::{at_least, resolve, version_of};
use crate::{AgentDriver, AgentSession, DriverError, DriverEvent, NativeToolBridge, ProbeContext, Result, SessionConfig, SpawnedSession};

/// Minimum Grok Build version that ships a stable `agent stdio` ACP mode.
const MIN_VERSION: (u64, u64, u64) = (0, 2, 0);

pub struct GrokDriver;

impl GrokDriver {
    async fn probe_inner(&self, context: &ProbeContext) -> ProviderStatus {
        let kind = self.kind();
        let mut status = ProviderStatus {
            kind,
            display_name: kind.display_name().into(),
            available: false,
            binary_path: None,
            version: None,
            unavailable_reason: None,
            // Grok ACP does not implement session/request_permission yet, so Supervised
            // and AcceptEdits fail with method_not_found. Advertise only the modes that work.
            supported_permission_modes: vec![PermissionMode::Auto, PermissionMode::FullAccess],
            supports_fork: false,
            // The model is chosen at session start (setSessionConfigOption); `set_model` is not wired, so no live switch.
            supports_model_switch: false,
            supports_effort_switch: false,
            supported_efforts: vec![],
            models: vec![],
            instances: vec!["default".into()],
        };

        let bin = match resolve(kind, context.binary.as_ref()) {
            Ok(b) => b,
            Err(e) => {
                status.unavailable_reason = Some(format!("{e}. {}", install_hint()));
                return status;
            }
        };
        status.binary_path = Some(bin.display().to_string());

        match version_of(&bin, &["--version"]).await {
            Some(v) => {
                let ok = at_least(&v, MIN_VERSION);
                status.available = ok;
                status.version = Some(v.clone());
                if !ok {
                    status.unavailable_reason =
                        Some(format!("Grok Build {v} is older than the required {}.{}.{}", MIN_VERSION.0, MIN_VERSION.1, MIN_VERSION.2));
                }
                if ok {
                    status.models = discover_models(&bin, context).await;
                }
            }
            None => {
                status.unavailable_reason = Some("could not run `grok --version`".into());
            }
        }
        status
    }
}

/// Best-effort model list. Falls back to empty if the command is missing or fails.
async fn discover_models(bin: &std::path::Path, context: &ProbeContext) -> Vec<ProviderModel> {
    use tokio::process::Command;

    let mut cmd = Command::new(bin).no_window();
    cmd.args(["models"]).stdin(std::process::Stdio::null()).kill_on_drop(true);
    if let Some(cwd) = &context.cwd {
        cmd.current_dir(cwd);
    }
    for (k, v) in &context.env {
        cmd.env(k, v);
    }

    let output = tokio::time::timeout(std::time::Duration::from_secs(8), crate::process_tree::output(&mut cmd))
        .await
        .ok()
        .and_then(|r| r.ok())
        .filter(|o| o.status.success());

    let Some(output) = output else { return Vec::new() };
    parse_models(&String::from_utf8_lossy(&output.stdout))
}

/// How to get Grok Build when it is not installed. The shell installer is for macOS and Linux only.
fn install_hint() -> &'static str {
    if cfg!(windows) {
        "Install Grok Build from https://x.ai/cli, then restart Kybern."
    } else {
        "Install Grok Build: https://x.ai/cli (or `curl -fsSL https://x.ai/cli/install.sh | bash`)"
    }
}

/// The models in `grok models` output: one per line, the id first and anything after it (a
/// "(default)" marker, a description) optional. Headings, blank lines and separators are skipped.
fn parse_models(text: &str) -> Vec<ProviderModel> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let starred = line.starts_with(['*', '>', '•']);
            let line = line.trim_start_matches(['*', '>', '•', '-', ' ']);
            let (id, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            let id = id.trim_matches(['`', ',']);
            let first = id.chars().next()?;
            // Model ids look like `grok-4` or `grok-code-fast-1`; "Available models:" or "MODEL" do not.
            if !first.is_ascii_alphanumeric() || id.ends_with(':') || !id.contains(['-', '.']) && !id.chars().any(|c| c.is_ascii_digit()) {
                return None;
            }
            let rest = rest.trim();
            let marked_default = starred || rest.to_ascii_lowercase().contains("default");
            let description = rest.trim_matches(|c: char| c.is_whitespace() || matches!(c, '-' | '–' | '—' | ':')).to_string();
            Some(ProviderModel {
                id: id.to_string(),
                display_name: id.to_string(),
                resolved_id: None,
                description: (!description.is_empty()).then_some(description),
                provider: Some("xai".into()),
                efforts: vec![],
                default_effort: None,
                is_default: marked_default || id.contains("grok-build"),
                parameters: vec![],
                variants: vec![],
            })
        })
        .collect()
}

#[async_trait]
impl AgentDriver for GrokDriver {
    fn kind(&self) -> ProviderKind {
        ProviderKind::GrokBuild
    }

    async fn probe(&self, binary: Option<&PathBuf>) -> ProviderStatus {
        self.probe_inner(&ProbeContext { binary: binary.cloned(), ..ProbeContext::default() }).await
    }

    async fn probe_with_context(&self, context: &ProbeContext) -> ProviderStatus {
        self.probe_inner(context).await
    }

    async fn list_sessions(
        &self,
        _context: &ProbeContext,
        _cursor: Option<&str>,
        _query: &str,
    ) -> Result<kybern_protocol::methods::SessionsListResult> {
        Ok(kybern_protocol::methods::SessionsListResult { sessions: vec![], next_cursor: None })
    }

    async fn read_session(&self, _context: &ProbeContext, _id: &str) -> Result<crate::sessions::SessionHistory> {
        Err(DriverError::Unsupported("Grok Build session history is not implemented yet".into()))
    }

    async fn open_session_reader(&self, _context: &ProbeContext) -> Result<Box<dyn crate::sessions::SessionReader>> {
        Err(DriverError::Unsupported("Grok Build session reader is not implemented yet".into()))
    }

    async fn spawn(&self, config: SessionConfig) -> Result<SpawnedSession> {
        let bin = resolve(ProviderKind::GrokBuild, config.binary.as_ref())?;
        let (events_tx, events_rx) = mpsc::channel(1024);
        let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>(64);
        let (bound_tx, bound_rx) = oneshot::channel::<Result<String>>();

        // Grok has no system channel: a new conversation carries Kybern's guide (and a
        // Coordinator's lead instructions) on its first ordinary prompt. A resumed one that
        // already holds it gets nothing; one that never received it gets it once.
        let guide = match (&config.resume_session_id, config.resume_lacks_instructions) {
            (None, _) | (Some(_), true) => config.native_tool_bridge.as_ref().and_then(|bridge| bridge.provider_instructions()),
            (Some(_), false) => None,
        };

        let stderr_tail = Arc::new(std::sync::Mutex::new(String::new()));
        let worker_shared = Arc::new(Shared { events: events_tx.clone() });
        let worker_stderr = stderr_tail.clone();
        let cfg = config.clone();
        let worker = crate::process_tree::SessionTask(tokio::spawn(async move {
            let outcome = run_connection(bin, cfg, worker_shared, cmd_rx, bound_tx, worker_stderr).await;
            let error = outcome.err().map(|e| e.to_string());
            let _ = events_tx.send(DriverEvent::Exited { code: None, error }).await;
        }));

        match bound_rx.await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(DriverError::ProcessExited(startup_exit_message(&stderr_tail))),
        }
        Ok(SpawnedSession { session: Box::new(Handle { commands: cmd_tx, _worker: worker, guide: Mutex::new(guide) }), events: events_rx })
    }
}

struct Shared {
    events: mpsc::Sender<DriverEvent>,
}

enum SessionCommand {
    Prompt { text: String },
    Cancel,
    Close,
}

struct Handle {
    _worker: crate::process_tree::SessionTask,
    commands: mpsc::Sender<SessionCommand>,
    /// Kybern's guide, until the first prompt that can carry it is sent.
    guide: Mutex<Option<String>>,
}

fn always_approve(mode: PermissionMode) -> bool {
    matches!(mode, PermissionMode::Auto | PermissionMode::FullAccess)
}

/// Appended to anything that goes wrong before the session is bound.
const SIGN_IN_HINT: &str = "If you are signed out, run `grok login` or set XAI_API_KEY.";

/// How much of the end of `grok`'s stderr is kept to explain a startup failure.
const STDERR_TAIL_BYTES: usize = 600;

fn remember_stderr(tail: &std::sync::Mutex<String>, line: &str) {
    let Ok(mut tail) = tail.lock() else { return };
    if !tail.is_empty() {
        tail.push('\n');
    }
    tail.push_str(line.trim_end());
    if tail.len() > STDERR_TAIL_BYTES {
        let mut cut = tail.len() - STDERR_TAIL_BYTES;
        while !tail.is_char_boundary(cut) {
            cut += 1;
        }
        tail.drain(..cut);
    }
}

fn startup_exit_message(tail: &std::sync::Mutex<String>) -> String {
    let detail = tail.lock().map(|t| t.trim().to_string()).unwrap_or_default();
    if detail.is_empty() {
        format!("grok agent exited during startup. {SIGN_IN_HINT}")
    } else {
        format!("grok agent exited during startup: {detail}. {SIGN_IN_HINT}")
    }
}

/// Fail the session start: the caller of `spawn` gets `message`, not just "exited during startup".
fn fail_startup(bound: &mut Option<oneshot::Sender<Result<String>>>, message: String) -> agent_client_protocol::Error {
    if let Some(b) = bound.take() {
        let _ = b.send(Err(DriverError::Protocol(message.clone())));
    }
    agent_client_protocol::Error::from(anyhow::anyhow!(message))
}

/// Attach Kybern MCP tools the same way Cursor does. Refuse if enforcement is required.
fn grok_mcp_servers(bridge: Option<&NativeToolBridge>) -> Result<Vec<McpServer>> {
    let Some(bridge) = bridge else { return Ok(Vec::new()) };
    bridge.validate()?;
    if bridge.restrictions.require_enforcement {
        return Err(DriverError::Unsupported(
            "Grok Build ACP can attach Kybern MCP tools but cannot enforce coordinator tool restrictions at the native boundary".into(),
        ));
    }
    let endpoint = bridge
        .endpoint
        .as_deref()
        .ok_or_else(|| DriverError::Unsupported("Grok Build Kybern tools require a daemon MCP endpoint".into()))?;
    let authorization = bridge
        .authorization
        .as_deref()
        .ok_or_else(|| DriverError::Unsupported("Grok Build Kybern tools require a scoped MCP authorization capability".into()))?;
    Ok(vec![McpServer::Http(
        McpServerHttp::new(bridge.server_name.clone(), endpoint.to_string())
            .headers(vec![HttpHeader::new("Authorization", format!("Bearer {authorization}"))]),
    )])
}

/// The tool's name: Grok's own (`_meta["x.ai/tool"].name`, e.g. `read_file`), else the ACP kind
/// (when it says something), else the call's title.
fn tool_name(kind: &ToolKind, title: &str, meta: Option<&Meta>) -> String {
    if let Some(name) = meta
        .and_then(|meta| meta.get("x.ai/tool"))
        .and_then(|tool| tool.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        return name.to_string();
    }
    let kind = serde_json::to_value(kind).ok().and_then(|kind| kind.as_str().map(str::to_string));
    if let Some(kind) = kind.filter(|kind| kind != "other") {
        return kind;
    }
    let title = title.trim();
    if title.is_empty() { "other".into() } else { title.to_string() }
}

/// One tick of `costUsdTicks` is 1e-10 USD.
const USD_PER_TICK: f64 = 1e-10;

/// A turn's tokens and cost from the prompt response's `_meta`. Grok's input already counts the
/// cached reads (as Codex's does), so `input_tokens` is taken as reported.
fn turn_usage(meta: Option<&Meta>) -> (Usage, Option<f64>) {
    let Some(meta) = meta else { return (Usage::default(), None) };
    let usage = meta.get("usage").and_then(Value::as_object);
    let tokens = |keys: &[&str]| usage.and_then(|usage| keys.iter().find_map(|key| usage.get(*key)).and_then(Value::as_u64)).unwrap_or(0);
    let ticks = usage
        .and_then(|usage| usage.get("costUsdTicks"))
        .or_else(|| meta.get("costUsdTicks"))
        .and_then(|ticks| ticks.as_f64().or_else(|| ticks.as_str().and_then(|ticks| ticks.parse().ok())));
    (
        Usage {
            input_tokens: tokens(&["inputTokens", "input_tokens", "promptTokens", "prompt_tokens"]),
            output_tokens: tokens(&["outputTokens", "output_tokens", "completionTokens", "completion_tokens"]),
            cache_read_tokens: tokens(&["cachedReadTokens", "cachedInputTokens", "cache_read_tokens", "cachedTokens", "cached_tokens"]),
            cache_write_tokens: tokens(&["cachedWriteTokens", "cacheWriteTokens", "cache_write_tokens"]),
        },
        ticks.map(|ticks| ticks * USD_PER_TICK),
    )
}

/// The prompt as Grok receives it: the guide once, in front of the first ordinary request.
/// A slash command stays at the start of its prompt; the guide waits for the next ordinary one.
/// Returns the text to send and the guide it carries, if any.
fn prompt_with_guide(guide: &mut Option<String>, text: String) -> (String, Option<String>) {
    if text.trim_start().starts_with('/') {
        return (text, None);
    }
    match guide.take() {
        Some(guide) => (crate::cursor::with_guide(&guide, &text), Some(guide)),
        None => (text, None),
    }
}

async fn run_connection(
    bin: PathBuf,
    config: SessionConfig,
    shared: Arc<Shared>,
    mut commands: mpsc::Receiver<SessionCommand>,
    bound: oneshot::Sender<Result<String>>,
    stderr_tail: Arc<std::sync::Mutex<String>>,
) -> Result<()> {
    let mut bound = Some(bound);
    // Checked before `grok` starts, so a refused bridge costs no process.
    let native_mcp_servers = match grok_mcp_servers(config.native_tool_bridge.as_ref()) {
        Ok(servers) => servers,
        Err(error) => {
            let message = error.to_string();
            if let Some(b) = bound.take() {
                let _ = b.send(Err(error));
            }
            return Err(DriverError::Protocol(message));
        }
    };

    let mut cmd = Command::new(&bin).no_window();
    cmd.arg("--no-auto-update").arg("agent");
    if always_approve(config.permission_mode) {
        cmd.arg("--always-approve");
    }
    cmd.arg("stdio")
        .current_dir(&config.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in &config.env {
        cmd.env(k, v);
    }
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd.spawn()?;
    let _tree = crate::process_tree::ProcessTree::new(child.id().expect("spawned grok"));
    let stdin = child.stdin.take().ok_or_else(|| DriverError::Protocol("no stdin".into()))?;
    let stdout = child.stdout.take().ok_or_else(|| DriverError::Protocol("no stdout".into()))?;
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                tracing::debug!(target: "provider.stderr", "{l}");
                remember_stderr(&stderr_tail, &l);
            }
        });
    }
    let agent = ByteStreams::new(stdin.compat_write(), stdout.compat());
    let _child_guard = child;

    let cwd = config.cwd.clone();
    let events = shared.events.clone();

    let msg_state: Arc<Mutex<(u64, bool)>> = Arc::new(Mutex::new((0, false)));
    let msg_state_notif = msg_state.clone();
    // `session/load` replays the whole conversation as updates; those are history, not new output.
    let loading = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let loading_notif = loading.clone();

    let result = Client
        .builder()
        .name("kybern")
        .on_receive_notification(
            async move |n: SessionNotification, _cx| {
                if loading_notif.load(std::sync::atomic::Ordering::Relaxed) {
                    return Ok(());
                }
                let ev = &events;
                match n.update {
                    SessionUpdate::AgentMessageChunk(c) => {
                        if let ContentBlock::Text(t) = c.content {
                            let id = {
                                let mut s = msg_state_notif.lock().await;
                                if !s.1 {
                                    s.0 += 1;
                                    s.1 = true;
                                }
                                format!("m{}", s.0)
                            };
                            let _ = ev.send(DriverEvent::TextDelta { message_id: id, origin: EventOrigin::Root, delta: t.text }).await;
                        }
                    }
                    SessionUpdate::AgentThoughtChunk(c) => {
                        if let ContentBlock::Text(t) = c.content {
                            let id = {
                                let mut s = msg_state_notif.lock().await;
                                if !s.1 {
                                    s.0 += 1;
                                    s.1 = true;
                                }
                                format!("m{}", s.0)
                            };
                            let _ = ev.send(DriverEvent::ThinkingDelta { message_id: id, origin: EventOrigin::Root, delta: t.text }).await;
                        }
                    }
                    SessionUpdate::ToolCall(tc) => {
                        msg_state_notif.lock().await.1 = false;
                        let _ = ev
                            .send(DriverEvent::ToolStarted(ToolCall {
                                id: tc.tool_call_id.to_string(),
                                name: tool_name(&tc.kind, &tc.title, tc.meta.as_ref()),
                                input: json!({ "title": tc.title, "raw": tc.raw_input }),
                                parent_id: None,
                            }))
                            .await;
                    }
                    SessionUpdate::ToolCallUpdate(u) => {
                        let status = u.fields.status.map(|s| format!("{s:?}").to_lowercase());
                        if matches!(status.as_deref(), Some("completed") | Some("failed")) {
                            let output = json!({
                                "status": status,
                                "raw": u.fields.raw_output,
                                "content": u.fields.content,
                                "title": u.fields.title,
                            });
                            let _ = ev
                                .send(DriverEvent::ToolCompleted {
                                    tool_call_id: u.tool_call_id.to_string(),
                                    output,
                                    is_error: status.as_deref() == Some("failed"),
                                })
                                .await;
                        }
                    }
                    _ => {}
                }
                Ok(())
            },
            on_receive_notification!(),
        )
        .connect_with(agent, async |cx: ConnectionTo<Agent>| {
            let init = cx
                .send_request(
                    InitializeRequest::new(ProtocolVersion::V1)
                        .client_capabilities(ClientCapabilities::new())
                        .client_info(Implementation::new("kybern", env!("CARGO_PKG_VERSION"))),
                )
                .block_task()
                .await;
            if let Err(e) = init {
                return Err(fail_startup(&mut bound, format!("Grok Build failed to initialize: {e}. {SIGN_IN_HINT}")));
            }

            // Assume already authenticated (grok login or XAI_API_KEY). Explicit auth can be added later.
            let session_id = match &config.resume_session_id {
                Some(id) => {
                    loading.store(true, std::sync::atomic::Ordering::Relaxed);
                    let request = LoadSessionRequest::new(id.clone(), cwd.clone()).mcp_servers(native_mcp_servers.clone());
                    let loaded = cx.send_request(request).block_task().await;
                    loading.store(false, std::sync::atomic::Ordering::Relaxed);
                    match loaded {
                        Ok(_) => agent_client_protocol::schema::v1::SessionId::from(id.clone()),
                        // A fresh session here would silently drop the conversation, so the
                        // thread is told it could not be resumed (as Cursor's is).
                        Err(e) => {
                            return Err(fail_startup(&mut bound, format!("Grok Build could not resume session {id}: {e}. {SIGN_IN_HINT}")));
                        }
                    }
                }
                None => {
                    let request = NewSessionRequest::new(cwd.clone()).mcp_servers(native_mcp_servers);
                    match cx.send_request(request).block_task().await {
                        Ok(created) => created.session_id,
                        Err(e) => {
                            return Err(fail_startup(&mut bound, format!("Grok Build could not start a session: {e}. {SIGN_IN_HINT}")));
                        }
                    }
                }
            };

            // A model that cannot be set must not pass as the one in use: say so, and report none.
            let mut model = config.model.clone();
            let mut model_notice = None;
            if let Some(wanted) = &model {
                let set =
                    cx.send_request(SetSessionConfigOptionRequest::new(session_id.clone(), "model", wanted.as_str())).block_task().await;
                if let Err(e) = set {
                    model_notice = Some(format!("Grok Build could not switch to {wanted}: {e}. It is using its default model."));
                    model = None;
                }
            }

            if let Some(b) = bound.take() {
                let _ = b.send(Ok(session_id.to_string()));
            }
            let _ = shared.events.send(DriverEvent::SessionBound { session_id: session_id.to_string(), model }).await;
            if let Some(text) = model_notice {
                let _ = shared.events.send(DriverEvent::Notice { level: NoticeLevel::Warning, text, data: None }).await;
            }

            while let Some(cmd) = commands.recv().await {
                match cmd {
                    SessionCommand::Prompt { text } => {
                        let started = std::time::Instant::now();
                        msg_state.lock().await.1 = false;
                        let blocks = vec![ContentBlock::Text(TextContent::new(text))];
                        let prompt = cx.send_request(PromptRequest::new(session_id.clone(), blocks));
                        let mut fut = std::pin::pin!(prompt.block_task());
                        let resp = loop {
                            tokio::select! {
                                r = &mut fut => break r,
                                c = commands.recv() => match c {
                                    Some(SessionCommand::Cancel) | Some(SessionCommand::Close) | None => {
                                        let _ = cx.send_notification(CancelNotification::new(session_id.clone()));
                                    }
                                    Some(SessionCommand::Prompt { .. }) => {}
                                },
                            }
                        };
                        let duration_ms = started.elapsed().as_millis() as u64;
                        let ev = match resp {
                            Ok(r) => {
                                let stop = match r.stop_reason {
                                    AcpStop::EndTurn => StopReason::Completed,
                                    AcpStop::MaxTokens => StopReason::MaxTurns,
                                    AcpStop::MaxTurnRequests => StopReason::MaxTurns,
                                    AcpStop::Cancelled => StopReason::Interrupted,
                                    _ => StopReason::Completed,
                                };
                                let (usage, cost_usd) = turn_usage(r.meta.as_ref());
                                DriverEvent::TurnCompleted { stop_reason: stop, usage, cost_usd, duration_ms, anchors: Default::default() }
                            }
                            Err(e) => DriverEvent::TurnFailed { error: e.to_string() },
                        };
                        let _ = shared.events.send(ev).await;
                    }
                    SessionCommand::Cancel => {
                        let _ = cx.send_notification(CancelNotification::new(session_id.clone()));
                    }
                    SessionCommand::Close => break,
                }
            }
            Ok(())
        })
        .await;

    result.map_err(|e| DriverError::Protocol(e.to_string()))
}

#[async_trait]
impl AgentSession for Handle {
    async fn send_message(&self, _message_id: &str, message: &UserMessage) -> Result<()> {
        let mut text = String::new();
        for part in &message.parts {
            match part {
                ContentPart::Text { text: t } => text.push_str(t),
                ContentPart::FileMention { path } => text.push_str(&format!("@{path}")),
                _ => {
                    if let Some(flat) = part.orchestration_text() {
                        text.push_str(&flat);
                    }
                }
            }
        }
        let (text, guide) = prompt_with_guide(&mut *self.guide.lock().await, text);
        if self.commands.send(SessionCommand::Prompt { text }).await.is_err() {
            // The agent never saw it; offer the guide again with the retry.
            if guide.is_some() {
                *self.guide.lock().await = guide;
            }
            return Err(DriverError::ProcessExited("session closed".into()));
        }
        Ok(())
    }

    async fn interrupt(&self) -> Result<()> {
        let _ = self.commands.send(SessionCommand::Cancel).await;
        Ok(())
    }

    async fn set_permission_mode(&self, _mode: PermissionMode) -> Result<()> {
        // Mode is applied at spawn via --always-approve. Runtime switch not supported yet.
        Err(DriverError::Unsupported("Grok Build permission mode cannot be changed after session start".into()))
    }

    async fn set_model(&self, _model: &str) -> Result<()> {
        Err(DriverError::Unsupported("Grok Build model switch is not implemented yet".into()))
    }

    async fn set_effort(&self, _effort: &str) -> Result<()> {
        Err(DriverError::Unsupported("Grok Build does not expose effort switching over ACP".into()))
    }

    async fn respond_permission(&self, _request_id: &str, _decision: &ApprovalDecision) -> Result<()> {
        // With --always-approve this is a no-op. Interactive permissions come later.
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        let _ = self.commands.send(SessionCommand::Close).await;
        Ok(())
    }
}

#[cfg(test)]
mod grok_meta_tests {
    use super::*;

    fn meta(value: Value) -> Meta {
        value.as_object().cloned().expect("an object")
    }

    #[test]
    fn tool_name_prefers_xai_meta_then_kind_then_title() {
        let named = meta(json!({ "x.ai/tool": { "name": "read_file" } }));
        assert_eq!(tool_name(&ToolKind::Other, "Read Cargo.toml", Some(&named)), "read_file");
        assert_eq!(tool_name(&ToolKind::Read, "Read Cargo.toml", Some(&named)), "read_file");
        // No usable name in the metadata: the ACP kind says what it is.
        let blank = meta(json!({ "x.ai/tool": { "name": " " } }));
        assert_eq!(tool_name(&ToolKind::Execute, "ls", Some(&blank)), "execute");
        assert_eq!(tool_name(&ToolKind::SwitchMode, "Plan", None), "switch_mode");
        // An `other` kind says nothing, so the title does.
        assert_eq!(tool_name(&ToolKind::Other, "  List crates ", None), "List crates");
        assert_eq!(tool_name(&ToolKind::Other, "", None), "other");
    }

    #[test]
    fn turn_usage_reads_meta_usage_and_cost() {
        let reported = meta(json!({
            "usage": { "inputTokens": 36893, "outputTokens": 267, "cachedReadTokens": 30000, "costUsdTicks": 155000000 }
        }));
        let (usage, cost) = turn_usage(Some(&reported));
        assert_eq!(
            usage,
            Usage { input_tokens: 36893, output_tokens: 267, cache_read_tokens: 30000, cache_write_tokens: 0 },
            "input is kept as reported: it already includes the cached reads"
        );
        assert!((cost.unwrap() - 0.0155).abs() < 1e-12, "{cost:?}");

        // The cost may sit beside the usage block.
        let beside = meta(json!({ "usage": { "inputTokens": 5, "outputTokens": 2 }, "costUsdTicks": 10_000_000_000u64 }));
        let (usage, cost) = turn_usage(Some(&beside));
        assert_eq!((usage.input_tokens, usage.output_tokens), (5, 2));
        assert!((cost.unwrap() - 1.0).abs() < 1e-9);

        // Nothing reported: zero tokens and no cost, not a made-up one.
        assert_eq!(turn_usage(None), (Usage::default(), None));
        assert_eq!(turn_usage(Some(&meta(json!({})))), (Usage::default(), None));
    }

    #[test]
    fn models_parse_with_markers_and_descriptions_and_skip_headings() {
        let models = parse_models(
            "Available models:\n\nMODEL  CONTEXT\n-----\ngrok-4  (default)  Flagship reasoning model\n* grok-code-fast-1\n  grok-build - Agentic coding\n# comment\n",
        );
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["grok-4", "grok-code-fast-1", "grok-build"]);
        assert_eq!(models[0].description.as_deref(), Some("(default)  Flagship reasoning model"));
        assert!(models[0].is_default && models[1].is_default && models[2].is_default);
        assert_eq!(models[2].description.as_deref(), Some("Agentic coding"));
        assert!(parse_models("").is_empty());
    }

    #[test]
    fn the_guide_rides_on_the_first_ordinary_prompt_only() {
        let mut guide = Some("# Working in Kybern".to_string());
        // A slash command keeps the start of its prompt and leaves the guide for later.
        assert_eq!(prompt_with_guide(&mut guide, "/compact".into()), ("/compact".into(), None));
        assert!(guide.is_some());

        let (first, carried) = prompt_with_guide(&mut guide, "Hello".into());
        assert_eq!(carried.as_deref(), Some("# Working in Kybern"));
        assert!(first.starts_with("<kybern_instructions>\n# Working in Kybern\n</kybern_instructions>\n\n"), "{first}");
        assert!(first.ends_with("Hello"));
        assert_eq!(crate::cursor::without_guide(&first), "Hello");

        // Sent once: the next prompt is the user's own text.
        assert_eq!(prompt_with_guide(&mut guide, "Again".into()), ("Again".into(), None));
    }

    #[test]
    fn a_failed_start_reaches_the_caller_and_stderr_explains_an_early_exit() {
        let (tx, mut rx) = oneshot::channel::<Result<String>>();
        let mut bound = Some(tx);
        let _ = fail_startup(&mut bound, "Grok Build could not start a session: auth required. run grok login".into());
        assert!(bound.is_none());
        let Ok(Err(DriverError::Protocol(message))) = rx.try_recv() else { panic!("expected the startup error") };
        assert!(message.contains("auth required"), "{message}");

        let tail = std::sync::Mutex::new(String::new());
        assert!(startup_exit_message(&tail).contains("grok login"));
        for line in 0..100 {
            remember_stderr(&tail, &format!("error line {line}"));
        }
        let message = startup_exit_message(&tail);
        assert!(message.contains("error line 99") && !message.contains("error line 0\n"), "{message}");
        assert!(tail.lock().unwrap().len() <= STDERR_TAIL_BYTES);
    }
}
