//! Antigravity (`agy`), Google's coding-agent CLI.
//!
//! One `agy` process runs per turn, so a session handle is lazy: `spawn`
//! checks the request and starts nothing, and each `send_message` starts
//! `agy -p= --input-format stream-json`, writes the prompt as one NDJSON line
//! on stdin, closes stdin, and maps the NDJSON that comes back. Turns after the
//! first pass `--conversation <id>`. The design and every measured wire fact
//! are in `docs/design/ade-111-antigravity.md`.
//!
//! Antigravity only runs in Full access (`--dangerously-skip-permissions`):
//! Kybern has no way to approve or limit its tools. Kybern's own tools reach it
//! through one global MCP entry (`kybern mcp-bridge`, added with `agy mcp add`)
//! that forwards to the session's gateway; the session's endpoint and
//! credential travel in the environment of each `agy` process, never on its
//! command line.
mod login;
mod stream;
pub(crate) mod tools;
mod usage;

pub use login::{StoredLogin, parse_login, read_login};
pub use usage::{
    ANTIGRAVITY_USER_AGENT_PREFIX, QuotaBucket, QuotaReading, merge_buckets, parse_load_code_assist, parse_quota_summary, plan_name,
    read_account_usage, to_limits,
};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use kybern_protocol::process::NoWindow;
use kybern_protocol::*;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};

use crate::binary::{at_least, resolve, version_of};
use crate::ndjson::{NdjsonChild, SessionLifetime};
use crate::{AgentDriver, AgentSession, DriverError, DriverEvent, ProbeContext, Result, SessionConfig, SpawnedSession};
use stream::TurnMapper;

/// `result.finished_reason` first shipped in this version.
const MIN_VERSION: (u64, u64, u64) = (1, 4, 11);
/// What `kybern mcp-bridge` reads from the environment `agy` hands it.
const BRIDGE_URL: &str = "KYBERN_MCP_URL";
const BRIDGE_AUTHORIZATION: &str = "KYBERN_MCP_AUTHORIZATION";
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const INSTALL_HINT: &str = "Install with: curl -fsSL https://antigravity.google/cli/install.sh | bash (Windows PowerShell: irm https://antigravity.google/cli/install.ps1 | iex)";
/// The sentence every refused permission mode gets, wherever it is refused.
pub const FULL_ACCESS_ONLY: &str = "Antigravity only runs in Full access. Choose Full access, or pick another agent.";
/// Kybern cannot narrow Antigravity's tools, so it cannot lead a project.
pub const NO_LEAD: &str = "Antigravity can't lead a project: Kybern can't limit its tools. Choose Claude Code or Codex for the lead.";
pub(crate) const SIGN_IN: &str = "Antigravity isn't signed in. Sign in from Settings → Agents → Antigravity, then send your message again.";

/// How long SIGINT gets before SIGTERM. SIGKILL follows after
/// [`crate::process_tree::TERMINATE_GRACE`].
const INTERRUPT_GRACE: Duration = Duration::from_secs(3);
const MODELS_TTL: Duration = Duration::from_secs(10 * 60);
const MODELS_TIMEOUT: Duration = Duration::from_secs(20);
const ONE_SHOT_TIMEOUT: Duration = Duration::from_secs(90);

/// Refuses every permission mode but Full access.
pub fn require_full_access(mode: PermissionMode) -> std::result::Result<(), &'static str> {
    if mode == PermissionMode::FullAccess { Ok(()) } else { Err(FULL_ACCESS_ONLY) }
}

#[derive(Default)]
pub struct AntigravityDriver {
    models: std::sync::Mutex<Option<CachedModels>>,
    catalog_file: Option<PathBuf>,
}

struct CachedModels {
    binary: PathBuf,
    at: Instant,
    models: Vec<ProviderModel>,
}

impl AntigravityDriver {
    /// Keep the model list in `file` so a restarted daemon lists models at once.
    pub fn with_catalog_file(file: PathBuf) -> Self {
        Self { models: std::sync::Mutex::new(None), catalog_file: Some(file) }
    }

    fn cached(&self, binary: &Path, max_age: Duration) -> Option<Vec<ProviderModel>> {
        let guard = self.models.lock().unwrap_or_else(|error| error.into_inner());
        guard.as_ref().filter(|cache| cache.binary == binary && cache.at.elapsed() < max_age).map(|cache| cache.models.clone())
    }

    fn last_good(&self, binary: &Path) -> Vec<ProviderModel> {
        let remembered = self.models.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(cache) = remembered.as_ref().filter(|cache| cache.binary == binary) {
            return cache.models.clone();
        }
        drop(remembered);
        self.catalog_file
            .as_ref()
            .and_then(|file| std::fs::read(file).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    async fn models(&self, binary: &Path, context: &ProbeContext, fresh: bool) -> Vec<ProviderModel> {
        if !fresh && let Some(models) = self.cached(binary, MODELS_TTL) {
            return models;
        }
        let mut command = Command::new(binary).no_window();
        command.arg("models").envs(&context.env).stdin(std::process::Stdio::null());
        if let Some(cwd) = context.cwd.as_deref().filter(|cwd| cwd.is_dir()) {
            command.current_dir(cwd);
        }
        let output = match tokio::time::timeout(MODELS_TIMEOUT, crate::process_tree::output(&mut command)).await {
            Ok(Ok(output)) if output.status.success() => output,
            // Signed out, offline or slow: keep whatever the last good read had.
            _ => return self.last_good(binary),
        };
        let models = parse_models(&String::from_utf8_lossy(&output.stdout));
        if models.is_empty() {
            return self.last_good(binary);
        }
        if let Some(file) = &self.catalog_file
            && let Ok(bytes) = serde_json::to_vec(&models)
        {
            let _ = std::fs::write(file, bytes);
        }
        *self.models.lock().unwrap_or_else(|error| error.into_inner()) =
            Some(CachedModels { binary: binary.to_path_buf(), at: Instant::now(), models: models.clone() });
        models
    }

    async fn probe_context(&self, context: &ProbeContext, fresh: bool) -> ProviderStatus {
        let kind = ProviderKind::Antigravity;
        let mut status = ProviderStatus {
            kind,
            display_name: kind.display_name().into(),
            available: false,
            binary_path: None,
            version: None,
            unavailable_reason: None,
            supported_permission_modes: vec![PermissionMode::FullAccess],
            supports_fork: false,
            supports_model_switch: true,
            supports_effort_switch: true,
            supported_efforts: EFFORTS.iter().map(|effort| effort.to_string()).collect(),
            models: vec![],
            instances: vec!["default".into()],
        };
        let binary = match resolve(kind, context.binary.as_ref()) {
            Ok(binary) => binary,
            Err(error) => {
                status.unavailable_reason = Some(format!("{error}. {INSTALL_HINT}"));
                return status;
            }
        };
        status.binary_path = Some(binary.display().to_string());
        match version_of(&binary, &["--version"]).await {
            Some(version) => {
                status.available = at_least(&version, MIN_VERSION);
                if !status.available {
                    status.unavailable_reason = Some(format!(
                        "Antigravity {version} is older than the required {}.{}.{}. Update it with `agy update`.",
                        MIN_VERSION.0, MIN_VERSION.1, MIN_VERSION.2
                    ));
                }
                status.version = Some(version);
            }
            None => status.unavailable_reason = Some("Could not read Antigravity's version. Verify the configured agy executable.".into()),
        }
        if status.available {
            status.models = self.models(&binary, context, fresh).await;
        }
        status
    }
}

/// Whether `agy models` succeeds with at least one model, which only a signed-in
/// `agy` does. Used when its login is somewhere Kybern cannot read.
pub async fn signed_in_by_models(binary: &Path, context: &ProbeContext) -> bool {
    let mut command = Command::new(binary).no_window();
    command.arg("models").envs(&context.env).stdin(std::process::Stdio::null());
    match tokio::time::timeout(MODELS_TIMEOUT, crate::process_tree::output(&mut command)).await {
        Ok(Ok(output)) if output.status.success() => !parse_models(&String::from_utf8_lossy(&output.stdout)).is_empty(),
        _ => false,
    }
}

/// `agy models`: a "Fetching available models..." line, then `slug<TAB>Display name`.
/// The first slug is the default.
///
/// `agy` lists each thinking tier of a model as its own slug
/// (`gemini-3.8-flash-high`, `-medium`, `-low`, named "Gemini 3.8 Flash (High)"…).
/// Two or more tiers of one model become one row whose efforts are the tiers,
/// so pickers show a single model with an effort slider. The row's id is the
/// first tier `agy` listed (a real slug, so it still runs without the catalog),
/// and its default effort is that tier. [`turn_model`] turns the row and the
/// chosen effort back into the tier's slug.
pub fn parse_models(text: &str) -> Vec<ProviderModel> {
    let mut entries: Vec<(&str, &str)> = Vec::new();
    for line in text.lines() {
        let Some((slug, name)) = line.split_once('\t') else { continue };
        let (slug, name) = (slug.trim(), name.trim());
        if slug.is_empty() || slug.contains(char::is_whitespace) || entries.iter().any(|(seen, _)| *seen == slug) {
            continue;
        }
        entries.push((slug, name));
    }
    // Tiers of each base, in the order `agy` listed them.
    let mut tiers: BTreeMap<&str, Vec<&'static str>> = BTreeMap::new();
    for (slug, name) in &entries {
        if let Some((base, tier, _)) = split_tier(slug, name) {
            tiers.entry(base).or_default().push(tier);
        }
    }
    let mut models: Vec<ProviderModel> = Vec::new();
    let mut grouped: Vec<&str> = Vec::new();
    for (slug, name) in entries {
        let family = split_tier(slug, name).filter(|(base, _, _)| tiers.get(base).is_some_and(|list| list.len() > 1));
        let (display_name, efforts, default_effort) = match family {
            Some((base, _, _)) if grouped.contains(&base) => continue,
            Some((base, tier, family_name)) => {
                grouped.push(base);
                let listed = &tiers[base];
                let efforts = EFFORTS.iter().filter(|effort| listed.contains(effort)).map(|effort| effort.to_string()).collect();
                (family_name.to_string(), efforts, Some(tier.to_string()))
            }
            None => (if name.is_empty() { crate::models::prettify_model_id(slug) } else { name.to_string() }, vec![], None),
        };
        models.push(ProviderModel {
            id: slug.to_string(),
            display_name,
            resolved_id: None,
            description: None,
            provider: None,
            efforts,
            default_effort,
            is_default: models.is_empty(),
            parameters: vec![],
            variants: vec![],
        });
    }
    models
}

/// `gemini-3.8-flash-high` named "Gemini 3.8 Flash (High)" → (`gemini-3.8-flash`,
/// `high`, "Gemini 3.8 Flash"). The slug's suffix and the name's parenthesis
/// must both name the tier, so `gpt-oss-120b-medium` alone stays a plain model
/// and `claude-opus-4-6-thinking` is never mistaken for one.
fn split_tier<'a>(slug: &'a str, name: &'a str) -> Option<(&'a str, &'static str, &'a str)> {
    EFFORTS.iter().find_map(|tier| {
        let base = slug.strip_suffix(tier)?.strip_suffix('-').filter(|base| !base.is_empty())?;
        let open = name.rfind(" (")?;
        let label = name[open + 2..].strip_suffix(')')?;
        label.eq_ignore_ascii_case(tier).then_some((base, *tier, name[..open].trim_end()))
    })
}

/// A model `agy` lists once per tier, as [`parse_models`] grouped it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TierFamily {
    base: String,
    tiers: Vec<String>,
}

/// The tiered families in a catalog [`parse_models`] built.
fn tier_families(models: &[ProviderModel]) -> Vec<TierFamily> {
    models
        .iter()
        .filter(|model| model.efforts.len() > 1)
        .filter_map(|model| {
            let tier = model.default_effort.as_deref()?;
            let base = model.id.strip_suffix(tier)?.strip_suffix('-')?;
            Some(TierFamily { base: base.to_string(), tiers: model.efforts.clone() })
        })
        .collect()
}

/// The `--model` and `--effort` of a turn. A tier of a grouped family runs the
/// tier the effort names (`gemini-3.8-flash-high` at low effort runs
/// `gemini-3.8-flash-low`) and sends no `--effort`, because the tier is the
/// effort; an effort the family has no tier for keeps the held tier. Any other
/// model passes through with its effort.
fn turn_model(model: Option<&str>, effort: Option<&str>, families: &[TierFamily]) -> (Option<String>, Option<String>) {
    let tiered = model.and_then(|model| {
        families.iter().find_map(|family| {
            let held = model.strip_prefix(family.base.as_str())?.strip_prefix('-')?;
            family.tiers.iter().any(|tier| tier == held).then_some((family, held))
        })
    });
    match tiered {
        Some((family, held)) => {
            let tier = effort.filter(|effort| family.tiers.iter().any(|tier| tier == effort)).unwrap_or(held);
            (Some(format!("{}-{tier}", family.base)), None)
        }
        None => (model.map(str::to_string), effort.map(str::to_string)),
    }
}

fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 128
        && !model.starts_with('-')
        && model.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'/'))
}

#[async_trait]
impl AgentDriver for AntigravityDriver {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Antigravity
    }

    fn supports_fork(&self) -> bool {
        false
    }

    async fn probe(&self, binary: Option<&PathBuf>) -> ProviderStatus {
        self.probe_context(&ProbeContext { binary: binary.cloned(), ..ProbeContext::default() }, false).await
    }

    async fn probe_with_context(&self, context: &ProbeContext) -> ProviderStatus {
        self.probe_context(context, false).await
    }

    async fn probe_fresh_with_context(&self, context: &ProbeContext) -> ProviderStatus {
        self.probe_context(context, true).await
    }

    async fn list_sessions(
        &self,
        context: &ProbeContext,
        cursor: Option<&str>,
        query: &str,
    ) -> Result<kybern_protocol::methods::SessionsListResult> {
        crate::sessions::list(ProviderKind::Antigravity, context, cursor, query).await
    }

    async fn read_session(&self, context: &ProbeContext, id: &str) -> Result<crate::sessions::SessionHistory> {
        crate::sessions::read(ProviderKind::Antigravity, context, id).await
    }

    async fn open_session_reader(&self, context: &ProbeContext) -> Result<Box<dyn crate::sessions::SessionReader>> {
        crate::sessions::reader(ProviderKind::Antigravity, context).await
    }

    /// A single tool-free run. `--mode plan` keeps the model from editing the
    /// checkout; it still reads files if it chooses to, which a title does not need.
    async fn one_shot(&self, cwd: &Path, prompt: &str, binary: Option<&PathBuf>, env: &BTreeMap<String, String>) -> Result<String> {
        let bin = resolve(ProviderKind::Antigravity, binary)?;
        let scratch = Scratch::new()?;
        let mut command = Command::new(&bin).no_window();
        command.current_dir(cwd).args(["--output-format", "stream-json", "--input-format", "stream-json", "--mode", "plan"]);
        command.arg("--log-file").arg(scratch.log_file()).arg("-p=").envs(env);
        let child = NdjsonChild::spawn(command)?;
        let run = async {
            child.write(&prompt_line(prompt)).await?;
            child.close_stdin().await?;
            let mut mapper = TurnMapper::new(None);
            let mut lines = child.lines.lock().await;
            while let Some(line) = lines.recv().await {
                mapper.handle(&line);
            }
            mapper.one_shot_answer().ok_or_else(|| DriverError::Protocol("Antigravity returned no answer".into()))
        };
        let result = tokio::time::timeout(ONE_SHOT_TIMEOUT, run).await;
        child.kill().await;
        match result {
            Ok(result) => result,
            Err(_) => Err(DriverError::Protocol("Antigravity one-shot timed out".into())),
        }
    }

    async fn spawn(&self, config: SessionConfig) -> Result<SpawnedSession> {
        require_full_access(config.permission_mode).map_err(|sentence| DriverError::Unsupported(sentence.into()))?;
        if config.fork || config.rewind.is_some() {
            return Err(DriverError::Unsupported(
                "Antigravity can't fork or rewind a conversation. Rewind restores the workspace only.".into(),
            ));
        }
        if let Some(bridge) = config.native_tool_bridge.as_ref() {
            bridge.validate()?;
            if bridge.restrictions.require_enforcement {
                return Err(DriverError::Unsupported(NO_LEAD.into()));
            }
        }
        let binary = resolve(ProviderKind::Antigravity, config.binary.as_ref())?;
        // The guide (and any role text) rides in the first prompt, because `agy` has
        // no system channel Kybern can reach. The daemon renders it from the tools
        // this session really has, so it never advertises one the agent cannot call.
        // A resumed conversation that already holds the block gets nothing; one that
        // never received it gets it once.
        let guide = match (&config.resume_session_id, config.resume_lacks_instructions) {
            (None, _) | (Some(_), true) => config.native_tool_bridge.as_ref().and_then(|bridge| bridge.provider_instructions()),
            (Some(_), false) => None,
        };
        let home = ["HOME", "USERPROFILE"]
            .iter()
            .find_map(|key| config.env.get(*key).cloned().or_else(|| std::env::var(key).ok()))
            .filter(|home| !home.is_empty())
            .map(PathBuf::from);
        let mut env: BTreeMap<String, String> = config.env.into_iter().collect();
        // The bridge process `agy` starts inherits these; without them it serves no tools.
        for key in [BRIDGE_URL, BRIDGE_AUTHORIZATION] {
            env.remove(key);
        }
        if let Some(bridge) = config.native_tool_bridge.as_ref()
            && let (Some(endpoint), Some(authorization)) = (&bridge.endpoint, &bridge.authorization)
        {
            env.insert(BRIDGE_URL.into(), endpoint.clone());
            env.insert(BRIDGE_AUTHORIZATION.into(), authorization.clone());
        }
        let scratch = Arc::new(Scratch::new()?);
        let (tx, events) = mpsc::channel(1024);
        let model = config.model.filter(|model| valid_model(model));
        let effort = config.effort.filter(|effort| EFFORTS.contains(&effort.as_str()));
        let state = State { conversation: config.resume_session_id, model, effort, guide, turn: None, closed: false, next_turn: 0 };
        let session = Session {
            state: Arc::new(Mutex::new(state)),
            run: Arc::new(Run { families: tier_families(&self.last_good(&binary)), binary, cwd: config.cwd, env, home, scratch, tx }),
        };
        Ok(SpawnedSession { session: Box::new(session), events })
    }
}

/// A per-session scratch folder: `agy`'s log file and the images of a prompt.
/// Removed with the session.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("kybern-agy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }

    fn log_file(&self) -> PathBuf {
        self.0.join("agy.log")
    }

    fn image_dir(&self) -> PathBuf {
        self.0.join("images")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What every turn needs and a session never changes.
struct Run {
    binary: PathBuf,
    /// Tiered models from the catalog at spawn: their effort picks the slug.
    families: Vec<TierFamily>,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
    home: Option<PathBuf>,
    scratch: Arc<Scratch>,
    tx: mpsc::Sender<DriverEvent>,
}

struct State {
    conversation: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    /// Kybern's instructions block, until a prompt that can carry it is sent.
    guide: Option<String>,
    turn: Option<Turn>,
    closed: bool,
    next_turn: u64,
}

struct Turn {
    id: u64,
    child: Arc<NdjsonChild>,
    interrupted: Arc<AtomicBool>,
    /// Dropping it stops the process tree and the reader task.
    _lifetime: SessionLifetime,
}

pub struct Session {
    state: Arc<Mutex<State>>,
    run: Arc<Run>,
}

/// The one NDJSON line `agy` reads for a turn.
fn prompt_line(text: &str) -> Value {
    json!({"event": "user", "message": {"role": "user", "content": text}})
}

/// The argument list for one turn (without the binary).
fn turn_args(model: Option<&str>, effort: Option<&str>, conversation: Option<&str>, log_file: &Path) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> =
        ["--output-format", "stream-json", "--input-format", "stream-json", "--dangerously-skip-permissions"].map(Into::into).into();
    if let Some(model) = model {
        args.extend(["--model".into(), model.into()]);
    }
    if let Some(effort) = effort {
        args.extend(["--effort".into(), effort.into()]);
    }
    if let Some(conversation) = conversation {
        args.extend(["--conversation".into(), conversation.into()]);
    }
    args.extend(["--log-file".into(), log_file.into()]);
    // `-p=` attached: a bare `-p` swallows the next flag as the prompt.
    args.push("-p=".into());
    args
}

#[async_trait]
impl AgentSession for Session {
    async fn send_message(&self, _message_id: &str, message: &UserMessage) -> Result<()> {
        let mut state = self.state.lock().await;
        if state.closed {
            return Err(DriverError::ProcessExited("session is closed; send again to resume".into()));
        }
        if state.turn.is_some() {
            return Err(DriverError::Protocol(
                "Antigravity is still working on the previous message. Wait for it, or interrupt it first.".into(),
            ));
        }
        let text = prompt_text(message, &self.run.scratch);
        // Slash commands (skills) must stay at the start of the prompt; the guide waits for the next ordinary one.
        let guide = if text.trim_start().starts_with('/') { None } else { state.guide.take() };
        let prompt = match &guide {
            Some(guide) => crate::cursor::with_guide(guide, &text),
            None => text,
        };
        let (model, effort) = turn_model(state.model.as_deref(), state.effort.as_deref(), &self.run.families);
        let mut command = Command::new(&self.run.binary).no_window();
        command.current_dir(&self.run.cwd).args(turn_args(
            model.as_deref(),
            effort.as_deref(),
            state.conversation.as_deref(),
            &self.run.scratch.log_file(),
        ));
        command.envs(&self.run.env);
        let spawned = async {
            let child = NdjsonChild::spawn(command)?;
            let sent = async {
                child.write(&prompt_line(&prompt)).await?;
                child.close_stdin().await
            }
            .await;
            match sent {
                Ok(()) => Ok(child),
                Err(error) => {
                    child.kill().await;
                    Err(error)
                }
            }
        }
        .await;
        let child = match spawned {
            Ok(child) => Arc::new(child),
            Err(error) => {
                // The agent never saw it; offer the guide again with the retry.
                if guide.is_some() {
                    state.guide = guide;
                }
                return Err(error);
            }
        };
        let id = state.next_turn;
        state.next_turn += 1;
        let interrupted = Arc::new(AtomicBool::new(false));
        let mut lifetime = SessionLifetime::new(child.clone());
        let task = tokio::spawn(run_turn(
            Arc::downgrade(&self.state),
            self.run.clone(),
            child.clone(),
            interrupted.clone(),
            id,
            guide,
            state.conversation.clone(),
        ));
        lifetime.track(task);
        state.turn = Some(Turn { id, child, interrupted, _lifetime: lifetime });
        Ok(())
    }

    async fn interrupt(&self) -> Result<()> {
        let child = {
            let state = self.state.lock().await;
            let Some(turn) = state.turn.as_ref() else { return Ok(()) };
            turn.interrupted.store(true, Ordering::SeqCst);
            turn.child.clone()
        };
        tokio::spawn(async move {
            // Ctrl+C lets `agy` write its own "interrupted" result and take its tool
            // children down with it; escalate only if it does not leave.
            if child.interrupt() && tokio::time::timeout(INTERRUPT_GRACE, child.wait()).await.is_ok() {
                return;
            }
            // SIGTERM, then SIGKILL after the process-tree grace (Windows: forced stop).
            child.kill().await;
        });
        Ok(())
    }

    async fn set_permission_mode(&self, mode: PermissionMode) -> Result<()> {
        require_full_access(mode).map_err(|sentence| DriverError::Unsupported(sentence.into()))
    }

    async fn set_model(&self, model: &str) -> Result<()> {
        if !valid_model(model) {
            return Err(DriverError::Protocol("Choose one of Antigravity's listed models.".into()));
        }
        self.state.lock().await.model = Some(model.to_string());
        Ok(())
    }

    async fn set_effort(&self, effort: &str) -> Result<()> {
        if !EFFORTS.contains(&effort) {
            return Err(DriverError::Protocol("Antigravity's effort is low, medium, high, xhigh or max.".into()));
        }
        self.state.lock().await.effort = Some(effort.to_string());
        Ok(())
    }

    async fn respond_permission(&self, _: &str, _: &ApprovalDecision) -> Result<()> {
        Err(DriverError::Unsupported("Antigravity runs in Full access and never asks for approval".into()))
    }

    async fn close(&self) -> Result<()> {
        let turn = {
            let mut state = self.state.lock().await;
            state.closed = true;
            state.turn.take()
        };
        if let Some(turn) = turn {
            turn.child.kill().await;
        }
        let _ = self.run.tx.send(DriverEvent::Exited { code: None, error: None }).await;
        Ok(())
    }
}

/// Read one turn's output, report how it ended, and free the session for the next message.
async fn run_turn(
    state: Weak<Mutex<State>>,
    run: Arc<Run>,
    child: Arc<NdjsonChild>,
    interrupted: Arc<AtomicBool>,
    id: u64,
    guide: Option<String>,
    conversation: Option<String>,
) {
    let mut mapper = TurnMapper::resumed(run.home.clone(), conversation);
    {
        let mut lines = child.lines.lock().await;
        while let Some(line) = lines.recv().await {
            for event in mapper.handle(&line) {
                if run.tx.send(event).await.is_err() {
                    return;
                }
            }
        }
    }
    // Stdout closed: the process is leaving. Do not wait on it forever.
    let code = match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(code) => code,
        Err(_) => {
            child.kill().await;
            None
        }
    };
    let mut last_stderr = None;
    for attempt in 0..2 {
        let mut stderr = child.stderr.lock().await;
        while let Ok(line) = stderr.try_recv() {
            if !line.trim().is_empty() {
                last_stderr = Some(line);
            }
        }
        drop(stderr);
        if last_stderr.is_some() || code == Some(0) || attempt == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    let was_interrupted = interrupted.load(Ordering::SeqCst);
    let conversation = mapper.conversation().map(str::to_string);
    let started = mapper.saw_init();
    for event in mapper.finish(code, last_stderr.as_deref(), was_interrupted) {
        if run.tx.send(event).await.is_err() {
            return;
        }
    }
    let Some(state) = state.upgrade() else { return };
    let turn = {
        let mut state = state.lock().await;
        if let Some(conversation) = conversation {
            state.conversation = Some(conversation);
        }
        // The prompt never reached a conversation: carry the guide on the retry.
        if !started && state.guide.is_none() {
            state.guide = guide;
        }
        if state.turn.as_ref().is_some_and(|turn| turn.id == id) { state.turn.take() } else { None }
    };
    // Dropping the turn stops this task, so it is the last thing done.
    drop(turn);
}

/// The text of a prompt. Images are saved in the session's scratch folder and
/// named by path so the agent can open them.
fn prompt_text(message: &UserMessage, scratch: &Scratch) -> String {
    use base64::Engine;
    let mut out = String::new();
    let mut images = 0;
    for part in &message.parts {
        match part {
            ContentPart::Text { text } => out.push_str(text),
            ContentPart::FileMention { path } => {
                out.push('@');
                out.push_str(path);
            }
            ContentPart::ThreadReference { thread_id, title, .. } => out.push_str(&thread_reference_text(*thread_id, title)),
            ContentPart::Skill { name, .. } => {
                out.push('/');
                out.push_str(name);
            }
            ContentPart::Mention { name, .. } => {
                out.push('@');
                out.push_str(name);
            }
            ContentPart::Image { media_type, data } => {
                images += 1;
                let extension = match media_type.as_str() {
                    "image/png" => "png",
                    "image/jpeg" | "image/jpg" => "jpg",
                    "image/gif" => "gif",
                    "image/webp" => "webp",
                    _ => "img",
                };
                let saved =
                    base64::engine::general_purpose::STANDARD.decode(data).ok().filter(|bytes| bytes.len() <= 20 * 1024 * 1024).and_then(
                        |bytes| {
                            let dir = scratch.image_dir();
                            std::fs::create_dir_all(&dir).ok()?;
                            let path = dir.join(format!("image-{}.{extension}", uuid::Uuid::new_v4()));
                            std::fs::write(&path, bytes).ok()?;
                            Some(path)
                        },
                    );
                match saved {
                    Some(path) => out.push_str(&format!("[image {images}: {}]", path.display())),
                    None => out.push_str("[image]"),
                }
            }
            ContentPart::Attachment { name, .. } => {
                out.push('[');
                out.push_str(name);
                out.push(']');
            }
            ContentPart::ThreadMessage { .. } | ContentPart::AgentResults { .. } | ContentPart::SystemRow { .. } => {
                if let Some(flat) = part.orchestration_text() {
                    out.push_str(&flat);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
