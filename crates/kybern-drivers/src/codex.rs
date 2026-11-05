//! Codex driver: speaks `codex app-server` (JSON-RPC 2.0 without the
//! `jsonrpc` member, newline-delimited over stdio).
//!
//! One app-server process per kybern thread, hosting one Codex thread. The
//! server sends requests of its own for approvals; those become permission
//! requests and are answered by id.

use kybern_protocol::process::NoWindow;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use async_trait::async_trait;
use base64::Engine;
use kybern_protocol::*;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::binary::{at_least, resolve, version_of};
use crate::ndjson::NdjsonChild;
use crate::{
    AgentDriver, AgentSession, DriverError, DriverEvent, DriverRuntimeTask, DriverRuntimeTaskUpdate, NativeToolBridge, ProbeContext,
    Result, SessionConfig, SpawnedSession,
};

const MIN_VERSION: (u64, u64, u64) = (0, 140, 0);

#[derive(Default)]
pub struct CodexDriver;

async fn catalog_call(child: &NdjsonChild, id: i64, method: &str, params: Value) -> Option<Value> {
    child.write(&json!({ "id": id, "method": method, "params": params })).await.ok()?;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let frame = {
                let mut lines = child.lines.lock().await;
                lines.recv().await?
            };
            if frame.get("id").and_then(Value::as_i64) == Some(id) {
                return frame.get("result").cloned();
            }
            if let (Some(request_id), Some(_)) = (frame.get("id"), frame.get("method")) {
                let _ = child
                    .write(&json!({ "id": request_id, "error": { "code": -32601, "message": "unsupported during model discovery" } }))
                    .await;
            }
        }
    })
    .await
    .ok()
    .flatten()
}

fn parse_codex_model(item: &Value) -> Option<ProviderModel> {
    if item.get("hidden").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let id = item.get("model").or_else(|| item.get("id")).and_then(Value::as_str)?.to_string();
    let display_name = item.get("displayName").and_then(Value::as_str).unwrap_or(&id).to_string();
    let efforts = item
        .get("supportedReasoningEfforts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|option| option.get("reasoningEffort").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    Some(ProviderModel {
        id,
        display_name,
        resolved_id: None,
        description: item.get("description").and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty()).map(str::to_string),
        provider: None,
        efforts,
        default_effort: item.get("defaultReasoningEffort").and_then(Value::as_str).map(str::to_string),
        is_default: item.get("isDefault").and_then(Value::as_bool).unwrap_or(false),
        parameters: Vec::new(),
        variants: Vec::new(),
    })
}

fn ensure_requested_codex_model(requested: Option<&str>, effective: Option<&str>) -> Result<()> {
    if let (Some(requested), Some(effective)) = (requested, effective)
        && requested != effective
    {
        return Err(DriverError::Unsupported(format!(
            "Codex resolved requested model '{requested}' to '{effective}'. Choose an exact model ID from the provider catalog or omit the model to use the default."
        )));
    }
    Ok(())
}

async fn codex_models(bin: &std::path::Path, context: &ProbeContext) -> Vec<ProviderModel> {
    let mut cmd = crate::binary::command(bin).no_window();
    cmd.arg("app-server");
    if let Some(cwd) = context.cwd.as_deref() {
        cmd.current_dir(cwd);
    }
    cmd.envs(&context.env);
    let child = match NdjsonChild::spawn(cmd) {
        Ok(child) => child,
        Err(_) => return Vec::new(),
    };
    let initialized = catalog_call(
        &child,
        1,
        "initialize",
        json!({
            "clientInfo": { "name": "kybern", "title": "Kybern", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": { "experimentalApi": true }
        }),
    )
    .await
    .is_some();
    if !initialized {
        child.kill().await;
        return Vec::new();
    }
    if child.write(&json!({ "method": "initialized" })).await.is_err() {
        child.kill().await;
        return Vec::new();
    }

    let mut models = Vec::new();
    let mut cursor: Option<String> = None;
    for page in 0..10i64 {
        let Some(result) =
            catalog_call(&child, page + 2, "model/list", json!({ "limit": 100, "cursor": cursor, "includeHidden": false })).await
        else {
            break;
        };
        if let Some(items) = result.get("data").and_then(Value::as_array) {
            models.extend(items.iter().filter_map(parse_codex_model));
        }
        cursor = result.get("nextCursor").and_then(Value::as_str).map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    child.kill().await;
    models
}

/// Read the account's current rate limits straight from the Codex app-server,
/// without starting a thread. Lets the Usage page show live plan limits on
/// demand instead of the last value a running turn happened to report.
pub async fn read_account_limits(
    cwd: &std::path::Path,
    binary: Option<&PathBuf>,
    env: &std::collections::BTreeMap<String, String>,
) -> Option<Vec<kybern_protocol::UsageLimit>> {
    let bin = resolve(ProviderKind::Codex, binary).ok()?;
    let mut cmd = crate::binary::command(bin).no_window();
    cmd.current_dir(cwd).arg("app-server").envs(env);
    let child = NdjsonChild::spawn(cmd).ok()?;
    let initialized = catalog_call(
        &child,
        1,
        "initialize",
        json!({
            "clientInfo": { "name": "kybern", "title": "Kybern", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": { "experimentalApi": true }
        }),
    )
    .await
    .is_some();
    if !initialized || child.write(&json!({ "method": "initialized" })).await.is_err() {
        child.kill().await;
        return None;
    }
    let result = catalog_call(&child, 2, "account/rateLimits/read", json!({})).await;
    child.kill().await;
    parse_rate_limits(&result?["rateLimits"])
}

/// Ask Codex's own app-server for its effective skill catalog. This preserves
/// plugin namespaces, disabled state, and repo/system precedence that cannot be
/// reconstructed reliably from a blind filesystem walk.
pub async fn discover_skills(
    cwd: &std::path::Path,
    binary: Option<&PathBuf>,
    env: &std::collections::BTreeMap<String, String>,
) -> Option<Vec<SkillInfo>> {
    let bin = resolve(ProviderKind::Codex, binary).ok()?;
    let mut cmd = crate::binary::command(bin).no_window();
    cmd.current_dir(cwd).arg("app-server").envs(env);
    let child = NdjsonChild::spawn(cmd).ok()?;
    let initialized = catalog_call(
        &child,
        1,
        "initialize",
        json!({
            "clientInfo": { "name": "kybern", "title": "Kybern", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": { "experimentalApi": false }
        }),
    )
    .await
    .is_some();
    if !initialized || child.write(&json!({ "method": "initialized" })).await.is_err() {
        child.kill().await;
        return None;
    }

    let cwd_text = cwd.to_string_lossy().to_string();
    let result = catalog_call(&child, 2, "skills/list", json!({ "cwds": [&cwd_text] })).await;
    let plugins = catalog_call(&child, 3, "plugin/installed", json!({})).await;
    let mut apps = Vec::new();
    let mut cursor = Value::Null;
    for page in 0..10 {
        let Some(value) = catalog_call(&child, 4 + page, "app/list", json!({ "cursor": cursor, "limit": 100 })).await else { break };
        apps.extend(connected_apps(&value));
        cursor = value.get("nextCursor").cloned().unwrap_or(Value::Null);
        if cursor.is_null() {
            break;
        }
    }
    child.kill().await;
    let result = result?;
    let entries = result.get("data")?.as_array()?;
    let entry =
        entries.iter().find(|entry| entry.get("cwd").and_then(Value::as_str) == Some(cwd_text.as_str())).or_else(|| entries.first())?;
    let mut skills: Vec<SkillInfo> = entry
        .get("skills")?
        .as_array()?
        .iter()
        .filter_map(|skill| {
            let name = skill.get("name")?.as_str()?.trim();
            let path = skill.get("path")?.as_str()?.trim();
            if name.is_empty() || path.is_empty() {
                return None;
            }
            let interface = skill.get("interface");
            let display_name = interface
                .and_then(|value| value.get("displayName"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let description = skill
                .get("shortDescription")
                .and_then(Value::as_str)
                .or_else(|| interface.and_then(|value| value.get("shortDescription")).and_then(Value::as_str))
                .or_else(|| skill.get("description").and_then(Value::as_str))
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let scope = match skill.get("scope").and_then(Value::as_str) {
                Some("repo") => SkillScope::Repo,
                Some("user") => SkillScope::User,
                Some("system") => SkillScope::System,
                Some("admin") => SkillScope::Admin,
                Some("project") => SkillScope::Project,
                _ => SkillScope::Other,
            };
            Some(SkillInfo {
                name: name.to_string(),
                display_name,
                description,
                path: path.to_string(),
                scope,
                enabled: skill.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            })
        })
        .collect();
    skills.extend(plugins.iter().flat_map(installed_plugins));
    skills.extend(apps);
    Some(skills)
}

fn connected_apps(result: &Value) -> Vec<SkillInfo> {
    result
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|app| {
            if app.get("isAccessible").and_then(Value::as_bool) != Some(true)
                || app.get("isEnabled").and_then(Value::as_bool) == Some(false)
            {
                return None;
            }
            let id = app.get("id")?.as_str()?;
            let name = app.get("name")?.as_str()?;
            Some(SkillInfo {
                name: name.into(),
                display_name: Some(name.into()),
                description: app.get("description").and_then(Value::as_str).map(str::to_string),
                path: format!("app://{id}"),
                scope: SkillScope::Plugin,
                enabled: true,
            })
        })
        .collect()
}

/// Installed plugins the composer can mention with `@name`, such as Computer
/// Use. Codex hides plugins without a display name from its own picker, so
/// they are skipped here too.
fn installed_plugins(result: &Value) -> Vec<SkillInfo> {
    let mut out = Vec::new();
    let marketplaces = result.get("marketplaces").and_then(Value::as_array).into_iter().flatten();
    for marketplace in marketplaces {
        let plugins = marketplace.get("plugins").and_then(Value::as_array).into_iter().flatten();
        for plugin in plugins {
            let Some(id) = plugin.get("id").and_then(Value::as_str).map(str::trim).filter(|id| !id.is_empty()) else { continue };
            let Some(name) = plugin.get("name").and_then(Value::as_str).map(str::trim).filter(|name| !name.is_empty()) else { continue };
            if plugin.get("installed").and_then(Value::as_bool) == Some(false) {
                continue;
            }
            let interface = plugin.get("interface");
            let Some(display_name) = interface
                .and_then(|value| value.get("displayName"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            let description = interface
                .and_then(|value| value.get("shortDescription"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            out.push(SkillInfo {
                name: name.to_string(),
                display_name: Some(display_name.to_string()),
                description,
                path: format!("plugin://{id}"),
                scope: SkillScope::Plugin,
                enabled: plugin.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            });
        }
    }
    out
}

impl CodexDriver {
    async fn probe_inner(&self, context: &ProbeContext) -> ProviderStatus {
        let mut status = ProviderStatus {
            kind: ProviderKind::Codex,
            display_name: ProviderKind::Codex.display_name().into(),
            available: false,
            binary_path: None,
            version: None,
            unavailable_reason: None,
            supported_permission_modes: PermissionMode::ALL.to_vec(),
            supports_fork: true,
            supports_model_switch: true,
            supports_effort_switch: true,
            supported_efforts: vec![],
            models: vec![],
            instances: vec!["default".into()],
        };
        let bin = match resolve(ProviderKind::Codex, context.binary.as_ref()) {
            Ok(b) => b,
            Err(e) => {
                status.unavailable_reason = Some(format!("{e}. Install with: npm install -g @openai/codex"));
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
                        Some(format!("Codex {v} is older than the required {}.{}.{}", MIN_VERSION.0, MIN_VERSION.1, MIN_VERSION.2));
                }
                status.version = Some(v);
                if ok {
                    status.models = codex_models(&bin, context).await;
                    for model in &status.models {
                        for effort in &model.efforts {
                            if !status.supported_efforts.contains(effort) {
                                status.supported_efforts.push(effort.clone());
                            }
                        }
                    }
                }
            }
            None => status.unavailable_reason = Some("could not run `codex --version`".into()),
        }
        status
    }
}

#[async_trait]
impl AgentDriver for CodexDriver {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Codex
    }

    async fn probe(&self, binary: Option<&PathBuf>) -> ProviderStatus {
        self.probe_inner(&ProbeContext { binary: binary.cloned(), ..ProbeContext::default() }).await
    }

    async fn probe_with_context(&self, context: &ProbeContext) -> ProviderStatus {
        self.probe_inner(context).await
    }

    async fn one_shot(
        &self,
        cwd: &std::path::Path,
        prompt: &str,
        binary: Option<&PathBuf>,
        env: &std::collections::BTreeMap<String, String>,
    ) -> Result<String> {
        let bin = resolve(ProviderKind::Codex, binary)?;
        let dir = tempfile_dir()?;
        let last = dir.join("last.txt");
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(90),
            crate::process_tree::output(
                crate::binary::command(&bin)
                    .no_window()
                    .current_dir(cwd)
                    .args(["exec", "-s", "read-only", "--skip-git-repo-check", "--output-last-message"])
                    .arg(&last)
                    .arg(prompt)
                    .envs(env)
                    .kill_on_drop(true)
                    .stdin(std::process::Stdio::null()),
            ),
        )
        .await
        .map_err(|_| DriverError::Protocol("codex one-shot timed out".into()))??;
        if !out.status.success() {
            return Err(DriverError::Protocol(format!("codex exited with {}", out.status)));
        }
        let text = tokio::fs::read_to_string(&last).await.unwrap_or_else(|_| String::from_utf8_lossy(&out.stdout).to_string());
        let _ = tokio::fs::remove_dir_all(&dir).await;
        Ok(text.trim().to_string())
    }

    async fn list_sessions(
        &self,
        context: &crate::ProbeContext,
        cursor: Option<&str>,
        query: &str,
    ) -> Result<kybern_protocol::methods::SessionsListResult> {
        crate::sessions::list(self.kind(), context, cursor, query).await
    }

    async fn read_session(&self, context: &crate::ProbeContext, id: &str) -> Result<crate::sessions::SessionHistory> {
        crate::sessions::read(self.kind(), context, id).await
    }

    async fn open_session_reader(&self, context: &crate::ProbeContext) -> Result<Box<dyn crate::sessions::SessionReader>> {
        crate::sessions::reader(self.kind(), context).await
    }

    async fn spawn(&self, config: SessionConfig) -> Result<SpawnedSession> {
        if let Some(bridge) = config.native_tool_bridge.as_ref() {
            validate_codex_bridge(bridge)?;
        }
        let bin = resolve(ProviderKind::Codex, config.binary.as_ref())?;
        let mut cmd = crate::binary::command(&bin).no_window();
        cmd.current_dir(&config.cwd).arg("app-server");
        for (k, v) in config.env.iter().filter(|(k, _)| k.as_str() != crate::login_lend::BORROW_ENV) {
            cmd.env(k, v);
        }
        tracing::info!(bin = %bin.display(), cwd = %config.cwd.display(), "spawning codex app-server");
        let child = Arc::new(NdjsonChild::spawn(cmd)?);

        let mut lifetime = crate::ndjson::SessionLifetime::new(child.clone());
        let (tx, rx) = mpsc::channel(1024);
        let session = Arc::new(CodexSession {
            child,
            events: tx,
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            pending_approvals: Mutex::new(HashMap::new()),
            pending_app_tools: Mutex::new(HashMap::new()),
            notification_gate: Mutex::new(()),
            state: Mutex::new(State {
                manual_compaction: false,
                deferred_notifications: Vec::new(),
                thread_id: None,
                turn_id: None,
                mode: config.permission_mode,
                model: config.model.clone(),
                effort: config.effort.clone(),
                cwd: config.cwd.clone(),
                last_total_tokens: None,
                message_ids: HashMap::new(),
                file_changes: HashMap::new(),
                subagents: HashMap::new(),
                background_processes: HashMap::new(),
                stopping_processes: HashSet::new(),
            }),
            turn_usage: Mutex::new(None),
            bridge: config.native_tool_bridge.clone(),
            closed: AtomicBool::new(false),
        });
        let reader = session.clone();
        lifetime.track(tokio::spawn(async move { reader.read_loop().await }));

        session
            .call(
                "initialize",
                json!({
                    "clientInfo": { "name": "kybern", "title": "kybern", "version": env!("CARGO_PKG_VERSION") },
                    "capabilities": { "experimentalApi": true }
                }),
            )
            .await?;
        session.child.write(&json!({ "method": "initialized" })).await?;
        // A login another machine lent (design §9.4): the app-server holds the token in memory and
        // asks us for a new one when it gets a 401. Nothing is written to `auth.json`.
        if let Some(source) = crate::login_lend::source_of(&config.env) {
            // Registered first: the app-server may ask for a new token while it is still logging in.
            lent_logins().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(session.lent_key(), source.clone());
            let lent = match source.current().await {
                Ok(login) => {
                    crate::login_lend::codex_external_auth(&login).ok_or_else(|| "The broker lent a login Codex cannot use.".to_string())
                }
                Err(reason) => Err(reason),
            };
            let started = match lent {
                Ok(mut auth) => {
                    auth["type"] = Value::String("chatgptAuthTokens".into());
                    session.call("account/login/start", auth).await.map_err(|error| format!("Codex did not accept the lent login: {error}"))
                }
                Err(reason) => Err(reason),
            };
            if let Err(reason) = started {
                lent_logins().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&session.lent_key());
                session.child.close().await;
                return Err(DriverError::Protocol(reason));
            }
        }

        let (approval, sandbox) = policy_for(config.permission_mode);
        let mut params = json!({ "cwd": config.cwd, "approvalPolicy": approval, "sandbox": sandbox });
        if let Some(bridge) = config.native_tool_bridge.as_ref() {
            params["dynamicTools"] = Value::Array(
                bridge
                    .tools()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "name": tool.name,
                            "description": tool.description,
                            "inputSchema": tool.input_schema,
                        })
                    })
                    .collect(),
            );
            if let Some(instructions) = bridge.provider_instructions() {
                params["developerInstructions"] = Value::String(instructions);
            }
        }
        if let Some(m) = &config.model {
            params["model"] = Value::String(m.clone());
        }
        let (method, params) = match (&config.resume_session_id, config.fork) {
            (Some(id), false) => {
                params["threadId"] = Value::String(id.clone());
                ("thread/resume", params)
            }
            (Some(id), true) => {
                params["threadId"] = Value::String(id.clone());
                if let Some(turn) = config.rewind.as_ref().and_then(|r| r.drop_from.turn_id.clone()) {
                    params["beforeTurnId"] = Value::String(turn);
                }
                ("thread/fork", params)
            }
            (None, _) => ("thread/start", params),
        };
        let resp = session.call(method, params).await?;
        let thread_id = resp
            .pointer("/thread/id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DriverError::Protocol(format!("{method}: no thread id in response")))?
            .to_string();
        let model = resp.get("model").and_then(|m| m.as_str()).map(str::to_string);
        if let Err(error) = ensure_requested_codex_model(config.model.as_deref(), model.as_deref()) {
            session.child.close().await;
            return Err(error);
        }
        session.bind_thread(thread_id, model).await;
        let usage_session = session.clone();
        lifetime.track(tokio::spawn(async move {
            if let Ok(value) = usage_session.call("account/rateLimits/read", json!({})).await
                && let Some(limits) = parse_rate_limits(&value["rateLimits"])
            {
                usage_session.emit(DriverEvent::UsageUpdated(kybern_protocol::ProviderUsage { context: None, limits: Some(limits) })).await;
            }
        }));
        let monitor = session.clone();
        lifetime.track(tokio::spawn(async move { monitor.poll_background_terminals().await }));

        Ok(SpawnedSession { session: Box::new(Handle(session, lifetime)), events: rx })
    }
}

fn validate_codex_bridge(bridge: &NativeToolBridge) -> Result<()> {
    // The daemon may provision one session capability for every provider. The
    // Codex app-server path deliberately ignores its MCP endpoint/credential
    // and carries only the filtered definitions over dynamicTools.
    bridge.validate()?;
    if bridge.restrictions.require_enforcement {
        return Err(DriverError::Unsupported(
            "Codex app-server dynamic tools cannot enforce coordinator restrictions on built-in tools".into(),
        ));
    }
    Ok(())
}

/// (approvalPolicy, sandbox mode) for a kybern permission mode.
fn policy_for(mode: PermissionMode) -> (&'static str, &'static str) {
    match mode {
        PermissionMode::Supervised => ("untrusted", "workspace-write"),
        PermissionMode::AcceptEdits => ("on-request", "workspace-write"),
        PermissionMode::Auto => ("never", "workspace-write"),
        PermissionMode::FullAccess => ("never", "danger-full-access"),
    }
}

fn sandbox_policy_for(mode: PermissionMode, cwd: &std::path::Path) -> Value {
    match mode {
        PermissionMode::FullAccess => json!({ "type": "dangerFullAccess" }),
        _ => {
            json!({ "type": "workspaceWrite", "writableRoots": [cwd], "networkAccess": false, "excludeTmpdirEnvVar": false, "excludeSlashTmp": false })
        }
    }
}

struct State {
    manual_compaction: bool,
    deferred_notifications: Vec<(String, Value)>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    mode: PermissionMode,
    model: Option<String>,
    effort: Option<String>,
    cwd: PathBuf,
    /// Cumulative thread total at the end of the previous turn, to compute per-turn usage.
    last_total_tokens: Option<Usage>,
    /// Agent message item ids seen this turn (for completion bookkeeping).
    message_ids: HashMap<String, ()>,
    /// fileChange item id -> changes[], so the approval card can show the diff.
    file_changes: HashMap<String, Value>,
    /// Child Codex thread id -> active turn, when one is running.
    subagents: HashMap<String, Option<String>>,
    /// Process ids returned by the experimental background terminal API.
    background_processes: HashMap<String, RuntimeTaskStats>,
    stopping_processes: HashSet<String>,
}

struct CodexSession {
    notification_gate: Mutex<()>,
    child: Arc<NdjsonChild>,
    events: mpsc::Sender<DriverEvent>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<std::result::Result<Value, String>>>>,
    /// Our request key -> (server request id, kind) for approvals awaiting the user.
    pending_approvals: Mutex<HashMap<String, (Value, ApprovalKind)>>,
    /// Dynamic tool request ids are provider JSON-RPC ids rendered as strings
    /// for the daemon event. Keep the original JSON value for the response.
    pending_app_tools: Mutex<HashMap<String, PendingAppTool>>,
    state: Mutex<State>,
    /// Per-turn usage derived from thread totals, consumed at turn/completed.
    turn_usage: Mutex<Option<Usage>>,
    bridge: Option<NativeToolBridge>,
    closed: AtomicBool,
}

/// Sessions that run on a login another machine lent, by the address of their child process handle.
/// Kept beside the session instead of in it so the session keeps one shape.
fn lent_logins() -> &'static std::sync::Mutex<HashMap<usize, Arc<dyn crate::login_lend::LoginSource>>> {
    static LENT: std::sync::OnceLock<std::sync::Mutex<HashMap<usize, Arc<dyn crate::login_lend::LoginSource>>>> =
        std::sync::OnceLock::new();
    LENT.get_or_init(Default::default)
}

impl CodexSession {
    fn lent_key(&self) -> usize {
        Arc::as_ptr(&self.child) as usize
    }
}

struct PendingAppTool {
    provider_id: Value,
    turn_id: Option<String>,
}

#[derive(Clone, Copy)]
enum ApprovalKind {
    Command,
    FileChange,
    UserInput,
    Elicitation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildNotificationRoute {
    /// Fold the notification into the child's compact runtime-task lifecycle.
    Activity,
    /// Child prose and command output: forwarded to the subagent's own thread,
    /// never to the root transcript.
    Forward,
    /// Child-owned metadata nothing consumes.
    Drop,
    /// The method is not child-owned; let the normal root handler inspect it.
    Root,
}

fn child_notification_route(method: &str) -> ChildNotificationRoute {
    match method {
        "turn/started"
        | "turn/completed"
        | "thread/status/changed"
        | "thread/tokenUsage/updated"
        | "thread/closed"
        | "item/started"
        | "item/completed"
        | "error" => ChildNotificationRoute::Activity,
        "item/agentMessage/delta"
        | "item/reasoning/summaryTextDelta"
        | "item/reasoning/textDelta"
        | "item/commandExecution/outputDelta" => ChildNotificationRoute::Forward,
        "turn/plan/updated" | "thread/name/updated" => ChildNotificationRoute::Drop,
        _ => ChildNotificationRoute::Root,
    }
}

fn should_register_subagent(
    root_thread_id: Option<&str>,
    known_subagents: &HashMap<String, Option<String>>,
    thread_id: &str,
    parent_id: Option<&str>,
) -> bool {
    thread_id != root_thread_id.unwrap_or_default()
        && parent_id.is_some_and(|parent| Some(parent) == root_thread_id || known_subagents.contains_key(parent))
}

fn codex_subagent_turn_status(status: Option<&str>) -> RuntimeTaskStatus {
    match status {
        Some("interrupted") => RuntimeTaskStatus::Interrupted,
        Some("failed") => RuntimeTaskStatus::Failed,
        _ => RuntimeTaskStatus::Completed,
    }
}

struct Handle(Arc<CodexSession>, #[allow(dead_code)] crate::ndjson::SessionLifetime);

impl CodexSession {
    async fn emit(&self, ev: DriverEvent) {
        let _ = self.events.send(ev).await;
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.child.write(&json!({ "id": id, "method": method, "params": params })).await?;
        match tokio::time::timeout(std::time::Duration::from_secs(120), rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => {
                let recovery = if method == "thread/resume" && e.contains("already has an active writer") {
                    " Another Codex session owns this conversation. Let that session finish and close it, then retry. Kybern preserved the conversation and did not stop the other session."
                } else {
                    ""
                };
                Err(DriverError::Protocol(format!("{method}: {e}{recovery}")))
            }
            Ok(Err(_)) => Err(DriverError::ProcessExited("codex exited while waiting for a response".into())),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(DriverError::Protocol(format!("{method}: timed out")))
            }
        }
    }

    async fn poll_background_terminals(self: Arc<Self>) {
        loop {
            if self.closed.load(Ordering::Relaxed) {
                return;
            }
            let thread_id = match self.state.lock().await.thread_id.clone() {
                Some(thread_id) => thread_id,
                None => {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    continue;
                }
            };
            let result = match self.call("thread/backgroundTerminals/list", json!({ "threadId": thread_id, "limit": 100 })).await {
                Ok(result) => result,
                Err(error) => {
                    tracing::debug!(%error, "Codex background terminal API unavailable");
                    return;
                }
            };
            let rows = result.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut current = HashMap::new();
            let mut snapshots = Vec::new();
            for row in rows {
                let Some(process_id) = row.get("processId").and_then(Value::as_str).map(str::to_string) else { continue };
                let stats = RuntimeTaskStats {
                    token_count: None,
                    tool_uses: None,
                    duration_ms: None,
                    cpu_percent: row.get("cpuPercent").and_then(Value::as_f64),
                    rss_kb: row.get("rssKb").and_then(Value::as_u64),
                };
                current.insert(process_id.clone(), stats.clone());
                snapshots.push((process_id, row, stats));
            }
            let (previous, stopping) = {
                let mut state = self.state.lock().await;
                let previous = std::mem::replace(&mut state.background_processes, current.clone());
                (previous, state.stopping_processes.clone())
            };
            for (process_id, row, stats) in snapshots {
                let id = format!("process:{process_id}");
                if let Some(before) = previous.get(&process_id) {
                    if process_stats_changed(before, &stats) {
                        self.emit(DriverEvent::RuntimeTaskUpdated(DriverRuntimeTaskUpdate {
                            id,
                            status: Some(RuntimeTaskStatus::Running),
                            detail: None,
                            backgrounded: Some(true),
                            last_tool_name: None,
                            usage: None,
                            stats: Some(stats),
                            capabilities: None,
                        }))
                        .await;
                    }
                } else {
                    self.emit(DriverEvent::RuntimeTaskStarted(codex_background_process(&process_id, &row, stats))).await;
                }
            }
            for process_id in previous.keys().filter(|process_id| !current.contains_key(*process_id)) {
                let status = if stopping.contains(process_id) { RuntimeTaskStatus::Stopped } else { RuntimeTaskStatus::Completed };
                self.emit(DriverEvent::RuntimeTaskCompleted(DriverRuntimeTaskUpdate::status(format!("process:{process_id}"), status)))
                    .await;
                self.state.lock().await.stopping_processes.remove(process_id);
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }

    async fn respond(&self, id: &Value, result: std::result::Result<Value, String>) -> Result<()> {
        let frame = match result {
            Ok(r) => json!({ "id": id, "result": r }),
            Err(e) => json!({ "id": id, "error": { "code": -32601, "message": e } }),
        };
        self.child.write(&frame).await
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
        self.closed.store(true, Ordering::Relaxed);
        lent_logins().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&self.lent_key());
        for (_, tx) in self.pending.lock().await.drain() {
            let _ = tx.send(Err("process exited".into()));
        }
        let error = match code {
            Some(0) | None => None,
            Some(c) => Some(format!("exit code {c}")),
        };
        self.emit(DriverEvent::Exited { code, error }).await;
    }

    async fn handle_frame(&self, v: Value) {
        let has_id = v.get("id").is_some_and(|i| !i.is_null());
        let method = v.get("method").and_then(|m| m.as_str());
        match (has_id, method) {
            (true, Some(m)) => self.handle_server_request(v["id"].clone(), m, &v["params"]).await,
            (false, Some(m)) => self.handle_notification(m, &v["params"]).await,
            (true, None) => {
                let Some(id) = v["id"].as_i64() else { return };
                if let Some(tx) = self.pending.lock().await.remove(&id) {
                    let out = if let Some(err) = v.get("error") {
                        Err(err.get("message").and_then(|m| m.as_str()).unwrap_or("error").to_string())
                    } else {
                        Ok(v.get("result").cloned().unwrap_or(Value::Null))
                    };
                    let _ = tx.send(out);
                }
            }
            _ => {}
        }
    }

    async fn handle_server_request(&self, id: Value, method: &str, params: &Value) {
        let item_id = params.get("itemId").and_then(|i| i.as_str()).map(str::to_string);
        match method {
            // The app-server got a 401 on a lent login: borrow again and hand it the new token.
            "account/chatgptAuthTokens/refresh" => {
                let source = lent_logins().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(&self.lent_key()).cloned();
                let renewed = match source {
                    Some(source) => source.renew(crate::login_lend::RenewReason::Unauthorized).await,
                    None => Err("This Codex session has no lent login to renew.".to_string()),
                };
                let answer = renewed.and_then(|login| {
                    crate::login_lend::codex_external_auth(&login).ok_or_else(|| "The broker lent a login Codex cannot use.".to_string())
                });
                let _ = self.respond(&id, answer).await;
            }
            "item/tool/call" => {
                let name = params.get("tool").and_then(Value::as_str).unwrap_or("").to_string();
                let allowed = self.bridge.as_ref().is_some_and(|bridge| bridge.has_tool(&name));
                if !allowed {
                    let _ = self.respond(&id, Err(format!("kybern dynamic tool is not enabled: {name}"))).await;
                    return;
                }
                // Codex's callId is the stable application-level identifier;
                // retain the JSON-RPC id separately for the response frame.
                let request_id = params.get("callId").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| id.to_string());
                let bound_turn = self.state.lock().await.turn_id.clone();
                let turn_id = params.get("turnId").and_then(Value::as_str).map(str::to_string).or(bound_turn);
                let mut pending = self.pending_app_tools.lock().await;
                if pending.contains_key(&request_id) {
                    drop(pending);
                    let _ = self.respond(&id, Err("duplicate Kybern dynamic tool request".into())).await;
                    return;
                }
                pending.insert(request_id.clone(), PendingAppTool { provider_id: id.clone(), turn_id });
                drop(pending);
                self.emit(DriverEvent::AppToolRequest {
                    request_id,
                    name,
                    arguments: params.get("arguments").cloned().unwrap_or_else(|| json!({})),
                })
                .await;
            }
            "item/commandExecution/requestApproval" => {
                let command = params.get("command").and_then(|c| c.as_str()).unwrap_or("").to_string();
                let key = format!("cmd:{}", serde_json::to_string(&id).unwrap_or_default());
                self.pending_approvals.lock().await.insert(key.clone(), (id, ApprovalKind::Command));
                let input = json!({
                    "command": command,
                    "cwd": params.get("cwd"),
                    "reason": params.get("reason"),
                    "network": params.get("networkApprovalContext"),
                });
                let summary = if command.is_empty() {
                    match params.pointer("/networkApprovalContext/host").and_then(|h| h.as_str()) {
                        Some(host) => format!("network access to {host}"),
                        None => "run command".into(),
                    }
                } else {
                    format!("run: {}", command.lines().next().unwrap_or("").chars().take(120).collect::<String>())
                };
                self.emit(DriverEvent::PermissionRequest {
                    request_id: key,
                    tool_call_id: item_id,
                    tool_name: "shell".into(),
                    input,
                    summary,
                    suggestions: vec![],
                })
                .await;
            }
            "item/fileChange/requestApproval" => {
                let mode = self.state.lock().await.mode;
                if matches!(mode, PermissionMode::AcceptEdits | PermissionMode::Auto | PermissionMode::FullAccess) {
                    let _ = self.respond(&id, Ok(json!({ "decision": "accept" }))).await;
                    return;
                }
                let key = format!("patch:{}", serde_json::to_string(&id).unwrap_or_default());
                self.pending_approvals.lock().await.insert(key.clone(), (id, ApprovalKind::FileChange));
                let changes = match &item_id {
                    Some(i) => self.state.lock().await.file_changes.get(i).cloned().unwrap_or(Value::Null),
                    None => Value::Null,
                };
                let paths: Vec<String> = changes
                    .as_array()
                    .map(|a| a.iter().filter_map(|c| c.get("path").and_then(|p| p.as_str()).map(str::to_string)).collect())
                    .unwrap_or_default();
                let summary = if paths.is_empty() { "apply file changes".to_string() } else { format!("edit: {}", paths.join(", ")) };
                let input = json!({ "changes": changes, "reason": params.get("reason"), "grant_root": params.get("grantRoot") });
                self.emit(DriverEvent::PermissionRequest {
                    request_id: key,
                    tool_call_id: item_id,
                    tool_name: "apply_patch".into(),
                    input,
                    summary,
                    suggestions: vec![],
                })
                .await;
            }
            "item/tool/requestUserInput" | "mcpServer/elicitation/request" => {
                let elicitation = method == "mcpServer/elicitation/request";
                let request_id = id.to_string();
                self.pending_approvals.lock().await.insert(
                    request_id.clone(),
                    (id.clone(), if elicitation { ApprovalKind::Elicitation } else { ApprovalKind::UserInput }),
                );
                self.emit(DriverEvent::PermissionRequest {
                    request_id,
                    tool_call_id: params.get("itemId").and_then(Value::as_str).map(str::to_owned),
                    tool_name: if elicitation { "mcp_elicitation" } else { "request_user_input" }.into(),
                    input: params.clone(),
                    summary: params.get("message").and_then(Value::as_str).unwrap_or("Answer the agent's questions").into(),
                    suggestions: vec![],
                })
                .await;
            }
            other => {
                let _ = self.respond(&id, Err(format!("kybern does not support {other}"))).await;
            }
        }
    }

    async fn handle_thread_started(&self, thread: &Value) {
        let Some(thread_id) = thread.get("id").and_then(Value::as_str).map(str::to_string) else { return };
        let Some(spawn) = thread.pointer("/source/subAgent/thread_spawn").or_else(|| thread.pointer("/source/subAgent/threadSpawn")) else {
            return;
        };
        let parent_id = spawn.get("parent_thread_id").or_else(|| spawn.get("parentThreadId")).and_then(Value::as_str).map(str::to_string);
        let known_parent = {
            let state = self.state.lock().await;
            should_register_subagent(state.thread_id.as_deref(), &state.subagents, &thread_id, parent_id.as_deref())
        };
        if !known_parent {
            return;
        }
        let title = spawn
            .get("agent_nickname")
            .or_else(|| spawn.get("agentNickname"))
            .or_else(|| spawn.get("agent_role"))
            .or_else(|| spawn.get("agentRole"))
            .and_then(Value::as_str)
            .or_else(|| thread.get("name").and_then(Value::as_str))
            .unwrap_or("Subagent")
            .to_string();
        let provider_type = spawn.get("agent_role").or_else(|| spawn.get("agentRole")).and_then(Value::as_str).map(str::to_string);
        let is_new = {
            let mut state = self.state.lock().await;
            if let std::collections::hash_map::Entry::Vacant(entry) = state.subagents.entry(thread_id.clone()) {
                entry.insert(None);
                true
            } else {
                false
            }
        };
        if is_new {
            self.emit(DriverEvent::RuntimeTaskStarted(DriverRuntimeTask {
                id: thread_id.clone(),
                kind: RuntimeTaskKind::Agent,
                status: RuntimeTaskStatus::Pending,
                title,
                detail: None,
                provider_type,
                parent_id,
                tool_call_id: None,
                provider_thread_id: Some(thread_id),
                model: thread.get("model").and_then(Value::as_str).map(str::to_string),
                effort: None,
                backgrounded: true,
                last_tool_name: None,
                usage: None,
                stats: RuntimeTaskStats::default(),
                capabilities: RuntimeTaskCapabilities::default(),
            }))
            .await;
        }
    }

    async fn ensure_subagent(&self, thread_id: &str) -> bool {
        let is_new = {
            let mut state = self.state.lock().await;
            if state.thread_id.as_deref().is_none_or(|root| root == thread_id) {
                return false;
            }
            if let std::collections::hash_map::Entry::Vacant(entry) = state.subagents.entry(thread_id.to_string()) {
                entry.insert(None);
                true
            } else {
                false
            }
        };
        if is_new {
            self.emit(DriverEvent::RuntimeTaskStarted(DriverRuntimeTask {
                id: thread_id.to_string(),
                kind: RuntimeTaskKind::Agent,
                status: RuntimeTaskStatus::Pending,
                title: "Subagent".into(),
                detail: None,
                provider_type: Some("sub_agent".into()),
                parent_id: None,
                tool_call_id: None,
                provider_thread_id: Some(thread_id.to_string()),
                model: None,
                effort: None,
                backgrounded: true,
                last_tool_name: None,
                usage: None,
                stats: RuntimeTaskStats::default(),
                capabilities: RuntimeTaskCapabilities::default(),
            }))
            .await;
        }
        true
    }

    async fn handle_subagent_notification(&self, thread_id: &str, method: &str, p: &Value) {
        if !self.ensure_subagent(thread_id).await {
            return;
        }
        match method {
            "turn/started" => {
                let turn_id = p.pointer("/turn/id").and_then(Value::as_str).map(str::to_string);
                self.state.lock().await.subagents.insert(thread_id.to_string(), turn_id);
                self.emit(DriverEvent::RuntimeTaskResumed(DriverRuntimeTaskUpdate {
                    id: thread_id.to_string(),
                    status: Some(RuntimeTaskStatus::Running),
                    detail: None,
                    backgrounded: None,
                    last_tool_name: None,
                    usage: None,
                    stats: None,
                    capabilities: Some(RuntimeTaskCapabilities { stop: true, background: false }),
                }))
                .await;
            }
            "item/started" | "item/completed" => {
                let item = &p["item"];
                let tool = codex_item_label(item);
                let detail = codex_item_detail(item);
                self.emit(DriverEvent::RuntimeTaskUpdated(DriverRuntimeTaskUpdate {
                    id: thread_id.to_string(),
                    status: Some(RuntimeTaskStatus::Running),
                    detail,
                    backgrounded: None,
                    last_tool_name: tool,
                    usage: None,
                    stats: None,
                    capabilities: Some(RuntimeTaskCapabilities { stop: true, background: false }),
                }))
                .await;
                // The subagent's own messages and tool calls go to its thread.
                self.handle_item_for(Some(thread_id), item, method == "item/completed").await;
            }
            "thread/tokenUsage/updated" => {
                let usage = p.pointer("/tokenUsage/total").map(parse_usage);
                self.emit(DriverEvent::RuntimeTaskUpdated(DriverRuntimeTaskUpdate {
                    id: thread_id.to_string(),
                    status: None,
                    detail: None,
                    backgrounded: None,
                    last_tool_name: None,
                    usage,
                    stats: None,
                    capabilities: None,
                }))
                .await;
            }
            "turn/completed" => {
                self.state.lock().await.subagents.insert(thread_id.to_string(), None);
                let status = codex_subagent_turn_status(p.pointer("/turn/status").and_then(Value::as_str));
                self.emit(DriverEvent::RuntimeTaskCompleted(DriverRuntimeTaskUpdate {
                    id: thread_id.to_string(),
                    status: Some(status),
                    detail: p.pointer("/turn/error/message").and_then(Value::as_str).map(str::to_string),
                    backgrounded: None,
                    last_tool_name: None,
                    usage: None,
                    stats: None,
                    capabilities: Some(RuntimeTaskCapabilities::default()),
                }))
                .await;
            }
            "thread/status/changed" => {
                let status = p.pointer("/status/type").and_then(Value::as_str).unwrap_or("");
                let mapped = match status {
                    "active" => RuntimeTaskStatus::Running,
                    "systemError" => RuntimeTaskStatus::Failed,
                    _ => RuntimeTaskStatus::Waiting,
                };
                let terminal = !mapped.is_active();
                let update = DriverRuntimeTaskUpdate {
                    id: thread_id.to_string(),
                    status: Some(mapped),
                    detail: None,
                    backgrounded: None,
                    last_tool_name: None,
                    usage: None,
                    stats: None,
                    capabilities: Some(RuntimeTaskCapabilities { stop: mapped == RuntimeTaskStatus::Running, background: false }),
                };
                self.emit(if terminal { DriverEvent::RuntimeTaskCompleted(update) } else { DriverEvent::RuntimeTaskUpdated(update) }).await;
            }
            "thread/closed" => {
                self.state.lock().await.subagents.remove(thread_id);
                self.emit(DriverEvent::RuntimeTaskCompleted(DriverRuntimeTaskUpdate {
                    id: thread_id.to_string(),
                    status: Some(RuntimeTaskStatus::Completed),
                    detail: None,
                    backgrounded: None,
                    last_tool_name: None,
                    usage: None,
                    stats: None,
                    capabilities: Some(RuntimeTaskCapabilities::default()),
                }))
                .await;
            }
            "error" => {
                let message = p.pointer("/error/message").and_then(Value::as_str).unwrap_or("Subagent failed").to_string();
                let retrying = p.get("willRetry").and_then(Value::as_bool).unwrap_or(false);
                let update = DriverRuntimeTaskUpdate {
                    id: thread_id.to_string(),
                    status: Some(if retrying { RuntimeTaskStatus::Running } else { RuntimeTaskStatus::Failed }),
                    detail: Some(if retrying { format!("{message} (retrying)") } else { message }),
                    backgrounded: None,
                    last_tool_name: None,
                    usage: None,
                    stats: None,
                    capabilities: Some(RuntimeTaskCapabilities { stop: retrying, background: false }),
                };
                self.emit(if retrying { DriverEvent::RuntimeTaskUpdated(update) } else { DriverEvent::RuntimeTaskCompleted(update) }).await;
            }
            _ => {}
        }
    }

    // Serialize binding with notifications so early root events are replayed in
    // wire order only after the start/resume/fork response identifies the root.
    async fn bind_thread(&self, thread_id: String, model: Option<String>) {
        let _gate = self.notification_gate.lock().await;
        let deferred = {
            let mut state = self.state.lock().await;
            state.thread_id = Some(thread_id.clone());
            if model.is_some() {
                state.model = model.clone();
            }
            std::mem::take(&mut state.deferred_notifications)
        };
        self.emit(DriverEvent::SessionBound { session_id: thread_id, model }).await;
        for (method, params) in deferred {
            self.handle_bound_notification(&method, &params).await;
        }
    }

    async fn handle_notification(&self, method: &str, p: &Value) {
        let _gate = self.notification_gate.lock().await;
        {
            let mut state = self.state.lock().await;
            if state.thread_id.is_none() {
                state.deferred_notifications.push((method.to_string(), p.clone()));
                return;
            }
        }
        self.handle_bound_notification(method, p).await;
    }

    async fn handle_bound_notification(&self, method: &str, p: &Value) {
        if method == "thread/started" {
            self.handle_thread_started(&p["thread"]).await;
            return;
        }
        let root_thread_id = self.state.lock().await.thread_id.clone();
        if let Some(thread_id) = p.get("threadId").and_then(Value::as_str)
            && root_thread_id.as_deref() != Some(thread_id)
        {
            match child_notification_route(method) {
                ChildNotificationRoute::Activity => {
                    self.handle_subagent_notification(thread_id, method, p).await;
                    return;
                }
                ChildNotificationRoute::Forward => {
                    self.forward_child_delta(thread_id, method, p).await;
                    return;
                }
                ChildNotificationRoute::Drop => return,
                ChildNotificationRoute::Root => {}
            }
        }
        match method {
            "account/rateLimits/updated" => {
                if let Some(limits) = parse_rate_limits(&p["rateLimits"]) {
                    self.emit(DriverEvent::UsageUpdated(kybern_protocol::ProviderUsage { context: None, limits: Some(limits) })).await;
                }
            }
            "turn/started" => {
                if let Some(id) = p.pointer("/turn/id").and_then(|i| i.as_str()) {
                    self.state.lock().await.turn_id = Some(id.to_string());
                }
            }
            "item/started" => self.handle_item(&p["item"], false).await,
            "item/completed" => self.handle_item(&p["item"], true).await,
            "item/agentMessage/delta" => {
                if let (Some(id), Some(delta)) = (p.get("itemId").and_then(|i| i.as_str()), p.get("delta").and_then(|d| d.as_str())) {
                    self.emit(DriverEvent::TextDelta { message_id: id.to_string(), origin: EventOrigin::Root, delta: delta.to_string() })
                        .await;
                }
            }
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                if let (Some(id), Some(delta)) = (p.get("itemId").and_then(|i| i.as_str()), p.get("delta").and_then(|d| d.as_str())) {
                    // Attribute reasoning to the turn's current message stream under the reasoning item id.
                    self.emit(DriverEvent::ThinkingDelta {
                        message_id: id.to_string(),
                        origin: EventOrigin::Root,
                        delta: delta.to_string(),
                    })
                    .await;
                }
            }
            "item/commandExecution/outputDelta" => {
                if let (Some(id), Some(delta)) = (p.get("itemId").and_then(|i| i.as_str()), p.get("delta").and_then(|d| d.as_str())) {
                    self.emit(DriverEvent::ToolOutputDelta { tool_call_id: id.to_string(), delta: delta.to_string() }).await;
                }
            }
            "turn/plan/updated" => {
                let steps = p.get("plan").and_then(|s| s.as_array()).map(|a| a.len()).unwrap_or(0);
                self.emit(DriverEvent::Notice {
                    level: NoticeLevel::Info,
                    text: format!("plan updated ({steps} steps)"),
                    data: Some(p.clone()),
                })
                .await;
            }
            "thread/tokenUsage/updated" => {
                if let (Some(used_tokens), Some(window_tokens)) = (
                    p.pointer("/tokenUsage/last/totalTokens").and_then(Value::as_u64),
                    p.pointer("/tokenUsage/modelContextWindow").and_then(Value::as_u64),
                ) {
                    self.emit(DriverEvent::UsageUpdated(kybern_protocol::ProviderUsage {
                        context: Some(kybern_protocol::ContextUsage { used_tokens, window_tokens }),
                        limits: None,
                    }))
                    .await;
                }

                let total = p.pointer("/tokenUsage/total").map(parse_usage).unwrap_or_default();
                let mut st = self.state.lock().await;
                let per_turn = match &st.last_total_tokens {
                    Some(prev) => Usage {
                        input_tokens: total.input_tokens.saturating_sub(prev.input_tokens),
                        output_tokens: total.output_tokens.saturating_sub(prev.output_tokens),
                        cache_read_tokens: total.cache_read_tokens.saturating_sub(prev.cache_read_tokens),
                        cache_write_tokens: total.cache_write_tokens.saturating_sub(prev.cache_write_tokens),
                    },
                    None if st.manual_compaction => Usage::default(),
                    None => total.clone(),
                };
                st.last_total_tokens = Some(total);
                drop(st);
                *self.turn_usage.lock().await = Some(per_turn);
            }
            "turn/completed" => {
                let turn = &p["turn"];
                let status = turn.get("status").and_then(|s| s.as_str()).unwrap_or("completed");
                let duration_ms = turn.get("durationMs").and_then(|d| d.as_u64()).unwrap_or(0);
                let usage = self.turn_usage.lock().await.take().unwrap_or_default();
                let anchors =
                    crate::TurnAnchors { turn_id: turn.get("id").and_then(|i| i.as_str()).map(str::to_string), previous_end: None };
                {
                    let mut st = self.state.lock().await;
                    st.turn_id = None;
                    st.manual_compaction = false;
                    st.message_ids.clear();
                    st.file_changes.clear();
                }
                self.pending_approvals.lock().await.clear();
                let ev = match status {
                    "completed" => {
                        DriverEvent::TurnCompleted { stop_reason: StopReason::Completed, usage, cost_usd: None, duration_ms, anchors }
                    }
                    "interrupted" => {
                        DriverEvent::TurnCompleted { stop_reason: StopReason::Interrupted, usage, cost_usd: None, duration_ms, anchors }
                    }
                    _ => DriverEvent::TurnFailed {
                        error: turn.pointer("/error/message").and_then(|m| m.as_str()).unwrap_or("turn failed").to_string(),
                    },
                };
                self.emit(ev).await;
            }
            "error" => {
                let msg = p.pointer("/error/message").and_then(|m| m.as_str()).unwrap_or("error").to_string();
                let retry = p.get("willRetry").and_then(|r| r.as_bool()).unwrap_or(false);
                self.emit(DriverEvent::Notice {
                    level: if retry { NoticeLevel::Warning } else { NoticeLevel::Error },
                    text: if retry { format!("{msg} (retrying)") } else { msg },
                    data: Some(p.clone()),
                })
                .await;
            }
            "warning" | "configWarning" | "deprecationNotice" => {
                let text = p.get("message").or_else(|| p.get("summary")).and_then(|m| m.as_str()).unwrap_or("").to_string();
                if !text.is_empty() {
                    self.emit(DriverEvent::Notice { level: NoticeLevel::Warning, text, data: None }).await;
                }
            }
            "serverRequest/resolved" => {
                // A pending approval was resolved elsewhere (interrupt). Drop it if still pending.
                let rid = p.get("requestId").cloned().unwrap_or(Value::Null);
                let mut pend = self.pending_approvals.lock().await;
                if let Some(key) = pend.iter().find(|(_, (id, _))| *id == rid).map(|(k, _)| k.clone()) {
                    pend.remove(&key);
                    drop(pend);
                    self.emit(DriverEvent::PermissionWithdrawn { request_id: key }).await;
                }
            }
            "thread/settings/updated" => {
                if let Some(model) = p.pointer("/threadSettings/model").and_then(|m| m.as_str()) {
                    let mut st = self.state.lock().await;
                    if st.model.as_deref() != Some(model) {
                        st.model = Some(model.to_string());
                        let tid = st.thread_id.clone().unwrap_or_default();
                        drop(st);
                        self.emit(DriverEvent::SessionBound { session_id: tid, model: Some(model.to_string()) }).await;
                    }
                }
            }
            _ => tracing::trace!(method, "ignored codex notification"),
        }
    }

    async fn handle_item(&self, item: &Value, completed: bool) {
        self.handle_item_for(None, item, completed).await;
    }

    /// Emit an item event for the root, or for the subagent thread `owner`
    /// when the item belongs to one.
    async fn emit_for(&self, owner: Option<&str>, event: DriverEvent) {
        match owner {
            Some(task_id) => self.emit(DriverEvent::SubagentOnly { task_id: task_id.to_string(), event: Box::new(event) }).await,
            None => self.emit(event).await,
        }
    }

    /// Streamed text, reasoning and command output of a subagent.
    async fn forward_child_delta(&self, thread_id: &str, method: &str, p: &Value) {
        let (Some(id), Some(delta)) = (p.get("itemId").and_then(Value::as_str), p.get("delta").and_then(Value::as_str)) else { return };
        let (message_id, delta) = (id.to_string(), delta.to_string());
        let event = match method {
            "item/agentMessage/delta" => DriverEvent::TextDelta { message_id, origin: EventOrigin::Root, delta },
            "item/commandExecution/outputDelta" => DriverEvent::ToolOutputDelta { tool_call_id: message_id, delta },
            _ => DriverEvent::ThinkingDelta { message_id, origin: EventOrigin::Root, delta },
        };
        self.emit_for(Some(thread_id), event).await;
    }

    /// Fold one thread item into driver events. `owner` is the subagent thread
    /// the item belongs to; its content then goes to that subagent's thread
    /// only, and runtime-task side effects are skipped.
    async fn handle_item_for(&self, owner: Option<&str>, item: &Value, completed: bool) {
        let ty = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let id = item.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
        match ty {
            "userMessage" => {
                // A subagent's first input is the prompt it was launched with.
                if let Some(task_id) = owner {
                    let prompt = item
                        .get("content")
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts.iter().filter_map(|part| part.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n")
                        })
                        .unwrap_or_default();
                    if !prompt.trim().is_empty() {
                        self.emit(DriverEvent::SubagentPrompt { task_id: task_id.to_string(), prompt }).await;
                    }
                }
            }
            "agentMessage" => {
                if completed {
                    if owner.is_none()
                        && item.get("delivery").and_then(Value::as_str) == Some("async")
                        && let Some(questions) = item
                            .get("questions")
                            .and_then(|v| serde_json::from_value::<Vec<kybern_protocol::AsyncQuestion>>(v.clone()).ok())
                        && !id.is_empty()
                        && !questions.is_empty()
                    {
                        self.emit(DriverEvent::AsyncQuestions(kybern_protocol::AsyncQuestionRequest { id: id.clone(), questions })).await;
                    }
                    let text = item.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
                    self.emit_for(owner, DriverEvent::MessageCompleted { message_id: id, origin: EventOrigin::Root, text, thinking: None })
                        .await;
                }
            }
            "reasoning" => {
                if completed {
                    self.emit_for(owner, DriverEvent::ThinkingCompleted { message_id: id.clone(), origin: EventOrigin::Root }).await;
                    if owner.is_some() {
                        return;
                    }
                    let summary = item
                        .get("summary")
                        .and_then(|s| s.as_array())
                        .map(|a| a.iter().filter_map(|s| s.as_str()).collect::<Vec<_>>().join("\n\n"))
                        .unwrap_or_default();
                    if !summary.is_empty() {
                        // Reasoning belongs to the next agent message; surfaced as a notice with data for the UI.
                        self.emit(DriverEvent::Notice {
                            level: NoticeLevel::Info,
                            text: "reasoning".into(),
                            data: Some(json!({ "reasoning": summary, "item_id": id })),
                        })
                        .await;
                    }
                }
            }
            "commandExecution" => {
                if !completed {
                    self.emit_for(
                        owner,
                        DriverEvent::ToolStarted(ToolCall {
                            id,
                            name: "shell".into(),
                            input: json!({ "command": item.get("command"), "cwd": item.get("cwd"), "actions": item.get("commandActions") }),
                            parent_id: None,
                        }),
                    )
                    .await;
                } else {
                    let status = item.get("status").and_then(|s| s.as_str()).unwrap_or("");
                    self.emit_for(owner, DriverEvent::ToolCompleted {
                        tool_call_id: id.clone(),
                        output: json!({ "content": item.get("aggregatedOutput"), "exit_code": item.get("exitCode"), "status": status, "duration_ms": item.get("durationMs") }),
                        is_error: matches!(status, "failed" | "declined"),
                    })
                    .await;
                    if status == "inProgress"
                        && owner.is_none()
                        && let Some(process_id) = item.get("processId").and_then(Value::as_str)
                    {
                        let stats = RuntimeTaskStats { duration_ms: item.get("durationMs").and_then(Value::as_u64), ..Default::default() };
                        self.state.lock().await.background_processes.insert(process_id.to_string(), stats.clone());
                        self.emit(DriverEvent::RuntimeTaskStarted(codex_background_process(
                            process_id,
                            &json!({
                                "command": item.get("command"),
                                "cwd": item.get("cwd"),
                                "itemId": id,
                            }),
                            stats,
                        )))
                        .await;
                    }
                }
            }
            "fileChange" => {
                let changes = item.get("changes").cloned().unwrap_or(Value::Array(vec![]));
                if !completed {
                    self.state.lock().await.file_changes.insert(id.clone(), changes.clone());
                    self.emit_for(
                        owner,
                        DriverEvent::ToolStarted(ToolCall {
                            id,
                            name: "apply_patch".into(),
                            input: json!({ "changes": changes }),
                            parent_id: None,
                        }),
                    )
                    .await;
                } else {
                    let status = item.get("status").and_then(|s| s.as_str()).unwrap_or("");
                    self.emit_for(
                        owner,
                        DriverEvent::ToolCompleted {
                            tool_call_id: id,
                            output: json!({ "status": status, "changes": changes }),
                            is_error: matches!(status, "failed" | "declined"),
                        },
                    )
                    .await;
                }
            }
            "mcpToolCall" => {
                let name = format!(
                    "mcp:{}/{}",
                    item.get("server").and_then(|s| s.as_str()).unwrap_or(""),
                    item.get("tool").and_then(|s| s.as_str()).unwrap_or("")
                );
                if !completed {
                    self.emit_for(
                        owner,
                        DriverEvent::ToolStarted(ToolCall {
                            id,
                            name,
                            input: item.get("arguments").cloned().unwrap_or(Value::Null),
                            parent_id: None,
                        }),
                    )
                    .await;
                } else {
                    let status = item.get("status").and_then(|s| s.as_str()).unwrap_or("");
                    self.emit_for(
                        owner,
                        DriverEvent::ToolCompleted {
                            tool_call_id: id,
                            output: json!({ "result": item.get("result"), "error": item.get("error"), "status": status }),
                            is_error: status == "failed" || !item.get("error").is_none_or(Value::is_null),
                        },
                    )
                    .await;
                }
            }
            "webSearch" => {
                if !completed {
                    self.emit_for(
                        owner,
                        DriverEvent::ToolStarted(ToolCall {
                            id,
                            name: "web_search".into(),
                            input: json!({ "query": item.get("query"), "action": item.get("action") }),
                            parent_id: None,
                        }),
                    )
                    .await;
                } else {
                    self.emit_for(
                        owner,
                        DriverEvent::ToolCompleted { tool_call_id: id, output: json!({ "results": item.get("results") }), is_error: false },
                    )
                    .await;
                }
            }
            "dynamicToolCall" => {
                let name = item.get("tool").and_then(Value::as_str).unwrap_or("dynamic_tool").to_string();
                if !completed {
                    self.emit_for(
                        owner,
                        DriverEvent::ToolStarted(ToolCall {
                            id,
                            name,
                            input: item.get("arguments").cloned().unwrap_or(Value::Null),
                            parent_id: None,
                        }),
                    )
                    .await;
                } else {
                    let status = item.get("status").and_then(Value::as_str).unwrap_or("");
                    let is_error = status == "failed" || item.get("success").and_then(Value::as_bool) == Some(false);
                    self.emit_for(owner, DriverEvent::ToolCompleted { tool_call_id: id, output: item.clone(), is_error }).await;
                }
            }
            "imageView" | "imageGeneration" => {
                if !completed {
                    self.emit_for(
                        owner,
                        DriverEvent::ToolStarted(ToolCall { id, name: ty.to_string(), input: item.clone(), parent_id: None }),
                    )
                    .await;
                } else {
                    self.emit_for(owner, DriverEvent::ToolCompleted { tool_call_id: id, output: item.clone(), is_error: false }).await;
                }
            }
            "collabAgentToolCall" => {
                let receiver_ids = item
                    .get("receiverThreadIds")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                // The spawn call carries the prompt each new subagent starts with.
                if item.get("tool").and_then(Value::as_str) == Some("spawnAgent")
                    && let Some(prompt) = item.get("prompt").and_then(Value::as_str).filter(|prompt| !prompt.trim().is_empty())
                {
                    for thread_id in &receiver_ids {
                        self.emit(DriverEvent::SubagentPrompt { task_id: thread_id.clone(), prompt: prompt.to_string() }).await;
                    }
                }
                for thread_id in receiver_ids {
                    self.ensure_subagent(&thread_id).await;
                }
                if let Some(states) = item.get("agentsStates").and_then(Value::as_object) {
                    for (thread_id, state) in states {
                        if !self.ensure_subagent(thread_id).await {
                            continue;
                        }
                        let status = codex_agent_status(state.get("status").and_then(Value::as_str).unwrap_or("running"));
                        let update = DriverRuntimeTaskUpdate {
                            id: thread_id.clone(),
                            status: Some(status),
                            detail: state.get("message").and_then(Value::as_str).map(str::to_string),
                            backgrounded: None,
                            last_tool_name: None,
                            usage: None,
                            stats: None,
                            capabilities: Some(RuntimeTaskCapabilities { stop: status == RuntimeTaskStatus::Running, background: false }),
                        };
                        self.emit(if status.is_active() {
                            DriverEvent::RuntimeTaskUpdated(update)
                        } else {
                            DriverEvent::RuntimeTaskCompleted(update)
                        })
                        .await;
                    }
                }
            }
            "subAgentActivity" => {
                let Some(thread_id) = item.get("agentThreadId").and_then(Value::as_str) else { return };
                if !self.ensure_subagent(thread_id).await {
                    return;
                }
                let status = match item.get("kind").and_then(Value::as_str).unwrap_or("started") {
                    "interrupted" => RuntimeTaskStatus::Interrupted,
                    "completed" => RuntimeTaskStatus::Completed,
                    _ => RuntimeTaskStatus::Running,
                };
                let update = DriverRuntimeTaskUpdate {
                    id: thread_id.to_string(),
                    status: Some(status),
                    detail: None,
                    backgrounded: None,
                    last_tool_name: None,
                    usage: None,
                    stats: None,
                    capabilities: Some(RuntimeTaskCapabilities { stop: status == RuntimeTaskStatus::Running, background: false }),
                };
                self.emit(if status.is_active() {
                    DriverEvent::RuntimeTaskUpdated(update)
                } else {
                    DriverEvent::RuntimeTaskCompleted(update)
                })
                .await;
            }
            "contextCompaction" => {
                self.emit(DriverEvent::Notice {
                    level: NoticeLevel::Info,
                    text: if completed { "Context compacted" } else { "Compacting context…" }.into(),
                    data: None,
                })
                .await;
            }
            _ => {}
        }
    }
}

fn codex_background_process(process_id: &str, value: &Value, stats: RuntimeTaskStats) -> DriverRuntimeTask {
    let command = value
        .get("command")
        .and_then(|command| {
            command
                .as_str()
                .map(str::to_string)
                .or_else(|| command.as_array().map(|parts| parts.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")))
        })
        .filter(|command| !command.trim().is_empty())
        .unwrap_or_else(|| "Background process".into());
    DriverRuntimeTask {
        id: format!("process:{process_id}"),
        kind: RuntimeTaskKind::Process,
        status: RuntimeTaskStatus::Running,
        title: command.lines().next().unwrap_or(&command).chars().take(120).collect(),
        detail: value.get("cwd").and_then(Value::as_str).map(str::to_string),
        provider_type: Some("background_terminal".into()),
        parent_id: None,
        tool_call_id: value.get("itemId").and_then(Value::as_str).map(str::to_string),
        provider_thread_id: None,
        model: None,
        effort: None,
        backgrounded: true,
        last_tool_name: Some("shell".into()),
        usage: None,
        stats,
        capabilities: RuntimeTaskCapabilities { stop: true, background: false },
    }
}

fn process_stats_changed(before: &RuntimeTaskStats, after: &RuntimeTaskStats) -> bool {
    let cpu_changed = match (before.cpu_percent, after.cpu_percent) {
        (Some(before), Some(after)) => (before - after).abs() >= 1.0,
        (before, after) => before != after,
    };
    let memory_changed = match (before.rss_kb, after.rss_kb) {
        (Some(before), Some(after)) => before.abs_diff(after) >= 1024,
        (before, after) => before != after,
    };
    cpu_changed || memory_changed
}

fn codex_agent_status(status: &str) -> RuntimeTaskStatus {
    match status {
        "pendingInit" => RuntimeTaskStatus::Pending,
        "running" => RuntimeTaskStatus::Running,
        "interrupted" => RuntimeTaskStatus::Interrupted,
        "completed" | "shutdown" => RuntimeTaskStatus::Completed,
        "errored" | "notFound" => RuntimeTaskStatus::Failed,
        _ => RuntimeTaskStatus::Waiting,
    }
}

fn codex_item_label(item: &Value) -> Option<String> {
    match item.get("type").and_then(Value::as_str)? {
        "commandExecution" => Some("shell".into()),
        "fileChange" => Some("apply_patch".into()),
        "mcpToolCall" => Some(format!(
            "mcp:{}/{}",
            item.get("server").and_then(Value::as_str).unwrap_or(""),
            item.get("tool").and_then(Value::as_str).unwrap_or("")
        )),
        "webSearch" => Some("web_search".into()),
        "imageView" => Some("image_view".into()),
        "imageGeneration" => Some("image_generation".into()),
        "dynamicToolCall" => item.get("tool").and_then(Value::as_str).map(str::to_string).or_else(|| Some("tool".into())),
        _ => None,
    }
}

fn codex_item_detail(item: &Value) -> Option<String> {
    item.get("command")
        .or_else(|| item.get("query"))
        .or_else(|| item.get("tool"))
        .and_then(Value::as_str)
        .map(|detail| detail.lines().next().unwrap_or(detail).chars().take(160).collect())
}

fn tempfile_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("kybern-codex-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn parse_usage(u: &Value) -> Usage {
    let g = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    Usage {
        input_tokens: g("inputTokens"),
        output_tokens: g("outputTokens"),
        cache_read_tokens: g("cachedInputTokens"),
        cache_write_tokens: g("cacheWriteInputTokens"),
    }
}

fn input_items(message: &UserMessage) -> Vec<Value> {
    let mut items = Vec::new();
    for part in &message.parts {
        match part {
            ContentPart::Text { text } => items.push(json!({ "type": "text", "text": text, "text_elements": [] })),
            ContentPart::FileMention { path } => items.push(json!({ "type": "text", "text": format!("@{path}"), "text_elements": [] })),
            ContentPart::ThreadReference { thread_id, title, .. } => {
                items.push(json!({ "type": "text", "text": thread_reference_text(*thread_id, title), "text_elements": [] }))
            }
            ContentPart::Skill { name, path } => {
                // App-server currently expects both the visible `$skill` text
                // and the structured item that identifies the canonical file.
                items.push(json!({ "type": "text", "text": format!("${name}"), "text_elements": [] }));
                items.push(json!({ "type": "skill", "name": name, "path": path }));
            }
            ContentPart::Mention { name, path, display_name } => {
                // Plugins and apps: the `@name` token in the text plus a
                // `mention` item carrying the exact `plugin://` or `app://` id.
                items.push(json!({ "type": "text", "text": format!("@{name}"), "text_elements": [] }));
                items.push(json!({ "type": "mention", "name": display_name.as_deref().unwrap_or(name), "path": path }));
            }
            ContentPart::Image { media_type, data } => {
                if base64::engine::general_purpose::STANDARD.decode(data).is_ok() {
                    items.push(json!({ "type": "image", "url": format!("data:{media_type};base64,{data}"), "detail": "auto" }));
                }
            }
            ContentPart::Attachment { name, .. } => {
                items.push(json!({ "type": "text", "text": format!("[attached file: {name}]"), "text_elements": [] }))
            }
            ContentPart::ThreadMessage { .. } | ContentPart::AgentResults { .. } | ContentPart::SystemRow { .. } => {
                if let Some(text) = part.orchestration_text() {
                    items.push(json!({ "type": "text", "text": text, "text_elements": [] }));
                }
            }
        }
    }
    items
}

#[async_trait]
impl AgentSession for Handle {
    async fn compact(&self) -> Result<()> {
        let thread_id = self.0.state.lock().await.thread_id.clone().ok_or_else(|| DriverError::Protocol("no Codex thread".into()))?;
        self.0.state.lock().await.manual_compaction = true;
        *self.0.turn_usage.lock().await = None;
        let result = self.0.call("thread/compact/start", json!({ "threadId": thread_id })).await;
        if result.is_err() {
            self.0.state.lock().await.manual_compaction = false;
        }
        result.map(|_| ())
    }

    async fn send_message(&self, message_id: &str, message: &UserMessage) -> Result<()> {
        let s = &self.0;
        let (thread_id, mode, model, effort, cwd) = {
            let st = s.state.lock().await;
            (
                st.thread_id.clone().ok_or_else(|| DriverError::Protocol("no codex thread".into()))?,
                st.mode,
                st.model.clone(),
                st.effort.clone(),
                st.cwd.clone(),
            )
        };
        let (approval, _) = policy_for(mode);
        let mut params = json!({
            "threadId": thread_id,
            "clientUserMessageId": message_id,
            "input": input_items(message),
            "approvalPolicy": approval,
            "sandboxPolicy": sandbox_policy_for(mode, &cwd),
        });
        if let Some(m) = model {
            params["model"] = Value::String(m);
        }
        if let Some(effort) = effort {
            params["effort"] = Value::String(effort);
        }
        let resp = s.call("turn/start", params).await?;
        if let Some(id) = resp.pointer("/turn/id").and_then(|i| i.as_str()) {
            s.state.lock().await.turn_id = Some(id.to_string());
        }
        Ok(())
    }

    async fn steer(&self, message_id: &str, message: &UserMessage) -> Result<()> {
        let (thread_id, turn_id) = {
            let state = self.0.state.lock().await;
            (state.thread_id.clone(), state.turn_id.clone())
        };
        let (Some(thread_id), Some(turn_id)) = (thread_id, turn_id) else {
            return Err(DriverError::Protocol("The turn has ended. Submit your answer again to continue the conversation.".into()));
        };
        self.0
            .call(
                "turn/steer",
                json!({
                    "threadId": thread_id, "expectedTurnId": turn_id,
                    "clientUserMessageId": message_id, "input": input_items(message),
                }),
            )
            .await
            .map(|_| ())
    }

    async fn interrupt(&self) -> Result<()> {
        let (thread_id, turn_id) = {
            let st = self.0.state.lock().await;
            (st.thread_id.clone(), st.turn_id.clone())
        };
        match (thread_id, turn_id) {
            (Some(t), Some(turn)) => self.0.call("turn/interrupt", json!({ "threadId": t, "turnId": turn })).await.map(|_| ()),
            _ => Ok(()),
        }
    }

    async fn set_permission_mode(&self, mode: PermissionMode) -> Result<()> {
        self.0.state.lock().await.mode = mode;
        Ok(())
    }

    async fn set_model(&self, model: &str) -> Result<()> {
        self.0.state.lock().await.model = Some(model.to_string());
        Ok(())
    }

    async fn set_effort(&self, effort: &str) -> Result<()> {
        self.0.state.lock().await.effort = Some(effort.to_string());
        Ok(())
    }

    async fn respond_permission(&self, request_id: &str, decision: &ApprovalDecision) -> Result<()> {
        let Some((id, kind)) = self.0.pending_approvals.lock().await.get(request_id).cloned() else {
            return Err(DriverError::Protocol(format!("no pending approval {request_id}")));
        };
        if matches!(kind, ApprovalKind::UserInput | ApprovalKind::Elicitation) {
            let response = match decision {
                ApprovalDecision::Submit { response } => response.clone(),
                ApprovalDecision::Deny { .. } if matches!(kind, ApprovalKind::Elicitation) => json!({ "action": "decline" }),
                ApprovalDecision::Deny { .. } => json!({ "answers": {} }),
                _ => return Err(DriverError::Protocol("this request needs an answer".into())),
            };
            self.0.respond(&id, Ok(response)).await?;
            self.0.pending_approvals.lock().await.remove(request_id);
            return Ok(());
        }
        let d = match decision {
            ApprovalDecision::Submit { .. } => return Err(DriverError::Protocol("expected permission decision".into())),
            ApprovalDecision::AllowOnce => "accept",
            ApprovalDecision::AllowAlways => "acceptForSession",
            ApprovalDecision::Deny { .. } => "decline",
        };
        self.0.respond(&id, Ok(json!({ "decision": d }))).await?;
        self.0.pending_approvals.lock().await.remove(request_id);
        Ok(())
    }

    async fn respond_app_tool(&self, request_id: &str, result: std::result::Result<Value, String>) -> Result<()> {
        let pending = self
            .0
            .pending_app_tools
            .lock()
            .await
            .remove(request_id)
            .ok_or_else(|| DriverError::Protocol("Codex dynamic tool request has expired".into()))?;
        let current_turn = self.0.state.lock().await.turn_id.clone();
        if pending.turn_id.is_some() && pending.turn_id != current_turn {
            let error = "Codex dynamic tool request belongs to a finished turn".to_string();
            let response_result = self.0.respond(&pending.provider_id, Err(error.clone())).await;
            return response_result.map_err(|_| DriverError::Protocol(error));
        }
        let response = match result {
            Ok(data) => json!({
                "success": true,
                "contentItems": [{ "type": "inputText", "text": serde_json::to_string(&data).unwrap_or_else(|_| "null".into()) }]
            }),
            Err(error) => json!({
                "success": false,
                "contentItems": [{ "type": "inputText", "text": error }]
            }),
        };
        self.0.respond(&pending.provider_id, Ok(response)).await
    }

    async fn stop_runtime_task(&self, task: &RuntimeTask) -> Result<()> {
        match task.kind {
            RuntimeTaskKind::Process => {
                let process_id = task.id.strip_prefix("process:").unwrap_or(&task.id);
                let thread_id =
                    self.0.state.lock().await.thread_id.clone().ok_or_else(|| DriverError::Protocol("no Codex thread".into()))?;
                let result =
                    self.0.call("thread/backgroundTerminals/terminate", json!({ "threadId": thread_id, "processId": process_id })).await?;
                if result.get("terminated").and_then(Value::as_bool) == Some(false) {
                    return Err(DriverError::Protocol("Codex could not terminate this background process".into()));
                }
                self.0.state.lock().await.stopping_processes.insert(process_id.to_string());
                Ok(())
            }
            RuntimeTaskKind::Agent => {
                let thread_id = task.provider_thread_id.as_deref().unwrap_or(&task.id);
                let mut turn_id = self.0.state.lock().await.subagents.get(thread_id).cloned().flatten();
                if turn_id.is_none()
                    && let Ok(thread) = self.0.call("thread/read", json!({ "threadId": thread_id, "includeTurns": true })).await
                {
                    turn_id = thread
                        .pointer("/thread/turns")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .rev()
                        .find(|turn| turn.get("status").and_then(Value::as_str) == Some("inProgress"))
                        .and_then(|turn| turn.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                let turn_id = turn_id.ok_or_else(|| DriverError::Unsupported("this subagent has no active turn to interrupt".into()))?;
                self.0.call("turn/interrupt", json!({ "threadId": thread_id, "turnId": turn_id })).await.map(|_| ())
            }
            RuntimeTaskKind::Monitor => Err(DriverError::Unsupported("Codex does not expose a targeted monitor stop".into())),
        }
    }

    async fn close(&self) -> Result<()> {
        self.0.closed.store(true, Ordering::Relaxed);
        self.0.child.close().await;
        Ok(())
    }
}

fn parse_rate_limits(value: &Value) -> Option<Vec<kybern_protocol::UsageLimit>> {
    if !value.is_object() {
        return None;
    }
    let limits = [("primary", "Primary"), ("secondary", "Secondary")]
        .into_iter()
        .filter_map(|(key, label)| {
            let window = &value[key];
            let used_percent = window.get("usedPercent")?.as_f64()?;
            if !used_percent.is_finite() {
                return None;
            }
            let window_minutes = window.get("windowDurationMins").and_then(Value::as_u64);
            let name = label.to_string();
            Some(kybern_protocol::UsageLimit {
                name,
                used_percent,
                window_minutes,
                resets_at: window.get("resetsAt").and_then(Value::as_i64),
            })
        })
        .collect::<Vec<_>>();
    if limits.is_empty() { None } else { Some(limits) }
}

#[cfg(test)]
mod tests {
    use super::{ensure_requested_codex_model, parse_codex_model};

    #[test]
    fn model_catalog_preserves_exact_ids_defaults_and_effort_levels() {
        let model = parse_codex_model(&json!({
            "model": "gpt-6-astra",
            "displayName": "GPT-6 Astra",
            "supportedReasoningEfforts": [
                {"reasoningEffort": "low"},
                {"reasoningEffort": "high"},
                {"reasoningEffort": "max"}
            ],
            "defaultReasoningEffort": "high",
            "isDefault": true
        }))
        .expect("visible model");

        assert_eq!(model.id, "gpt-6-astra");
        assert_eq!(model.display_name, "GPT-6 Astra");
        assert_eq!(model.efforts, ["low", "high", "max"]);
        assert_eq!(model.default_effort.as_deref(), Some("high"));
        assert!(model.is_default);
    }

    #[test]
    fn model_catalog_excludes_hidden_and_malformed_entries() {
        assert!(parse_codex_model(&json!({"model": "internal", "hidden": true})).is_none());
        assert!(parse_codex_model(&json!({"displayName": "Missing selector"})).is_none());
    }

    #[test]
    fn explicit_model_must_not_silently_fall_back() {
        assert!(ensure_requested_codex_model(Some("custom/luna"), Some("gpt-6-astra")).is_err());
        assert!(ensure_requested_codex_model(Some("custom/luna"), Some("custom/luna")).is_ok());
        assert!(ensure_requested_codex_model(None, Some("gpt-6-astra")).is_ok());
        assert!(ensure_requested_codex_model(Some("custom/luna"), None).is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn model_discovery_uses_project_and_configured_environment() {
        use crate::{AgentDriver, ProbeContext};

        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(fixture.path().join("project-marker"), "present").unwrap();
        let binary = fixture.path().join("codex-fixture");
        kybern_protocol::process::write_executable(
            &binary,
            r##"#!/bin/sh
if [ "$1" = "--version" ]; then
  printf 'codex-cli 0.154.0\n'
  exit 0
fi
[ "$1" = "app-server" ] || exit 2
IFS= read -r initialize
printf '{"id":1,"result":{}}\n'
IFS= read -r initialized
IFS= read -r models
if [ "$CODEX_DISCOVERY_FIXTURE" = "from-env" ] && [ -f project-marker ]; then
  printf '{"id":2,"result":{"data":[{"model":"custom/from-context","displayName":"Context model","supportedReasoningEfforts":[{"reasoningEffort":"max"}],"defaultReasoningEffort":"max","isDefault":true}]}}\n'
else
  printf '{"id":2,"result":{"data":[]}}\n'
fi
"##,
        )
        .unwrap();

        let status = super::CodexDriver
            .probe_with_context(&ProbeContext {
                binary: Some(binary),
                cwd: Some(fixture.path().to_path_buf()),
                env: [("CODEX_DISCOVERY_FIXTURE".into(), "from-env".into())].into(),
            })
            .await;

        assert!(status.available);
        assert_eq!(status.models.len(), 1);
        assert_eq!(status.models[0].id, "custom/from-context");
        assert_eq!(status.models[0].efforts, ["max"]);
        assert_eq!(status.models[0].default_effort.as_deref(), Some("max"));
    }

    #[test]
    fn dynamic_tools_accept_shared_gateway_credentials_without_using_mcp() {
        use super::*;
        use crate::{NativeToolDefinition, NativeToolRestrictions};

        let bridge = NativeToolBridge {
            server_name: "kybern".into(),
            endpoint: Some("http://127.0.0.1:4199/native-tools/mcp".into()),
            authorization: Some("session-capability".into()),
            coordinator_instructions: None,
            guide: None,
            tools: vec![NativeToolDefinition {
                name: "kybern_collaboration_read".into(),
                description: "Read collaboration state".into(),
                input_schema: json!({"type":"object"}),
            }],
            restrictions: NativeToolRestrictions::default(),
        };
        assert!(validate_codex_bridge(&bridge).is_ok());

        let mut restricted = bridge;
        restricted.restrictions.require_enforcement = true;
        assert!(matches!(
            validate_codex_bridge(&restricted),
            Err(DriverError::Unsupported(message)) if message.contains("cannot enforce coordinator restrictions")
        ));
    }

    #[tokio::test]
    async fn dynamic_tool_requests_round_trip_with_native_ids() {
        use super::*;
        use crate::{NativeToolDefinition, NativeToolRestrictions};

        let child = Arc::new(NdjsonChild::spawn(crate::test_process::echo()).unwrap());
        let (events, mut rx) = mpsc::channel(8);
        let session = Arc::new(CodexSession {
            child: child.clone(),
            events,
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            pending_approvals: Mutex::new(HashMap::new()),
            pending_app_tools: Mutex::new(HashMap::new()),
            notification_gate: Mutex::new(()),
            state: Mutex::new(State {
                manual_compaction: false,
                deferred_notifications: Vec::new(),
                thread_id: Some("root".into()),
                turn_id: Some("turn-1".into()),
                mode: PermissionMode::Supervised,
                model: None,
                effort: None,
                cwd: PathBuf::from("/tmp"),
                last_total_tokens: None,
                message_ids: HashMap::new(),
                file_changes: HashMap::new(),
                subagents: HashMap::new(),
                background_processes: HashMap::new(),
                stopping_processes: HashSet::new(),
            }),
            turn_usage: Mutex::new(None),
            bridge: Some(NativeToolBridge {
                server_name: "kybern".into(),
                endpoint: None,
                authorization: None,
                coordinator_instructions: None,
                guide: None,
                tools: vec![NativeToolDefinition {
                    name: "kybern_collaboration_read".into(),
                    description: "Read collaboration state".into(),
                    input_schema: json!({"type":"object"}),
                }],
                restrictions: NativeToolRestrictions::default(),
            }),
            closed: AtomicBool::new(false),
        });

        session
            .handle_server_request(
                json!(17),
                "item/tool/call",
                &json!({"callId":"call-17","threadId":"root","turnId":"turn-1","tool":"kybern_collaboration_read","arguments":{"group_id":"g"}}),
            )
            .await;
        assert!(matches!(
            rx.recv().await,
            Some(DriverEvent::AppToolRequest { request_id, name, arguments })
                if request_id == "call-17" && name == "kybern_collaboration_read" && arguments["group_id"] == "g"
        ));

        let handle = Handle(session.clone(), crate::ndjson::SessionLifetime::new(child.clone()));
        handle.respond_app_tool("call-17", Ok(json!({"messages":[]}))).await.unwrap();
        assert!(session.pending_app_tools.lock().await.is_empty());
        handle.close().await.unwrap();
    }

    #[tokio::test]
    async fn questions_round_trip_and_withdraw_on_provider_resolution() {
        use super::*;

        let child = Arc::new(NdjsonChild::spawn(crate::test_process::echo()).unwrap());
        let (events, mut rx) = mpsc::channel(8);
        let session = Arc::new(CodexSession {
            child: child.clone(),
            events,
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            pending_approvals: Mutex::new(HashMap::new()),
            pending_app_tools: Mutex::new(HashMap::new()),
            notification_gate: Mutex::new(()),
            state: Mutex::new(State {
                manual_compaction: false,
                deferred_notifications: Vec::new(),
                thread_id: Some("test".into()),
                turn_id: None,
                mode: PermissionMode::Supervised,
                model: None,
                effort: None,
                cwd: PathBuf::from("/tmp"),
                last_total_tokens: None,
                message_ids: HashMap::new(),
                file_changes: HashMap::new(),
                subagents: HashMap::new(),
                background_processes: HashMap::new(),
                stopping_processes: HashSet::new(),
            }),
            turn_usage: Mutex::new(None),
            bridge: None,
            closed: AtomicBool::new(false),
        });
        session
            .handle_server_request(json!(42), "item/tool/requestUserInput", &json!({"questions":[{"id":"choice", "question":"Which?"}]}))
            .await;
        let Some(DriverEvent::PermissionRequest { request_id, tool_name, .. }) = rx.recv().await else {
            panic!("missing question");
        };
        assert_eq!(tool_name, "request_user_input");
        let handle = Handle(session.clone(), crate::ndjson::SessionLifetime::new(session.child.clone()));
        let answer = json!({"answers":{"choice":{"answers":["Custom answer"]}}});
        handle.respond_permission(&request_id, &ApprovalDecision::Submit { response: answer.clone() }).await.unwrap();
        let wire = tokio::time::timeout(std::time::Duration::from_secs(1), async { child.lines.lock().await.recv().await.unwrap() })
            .await
            .unwrap();
        assert_eq!(wire["id"], 42);
        assert_eq!(wire["result"], answer);
        session.handle_server_request(json!(43), "item/tool/requestUserInput", &json!({"questions":[]})).await;
        let _ = rx.recv().await;
        session.handle_notification("serverRequest/resolved", &json!({"requestId":43})).await;
        assert!(matches!(rx.recv().await, Some(DriverEvent::PermissionWithdrawn { .. })));
        assert!(session.pending_approvals.lock().await.is_empty());
        child.kill().await;
    }
    #[test]
    fn installed_plugins_become_mentionable_catalog_entries() {
        let result = json!({
            "marketplaces": [{
                "name": "openai-bundled",
                "plugins": [
                    { "id": "computer-use@openai-bundled", "name": "computer-use", "installed": true, "enabled": true,
                      "interface": { "displayName": "Computer Use", "shortDescription": "Control Mac apps from ChatGPT" } },
                    { "id": "unified-computer-use@openai-bundled", "name": "unified-computer-use", "installed": true, "enabled": true,
                      "interface": { "displayName": null } },
                    { "id": "sites@openai-bundled", "name": "sites", "installed": false, "enabled": true,
                      "interface": { "displayName": "Sites" } }
                ]
            }]
        });
        let plugins = installed_plugins(&result);
        assert_eq!(plugins.len(), 1);
        let plugin = &plugins[0];
        assert_eq!(plugin.name, "computer-use");
        assert_eq!(plugin.display_name.as_deref(), Some("Computer Use"));
        assert_eq!(plugin.path, "plugin://computer-use@openai-bundled");
        assert_eq!(plugin.scope, SkillScope::Plugin);
        assert!(plugin.enabled);
    }

    #[test]
    fn plugin_mentions_send_the_token_and_a_structured_mention_item() {
        let message = UserMessage {
            parts: vec![
                ContentPart::Mention {
                    name: "computer-use".into(),
                    path: "plugin://computer-use@openai-bundled".into(),
                    display_name: Some("Computer Use".into()),
                },
                ContentPart::Text { text: " open Finder".into() },
            ],
        };
        let items = input_items(&message);
        assert_eq!(items[0]["text"], "@computer-use");
        assert_eq!(items[1], json!({ "type": "mention", "name": "Computer Use", "path": "plugin://computer-use@openai-bundled" }));
        assert_eq!(items[2]["text"], " open Finder");
    }

    use std::collections::HashMap;

    use super::{
        ChildNotificationRoute, child_notification_route, codex_agent_status, codex_background_process, codex_subagent_turn_status,
        input_items, installed_plugins, process_stats_changed, should_register_subagent,
    };
    use kybern_protocol::{ContentPart, RuntimeTaskKind, RuntimeTaskStats, RuntimeTaskStatus, SkillScope, UserMessage};
    use serde_json::json;

    #[test]
    fn sends_codex_skills_as_visible_text_and_structured_items() {
        let message = UserMessage {
            parts: vec![ContentPart::Skill { name: "Expo UI SwiftUI".into(), path: "/skills/expo-ui-swiftui/SKILL.md".into() }],
        };
        assert_eq!(
            input_items(&message),
            vec![
                json!({ "type": "text", "text": "$Expo UI SwiftUI", "text_elements": [] }),
                json!({ "type": "skill", "name": "Expo UI SwiftUI", "path": "/skills/expo-ui-swiftui/SKILL.md" }),
            ]
        );
    }

    #[test]
    fn maps_subagent_states_and_background_terminal_inventory() {
        assert_eq!(codex_agent_status("pendingInit"), RuntimeTaskStatus::Pending);
        assert_eq!(codex_agent_status("running"), RuntimeTaskStatus::Running);
        assert_eq!(codex_agent_status("completed"), RuntimeTaskStatus::Completed);
        assert_eq!(codex_agent_status("errored"), RuntimeTaskStatus::Failed);
        assert_eq!(codex_subagent_turn_status(Some("completed")), RuntimeTaskStatus::Completed);
        assert_eq!(codex_subagent_turn_status(Some("failed")), RuntimeTaskStatus::Failed);
        assert_eq!(codex_subagent_turn_status(Some("interrupted")), RuntimeTaskStatus::Interrupted);

        let task = codex_background_process(
            "42",
            &json!({ "command": ["pnpm", "dev"], "cwd": "/tmp/project", "itemId": "item-9" }),
            RuntimeTaskStats { cpu_percent: Some(2.5), rss_kb: Some(4096), ..Default::default() },
        );
        assert_eq!(task.id, "process:42");
        assert_eq!(task.kind, RuntimeTaskKind::Process);
        assert_eq!(task.title, "pnpm dev");
        assert!(task.backgrounded);
        assert!(task.capabilities.stop);
        assert_eq!(task.stats.rss_kb, Some(4096));
    }

    #[test]
    fn suppresses_noisy_background_process_metric_updates() {
        let before = RuntimeTaskStats { cpu_percent: Some(2.0), rss_kb: Some(4096), ..Default::default() };
        let noise = RuntimeTaskStats { cpu_percent: Some(2.5), rss_kb: Some(4600), ..Default::default() };
        let meaningful = RuntimeTaskStats { cpu_percent: Some(3.0), rss_kb: Some(5120), ..Default::default() };
        assert!(!process_stats_changed(&before, &noise));
        assert!(process_stats_changed(&before, &meaningful));
    }

    #[test]
    fn registers_only_descendants_of_the_root_thread() {
        let mut known = HashMap::new();
        assert!(!should_register_subagent(Some("root"), &known, "root", Some("root")));
        assert!(!should_register_subagent(Some("root"), &known, "unrelated", Some("elsewhere")));
        assert!(should_register_subagent(Some("root"), &known, "child", Some("root")));

        known.insert("child".to_string(), None);
        assert!(should_register_subagent(Some("root"), &known, "grandchild", Some("child")));
    }

    #[test]
    fn routes_child_lifecycle_without_leaking_child_prose() {
        assert_eq!(child_notification_route("turn/started"), ChildNotificationRoute::Activity);
        assert_eq!(child_notification_route("item/started"), ChildNotificationRoute::Activity);
        assert_eq!(child_notification_route("error"), ChildNotificationRoute::Activity);

        assert_eq!(child_notification_route("item/agentMessage/delta"), ChildNotificationRoute::Forward);
        assert_eq!(child_notification_route("item/reasoning/textDelta"), ChildNotificationRoute::Forward);
        assert_eq!(child_notification_route("item/commandExecution/outputDelta"), ChildNotificationRoute::Forward);
        assert_eq!(child_notification_route("turn/plan/updated"), ChildNotificationRoute::Drop);

        assert_eq!(child_notification_route("serverRequest/resolved"), ChildNotificationRoute::Root);
        assert_eq!(child_notification_route("future/method"), ChildNotificationRoute::Root);
    }
}

#[cfg(all(test, unix))]
mod startup_cleanup_tests {
    use super::*;
    use std::time::Duration;

    /// Linux refuses to run a file that another test's freshly forked child
    /// still holds open for writing (ETXTBSY) until that child execs. Retry.
    async fn spawn_fixture(config: SessionConfig) -> Result<SpawnedSession> {
        for _ in 0..50 {
            match CodexDriver.spawn(config.clone()).await {
                Err(DriverError::Io(error)) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                result => return result,
            }
        }
        CodexDriver.spawn(config).await
    }

    #[tokio::test]
    async fn provider_session_attach_scopes_tools_and_coordinator_instructions() {
        use crate::{NativeToolDefinition, NativeToolRestrictions};

        for (resume, fork, coordinator, guide, method) in [
            (None, false, true, false, "thread/start"),
            (Some("saved"), false, true, false, "thread/resume"),
            (Some("saved"), true, true, false, "thread/fork"),
            (Some("ordinary"), false, false, false, "thread/resume"),
            // The guide reaches ordinary threads on every attach, and precedes a coordinator role.
            (None, false, false, true, "thread/start"),
            (Some("saved"), false, false, true, "thread/resume"),
            (Some("saved"), true, false, true, "thread/fork"),
            (Some("saved"), false, true, true, "thread/resume"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let binary = root.path().join("codex-fixture");
            let requests = root.path().join("requests");
            kybern_protocol::process::write_executable(
                &binary,
                r#"#!/bin/sh
IFS= read -r line
printf '%s\n' "$line" >> "$REQUESTS"
printf '{"id":1,"result":{}}\n'
IFS= read -r line
printf '%s\n' "$line" >> "$REQUESTS"
IFS= read -r line
printf '%s\n' "$line" >> "$REQUESTS"
printf '{"id":2,"result":{"thread":{"id":"root"},"model":"fixture"}}\n'
IFS= read -r line
printf '{"id":3,"result":{"rateLimits":{}}}\n'
wait
"#,
            )
            .unwrap();
            let bridge = NativeToolBridge {
                server_name: "kybern".into(),
                endpoint: Some("http://127.0.0.1:4199/native-tools/mcp".into()),
                authorization: Some("session-capability".into()),
                coordinator_instructions: coordinator.then(|| "COORDINATOR ROLE SENTINEL".into()),
                guide: guide.then(|| "KYBERN GUIDE SENTINEL".into()),
                tools: vec![NativeToolDefinition {
                    name: "kybern_collaboration_spawn".into(),
                    description: "Delegate bounded work".into(),
                    input_schema: json!({"type":"object"}),
                }],
                restrictions: NativeToolRestrictions::default(),
            };
            let spawned = spawn_fixture(SessionConfig {
                cwd: root.path().into(),
                model: None,
                effort: None,
                permission_mode: PermissionMode::Supervised,
                native_tool_bridge: Some(bridge),
                resume_session_id: resume.map(str::to_string),
                resume_lacks_instructions: false,
                fork,
                rewind: None,
                binary: Some(binary),
                env: HashMap::from([("REQUESTS".into(), requests.to_string_lossy().into_owned())]),
            })
            .await
            .unwrap();
            let recorded = std::fs::read_to_string(requests).unwrap();
            let lines = recorded.lines().map(|line| serde_json::from_str::<Value>(line).unwrap()).collect::<Vec<_>>();
            assert_eq!(lines.len(), 3);
            assert_eq!(lines[2]["method"], method);
            assert!(lines.iter().all(|line| line["method"] != "config/read"));
            assert_eq!(lines[2]["params"].get("developerInstructions").is_some(), coordinator || guide);
            match (guide, coordinator) {
                (true, true) => assert_eq!(lines[2]["params"]["developerInstructions"], "KYBERN GUIDE SENTINEL\nCOORDINATOR ROLE SENTINEL"),
                (true, false) => assert_eq!(lines[2]["params"]["developerInstructions"], "KYBERN GUIDE SENTINEL"),
                (false, true) => assert_eq!(lines[2]["params"]["developerInstructions"], "COORDINATOR ROLE SENTINEL"),
                (false, false) => {}
            }
            assert_eq!(lines[2]["params"]["dynamicTools"][0]["name"], "kybern_collaboration_spawn");
            spawned.session.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn failed_resume_cleans_up_and_preserves_history() {
        let root = tempfile::tempdir().unwrap();
        let binary = root.path().join("codex-fixture");
        std::fs::write(root.path().join("history"), "existing conversation").unwrap();
        kybern_protocol::process::write_executable(
            &binary,
            r#"#!/bin/sh
read line
printf '{"id":1,"result":{}}\n'
read line
read line
(sleep 0.3; touch escaped) &
printf '{"id":2,"error":{"message":"thread fixture already has an active writer"}}\n'
wait
"#,
        )
        .unwrap();
        let result = spawn_fixture(SessionConfig {
            cwd: root.path().into(),
            model: None,
            effort: None,
            permission_mode: PermissionMode::Supervised,
            native_tool_bridge: None,
            resume_session_id: Some("fixture".into()),
            resume_lacks_instructions: false,
            fork: false,
            rewind: None,
            binary: Some(binary),
            env: HashMap::new(),
        })
        .await;
        let error = match result {
            Ok(_) => panic!("resume should fail"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("did not stop the other session"), "{error}");
        tokio::time::sleep(Duration::from_millis(450)).await;
        assert!(!root.path().join("escaped").exists());
        assert_eq!(std::fs::read_to_string(root.path().join("history")).unwrap(), "existing conversation");
    }

    #[tokio::test]
    async fn parent_context_and_account_limits_ignore_child_token_usage() {
        let child = Arc::new(NdjsonChild::spawn(crate::test_process::echo()).unwrap());
        let (events, mut rx) = mpsc::channel(8);
        let session = Arc::new(CodexSession {
            child: child.clone(),
            events,
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            pending_approvals: Mutex::new(HashMap::new()),
            pending_app_tools: Mutex::new(HashMap::new()),
            notification_gate: Mutex::new(()),
            state: Mutex::new(State {
                manual_compaction: false,
                deferred_notifications: Vec::new(),
                thread_id: Some("test".into()),
                turn_id: None,
                mode: PermissionMode::Supervised,
                model: None,
                effort: None,
                cwd: PathBuf::from("/tmp"),
                last_total_tokens: None,
                message_ids: HashMap::new(),
                file_changes: HashMap::new(),
                subagents: HashMap::new(),
                background_processes: HashMap::new(),
                stopping_processes: HashSet::new(),
            }),
            turn_usage: Mutex::new(None),
            bridge: None,
            closed: AtomicBool::new(false),
        });
        let tokens = json!({"threadId":"test","tokenUsage":{"last":{"totalTokens":1234},"modelContextWindow":200000}});
        session.handle_notification("thread/tokenUsage/updated", &tokens).await;
        let Some(DriverEvent::UsageUpdated(usage)) = rx.recv().await else { panic!("missing parent usage") };
        assert_eq!(usage.context.unwrap().used_tokens, 1234);
        session
            .handle_notification(
                "thread/tokenUsage/updated",
                &json!({"threadId":"child","tokenUsage":{"last":{"totalTokens":999999},"modelContextWindow":1000000}}),
            )
            .await;
        while let Ok(event) = rx.try_recv() {
            assert!(!matches!(event, DriverEvent::UsageUpdated(_)));
        }
        session
            .handle_notification(
                "account/rateLimits/updated",
                &json!({"rateLimits":{"primary":{"usedPercent":42,"windowDurationMins":300}}}),
            )
            .await;
        let Some(DriverEvent::UsageUpdated(usage)) = rx.recv().await else { panic!("missing account usage") };
        assert_eq!(usage.limits.unwrap()[0].used_percent, 42.0);
        child.kill().await;
    }

    #[tokio::test]
    async fn early_resume_notifications_never_register_the_root_as_a_subagent() {
        let child = Arc::new(NdjsonChild::spawn(crate::test_process::echo()).unwrap());
        let (events, mut rx) = mpsc::channel(64);
        let session = Arc::new(CodexSession {
            child: child.clone(),
            events,
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            pending_approvals: Mutex::new(HashMap::new()),
            pending_app_tools: Mutex::new(HashMap::new()),
            notification_gate: Mutex::new(()),
            state: Mutex::new(State {
                manual_compaction: false,
                deferred_notifications: Vec::new(),
                thread_id: None,
                turn_id: None,
                mode: PermissionMode::Supervised,
                model: None,
                effort: None,
                cwd: PathBuf::from("/tmp"),
                last_total_tokens: None,
                message_ids: HashMap::new(),
                file_changes: HashMap::new(),
                subagents: HashMap::new(),
                background_processes: HashMap::new(),
                stopping_processes: HashSet::new(),
            }),
            turn_usage: Mutex::new(None),
            bridge: None,
            closed: AtomicBool::new(false),
        });

        // Codex may notify before returning thread/resume (also start/fork).
        session.handle_notification("turn/started", &json!({"threadId":"root", "turn":{"id":"root-turn"}})).await;
        session.handle_notification("item/agentMessage/delta", &json!({"threadId":"root", "itemId":"message", "delta":"root prose"})).await;
        session.handle_notification("turn/started", &json!({"threadId":"child", "turn":{"id":"child-turn"}})).await;
        assert!(rx.try_recv().is_err());
        assert!(session.state.lock().await.subagents.is_empty());
        session.bind_thread("root".into(), None).await;
        assert!(matches!(rx.recv().await, Some(DriverEvent::SessionBound { session_id, .. }) if session_id == "root"));
        assert_eq!(session.state.lock().await.turn_id.as_deref(), Some("root-turn"));
        assert!(session.state.lock().await.subagents.contains_key("child"));
        assert!(!session.ensure_subagent("root").await);
        session.handle_notification("item/started", &json!({"threadId":"root", "item":{"type":"collabAgentToolCall", "id":"collab", "receiverThreadIds":["root"], "agentsStates":{"root":{"status":"running"}}}})).await;
        session
            .handle_notification(
                "item/started",
                &json!({"threadId":"root", "item":{"type":"subAgentActivity", "id":"activity", "agentThreadId":"root"}}),
            )
            .await;
        let mut child_started = false;
        let mut root_prose = String::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                DriverEvent::TextDelta { origin: EventOrigin::Root, delta, .. } => root_prose.push_str(&delta),
                DriverEvent::RuntimeTaskStarted(task) => {
                    assert_ne!(task.id, "root");
                    child_started |= task.id == "child";
                }
                DriverEvent::RuntimeTaskUpdated(task) | DriverEvent::RuntimeTaskCompleted(task) => assert_ne!(task.id, "root"),
                _ => {}
            }
        }
        assert!(child_started, "real child activity must still be delivered");
        assert_eq!(root_prose, "root prose", "early root prose must survive binding");
        assert!(!session.state.lock().await.subagents.contains_key("root"));
        child.kill().await;
    }

    #[tokio::test]
    async fn subagent_items_and_deltas_are_forwarded_to_the_subagent_only() {
        let child = Arc::new(NdjsonChild::spawn(crate::test_process::echo()).unwrap());
        let (events, mut rx) = mpsc::channel(64);
        let session = Arc::new(CodexSession {
            child: child.clone(),
            events,
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            pending_approvals: Mutex::new(HashMap::new()),
            pending_app_tools: Mutex::new(HashMap::new()),
            notification_gate: Mutex::new(()),
            state: Mutex::new(State {
                manual_compaction: false,
                deferred_notifications: Vec::new(),
                thread_id: Some("root".into()),
                turn_id: None,
                mode: PermissionMode::Supervised,
                model: None,
                effort: None,
                cwd: PathBuf::from("/tmp"),
                last_total_tokens: None,
                message_ids: HashMap::new(),
                file_changes: HashMap::new(),
                subagents: HashMap::new(),
                background_processes: HashMap::new(),
                stopping_processes: HashSet::new(),
            }),
            turn_usage: Mutex::new(None),
            bridge: None,
            closed: AtomicBool::new(false),
        });
        session
            .handle_notification(
                "thread/started",
                &json!({"thread":{"id":"kid", "source":{"subAgent":{"thread_spawn":{"parent_thread_id":"root", "agent_role":"worker"}}}}}),
            )
            .await;
        let kid = |method: &str, params: Value| (method.to_string(), params);
        let notifications = [
            kid(
                "item/started",
                json!({"threadId":"kid", "item":{"type":"userMessage", "id":"u1", "content":[{"type":"text","text":"Check the diff"}]}}),
            ),
            kid(
                "item/started",
                json!({"threadId":"kid", "item":{"type":"commandExecution", "id":"cmd", "command":"git diff", "cwd":"/tmp"}}),
            ),
            kid("item/commandExecution/outputDelta", json!({"threadId":"kid", "itemId":"cmd", "delta":"+x"})),
            kid(
                "item/completed",
                json!({"threadId":"kid", "item":{"type":"commandExecution", "id":"cmd", "status":"completed", "aggregatedOutput":"+x", "exitCode":0}}),
            ),
            kid("item/reasoning/textDelta", json!({"threadId":"kid", "itemId":"r", "delta":"thinking"})),
            kid("item/agentMessage/delta", json!({"threadId":"kid", "itemId":"m", "delta":"All"})),
            kid("item/completed", json!({"threadId":"kid", "item":{"type":"agentMessage", "id":"m", "text":"All good"}})),
        ];
        for (method, params) in &notifications {
            session.handle_notification(method, params).await;
        }
        let mut forwarded = Vec::new();
        let mut leaked_to_root = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                DriverEvent::SubagentOnly { task_id, event } => {
                    assert_eq!(task_id, "kid");
                    forwarded.push(*event);
                }
                DriverEvent::SubagentPrompt { task_id, prompt } => {
                    assert_eq!((task_id.as_str(), prompt.as_str()), ("kid", "Check the diff"));
                    forwarded.push(DriverEvent::SubagentPrompt { task_id, prompt });
                }
                DriverEvent::TextDelta { .. }
                | DriverEvent::ThinkingDelta { .. }
                | DriverEvent::MessageCompleted { .. }
                | DriverEvent::ToolStarted(_)
                | DriverEvent::ToolCompleted { .. }
                | DriverEvent::ToolOutputDelta { .. } => leaked_to_root = true,
                _ => {}
            }
        }
        assert!(!leaked_to_root, "a subagent's content never reaches the root as a bare event");
        let shape: Vec<&str> = forwarded
            .iter()
            .map(|event| match event {
                DriverEvent::SubagentPrompt { .. } => "prompt",
                DriverEvent::ToolStarted(call) if call.name == "shell" && call.parent_id.is_none() => "tool",
                DriverEvent::ToolOutputDelta { .. } => "output",
                DriverEvent::ToolCompleted { .. } => "done",
                DriverEvent::ThinkingDelta { .. } => "thinking",
                DriverEvent::TextDelta { delta, .. } if delta == "All" => "text",
                DriverEvent::MessageCompleted { text, .. } if text == "All good" => "message",
                _ => "other",
            })
            .collect();
        assert_eq!(shape, ["prompt", "tool", "output", "done", "thinking", "text", "message"]);
        child.kill().await;
    }

    #[tokio::test]
    async fn manual_compaction_uses_native_rpc_and_waits_for_turn_completion() {
        let child = Arc::new(NdjsonChild::spawn(crate::test_process::echo()).unwrap());
        let (events, mut rx) = mpsc::channel(8);
        let session = Arc::new(CodexSession {
            child: child.clone(),
            events,
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            pending_approvals: Mutex::new(HashMap::new()),
            pending_app_tools: Mutex::new(HashMap::new()),
            notification_gate: Mutex::new(()),
            state: Mutex::new(State {
                manual_compaction: false,
                deferred_notifications: Vec::new(),
                thread_id: Some("test".into()),
                turn_id: None,
                mode: PermissionMode::Supervised,
                model: None,
                effort: None,
                cwd: PathBuf::from("/tmp"),
                last_total_tokens: None,
                message_ids: HashMap::new(),
                file_changes: HashMap::new(),
                subagents: HashMap::new(),
                background_processes: HashMap::new(),
                stopping_processes: HashSet::new(),
            }),
            turn_usage: Mutex::new(None),
            bridge: None,
            closed: AtomicBool::new(false),
        });

        let handle = Handle(session.clone(), crate::ndjson::SessionLifetime::new(child.clone()));
        let task = tokio::spawn(async move {
            let result = handle.compact().await;
            (handle, result)
        });
        let request = tokio::time::timeout(Duration::from_secs(1), async { child.lines.lock().await.recv().await.unwrap() }).await.unwrap();
        assert_eq!(request["method"], "thread/compact/start");
        assert_eq!(request["params"], json!({"threadId":"test"}));
        session.pending.lock().await.remove(&request["id"].as_i64().unwrap()).unwrap().send(Ok(json!({}))).unwrap();
        let (handle, result) = task.await.unwrap();
        result.unwrap();
        assert!(rx.try_recv().is_err(), "request acceptance is not completion");
        session.handle_notification("turn/started", &json!({"threadId":"test", "turn":{"id":"compact-turn"}})).await;
        session
            .handle_notification("turn/completed", &json!({"threadId":"test", "turn":{"id":"compact-turn", "status":"completed"}}))
            .await;
        assert!(matches!(rx.recv().await, Some(DriverEvent::TurnCompleted { .. })));
        let task = tokio::spawn(async move {
            let result = handle.compact().await;
            (handle, result)
        });
        let request = tokio::time::timeout(Duration::from_secs(1), async { child.lines.lock().await.recv().await.unwrap() }).await.unwrap();
        session
            .pending
            .lock()
            .await
            .remove(&request["id"].as_i64().unwrap())
            .unwrap()
            .send(Err("unsupported installed version".into()))
            .unwrap();
        let (handle, result) = task.await.unwrap();
        assert!(result.is_err());
        assert!(!session.state.lock().await.manual_compaction);
        handle.close().await.unwrap();
    }

    #[tokio::test]
    async fn async_questions_keep_native_fields_and_answers_steer_the_current_turn() {
        let child = Arc::new(NdjsonChild::spawn(crate::test_process::echo()).unwrap());
        let (events, mut rx) = mpsc::channel(8);
        let session = Arc::new(CodexSession {
            child: child.clone(),
            events,
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            pending_approvals: Mutex::new(HashMap::new()),
            pending_app_tools: Mutex::new(HashMap::new()),
            notification_gate: Mutex::new(()),
            state: Mutex::new(State {
                manual_compaction: false,
                deferred_notifications: Vec::new(),
                thread_id: Some("test".into()),
                turn_id: None,
                mode: PermissionMode::Supervised,
                model: None,
                effort: None,
                cwd: PathBuf::from("/tmp"),
                last_total_tokens: None,
                message_ids: HashMap::new(),
                file_changes: HashMap::new(),
                subagents: HashMap::new(),
                background_processes: HashMap::new(),
                stopping_processes: HashSet::new(),
            }),
            turn_usage: Mutex::new(None),
            bridge: None,
            closed: AtomicBool::new(false),
        });
        session.state.lock().await.turn_id = Some("active-turn".into());
        let item = json!({"type":"agentMessage", "id":"call-question", "text":"Choose a window", "delivery":"async",
            "questions":[{"title":"Allow a QA window?", "options":["Allow", "Keep current window"]}, {"title":"Any constraints?", "options":null}]});
        session.handle_item(&item, false).await;
        assert!(rx.try_recv().is_err());
        session.handle_item(&item, true).await;
        let Some(DriverEvent::AsyncQuestions(request)) = rx.recv().await else { panic!("missing structured question") };
        assert_eq!(request.id, "call-question");
        assert_eq!(request.questions[0].options, ["Allow", "Keep current window"]);
        assert!(request.questions[1].options.is_empty());
        assert!(matches!(rx.recv().await, Some(DriverEvent::MessageCompleted { .. })));
        assert!(session.pending_approvals.lock().await.is_empty());
        let handle = Handle(session.clone(), crate::ndjson::SessionLifetime::new(child.clone()));
        let task = tokio::spawn(async move {
            let result = handle.steer("answer-id", &UserMessage::text("Keep current window")).await;
            (handle, result)
        });
        let request = tokio::time::timeout(Duration::from_secs(1), async { child.lines.lock().await.recv().await.unwrap() }).await.unwrap();
        assert_eq!(request["method"], "turn/steer");
        assert_eq!(request["params"]["expectedTurnId"], "active-turn");
        assert_eq!(request["params"]["clientUserMessageId"], "answer-id");
        assert_eq!(request["params"]["input"][0]["text"], "Keep current window");
        session.pending.lock().await.remove(&request["id"].as_i64().unwrap()).unwrap().send(Ok(json!({}))).unwrap();
        let (handle, result) = task.await.unwrap();
        result.unwrap();
        assert!(rx.try_recv().is_err(), "steering must not complete or interrupt the turn");
        session.state.lock().await.turn_id = None;
        assert!(handle.steer("late", &UserMessage::text("late answer")).await.unwrap_err().to_string().contains("turn has ended"));
        handle.close().await.unwrap();
    }

    #[test]
    fn limits_preserve_reported_windows_and_reset_times() {
        let limits = parse_rate_limits(&json!({"primary":{"usedPercent":23.5,"windowDurationMins":300,"resetsAt":1900000000},"secondary":{"usedPercent":40,"windowDurationMins":10080}})).unwrap();
        assert_eq!(limits[0].name, "Primary");
        assert_eq!(limits[0].window_minutes, Some(300));
        assert_eq!(limits[0].resets_at, Some(1900000000));
        assert_eq!(limits[1].name, "Secondary");
        assert_eq!(limits[1].window_minutes, Some(10080));
        assert!(limits[1].resets_at.is_none());
        assert!(parse_rate_limits(&json!({"primary":null})).is_none());
    }
}

#[cfg(test)]
mod app_catalog_tests {
    #[test]
    fn only_connected_enabled_apps_can_be_mentioned() {
        let apps = super::connected_apps(&serde_json::json!({"data":[
            {"id":"app-one","name":"One","isAccessible":true,"isEnabled":true},
            {"id":"app-two","name":"Two","isAccessible":false},
            {"id":"app-three","name":"Three","isAccessible":true,"isEnabled":false}
        ]}));
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].path, "app://app-one");
        assert_eq!(apps[0].scope, kybern_protocol::SkillScope::Plugin);
    }
}

#[cfg(test)]
mod orchestration_part_tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn thread_messages_and_agent_results_flatten_to_text_items() {
        let items = input_items(&orchestration_message());
        assert_flattened(&items.iter().filter_map(|i| i["text"].as_str()).collect::<Vec<_>>().join("\n"));
    }
}
