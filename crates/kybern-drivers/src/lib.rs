//! Native drivers for coding agents.
//!
//! Each provider gets its own module speaking that provider's own protocol.
//! The daemon only sees [`AgentDriver`] and [`AgentSession`]: it spawns a
//! session per thread, feeds it user messages, answers permission requests,
//! and turns [`DriverEvent`]s into persisted thread events.

pub mod account_auth;
pub mod antigravity;
pub mod binary;
#[cfg(test)]
mod test_process;
#[cfg(test)]
pub(crate) mod test_support {
    use kybern_protocol::*;
    use uuid::Uuid;

    /// A user message holding one thread message and one results batch.
    pub fn orchestration_message() -> UserMessage {
        UserMessage {
            parts: vec![
                ContentPart::Text { text: "intro".into() },
                ContentPart::ThreadMessage {
                    message_id: Uuid::nil(),
                    from_thread_id: None,
                    from_title: "Kybern".into(),
                    purpose: ThreadMessagePurpose::Warning,
                    reply_to: None,
                    body: "stop editing a.rs".into(),
                },
                ContentPart::AgentResults { items: Vec::new() },
            ],
        }
    }

    pub const MARKERS: [&str; 3] = ["Message from Kybern · warning", "stop editing a.rs", "Results from 0 delegated agent(s):"];

    pub fn assert_flattened(text: &str) {
        for marker in MARKERS {
            assert!(text.contains(marker), "missing `{marker}` in `{text}`");
        }
    }
}
pub mod claude;
mod claude_catalog;
mod claude_config;
pub mod claude_integrations;
mod claude_usage;
pub mod codex;
pub mod codex_integrations;
pub mod cursor;
pub mod edit_paths;
pub mod grok;
pub mod login_lend;
pub mod login_recipes;
pub mod models;
mod ndjson;
pub mod omp_profile;
pub mod opencode;
pub mod pi;
pub mod process_tree;
pub mod registry;
pub mod sessions;
pub mod update;

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::PathBuf;

use async_trait::async_trait;
use kybern_protocol::*;
use serde_json::Value;
use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("{0} isn’t installed or isn’t on PATH")]
    BinaryNotFound(String),
    #[error("provider version {found} is older than required {required}")]
    VersionTooOld { found: String, required: String },
    #[error("provider process exited: {0}")]
    ProcessExited(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, DriverError>;

/// Read-only context used while discovering a provider's installed
/// capabilities and effective defaults.
///
/// Provider catalogs are not always machine-global. Claude Code, for example,
/// layers user, project, local, managed, and environment settings. Keeping the
/// context here lets each native driver follow its own harness semantics.
#[derive(Debug, Clone, Default)]
pub struct ProbeContext {
    pub binary: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
}

/// Everything a driver needs to start (or resume) a provider session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: PermissionMode,
    /// Optional daemon-owned tool bridge for this provider session.
    ///
    /// The bridge is deliberately session-scoped.  A driver may expose it
    /// through the provider's native MCP or dynamic-tool mechanism, but must
    /// never turn the opaque credential into a general daemon credential or
    /// interpolate it into a shell prompt.
    pub native_tool_bridge: Option<NativeToolBridge>,
    /// Resume this provider session instead of starting fresh.
    pub resume_session_id: Option<String>,
    /// The resumed conversation never received Kybern's instructions block (it
    /// began with the guide off, under an older Kybern, or outside Kybern). A
    /// harness without a system channel (Cursor) sends the block once more.
    pub resume_lacks_instructions: bool,
    /// When resuming, fork into a new provider session (used by rewind).
    pub fork: bool,
    /// When forking, drop everything after this point in the provider conversation.
    pub rewind: Option<RewindPoint>,
    /// Absolute path to the provider binary; `None` means look up on PATH.
    pub binary: Option<PathBuf>,
    pub env: HashMap<String, String>,
}

/// A provider-neutral description of tools that the daemon is willing to
/// expose to one live provider session.
///
/// `endpoint` is optional because Codex app-server can carry the same tool
/// definitions as experimental dynamic tools over its existing JSON-RPC
/// transport.  MCP-capable providers use the endpoint and authorization
/// fields.  The authorization value must be an opaque, per-session capability
/// issued by the daemon; it must not be the daemon's normal client token.
#[derive(Clone, PartialEq)]
pub struct NativeToolBridge {
    pub server_name: String,
    pub endpoint: Option<String>,
    pub authorization: Option<String>,
    /// Role and bounded project context: a project Coordinator's lead instructions
    /// and the project brief and notes index for threads that carry them. Threads
    /// without either leave this unset.
    pub coordinator_instructions: Option<String>,
    /// Kybern's "Working in Kybern" guide, rendered from this session's tools.
    /// Every thread may carry one; it is independent of the coordinator role.
    pub guide: Option<String>,
    pub tools: Vec<NativeToolDefinition>,
    pub restrictions: NativeToolRestrictions,
}

impl fmt::Debug for NativeToolBridge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeToolBridge")
            .field("server_name", &self.server_name)
            .field("endpoint", &self.endpoint)
            .field("authorization", &self.authorization.as_ref().map(|_| "<redacted>"))
            .field("coordinator_instructions", &self.coordinator_instructions.as_ref().map(|_| "<coordinator-scoped>"))
            .field("guide", &self.guide.as_ref().map(String::len))
            .field("tools", &self.tools)
            .field("restrictions", &self.restrictions)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NativeToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema object accepted by the provider's native tool protocol.
    pub input_schema: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeToolRestrictions {
    /// If non-empty, only these bridge tool names may be exposed.
    pub allowed_tools: Vec<String>,
    /// Bridge tool names to omit even when present in `allowed_tools` or in
    /// the bridge's tool list. Provider-specific built-in names are handled
    /// by each driver's native restriction mechanism.
    pub denied_tools: Vec<String>,
    /// The coordinator policy requires the provider to enforce the restriction
    /// at the native tool boundary.  Drivers return an explicit unsupported
    /// error when their harness cannot do that.
    pub require_enforcement: bool,
    /// With `require_enforcement`, keep the harness's read-only built-in tools
    /// (reading, searching and listing files, and web lookups where the harness
    /// has them) instead of switching every built-in tool off. Never a shell or
    /// an editor. Ignored without `require_enforcement`.
    pub allow_read_only_builtins: bool,
}

impl NativeToolRestrictions {
    /// Whether a bridge tool of this name may be exposed under these restrictions.
    pub fn permits(&self, name: &str) -> bool {
        (self.allowed_tools.is_empty() || self.allowed_tools.iter().any(|allowed| allowed == name))
            && !self.denied_tools.iter().any(|denied| denied == name)
    }
}

impl NativeToolBridge {
    /// The system or developer text a provider receives: the guide, then a
    /// coordinator's role. Deterministic, so the provider's prefix stays cacheable.
    pub fn provider_instructions(&self) -> Option<String> {
        match (self.guide.as_deref(), self.coordinator_instructions.as_deref()) {
            (Some(guide), Some(role)) => Some(format!("{guide}\n{role}")),
            (Some(text), None) | (None, Some(text)) => Some(text.to_owned()),
            (None, None) => None,
        }
    }

    /// The guide for the first prompt of a harness that has no system channel.
    pub fn guide(&self) -> Option<&str> {
        self.guide.as_deref()
    }

    pub fn tools(&self) -> impl Iterator<Item = &NativeToolDefinition> {
        self.tools.iter().filter(|tool| self.restrictions.permits(&tool.name))
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.tools().any(|tool| tool.name == name)
    }

    pub fn validate(&self) -> Result<()> {
        if self.server_name.is_empty() || self.server_name.len() > 64 {
            return Err(DriverError::Unsupported("native tool bridge has an invalid server name".into()));
        }
        if self.tools.len() > 64 {
            return Err(DriverError::Unsupported("native tool bridge exposes too many tools".into()));
        }
        if self.coordinator_instructions.as_ref().is_some_and(|instructions| instructions.is_empty() || instructions.len() > 64 * 1024) {
            return Err(DriverError::Unsupported("native tool bridge has invalid coordinator instructions".into()));
        }
        if self.guide.as_ref().is_some_and(|guide| guide.is_empty() || guide.len() > 16 * 1024) {
            return Err(DriverError::Unsupported("native tool bridge has an invalid guide".into()));
        }
        for tool in &self.tools {
            if tool.name.is_empty()
                || tool.name.len() > 128
                || !tool.name.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                || !tool.input_schema.is_object()
            {
                return Err(DriverError::Unsupported(format!("native tool bridge has invalid tool `{}`", tool.name)));
            }
        }
        if self.endpoint.is_some() != self.authorization.is_some() {
            return Err(DriverError::Unsupported("native tool bridge endpoint and authorization must be provided together".into()));
        }
        Ok(())
    }
}

/// Provider-side identifiers of a turn boundary, recorded when the turn completed.
#[derive(Debug, Clone, Default)]
pub struct TurnAnchors {
    /// Provider id of the turn being dropped (Codex turn id, OpenCode user message id, Claude user uuid).
    pub turn_id: Option<String>,
    /// Provider id of the last entry of the previous turn (Claude: last assistant uuid).
    pub previous_end: Option<String>,
}

/// Where to cut the conversation when forking for a rewind.
#[derive(Debug, Clone)]
pub struct RewindPoint {
    /// Anchors of the first turn to drop.
    pub drop_from: TurnAnchors,
    /// Anchors of the last turn to keep, if any.
    pub keep_through: Option<TurnAnchors>,
}

/// Provider-normalized task metadata. The daemon adds Kybern thread/turn ids
/// and durable timestamps before publishing it.
#[derive(Debug, Clone)]
pub struct DriverRuntimeTask {
    pub id: String,
    pub kind: RuntimeTaskKind,
    pub status: RuntimeTaskStatus,
    pub title: String,
    pub detail: Option<String>,
    pub provider_type: Option<String>,
    pub parent_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub provider_thread_id: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub backgrounded: bool,
    pub last_tool_name: Option<String>,
    pub usage: Option<Usage>,
    pub stats: RuntimeTaskStats,
    pub capabilities: RuntimeTaskCapabilities,
}

#[derive(Debug, Clone)]
pub struct DriverRuntimeTaskUpdate {
    pub id: String,
    pub status: Option<RuntimeTaskStatus>,
    pub detail: Option<String>,
    pub backgrounded: Option<bool>,
    pub last_tool_name: Option<String>,
    pub usage: Option<Usage>,
    pub stats: Option<RuntimeTaskStats>,
    pub capabilities: Option<RuntimeTaskCapabilities>,
}

impl DriverRuntimeTaskUpdate {
    pub fn status(id: impl Into<String>, status: RuntimeTaskStatus) -> Self {
        Self {
            id: id.into(),
            status: Some(status),
            detail: None,
            backgrounded: None,
            last_tool_name: None,
            usage: None,
            stats: None,
            capabilities: None,
        }
    }
}

/// Normalized stream of what a provider is doing. Ordered per session.
#[derive(Debug, Clone)]
pub enum DriverEvent {
    /// A native child's PreToolUse callback accepted exactly this queued input.
    SubagentMessageDelivered {
        task_id: String,
        message_id: String,
    },
    /// Root provider inference began, possibly after a background notification
    /// with no new user message. Repeated signals within a turn are harmless.
    ResponseStarted,
    /// A provider response ended while explicit input remains queued. Keep the
    /// turn active, but start a new logical root message for the next response.
    ResponseBoundary,
    /// Provider reported its session id and effective model.
    SessionBound {
        session_id: String,
        model: Option<String>,
    },
    /// Text of an assistant message. A non-root `origin` (its `task_id` may be
    /// the runtime task id, the provider thread id or the launching tool call
    /// id) marks a subagent's own prose: the daemon routes it to that
    /// subagent's thread and never to the parent transcript.
    TextDelta {
        message_id: String,
        origin: EventOrigin,
        delta: String,
    },
    ImageReceived {
        id: String,
        origin: EventOrigin,
        source: String,
    },
    ThinkingDelta {
        message_id: String,
        origin: EventOrigin,
        delta: String,
    },
    /// The provider closed a reasoning block. Text or tool calls may follow in
    /// the same message; without this the reasoning looks live until they do.
    ThinkingCompleted {
        message_id: String,
        origin: EventOrigin,
    },
    /// Full text of an assistant message once the provider finalizes it.
    AsyncQuestions(kybern_protocol::AsyncQuestionRequest),
    MessageCompleted {
        message_id: String,
        origin: EventOrigin,
        text: String,
        thinking: Option<String>,
    },
    ToolStarted(ToolCall),
    /// Incremental output from a running tool (command stdout, patch progress).
    ToolOutputDelta {
        tool_call_id: String,
        delta: String,
    },
    ToolCompleted {
        tool_call_id: String,
        output: Value,
        is_error: bool,
    },
    /// The prompt a native subagent was launched with. Sent before or right
    /// after its `RuntimeTaskStarted`; it becomes the first message of the
    /// subagent's own thread. `task_id` is the runtime task id.
    SubagentPrompt {
        task_id: String,
        prompt: String,
    },
    /// An item that belongs only to a subagent's own conversation: the daemon
    /// records it in the subagent's thread and nowhere in the parent's log.
    /// `task_id` is the runtime task id, the provider thread id, or the
    /// launching tool call id. The inner event is a `TextDelta`,
    /// `ThinkingDelta`, `ThinkingCompleted`, `MessageCompleted`, `ToolStarted`,
    /// `ToolOutputDelta` or `ToolCompleted`; its origin and tool `parent_id`
    /// are ignored. Drivers use it for harnesses whose child tool calls were
    /// never part of the parent transcript (Codex, OpenCode). Harnesses whose
    /// child tool calls already nest under the launch row (Claude, Cursor)
    /// keep sending `ToolStarted` with `parent_id` and origin-tagged text
    /// instead, and the daemon mirrors those into the subagent's thread.
    SubagentOnly {
        task_id: String,
        event: Box<DriverEvent>,
    },
    RuntimeTaskStarted(DriverRuntimeTask),
    /// A provider-stable task began another unit of work after previously
    /// reaching a terminal state. Unlike a late progress update, this may
    /// deliberately reactivate the existing runtime-task row.
    RuntimeTaskResumed(DriverRuntimeTaskUpdate),
    RuntimeTaskUpdated(DriverRuntimeTaskUpdate),
    RuntimeTaskCompleted(DriverRuntimeTaskUpdate),
    /// The provider is blocked waiting for `respond_permission`.
    PermissionRequest {
        request_id: String,
        tool_call_id: Option<String>,
        tool_name: String,
        input: Value,
        summary: String,
        suggestions: Vec<Value>,
    },
    /// The provider withdrew a pending permission request (tool was cancelled).
    PermissionWithdrawn {
        request_id: String,
    },
    /// The provider is waiting for a result from one of Kybern's internal,
    /// read-only app tools. The daemon binds the request to the live session's
    /// owning thread; no client-visible RPC or credential is involved.
    AppToolRequest {
        request_id: String,
        name: String,
        arguments: Value,
    },
    TurnCompleted {
        stop_reason: StopReason,
        usage: Usage,
        cost_usd: Option<f64>,
        duration_ms: u64,
        anchors: TurnAnchors,
    },
    TurnFailed {
        error: String,
    },
    CommandsUpdated(Vec<kybern_protocol::ProviderCommand>),
    UsageUpdated(kybern_protocol::ProviderUsage),
    Notice {
        level: NoticeLevel,
        text: String,
        data: Option<Value>,
    },
    /// The provider process ended. No further events follow.
    Exited {
        code: Option<i32>,
        error: Option<String>,
    },
}

#[async_trait]
pub trait AgentSession: Send + Sync {
    /// Queue input for this exact active native child. Never write it to root stdin.
    async fn send_subagent_message(&self, _task_id: &str, _message_id: &str, _message: &UserMessage) -> Result<()> {
        Err(DriverError::Unsupported("This harness does not support native subagent messages.".into()))
    }
    /// Queue a user turn. `message_id` is kybern's id for the message; drivers that
    /// accept a client-chosen id use it so rewinds can reference the turn.
    async fn send_message(&self, message_id: &str, message: &UserMessage) -> Result<()>;
    /// Deliver explicit user input to the current turn without interrupting it.
    async fn steer(&self, _message_id: &str, _message: &UserMessage) -> Result<()> {
        Err(DriverError::Unsupported(
            "This harness does not support input during a turn. Queue your message or try again when it finishes.".into(),
        ))
    }
    /// Initiate native compaction; report completion through the usual turn events.
    async fn compact(&self) -> Result<()> {
        Err(DriverError::Unsupported("This harness does not expose manual compaction. Automatic compaction remains available.".into()))
    }
    async fn interrupt(&self) -> Result<()>;
    async fn set_permission_mode(&self, mode: PermissionMode) -> Result<()>;
    async fn set_model(&self, model: &str) -> Result<()>;
    async fn set_effort(&self, effort: &str) -> Result<()>;
    async fn respond_permission(&self, request_id: &str, decision: &ApprovalDecision) -> Result<()>;
    /// Return the result of an internal Kybern app tool request.
    async fn respond_app_tool(&self, _request_id: &str, _result: std::result::Result<Value, String>) -> Result<()> {
        Err(DriverError::Unsupported("Kybern app tools".into()))
    }
    /// Stop one provider-owned subagent or background process without
    /// interrupting unrelated work in the parent thread.
    async fn stop_runtime_task(&self, _task: &RuntimeTask) -> Result<()> {
        Err(DriverError::Unsupported("targeted task stop".into()))
    }
    /// Detach a foreground provider task so the parent can continue.
    async fn background_runtime_task(&self, _task: &RuntimeTask) -> Result<()> {
        Err(DriverError::Unsupported("task backgrounding".into()))
    }
    /// Graceful shutdown. The event stream ends with `Exited`.
    async fn close(&self) -> Result<()>;
}

pub struct SpawnedSession {
    pub session: Box<dyn AgentSession>,
    pub events: mpsc::Receiver<DriverEvent>,
}

#[async_trait]
pub trait AgentDriver: Send + Sync {
    fn kind(&self) -> ProviderKind;
    /// Look for the binary, read its version, report capabilities.
    async fn probe(&self, binary: Option<&PathBuf>) -> ProviderStatus;
    /// Probe with the same project and environment that a new session would
    /// inherit. Drivers whose catalog is machine-global can use `probe`.
    async fn probe_with_context(&self, context: &ProbeContext) -> ProviderStatus {
        self.probe(context.binary.as_ref()).await
    }
    /// Explicit model reload; drivers with another catalog cache bypass it.
    async fn probe_fresh_with_context(&self, context: &ProbeContext) -> ProviderStatus {
        self.probe_with_context(context).await
    }
    /// Discover saved conversations without starting a model turn.
    async fn list_sessions(
        &self,
        _context: &ProbeContext,
        _cursor: Option<&str>,
        _query: &str,
    ) -> Result<kybern_protocol::methods::SessionsListResult> {
        Err(DriverError::Unsupported("saved sessions are unavailable for this harness".into()))
    }
    /// Read the native conversation before importing it into Kybern.
    async fn read_session(&self, _context: &ProbeContext, _id: &str) -> Result<sessions::SessionHistory> {
        Err(DriverError::Unsupported("session history is unavailable for this harness".into()))
    }
    /// A reader that keeps one helper process (when the harness needs one) across many
    /// `list_page` and `read` calls. Used by the first-run scan and bulk import.
    async fn open_session_reader(&self, _context: &ProbeContext) -> Result<Box<dyn sessions::SessionReader>> {
        Err(DriverError::Unsupported("saved sessions are unavailable for this harness".into()))
    }
    /// Whether `spawn` with `fork: true` can drop turns from the conversation.
    fn supports_fork(&self) -> bool {
        true
    }
    /// Run a single tool-free prompt and return the model's text. Used for thread titles.
    /// `env` is the account environment of whoever the prompt is for (a named account's
    /// `CODEX_HOME` / `CLAUDE_CONFIG_DIR`); without it the CLI would use this machine's own login.
    async fn one_shot(
        &self,
        _cwd: &std::path::Path,
        _prompt: &str,
        _binary: Option<&PathBuf>,
        _env: &std::collections::BTreeMap<String, String>,
    ) -> Result<String> {
        Err(DriverError::Unsupported("one-shot prompts".into()))
    }
    async fn spawn(&self, config: SessionConfig) -> Result<SpawnedSession>;
}

/// One-line human summary of a tool call, for approval cards and notifications.
pub fn summarize_tool_call(name: &str, input: &Value) -> String {
    let short =
        |key: &str| input.get(key).and_then(|v| v.as_str()).map(|s| s.lines().next().unwrap_or("").chars().take(120).collect::<String>());
    match name {
        "Bash" | "bash" | "shell" | "execute" => short("command").map(|c| format!("run: {c}")).unwrap_or_else(|| name.to_string()),
        "Edit" | "Write" | "MultiEdit" | "edit" | "write" | "apply_patch" => short("file_path")
            .or_else(|| short("path"))
            .map(|p| format!("{}: {p}", name.to_lowercase()))
            .unwrap_or_else(|| name.to_string()),
        "Read" | "read" => short("file_path").or_else(|| short("path")).map(|p| format!("read: {p}")).unwrap_or_else(|| name.to_string()),
        "WebFetch" | "webfetch" => short("url").map(|u| format!("fetch: {u}")).unwrap_or_else(|| name.to_string()),
        _ => name.to_string(),
    }
}

#[cfg(all(test, unix))]
mod lifecycle_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn native_bridge_debug_redacts_session_capability() {
        let bridge = NativeToolBridge {
            server_name: "kybern".into(),
            endpoint: Some("http://127.0.0.1/native-tools/mcp".into()),
            authorization: Some("secret-capability".into()),
            coordinator_instructions: Some("secret-coordinator-context".into()),
            guide: None,
            tools: Vec::new(),
            restrictions: NativeToolRestrictions::default(),
        };
        let debug = format!("{bridge:?}");
        assert!(!debug.contains("secret-capability"));
        assert!(!debug.contains("secret-coordinator-context"));
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("<coordinator-scoped>"));
    }

    #[test]
    fn provider_instructions_put_the_guide_before_the_coordinator_role() {
        let mut bridge = NativeToolBridge {
            server_name: "kybern".into(),
            endpoint: None,
            authorization: None,
            coordinator_instructions: None,
            guide: None,
            tools: Vec::new(),
            restrictions: NativeToolRestrictions::default(),
        };
        assert_eq!(bridge.provider_instructions(), None);
        bridge.coordinator_instructions = Some("ROLE".into());
        assert_eq!(bridge.provider_instructions().as_deref(), Some("ROLE"));
        bridge.guide = Some("GUIDE".into());
        assert_eq!(bridge.provider_instructions().as_deref(), Some("GUIDE\nROLE"));
        assert_eq!(bridge.guide(), Some("GUIDE"));
        bridge.coordinator_instructions = None;
        assert_eq!(bridge.provider_instructions().as_deref(), Some("GUIDE"));
        assert!(bridge.validate().is_ok());
        bridge.guide = Some(String::new());
        assert!(bridge.validate().is_err());
    }

    #[tokio::test]
    async fn every_driver_cleans_up_cancelled_startup_or_dropped_handle() {
        let registry = registry::DriverRegistry::with_defaults();
        for kind in ProviderKind::ALL {
            let root = tempfile::tempdir().unwrap();
            let binary = root.path().join("harness-fixture");
            // A live descendant announces readiness and cannot expire during
            // the assertion. Cleanup is tested directly, not against a touch
            // racing an 80 ms startup timeout on a loaded CI runner.
            kybern_protocol::process::write_executable(&binary, "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 0.80.5; exit 0; fi\nsleep 300 &\nchild=$!\nprintf '%s %s' \"$$\" \"$child\" > ready.tmp\nmv ready.tmp ready\ncat >/dev/null\nwait\n").unwrap();
            let config = SessionConfig {
                cwd: root.path().into(),
                model: None,
                effort: None,
                permission_mode: match kind {
                    ProviderKind::Cursor => PermissionMode::Auto,
                    ProviderKind::Antigravity => PermissionMode::FullAccess,
                    _ => PermissionMode::Supervised,
                },
                native_tool_bridge: None,
                resume_session_id: None,
                resume_lacks_instructions: false,
                fork: false,
                rewind: None,
                binary: Some(binary.clone()),
                env: if kind == ProviderKind::Cursor {
                    std::collections::HashMap::from([("KYBERN_CURSOR_NODE".into(), binary.display().to_string())])
                } else {
                    std::collections::HashMap::new()
                },
            };
            let driver = registry.get(kind).unwrap();
            let mut startup = Box::pin(driver.spawn(config.clone()));
            let mut session = None;
            let ready = root.path().join("ready");
            // Linux refuses to run a file that another test's freshly forked child still holds open
            // for writing (ETXTBSY) until that child execs, so a busy fixture starts again.
            let mut busy_retries = 0;
            tokio::time::timeout(Duration::from_secs(10), async {
                while !ready.exists() {
                    let mut busy = false;
                    tokio::select! {
                        result = &mut startup, if session.is_none() => {
                            let spawned = match result {
                                Err(DriverError::Io(error)) if error.kind() == std::io::ErrorKind::ExecutableFileBusy && busy_retries < 50 => {
                                    busy = true;
                                    None
                                }
                                result => Some(result.unwrap_or_else(|error| panic!("{kind} fixture startup failed: {error}"))),
                            };
                            if let Some(spawned) = spawned {
                                // Antigravity starts one process per turn, so its session is
                                // lazy: a message starts the process this test then drops.
                                if kind == ProviderKind::Antigravity {
                                    spawned.session.send_message("fixture", &UserMessage::text("hello")).await.unwrap();
                                }
                                session = Some(spawned);
                            }
                        }
                        _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                    }
                    if busy {
                        busy_retries += 1;
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        startup = Box::pin(driver.spawn(config.clone()));
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{kind} fixture did not announce its descendant"));
            let pids = std::fs::read_to_string(ready).unwrap();
            let pids: Vec<u32> = pids.split_whitespace().map(|pid| pid.parse().unwrap()).collect();
            // Also clean up if an assertion fails; only this fixture's isolated
            // group is owned by the guard.
            let _cleanup = crate::process_tree::ProcessTree::new(pids[0]);
            assert!(process_is_running(pids[1]).await, "{kind} descendant was not live before cancellation");
            // Claude can finish its non-blocking startup; the others wait for
            // a protocol handshake. Both ownership paths must close descendants.
            drop(startup);
            drop(session);
            tokio::time::timeout(Duration::from_secs(10), async {
                while process_is_running(pids[1]).await {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{kind} left startup descendant {} running", pids[1]));
        }
    }

    async fn process_is_running(pid: u32) -> bool {
        let output = tokio::process::Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output().await.unwrap();
        // An exited child can briefly remain a zombie until reaped by init.
        let state = String::from_utf8_lossy(&output.stdout);
        !state.trim().is_empty() && !state.trim_start().starts_with('Z')
    }
}

/// Preserve only usable advertised command names; never manufacture terminal commands.
pub(crate) fn provider_commands(value: &Value) -> Vec<kybern_protocol::ProviderCommand> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|command| {
            let name = command.as_str().or_else(|| command.get("name").and_then(Value::as_str))?.trim_start_matches('/');
            if name.is_empty() || name.chars().any(char::is_whitespace) {
                return None;
            }
            Some(kybern_protocol::ProviderCommand {
                name: name.to_string(),
                description: command.get("description").and_then(Value::as_str).unwrap_or("Run harness command").to_string(),
            })
        })
        .collect()
}
