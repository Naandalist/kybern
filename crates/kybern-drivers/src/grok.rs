//! Grok Build driver: ACP over stdio (`grok agent stdio`).
//!
//! Uses the official `agent-client-protocol` crate, same as the Cursor ACP path.

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ClientCapabilities, ContentBlock, HttpHeader, Implementation, InitializeRequest, LoadSessionRequest, McpServer,
    McpServerHttp, NewSessionRequest, PromptRequest, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest, StopReason as AcpStop,
    TextContent,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo, on_receive_notification};
use async_trait::async_trait;
use kybern_protocol::process::NoWindow;
use kybern_protocol::*;
use serde_json::json;
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
            supports_model_switch: true, // sent at session start via setSessionConfigOption; runtime switch not yet wired
            supports_effort_switch: false,
            supported_efforts: vec![],
            models: vec![],
            instances: vec!["default".into()],
        };

        let bin = match resolve(kind, context.binary.as_ref()) {
            Ok(b) => b,
            Err(e) => {
                status.unavailable_reason = Some(format!(
                    "{e}. Install Grok Build: https://x.ai/cli (or `curl -fsSL https://x.ai/cli/install.sh | bash` on macOS/Linux)"
                ));
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
                    status.unavailable_reason = Some(format!(
                        "Grok Build {v} is older than the required {}.{}.{}",
                        MIN_VERSION.0, MIN_VERSION.1, MIN_VERSION.2
                    ));
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
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .filter_map(|line| {
            let id = line.trim();
            if id.is_empty() || id.starts_with('#') || id.contains(' ') {
                return None;
            }
            Some(ProviderModel {
                id: id.to_string(),
                display_name: id.to_string(),
                resolved_id: None,
                description: None,
                provider: Some("xai".into()),
                efforts: vec![],
                default_effort: None,
                is_default: id.contains("grok-4") || id.contains("grok-build"),
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
        self.probe_inner(&ProbeContext {
            binary: binary.cloned(),
            ..ProbeContext::default()
        })
        .await
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
        Ok(kybern_protocol::methods::SessionsListResult {
            sessions: vec![],
            next_cursor: None,
        })
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

        let shared = Arc::new(Shared { events: events_tx.clone() });
        let (finished, closed) = tokio::sync::watch::channel(());
        let worker_shared = shared.clone();
        let cfg = config.clone();
        let worker = crate::process_tree::SessionTask(tokio::spawn(async move {
            let _finished = finished;
            let outcome = run_connection(bin, cfg, worker_shared, cmd_rx, bound_tx).await;
            let error = outcome.err().map(|e| e.to_string());
            let _ = events_tx.send(DriverEvent::Exited { code: None, error }).await;
        }));

        match bound_rx.await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(DriverError::ProcessExited("grok agent exited during startup".into())),
        }
        Ok(SpawnedSession {
            session: Box::new(Handle {
                commands: cmd_tx,
                shared,
                _worker: worker,
                closed,
            }),
            events: events_rx,
        })
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
    closed: tokio::sync::watch::Receiver<()>,
    _worker: crate::process_tree::SessionTask,
    commands: mpsc::Sender<SessionCommand>,
    shared: Arc<Shared>,
}

fn always_approve(mode: PermissionMode) -> bool {
    matches!(mode, PermissionMode::Auto | PermissionMode::FullAccess)
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

async fn run_connection(
    bin: PathBuf,
    config: SessionConfig,
    shared: Arc<Shared>,
    mut commands: mpsc::Receiver<SessionCommand>,
    bound: oneshot::Sender<Result<String>>,
) -> Result<()> {
    let mut cmd = Command::new(&bin).no_window();
    cmd.arg("--no-auto-update")
        .arg("agent");
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
            }
        });
    }
    let agent = ByteStreams::new(stdin.compat_write(), stdout.compat());
    let _child_guard = child;

    let cwd = config.cwd.clone();
    let events = shared.events.clone();
    let mut bound = Some(bound);

    let msg_state: Arc<Mutex<(u64, bool)>> = Arc::new(Mutex::new((0, false)));
    let msg_state_notif = msg_state.clone();
    let native_mcp_servers = grok_mcp_servers(config.native_tool_bridge.as_ref())?;

    let result = Client
        .builder()
        .name("kybern")
        .on_receive_notification(
            async move |n: SessionNotification, _cx| {
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
                            let _ = ev
                                .send(DriverEvent::TextDelta {
                                    message_id: id,
                                    origin: EventOrigin::Root,
                                    delta: t.text,
                                })
                                .await;
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
                            let _ = ev
                                .send(DriverEvent::ThinkingDelta {
                                    message_id: id,
                                    origin: EventOrigin::Root,
                                    delta: t.text,
                                })
                                .await;
                        }
                    }
                    SessionUpdate::ToolCall(tc) => {
                        msg_state_notif.lock().await.1 = false;
                        let _ = ev
                            .send(DriverEvent::ToolStarted(ToolCall {
                                id: tc.tool_call_id.to_string(),
                                name: format!("{:?}", tc.kind).to_lowercase(),
                                input: json!({ "title": tc.title, "raw": tc.raw_input }),
                                parent_id: None,
                            }))
                            .await;
                    }
                    SessionUpdate::ToolCallUpdate(u) => {
                        let status = u.fields.status.map(|s| format!("{s:?}").to_lowercase());
                        if matches!(status.as_deref(), Some("completed") | Some("failed")) {
                            let mut output = json!({ "status": status });
                            if let Some(content) = &u.fields.content {
                                output["content"] = json!(content);
                            }
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
                let msg = format!("Grok Build failed to initialize: {e}. Run `grok login` or set XAI_API_KEY.");
                if let Some(b) = bound.take() {
                    let _ = b.send(Err(DriverError::Protocol(msg.clone())));
                }
                return Err(agent_client_protocol::Error::from(anyhow::anyhow!(msg)));
            }

            // Assume already authenticated (grok login or XAI_API_KEY). Explicit auth can be added later.
            let session_id = match &config.resume_session_id {
                Some(id) => {
                    let request = LoadSessionRequest::new(id.clone(), cwd.clone()).mcp_servers(native_mcp_servers.clone());
                    match cx.send_request(request).block_task().await {
                        Ok(_) => agent_client_protocol::schema::v1::SessionId::from(id.clone()),
                        Err(_) => {
                            // Fall back to a new session if load fails.
                            let request = NewSessionRequest::new(cwd.clone()).mcp_servers(native_mcp_servers);
                            cx.send_request(request).block_task().await?.session_id
                        }
                    }
                }
                None => {
                    let request = NewSessionRequest::new(cwd.clone()).mcp_servers(native_mcp_servers);
                    cx.send_request(request).block_task().await?.session_id
                }
            };

            if let Some(model) = &config.model {
                let _ = cx
                    .send_request(SetSessionConfigOptionRequest::new(session_id.clone(), "model", model.as_str()))
                    .block_task()
                    .await;
            }

            if let Some(b) = bound.take() {
                let _ = b.send(Ok(session_id.to_string()));
            }
            let _ = shared
                .events
                .send(DriverEvent::SessionBound {
                    session_id: session_id.to_string(),
                    model: config.model.clone(),
                })
                .await;

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
                                DriverEvent::TurnCompleted {
                                    stop_reason: stop,
                                    usage: Usage::default(),
                                    cost_usd: None,
                                    duration_ms,
                                    anchors: Default::default(),
                                }
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
        self.commands
            .send(SessionCommand::Prompt { text })
            .await
            .map_err(|_| DriverError::ProcessExited("session closed".into()))?;
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
