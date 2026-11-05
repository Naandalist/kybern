//! Claude Code driver: speaks the CLI's `stream-json` protocol over stdio.
//!
//! Spawn shape (mirrors what the official Agent SDK does):
//! `claude --output-format stream-json --input-format stream-json --verbose
//!   --include-partial-messages --permission-prompt-tool stdio --session-id <uuid>`
//! The process stays alive across turns; each user message on stdin starts a
//! turn that ends with a `result` frame. Permission prompts arrive as
//! `control_request{subtype:"can_use_tool"}` and are answered on stdin.

use kybern_protocol::process::NoWindow;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use kybern_protocol::*;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc, oneshot};
use uuid::Uuid;

use crate::binary::{at_least, resolve, version_of};
use crate::claude_catalog::{CatalogKey, CatalogModel, ModelCatalog};
use crate::claude_config::ClaudeConfig;
use crate::ndjson::NdjsonChild;
use crate::{
    AgentDriver, AgentSession, DriverError, DriverEvent, DriverRuntimeTask, DriverRuntimeTaskUpdate, NativeToolBridge, ProbeContext,
    Result, SessionConfig, SpawnedSession, summarize_tool_call,
};

const MIN_VERSION: (u64, u64, u64) = (2, 1, 0);

/// Claude Code labels its API requests with this variable. Headless runs default to `sdk-cli` (or
/// inherit `sdk-*` from a parent Claude process), and measurements in anthropics/claude-code#97074
/// show those labels drawing the 5-hour window about 1.8× faster per token than any other label.
/// Anthropic has not confirmed this, so the fix is only to name Kybern honestly.
pub(crate) const ENTRYPOINT_ENV: &str = "CLAUDE_CODE_ENTRYPOINT";
pub(crate) const KYBERN_ENTRYPOINT: &str = "kybern";

/// The label for a Claude process: a value set in the session's own environment wins; anything
/// inherited from the daemon's environment is replaced, so Kybern never runs as `sdk-*` or as
/// another client's label (`cli`, `claude-vscode`).
pub(crate) fn entrypoint(session_value: Option<&String>) -> String {
    session_value.map(|value| value.trim()).filter(|value| !value.is_empty()).unwrap_or(KYBERN_ENTRYPOINT).to_string()
}

#[derive(Default)]
pub struct ClaudeDriver {
    catalog: Arc<ModelCatalog>,
}

impl ClaudeDriver {
    /// A driver that keeps Claude Code's model catalog in `file` across
    /// daemon restarts.
    pub fn with_catalog_file(file: PathBuf) -> Self {
        Self { catalog: Arc::new(ModelCatalog::persistent(file)) }
    }
}

#[async_trait]
impl AgentDriver for ClaudeDriver {
    fn kind(&self) -> ProviderKind {
        ProviderKind::ClaudeCode
    }

    async fn probe(&self, binary: Option<&PathBuf>) -> ProviderStatus {
        self.probe_context(&ProbeContext { binary: binary.cloned(), ..ProbeContext::default() }).await
    }

    async fn probe_with_context(&self, context: &ProbeContext) -> ProviderStatus {
        self.probe_context(context).await
    }
    async fn probe_fresh_with_context(&self, context: &ProbeContext) -> ProviderStatus {
        if let Ok(binary) = resolve(ProviderKind::ClaudeCode, context.binary.as_ref()) {
            // Refresh this native config/account only; normal probes keep using
            // the bounded, coalesced cache and preserve previous-good fallback.
            let _ = self.catalog.refresh(&binary, context).await;
        }
        self.probe_context(context).await
    }

    async fn one_shot(
        &self,
        cwd: &std::path::Path,
        prompt: &str,
        binary: Option<&PathBuf>,
        env: &std::collections::BTreeMap<String, String>,
    ) -> Result<String> {
        let bin = resolve(ProviderKind::ClaudeCode, binary)?;
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            crate::process_tree::output(
                crate::binary::command(&bin)
                    .no_window()
                    .current_dir(cwd)
                    .args([
                        "-p",
                        "--output-format",
                        "text",
                        "--model",
                        "haiku",
                        "--max-turns",
                        "1",
                        "--no-session-persistence",
                        "--permission-mode",
                        "dontAsk",
                        "--disallowedTools",
                        "*",
                        // `--disallowedTools` is variadic: without the
                        // terminator it swallows the prompt as deny rules.
                        "--",
                    ])
                    .arg(prompt)
                    .envs(env)
                    .env(ENTRYPOINT_ENV, entrypoint(env.get(ENTRYPOINT_ENV)))
                    .env_remove("NODE_OPTIONS")
                    .stdin(std::process::Stdio::null()),
            ),
        )
        .await
        .map_err(|_| DriverError::Protocol("claude one-shot timed out".into()))??;
        if !out.status.success() {
            return Err(DriverError::Protocol(format!(
                "claude exited with {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    async fn list_sessions(
        &self,
        context: &ProbeContext,
        cursor: Option<&str>,
        query: &str,
    ) -> Result<kybern_protocol::methods::SessionsListResult> {
        crate::sessions::list(self.kind(), context, cursor, query).await
    }

    async fn read_session(&self, context: &ProbeContext, id: &str) -> Result<crate::sessions::SessionHistory> {
        crate::sessions::read(self.kind(), context, id).await
    }

    async fn open_session_reader(&self, context: &ProbeContext) -> Result<Box<dyn crate::sessions::SessionReader>> {
        crate::sessions::reader(self.kind(), context).await
    }

    async fn spawn(&self, config: SessionConfig) -> Result<SpawnedSession> {
        let bin = resolve(ProviderKind::ClaudeCode, config.binary.as_ref())?;
        let native_mcp_config = config.native_tool_bridge.as_ref().map(write_claude_mcp_config).transpose()?;
        let session_id = match (&config.resume_session_id, config.fork) {
            (Some(id), false) => id.clone(),
            _ => Uuid::new_v4().to_string(),
        };

        let mut cmd = crate::binary::command(&bin).no_window();
        cmd.current_dir(&config.cwd)
            .args(["--output-format", "stream-json", "--input-format", "stream-json", "--verbose", "--include-partial-messages"])
            .args(["--permission-prompt-tool", "stdio"]);
        match (&config.resume_session_id, config.fork) {
            (Some(id), false) => {
                cmd.arg(format!("--resume={id}"));
            }
            (Some(id), true) => {
                cmd.arg(format!("--resume={id}")).arg("--fork-session").arg(format!("--session-id={session_id}"));
                // Rewind: keep the transcript through the last assistant entry of the turn before the cut.
                if let Some(end) = config.rewind.as_ref().and_then(|r| r.keep_through.as_ref()).and_then(|a| a.previous_end.clone()) {
                    cmd.arg(format!("--resume-session-at={end}"));
                }
            }
            (None, _) => {
                cmd.arg(format!("--session-id={session_id}"));
            }
        }
        cmd.args(["--permission-mode", mode_arg(config.permission_mode)]);
        if let Some(native_mcp_config) = &native_mcp_config {
            cmd.args(["--mcp-config", native_mcp_config.path.to_string_lossy().as_ref()]);
        }
        if let Some(bridge) = config.native_tool_bridge.as_ref() {
            if let Some(instructions) = bridge.provider_instructions() {
                cmd.args(["--append-system-prompt", instructions.as_str()]);
            }
            for tool in &bridge.restrictions.denied_tools {
                let provider_name = claude_tool_name(bridge, tool);
                cmd.args(["--disallowedTools", &provider_name]);
            }
            // Kybern asks per-app consent for computer use, and approves note
            // and task writes, itself, so a second harness prompt for the same
            // call would only add noise. Note and task reads are harmless.
            for tool in bridge.tools().filter(|tool| {
                tool.name.starts_with("kybern_html_")
                    || tool.name.starts_with("kybern_computer_")
                    || tool.name.starts_with("kybern_note")
                    || tool.name.starts_with("kybern_task")
            }) {
                cmd.args(["--allowedTools", &claude_tool_name(bridge, &tool.name)]);
            }
            if bridge.restrictions.require_enforcement {
                // Claude's `--tools` set is the native built-in catalog. An
                // empty set leaves the session MCP server available while
                // preventing shell, file, web, and other built-in tools from
                // bypassing the coordinator policy. A lead keeps only the
                // read-only ones. Strict MCP config also prevents unrelated
                // project/user servers from widening it.
                let builtins = if bridge.restrictions.allow_read_only_builtins { CLAUDE_READ_ONLY_TOOLS } else { "" };
                cmd.args(["--tools", builtins, "--strict-mcp-config"]);
            }
        }
        if config.permission_mode == PermissionMode::FullAccess {
            cmd.arg("--allow-dangerously-skip-permissions");
        }
        if let Some(model) = &config.model {
            cmd.args(["--model", model]);
        }
        if let Some(effort) = &config.effort {
            cmd.args(["--effort", effort]);
        }
        cmd.env_remove("NODE_OPTIONS");
        // stream-json makes Claude Code an `sdk-cli` session, where the
        // Artifact tools are off unless explicitly enabled. Session env may
        // still turn them off.
        cmd.env("CLAUDE_CODE_ARTIFACT", "1");
        cmd.env("CLAUDE_CODE_ARTIFACT_AUTO_OPEN", "0");
        for (k, v) in config.env.iter().filter(|(k, _)| k.as_str() != crate::login_lend::BORROW_ENV) {
            cmd.env(k, v);
        }
        cmd.env(ENTRYPOINT_ENV, entrypoint(config.env.get(ENTRYPOINT_ENV)));
        // A login another machine lent (design §9.4): the access token rides in this process' environment
        // only. A named account blanks `CLAUDE_CODE_OAUTH_TOKEN`, so this comes after the loop.
        let lent = crate::login_lend::source_of(&config.env);
        let hotswap = lent.is_some() && config.env.get(crate::login_lend::CLAUDE_HOTSWAP_ENV).is_some_and(|value| value == "1");
        if let Some(source) = &lent {
            let login = source.current().await.map_err(DriverError::Protocol)?;
            let env = crate::login_lend::claude_env(&login)
                .ok_or_else(|| DriverError::Protocol("The broker lent a login Claude Code cannot use.".into()))?;
            for (k, v) in env {
                cmd.env(k, v);
            }
            if hotswap {
                // Optional and undocumented: Claude asks the host for a new token on a 401 instead of failing the turn.
                cmd.env("CLAUDE_CODE_SDK_HAS_OAUTH_REFRESH", "1").env("CLAUDE_CODE_ENTRYPOINT", "claude-vscode");
            }
        }
        tracing::info!(bin = %bin.display(), cwd = %config.cwd.display(), session_id, "spawning claude");

        // Claude Code lists the session's own `--model` in its catalog in place
        // of the configured model, so only a session started without one
        // reports the list a probe sees.
        let catalog = config
            .model
            .is_none()
            .then(|| {
                let context = ProbeContext {
                    binary: config.binary.clone(),
                    cwd: Some(config.cwd.clone()),
                    env: config.env.clone().into_iter().collect(),
                };
                CatalogKey::new(&bin, &context)
            })
            .flatten()
            .map(|key| (self.catalog.clone(), key));

        let child = Arc::new(NdjsonChild::spawn(cmd)?);
        let mut lifetime = crate::ndjson::SessionLifetime::new(child.clone());
        let (tx, rx) = mpsc::channel(1024);
        let session = Arc::new(ClaudeSession {
            child: child.clone(),
            events: tx,
            pending_control: Mutex::new(HashMap::new()),
            pending_permissions: Mutex::new(HashMap::new()),
            state: Mutex::new(TurnState::default()),
            child_inbox: Mutex::new(ChildInbox::default()),
            session_id: Mutex::new(session_id.clone()),
            catalog,
            _native_mcp_config: native_mcp_config,
        });

        if lent.is_some() {
            let mut state = session.state.lock().await;
            state.lent_login = true;
            if hotswap {
                state.login_source = lent.clone();
            }
        }
        // Initialize is optional; we send it for the command and model catalogs and to be a well-behaved client.
        session.send_control_nowait("initialize", json!({ "supportedDialogKinds": ["resume_return"], "hooks": {"PreToolUse": [{"matcher": null, "hookCallbackIds": [CHILD_MESSAGE_HOOK]}]} })).await?;

        let reader = session.clone();
        lifetime.track(tokio::spawn(async move { reader.read_loop().await }));

        Ok(SpawnedSession { session: Box::new(SessionHandle(session, lifetime)), events: rx })
    }
}

/// Built-in tools a project Coordinator keeps: reading, searching and web lookups,
/// plus asking the user a question and running a skill. None of them edits a file.
const CLAUDE_READ_ONLY_TOOLS: &str = "Read,Grep,Glob,WebSearch,WebFetch,AskUserQuestion,Skill";

impl ClaudeDriver {
    async fn probe_context(&self, context: &ProbeContext) -> ProviderStatus {
        let mut status = ProviderStatus {
            kind: ProviderKind::ClaudeCode,
            display_name: ProviderKind::ClaudeCode.display_name().into(),
            available: false,
            binary_path: None,
            version: None,
            unavailable_reason: None,
            supported_permission_modes: PermissionMode::ALL.to_vec(),
            supports_fork: true,
            supports_model_switch: true,
            supports_effort_switch: false,
            supported_efforts: vec!["low".into(), "medium".into(), "high".into(), "xhigh".into(), "max".into()],
            models: vec![
                ProviderModel {
                    id: "sonnet".into(),
                    display_name: "Sonnet".into(),
                    resolved_id: None,
                    description: None,
                    provider: None,
                    efforts: vec!["low".into(), "medium".into(), "high".into(), "xhigh".into(), "max".into()],
                    default_effort: None,
                    is_default: false,
                    parameters: Vec::new(),
                    variants: Vec::new(),
                },
                ProviderModel {
                    id: "opus".into(),
                    display_name: "Opus".into(),
                    resolved_id: None,
                    description: None,
                    provider: None,
                    efforts: vec!["low".into(), "medium".into(), "high".into(), "xhigh".into(), "max".into()],
                    default_effort: None,
                    is_default: false,
                    parameters: Vec::new(),
                    variants: Vec::new(),
                },
            ],
            instances: vec!["default".into()],
        };
        let bin = match resolve(ProviderKind::ClaudeCode, context.binary.as_ref()) {
            Ok(b) => b,
            Err(e) => {
                status.unavailable_reason = Some(format!("{e}. Install with: npm install -g @anthropic-ai/claude-code"));
                return status;
            }
        };
        status.binary_path = Some(bin.display().to_string());
        match version_of(&bin, &["--version"]).await {
            Some(v) => {
                let ok = at_least(&v, MIN_VERSION);
                status.available = ok;
                if !ok {
                    status.unavailable_reason =
                        Some(format!("Claude Code {v} is older than the required {}.{}.{}", MIN_VERSION.0, MIN_VERSION.1, MIN_VERSION.2));
                }
                status.version = Some(v);
            }
            None => status.unavailable_reason = Some("could not run `claude --version`".into()),
        }
        if status.available {
            match self.catalog.models(&bin, context).await {
                Some(catalog) => {
                    let config = crate::claude_config::resolve(context, &bin, catalog_default(&catalog)).await;
                    let efforts = catalog_efforts(&catalog);
                    if !efforts.is_empty() {
                        status.supported_efforts = efforts;
                    }
                    status.models = catalog_models(&catalog, &config, &status.supported_efforts);
                    insert_configured_model(&mut status, &config, &catalog);
                }
                None => {
                    let config = crate::claude_config::resolve(context, &bin, None).await;
                    self.probe_help(&mut status, &bin, context, &config).await;
                    insert_configured_model(&mut status, &config, &[]);
                }
            }
        }
        status
    }

    /// Fallback when Claude Code reports no catalog (an old CLI, or a first
    /// read still running): aliases and efforts from `--help`, unversioned.
    async fn probe_help(&self, status: &mut ProviderStatus, bin: &std::path::Path, context: &ProbeContext, config: &ClaudeConfig) {
        let Ok(Ok(output)) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            crate::process_tree::output(contextual_command(bin, context).arg("--help")),
        )
        .await
        else {
            return;
        };
        let help = String::from_utf8_lossy(&output.stdout);
        let efforts = claude_efforts(&help);
        if !efforts.is_empty() {
            status.supported_efforts = efforts;
        }
        let aliases = claude_model_aliases(&help);
        if aliases.is_empty() {
            return;
        }
        status.models = aliases
            .into_iter()
            .map(|id| {
                let target = (id == config.model).then_some(config.alias_target.as_deref()).flatten();
                ProviderModel {
                    display_name: claude_model_name(&id, target),
                    resolved_id: target.map(|target| resolved_model_id(&id, target)),
                    description: None,
                    is_default: id == config.model,
                    parameters: Vec::new(),
                    variants: Vec::new(),
                    default_effort: Some(config.effort_for(&id)),
                    efforts: status.supported_efforts.clone(),
                    provider: None,
                    id,
                }
            })
            .collect();
    }
}

/// The selector the catalog's `default` entry stands for: the listed entry
/// that runs the same model (`opus[1m]`), else `default` itself.
fn catalog_default(catalog: &[CatalogModel]) -> Option<String> {
    let default = catalog.iter().find(|model| model.value == "default")?;
    Some(default_twin(catalog, default).map_or_else(|| default.value.clone(), |twin| twin.value.clone()))
}

/// Another entry that runs the same model as the `default` entry. Older CLIs
/// omit `resolvedModel`, but give both entries the same description.
fn default_twin<'a>(catalog: &'a [CatalogModel], default: &CatalogModel) -> Option<&'a CatalogModel> {
    catalog.iter().filter(|model| model.value != default.value).find(|model| match (&model.resolved_model, &default.resolved_model) {
        (Some(model), Some(default)) => model == default,
        _ => model.description.is_some() && model.description == default.description,
    })
}

/// Every effort any model accepts, in Claude Code's order.
fn catalog_efforts(catalog: &[CatalogModel]) -> Vec<String> {
    let mut efforts: Vec<String> = Vec::new();
    for effort in catalog.iter().flat_map(|model| &model.efforts) {
        if !efforts.contains(effort) {
            efforts.push(effort.clone());
        }
    }
    efforts
}

fn catalog_models(catalog: &[CatalogModel], config: &ClaudeConfig, fallback_efforts: &[String]) -> Vec<ProviderModel> {
    catalog
        .iter()
        // Kybern offers "Agent default" itself; list the model it runs instead.
        .filter(|model| model.value != "default" || default_twin(catalog, model).is_none())
        .map(|model| {
            let efforts = match (model.supports_effort, model.efforts.is_empty()) {
                (false, _) => Vec::new(),
                (true, true) => fallback_efforts.to_vec(),
                (true, false) => model.efforts.clone(),
            };
            ProviderModel {
                id: model.value.clone(),
                display_name: match &model.resolved_model {
                    Some(target) => claude_model_name(&model.value, Some(target)),
                    None if model.value == "default" => model.display_name.clone(),
                    None => claude_model_name(&model.value, None),
                },
                resolved_id: model.resolved_model.clone(),
                description: model.description.clone(),
                provider: None,
                default_effort: (!efforts.is_empty()).then(|| config.effort_for(&model.value)),
                efforts,
                is_default: model.value == config.model,
                parameters: Vec::new(),
                variants: Vec::new(),
            }
        })
        .collect()
}

/// Claude Code runs the configured model even when its catalog omits it (a
/// project `model` setting, `ANTHROPIC_MODEL`), so list it as the default.
fn insert_configured_model(status: &mut ProviderStatus, config: &ClaudeConfig, catalog: &[CatalogModel]) {
    if status.models.iter().any(|model| model.is_default) {
        return;
    }
    let base = config.model.split('[').next().unwrap_or(&config.model);
    let target = catalog
        .iter()
        .find(|model| model.value.split('[').next() == Some(base))
        .and_then(|model| model.resolved_model.as_deref())
        .or(config.alias_target.as_deref());
    status.models.insert(
        0,
        ProviderModel {
            id: config.model.clone(),
            display_name: claude_model_name(&config.model, target),
            resolved_id: target.map(|target| resolved_model_id(&config.model, target)),
            description: None,
            provider: None,
            efforts: status.supported_efforts.clone(),
            default_effort: Some(config.effort.clone()),
            is_default: true,
            parameters: Vec::new(),
            variants: Vec::new(),
        },
    );
}

struct NativeMcpConfig {
    path: PathBuf,
}

impl Drop for NativeMcpConfig {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// How long Kybern's MCP server may stay silent during one call (6 h, as `MCP_TOOL_TIMEOUT`).
const KYBERN_MCP_SILENT_MS: u64 = 6 * 60 * 60 * 1000;

fn write_claude_mcp_config(bridge: &NativeToolBridge) -> Result<NativeMcpConfig> {
    bridge.validate()?;
    let endpoint = bridge
        .endpoint
        .as_deref()
        .ok_or_else(|| DriverError::Unsupported("Claude Code Kybern tools require a daemon MCP endpoint".into()))?;
    let authorization = bridge
        .authorization
        .as_deref()
        .ok_or_else(|| DriverError::Unsupported("Claude Code Kybern tools require a scoped MCP authorization capability".into()))?;
    let config = json!({
        "mcpServers": {
            bridge.server_name.clone(): {
                "type": "http",
                "url": endpoint,
                "headers": { "Authorization": format!("Bearer {authorization}") },
                // Claude Code aborts a call that sends nothing for 5 min, which ends a waiting visual, sign-in, permission
                // or question while the user is still deciding. The daemon bounds every call itself (at most a few hours).
                "timeout": KYBERN_MCP_SILENT_MS,
            }
        }
    });
    let path = std::env::temp_dir().join(format!("kybern-claude-mcp-{}.json", Uuid::new_v4().simple()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    let bytes = serde_json::to_vec(&config).map_err(|error| DriverError::Other(error.into()))?;
    if let Err(error) = file.write_all(&bytes) {
        let _ = fs::remove_file(&path);
        return Err(error.into());
    }
    Ok(NativeMcpConfig { path })
}

fn claude_tool_name(bridge: &NativeToolBridge, name: &str) -> String {
    if name.starts_with("mcp__") || !bridge.tools.iter().any(|tool| tool.name == name) {
        name.to_string()
    } else {
        format!("mcp__{}__{name}", bridge.server_name)
    }
}

fn mode_arg(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Supervised => "default",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::Auto => "auto",
        PermissionMode::FullAccess => "bypassPermissions",
    }
}

fn option_excerpt<'a>(help: &'a str, flag: &str, max_len: usize) -> Option<&'a str> {
    let start = help.find(flag)?;
    let tail = &help[start..];
    let end = tail.char_indices().nth(max_len).map(|(index, _)| index).unwrap_or(tail.len());
    Some(&tail[..end])
}

fn claude_model_aliases(help: &str) -> Vec<String> {
    let Some(excerpt) = option_excerpt(help, "--model <model>", 420) else { return Vec::new() };
    let alias_text = excerpt.split("or a model's full name").next().unwrap_or(excerpt);
    let mut aliases = Vec::new();
    let mut rest = alias_text;
    while let Some(start) = rest.find('\'') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('\'') else { break };
        let value = &rest[..end];
        if !value.is_empty() && value.chars().all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '_') {
            aliases.push(value.to_string());
        }
        rest = &rest[end + 1..];
    }
    aliases
}

fn claude_efforts(help: &str) -> Vec<String> {
    let Some(excerpt) = option_excerpt(help, "--effort <level>", 260) else { return Vec::new() };
    let Some(start) = excerpt.find('(') else { return Vec::new() };
    let Some(end) = excerpt[start + 1..].find(')') else { return Vec::new() };
    excerpt[start + 1..start + 1 + end]
        .split(',')
        .map(|value| value.trim().trim_start_matches("or "))
        .filter(|value| !value.is_empty() && value.chars().all(|character| character.is_ascii_alphanumeric() || character == '-'))
        .map(str::to_string)
        .collect()
}

pub(crate) fn contextual_command(binary: &std::path::Path, context: &ProbeContext) -> Command {
    let mut command = crate::binary::command(binary).no_window();
    if let Some(cwd) = context.cwd.as_deref() {
        command.current_dir(cwd);
    }
    command.env_remove("NODE_OPTIONS").envs(&context.env).env(ENTRYPOINT_ENV, entrypoint(context.env.get(ENTRYPOINT_ENV)));
    command
}

/// Read the account's plan limits and plan name. Anthropic's OAuth usage
/// endpoint answers in one request; when no usable login is stored, fall back
/// to Claude Code's `/usage` command. Throttling or a login due for refresh
/// returns `None`, and the caller keeps its last values.
/// Why Claude's plan limits were not read: the reason for clients, the detail for the log.
#[derive(Debug, Clone)]
pub struct UsageUnread {
    pub reason: kybern_protocol::methods::LimitsStale,
    /// While throttled: when reads may run again.
    pub retry_at: Option<chrono::DateTime<chrono::Utc>>,
    pub detail: String,
}

pub async fn read_account_usage(
    cwd: &std::path::Path,
    binary: Option<&PathBuf>,
    env: &std::collections::BTreeMap<String, String>,
) -> std::result::Result<(Vec<kybern_protocol::UsageLimit>, Option<String>), UsageUnread> {
    let context = ProbeContext { binary: resolve(ProviderKind::ClaudeCode, binary).ok(), cwd: Some(cwd.to_path_buf()), env: env.clone() };
    match crate::claude_usage::read(&context).await {
        crate::claude_usage::OauthUsage::Read { limits, plan } => Ok((limits, plan)),
        crate::claude_usage::OauthUsage::Wait(unread) => Err(unread),
        crate::claude_usage::OauthUsage::Unavailable(detail) => {
            // `/usage` also skips a login that is due for a refresh; say so instead of "unavailable".
            if crate::claude_config::login_needs_refresh(&context).await {
                return Err(UsageUnread {
                    reason: kybern_protocol::methods::LimitsStale::LoginRefresh,
                    retry_at: None,
                    detail: format!("{detail}; the Claude Code login is due for a refresh"),
                });
            }
            read_account_limits(cwd, binary, env).await.map(|limits| (limits, None)).ok_or_else(|| UsageUnread {
                reason: kybern_protocol::methods::LimitsStale::Unavailable,
                retry_at: None,
                detail: format!("{detail}; Claude Code's /usage gave no limits either"),
            })
        }
    }
}

/// Read the account's current plan limits by driving Claude Code's local
/// `/usage` command over stream-json. It runs no model turn (num_turns 0, zero
/// cost), so the Usage page can show live limits without the user prompting.
/// While the login is due for a refresh this returns `None` and the caller
/// keeps the last stored limits: the next session rotates the tokens, not a
/// probe opened by a page view.
pub async fn read_account_limits(
    cwd: &std::path::Path,
    binary: Option<&PathBuf>,
    env: &std::collections::BTreeMap<String, String>,
) -> Option<Vec<kybern_protocol::UsageLimit>> {
    let bin = resolve(ProviderKind::ClaudeCode, binary).ok()?;
    let context = ProbeContext { binary: Some(bin.clone()), cwd: Some(cwd.to_path_buf()), env: env.clone() };
    if crate::claude_config::login_needs_refresh(&context).await {
        tracing::debug!("skipped the live Claude Code usage read while its login is due for a refresh");
        return None;
    }
    let mut cmd = contextual_command(&bin, &context);
    cmd.args(["-p", "--output-format", "stream-json", "--input-format", "stream-json", "--verbose", "--no-session-persistence"]);
    let child = NdjsonChild::spawn(cmd).ok()?;
    child.write(&json!({ "type": "user", "message": { "role": "user", "content": "/usage" } })).await.ok()?;
    let _ = child.close_stdin().await;
    let text = tokio::time::timeout(std::time::Duration::from_secs(12), async {
        loop {
            let frame = {
                let mut lines = child.lines.lock().await;
                lines.recv().await?
            };
            if frame.get("type").and_then(Value::as_str) == Some("result") {
                return frame.get("result").and_then(Value::as_str).map(str::to_string);
            }
        }
    })
    .await
    .ok()
    .flatten();
    // Input is already closed, so Claude Code exits once it has saved any
    // login it refreshed. See `claude_catalog::read_catalog`.
    tokio::spawn(async move { child.close().await });
    let limits = parse_usage_limits(&text?);
    (!limits.is_empty()).then_some(limits)
}

/// Parse the human-readable `/usage` output into structured limits. Lines look
/// like `Current session: 7% used · resets Sep 16 at 7:30pm (Asia/Makassar)`.
fn parse_usage_limits(text: &str) -> Vec<kybern_protocol::UsageLimit> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some((label, rest)) = line.split_once(':') else { continue };
        if !rest.contains("% used") {
            continue;
        }
        let Some(percent) = rest.split('%').next().and_then(|value| value.trim().parse::<f64>().ok()) else {
            continue;
        };
        let (name, window_minutes) = classify_usage_limit(label.trim());
        let resets_at = rest.split("resets").nth(1).and_then(parse_reset_local);
        out.push(kybern_protocol::UsageLimit { name, used_percent: percent, window_minutes, resets_at });
    }
    out
}

/// Map a `/usage` row label to a stable name + window so it lines up with the
/// desktop labels and Codex's window-based limits.
fn classify_usage_limit(label: &str) -> (String, Option<u64>) {
    if label.to_lowercase().contains("session") {
        ("Current session".into(), Some(300))
    } else if label.eq_ignore_ascii_case("Current week (all models)") {
        ("This week".into(), Some(10080))
    } else if let Some(model) = label.strip_prefix("Current week (").and_then(|inner| inner.strip_suffix(')')) {
        (format!("{model} this week"), None)
    } else {
        (label.to_string(), None)
    }
}

/// Parse `Sep 16 at 7:30pm (Asia/Makassar)` as a local time. The daemon shares
/// the user's timezone, so interpret the naive datetime in `Local` and ignore
/// the parenthetical tz name. Assume the current year, rolling forward a year if
/// the date already passed (resets are always in the future).
fn parse_reset_local(text: &str) -> Option<i64> {
    use chrono::{Datelike, Local, NaiveDate, TimeZone};
    let cleaned = text.split('(').next().unwrap_or(text).trim().replace(" at ", " ");
    let mut parts = cleaned.split_whitespace();
    let month = month_number(parts.next()?)?;
    let day: u32 = parts.next()?.parse().ok()?;
    let (hour, minute) = parse_clock(parts.next()?)?;
    let now = Local::now();
    for year in [now.year(), now.year() + 1] {
        let naive = NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, minute, 0)?;
        if let chrono::LocalResult::Single(dt) = Local.from_local_datetime(&naive)
            && dt.timestamp() + 86_400 >= now.timestamp()
        {
            return Some(dt.timestamp());
        }
    }
    None
}

fn month_number(abbr: &str) -> Option<u32> {
    Some(match abbr.to_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    })
}

fn parse_clock(text: &str) -> Option<(u32, u32)> {
    let lower = text.to_lowercase();
    let pm = lower.ends_with("pm");
    let body = lower.trim_end_matches("pm").trim_end_matches("am");
    let (hour12, minute) = match body.split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => (body.parse::<u32>().ok()?, 0),
    };
    let hour = match (hour12, pm) {
        (12, false) => 0,
        (12, true) => 12,
        (h, true) => h + 12,
        (h, false) => h,
    };
    (hour < 24 && minute < 60).then_some((hour, minute))
}

/// Human label for a Claude selector. The selector itself stays exactly what
/// Claude Code accepts; the name is derived mechanically so new models format
/// themselves. `target` is the concrete id the selector runs as, from Claude
/// Code's catalog or an `ANTHROPIC_DEFAULT_*_MODEL` override, and carries the
/// real version. Without one, a bare alias (`opus`) shows unversioned rather
/// than with a version we would have to guess.
fn claude_model_name(selector: &str, target: Option<&str>) -> String {
    let context_suffix = |id: &str| id.split_once('[').map(|(_, suffix)| suffix.trim_end_matches(']').to_string());
    let base = selector.split('[').next().unwrap_or(selector);
    let label = crate::models::prettify_model_id(target.unwrap_or(base));
    // `default` resolves to `claude-opus-5-5[1m]`: the context size can live
    // on either side.
    match context_suffix(selector).or_else(|| target.and_then(context_suffix)) {
        Some(suffix) if suffix.eq_ignore_ascii_case("1m") => format!("{label} · 1M"),
        Some(suffix) if !suffix.is_empty() => format!("{label} · {suffix}"),
        _ => label,
    }
}

/// The concrete id a selector runs as: the alias's resolved target, keeping a
/// context suffix such as `[1m]` from the selector (`opus[1m]` →
/// `claude-opus-5-5[1m]`). A selector that is already concrete stays itself.
fn resolved_model_id(selector: &str, target: &str) -> String {
    let suffix = selector.find('[').map_or("", |index| &selector[index..]);
    if suffix.is_empty() || target.contains('[') { target.to_string() } else { format!("{target}{suffix}") }
}

#[derive(Default)]
struct TurnState {
    /// The lent login to answer `oauth_token_refresh` from, only when the optional hot swap is on.
    login_source: Option<Arc<dyn crate::login_lend::LoginSource>>,
    /// This process runs on a login another machine lent, so a turn that ends on a rejected token is a
    /// failure the daemon can recover from (it borrows again, resumes and sends the message once more).
    lent_login: bool,
    last_context: Option<(String, u64)>,
    /// Accumulated final text per API message id, so multi-block messages coalesce.
    text: HashMap<String, String>,
    thinking: HashMap<String, String>,
    /// Message id from the last `message_start`, used to attribute stream deltas.
    current_message: Option<String>,
    /// Content block index of the thinking block currently streaming.
    thinking_block: Option<u64>,
    /// The same two cursors for each subagent, keyed by the tool call that
    /// launched it. Subagents run concurrently, so they cannot share the root's.
    child_streams: HashMap<String, (Option<String>, Option<u64>)>,
    /// `subagent_type` of recent Agent/Task launch calls, oldest first. Claude Code
    /// 2.1.290 omits it from `task_started`, so the role is recovered from the launching
    /// call's input by tool call id.
    agent_roles: VecDeque<(String, String)>,
    last_total_cost: f64,
    last_bound: Option<(String, Option<String>)>,
    /// uuid of the most recent assistant frame this turn (the rewind anchor).
    last_assistant_uuid: Option<String>,
    /// Our uuid for the current turn's user message.
    current_user_uuid: Option<String>,
    /// An automatic notification can finish before a newly sent user message
    /// is consumed. Carry its accounting until that explicit request finishes.
    queued_usage: Usage,
    queued_cost: f64,
    queued_duration_ms: u64,
}

/// Launch calls remembered for their role; older ones are forgotten first.
const MAX_AGENT_ROLES: usize = 256;

impl TurnState {
    fn remember_agent_role(&mut self, tool_call_id: &str, role: &str) {
        self.agent_roles.retain(|(id, _)| id != tool_call_id);
        if self.agent_roles.len() >= MAX_AGENT_ROLES {
            self.agent_roles.pop_front();
        }
        self.agent_roles.push_back((tool_call_id.to_string(), role.to_string()));
    }

    /// The message and thinking-block cursors of the root stream (`None`) or of one subagent.
    fn stream_cursor(&mut self, parent: Option<&str>) -> (&mut Option<String>, &mut Option<u64>) {
        match parent {
            None => (&mut self.current_message, &mut self.thinking_block),
            Some(parent) => {
                let cursor = self.child_streams.entry(parent.to_string()).or_default();
                (&mut cursor.0, &mut cursor.1)
            }
        }
    }
}

const CHILD_MESSAGE_HOOK: &str = "kybern_child_message";

#[derive(Default)]
struct ChildInbox {
    active: std::collections::HashSet<String>,
    pending: HashMap<String, Vec<(String, String)>>,
    /// Session-local dedup survives HTTP retries; durable dedup belongs to the daemon.
    accepted: HashMap<String, (String, u64)>,
}

impl ChildInbox {
    fn queue(&mut self, task_id: &str, id: &str, text: String) -> Result<()> {
        use std::hash::{Hash, Hasher};
        let mut digest = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut digest);
        let fingerprint = digest.finish();
        if let Some((owner, original)) = self.accepted.get(id) {
            return if owner == task_id && *original == fingerprint {
                Ok(())
            } else {
                Err(DriverError::Protocol("Message identity already belongs to another input.".into()))
            };
        }
        if !self.active.contains(task_id) {
            return Err(DriverError::Protocol("Not delivered — subagent finished.".into()));
        }
        if self.pending.values().map(Vec::len).sum::<usize>() >= 100 {
            return Err(DriverError::Protocol("Too many pending subagent messages. Wait for delivery.".into()));
        }
        if self.accepted.len() >= 4096 {
            return Err(DriverError::Protocol("Native child message history is full. Start a new parent session.".into()));
        }
        self.accepted.insert(id.into(), (task_id.into(), fingerprint));
        self.pending.entry(task_id.into()).or_default().push((id.into(), text));
        Ok(())
    }

    fn finish(&mut self, task_id: &str) {
        self.active.remove(task_id);
        self.pending.remove(task_id);
    }
}

/// Hooks are text context, not a multimodal message. Keep stored attachments intact
/// and supply only honest references that the child can read with its file tools.
fn child_message_context(message: &UserMessage) -> Result<String> {
    let mut parts = Vec::new();
    for part in &message.parts {
        match part {
            ContentPart::Image { .. } => {
                return Err(DriverError::Unsupported(
                    "Attach an image file so the subagent can read its path. Inline images cannot be delivered through a native hook."
                        .into(),
                ));
            }
            ContentPart::Attachment { .. } => {
                return Err(DriverError::Protocol("The attachment must be resolved to a readable file reference.".into()));
            }
            _ => parts.push(UserMessage { parts: vec![part.clone()] }.plain_text()),
        }
    }
    let text = parts.join("\n");
    if text.trim().is_empty() {
        return Err(DriverError::Protocol("Write a message or attach a file.".into()));
    }
    if text.len() > 64 * 1024 {
        return Err(DriverError::Protocol("Subagent message exceeds 64 KiB.".into()));
    }
    Ok(text)
}

#[cfg(test)]
mod child_message_tests {
    use super::*;

    #[tokio::test]
    async fn native_child_callback_routes_once_and_never_drains_root_or_another_child() {
        let child = Arc::new(NdjsonChild::spawn(crate::test_process::echo()).unwrap());
        let (events, mut rx) = mpsc::channel(32);
        let session = Arc::new(ClaudeSession {
            child: child.clone(),
            events,
            pending_control: Mutex::new(HashMap::new()),
            pending_permissions: Mutex::new(HashMap::new()),
            state: Mutex::new(TurnState::default()),
            child_inbox: Mutex::new(ChildInbox::default()),
            session_id: Mutex::new("native-session".into()),
            catalog: None,
            _native_mcp_config: None,
        });
        let handle = SessionHandle(session.clone(), crate::ndjson::SessionLifetime::new(child.clone()));
        session.handle_frame(json!({"type":"system","subtype":"task_started","task_id":"child-a","tool_use_id":"launch-a"})).await;
        while rx.try_recv().is_ok() {}
        handle.send_subagent_message("child-a", "message-a", &UserMessage::text("Read the whole file.")).await.unwrap();
        handle.send_subagent_message("child-a", "message-a", &UserMessage::text("Read the whole file.")).await.unwrap();
        assert!(handle.send_subagent_message("child-a", "message-a", &UserMessage::text("Different input")).await.is_err());
        // Queueing does not write any user frame to the parent's stdin.
        assert!(child.lines.lock().await.try_recv().is_err());
        for agent in [None, Some("child-b"), Some("child-a"), Some("child-a")] {
            session
                .handle_control_request(
                    &json!({"request_id":"hook-request", "request":{"subtype":"hook_callback", "callback_id":CHILD_MESSAGE_HOOK,
                "input":{"hook_event_name":"PreToolUse","agent_id":agent,"tool_name":"Read"}}}),
                )
                .await;
            let frame = tokio::time::timeout(std::time::Duration::from_secs(1), child.lines.lock().await.recv()).await.unwrap().unwrap();
            assert_eq!(frame["type"], "control_response");
            let context = frame.pointer("/response/response/hookSpecificOutput/additionalContext").and_then(Value::as_str);
            if agent == Some("child-a")
                && let Some(context) = context
            {
                assert!(context.contains("Read the whole file."));
                assert!(
                    matches!(rx.try_recv(), Ok(DriverEvent::SubagentMessageDelivered {task_id, message_id}) if task_id == "child-a" && message_id == "message-a")
                );
            } else {
                assert!(context.is_none());
                assert!(rx.try_recv().is_err());
            }
        }
        assert!(session.child_inbox.lock().await.pending.is_empty());
        session.handle_frame(json!({"type":"system","subtype":"task_notification","task_id":"child-a","status":"completed"})).await;
        assert!(handle.send_subagent_message("child-a", "late", &UserMessage::text("Too late")).await.is_err());
        child.close().await;
    }

    #[tokio::test]
    async fn failed_callback_write_keeps_pending_without_acknowledgment() {
        let child = Arc::new(NdjsonChild::spawn(crate::test_process::echo()).unwrap());
        let (events, mut rx) = mpsc::channel(4);
        let session = ClaudeSession {
            child: child.clone(),
            events,
            pending_control: Mutex::new(HashMap::new()),
            pending_permissions: Mutex::new(HashMap::new()),
            state: Mutex::new(TurnState::default()),
            child_inbox: Mutex::new(ChildInbox::default()),
            session_id: Mutex::new("test".into()),
            catalog: None,
            _native_mcp_config: None,
        };
        session.child_inbox.lock().await.active.insert("child".into());
        session.child_inbox.lock().await.queue("child", "message", "Pending".into()).unwrap();
        child.close_stdin().await.unwrap();
        session
            .deliver_child_hook(
                "request",
                &json!({"callback_id":CHILD_MESSAGE_HOOK,"input":{"hook_event_name":"PreToolUse","agent_id":"child"}}),
            )
            .await;
        assert_eq!(session.child_inbox.lock().await.pending["child"].len(), 1);
        assert!(rx.try_recv().is_err());
        child.close().await;
    }

    #[test]
    fn hook_messages_keep_all_text_and_file_references_and_refuse_inline_images() {
        let message = UserMessage {
            parts: vec![
                ContentPart::Text { text: "Line one\nLine two".into() },
                ContentPart::FileMention { path: "/tmp/report.txt".into() },
            ],
        };
        let context = child_message_context(&message).unwrap();
        assert!(context.contains("Line one\nLine two") && context.contains("/tmp/report.txt"));
        assert!(
            child_message_context(&UserMessage { parts: vec![ContentPart::Image { media_type: "image/png".into(), data: "raw".into() }] })
                .is_err()
        );
    }
}

struct ClaudeSession {
    child: Arc<NdjsonChild>,
    events: mpsc::Sender<DriverEvent>,
    pending_control: Mutex<HashMap<String, oneshot::Sender<std::result::Result<Value, String>>>>,
    /// request_id -> original tool input, echoed back as `updatedInput` on allow.
    pending_permissions: Mutex<HashMap<String, (Value, Vec<Value>)>>,
    state: Mutex<TurnState>,
    child_inbox: Mutex<ChildInbox>,
    session_id: Mutex<String>,
    /// Where this session's `initialize` catalog is recorded, keeping the
    /// probe's cached catalog fresh without a separate process.
    catalog: Option<(Arc<ModelCatalog>, CatalogKey)>,
    _native_mcp_config: Option<NativeMcpConfig>,
}

/// Thin wrapper so the daemon owns a `Box<dyn AgentSession>` while the reader task keeps an Arc.
struct SessionHandle(Arc<ClaudeSession>, #[allow(dead_code)] crate::ndjson::SessionLifetime);

impl ClaudeSession {
    async fn emit(&self, ev: DriverEvent) {
        let _ = self.events.send(ev).await;
    }

    async fn send_control_nowait(&self, subtype: &str, mut request: Value) -> Result<String> {
        let id = Uuid::new_v4().simple().to_string();
        request["subtype"] = Value::String(subtype.into());
        self.child.write(&json!({ "type": "control_request", "request_id": id, "request": request })).await?;
        Ok(id)
    }

    async fn send_control(&self, subtype: &str, request: Value) -> Result<Value> {
        let (tx, rx) = oneshot::channel();
        let id = Uuid::new_v4().simple().to_string();
        self.pending_control.lock().await.insert(id.clone(), tx);
        let mut request = request;
        request["subtype"] = Value::String(subtype.into());
        self.child.write(&json!({ "type": "control_request", "request_id": id, "request": request })).await?;
        match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(DriverError::Protocol(format!("{subtype}: {e}"))),
            Ok(Err(_)) => Err(DriverError::ProcessExited("claude exited while waiting for a control response".into())),
            Err(_) => {
                self.pending_control.lock().await.remove(&id);
                Err(DriverError::Protocol(format!("{subtype}: timed out")))
            }
        }
    }

    async fn read_loop(self: Arc<Self>) {
        loop {
            let line = {
                let mut rx = self.child.lines.lock().await;
                rx.recv().await
            };
            let Some(v) = line else { break };
            self.handle_frame(v).await;
        }
        let code = self.child.wait().await;
        let stderr_tail = {
            let mut rx = self.child.stderr.lock().await;
            let mut lines = Vec::new();
            while let Ok(l) = rx.try_recv() {
                lines.push(l);
            }
            lines.into_iter().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
        };
        for (_, tx) in self.pending_control.lock().await.drain() {
            let _ = tx.send(Err("process exited".into()));
        }
        let error = match code {
            Some(0) | None => None,
            Some(c) => Some(if stderr_tail.is_empty() { format!("exit code {c}") } else { stderr_tail }),
        };
        self.emit(DriverEvent::Exited { code, error }).await;
    }

    async fn handle_frame(&self, v: Value) {
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "control_response" => {
                let resp = &v["response"];
                if let Some(commands) = resp.pointer("/response/commands") {
                    self.emit(DriverEvent::CommandsUpdated(crate::provider_commands(commands))).await;
                }
                if let (Some((catalog, key)), Some(models)) =
                    (&self.catalog, resp.pointer("/response/models").and_then(crate::claude_catalog::parse_models))
                {
                    catalog.record(key.clone(), models);
                }
                let id = resp.get("request_id").and_then(|r| r.as_str()).unwrap_or("").to_string();
                if let Some(tx) = self.pending_control.lock().await.remove(&id) {
                    let out = if resp.get("subtype").and_then(|s| s.as_str()) == Some("success") {
                        Ok(resp.get("response").cloned().unwrap_or(Value::Null))
                    } else {
                        Err(resp.get("error").and_then(|e| e.as_str()).unwrap_or("unknown error").to_string())
                    };
                    let _ = tx.send(out);
                }
            }
            "control_request" => self.handle_control_request(&v).await,
            "control_cancel_request" => {
                let id = v.get("request_id").and_then(|r| r.as_str()).unwrap_or("").to_string();
                if self.pending_permissions.lock().await.remove(&id).is_some() {
                    self.emit(DriverEvent::PermissionWithdrawn { request_id: id }).await;
                }
            }
            "keep_alive" => {}
            "system" => self.handle_system(&v).await,
            "stream_event" => self.handle_stream_event(&v).await,
            "assistant" => self.handle_assistant(&v).await,
            "user" => self.handle_user(&v).await,
            "result" => self.handle_result(&v).await,
            "rate_limit_event" => {
                let info = &v["rate_limit_info"];
                if let (Some(kind), Some(utilization)) =
                    (info.get("rateLimitType").and_then(Value::as_str), info.get("utilization").and_then(Value::as_f64))
                {
                    // Same names as `/usage` (see `classify_usage_limit`), so a
                    // live read and an in-turn report fold into one row.
                    let (name, window_minutes) = match kind {
                        "five_hour" => ("Current session", Some(300)),
                        "seven_day" => ("This week", Some(10080)),
                        _ => (kind, None),
                    };
                    self.emit(DriverEvent::UsageUpdated(kybern_protocol::ProviderUsage {
                        context: None,
                        limits: Some(vec![kybern_protocol::UsageLimit {
                            name: name.into(),
                            used_percent: utilization * 100.0,
                            window_minutes,
                            resets_at: info.get("resetsAt").and_then(Value::as_i64),
                        }]),
                    }))
                    .await;
                }

                if info.get("status").and_then(|s| s.as_str()).is_some_and(|s| !s.starts_with("allowed") && s != "ok") {
                    self.emit(DriverEvent::Notice {
                        level: NoticeLevel::Warning,
                        text: format!("rate limit: {}", info.get("status").and_then(|s| s.as_str()).unwrap_or("")),
                        data: Some(info.clone()),
                    })
                    .await;
                }
            }
            _ => tracing::trace!(frame = %ty, "ignored claude frame"),
        }
    }

    async fn handle_control_request(&self, v: &Value) {
        let request_id = v.get("request_id").and_then(|r| r.as_str()).unwrap_or("").to_string();
        let req = &v["request"];
        let subtype = req.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
        match subtype {
            // The hot swap: a 401 with no refresh token of its own. Answer from a fresh borrow, and the same turn continues.
            "oauth_token_refresh" => {
                let source = self.state.lock().await.login_source.clone();
                let answer = match source {
                    Some(source) => match source.renew(crate::login_lend::RenewReason::Unauthorized).await {
                        Ok(login) => match crate::login_lend::claude_env(&login) {
                            Some(env) => Ok(json!({ "accessToken": env[0].1 })),
                            None => Err("The broker lent a login Claude Code cannot use.".to_string()),
                        },
                        Err(reason) => Err(reason),
                    },
                    None => Err("This session has no lent login to renew.".to_string()),
                };
                let _ = self.respond_control(&request_id, answer).await;
            }
            "can_use_tool" => {
                let tool_name = req.get("tool_name").and_then(|s| s.as_str()).unwrap_or("tool").to_string();
                let input = req.get("input").cloned().unwrap_or(Value::Null);
                let suggestions = req.get("permission_suggestions").and_then(|s| s.as_array()).cloned().unwrap_or_default();
                let tool_call_id = req.get("tool_use_id").and_then(|s| s.as_str()).map(str::to_string);
                let summary = match req.get("description").and_then(|s| s.as_str()) {
                    Some(d) if !d.is_empty() => format!("{tool_name}: {d}"),
                    _ => summarize_tool_call(&tool_name, &input),
                };
                self.pending_permissions.lock().await.insert(request_id.clone(), (input.clone(), suggestions.clone()));
                self.emit(DriverEvent::PermissionRequest { request_id, tool_call_id, tool_name, input, summary, suggestions }).await;
            }
            "request_user_dialog" => {
                if req.get("dialog_kind").and_then(Value::as_str) != Some("resume_return") {
                    // Undeclared dialogs may belong to another attached client. Do not settle them.
                    self.emit(DriverEvent::Notice {
                        level: NoticeLevel::Warning,
                        text: "Claude requested an unsupported dialog. Answer it in the originating client.".into(),
                        data: None,
                    })
                    .await;
                    return;
                }
                self.pending_permissions.lock().await.insert(request_id.clone(), (json!({ "_kybern_dialog": req }), vec![]));
                self.emit(DriverEvent::PermissionRequest {
                    request_id,
                    tool_call_id: req.get("tool_use_id").and_then(Value::as_str).map(str::to_owned),
                    tool_name: "ui_select".into(),
                    input: json!({ "title": "Resume session", "options": ["Compact and continue", "Keep full history", "Keep full history and skip future prompts"] }),
                    summary: "Resume with a summary to use fewer tokens, or keep the full conversation.".into(),
                    suggestions: vec![],
                }).await;
            }
            "elicitation" => {
                self.pending_permissions.lock().await.insert(request_id.clone(), (json!({ "_kybern_elicitation": req }), vec![]));
                self.emit(DriverEvent::PermissionRequest {
                    request_id,
                    tool_call_id: None,
                    tool_name: "mcp_elicitation".into(),
                    input: req.clone(),
                    summary: req.get("message").and_then(Value::as_str).unwrap_or("Provide the requested information").into(),
                    suggestions: vec![],
                })
                .await;
            }
            "hook_callback" => {
                self.deliver_child_hook(&request_id, req).await;
            }
            other => {
                tracing::debug!(subtype = other, "unsupported control request from claude");
                let _ = self.respond_control(&request_id, Err(format!("kybern does not support {other}"))).await;
            }
        }
    }

    async fn deliver_child_hook(&self, request_id: &str, req: &Value) {
        let input = &req["input"];
        let agent = input.get("agent_id").and_then(Value::as_str);
        let mut inbox = self.child_inbox.lock().await;
        let target = agent.filter(|id| {
            req.get("callback_id").and_then(Value::as_str) == Some(CHILD_MESSAGE_HOOK)
                && input.get("hook_event_name").and_then(Value::as_str) == Some("PreToolUse")
                && inbox.active.contains(*id)
        });
        let pending = target.and_then(|id| inbox.pending.get(id)).cloned().unwrap_or_default();
        let response = if pending.is_empty() {
            json!({"continue": true})
        } else {
            let text = pending.iter().map(|(_, text)| text.as_str()).collect::<Vec<_>>().join("\n\n");
            json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "additionalContext": format!("The user sent this message to you while you were working. Follow it within your task:\n\n{text}")}})
        };
        // Successful write to this actual native callback is the acknowledgment boundary.
        // If it fails, keep pending until the daemon settles the ended session.
        if self.respond_control(request_id, Ok(response)).await.is_err() {
            return;
        }
        if let Some(target) = target {
            inbox.pending.remove(target);
        }
        drop(inbox);
        if let Some(task_id) = target {
            for (message_id, _) in pending {
                self.emit(DriverEvent::SubagentMessageDelivered { task_id: task_id.into(), message_id }).await;
            }
        }
    }

    async fn respond_control(&self, request_id: &str, result: std::result::Result<Value, String>) -> Result<()> {
        let response = match result {
            Ok(r) => json!({ "subtype": "success", "request_id": request_id, "response": r }),
            Err(e) => json!({ "subtype": "error", "request_id": request_id, "error": e }),
        };
        self.child.write(&json!({ "type": "control_response", "response": response })).await
    }

    /// Gives an agent task without a role the `subagent_type` its launching call carried.
    async fn fill_agent_role(&self, task: &mut DriverRuntimeTask) {
        if task.kind != RuntimeTaskKind::Agent || task.provider_type.is_some() {
            return;
        }
        let Some(call) = task.tool_call_id.as_deref() else { return };
        let st = self.state.lock().await;
        task.provider_type = st.agent_roles.iter().rev().find(|(id, _)| id == call).map(|(_, role)| role.clone());
    }

    async fn handle_system(&self, v: &Value) {
        match v.get("subtype").and_then(|s| s.as_str()).unwrap_or("") {
            "init" => {
                let session_id = v.get("session_id").and_then(|s| s.as_str()).unwrap_or("").to_string();
                let model = v.get("model").and_then(|s| s.as_str()).map(str::to_string);
                let mut st = self.state.lock().await;
                let bound = (session_id.clone(), model.clone());
                if st.last_bound.as_ref() != Some(&bound) {
                    st.last_bound = Some(bound);
                    *self.session_id.lock().await = session_id.clone();
                    drop(st);
                    self.emit(DriverEvent::SessionBound { session_id, model }).await;
                }
            }
            "status" => {
                if v.get("status").and_then(|s| s.as_str()) == Some("requesting") && v.get("parent_tool_use_id").is_none_or(Value::is_null)
                {
                    self.emit(DriverEvent::ResponseStarted).await;
                }
                if v.get("status").and_then(|s| s.as_str()) == Some("compacting") {
                    self.emit(DriverEvent::Notice { level: NoticeLevel::Info, text: "compacting context".into(), data: None }).await;
                }
            }
            "compact_boundary" => {
                let meta = &v["compact_metadata"];
                self.emit(DriverEvent::Notice {
                    level: NoticeLevel::Info,
                    text: match meta.get("post_tokens").and_then(Value::as_u64) {
                        Some(after) => format!("Context compacted to {after} tokens"),
                        None => "Context compacted".into(),
                    },
                    data: Some(meta.clone()),
                })
                .await;
            }
            "permission_denied" => {
                self.emit(DriverEvent::Notice {
                    level: NoticeLevel::Warning,
                    text: format!(
                        "{} denied: {}",
                        v.get("tool_name").and_then(|s| s.as_str()).unwrap_or("tool"),
                        v.get("message").and_then(|s| s.as_str()).unwrap_or("")
                    ),
                    data: None,
                })
                .await;
            }
            "informational" => {
                let level = match v.get("level").and_then(|s| s.as_str()) {
                    Some("warning") => NoticeLevel::Warning,
                    _ => NoticeLevel::Info,
                };
                self.emit(DriverEvent::Notice {
                    level,
                    text: v.get("content").and_then(|s| s.as_str()).unwrap_or("").to_string(),
                    data: None,
                })
                .await;
            }
            "task_started" => {
                if let Some(mut task) = claude_task_started(v) {
                    self.fill_agent_role(&mut task).await;
                    if task.kind == RuntimeTaskKind::Agent && task.status.is_active() {
                        self.child_inbox.lock().await.active.insert(task.id.clone());
                    }
                    // The launch prompt becomes the first message of the subagent's own thread.
                    if task.kind == RuntimeTaskKind::Agent
                        && let Some(prompt) = v.get("prompt").and_then(Value::as_str).filter(|prompt| !prompt.trim().is_empty())
                    {
                        self.emit(DriverEvent::SubagentPrompt { task_id: task.id.clone(), prompt: prompt.to_string() }).await;
                    }
                    self.emit(DriverEvent::RuntimeTaskStarted(task)).await;
                }
            }
            "task_progress" | "task_updated" => {
                if let Some(update) = claude_task_update(v) {
                    let terminal = update.status.is_some_and(|status| !status.is_active());
                    if terminal {
                        self.child_inbox.lock().await.finish(&update.id);
                    }
                    self.emit(if terminal { DriverEvent::RuntimeTaskCompleted(update) } else { DriverEvent::RuntimeTaskUpdated(update) })
                        .await;
                }
            }
            "task_notification" => {
                if let Some(tool_use_id) = v.get("tool_use_id").and_then(Value::as_str) {
                    self.state.lock().await.child_streams.remove(tool_use_id);
                }
                if let Some(mut update) = claude_task_update(v) {
                    if update.status.is_none() {
                        update.status = Some(RuntimeTaskStatus::Completed);
                    }
                    self.child_inbox.lock().await.finish(&update.id);
                    self.emit(DriverEvent::RuntimeTaskCompleted(update)).await;
                }
            }
            "background_tasks_changed" => {
                if let Some(tasks) = v.get("tasks").or_else(|| v.get("running_background_tasks")).and_then(Value::as_array) {
                    for task in tasks {
                        if let Some(mut task) = claude_background_task(task) {
                            self.fill_agent_role(&mut task).await;
                            if task.kind == RuntimeTaskKind::Agent && task.status.is_active() {
                                self.child_inbox.lock().await.active.insert(task.id.clone());
                            }
                            self.emit(DriverEvent::RuntimeTaskStarted(task)).await;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    async fn handle_stream_event(&self, v: &Value) {
        // Root deltas are the assistant transcript. A subagent's carry the id of
        // the tool call that launched it; they are tagged so the daemon records
        // them in that subagent's own thread, never in the parent's.
        let parent = v.get("parent_tool_use_id").and_then(Value::as_str).map(str::to_string);
        let origin = match &parent {
            Some(task_id) => EventOrigin::Agent { task_id: task_id.clone(), provider_thread_id: None },
            None => EventOrigin::Root,
        };
        let ev = &v["event"];
        match ev.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "message_start" => {
                if parent.is_none() {
                    self.emit(DriverEvent::ResponseStarted).await;
                }
                if let Some(id) = ev.pointer("/message/id").and_then(|i| i.as_str()) {
                    let mut st = self.state.lock().await;
                    let (message, thinking_block) = st.stream_cursor(parent.as_deref());
                    *message = Some(id.to_string());
                    *thinking_block = None;
                }
            }
            "content_block_start" => {
                if ev.pointer("/content_block/type").and_then(Value::as_str) == Some("thinking") {
                    let mut st = self.state.lock().await;
                    *st.stream_cursor(parent.as_deref()).1 = ev.get("index").and_then(Value::as_u64);
                }
            }
            "content_block_stop" => {
                let index = ev.get("index").and_then(Value::as_u64);
                let message_id = {
                    let mut st = self.state.lock().await;
                    let (message, thinking_block) = st.stream_cursor(parent.as_deref());
                    if index.is_none() || *thinking_block != index {
                        return;
                    }
                    *thinking_block = None;
                    message.clone()
                };
                if let Some(message_id) = message_id {
                    self.emit(DriverEvent::ThinkingCompleted { message_id, origin }).await;
                }
            }
            "content_block_delta" => {
                let delta = &ev["delta"];
                let message_id = match self.state.lock().await.stream_cursor(parent.as_deref()).0.clone() {
                    Some(id) => id,
                    None => return,
                };
                match delta.get("type").and_then(|t| t.as_str()) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                            self.emit(DriverEvent::TextDelta { message_id, origin, delta: text.to_string() }).await;
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta.get("thinking").and_then(|t| t.as_str()) {
                            self.emit(DriverEvent::ThinkingDelta { message_id, origin, delta: text.to_string() }).await;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    async fn handle_assistant(&self, v: &Value) {
        let parent = v.get("parent_tool_use_id").and_then(|p| p.as_str()).map(str::to_string);
        if parent.is_none()
            && let Some(u) = v.get("uuid").and_then(|u| u.as_str())
        {
            self.state.lock().await.last_assistant_uuid = Some(u.to_string());
        }
        let msg = &v["message"];
        if parent.is_none()
            && let (Some(model), Some(usage)) = (msg.get("model").and_then(Value::as_str), msg.get("usage"))
        {
            let used = ["input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"]
                .iter()
                .map(|key| usage.get(*key).and_then(Value::as_u64).unwrap_or(0))
                .sum();
            self.state.lock().await.last_context = Some((model.into(), used));
        }

        let message_id = msg.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
        if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
            self.emit(DriverEvent::Notice { level: NoticeLevel::Error, text: format!("API error: {err}"), data: Some(msg.clone()) }).await;
        }
        let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) else { return };
        // A subagent's own prose is tagged with the call that launched it.
        let origin = match &parent {
            Some(task_id) => EventOrigin::Agent { task_id: task_id.clone(), provider_thread_id: None },
            None => EventOrigin::Root,
        };
        for block in blocks {
            match block.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "text" => {
                    let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    let mut st = self.state.lock().await;
                    let acc = st.text.entry(message_id.clone()).or_default();
                    acc.push_str(text);
                    let full = acc.clone();
                    let thinking = st.thinking.get(&message_id).cloned();
                    drop(st);
                    self.emit(DriverEvent::MessageCompleted {
                        message_id: message_id.clone(),
                        origin: origin.clone(),
                        text: full,
                        thinking,
                    })
                    .await;
                }
                "thinking" => {
                    let text = block.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                    self.state.lock().await.thinking.entry(message_id.clone()).or_default().push_str(text);
                }
                "tool_use" => {
                    let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                    if matches!(name, "Agent" | "Task")
                        && let (Some(id), Some(role)) =
                            (block.get("id").and_then(Value::as_str), block.pointer("/input/subagent_type").and_then(Value::as_str))
                        && !role.trim().is_empty()
                    {
                        self.state.lock().await.remember_agent_role(id, role);
                    }
                    self.emit(DriverEvent::ToolStarted(ToolCall {
                        id: block.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string(),
                        name: name.to_string(),
                        input: block.get("input").cloned().unwrap_or(Value::Null),
                        parent_id: parent.clone(),
                    }))
                    .await;
                }
                _ => {}
            }
        }
    }

    async fn handle_user(&self, v: &Value) {
        if v.get("isReplay").and_then(|r| r.as_bool()).unwrap_or(false) {
            return;
        }
        let Some(blocks) = v.pointer("/message/content").and_then(|c| c.as_array()) else { return };
        let structured = v.get("tool_use_result").cloned();
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                continue;
            }
            let id = block.get("tool_use_id").and_then(|i| i.as_str()).unwrap_or("").to_string();
            let is_error = block.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
            let content = block.get("content").cloned().unwrap_or(Value::Null);
            let output = match &structured {
                Some(s) if blocks.len() == 1 => json!({ "content": content, "structured": s }),
                _ => json!({ "content": content }),
            };
            self.emit(DriverEvent::ToolCompleted { tool_call_id: id, output, is_error }).await;
        }
    }

    async fn handle_result(&self, v: &Value) {
        let context = self.state.lock().await.last_context.clone();
        if let Some((model, used_tokens)) = context
            && let Some(window_tokens) = v
                .get("modelUsage")
                .and_then(|models| models.get(&model))
                .and_then(|model| model.get("contextWindow"))
                .and_then(Value::as_u64)
        {
            self.emit(DriverEvent::UsageUpdated(kybern_protocol::ProviderUsage {
                context: Some(kybern_protocol::ContextUsage { used_tokens, window_tokens }),
                limits: None,
            }))
            .await;
        }

        let subtype = v.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
        let mut usage = v.get("usage").map(parse_usage).unwrap_or_default();
        let mut duration_ms = v.get("duration_ms").and_then(|d| d.as_u64()).unwrap_or(0);
        let total_cost = v.get("total_cost_usd").and_then(|c| c.as_f64()).unwrap_or(0.0);
        let (cost_usd, anchors) = {
            let mut st = self.state.lock().await;
            let delta = (total_cost - st.last_total_cost).max(0.0);
            st.last_total_cost = total_cost;
            st.text.clear();
            st.thinking.clear();
            st.current_message = None;
            st.thinking_block = None;
            if subtype == "success"
                && v.pointer("/origin/kind").and_then(Value::as_str) == Some("task-notification")
                && st.current_user_uuid.is_some()
            {
                st.queued_usage.add(&usage);
                st.queued_cost += delta;
                st.queued_duration_ms = st.queued_duration_ms.saturating_add(duration_ms);
                drop(st);
                self.emit(DriverEvent::ResponseBoundary).await;
                return;
            }
            usage.add(&std::mem::take(&mut st.queued_usage));
            duration_ms = duration_ms.saturating_add(std::mem::take(&mut st.queued_duration_ms));
            let cost = delta + std::mem::take(&mut st.queued_cost);
            let anchors = crate::TurnAnchors { turn_id: st.current_user_uuid.take(), previous_end: st.last_assistant_uuid.take() };
            (Some(cost), anchors)
        };
        let terminal = v.get("terminal_reason").and_then(|t| t.as_str()).unwrap_or("");
        let errors = v
            .get("errors")
            .and_then(|e| e.as_array())
            .map(|a| a.iter().filter_map(|e| e.as_str()).collect::<Vec<_>>().join("; "))
            .unwrap_or_default();
        // Claude reports a rejected token as a result with `is_error` (subtype "success") whose text says why.
        let rejected_login = v.get("is_error").and_then(Value::as_bool) == Some(true)
            && self.state.lock().await.lent_login
            && v.get("result")
                .and_then(Value::as_str)
                .is_some_and(|text| crate::login_lend::is_login_failure(ProviderKind::ClaudeCode, text));
        let ev = match subtype {
            _ if rejected_login => {
                DriverEvent::TurnFailed { error: v.get("result").and_then(Value::as_str).unwrap_or_default().to_string() }
            }
            "success" => DriverEvent::TurnCompleted { stop_reason: StopReason::Completed, usage, cost_usd, duration_ms, anchors },
            "error_max_turns" => DriverEvent::TurnCompleted { stop_reason: StopReason::MaxTurns, usage, cost_usd, duration_ms, anchors },
            _ if terminal == "aborted_streaming" || terminal == "aborted" => {
                DriverEvent::TurnCompleted { stop_reason: StopReason::Interrupted, usage, cost_usd, duration_ms, anchors }
            }
            _ => DriverEvent::TurnFailed { error: if errors.is_empty() { format!("claude: {subtype}") } else { errors } },
        };
        self.emit(ev).await;
    }
}

fn parse_usage(u: &Value) -> Usage {
    let g = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    Usage {
        input_tokens: g("input_tokens"),
        output_tokens: g("output_tokens"),
        cache_read_tokens: g("cache_read_input_tokens"),
        cache_write_tokens: g("cache_creation_input_tokens"),
    }
}

fn claude_task_started(value: &Value) -> Option<DriverRuntimeTask> {
    let id = value.get("task_id").or_else(|| value.get("taskId")).or_else(|| value.get("id")).and_then(Value::as_str)?.to_string();
    let provider_type = value
        .get("task_type")
        .or_else(|| value.get("taskType"))
        .or_else(|| value.get("subagent_type"))
        .or_else(|| value.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("local_agent")
        .to_string();
    let kind = match provider_type.as_str() {
        "local_bash" | "shell" => RuntimeTaskKind::Process,
        "monitor" | "monitor_mcp" => RuntimeTaskKind::Monitor,
        _ => RuntimeTaskKind::Agent,
    };
    // An agent's role (`Explore`, `general-purpose`) is its `subagent_type`. The generic task
    // type (`local_agent`) is no role, so without one the role stays absent.
    let provider_type = match kind {
        RuntimeTaskKind::Agent => value
            .get("subagent_type")
            .or_else(|| value.get("subagentType"))
            .and_then(Value::as_str)
            .filter(|role| !role.trim().is_empty())
            .map(str::to_string),
        _ => Some(provider_type),
    };
    let status = value.get("status").and_then(Value::as_str).map(claude_task_status).unwrap_or(RuntimeTaskStatus::Running);
    let backgrounded = value.get("is_backgrounded").or_else(|| value.get("isBackgrounded")).and_then(Value::as_bool).unwrap_or(false);
    let tool_call_id = value.get("tool_use_id").or_else(|| value.get("toolUseId")).and_then(Value::as_str).map(str::to_string);
    let title = value
        .get("description")
        .or_else(|| value.get("summary"))
        .or_else(|| value.get("prompt"))
        .and_then(Value::as_str)
        .filter(|title| !title.trim().is_empty())
        .map(|title| title.lines().next().unwrap_or(title).chars().take(120).collect())
        .unwrap_or_else(|| match kind {
            RuntimeTaskKind::Agent => "Subagent".into(),
            RuntimeTaskKind::Process => "Background process".into(),
            RuntimeTaskKind::Monitor => "Monitor".into(),
        });
    Some(DriverRuntimeTask {
        id,
        kind,
        status,
        title,
        detail: value.get("summary").and_then(Value::as_str).map(str::to_string),
        provider_type,
        parent_id: value.get("parent_task_id").or_else(|| value.get("parentTaskId")).and_then(Value::as_str).map(str::to_string),
        tool_call_id: tool_call_id.clone(),
        provider_thread_id: None,
        model: value.get("model").and_then(Value::as_str).map(str::to_string),
        effort: None,
        backgrounded,
        last_tool_name: value.get("last_tool_name").or_else(|| value.get("lastToolName")).and_then(Value::as_str).map(str::to_string),
        usage: claude_task_usage(value.get("usage")),
        stats: claude_task_stats(value.get("usage")),
        capabilities: RuntimeTaskCapabilities {
            stop: status.is_active(),
            background: status.is_active() && !backgrounded && tool_call_id.is_some() && kind != RuntimeTaskKind::Monitor,
        },
    })
}

fn claude_background_task(value: &Value) -> Option<DriverRuntimeTask> {
    let mut task = claude_task_started(value)?;
    // `background_tasks_changed` is the full background roster; individual
    // rows do not repeat the background flag.
    task.backgrounded = true;
    task.capabilities.background = false;
    Some(task)
}

fn claude_task_update(value: &Value) -> Option<DriverRuntimeTaskUpdate> {
    let patch = value.get("patch").unwrap_or(value);
    let id = value
        .get("task_id")
        .or_else(|| value.get("taskId"))
        .or_else(|| value.get("id"))
        .or_else(|| patch.get("task_id"))
        .or_else(|| patch.get("taskId"))
        .and_then(Value::as_str)?
        .to_string();
    let status = patch.get("status").or_else(|| value.get("status")).and_then(Value::as_str).map(claude_task_status);
    let usage_value = value.get("usage").or_else(|| patch.get("usage"));
    let backgrounded = patch
        .get("is_backgrounded")
        .or_else(|| patch.get("isBackgrounded"))
        .or_else(|| value.get("is_backgrounded"))
        .or_else(|| value.get("isBackgrounded"))
        .and_then(Value::as_bool);
    Some(DriverRuntimeTaskUpdate {
        id,
        status,
        detail: value
            .get("summary")
            .or_else(|| value.get("description"))
            .or_else(|| patch.get("summary"))
            .and_then(Value::as_str)
            .map(str::to_string),
        backgrounded,
        last_tool_name: value
            .get("last_tool_name")
            .or_else(|| value.get("lastToolName"))
            .or_else(|| patch.get("last_tool_name"))
            .or_else(|| patch.get("lastToolName"))
            .and_then(Value::as_str)
            .map(str::to_string),
        usage: claude_task_usage(usage_value),
        stats: usage_value.map(|usage| claude_task_stats(Some(usage))),
        capabilities: (backgrounded == Some(true))
            .then_some(RuntimeTaskCapabilities { stop: status.is_none_or(RuntimeTaskStatus::is_active), background: false }),
    })
}

fn claude_task_status(status: &str) -> RuntimeTaskStatus {
    match status {
        "pending" => RuntimeTaskStatus::Pending,
        "paused" | "waiting" => RuntimeTaskStatus::Waiting,
        "completed" | "done" | "success" => RuntimeTaskStatus::Completed,
        "failed" | "error" => RuntimeTaskStatus::Failed,
        "killed" | "stopped" | "cancelled" | "canceled" => RuntimeTaskStatus::Stopped,
        "interrupted" => RuntimeTaskStatus::Interrupted,
        _ => RuntimeTaskStatus::Running,
    }
}

fn claude_task_usage(value: Option<&Value>) -> Option<Usage> {
    let value = value?;
    let usage = parse_usage(value);
    (usage != Usage::default()).then_some(usage)
}

fn claude_task_stats(value: Option<&Value>) -> RuntimeTaskStats {
    let Some(value) = value else { return RuntimeTaskStats::default() };
    RuntimeTaskStats {
        token_count: value.get("total_tokens").or_else(|| value.get("totalTokens")).and_then(Value::as_u64),
        tool_uses: value.get("tool_uses").or_else(|| value.get("toolUses")).and_then(Value::as_u64),
        duration_ms: value.get("duration_ms").or_else(|| value.get("durationMs")).and_then(Value::as_u64),
        cpu_percent: None,
        rss_kb: None,
    }
}

fn content_blocks(message: &UserMessage) -> Vec<Value> {
    let Some(last_skill) = message.parts.iter().rposition(|part| matches!(part, ContentPart::Skill { .. })) else {
        return message.parts.iter().filter_map(claude_block).collect();
    };

    let mut blocks = Vec::new();
    for part in &message.parts[..last_skill] {
        if let Some(block) = match part {
            // Claude expands only one leading slash command. Earlier selected
            // skills remain explicit context for the model to invoke itself.
            ContentPart::Skill { name, .. } => Some(json!({ "type": "text", "text": format!("/{name}") })),
            _ => claude_block(part),
        } {
            blocks.push(block);
        }
    }

    let ContentPart::Skill { name, .. } = &message.parts[last_skill] else { unreachable!() };
    let mut trailing = String::new();
    let mut trailing_media = Vec::new();
    for part in &message.parts[last_skill + 1..] {
        match part {
            ContentPart::Text { text } => trailing.push_str(text),
            ContentPart::FileMention { path } => {
                trailing.push('@');
                trailing.push_str(path);
            }
            ContentPart::ThreadReference { thread_id, title, .. } => {
                trailing.push_str(&thread_reference_text(*thread_id, title));
            }
            ContentPart::Attachment { name, .. } => {
                trailing.push_str(&format!("\n[attached file: {name}]"));
            }
            ContentPart::Mention { name, .. } => {
                trailing.push('@');
                trailing.push_str(name);
            }
            ContentPart::Image { .. } => {
                if let Some(block) = claude_block(part) {
                    trailing_media.push(block);
                }
            }
            ContentPart::Skill { .. } => unreachable!(),
            ContentPart::ThreadMessage { .. } | ContentPart::AgentResults { .. } | ContentPart::SystemRow { .. } => {
                if let Some(text) = part.orchestration_text() {
                    trailing.push('\n');
                    trailing.push_str(&text);
                }
            }
        }
    }
    // Claude Code only expands a slash command at the start of the last text
    // block. Media can precede it, so keep uploads while making invocation
    // deterministic.
    blocks.extend(trailing_media);
    blocks.push(json!({ "type": "text", "text": format!("/{name}{trailing}").trim_end() }));
    blocks
}

fn claude_block(part: &ContentPart) -> Option<Value> {
    match part {
        ContentPart::Text { text } => Some(json!({ "type": "text", "text": text })),
        ContentPart::FileMention { path } => Some(json!({ "type": "text", "text": format!("@{path}") })),
        ContentPart::ThreadReference { thread_id, title, .. } => {
            Some(json!({ "type": "text", "text": thread_reference_text(*thread_id, title) }))
        }
        ContentPart::Skill { name, .. } => Some(json!({ "type": "text", "text": format!("${name}") })),
        ContentPart::Mention { name, .. } => Some(json!({ "type": "text", "text": format!("@{name}") })),
        ContentPart::Image { media_type, data } => {
            // Validate base64 so a bad upload fails here instead of as an API error mid-turn.
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .is_ok()
                .then(|| json!({ "type": "image", "source": { "type": "base64", "media_type": media_type, "data": data } }))
        }
        ContentPart::Attachment { name, .. } => Some(json!({ "type": "text", "text": format!("[attached file: {name}]") })),
        ContentPart::ThreadMessage { .. } | ContentPart::AgentResults { .. } | ContentPart::SystemRow { .. } => {
            part.orchestration_text().map(|text| json!({ "type": "text", "text": text }))
        }
    }
}

#[async_trait]
impl AgentSession for SessionHandle {
    async fn send_subagent_message(&self, task_id: &str, message_id: &str, message: &UserMessage) -> Result<()> {
        self.0.child_inbox.lock().await.queue(task_id, message_id, child_message_context(message)?)
    }

    async fn compact(&self) -> Result<()> {
        self.send_message(&uuid::Uuid::now_v7().to_string(), &UserMessage::text("/compact")).await
    }

    async fn send_message(&self, message_id: &str, message: &UserMessage) -> Result<()> {
        let session_id = self.0.session_id.lock().await.clone();
        self.0.state.lock().await.current_user_uuid = Some(message_id.to_string());
        self.0
            .child
            .write(&json!({
                "type": "user",
                "uuid": message_id,
                "session_id": session_id,
                "message": { "role": "user", "content": content_blocks(message) },
                "parent_tool_use_id": null,
            }))
            .await
    }

    async fn steer(&self, message_id: &str, message: &UserMessage) -> Result<()> {
        // Claude's stream-json input remains writable while native background
        // work is active. A task-notification result may arrive before Claude
        // consumes this prompt; handle_result keeps that notification as a
        // response boundary and leaves current_user_uuid attached to this input.
        self.send_message(message_id, message).await
    }

    async fn interrupt(&self) -> Result<()> {
        self.0.send_control("interrupt", json!({})).await.map(|_| ())
    }

    async fn set_permission_mode(&self, mode: PermissionMode) -> Result<()> {
        if mode == PermissionMode::FullAccess {
            // bypassPermissions can only be enabled at spawn time with the explicit flag.
            return Err(DriverError::Unsupported("switch to Full access requires restarting the session".into()));
        }
        self.0.send_control("set_permission_mode", json!({ "mode": mode_arg(mode) })).await.map(|_| ())
    }

    async fn set_model(&self, model: &str) -> Result<()> {
        self.0.send_control("set_model", json!({ "model": model })).await.map(|_| ())
    }

    async fn set_effort(&self, _effort: &str) -> Result<()> {
        Err(DriverError::Unsupported("Claude Code effort is fixed when the session starts; start a new thread to change it".into()))
    }

    async fn respond_permission(&self, request_id: &str, decision: &ApprovalDecision) -> Result<()> {
        let Some((mut input, suggestions)) = self.0.pending_permissions.lock().await.get(request_id).cloned() else {
            return Err(DriverError::Protocol(format!("no pending permission request {request_id}")));
        };
        if input.get("_kybern_dialog").is_some() {
            let response = match decision {
                ApprovalDecision::Submit { response } => {
                    let result = match response.get("value").and_then(Value::as_str) {
                        Some("Compact and continue") => "compact",
                        Some("Keep full history") => "continue",
                        Some("Keep full history and skip future prompts") => "never",
                        _ => return Err(DriverError::Protocol("choose a resume option".into())),
                    };
                    json!({ "behavior": "completed", "result": result })
                }
                ApprovalDecision::Deny { .. } => json!({ "behavior": "cancelled" }),
                _ => return Err(DriverError::Protocol("this dialog needs an answer".into())),
            };
            self.0.respond_control(request_id, Ok(response)).await?;
            self.0.pending_permissions.lock().await.remove(request_id);
            return Ok(());
        }
        if input.get("_kybern_elicitation").is_some() {
            let response = match decision {
                ApprovalDecision::Submit { response } => response.clone(),
                ApprovalDecision::Deny { .. } => json!({ "action": "decline" }),
                _ => return Err(DriverError::Protocol("this form needs an answer".into())),
            };
            self.0.respond_control(request_id, Ok(response)).await?;
            self.0.pending_permissions.lock().await.remove(request_id);
            return Ok(());
        }
        let response = match decision {
            ApprovalDecision::Submit { response } => {
                input["answers"] = response.get("answers").cloned().ok_or_else(|| DriverError::Protocol("missing answers".into()))?;
                // Per-question notes and the chosen preview (`AskUserQuestion`'s `annotations`): attachments, exact multi-select labels.
                if let Some(annotations) = response.get("annotations").filter(|annotations| annotations.is_object()) {
                    input["annotations"] = annotations.clone();
                }
                json!({ "behavior": "allow", "updatedInput": input })
            }
            ApprovalDecision::AllowOnce => json!({ "behavior": "allow", "updatedInput": input }),
            ApprovalDecision::AllowAlways => {
                json!({ "behavior": "allow", "updatedInput": input, "updatedPermissions": suggestions })
            }
            ApprovalDecision::Deny { reason } => json!({
                "behavior": "deny",
                "message": reason.clone().unwrap_or_else(|| "The user declined this action.".into()),
                "interrupt": false,
            }),
        };
        self.0.respond_control(request_id, Ok(response)).await?;
        self.0.pending_permissions.lock().await.remove(request_id);
        Ok(())
    }

    async fn stop_runtime_task(&self, task: &RuntimeTask) -> Result<()> {
        self.0.send_control("stop_task", json!({ "task_id": task.id })).await.map(|_| ())
    }

    async fn background_runtime_task(&self, task: &RuntimeTask) -> Result<()> {
        let tool_use_id = task
            .tool_call_id
            .as_deref()
            .ok_or_else(|| DriverError::Unsupported("Claude Code did not expose the tool id needed to background this task".into()))?;
        self.0.send_control("background_tasks", json!({ "tool_use_id": tool_use_id })).await.map(|_| ())
    }

    async fn close(&self) -> Result<()> {
        self.0.child.close().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn resolved_model_id_keeps_the_selector_context_suffix() {
        use super::resolved_model_id;
        assert_eq!(resolved_model_id("opus", "claude-opus-5-5"), "claude-opus-5-5");
        assert_eq!(resolved_model_id("opus[1m]", "claude-opus-5-5"), "claude-opus-5-5[1m]");
        assert_eq!(resolved_model_id("opus[1m]", "claude-opus-5-5[1m]"), "claude-opus-5-5[1m]");
    }

    fn parse_catalog(models: serde_json::Value) -> Vec<super::CatalogModel> {
        crate::claude_catalog::parse_models(&models).unwrap()
    }

    #[test]
    fn catalog_models_carry_versions_descriptions_and_the_account_default() {
        use super::{ClaudeConfig, catalog_default, catalog_models};
        let catalog = parse_catalog(serde_json::json!([
            { "value": "default", "resolvedModel": "claude-opus-5-5[1m]", "displayName": "Default (recommended)",
              "description": "Opus 5.5 with 1M context", "supportsEffort": true, "supportedEffortLevels": ["low", "high"] },
            { "value": "opus[1m]", "resolvedModel": "claude-opus-5-5[1m]", "displayName": "Opus (1M context)",
              "description": "Opus 5.5 with 1M context", "supportsEffort": true, "supportedEffortLevels": ["low", "high"] },
            { "value": "claude-fable-5-1[1m]", "resolvedModel": "claude-fable-5-1", "displayName": "Fable",
              "supportsEffort": true },
            { "value": "haiku", "resolvedModel": "claude-haiku-4-5-20251001", "displayName": "Haiku" },
            { "value": "claude-opus-4-8", "resolvedModel": "claude-opus-4-8", "displayName": "Opus 4.8",
              "description": "Newer version available · select Opus for Opus 5.5", "supportsEffort": true }
        ]));
        assert_eq!(catalog_default(&catalog).as_deref(), Some("opus[1m]"));
        let models = catalog_models(&catalog, &ClaudeConfig::for_model("opus[1m]"), &["medium".into()]);
        let summary: Vec<_> = models.iter().map(|model| (model.id.as_str(), model.display_name.as_str(), model.is_default)).collect();
        assert_eq!(
            summary,
            [
                ("opus[1m]", "Claude Opus 5.5 · 1M", true),
                ("claude-fable-5-1[1m]", "Claude Fable 5.1 · 1M", false),
                ("haiku", "Claude Haiku 4.5", false),
                ("claude-opus-4-8", "Claude Opus 4.8", false),
            ]
        );
        assert_eq!(models[0].resolved_id.as_deref(), Some("claude-opus-5-5[1m]"));
        assert_eq!(models[0].efforts, ["low", "high"]);
        assert_eq!(models[1].efforts, ["medium"], "supports effort without listing levels");
        assert!(models[2].efforts.is_empty() && models[2].default_effort.is_none(), "haiku takes no effort");
        assert_eq!(models[3].description.as_deref(), Some("Newer version available · select Opus for Opus 5.5"));
    }

    #[test]
    fn older_catalogs_match_the_default_by_description_and_stay_unversioned() {
        use super::{ClaudeConfig, catalog_default, catalog_models};
        let catalog = parse_catalog(serde_json::json!([
            { "value": "default", "displayName": "Default (recommended)", "description": "Opus 4.8 · Best for everyday tasks" },
            { "value": "opus", "displayName": "Opus", "description": "Opus 4.8 · Best for everyday tasks" },
            { "value": "claude-fable-5[1m]", "displayName": "Fable", "description": "Fable 5" }
        ]));
        assert_eq!(catalog_default(&catalog).as_deref(), Some("opus"));
        let names: Vec<_> =
            catalog_models(&catalog, &ClaudeConfig::for_model("opus"), &[]).into_iter().map(|model| model.display_name).collect();
        assert_eq!(names, ["Opus", "Claude Fable 5 · 1M"]);

        let lone_default = parse_catalog(serde_json::json!([{ "value": "default", "displayName": "Default (recommended)" }]));
        assert_eq!(catalog_default(&lone_default).as_deref(), Some("default"));
        let models = catalog_models(&lone_default, &ClaudeConfig::for_model("default"), &[]);
        assert_eq!((models[0].display_name.as_str(), models[0].is_default), ("Default (recommended)", true));
    }

    #[tokio::test]
    async fn notification_result_does_not_finish_a_queued_user_request() {
        use super::*;
        let child = Arc::new(NdjsonChild::spawn(crate::test_process::echo()).unwrap());
        let (events, mut rx) = mpsc::channel(32);
        let session = Arc::new(ClaudeSession {
            child: child.clone(),
            events,
            pending_control: Mutex::new(HashMap::new()),
            pending_permissions: Mutex::new(HashMap::new()),
            state: Mutex::new(TurnState::default()),
            child_inbox: Mutex::new(ChildInbox::default()),
            session_id: Mutex::new("test".into()),
            catalog: None,
            _native_mcp_config: None,
        });
        let handle = SessionHandle(session.clone(), crate::ndjson::SessionLifetime::new(child.clone()));
        handle.send_message("first-user", &UserMessage::text("Run a background command")).await.unwrap();
        session
            .handle_frame(json!({"type":"result", "subtype":"success", "total_cost_usd":0.1,
            "duration_ms":10, "usage":{"input_tokens":1}}))
            .await;
        assert!(
            matches!(rx.recv().await, Some(DriverEvent::TurnCompleted { anchors, .. }) if anchors.turn_id.as_deref() == Some("first-user"))
        );

        handle.steer("second-user", &UserMessage::text("Another request")).await.unwrap();
        // Captured Claude notification sequence: request starts, then the result
        // is explicitly attributed to task-notification rather than user input.
        session.handle_frame(json!({"type":"system", "subtype":"status", "status":"requesting"})).await;
        assert!(matches!(rx.recv().await, Some(DriverEvent::ResponseStarted)));
        session
            .handle_frame(json!({"type":"result", "subtype":"success", "origin":{"kind":"task-notification"},
            "total_cost_usd":0.2, "duration_ms":20, "usage":{"input_tokens":2}}))
            .await;
        assert!(matches!(rx.recv().await, Some(DriverEvent::ResponseBoundary)));
        assert!(rx.try_recv().is_err(), "the queued user request must remain active");
        session
            .handle_frame(json!({"type":"result", "subtype":"success", "total_cost_usd":0.3,
            "duration_ms":30, "usage":{"input_tokens":3}}))
            .await;
        let Some(DriverEvent::TurnCompleted { anchors, usage, cost_usd, duration_ms, .. }) = rx.recv().await else {
            panic!("missing final result")
        };
        assert_eq!(anchors.turn_id.as_deref(), Some("second-user"));
        assert_eq!(usage.input_tokens, 5);
        assert!((cost_usd.unwrap() - 0.2).abs() < 1e-10);
        assert_eq!(duration_ms, 50);

        handle.send_message("interrupted-user", &UserMessage::text("queued during background work")).await.unwrap();
        session
            .handle_frame(json!({"type":"result", "subtype":"error_during_execution", "terminal_reason":"aborted",
            "origin":{"kind":"task-notification"}, "total_cost_usd":0.3}))
            .await;
        assert!(matches!(rx.recv().await, Some(DriverEvent::TurnCompleted { stop_reason: StopReason::Interrupted, .. })));

        // A child's stream must never reopen a completed parent response.
        session
            .handle_frame(json!({"type":"stream_event", "parent_tool_use_id":"child",
            "event":{"type":"message_start", "message":{"id":"child-message"}}}))
            .await;
        assert!(rx.try_recv().is_err());
        session
            .handle_frame(json!({"type":"stream_event", "parent_tool_use_id":null,
            "event":{"type":"message_start", "message":{"id":"root-message"}}}))
            .await;
        assert!(matches!(rx.recv().await, Some(DriverEvent::ResponseStarted)));

        // Closing the thinking block ends reasoning; closing text does not.
        for event in [
            json!({"type":"content_block_start", "index":0, "content_block":{"type":"thinking", "thinking":""}}),
            json!({"type":"content_block_delta", "index":0, "delta":{"type":"thinking_delta", "thinking":"Plan."}}),
            json!({"type":"content_block_stop", "index":0}),
            json!({"type":"content_block_start", "index":1, "content_block":{"type":"text", "text":""}}),
            json!({"type":"content_block_stop", "index":1}),
        ] {
            session.handle_frame(json!({"type":"stream_event", "parent_tool_use_id":null, "event":event})).await;
        }
        assert!(matches!(rx.recv().await, Some(DriverEvent::ThinkingDelta { .. })));
        assert!(matches!(rx.recv().await, Some(DriverEvent::ThinkingCompleted { message_id, .. }) if message_id == "root-message"));
        assert!(rx.try_recv().is_err());
        child.kill().await;
    }

    /// Frames recorded from Claude Code 2.1.290 (haiku, `--include-partial-messages`) while a
    /// background Agent read a file: lifecycle plus the subagent's own tool call, result and
    /// answer, all stamped with `parent_tool_use_id`. The CLI sent no `stream_event` for them.
    #[tokio::test]
    async fn subagent_frames_become_origin_tagged_events_and_a_launch_prompt() {
        use super::*;
        let child = Arc::new(NdjsonChild::spawn(crate::test_process::echo()).unwrap());
        let (events, mut rx) = mpsc::channel(64);
        let session = Arc::new(ClaudeSession {
            child: child.clone(),
            events,
            pending_control: Mutex::new(HashMap::new()),
            pending_permissions: Mutex::new(HashMap::new()),
            state: Mutex::new(TurnState::default()),
            child_inbox: Mutex::new(ChildInbox::default()),
            session_id: Mutex::new("test".into()),
            catalog: None,
            _native_mcp_config: None,
        });
        for line in include_str!("../tests/fixtures/claude-subagent-frames.ndjson").lines() {
            session.handle_frame(serde_json::from_str(line).unwrap()).await;
        }
        let mut seen = Vec::new();
        while let Ok(event) = rx.try_recv() {
            seen.push(event);
        }
        let launch = "toolu_017gG1LQ2MRcP3bjnhLKvnD5";
        let task = seen.iter().find_map(|event| match event {
            // The background roster announces the task first, without its launch call.
            DriverEvent::RuntimeTaskStarted(task) if task.kind == RuntimeTaskKind::Agent && task.tool_call_id.is_some() => Some(task),
            _ => None,
        });
        let task = task.expect("the subagent task starts");
        assert_eq!((task.provider_type.as_deref(), task.tool_call_id.as_deref()), (Some("general-purpose"), Some(launch)));
        let prompt = seen.iter().position(|event| {
            matches!(event, DriverEvent::SubagentPrompt { task_id, prompt }
            if *task_id == task.id && prompt.starts_with("Read the file note.txt"))
        });
        let started = seen.iter().position(|event| matches!(event, DriverEvent::RuntimeTaskStarted(task) if task.tool_call_id.is_some()));
        assert!(prompt.is_some() && prompt < started, "the prompt precedes the task start");
        // Its tool call keeps nesting under the launch call, and the result follows.
        assert!(seen.iter().any(|event| matches!(event, DriverEvent::ToolStarted(call)
            if call.name == "Read" && call.parent_id.as_deref() == Some(launch))));
        assert!(seen.iter().any(|event| matches!(event, DriverEvent::ToolCompleted { output, .. }
            if output["content"].as_str().is_some_and(|text| text.contains("hello from fixture")))));
        // Its prose is tagged with the launch call and never root-owned.
        let texts: Vec<_> = seen
            .iter()
            .filter_map(|event| match event {
                DriverEvent::MessageCompleted { origin, text, .. } => Some((origin.clone(), text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(texts.len(), 1, "empty thinking blocks produce no message: {texts:?}");
        assert!(matches!(&texts[0].0, EventOrigin::Agent { task_id, .. } if task_id == launch));
        assert!(texts[0].1.contains("hello from fixture"));
        // Progress carries the latest tool, the notification completes it with token and tool counts.
        assert!(seen.iter().any(|event| matches!(event, DriverEvent::RuntimeTaskUpdated(update)
            if update.last_tool_name.as_deref() == Some("Read"))));
        assert!(seen.iter().any(|event| matches!(event, DriverEvent::RuntimeTaskCompleted(update)
            if update.status == Some(RuntimeTaskStatus::Completed)
                && update.stats.as_ref().is_some_and(|stats| stats.token_count == Some(27432) && stats.tool_uses == Some(1)))));
        child.kill().await;
    }

    #[tokio::test]
    async fn subagent_stream_deltas_follow_their_own_message_and_never_disturb_the_root() {
        use super::*;
        let child = Arc::new(NdjsonChild::spawn(crate::test_process::echo()).unwrap());
        let (events, mut rx) = mpsc::channel(64);
        let session = Arc::new(ClaudeSession {
            child: child.clone(),
            events,
            pending_control: Mutex::new(HashMap::new()),
            pending_permissions: Mutex::new(HashMap::new()),
            state: Mutex::new(TurnState::default()),
            child_inbox: Mutex::new(ChildInbox::default()),
            session_id: Mutex::new("test".into()),
            catalog: None,
            _native_mcp_config: None,
        });
        let frame = |parent: Option<&str>, event: Value| json!({"type":"stream_event", "parent_tool_use_id":parent, "event":event});
        session.handle_frame(frame(None, json!({"type":"message_start", "message":{"id":"root-1"}}))).await;
        session.handle_frame(frame(Some("toolu_a"), json!({"type":"message_start", "message":{"id":"child-a"}}))).await;
        session.handle_frame(frame(Some("toolu_b"), json!({"type":"message_start", "message":{"id":"child-b"}}))).await;
        for (parent, text) in [(Some("toolu_a"), "alpha"), (None, "root"), (Some("toolu_b"), "beta")] {
            session
                .handle_frame(frame(parent, json!({"type":"content_block_delta", "index":0, "delta":{"type":"text_delta", "text":text}})))
                .await;
        }
        assert!(matches!(rx.recv().await, Some(DriverEvent::ResponseStarted)));
        let mut deltas = Vec::new();
        while let Ok(DriverEvent::TextDelta { message_id, origin, delta }) = rx.try_recv() {
            deltas.push((message_id, origin, delta));
        }
        let agent = |task: &str| EventOrigin::Agent { task_id: task.into(), provider_thread_id: None };
        assert_eq!(
            deltas,
            [
                ("child-a".to_string(), agent("toolu_a"), "alpha".to_string()),
                ("root-1".to_string(), EventOrigin::Root, "root".to_string()),
                ("child-b".to_string(), agent("toolu_b"), "beta".to_string()),
            ]
        );
        child.kill().await;
    }

    #[tokio::test]
    async fn questions_and_elicitation_keep_native_response_shapes() {
        use super::*;
        let child = Arc::new(NdjsonChild::spawn(crate::test_process::echo()).unwrap());
        let (events, mut rx) = mpsc::channel(8);
        let session = Arc::new(ClaudeSession {
            child: child.clone(),
            events,
            pending_control: Mutex::new(HashMap::new()),
            pending_permissions: Mutex::new(HashMap::new()),
            state: Mutex::new(TurnState::default()),
            child_inbox: Mutex::new(ChildInbox::default()),
            session_id: Mutex::new("test".into()),
            catalog: None,
            _native_mcp_config: None,
        });
        let handle = SessionHandle(session.clone(), crate::ndjson::SessionLifetime::new(session.child.clone()));
        let questions = json!([{ "question": "Which sections?", "options": [{"label":"Intro"},{"label":"Summary"}], "multiSelect": true }]);
        session.handle_control_request(&json!({ "request_id": "question", "request": { "subtype": "can_use_tool", "tool_name": "AskUserQuestion", "input": { "questions": questions } } })).await;
        assert!(matches!(rx.recv().await, Some(DriverEvent::PermissionRequest { tool_name, .. }) if tool_name == "AskUserQuestion"));
        handle
            .respond_permission(
                "question",
                &ApprovalDecision::Submit { response: json!({ "answers": { "Which sections?": "Intro, Summary" } }) },
            )
            .await
            .unwrap();
        let response = tokio::time::timeout(std::time::Duration::from_secs(1), async { child.lines.lock().await.recv().await.unwrap() })
            .await
            .unwrap();
        assert_eq!(
            response["response"]["response"]["updatedInput"],
            json!({ "questions": questions, "answers": { "Which sections?": "Intro, Summary" } })
        );
        // A multi-select answer goes as the list of labels, with notes when a label holds a comma.
        session.handle_control_request(&json!({ "request_id": "labels", "request": { "subtype": "can_use_tool", "tool_name": "AskUserQuestion", "input": { "questions": questions } } })).await;
        assert!(matches!(rx.recv().await, Some(DriverEvent::PermissionRequest { .. })));
        let annotations = json!({ "Which sections?": { "notes": "Selected options, exactly: \"Intro, short\"; \"Summary\"" } });
        handle
            .respond_permission(
                "labels",
                &ApprovalDecision::Submit {
                    response: json!({ "answers": { "Which sections?": ["Intro, short", "Summary"] }, "annotations": annotations }),
                },
            )
            .await
            .unwrap();
        let response = tokio::time::timeout(std::time::Duration::from_secs(1), async { child.lines.lock().await.recv().await.unwrap() })
            .await
            .unwrap();
        assert_eq!(
            response["response"]["response"]["updatedInput"],
            json!({ "questions": questions, "answers": { "Which sections?": ["Intro, short", "Summary"] }, "annotations": annotations })
        );
        session.handle_control_request(&json!({ "request_id": "form", "request": { "subtype": "elicitation", "message": "Name", "requestedSchema": { "type": "object" } } })).await;
        assert!(matches!(rx.recv().await, Some(DriverEvent::PermissionRequest { tool_name, .. }) if tool_name == "mcp_elicitation"));
        handle
            .respond_permission("form", &ApprovalDecision::Submit { response: json!({ "action": "accept", "content": { "name": "Ada" } }) })
            .await
            .unwrap();
        let response = tokio::time::timeout(std::time::Duration::from_secs(1), async { child.lines.lock().await.recv().await.unwrap() })
            .await
            .unwrap();
        assert_eq!(response["response"]["response"], json!({ "action": "accept", "content": { "name": "Ada" } }));
        for (choice, expected) in
            [("Compact and continue", "compact"), ("Keep full history", "continue"), ("Keep full history and skip future prompts", "never")]
        {
            session.handle_control_request(&json!({ "request_id": "resume", "request": { "subtype": "request_user_dialog", "dialog_kind": "resume_return", "payload": {} } })).await;
            assert!(matches!(rx.recv().await, Some(DriverEvent::PermissionRequest { tool_name, .. }) if tool_name == "ui_select"));
            handle.respond_permission("resume", &ApprovalDecision::Submit { response: json!({ "value": choice }) }).await.unwrap();
            let response =
                tokio::time::timeout(std::time::Duration::from_secs(1), async { child.lines.lock().await.recv().await.unwrap() })
                    .await
                    .unwrap();
            assert_eq!(response["response"]["response"], json!({ "behavior": "completed", "result": expected }));
        }
        child.kill().await;
    }
    use super::{
        claude_background_task, claude_efforts, claude_model_aliases, claude_task_started, claude_task_update, content_blocks,
        write_claude_mcp_config,
    };
    use crate::NativeToolBridge;
    use kybern_protocol::{ContentPart, RuntimeTaskKind, RuntimeTaskStatus, UserMessage};
    use serde_json::{Value, json};

    const HELP: &str = "\
  --effort <level>  Effort level for the current session\n\
                    (low, medium, high, xhigh, max)\n\
  --environment <environment_id>  Create a cloud session\n\
  --model <model>  Model for the current session. Provide an alias for the latest model\n\
                   (e.g. 'fable', 'opus', or 'sonnet') or a model's full name\n\
                   (e.g. 'claude-fable-5').\n";

    #[test]
    fn parses_capabilities_reported_by_help() {
        assert_eq!(claude_model_aliases(HELP), ["fable", "opus", "sonnet"]);
        assert_eq!(claude_efforts(HELP), ["low", "medium", "high", "xhigh", "max"]);
    }

    #[test]
    fn writes_scoped_mcp_config_and_removes_it_on_drop() {
        let bridge = NativeToolBridge {
            server_name: "kybern".into(),
            endpoint: Some("http://127.0.0.1:4199/native-tools/mcp".into()),
            authorization: Some("session-capability".into()),
            coordinator_instructions: None,
            guide: None,
            tools: vec![],
            restrictions: Default::default(),
        };
        let config = write_claude_mcp_config(&bridge).unwrap();
        let bytes = std::fs::read(&config.path).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["mcpServers"]["kybern"]["url"], "http://127.0.0.1:4199/native-tools/mcp");
        assert_eq!(value["mcpServers"]["kybern"]["headers"]["Authorization"], "Bearer session-capability");
        assert_eq!(value["mcpServers"]["kybern"]["timeout"], 21_600_000);
        assert!(config.path.exists());
        let path = config.path.clone();
        drop(config);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn one_shot_keeps_the_prompt_out_of_variadic_tool_rules() {
        use crate::AgentDriver;
        let root = tempfile::tempdir().unwrap();
        let binary = root.path().join("claude-fixture");
        kybern_protocol::process::write_executable(&binary, "#!/usr/bin/env python3\nimport json,sys\nprint(json.dumps(sys.argv[1:]))\n")
            .unwrap();
        let out =
            super::ClaudeDriver::default().one_shot(root.path(), "Name this session", Some(&binary), &Default::default()).await.unwrap();
        let args: Vec<String> = serde_json::from_str(&out).unwrap();
        assert_eq!(args[args.len() - 2..], ["--", "Name this session"]);
    }

    #[tokio::test]
    async fn session_start_resume_and_fork_scope_mcp_and_coordinator_bootstrap() {
        use crate::{AgentDriver, NativeToolDefinition, SessionConfig};
        for (resume, fork, enabled, coordinator, guide) in [
            (false, false, true, false, false),
            (false, false, true, true, false),
            (true, false, true, true, false),
            (true, true, true, true, false),
            (false, false, false, false, false),
            // The guide alone (an ordinary thread), and with a coordinator role after it.
            (false, false, true, false, true),
            (true, false, true, false, true),
            (true, false, true, true, true),
        ] {
            let root = tempfile::tempdir().unwrap();
            let binary = crate::test_process::binary().to_path_buf();
            let bridge = NativeToolBridge {
                server_name: "kybern".into(),
                endpoint: Some("http://127.0.0.1:4199/native-tools/mcp".into()),
                authorization: Some("fixture-capability".into()),
                coordinator_instructions: coordinator.then(|| "COORDINATOR ROLE SENTINEL".into()),
                guide: guide.then(|| "KYBERN GUIDE SENTINEL".into()),
                tools: ["kybern_thread_read", "kybern_collaboration_spawn"]
                    .into_iter()
                    .map(|name| NativeToolDefinition {
                        name: name.into(),
                        description: name.into(),
                        input_schema: json!({"type":"object"}),
                    })
                    .collect(),
                restrictions: Default::default(),
            };
            let spawned = super::ClaudeDriver::default()
                .spawn(SessionConfig {
                    cwd: root.path().into(),
                    model: None,
                    effort: None,
                    permission_mode: kybern_protocol::PermissionMode::FullAccess,
                    native_tool_bridge: enabled.then_some(bridge),
                    resume_session_id: resume.then(|| "existing-provider-session".into()),
                    resume_lacks_instructions: false,
                    fork,
                    rewind: None,
                    binary: Some(binary),
                    env: Default::default(),
                })
                .await
                .unwrap();
            let argv_path = root.path().join("argv.json");
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while !argv_path.exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let args: Vec<String> = serde_json::from_slice(&std::fs::read(argv_path).unwrap()).unwrap();
            spawned.session.close().await.unwrap();
            assert_eq!(args.iter().any(|arg| arg == "--mcp-config"), enabled);
            let delivered = enabled && (coordinator || guide);
            assert_eq!(args.iter().filter(|arg| arg.as_str() == "--append-system-prompt").count(), usize::from(delivered));
            let expected = match (guide, coordinator) {
                (true, true) => Some("KYBERN GUIDE SENTINEL\nCOORDINATOR ROLE SENTINEL"),
                (true, false) => Some("KYBERN GUIDE SENTINEL"),
                (false, true) => Some("COORDINATOR ROLE SENTINEL"),
                (false, false) => None,
            };
            let position = args.iter().position(|arg| arg == "--append-system-prompt");
            assert_eq!(position.map(|index| args[index + 1].as_str()).filter(|_| enabled), expected.filter(|_| enabled));
            assert!(
                !args.iter().any(|arg| ["--system-prompt", "--tools", "--strict-mcp-config"].contains(&arg.as_str())),
                "ordinary MCP availability must not replace system prompts or disable native plugins"
            );
        }
    }

    #[tokio::test]
    async fn a_lead_gets_only_the_read_only_builtins_and_everyone_else_loses_them_all_when_enforced() {
        use crate::{AgentDriver, SessionConfig};
        for (enforced, read_only, expected) in
            [(true, true, Some("Read,Grep,Glob,WebSearch,WebFetch,AskUserQuestion,Skill")), (true, false, Some("")), (false, true, None)]
        {
            let root = tempfile::tempdir().unwrap();
            let binary = crate::test_process::binary().to_path_buf();
            let bridge = NativeToolBridge {
                server_name: "kybern".into(),
                endpoint: Some("http://127.0.0.1:4199/native-tools/mcp".into()),
                authorization: Some("fixture-capability".into()),
                coordinator_instructions: Some("LEAD".into()),
                guide: None,
                tools: vec![crate::NativeToolDefinition {
                    name: "kybern_thread_read".into(),
                    description: "x".into(),
                    input_schema: json!({"type":"object"}),
                }],
                restrictions: crate::NativeToolRestrictions {
                    require_enforcement: enforced,
                    allow_read_only_builtins: read_only,
                    ..Default::default()
                },
            };
            let spawned = super::ClaudeDriver::default()
                .spawn(SessionConfig {
                    cwd: root.path().into(),
                    model: None,
                    effort: None,
                    permission_mode: kybern_protocol::PermissionMode::FullAccess,
                    native_tool_bridge: Some(bridge),
                    resume_session_id: None,
                    resume_lacks_instructions: false,
                    fork: false,
                    rewind: None,
                    binary: Some(binary),
                    env: Default::default(),
                })
                .await
                .unwrap();
            let argv_path = root.path().join("argv.json");
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while !argv_path.exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let args: Vec<String> = serde_json::from_slice(&std::fs::read(argv_path).unwrap()).unwrap();
            spawned.session.close().await.unwrap();
            let tools = args.iter().position(|arg| arg == "--tools").map(|index| args[index + 1].as_str());
            assert_eq!(tools, expected, "enforced={enforced} read_only={read_only}");
            assert_eq!(args.iter().any(|arg| arg == "--strict-mcp-config"), enforced);
        }
    }

    #[tokio::test]
    async fn full_access_launches_new_resumed_and_forked_sessions_with_bypass() {
        use crate::{AgentDriver, SessionConfig};
        use kybern_protocol::PermissionMode;
        for (mode, resume, fork) in [
            (PermissionMode::FullAccess, None, false),
            (PermissionMode::FullAccess, Some("native-session"), false),
            (PermissionMode::FullAccess, Some("native-session"), true),
            (PermissionMode::Supervised, Some("native-session"), false),
        ] {
            let root = tempfile::tempdir().unwrap();
            let binary = crate::test_process::binary().to_path_buf();
            let spawned = super::ClaudeDriver::default()
                .spawn(SessionConfig {
                    cwd: root.path().into(),
                    model: None,
                    effort: None,
                    permission_mode: mode,
                    native_tool_bridge: None,
                    resume_session_id: resume.map(str::to_string),
                    resume_lacks_instructions: false,
                    fork,
                    rewind: None,
                    binary: Some(binary.clone()),
                    env: Default::default(),
                })
                .await
                .unwrap();
            let argv_path = root.path().join("argv.json");
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while !argv_path.exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let args: Vec<String> = serde_json::from_slice(&std::fs::read(argv_path).unwrap()).unwrap();
            spawned.session.close().await.unwrap();
            let bypass = mode == PermissionMode::FullAccess;
            let permission = args.iter().position(|arg| arg == "--permission-mode").map(|index| args[index + 1].as_str());
            assert_eq!(permission, Some(if bypass { "bypassPermissions" } else { "default" }), "{mode:?} resume={resume:?} fork={fork}");
            assert_eq!(args.iter().any(|arg| arg == "--allow-dangerously-skip-permissions"), bypass, "{mode:?} resume={resume:?}");
            assert_eq!(args.iter().any(|arg| arg == "--resume=native-session"), resume.is_some());
        }
    }

    /// Claude Code still asks under bypassPermissions when one of its own
    /// safety checks fires (here, `rm` on a possibly-empty variable path). The
    /// request reaches the daemon like any other, which allows it in Full access.
    #[tokio::test]
    async fn bypass_safety_checks_surface_as_ordinary_permission_requests() {
        use super::*;
        let child = Arc::new(NdjsonChild::spawn(crate::test_process::echo()).unwrap());
        let (events, mut rx) = mpsc::channel(8);
        let session = ClaudeSession {
            child: child.clone(),
            events,
            pending_control: Mutex::new(HashMap::new()),
            pending_permissions: Mutex::new(HashMap::new()),
            state: Mutex::new(TurnState::default()),
            child_inbox: Mutex::new(ChildInbox::default()),
            session_id: Mutex::new("test".into()),
            catalog: None,
            _native_mcp_config: None,
        };
        let command = "for d in a; do rm -f $P/$d/*-[0-9].png; done";
        session
            .handle_control_request(&json!({"request_id":"safety-1","request":{
                "subtype":"can_use_tool","tool_name":"Bash","input":{"command":command,"description":"Delete raw frames"},
                "description":"Delete raw frames","permission_suggestions":[],
                "decision_reason":"Dangerous rm operation on possibly-empty variable path","decision_reason_type":"safetyCheck",
                "classifier_approvable":false,"tool_use_id":"toolu_1","suppress_always_allow_rule":true}}))
            .await;
        let Some(DriverEvent::PermissionRequest { request_id, tool_name, tool_call_id, .. }) = rx.recv().await else {
            panic!("expected a permission request")
        };
        assert_eq!((request_id.as_str(), tool_name.as_str(), tool_call_id.as_deref()), ("safety-1", "Bash", Some("toolu_1")));
        // Keep the handle alive until the echo is read: dropping it ends the session and closes `cat`.
        let handle = SessionHandle(Arc::new(session), crate::ndjson::SessionLifetime::new(child.clone()));
        handle.respond_permission("safety-1", &ApprovalDecision::AllowOnce).await.unwrap();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), child.lines.lock().await.recv()).await.unwrap().unwrap();
        drop(handle);
        assert_eq!(frame.pointer("/response/response/behavior").and_then(Value::as_str), Some("allow"));
        assert_eq!(frame.pointer("/response/response/updatedInput/command").and_then(Value::as_str), Some(command));
        child.close().await;
    }

    #[tokio::test]
    async fn sessions_enable_artifacts_unless_the_session_env_disables_them() {
        use crate::{AgentDriver, SessionConfig};
        for (env, expected) in [(None, "1"), (Some("0"), "0")] {
            let root = tempfile::tempdir().unwrap();
            let binary = crate::test_process::binary().to_path_buf();
            let spawned = super::ClaudeDriver::default()
                .spawn(SessionConfig {
                    cwd: root.path().into(),
                    model: None,
                    effort: None,
                    permission_mode: kybern_protocol::PermissionMode::FullAccess,
                    native_tool_bridge: None,
                    resume_session_id: None,
                    resume_lacks_instructions: false,
                    fork: false,
                    rewind: None,
                    binary: Some(binary),
                    env: env.map(|value| ("CLAUDE_CODE_ARTIFACT".to_string(), value.to_string())).into_iter().collect(),
                })
                .await
                .unwrap();
            let env_path = root.path().join("env.json");
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while !env_path.exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let seen: serde_json::Value = serde_json::from_slice(&std::fs::read(env_path).unwrap()).unwrap();
            spawned.session.close().await.unwrap();
            assert_eq!(seen["CLAUDE_CODE_ARTIFACT"], expected);
            assert_eq!(seen["CLAUDE_CODE_ARTIFACT_AUTO_OPEN"], "0");
            assert_eq!(seen["CLAUDE_CODE_ENTRYPOINT"], "kybern");
        }
    }

    #[test]
    fn claude_is_labelled_as_kybern_unless_the_session_names_a_label() {
        assert_eq!(super::entrypoint(None), "kybern");
        assert_eq!(super::entrypoint(Some(&"  ".to_string())), "kybern");
        assert_eq!(super::entrypoint(Some(&"my-label".to_string())), "my-label");
    }

    #[test]
    fn dispatches_the_last_selected_skill_as_claudes_slash_command() {
        let message = UserMessage {
            parts: vec![
                ContentPart::Text { text: "First use ".into() },
                ContentPart::Skill { name: "review".into(), path: "/skills/review/SKILL.md".into() },
                ContentPart::Text { text: ", then ".into() },
                ContentPart::Skill { name: "fix-ci".into(), path: "/skills/fix-ci/SKILL.md".into() },
                ContentPart::Text { text: " carefully".into() },
            ],
        };
        assert_eq!(
            content_blocks(&message),
            vec![
                json!({ "type": "text", "text": "First use " }),
                json!({ "type": "text", "text": "/review" }),
                json!({ "type": "text", "text": ", then " }),
                json!({ "type": "text", "text": "/fix-ci carefully" }),
            ]
        );
    }

    /// Claude Code 2.1.290 sends `task_started` with `task_type: "local_agent"` and no
    /// `subagent_type`; the role is only in the launching Agent call's input.
    #[tokio::test]
    async fn subagent_role_comes_from_the_launch_call_when_task_started_lacks_it() {
        use super::*;
        let child = Arc::new(NdjsonChild::spawn(crate::test_process::echo()).unwrap());
        let (events, mut rx) = mpsc::channel(64);
        let session = Arc::new(ClaudeSession {
            child: child.clone(),
            events,
            pending_control: Mutex::new(HashMap::new()),
            pending_permissions: Mutex::new(HashMap::new()),
            state: Mutex::new(TurnState::default()),
            child_inbox: Mutex::new(ChildInbox::default()),
            session_id: Mutex::new("test".into()),
            catalog: None,
            _native_mcp_config: None,
        });
        let launch = |id: &str, role: Option<&str>| {
            let input = role.map_or(json!({ "prompt": "go" }), |role| json!({ "subagent_type": role, "prompt": "go" }));
            json!({"type":"assistant", "parent_tool_use_id":null, "message":{"id":format!("msg-{id}"), "content":[
                {"type":"tool_use", "id":id, "name":"Agent", "input":input}]}})
        };
        let started = |task: &str, id: &str| {
            json!({"type":"system", "subtype":"task_started", "task_id":task, "tool_use_id":id,
                "description":"look", "task_type":"local_agent", "prompt":"go"})
        };
        for frame in [
            launch("toolu_a", Some("Explore")),
            started("task-a", "toolu_a"),
            launch("toolu_b", None),
            started("task-b", "toolu_b"),
            json!({"type":"system", "subtype":"background_tasks_changed", "tasks":[
                {"task_id":"task-a", "tool_use_id":"toolu_a", "task_type":"local_agent"}]}),
        ] {
            session.handle_frame(frame).await;
        }
        let mut roles = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let DriverEvent::RuntimeTaskStarted(task) = event {
                roles.push((task.id, task.kind, task.provider_type));
            }
        }
        assert_eq!(
            roles,
            vec![
                ("task-a".to_string(), RuntimeTaskKind::Agent, Some("Explore".to_string())),
                ("task-b".to_string(), RuntimeTaskKind::Agent, None),
                ("task-a".to_string(), RuntimeTaskKind::Agent, Some("Explore".to_string())),
            ]
        );
        child.kill().await;
    }

    #[test]
    fn parses_native_task_lifecycle_and_capabilities() {
        let task = claude_task_started(&json!({
            "task_id": "agent-7",
            "task_type": "local_agent",
            "description": "Inspect the daemon",
            "tool_use_id": "tool-2",
            "status": "running",
            "usage": { "total_tokens": 1200, "tool_uses": 4, "duration_ms": 3200 }
        }))
        .unwrap();
        assert_eq!(task.kind, RuntimeTaskKind::Agent);
        assert_eq!(task.provider_type, None, "the generic task type is not a role");
        assert_eq!(task.status, RuntimeTaskStatus::Running);
        assert_eq!(task.title, "Inspect the daemon");
        assert!(task.capabilities.stop);
        assert!(task.capabilities.background);
        assert_eq!(task.stats.token_count, Some(1200));

        let update = claude_task_update(&json!({
            "taskId": "agent-7",
            "patch": { "isBackgrounded": true, "status": "waiting" },
            "summary": "Waiting for results"
        }))
        .unwrap();
        assert_eq!(update.status, Some(RuntimeTaskStatus::Waiting));
        assert_eq!(update.backgrounded, Some(true));
        assert_eq!(update.detail.as_deref(), Some("Waiting for results"));
        assert!(!update.capabilities.unwrap().background);

        let background = claude_background_task(&json!({
            "task_id": "shell-4",
            "task_type": "local_bash",
            "description": "pnpm dev"
        }))
        .unwrap();
        assert!(background.backgrounded);
        assert!(!background.capabilities.background);
    }

    /// A `/usage` read is a full Claude Code startup. Like the catalog probe,
    /// it must let Claude Code save a refreshed login before it exits.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_usage_read_lets_claude_finish_saving_a_refreshed_login() {
        let temp = tempfile::tempdir().unwrap();
        let (binary, saved, launches) = (temp.path().join("claude"), temp.path().join("saved-login"), temp.path().join("launches"));
        let result = r#"{"type":"result","result":"Current session: 7% used · resets Sep 16 at 7:30pm (UTC)"}"#;
        kybern_protocol::process::write_executable(
            &binary,
            format!(
                "#!/bin/sh\necho launch >> '{}'\nread message\necho '{result}'\nwhile read line; do :; done\nsleep 0.3\necho rotated > '{}'\n",
                launches.display(),
                saved.display()
            ),
        )
        .unwrap();
        let config = temp.path().join("config");
        let env = std::collections::BTreeMap::from([("CLAUDE_CONFIG_DIR".to_string(), config.to_string_lossy().into_owned())]);

        let limits = super::read_account_limits(temp.path(), Some(&binary), &env).await.unwrap();
        assert_eq!(limits[0].used_percent, 7.0);
        tokio::time::timeout(std::time::Duration::from_secs(4), async {
            while !saved.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the usage read stopped Claude Code before it saved the refreshed login");

        // An expiring login is left for the next session to refresh.
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join(".credentials.json"), r#"{"claudeAiOauth":{"refreshToken":"r","expiresAt":1}}"#).unwrap();
        assert!(super::read_account_limits(temp.path(), Some(&binary), &env).await.is_none());
        assert_eq!(std::fs::read_to_string(&launches).unwrap().lines().count(), 1);
    }
}

#[cfg(test)]
mod orchestration_part_tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn thread_messages_and_agent_results_flatten_to_text_blocks() {
        let blocks = content_blocks(&orchestration_message());
        assert_flattened(&blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"));
    }

    #[test]
    fn they_also_flatten_after_a_skill_invocation() {
        let mut message = orchestration_message();
        message.parts.insert(0, ContentPart::Skill { name: "review".into(), path: "/s/SKILL.md".into() });
        let blocks = content_blocks(&message);
        let text = blocks.last().unwrap()["text"].as_str().unwrap();
        assert!(text.starts_with("/review"));
        assert_flattened(text);
    }
}
