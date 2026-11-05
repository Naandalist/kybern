//! OpenCode driver: runs `opencode serve` per session and speaks its HTTP API,
//! consuming the server-sent event stream for progress.
//!
//! One server process per kybern thread keeps lifecycle simple: the process's
//! cwd is the project directory, and closing the session kills it. Sessions
//! are persisted by OpenCode on disk, so resuming is just prompting the same
//! `ses_*` id from a fresh server.
//!
//! OpenCode 2 replaced the server API; `v2` speaks it. The installed version
//! picks the implementation at probe and spawn time.

mod v2;

use kybern_protocol::process::NoWindow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use kybern_protocol::*;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::{Mutex, mpsc};

pub(crate) use v2::Server as V2Server;

use crate::binary::{at_least, parse_semver, resolve, version_of};
use crate::{
    AgentDriver, AgentSession, DriverError, DriverEvent, DriverRuntimeTask, DriverRuntimeTaskUpdate, NativeToolBridge, ProbeContext,
    Result, SessionConfig, SpawnedSession, summarize_tool_call,
};

const MIN_VERSION: (u64, u64, u64) = (1, 10, 0);
/// The v2 API Kybern was built against; earlier 2.x builds predate it.
const MIN_V2_VERSION: (u64, u64, u64) = (2, 2, 0);
const MAX_SKILL_DISCOVERY_BYTES: usize = 8 * 1024 * 1024;
const MODEL_DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Default)]
pub struct OpencodeDriver;

/// `opencode models` arguments. OpenCode 2 hands CLI commands to a shared
/// background server (`opencode serve --service`) that it starts on demand
/// and leaves running with the user's MCP servers loaded; `--standalone`
/// answers from a private server that exits with the command.
fn models_args(api: Option<Api>) -> &'static [&'static str] {
    match api {
        Some(Api::V2) => &["models", "--standalone"],
        _ => &["models"],
    }
}

async fn opencode_models(bin: &std::path::Path, api: Option<Api>, context: &ProbeContext) -> Vec<ProviderModel> {
    let mut command = crate::binary::command(bin).no_window();
    command.args(models_args(api)).stdin(Stdio::null()).kill_on_drop(true);
    if let Some(cwd) = &context.cwd {
        command.current_dir(cwd);
    }
    for (key, value) in &context.env {
        command.env(key, value);
    }
    let output = match tokio::time::timeout(MODEL_DISCOVERY_TIMEOUT, crate::process_tree::output(&mut command)).await {
        Ok(Ok(output)) if output.status.success() => output,
        _ => return Vec::new(),
    };
    let mut models: Vec<ProviderModel> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let id = line.trim();
            if id.is_empty() {
                return None;
            }
            let (provider, name) = id.split_once('/').unwrap_or(("", id));
            Some(ProviderModel {
                id: id.to_string(),
                display_name: name.to_string(),
                resolved_id: None,
                description: None,
                provider: (!provider.is_empty()).then(|| provider.to_string()),
                efforts: Vec::new(),
                default_effort: None,
                is_default: false,
                parameters: Vec::new(),
                variants: Vec::new(),
            })
        })
        .collect();
    models.sort_by(|a, b| a.provider.cmp(&b.provider).then_with(|| a.display_name.cmp(&b.display_name)));
    models
}

/// Efforts a model accepts: the ids of its v2 variants. Listing variant ids
/// directly covers variants that do not map `settings.reasoningEffort`, and
/// keeps what the picker shows in sync with what a session accepts.
fn variant_efforts(variants: &[Value]) -> Vec<String> {
    let mut efforts: Vec<String> = Vec::new();
    for variant in variants {
        let Some(id) = variant["id"].as_str().map(str::trim).filter(|id| !id.is_empty()) else { continue };
        if !efforts.iter().any(|seen| seen == id) {
            efforts.push(id.to_string());
        }
    }
    efforts
}

const VARIANT_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(600);
const VARIANT_CACHE_MISS_TTL: std::time::Duration = std::time::Duration::from_secs(60);

type VariantCacheKey = (PathBuf, BTreeMap<String, String>);
type VariantCache = HashMap<VariantCacheKey, (std::time::Instant, HashMap<String, Vec<Value>>)>;

fn cached_variants(cache: &VariantCache, key: &VariantCacheKey, now: std::time::Instant) -> Option<HashMap<String, Vec<Value>>> {
    cache.get(key).and_then(|(at, catalog)| {
        let ttl = if catalog.is_empty() { VARIANT_CACHE_MISS_TTL } else { VARIANT_CACHE_TTL };
        (now.duration_since(*at) < ttl).then(|| catalog.clone())
    })
}

async fn variant_catalog(bin: &Path, context: &ProbeContext) -> HashMap<String, Vec<Value>> {
    static CACHE: std::sync::Mutex<Option<VariantCache>> = std::sync::Mutex::new(None);
    static PROBE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let key = (bin.to_path_buf(), context.env.clone());
    if let Some(catalog) = CACHE.lock().unwrap().as_ref().and_then(|cache| cached_variants(cache, &key, std::time::Instant::now())) {
        return catalog;
    }
    let _flight = PROBE.lock().await;
    if let Some(catalog) = CACHE.lock().unwrap().as_ref().and_then(|cache| cached_variants(cache, &key, std::time::Instant::now())) {
        return catalog;
    }
    let catalog = read_variant_catalog(bin, &context.env).await.unwrap_or_default();
    CACHE.lock().unwrap().get_or_insert_with(HashMap::new).insert(key, (std::time::Instant::now(), catalog.clone()));
    catalog
}

/// Parse `/api/model` variants into a catalog keyed by `providerID/id`.
fn parse_model_catalog(models: &[Value]) -> HashMap<String, Vec<Value>> {
    let mut catalog = HashMap::new();
    for model in models {
        let Some(id) = model["id"].as_str() else { continue };
        let key = match model["providerID"].as_str() {
            Some(provider) if !provider.is_empty() => format!("{provider}/{id}"),
            _ => id.to_string(),
        };
        if let Some(variants) = model["variants"].as_array() {
            catalog.insert(key, variants.clone());
        }
    }
    catalog
}

/// One fixed, per-user folder for the variant probe. OpenCode 2 records every
/// folder it serves as a project, so a fresh folder per probe would add a
/// project to the user's OpenCode on every refresh. The folder is kept: a
/// server still exiting holds it as its working directory on Windows.
fn probe_folder(env: &BTreeMap<String, String>) -> PathBuf {
    let var = |key: &str| env.get(key).cloned().or_else(|| std::env::var(key).ok()).filter(|value| !value.is_empty()).map(PathBuf::from);
    // The system temp folder is per-user on macOS and Windows, shared on Linux.
    let base = if cfg!(target_os = "linux") {
        var("XDG_CACHE_HOME").or_else(|| var("HOME").map(|home| home.join(".cache"))).unwrap_or_else(std::env::temp_dir)
    } else {
        std::env::temp_dir()
    };
    base.join("kybern").join("opencode-probe")
}

async fn read_variant_catalog(bin: &Path, env: &BTreeMap<String, String>) -> Option<HashMap<String, Vec<Value>>> {
    let folder = probe_folder(env);
    tokio::fs::create_dir_all(&folder).await.ok()?;
    let models = tokio::time::timeout(MODEL_DISCOVERY_TIMEOUT, async {
        let env = env.iter().collect::<Vec<_>>();
        let server = v2::Server::start(bin, Some(&folder), env, &[]).await.ok()?;
        server.call(reqwest::Method::GET, "/api/model", None).await.ok()?["data"].as_array().cloned()
    })
    .await
    .ok()
    .flatten();
    Some(parse_model_catalog(models.unwrap_or_default().as_slice()))
}

/// Merge v2 variant efforts into the CLI model catalog. Rows join on the
/// full `provider/model` id; v1 and failed probes keep the plain catalog.
fn merge_efforts(models: &mut [ProviderModel], catalog: &HashMap<String, Vec<Value>>) {
    for model in models {
        if !model.efforts.is_empty() {
            continue;
        }
        let efforts = catalog.get(&model.id).map(|variants| variant_efforts(variants)).unwrap_or_default();
        if !efforts.is_empty() {
            model.efforts = efforts;
        }
    }
}
/// Which server API an installed OpenCode speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Api {
    V1,
    V2,
}

/// The API for a parsed version, or why Kybern cannot drive it.
fn api_for(version: (u64, u64, u64)) -> std::result::Result<Api, String> {
    let shown = format!("{}.{}.{}", version.0, version.1, version.2);
    match version {
        (major, ..) if major >= 3 => Err(format!(
            "OpenCode {shown} is newer than this Kybern supports. Update Kybern, or install OpenCode 2 with `opencode upgrade {}.{}.{}`",
            MIN_V2_VERSION.0, MIN_V2_VERSION.1, MIN_V2_VERSION.2
        )),
        (2, ..) if version < MIN_V2_VERSION => Err(format!(
            "OpenCode {shown} has an early 2.x server API. Update with `opencode upgrade` (Kybern needs {}.{}.{} or later)",
            MIN_V2_VERSION.0, MIN_V2_VERSION.1, MIN_V2_VERSION.2
        )),
        (2, ..) => Ok(Api::V2),
        _ if version < MIN_VERSION => {
            Err(format!("OpenCode {shown} is older than the required {}.{}.{}", MIN_VERSION.0, MIN_VERSION.1, MIN_VERSION.2))
        }
        _ => Ok(Api::V1),
    }
}

/// Binary path -> (modification time, parsed version).
type VersionCache = HashMap<PathBuf, (Option<std::time::SystemTime>, Option<(u64, u64, u64)>)>;

/// The API of `bin`, cached by path and modification time. An unreadable
/// version keeps the v1 path, as before v2 support.
pub(crate) async fn api_of(bin: &Path) -> std::result::Result<Api, String> {
    static CACHE: std::sync::Mutex<Option<VersionCache>> = std::sync::Mutex::new(None);
    let modified = std::fs::metadata(bin).and_then(|meta| meta.modified()).ok();
    let cached = CACHE.lock().unwrap().as_ref().and_then(|cache| cache.get(bin).cloned());
    let version = match cached {
        Some((stamp, version)) if stamp == modified && modified.is_some() => version,
        _ => {
            let version = version_of(bin, &["--version"]).await.and_then(|line| parse_semver(&line));
            CACHE.lock().unwrap().get_or_insert_with(HashMap::new).insert(bin.to_path_buf(), (modified, version));
            version
        }
    };
    match version {
        // Spawning never rejected an old 1.x build; only 2.x and later are gated here.
        Some(version) if version.0 >= 2 => api_for(version),
        _ => Ok(Api::V1),
    }
}

impl OpencodeDriver {
    async fn probe_inner(&self, context: &ProbeContext) -> ProviderStatus {
        let mut status = ProviderStatus {
            kind: ProviderKind::Opencode,
            display_name: ProviderKind::Opencode.display_name().into(),
            available: false,
            binary_path: None,
            version: None,
            unavailable_reason: None,
            supported_permission_modes: PermissionMode::ALL.to_vec(),
            supports_fork: true,
            supports_model_switch: true,
            supports_effort_switch: false,
            supported_efforts: vec![],
            models: vec![],
            instances: vec!["default".into()],
        };
        let bin = match resolve(ProviderKind::Opencode, context.binary.as_ref()) {
            Ok(b) => b,
            Err(e) => {
                status.unavailable_reason = Some(format!("{e}. Install with: npm install -g opencode-ai"));
                return status;
            }
        };
        status.binary_path = Some(bin.display().to_string());
        match version_of(&bin, &["--version"]).await {
            Some(v) => {
                let support = match parse_semver(&v) {
                    Some(version) => api_for(version).map(Some),
                    None if at_least(&v, MIN_VERSION) => Ok(None),
                    None => Err(format!("OpenCode {v} is older than the required {}.{}.{}", MIN_VERSION.0, MIN_VERSION.1, MIN_VERSION.2)),
                };
                status.version = Some(v);
                match support {
                    Ok(api) => {
                        status.available = true;
                        status.models = opencode_models(&bin, api, context).await;
                        if api == Some(Api::V2) {
                            let catalog = variant_catalog(&bin, context).await;
                            merge_efforts(&mut status.models, &catalog);
                            // Per-model efforts only: a global fallback list would
                            // accept an effort for models without variants, and a
                            // probe-time union changes order run to run.
                            if status.models.iter().any(|model| !model.efforts.is_empty()) {
                                status.supports_effort_switch = true;
                            }
                        }
                    }
                    Err(reason) => status.unavailable_reason = Some(reason),
                }
            }
            None => status.unavailable_reason = Some("could not run `opencode --version`".into()),
        }
        status
    }
}

/// Ask OpenCode for the effective skill catalog in this project. The CLI
/// applies OpenCode's own config, compatibility roots, and precedence rules,
/// which a filesystem walk cannot reconstruct reliably.
pub async fn discover_skills(cwd: &Path, binary: Option<&PathBuf>, env: &BTreeMap<String, String>) -> Option<Vec<SkillInfo>> {
    let bin = resolve(ProviderKind::Opencode, binary).ok()?;
    match api_of(&bin).await {
        Ok(Api::V2) => return v2::discover_skills(&bin, cwd, env).await,
        Err(_) => return None,
        Ok(Api::V1) => {}
    }
    let mut command = crate::binary::command(bin).no_window();
    command.current_dir(cwd).args(["debug", "skill"]).stdin(Stdio::null()).stderr(Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), crate::process_tree::output(&mut command)).await.ok()?.ok()?;
    if !output.status.success() || output.stdout.len() > MAX_SKILL_DISCOVERY_BYTES {
        return None;
    }
    parse_skills(&output.stdout, cwd)
}

fn parse_skills(bytes: &[u8], cwd: &Path) -> Option<Vec<SkillInfo>> {
    let entries = serde_json::from_slice::<Vec<Value>>(bytes).ok()?;
    let mut skills = Vec::new();
    let mut seen = HashSet::new();
    for entry in entries {
        let Some(name) = entry.get("name").and_then(Value::as_str).map(str::trim).filter(|name| !name.is_empty()) else { continue };
        let Some(location) = entry.get("location").and_then(Value::as_str).map(str::trim).filter(|path| !path.is_empty()) else { continue };
        if !seen.insert(name.to_ascii_lowercase()) {
            continue;
        }
        let description =
            entry.get("description").and_then(Value::as_str).map(str::trim).filter(|value| !value.is_empty()).map(str::to_string);
        let scope = if location == "<built-in>" {
            SkillScope::System
        } else {
            let path = Path::new(location);
            let resolved = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
            if resolved.starts_with(cwd) { SkillScope::Project } else { SkillScope::User }
        };
        skills.push(SkillInfo {
            name: name.to_string(),
            display_name: None,
            description,
            path: location.to_string(),
            scope,
            enabled: true,
        });
    }
    skills.sort_by_key(|skill| skill.name.to_ascii_lowercase());
    Some(skills)
}

#[async_trait]
impl AgentDriver for OpencodeDriver {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Opencode
    }

    async fn probe(&self, binary: Option<&PathBuf>) -> ProviderStatus {
        self.probe_inner(&ProbeContext { binary: binary.cloned(), ..ProbeContext::default() }).await
    }

    async fn probe_with_context(&self, context: &ProbeContext) -> ProviderStatus {
        self.probe_inner(context).await
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
        let bin = resolve(ProviderKind::Opencode, config.binary.as_ref())?;
        if api_of(&bin).await.map_err(DriverError::Unsupported)? == Api::V2 {
            return v2::spawn(&bin, config).await;
        }
        let native_config = config
            .native_tool_bridge
            .as_ref()
            .map(|bridge| opencode_config_content(config.env.get("OPENCODE_CONFIG_CONTENT"), bridge))
            .transpose()?;
        let mut cmd = crate::binary::command(&bin).no_window();
        cmd.current_dir(&config.cwd)
            .args(["serve", "--port", "0", "--hostname", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &config.env {
            cmd.env(k, v);
        }
        if let Some(native_config) = native_config {
            cmd.env("OPENCODE_CONFIG_CONTENT", native_config);
        }
        tracing::info!(bin = %bin.display(), cwd = %config.cwd.display(), "spawning opencode serve");
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn()?;
        let tree = crate::process_tree::ProcessTree::new(child.id().expect("spawned opencode"));
        let stdout = child.stdout.take().ok_or_else(|| DriverError::Protocol("no stdout".into()))?;
        let stderr = child.stderr.take().ok_or_else(|| DriverError::Protocol("no stderr".into()))?;
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                tracing::debug!(target: "provider.stderr", "{l}");
            }
        });

        // Wait for "opencode server listening on http://127.0.0.1:PORT".
        let base = {
            let mut lines = BufReader::new(stdout).lines();
            let found = tokio::time::timeout(std::time::Duration::from_secs(60), async {
                while let Ok(Some(l)) = lines.next_line().await {
                    tracing::debug!(target: "provider.stdout", "{l}");
                    if let Some(idx) = l.find("http://") {
                        let url = l[idx..].trim().trim_end_matches('/').to_string();
                        return Some((url, lines));
                    }
                }
                None
            })
            .await;
            match found {
                Ok(Some((url, mut lines))) => {
                    tokio::spawn(async move {
                        while let Ok(Some(l)) = lines.next_line().await {
                            tracing::debug!(target: "provider.stdout", "{l}");
                        }
                    });
                    url
                }
                _ => {
                    let _ = child.kill().await;
                    return Err(DriverError::Protocol("opencode serve did not announce a listening address".into()));
                }
            }
        };

        let http = reqwest::Client::builder().build().map_err(|e| DriverError::Protocol(e.to_string()))?;
        let dir = config.cwd.to_string_lossy().to_string();
        for _ in 0..50 {
            if http.get(format!("{base}/global/health")).send().await.is_ok_and(|r| r.status().is_success()) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        let (tx, rx) = mpsc::channel(1024);
        let session = Arc::new(OpencodeSession {
            http,
            base: base.clone(),
            dir: dir.clone(),
            child: Mutex::new(child),
            events: tx,
            state: Mutex::new(State {
                compact_operation: None,
                commands: Vec::new(),
                context_windows: HashMap::new(),
                session_id: None,
                model: config.model.clone(),
                coordinator_instructions: config.native_tool_bridge.as_ref().and_then(|bridge| bridge.provider_instructions()),
                mode: config.permission_mode,
                parts: HashMap::new(),
                message_roles: HashMap::new(),
                pending_images: HashMap::new(),
                turn_usage: Usage::default(),
                turn_cost: 0.0,
                turn_started: None,
                active: false,
                pending: HashMap::new(),
                current_message_id: None,
                children: HashMap::new(),
            }),
        });

        // Create or adopt the session.
        let session_id = match (&config.resume_session_id, config.fork) {
            (Some(id), false) => {
                // Verify it exists on this server.
                let r = session.http.get(format!("{base}/session/{id}")).query(&[("directory", &dir)]).send().await.map_err(net)?;
                if !r.status().is_success() {
                    return Err(DriverError::Protocol(format!("opencode session {id} not found in {dir}")));
                }
                id.clone()
            }
            (Some(id), true) => {
                // Fork keeps messages before `messageID`; the id of the first dropped turn's user message.
                let body = match config.rewind.as_ref().and_then(|r| r.drop_from.turn_id.clone()) {
                    Some(mid) => json!({ "messageID": mid }),
                    None => json!({}),
                };
                let r = session
                    .http
                    .post(format!("{base}/session/{id}/fork"))
                    .query(&[("directory", &dir)])
                    .json(&body)
                    .send()
                    .await
                    .map_err(net)?;
                let v: Value = r.json().await.map_err(net)?;
                v.get("id").and_then(|i| i.as_str()).ok_or_else(|| DriverError::Protocol("fork returned no id".into()))?.to_string()
            }
            (None, _) => {
                let mut body = json!({ "title": "kybern", "permission": ruleset(config.permission_mode) });
                if let Some((provider, model)) = config.model.as_deref().and_then(split_model) {
                    body["model"] = json!({ "providerID": provider, "id": model });
                }
                let r = session
                    .http
                    .post(format!("{base}/session"))
                    .query(&[("directory", &dir)])
                    .json(&body)
                    .timeout(std::time::Duration::from_secs(600))
                    .send()
                    .await
                    .map_err(net)?;
                let v: Value = r.json().await.map_err(net)?;
                v.get("id")
                    .and_then(|i| i.as_str())
                    .ok_or_else(|| DriverError::Protocol(format!("session create failed: {v}")))?
                    .to_string()
            }
        };
        session.state.lock().await.session_id = Some(session_id.clone());
        session.emit(DriverEvent::SessionBound { session_id, model: config.model.clone() }).await;

        let reader = session.clone();
        let worker = crate::process_tree::SessionTask(tokio::spawn(async move {
            tokio::join!(reader.discover_metadata(), reader.clone().event_loop());
        }));

        Ok(SpawnedSession { session: Box::new(Handle(session, std::sync::Mutex::new(Some(tree)), Some(worker))), events: rx })
    }
}

fn net(e: reqwest::Error) -> DriverError {
    DriverError::Protocol(format!("opencode http: {e}"))
}

fn split_model(s: &str) -> Option<(String, String)> {
    let (p, m) = s.split_once('/')?;
    Some((p.to_string(), m.to_string()))
}

fn opencode_config_content(base: Option<&String>, bridge: &NativeToolBridge) -> Result<String> {
    let mut config = mcp_config(base, bridge)?;
    let config_object = config.as_object_mut().expect("mcp_config returns an object");
    if bridge.restrictions.require_enforcement {
        let tools = config_object.entry("tools").or_insert_with(|| json!({}));
        let Some(tool_permissions) = tools.as_object_mut() else {
            return Err(DriverError::Unsupported("OpenCode inline config has a non-object tools section".into()));
        };
        // OpenCode registers remote MCP tools with the server name as a prefix.
        // The gateway's tools/list response applies the exact bridge allow/deny
        // set, so this wildcard only admits that already-filtered server.
        tool_permissions.insert("*".into(), json!(false));
        tool_permissions.insert(format!("{}_*", bridge.server_name), json!(true));
        if bridge.restrictions.allow_read_only_builtins {
            for tool in OPENCODE_READ_ONLY_TOOLS {
                tool_permissions.insert((*tool).into(), json!(true));
            }
        }
    }
    serde_json::to_string(&config).map_err(|error| DriverError::Other(error.into()))
}

/// The inline config with the Kybern MCP server added. Both APIs read this
/// v1 `mcp` shape.
fn mcp_config(base: Option<&String>, bridge: &NativeToolBridge) -> Result<Value> {
    bridge.validate()?;
    let endpoint =
        bridge.endpoint.as_deref().ok_or_else(|| DriverError::Unsupported("OpenCode Kybern tools require a daemon MCP endpoint".into()))?;
    let authorization = bridge
        .authorization
        .as_deref()
        .ok_or_else(|| DriverError::Unsupported("OpenCode Kybern tools require a scoped MCP authorization capability".into()))?;
    let mut config = match base {
        Some(raw) => serde_json::from_str::<Value>(raw)
            .map_err(|error| DriverError::Unsupported(format!("OPENCODE_CONFIG_CONTENT is not valid JSON: {error}")))?,
        None => json!({}),
    };
    let Some(config_object) = config.as_object_mut() else {
        return Err(DriverError::Unsupported("OpenCode inline config must be a JSON object".into()));
    };
    let mcp = config_object.entry("mcp").or_insert_with(|| json!({}));
    let Some(mcp_servers) = mcp.as_object_mut() else {
        return Err(DriverError::Unsupported("OpenCode inline config has a non-object mcp section".into()));
    };
    mcp_servers.insert(
        bridge.server_name.clone(),
        json!({
            "type": "remote",
            "url": endpoint,
            "enabled": true,
            "oauth": false,
            "headers": { "Authorization": format!("Bearer {authorization}") },
        }),
    );
    Ok(config)
}

/// Built-in tools a project Coordinator keeps: reading, searching and web lookups,
/// plus asking the user a question and running a skill. None of them edits a file.
const OPENCODE_READ_ONLY_TOOLS: &[&str] = &["read", "grep", "glob", "list", "webfetch", "question", "skill"];

/// Per-session permission ruleset for a kybern mode. Last matching rule wins.
fn ruleset(mode: PermissionMode) -> Value {
    let rule = |permission: &str, action: &str| json!({ "permission": permission, "pattern": "*", "action": action });
    match mode {
        PermissionMode::Supervised => json!([
            rule("*", "ask"),
            rule("read", "allow"),
            rule("glob", "allow"),
            rule("grep", "allow"),
            rule("list", "allow"),
            rule("todowrite", "allow"),
            rule("question", "allow"),
            rule("skill", "allow"),
            rule("task", "allow"),
            rule("lsp", "allow"),
        ]),
        PermissionMode::AcceptEdits => json!([
            rule("*", "allow"),
            rule("bash", "ask"),
            rule("webfetch", "ask"),
            rule("websearch", "ask"),
            rule("external_directory", "ask"),
            rule("doom_loop", "ask"),
        ]),
        PermissionMode::Auto => json!([rule("*", "allow"), rule("external_directory", "ask"), rule("doom_loop", "ask")]),
        PermissionMode::FullAccess => json!([rule("*", "allow")]),
    }
}

struct PartInfo {
    kind: String,
    message_id: String,
    started: bool,
}

#[derive(Default)]
struct ChildRuntime {
    task_id: String,
    seen_tool_parts: HashSet<String>,
    stats: RuntimeTaskStats,
    stop_requested: bool,
    terminal: bool,
    /// Message id -> role, so only the subagent's own (assistant) output is
    /// forwarded; its first message is the prompt it was launched with.
    message_roles: HashMap<String, String>,
    parts: HashMap<String, ChildPart>,
    /// Parts that arrived before their message's role, replayed once it is known.
    held_parts: Vec<Value>,
}

#[derive(Default)]
struct ChildPart {
    kind: String,
    message_id: String,
    started: bool,
    finished: bool,
}

/// How many parts may wait for their message's role.
const CHILD_HELD_PARTS: usize = 64;

fn opencode_child_status(status: &str, backgrounded: bool) -> RuntimeTaskStatus {
    match status {
        "error" | "failed" => RuntimeTaskStatus::Failed,
        "cancelled" | "canceled" | "aborted" | "stopped" => RuntimeTaskStatus::Stopped,
        // A background task's parent tool returns immediately. The child
        // session's own idle/error event is the authoritative terminal edge.
        "completed" if !backgrounded => RuntimeTaskStatus::Completed,
        "pending" => RuntimeTaskStatus::Pending,
        _ => RuntimeTaskStatus::Running,
    }
}

fn opencode_model(metadata: &Value) -> Option<String> {
    let model = metadata.get("model")?;
    let provider = model.get("providerID").and_then(Value::as_str)?;
    let id = model.get("modelID").and_then(Value::as_str)?;
    Some(format!("{provider}/{id}"))
}

/// OpenCode's native `task` tool publishes the child session id in part
/// metadata. The SSE stream then carries that child's status and tool events.
fn opencode_child_task(part: &Value, parent_id: Option<String>) -> Option<(String, DriverRuntimeTask)> {
    if part.get("type").and_then(Value::as_str) != Some("tool") || part.get("tool").and_then(Value::as_str) != Some("task") {
        return None;
    }
    let state = part.get("state")?;
    let metadata = state.get("metadata")?;
    let child_id = metadata.get("sessionId").and_then(Value::as_str)?.to_string();
    let input = state.get("input").unwrap_or(&Value::Null);
    let backgrounded =
        metadata.get("background").and_then(Value::as_bool) == Some(true) || input.get("background").and_then(Value::as_bool) == Some(true);
    let raw_status = state.get("status").and_then(Value::as_str).unwrap_or("running");
    let title = state
        .get("title")
        .or_else(|| input.get("description"))
        .or_else(|| input.get("prompt"))
        .and_then(Value::as_str)
        .map(|value| value.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or(value).chars().take(120).collect())
        .filter(|value: &String| !value.is_empty())
        .unwrap_or_else(|| "Subagent".into());
    let provider_type =
        input.get("subagent_type").and_then(Value::as_str).map(|value| format!("task:{value}")).or_else(|| Some("task".into()));
    let status = opencode_child_status(raw_status, backgrounded);
    let task = DriverRuntimeTask {
        id: format!("opencode:{child_id}"),
        kind: RuntimeTaskKind::Agent,
        status,
        title,
        detail: None,
        provider_type,
        parent_id,
        tool_call_id: part.get("callID").and_then(Value::as_str).map(str::to_string),
        provider_thread_id: Some(child_id.clone()),
        model: opencode_model(metadata),
        effort: None,
        backgrounded,
        last_tool_name: None,
        usage: None,
        stats: RuntimeTaskStats::default(),
        capabilities: RuntimeTaskCapabilities { stop: status.is_active(), background: false },
    };
    Some((child_id, task))
}

fn opencode_child_session(info: &Value, parent_id: Option<String>) -> Option<(String, DriverRuntimeTask)> {
    let child_id = info.get("id").and_then(Value::as_str)?.to_string();
    let provider_type = info.get("agent").and_then(Value::as_str).map(|value| format!("task:{value}"));
    let title = info
        .get("title")
        .and_then(Value::as_str)
        .map(|value| value.rsplit_once(" (@").map_or(value, |(description, _)| description).trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Subagent".into());
    Some((
        child_id.clone(),
        DriverRuntimeTask {
            id: format!("opencode:{child_id}"),
            kind: RuntimeTaskKind::Agent,
            status: RuntimeTaskStatus::Running,
            title,
            detail: None,
            provider_type,
            parent_id,
            tool_call_id: None,
            provider_thread_id: Some(child_id),
            model: None,
            effort: None,
            backgrounded: false,
            last_tool_name: None,
            usage: None,
            stats: RuntimeTaskStats::default(),
            capabilities: RuntimeTaskCapabilities { stop: true, background: false },
        },
    ))
}

struct State {
    compact_operation: Option<String>,
    context_windows: HashMap<String, u64>,
    commands: Vec<kybern_protocol::ProviderCommand>,
    session_id: Option<String>,
    model: Option<String>,
    /// Stable role supplied on each coordinator prompt. OpenCode uses the
    /// latest user message's system field, not earlier message metadata.
    coordinator_instructions: Option<String>,
    mode: PermissionMode,
    parts: HashMap<String, PartInfo>,
    message_roles: HashMap<String, String>,
    pending_images: HashMap<String, Value>,
    turn_usage: Usage,
    turn_cost: f64,
    turn_started: Option<std::time::Instant>,
    active: bool,
    /// permission id -> session id
    pending: HashMap<String, String>,
    /// Client-chosen OpenCode message id of the current turn's user message.
    current_message_id: Option<String>,
    /// Provider child session id -> runtime bookkeeping. Child output stays out
    /// of the parent transcript; only compact lifecycle/progress is projected.
    children: HashMap<String, ChildRuntime>,
}

struct OpencodeSession {
    http: reqwest::Client,
    base: String,
    dir: String,
    child: Mutex<Child>,
    events: mpsc::Sender<DriverEvent>,
    state: Mutex<State>,
}

struct Handle(
    Arc<OpencodeSession>,
    #[allow(dead_code)] std::sync::Mutex<Option<crate::process_tree::ProcessTree>>,
    #[allow(dead_code)] Option<crate::process_tree::SessionTask>,
);

impl OpencodeSession {
    async fn discover_metadata(&self) {
        let read = |path: &'static str| async move {
            let response = self
                .http
                .get(format!("{}{path}", self.base))
                .query(&[("directory", &self.dir)])
                .timeout(std::time::Duration::from_secs(3))
                .send()
                .await
                .ok()?;
            if !response.status().is_success() {
                return None;
            }
            response.json::<Value>().await.ok()
        };
        let (commands, providers) = tokio::join!(read("/command"), read("/provider"));
        if let Some(value) = commands {
            let commands = crate::provider_commands(&value);
            self.state.lock().await.commands = commands.clone();
            self.emit(DriverEvent::CommandsUpdated(commands)).await;
        }
        if let Some(value) = providers {
            let mut windows = HashMap::new();
            for provider in value["all"].as_array().into_iter().flatten() {
                if let (Some(id), Some(models)) = (provider["id"].as_str(), provider["models"].as_object()) {
                    for (model, value) in models {
                        if let Some(window) = value.pointer("/limit/context").and_then(Value::as_u64).filter(|w| *w > 0) {
                            windows.insert(format!("{id}/{model}"), window);
                        }
                    }
                }
            }
            self.state.lock().await.context_windows = windows;
        }
    }

    async fn emit(&self, ev: DriverEvent) {
        let _ = self.events.send(ev).await;
    }

    async fn observe_child(&self, child_id: String, task: DriverRuntimeTask) {
        let task_id = task.id.clone();
        let mut state = self.state.lock().await;
        state.children.entry(child_id).or_default().task_id = task_id;
        drop(state);
        self.emit(DriverEvent::RuntimeTaskStarted(task)).await;
    }

    async fn update_child(
        &self,
        child_id: &str,
        status: Option<RuntimeTaskStatus>,
        detail: Option<String>,
        last_tool_name: Option<String>,
        stats: Option<RuntimeTaskStats>,
        terminal: bool,
    ) {
        let task_id = {
            let mut state = self.state.lock().await;
            let Some(child) = state.children.get_mut(child_id) else { return };
            if child.terminal {
                return;
            }
            child.terminal = terminal;
            child.task_id.clone()
        };
        let update = DriverRuntimeTaskUpdate {
            id: task_id,
            status,
            detail,
            backgrounded: None,
            last_tool_name,
            usage: None,
            stats,
            capabilities: terminal.then(RuntimeTaskCapabilities::default),
        };
        self.emit(if terminal { DriverEvent::RuntimeTaskCompleted(update) } else { DriverEvent::RuntimeTaskUpdated(update) }).await;
    }

    async fn stop_child(&self, session_id: &str) -> Result<()> {
        {
            let mut state = self.state.lock().await;
            let child = state
                .children
                .get_mut(session_id)
                .ok_or_else(|| DriverError::Unsupported("OpenCode child session is no longer active".into()))?;
            if child.terminal {
                return Err(DriverError::Unsupported("OpenCode child session is no longer active".into()));
            }
            child.stop_requested = true;
        }
        if let Err(error) = self.post(&format!("/session/{session_id}/abort"), json!({})).await {
            if let Some(child) = self.state.lock().await.children.get_mut(session_id) {
                child.stop_requested = false;
            }
            return Err(error);
        }
        self.update_child(session_id, Some(RuntimeTaskStatus::Stopped), Some("Stopped by user".into()), None, None, true).await;
        Ok(())
    }

    async fn event_loop(self: Arc<Self>) {
        let mut backoff = 200u64;
        loop {
            let resp = self.http.get(format!("{}/event", self.base)).query(&[("directory", &self.dir)]).send().await;
            match resp {
                Ok(r) if r.status().is_success() => {
                    backoff = 200;
                    let mut stream = r.bytes_stream();
                    let mut buf = String::new();
                    while let Some(chunk) = stream.next().await {
                        let Ok(chunk) = chunk else { break };
                        buf.push_str(&String::from_utf8_lossy(&chunk));
                        while let Some(pos) = buf.find("\n\n") {
                            let frame = buf[..pos].to_string();
                            buf.drain(..pos + 2);
                            for line in frame.lines() {
                                if let Some(data) = line.strip_prefix("data:")
                                    && let Ok(v) = serde_json::from_str::<Value>(data.trim())
                                {
                                    self.handle_event(&v).await;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            // Stream dropped: is the server still alive?
            if let Ok(Some(status)) = self.child.lock().await.try_wait() {
                let code = status.code();
                self.emit(DriverEvent::Exited {
                    code,
                    error: code.filter(|c| *c != 0).map(|c| format!("opencode serve exited with code {c}")),
                })
                .await;
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
            backoff = (backoff * 2).min(5000);
        }
    }

    async fn handle_event(&self, v: &Value) {
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let p = &v["properties"];
        let my_session = self.state.lock().await.session_id.clone();

        if ty == "session.created" {
            let info = &p["info"];
            let parent_session_id = info.get("parentID").and_then(Value::as_str);
            let parent_task_id = {
                let state = self.state.lock().await;
                match (my_session.as_deref(), parent_session_id) {
                    (Some(root), Some(parent)) if root == parent => Some(None),
                    (_, Some(parent)) => state.children.get(parent).map(|child| Some(child.task_id.clone())),
                    _ => None,
                }
            };
            if let Some(parent_task_id) = parent_task_id
                && let Some((child_id, task)) = opencode_child_session(info, parent_task_id)
            {
                self.observe_child(child_id, task).await;
            }
            return;
        }

        let sid =
            p.get("sessionID").or_else(|| p.pointer("/part/sessionID")).or_else(|| p.pointer("/info/sessionID")).and_then(|s| s.as_str());
        let child_task_id = match sid {
            Some(session_id) => self.state.lock().await.children.get(session_id).map(|child| child.task_id.clone()),
            None => None,
        };
        let child_request = child_task_id.is_some()
            && matches!(ty, "question.asked" | "question.replied" | "question.rejected" | "permission.asked" | "permission.replied");
        if !child_request && let (Some(session_id), Some(task_id)) = (sid, child_task_id) {
            self.handle_child_event(session_id, &task_id, ty, p).await;
            return;
        }
        if let (Some(mine), Some(theirs)) = (&my_session, sid)
            && mine != theirs
            && !child_request
        {
            return;
        }
        match ty {
            "message.part.updated" => self.handle_part(&p["part"]).await,
            "message.part.delta" => {
                let part_id = p.get("partID").and_then(|s| s.as_str()).unwrap_or("");
                let delta = p.get("delta").and_then(|s| s.as_str()).unwrap_or("");
                let info = {
                    let st = self.state.lock().await;
                    st.parts.get(part_id).map(|i| (i.kind.clone(), i.message_id.clone()))
                };
                match info.as_ref().map(|(k, m)| (k.as_str(), m.clone())) {
                    Some(("text", message_id)) => {
                        self.emit(DriverEvent::TextDelta { message_id, origin: EventOrigin::Root, delta: delta.to_string() }).await
                    }
                    Some(("reasoning", message_id)) => {
                        self.emit(DriverEvent::ThinkingDelta { message_id, origin: EventOrigin::Root, delta: delta.to_string() }).await
                    }
                    _ => {}
                }
            }
            "message.updated" => {
                let info = &p["info"];
                if info["role"].as_str() == Some("assistant") && info.pointer("/time/completed").and_then(Value::as_u64).is_some() {
                    let model = format!("{}/{}", info["providerID"].as_str().unwrap_or(""), info["modelID"].as_str().unwrap_or(""));
                    let window = self.state.lock().await.context_windows.get(&model).copied();
                    if let (Some(window_tokens), Some(tokens)) = (window, info.get("tokens")) {
                        let used_tokens = ["/input", "/output", "/reasoning", "/cache/read", "/cache/write"]
                            .iter()
                            .filter_map(|path| tokens.pointer(path).and_then(Value::as_u64))
                            .sum();
                        self.emit(DriverEvent::UsageUpdated(kybern_protocol::ProviderUsage {
                            context: Some(kybern_protocol::ContextUsage { used_tokens, window_tokens }),
                            limits: None,
                        }))
                        .await;
                    }
                }
                if let (Some(id), Some(role)) = (info.get("id").and_then(Value::as_str), info.get("role").and_then(Value::as_str)) {
                    let pending = {
                        let mut st = self.state.lock().await;
                        st.message_roles.insert(id.to_owned(), role.to_owned());
                        let ids: Vec<_> = st
                            .pending_images
                            .iter()
                            .filter(|(_, part)| part.get("messageID").and_then(Value::as_str) == Some(id))
                            .map(|(id, _)| id.clone())
                            .collect();
                        ids.into_iter().filter_map(|id| st.pending_images.remove(&id)).collect::<Vec<_>>()
                    };
                    for part in pending {
                        self.handle_part(&part).await;
                    }
                }
                if info.get("role").and_then(|r| r.as_str()) == Some("assistant")
                    && let Some(err) = info.get("error")
                    && !err.is_null()
                {
                    let msg = err.pointer("/data/message").and_then(|m| m.as_str()).unwrap_or("provider error");
                    self.emit(DriverEvent::Notice { level: NoticeLevel::Error, text: msg.to_string(), data: Some(err.clone()) }).await;
                }
            }
            "permission.asked" => {
                let id = p.get("id").and_then(|s| s.as_str()).unwrap_or("").to_string();
                let permission = p.get("permission").and_then(|s| s.as_str()).unwrap_or("tool").to_string();
                let metadata = p.get("metadata").cloned().unwrap_or(Value::Null);
                let patterns = p.get("patterns").cloned().unwrap_or(Value::Null);
                let input = json!({ "permission": permission, "patterns": patterns, "metadata": metadata });
                let summary = match metadata.get("command").and_then(|c| c.as_str()) {
                    Some(c) => format!("run: {}", c.lines().next().unwrap_or("").chars().take(120).collect::<String>()),
                    None => match patterns.as_array().and_then(|a| a.first()).and_then(|f| f.as_str()) {
                        Some(pat) => format!("{permission}: {pat}"),
                        None => summarize_tool_call(&permission, &metadata),
                    },
                };
                let suggestions = p.get("always").and_then(|a| a.as_array()).cloned().unwrap_or_default();
                let tool_call_id = p.pointer("/tool/callID").and_then(|c| c.as_str()).map(str::to_string);
                if let Some(s) = sid.or(my_session.as_deref()) {
                    self.state.lock().await.pending.insert(id.clone(), s.to_owned());
                }
                self.emit(DriverEvent::PermissionRequest {
                    request_id: id,
                    tool_call_id,
                    tool_name: permission,
                    input,
                    summary,
                    suggestions,
                })
                .await;
            }
            "permission.replied" => {
                let id = p.get("requestID").and_then(|s| s.as_str()).unwrap_or("").to_string();
                if self.state.lock().await.pending.remove(&id).is_some() {
                    self.emit(DriverEvent::PermissionWithdrawn { request_id: id }).await;
                }
            }
            "question.asked" => {
                let id = p.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
                self.state.lock().await.pending.insert(id.clone(), "__question".into());
                self.emit(DriverEvent::PermissionRequest {
                    request_id: id,
                    tool_call_id: p.pointer("/tool/callID").and_then(Value::as_str).map(str::to_owned),
                    tool_name: "opencode_question".into(),
                    input: p.clone(),
                    summary: "Answer the agent's questions".into(),
                    suggestions: vec![],
                })
                .await;
            }
            "question.replied" | "question.rejected" => {
                let id = p.get("requestID").and_then(Value::as_str).unwrap_or("").to_owned();
                if self.state.lock().await.pending.remove(&id).is_some() {
                    self.emit(DriverEvent::PermissionWithdrawn { request_id: id }).await;
                }
            }
            "session.status" => {
                let status = p.pointer("/status/type").and_then(|s| s.as_str()).unwrap_or("");
                let mut st = self.state.lock().await;
                match status {
                    "busy" if !st.active => {
                        st.active = true;
                        st.turn_started = Some(std::time::Instant::now());
                        st.turn_usage = Usage::default();
                        st.turn_cost = 0.0;
                    }
                    "retry" => {
                        let msg = p.pointer("/status/message").and_then(|m| m.as_str()).unwrap_or("retrying").to_string();
                        drop(st);
                        self.emit(DriverEvent::Notice { level: NoticeLevel::Warning, text: msg, data: None }).await;
                    }
                    _ => {}
                }
            }
            "session.idle" => {
                let (usage, cost, duration_ms, active, anchors) = {
                    let mut st = self.state.lock().await;
                    let active = st.active;
                    st.compact_operation = None;
                    st.active = false;
                    let d = st.turn_started.take().map(|t| t.elapsed().as_millis() as u64).unwrap_or(0);
                    let anchors = crate::TurnAnchors { turn_id: st.current_message_id.take(), previous_end: None };
                    (std::mem::take(&mut st.turn_usage), st.turn_cost, d, active, anchors)
                };
                if active {
                    self.emit(DriverEvent::TurnCompleted {
                        stop_reason: StopReason::Completed,
                        usage,
                        cost_usd: Some(cost),
                        duration_ms,
                        anchors,
                    })
                    .await;
                }
            }
            "session.error" => {
                let err = &p["error"];
                let name = err.get("name").and_then(|n| n.as_str()).unwrap_or("error");
                let msg = err.pointer("/data/message").and_then(|m| m.as_str()).unwrap_or("");
                let active = {
                    let mut st = self.state.lock().await;
                    let a = st.active;
                    st.compact_operation = None;
                    st.active = false;
                    st.turn_started = None;
                    a
                };
                if name == "MessageAbortedError" {
                    if active {
                        self.emit(DriverEvent::TurnCompleted {
                            stop_reason: StopReason::Interrupted,
                            usage: Usage::default(),
                            cost_usd: None,
                            duration_ms: 0,
                            anchors: crate::TurnAnchors::default(),
                        })
                        .await;
                    }
                } else if active {
                    self.emit(DriverEvent::TurnFailed { error: format!("{name}: {msg}") }).await;
                } else {
                    self.emit(DriverEvent::Notice { level: NoticeLevel::Error, text: format!("{name}: {msg}"), data: Some(err.clone()) })
                        .await;
                }
            }
            "session.compacted" => {
                self.emit(DriverEvent::Notice { level: NoticeLevel::Info, text: "context compacted".into(), data: None }).await
            }
            "todo.updated" => {
                self.emit(DriverEvent::Notice { level: NoticeLevel::Info, text: "todo list updated".into(), data: Some(p.clone()) }).await
            }
            _ => {}
        }
    }

    async fn handle_child_event(&self, session_id: &str, task_id: &str, ty: &str, properties: &Value) {
        match ty {
            "message.part.updated" => self.handle_child_part(session_id, task_id, &properties["part"]).await,
            "message.updated" => self.handle_child_message(session_id, task_id, &properties["info"]).await,
            "message.part.delta" => {
                let part_id = properties.get("partID").and_then(Value::as_str).unwrap_or("");
                let delta = properties.get("delta").and_then(Value::as_str).unwrap_or("");
                let part = {
                    let state = self.state.lock().await;
                    state
                        .children
                        .get(session_id)
                        .and_then(|child| child.parts.get(part_id))
                        .map(|part| (part.kind.clone(), part.message_id.clone()))
                };
                let event = match part.as_ref().map(|(kind, message_id)| (kind.as_str(), message_id.clone())) {
                    Some(("text", message_id)) => {
                        DriverEvent::TextDelta { message_id, origin: EventOrigin::Root, delta: delta.to_string() }
                    }
                    Some(("reasoning", message_id)) => {
                        DriverEvent::ThinkingDelta { message_id, origin: EventOrigin::Root, delta: delta.to_string() }
                    }
                    _ => return,
                };
                self.emit_child(task_id, event).await;
            }
            "session.status" => {
                let status = properties.pointer("/status/type").and_then(Value::as_str).unwrap_or("");
                match status {
                    "busy" => self.update_child(session_id, Some(RuntimeTaskStatus::Running), None, None, None, false).await,
                    "retry" => {
                        let detail = properties.pointer("/status/message").and_then(Value::as_str).map(str::to_string);
                        self.update_child(session_id, Some(RuntimeTaskStatus::Waiting), detail, None, None, false).await;
                    }
                    _ => {}
                }
            }
            "session.idle" => {
                let stop_requested = self.state.lock().await.children.get(session_id).is_some_and(|child| child.stop_requested);
                let status = if stop_requested { RuntimeTaskStatus::Stopped } else { RuntimeTaskStatus::Completed };
                self.update_child(session_id, Some(status), None, None, None, true).await;
            }
            "session.error" => {
                let error = &properties["error"];
                let name = error.get("name").and_then(Value::as_str).unwrap_or("error");
                let detail = error.pointer("/data/message").and_then(Value::as_str).map(str::to_string);
                let stop_requested = self.state.lock().await.children.get(session_id).is_some_and(|child| child.stop_requested);
                let status =
                    if stop_requested || name == "MessageAbortedError" { RuntimeTaskStatus::Stopped } else { RuntimeTaskStatus::Failed };
                self.update_child(session_id, Some(status), detail, None, None, true).await;
            }
            _ => {}
        }
    }

    async fn emit_child(&self, task_id: &str, event: DriverEvent) {
        self.emit(DriverEvent::SubagentOnly { task_id: task_id.to_string(), event: Box::new(event) }).await;
    }

    /// A subagent session announced a message. Its role decides whether its
    /// parts are the subagent's own output or the prompt it was given.
    async fn handle_child_message(&self, session_id: &str, task_id: &str, info: &Value) {
        let (Some(id), Some(role)) = (info.get("id").and_then(Value::as_str), info.get("role").and_then(Value::as_str)) else { return };
        let held = {
            let mut state = self.state.lock().await;
            let Some(child) = state.children.get_mut(session_id) else { return };
            child.message_roles.insert(id.to_owned(), role.to_owned());
            let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut child.held_parts)
                .into_iter()
                .partition(|part| part.get("messageID").and_then(Value::as_str) == Some(id));
            child.held_parts = waiting;
            ready
        };
        for part in held {
            self.forward_child_part(session_id, task_id, &part).await;
        }
    }

    /// Forward the subagent's own text, reasoning and tool calls to its thread.
    async fn forward_child_part(&self, session_id: &str, task_id: &str, part: &Value) {
        let message_id = part.get("messageID").and_then(Value::as_str).unwrap_or("").to_string();
        let id = part.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let kind = part.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        if id.is_empty() || !matches!(kind.as_str(), "text" | "reasoning" | "tool") {
            return;
        }
        {
            let mut state = self.state.lock().await;
            let Some(child) = state.children.get_mut(session_id) else { return };
            match child.message_roles.get(&message_id).map(String::as_str) {
                Some("assistant") => {}
                Some(_) => return,
                None => {
                    if child.held_parts.len() < CHILD_HELD_PARTS {
                        child.held_parts.push(part.clone());
                    }
                    return;
                }
            }
            child.parts.entry(id.clone()).or_insert_with(|| ChildPart {
                kind: kind.clone(),
                message_id: message_id.clone(),
                ..ChildPart::default()
            });
        }
        let finished_at = |part: &Value| part.pointer("/time/end").is_some_and(|end| !end.is_null());
        match kind.as_str() {
            "text" => {
                let text = part.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                let synthetic = part.get("synthetic").and_then(Value::as_bool).unwrap_or(false);
                if finished_at(part) && !synthetic && !text.is_empty() && self.set_child_part_flag(session_id, &id, false).await {
                    self.emit_child(task_id, DriverEvent::MessageCompleted { message_id, origin: EventOrigin::Root, text, thinking: None })
                        .await;
                }
            }
            "reasoning" => {
                if finished_at(part) && self.set_child_part_flag(session_id, &id, false).await {
                    self.emit_child(task_id, DriverEvent::ThinkingCompleted { message_id, origin: EventOrigin::Root }).await;
                }
            }
            "tool" => {
                let status = part.pointer("/state/status").and_then(Value::as_str).unwrap_or("");
                let call_id = part.get("callID").and_then(Value::as_str).unwrap_or(&id).to_string();
                let tool = part.get("tool").and_then(Value::as_str).unwrap_or("tool").to_string();
                let input = part.pointer("/state/input").cloned().unwrap_or(Value::Null);
                let terminal = matches!(status, "completed" | "error");
                if (status == "running" || terminal) && self.set_child_part_flag(session_id, &id, true).await {
                    self.emit_child(
                        task_id,
                        DriverEvent::ToolStarted(ToolCall { id: call_id.clone(), name: tool, input, parent_id: None }),
                    )
                    .await;
                }
                if terminal && self.set_child_part_flag(session_id, &id, false).await {
                    let output = json!({
                        "content": part.pointer("/state/output").or_else(|| part.pointer("/state/error")),
                        "title": part.pointer("/state/title"),
                        "metadata": part.pointer("/state/metadata"),
                        "attachments": part.pointer("/state/attachments"),
                    });
                    self.emit_child(task_id, DriverEvent::ToolCompleted { tool_call_id: call_id, output, is_error: status == "error" })
                        .await;
                }
            }
            _ => {}
        }
    }

    /// Set the started (`started`) or finished flag of a subagent part. True
    /// only the first time, so each event is forwarded once.
    async fn set_child_part_flag(&self, session_id: &str, part_id: &str, started: bool) -> bool {
        let mut state = self.state.lock().await;
        let Some(part) = state.children.get_mut(session_id).and_then(|child| child.parts.get_mut(part_id)) else { return false };
        let flag = if started { &mut part.started } else { &mut part.finished };
        !std::mem::replace(flag, true)
    }

    async fn handle_child_part(&self, session_id: &str, owner_task_id: &str, part: &Value) {
        if let Some((child_id, task)) = opencode_child_task(part, Some(owner_task_id.to_string())) {
            self.observe_child(child_id, task).await;
        }
        self.forward_child_part(session_id, owner_task_id, part).await;

        match part.get("type").and_then(Value::as_str).unwrap_or("") {
            "tool" => {
                let status = part.pointer("/state/status").and_then(Value::as_str).unwrap_or("");
                if !matches!(status, "running" | "completed" | "error") {
                    return;
                }
                let part_id = part.get("id").and_then(Value::as_str).unwrap_or("");
                let tool = part.get("tool").and_then(Value::as_str).unwrap_or("tool").to_string();
                let stats = {
                    let mut state = self.state.lock().await;
                    let Some(child) = state.children.get_mut(session_id) else { return };
                    if !part_id.is_empty() && child.seen_tool_parts.insert(part_id.to_string()) {
                        child.stats.tool_uses = Some(child.stats.tool_uses.unwrap_or(0) + 1);
                    }
                    child.stats.clone()
                };
                self.update_child(session_id, Some(RuntimeTaskStatus::Running), None, Some(tool), Some(stats), false).await;
            }
            "step-finish" => {
                let step_tokens = part
                    .get("tokens")
                    .map(|tokens| {
                        tokens.get("input").and_then(Value::as_u64).unwrap_or(0)
                            + tokens.get("output").and_then(Value::as_u64).unwrap_or(0)
                            + tokens.get("reasoning").and_then(Value::as_u64).unwrap_or(0)
                            + tokens.pointer("/cache/write").and_then(Value::as_u64).unwrap_or(0)
                    })
                    .unwrap_or(0);
                let stats = {
                    let mut state = self.state.lock().await;
                    let Some(child) = state.children.get_mut(session_id) else { return };
                    child.stats.token_count = Some(child.stats.token_count.unwrap_or(0) + step_tokens);
                    child.stats.clone()
                };
                self.update_child(session_id, Some(RuntimeTaskStatus::Running), None, None, Some(stats), false).await;
            }
            _ => {}
        }
    }

    async fn handle_part(&self, part: &Value) {
        if let Some((child_id, task)) = opencode_child_task(part, None) {
            self.observe_child(child_id, task).await;
        }
        let id = part.get("id").and_then(|s| s.as_str()).unwrap_or("").to_string();
        let kind = part.get("type").and_then(|s| s.as_str()).unwrap_or("").to_string();
        let message_id = part.get("messageID").and_then(|s| s.as_str()).unwrap_or("").to_string();
        let mut st = self.state.lock().await;
        if kind == "file" {
            match st.message_roles.get(&message_id).map(String::as_str) {
                Some("assistant") => {}
                Some(_) => return,
                None => {
                    st.pending_images.insert(id, part.clone());
                    return;
                }
            }
        }
        let info =
            st.parts.entry(id.clone()).or_insert_with(|| PartInfo { kind: kind.clone(), message_id: message_id.clone(), started: false });
        match kind.as_str() {
            "file" if !info.started && part.get("mime").and_then(Value::as_str).is_some_and(|mime| mime.starts_with("image/")) => {
                info.started = true;
                drop(st);
                if let Some(source) = part.get("url").and_then(Value::as_str) {
                    self.emit(DriverEvent::ImageReceived { id, origin: EventOrigin::Root, source: source.to_owned() }).await;
                }
            }
            "text" => {
                let text = part.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
                let finished = part.pointer("/time/end").is_some_and(|e| !e.is_null());
                let synthetic = part.get("synthetic").and_then(|b| b.as_bool()).unwrap_or(false);
                drop(st);
                if finished && !synthetic && !text.is_empty() {
                    self.emit(DriverEvent::MessageCompleted { message_id, origin: EventOrigin::Root, text, thinking: None }).await;
                }
            }
            "reasoning" => {
                // `started` marks the end as reported; the part updates again later.
                if !info.started && part.pointer("/time/end").is_some_and(|end| !end.is_null()) {
                    info.started = true;
                    drop(st);
                    self.emit(DriverEvent::ThinkingCompleted { message_id, origin: EventOrigin::Root }).await;
                }
            }
            "tool" => {
                let status = part.pointer("/state/status").and_then(|s| s.as_str()).unwrap_or("");
                let call_id = part.get("callID").and_then(|s| s.as_str()).unwrap_or(&id).to_string();
                let tool = part.get("tool").and_then(|s| s.as_str()).unwrap_or("tool").to_string();
                let input = part.pointer("/state/input").cloned().unwrap_or(Value::Null);
                match status {
                    "running" if !info.started => {
                        info.started = true;
                        drop(st);
                        self.emit(DriverEvent::ToolStarted(ToolCall { id: call_id, name: tool, input, parent_id: None })).await;
                    }
                    "completed" | "error" => {
                        let started = info.started;
                        info.started = true;
                        drop(st);
                        if !started {
                            self.emit(DriverEvent::ToolStarted(ToolCall { id: call_id.clone(), name: tool, input, parent_id: None })).await;
                        }
                        let output = json!({
                            "content": part.pointer("/state/output").or_else(|| part.pointer("/state/error")),
                            "title": part.pointer("/state/title"),
                            "metadata": part.pointer("/state/metadata"),
                            "attachments": part.pointer("/state/attachments"),
                        });
                        self.emit(DriverEvent::ToolCompleted { tool_call_id: call_id, output, is_error: status == "error" }).await;
                    }
                    _ => {}
                }
            }
            "step-finish" => {
                if let Some(t) = part.get("tokens") {
                    st.turn_usage.input_tokens += t.get("input").and_then(|v| v.as_u64()).unwrap_or(0);
                    st.turn_usage.output_tokens +=
                        t.get("output").and_then(|v| v.as_u64()).unwrap_or(0) + t.get("reasoning").and_then(|v| v.as_u64()).unwrap_or(0);
                    st.turn_usage.cache_read_tokens += t.pointer("/cache/read").and_then(|v| v.as_u64()).unwrap_or(0);
                    st.turn_usage.cache_write_tokens += t.pointer("/cache/write").and_then(|v| v.as_u64()).unwrap_or(0);
                }
                st.turn_cost += part.get("cost").and_then(|c| c.as_f64()).unwrap_or(0.0);
            }
            _ => {}
        }
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        let r = self
            .http
            .post(format!("{}{path}", self.base))
            .query(&[("directory", &self.dir)])
            .json(&body)
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .await
            .map_err(net)?;
        let status = r.status();
        let text = r.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(DriverError::Protocol(format!("opencode {path}: {status} {text}")));
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }
}

fn parts(message: &UserMessage) -> Vec<Value> {
    let mut out = Vec::new();
    for part in &message.parts {
        match part {
            ContentPart::Text { text } => out.push(json!({ "type": "text", "text": text })),
            ContentPart::FileMention { path } => out.push(json!({ "type": "text", "text": format!("@{path}") })),
            ContentPart::ThreadReference { thread_id, title, .. } => {
                out.push(json!({ "type": "text", "text": thread_reference_text(*thread_id, title) }))
            }
            ContentPart::Skill { name, .. } => out.push(json!({ "type": "text", "text": format!("/{name}") })),
            ContentPart::Mention { name, .. } => out.push(json!({ "type": "text", "text": format!("@{name}") })),
            ContentPart::Image { media_type, data } => out.push(
                json!({ "type": "file", "mime": media_type, "url": format!("data:{media_type};base64,{data}"), "filename": "image" }),
            ),
            ContentPart::Attachment { name, .. } => out.push(json!({ "type": "text", "text": format!("[attached file: {name}]") })),
            ContentPart::ThreadMessage { .. } | ContentPart::AgentResults { .. } | ContentPart::SystemRow { .. } => {
                if let Some(text) = part.orchestration_text() {
                    out.push(json!({ "type": "text", "text": text }));
                }
            }
        }
    }
    out
}

struct SkillCommand {
    name: String,
    arguments: String,
    files: Vec<Value>,
}

/// OpenCode exposes every discovered skill as a native session command. Use
/// that endpoint instead of hoping a `$name` text mention makes the model load
/// it. The last selected skill is the command; earlier skills remain explicit
/// slash references in its arguments, matching the single-command behavior of
/// the OpenCode and Claude harnesses.
fn skill_command(message: &UserMessage) -> Option<SkillCommand> {
    let command_index = message.parts.iter().rposition(|part| matches!(part, ContentPart::Skill { .. }))?;
    let ContentPart::Skill { name, .. } = &message.parts[command_index] else { unreachable!() };
    let mut arguments = String::new();
    let mut files = Vec::new();
    for (index, part) in message.parts.iter().enumerate() {
        if index == command_index {
            continue;
        }
        match part {
            ContentPart::Text { text } => arguments.push_str(text),
            ContentPart::FileMention { path } => {
                arguments.push('@');
                arguments.push_str(path);
            }
            ContentPart::ThreadReference { thread_id, title, .. } => {
                arguments.push_str(&thread_reference_text(*thread_id, title));
            }
            ContentPart::Skill { name, .. } => {
                arguments.push('/');
                arguments.push_str(name);
            }
            ContentPart::Mention { name, .. } => {
                arguments.push('@');
                arguments.push_str(name);
            }
            ContentPart::Image { media_type, data } => files.push(
                json!({ "type": "file", "mime": media_type, "url": format!("data:{media_type};base64,{data}"), "filename": "image" }),
            ),
            ContentPart::Attachment { name, .. } => arguments.push_str(&format!("[attached file: {name}]")),
            ContentPart::ThreadMessage { .. } | ContentPart::AgentResults { .. } | ContentPart::SystemRow { .. } => {
                if let Some(text) = part.orchestration_text() {
                    arguments.push('\n');
                    arguments.push_str(&text);
                }
            }
        }
    }
    Some(SkillCommand { name: name.clone(), arguments: arguments.trim().to_string(), files })
}

#[async_trait]
impl AgentSession for Handle {
    async fn compact(&self) -> Result<()> {
        let operation = uuid::Uuid::now_v7().to_string();
        let (id, provider, model) = {
            let mut state = self.0.state.lock().await;
            let id = state.session_id.clone().ok_or_else(|| DriverError::Protocol("no OpenCode session".into()))?;
            let (provider, model) = state
                .model
                .as_deref()
                .and_then(split_model)
                .ok_or_else(|| DriverError::Unsupported("Choose a provider/model before compacting.".into()))?;
            state.active = true;
            state.turn_started = Some(std::time::Instant::now());
            state.turn_usage = Usage::default();
            state.turn_cost = 0.0;
            state.current_message_id = None;
            state.compact_operation = Some(operation.clone());
            (id, provider, model)
        };
        self.0.emit(DriverEvent::Notice { level: NoticeLevel::Info, text: "Compacting context…".into(), data: None }).await;
        let result =
            self.0.post(&format!("/session/{id}/summarize"), json!({ "providerID": provider, "modelID": model, "auto": false })).await;
        let active = {
            let mut state = self.0.state.lock().await;
            if state.compact_operation.as_deref() != Some(&operation) {
                return Ok(());
            }
            state.compact_operation = None;
            std::mem::take(&mut state.active)
        };
        result?;
        if active {
            self.0
                .emit(DriverEvent::TurnCompleted {
                    stop_reason: StopReason::Completed,
                    usage: Usage::default(),
                    cost_usd: None,
                    duration_ms: 0,
                    anchors: crate::TurnAnchors::default(),
                })
                .await;
        }
        Ok(())
    }

    async fn send_message(&self, message_id: &str, message: &UserMessage) -> Result<()> {
        let s = &self.0;
        let (session_id, model) = {
            let st = s.state.lock().await;
            (st.session_id.clone().ok_or_else(|| DriverError::Protocol("no opencode session".into()))?, st.model.clone())
        };
        // OpenCode ids must start with "msg"; derive a stable one from ours.
        let oc_message_id = format!("msg_{}", message_id.replace('-', ""));
        let commands = s.state.lock().await.commands.clone();
        let command = skill_command(message).or_else(|| {
            let text = message
                .parts
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            let rest = text.trim().strip_prefix('/')?;
            let (name, arguments) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            // Only advertised commands are routed to /command; prose stays prose.
            commands.iter().any(|command| command.name == name).then(|| SkillCommand {
                name: name.into(),
                arguments: arguments.trim().into(),
                files: parts(message).into_iter().filter(|part| part["type"] == "file").collect(),
            })
        });
        let mut body = match &command {
            Some(command) => json!({
                "messageID": oc_message_id,
                "command": command.name,
                "arguments": command.arguments,
                "parts": command.files,
            }),
            None => json!({ "parts": parts(message), "messageID": oc_message_id }),
        };
        if let Some(model) = model.as_deref() {
            if command.is_some() {
                body["model"] = Value::String(model.to_string());
            } else if let Some((provider, model)) = split_model(model) {
                body["model"] = json!({ "providerID": provider, "modelID": model });
            }
        }
        // OpenCode constructs the system prefix from the latest user message's
        // system field. It is not a session-level bootstrap: repeat the stable
        // role on every prompt, including after an idle process resume.
        let coordinator_instructions = if command.is_none() { s.state.lock().await.coordinator_instructions.clone() } else { None };
        if let Some(instructions) = coordinator_instructions.as_ref() {
            body["system"] = Value::String(instructions.clone());
        }
        {
            let mut st = s.state.lock().await;
            st.active = true;
            st.turn_started = Some(std::time::Instant::now());
            st.turn_usage = Usage::default();
            st.turn_cost = 0.0;
            st.current_message_id = Some(oc_message_id);
        }
        if command.is_some() {
            let session = s.clone();
            tokio::spawn(async move {
                if let Err(error) = session.post(&format!("/session/{session_id}/command"), body).await {
                    session.state.lock().await.active = false;
                    session.emit(DriverEvent::TurnFailed { error: error.to_string() }).await;
                }
            });
            Ok(())
        } else {
            s.post(&format!("/session/{session_id}/prompt_async"), body).await.map(|_| ())
        }
    }

    async fn interrupt(&self) -> Result<()> {
        let s = &self.0;
        let Some(session_id) = s.state.lock().await.session_id.clone() else { return Ok(()) };
        s.post(&format!("/session/{session_id}/abort"), json!({})).await.map(|_| ())
    }

    async fn set_permission_mode(&self, mode: PermissionMode) -> Result<()> {
        // Session rulesets are fixed at creation; the mode applies to the next session (resume creates none).
        self.0.state.lock().await.mode = mode;
        Err(DriverError::Unsupported("OpenCode permission rules are set when the session starts; start a new thread to change them".into()))
    }

    async fn set_model(&self, model: &str) -> Result<()> {
        if split_model(model).is_none() {
            return Err(DriverError::Unsupported(format!("OpenCode models are named provider/model, got {model}")));
        }
        self.0.state.lock().await.model = Some(model.to_string());
        Ok(())
    }

    async fn set_effort(&self, _effort: &str) -> Result<()> {
        Err(DriverError::Unsupported("OpenCode did not report effort controls for this model".into()))
    }

    async fn respond_permission(&self, request_id: &str, decision: &ApprovalDecision) -> Result<()> {
        let s = &self.0;
        let Some(session_id) = s.state.lock().await.pending.get(request_id).cloned() else {
            return Err(DriverError::Protocol(format!("no pending permission {request_id}")));
        };
        if session_id == "__question" {
            let (action, body) = match decision {
                ApprovalDecision::Submit { response } => ("reply", response.clone()),
                ApprovalDecision::Deny { .. } => ("reject", json!({})),
                _ => return Err(DriverError::Protocol("this question needs an answer".into())),
            };
            s.post(&format!("/question/{request_id}/{action}"), body).await?;
            s.state.lock().await.pending.remove(request_id);
            return Ok(());
        }
        let response = match decision {
            ApprovalDecision::Submit { .. } => return Err(DriverError::Protocol("expected permission decision".into())),
            ApprovalDecision::AllowOnce => "once",
            ApprovalDecision::AllowAlways => "always",
            ApprovalDecision::Deny { .. } => "reject",
        };
        s.post(&format!("/session/{session_id}/permissions/{request_id}"), json!({ "response": response })).await?;
        s.state.lock().await.pending.remove(request_id);
        Ok(())
    }

    async fn stop_runtime_task(&self, task: &RuntimeTask) -> Result<()> {
        let session_id = task
            .provider_thread_id
            .as_deref()
            .ok_or_else(|| DriverError::Unsupported("OpenCode did not expose the child session id needed to stop this task".into()))?;
        self.0.stop_child(session_id).await
    }

    async fn close(&self) -> Result<()> {
        self.1.lock().unwrap().take();
        let mut child = self.0.child.lock().await;
        let _ = child.kill().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    #[tokio::test]
    async fn child_session_output_reaches_the_subagent_and_never_the_root() {
        use super::*;
        let (events, mut rx) = mpsc::channel(64);
        let mut command = crate::test_process::sleep();
        command.arg("30").kill_on_drop(true);
        let session = Arc::new(OpencodeSession {
            http: reqwest::Client::new(),
            base: "http://127.0.0.1:9".into(),
            dir: "/tmp".into(),
            child: Mutex::new(command.spawn().unwrap()),
            events,
            state: Mutex::new(State {
                compact_operation: None,
                commands: vec![],
                context_windows: HashMap::new(),
                session_id: Some("root".into()),
                model: None,
                coordinator_instructions: None,
                mode: PermissionMode::Supervised,
                parts: HashMap::new(),
                message_roles: HashMap::new(),
                pending_images: HashMap::new(),
                turn_usage: Usage::default(),
                turn_cost: 0.0,
                turn_started: None,
                active: true,
                pending: HashMap::new(),
                current_message_id: None,
                children: HashMap::new(),
            }),
        });
        let event = |ty: &str, properties: Value| json!({"type": ty, "properties": properties});
        // The parent's task tool part announces the child session.
        session
            .handle_event(&event(
                "message.part.updated",
                json!({"sessionID":"root", "part":{"id":"p0", "type":"tool", "tool":"task", "callID":"call-1", "messageID":"rm",
                    "state":{"status":"running", "input":{"description":"Look around", "prompt":"List the files"}, "metadata":{"sessionId":"kid"}}}}),
            ))
            .await;
        let frames = [
            // Its first message is the prompt it was given: never forwarded.
            event("message.updated", json!({"info":{"id":"um", "role":"user", "sessionID":"kid"}})),
            event(
                "message.part.updated",
                json!({"sessionID":"kid", "part":{"id":"up", "type":"text", "messageID":"um", "text":"List the files", "time":{"end":1}}}),
            ),
            // A part may precede the message that gives it a role.
            event(
                "message.part.updated",
                json!({"sessionID":"kid", "part":{"id":"tp", "type":"tool", "tool":"ls", "callID":"c1", "messageID":"am", "state":{"status":"running", "input":{"path":"."}}}}),
            ),
            event("message.updated", json!({"info":{"id":"am", "role":"assistant", "sessionID":"kid"}})),
            event(
                "message.part.updated",
                json!({"sessionID":"kid", "part":{"id":"tp", "type":"tool", "tool":"ls", "callID":"c1", "messageID":"am", "state":{"status":"completed", "input":{"path":"."}, "output":"a.txt"}}}),
            ),
            event(
                "message.part.updated",
                json!({"sessionID":"kid", "part":{"id":"xp", "type":"text", "messageID":"am", "text":"", "time":{"start":1}}}),
            ),
            event("message.part.delta", json!({"sessionID":"kid", "messageID":"am", "partID":"xp", "field":"text", "delta":"One file."})),
            event(
                "message.part.updated",
                json!({"sessionID":"kid", "part":{"id":"xp", "type":"text", "messageID":"am", "text":"One file.", "time":{"start":1, "end":2}}}),
            ),
        ];
        for frame in &frames {
            session.handle_event(frame).await;
        }
        let mut shape = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                DriverEvent::SubagentOnly { task_id, event } => {
                    assert_eq!(task_id, "opencode:kid");
                    shape.push(match *event {
                        DriverEvent::ToolStarted(call) => format!("tool {} {}", call.name, call.id),
                        DriverEvent::ToolCompleted { tool_call_id, .. } => format!("done {tool_call_id}"),
                        DriverEvent::TextDelta { delta, .. } => format!("text {delta}"),
                        DriverEvent::MessageCompleted { text, .. } => format!("message {text}"),
                        other => panic!("unexpected {other:?}"),
                    });
                }
                DriverEvent::TextDelta { .. } | DriverEvent::MessageCompleted { .. } => panic!("child prose leaked to the root"),
                _ => {}
            }
        }
        assert_eq!(shape, ["tool ls c1", "done c1", "text One file.", "message One file."]);
    }

    #[tokio::test]
    async fn native_compaction_and_advertised_commands_use_their_own_endpoints() {
        use super::*;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut requests = vec![];
            for _ in 0..4 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = vec![];
                loop {
                    let mut chunk = [0; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&chunk[..count]);
                    let text = String::from_utf8_lossy(&bytes);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length: usize = text[..end]
                            .lines()
                            .find_map(|line| line.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(bytes).unwrap());
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                    .await
                    .unwrap();
            }
            requests
        });
        let (events, mut rx) = mpsc::channel(8);
        let mut command = crate::test_process::sleep();
        command.arg("30").kill_on_drop(true);
        let session = Arc::new(OpencodeSession {
            http: reqwest::Client::new(),
            base,
            dir: "/tmp".into(),
            child: Mutex::new(command.spawn().unwrap()),
            events,
            state: Mutex::new(State {
                compact_operation: None,
                commands: vec![kybern_protocol::ProviderCommand { name: "review".into(), description: "Review".into() }],
                context_windows: HashMap::new(),
                session_id: Some("root".into()),
                model: Some("test/model".into()),
                coordinator_instructions: Some("COORDINATOR ROLE SENTINEL".into()),
                mode: PermissionMode::Supervised,
                parts: HashMap::new(),
                message_roles: HashMap::new(),
                pending_images: HashMap::new(),
                turn_usage: Usage::default(),
                turn_cost: 0.0,
                turn_started: None,
                active: true,
                pending: HashMap::new(),
                current_message_id: None,
                children: HashMap::new(),
            }),
        });

        let handle = Handle(session.clone(), std::sync::Mutex::new(None), None);
        handle.compact().await.unwrap();
        assert!(matches!(rx.recv().await, Some(DriverEvent::Notice { .. })));
        assert!(matches!(rx.recv().await, Some(DriverEvent::TurnCompleted { .. })));
        handle.send_message("message", &UserMessage::text("/review changed files")).await.unwrap();
        handle.send_message("plain", &UserMessage::text("delegate a review")).await.unwrap();
        handle.send_message("plain-two", &UserMessage::text("continue coordinating")).await.unwrap();
        let requests = tokio::time::timeout(Duration::from_secs(2), server).await.unwrap().unwrap();
        assert!(requests[0].starts_with("POST /session/root/summarize?"));
        let body: Value = serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body, json!({"providerID":"test", "modelID":"model", "auto":false}));
        let command_request = requests.iter().find(|request| request.starts_with("POST /session/root/command?")).unwrap();
        let body: Value = serde_json::from_str(command_request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["command"], "review");
        assert_eq!(body["arguments"], "changed files");
        assert!(body.get("system").is_none(), "command dispatch must retain its native payload");
        let prompt_bodies = requests
            .iter()
            .filter(|request| request.starts_with("POST /session/root/prompt_async?"))
            .map(|request| serde_json::from_str::<Value>(request.split("\r\n\r\n").nth(1).unwrap()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(prompt_bodies.len(), 2);
        assert_eq!(prompt_bodies[0]["system"], "COORDINATOR ROLE SENTINEL");
        assert_eq!(prompt_bodies[0]["parts"][0]["text"], "delegate a review");
        assert_eq!(prompt_bodies[1]["system"], prompt_bodies[0]["system"], "coordinator system prefix must remain stable");
        assert_eq!(prompt_bodies[1]["parts"][0]["text"], "continue coordinating");
        session.state.lock().await.context_windows.insert("test/model".into(), 200000);
        session.handle_event(&json!({"type":"message.updated", "properties":{"info":{"id":"context", "sessionID":"root", "role":"assistant", "providerID":"test", "modelID":"model", "time":{"completed":1}, "tokens":{"input":1000,"output":100,"reasoning":10,"cache":{"read":200,"write":0}}}}})).await;
        let Some(DriverEvent::UsageUpdated(usage)) = rx.recv().await else { panic!("missing context usage") };
        let context = usage.context.unwrap();
        assert_eq!(context.used_tokens, 1310);
        assert_eq!(context.window_tokens, 200000);
        handle.close().await.unwrap();
    }

    #[tokio::test]
    async fn questions_reply_to_native_endpoint_and_can_be_declined() {
        use super::*;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut requests = vec![];
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = vec![];
                loop {
                    let mut chunk = [0; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&chunk[..count]);
                    let text = String::from_utf8_lossy(&bytes);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length: usize = text[..end]
                            .lines()
                            .find_map(|line| line.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(bytes).unwrap());
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                    .await
                    .unwrap();
            }
            requests
        });
        let (events, mut rx) = mpsc::channel(8);
        let mut command = crate::test_process::sleep();
        command.arg("30").kill_on_drop(true);
        let session = Arc::new(OpencodeSession {
            http: reqwest::Client::new(),
            base,
            dir: "/tmp".into(),
            child: Mutex::new(command.spawn().unwrap()),
            events,
            state: Mutex::new(State {
                compact_operation: None,
                commands: Vec::new(),
                context_windows: HashMap::new(),
                session_id: Some("root".into()),
                model: None,
                coordinator_instructions: None,
                mode: PermissionMode::Supervised,
                parts: HashMap::new(),
                message_roles: HashMap::new(),
                pending_images: HashMap::new(),
                turn_usage: Usage::default(),
                turn_cost: 0.0,
                turn_started: None,
                active: true,
                pending: HashMap::new(),
                current_message_id: None,
                children: HashMap::new(),
            }),
        });
        // Parts can arrive before message metadata. Only assistant-authored images are emitted.
        for role in ["user", "assistant"] {
            let part = json!({"id":role,"messageID":role,"type":"file","mime":"image/png","url":"data:image/png;base64,aW1hZ2U="});
            session.handle_part(&part).await;
            assert!(rx.try_recv().is_err());
            session.handle_event(&json!({"type":"message.updated","properties":{"info":{"id":role,"role":role,"sessionID":"root"}}})).await;
            if role == "assistant" {
                assert!(matches!(rx.recv().await, Some(DriverEvent::ImageReceived { .. })));
            } else {
                assert!(rx.try_recv().is_err());
            }
            session.handle_part(&part).await;
            assert!(rx.try_recv().is_err());
        }
        let handle = Handle(session.clone(), std::sync::Mutex::new(None), None);
        for id in ["question-1", "question-2"] {
            session.handle_event(&json!({"type":"question.asked","properties":{"id":id,"sessionID":"root","questions":[{"question":"Which?","multiple":true}]}})).await;
            assert!(matches!(rx.recv().await, Some(DriverEvent::PermissionRequest { tool_name, .. }) if tool_name == "opencode_question"));
            let decision = if id == "question-1" {
                ApprovalDecision::Submit { response: json!({"answers":[["A","Custom"]]}) }
            } else {
                ApprovalDecision::Deny { reason: None }
            };
            handle.respond_permission(id, &decision).await.unwrap();
        }
        let requests = tokio::time::timeout(Duration::from_secs(2), server).await.unwrap().unwrap();
        assert!(requests[0].starts_with("POST /question/question-1/reply?"));
        assert_eq!(
            serde_json::from_str::<Value>(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap(),
            json!({"answers":[["A","Custom"]]})
        );
        assert!(requests[1].starts_with("POST /question/question-2/reject?"));
        assert!(session.state.lock().await.pending.is_empty());
        handle.close().await.unwrap();
    }

    #[test]
    fn builds_remote_mcp_config_and_enforces_coordinator_tool_scope() {
        let bridge = NativeToolBridge {
            server_name: "kybern".into(),
            endpoint: Some("http://127.0.0.1:4199/native-tools/mcp".into()),
            authorization: Some("session-capability".into()),
            coordinator_instructions: None,
            guide: None,
            tools: vec![],
            restrictions: crate::NativeToolRestrictions { require_enforcement: true, ..Default::default() },
        };
        let config = opencode_config_content(Some(&r#"{"theme":"dark"}"#.to_string()), &bridge).unwrap();
        let value: Value = serde_json::from_str(&config).unwrap();
        assert_eq!(value["theme"], "dark");
        assert_eq!(value["mcp"]["kybern"]["type"], "remote");
        assert_eq!(value["mcp"]["kybern"]["headers"]["Authorization"], "Bearer session-capability");
        assert_eq!(value["tools"]["*"], false);
        assert_eq!(value["tools"]["kybern_*"], true);
    }

    #[test]
    fn a_lead_keeps_only_the_read_only_builtins() {
        let bridge = NativeToolBridge {
            server_name: "kybern".into(),
            endpoint: Some("http://127.0.0.1:4199/native-tools/mcp".into()),
            authorization: Some("session-capability".into()),
            coordinator_instructions: None,
            guide: None,
            tools: vec![],
            restrictions: crate::NativeToolRestrictions { require_enforcement: true, allow_read_only_builtins: true, ..Default::default() },
        };
        let value: Value = serde_json::from_str(&opencode_config_content(None, &bridge).unwrap()).unwrap();
        assert_eq!(value["tools"]["*"], false);
        assert_eq!(value["tools"]["kybern_*"], true);
        for name in ["read", "grep", "glob", "list", "webfetch", "question", "skill"] {
            assert_eq!(value["tools"][name], true, "{name}");
        }
        for name in ["bash", "edit", "write", "patch"] {
            assert!(value["tools"].get(name).is_none(), "{name} stays behind the wildcard");
        }
        let without = NativeToolBridge {
            restrictions: crate::NativeToolRestrictions {
                require_enforcement: false,
                allow_read_only_builtins: true,
                ..Default::default()
            },
            ..bridge
        };
        let value: Value = serde_json::from_str(&opencode_config_content(None, &without).unwrap()).unwrap();
        assert!(value.get("tools").is_none(), "an unenforced harness keeps all of its tools");
    }

    #[test]
    fn rejects_invalid_inline_opencode_config() {
        let bridge = NativeToolBridge {
            server_name: "kybern".into(),
            endpoint: Some("http://127.0.0.1:4199/native-tools/mcp".into()),
            authorization: Some("session-capability".into()),
            coordinator_instructions: None,
            guide: None,
            tools: vec![],
            restrictions: Default::default(),
        };
        assert!(matches!(
            opencode_config_content(Some(&"[]".to_string()), &bridge),
            Err(DriverError::Unsupported(message)) if message.contains("JSON object")
        ));
    }

    use super::{
        MODEL_DISCOVERY_TIMEOUT, opencode_child_session, opencode_child_task, opencode_config_content, parse_skills, skill_command,
    };
    use crate::{DriverError, NativeToolBridge};
    use kybern_protocol::{ContentPart, RuntimeTaskKind, RuntimeTaskStatus, SkillScope, UserMessage};
    use serde_json::{Value, json};
    use std::path::Path;
    use std::time::Duration;

    #[test]
    fn versions_pick_the_api_or_explain_why_not() {
        use super::{Api, api_for};
        assert_eq!(api_for((1, 19, 4)), Ok(Api::V1));
        assert_eq!(api_for((2, 2, 0)), Ok(Api::V2));
        assert_eq!(api_for((2, 9, 1)), Ok(Api::V2));
        assert!(api_for((1, 9, 0)).unwrap_err().contains("older than the required 1.10.0"));
        assert!(api_for((2, 1, 2)).unwrap_err().contains("early 2.x server API"));
        assert!(api_for((3, 0, 0)).unwrap_err().contains("newer than this Kybern supports"));
    }

    #[test]
    fn model_discovery_never_starts_opencodes_background_service() {
        use super::{Api, models_args};
        assert_eq!(models_args(Some(Api::V2)), ["models", "--standalone"]);
        assert_eq!(models_args(Some(Api::V1)), ["models"]);
        assert_eq!(models_args(None), ["models"]);
    }

    #[test]
    fn model_discovery_budget_covers_a_cold_provider_refresh() {
        assert!(MODEL_DISCOVERY_TIMEOUT >= Duration::from_secs(10));
    }

    #[test]
    fn parses_opencodes_effective_skill_catalog() {
        let skills = parse_skills(
            br#"[
                {"name":"built-in","description":"Bundled","location":"<built-in>"},
                {"name":"review","description":"Review code","location":"/work/repo/.opencode/skills/review/SKILL.md"},
                {"name":"global","description":null,"location":"/home/me/.config/opencode/skills/global/SKILL.md"}
            ]"#,
            Path::new("/work/repo"),
        )
        .expect("valid catalog");

        assert_eq!(skills.iter().map(|skill| skill.name.as_str()).collect::<Vec<_>>(), ["built-in", "global", "review"]);
        assert_eq!(skills[0].scope, SkillScope::System);
        assert_eq!(skills[1].scope, SkillScope::User);
        assert_eq!(skills[2].scope, SkillScope::Project);
    }

    #[test]
    fn dispatches_selected_skill_through_opencodes_command_contract() {
        let message = UserMessage {
            parts: vec![
                ContentPart::Text { text: "Review ".into() },
                ContentPart::Skill { name: "security".into(), path: "/skills/security/SKILL.md".into() },
                ContentPart::Text { text: " then ".into() },
                ContentPart::Skill { name: "fix-ci".into(), path: "/skills/fix-ci/SKILL.md".into() },
                ContentPart::Text { text: " carefully".into() },
                ContentPart::Image { media_type: "image/png".into(), data: "aW1hZ2U=".into() },
            ],
        };

        let command = skill_command(&message).expect("skill command");
        assert_eq!(command.name, "fix-ci");
        assert_eq!(command.arguments, "Review /security then  carefully");
        assert_eq!(
            command.files,
            vec![json!({ "type": "file", "mime": "image/png", "url": "data:image/png;base64,aW1hZ2U=", "filename": "image" })]
        );
    }

    #[test]
    fn maps_opencode_child_session_metadata_to_native_runtime_activity() {
        let (child_id, task) = opencode_child_task(
            &json!({
                "id": "part-task-1",
                "sessionID": "ses_parent",
                "messageID": "msg_parent",
                "type": "tool",
                "callID": "call-task-1",
                "tool": "task",
                "state": {
                    "status": "running",
                    "input": {
                        "description": "Audit the driver",
                        "prompt": "Check native lifecycle support",
                        "subagent_type": "general",
                        "background": true
                    },
                    "title": "Audit the driver",
                    "metadata": {
                        "parentSessionId": "ses_parent",
                        "sessionId": "ses_child",
                        "background": true,
                        "model": { "providerID": "openai", "modelID": "gpt-5.6-sol" }
                    }
                }
            }),
            None,
        )
        .expect("OpenCode child task");

        assert_eq!(child_id, "ses_child");
        assert_eq!(task.id, "opencode:ses_child");
        assert_eq!(task.kind, RuntimeTaskKind::Agent);
        assert_eq!(task.status, RuntimeTaskStatus::Running);
        assert_eq!(task.title, "Audit the driver");
        assert_eq!(task.provider_thread_id.as_deref(), Some("ses_child"));
        assert_eq!(task.tool_call_id.as_deref(), Some("call-task-1"));
        assert_eq!(task.model.as_deref(), Some("openai/gpt-5.6-sol"));
        assert!(task.backgrounded);
        assert!(task.capabilities.stop);
    }

    #[test]
    fn discovers_nested_children_from_session_events() {
        let (child_id, task) = opencode_child_session(
            &json!({
                "id": "ses_nested",
                "parentID": "ses_child",
                "title": "Trace lifecycle edges (@explore subagent)",
                "agent": "explore"
            }),
            Some("opencode:ses_child".into()),
        )
        .expect("OpenCode child session");

        assert_eq!(child_id, "ses_nested");
        assert_eq!(task.title, "Trace lifecycle edges");
        assert_eq!(task.parent_id.as_deref(), Some("opencode:ses_child"));
        assert_eq!(task.provider_type.as_deref(), Some("task:explore"));
    }

    #[test]
    fn keeps_promoted_opencode_children_active_after_the_parent_tool_returns() {
        let (_, task) = opencode_child_task(
            &json!({
                "id": "part-task-2",
                "type": "tool",
                "callID": "call-task-2",
                "tool": "task",
                "state": {
                    "status": "completed",
                    "input": { "description": "Run independently" },
                    "metadata": { "sessionId": "ses_background", "background": true }
                }
            }),
            Some("opencode:ses_parent_child".into()),
        )
        .expect("promoted OpenCode child");

        assert_eq!(task.status, RuntimeTaskStatus::Running);
        assert_eq!(task.parent_id.as_deref(), Some("opencode:ses_parent_child"));
    }
}

#[cfg(test)]
mod orchestration_part_tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn thread_messages_and_agent_results_flatten_to_text_parts() {
        let out = parts(&orchestration_message());
        assert_flattened(&out.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"));
    }

    #[test]
    fn they_also_flatten_into_skill_command_arguments() {
        let mut message = orchestration_message();
        message.parts.insert(0, ContentPart::Skill { name: "review".into(), path: "/s/SKILL.md".into() });
        assert_flattened(&skill_command(&message).unwrap().arguments);
    }

    #[test]
    fn the_variant_probe_reuses_one_folder() {
        let env = BTreeMap::from([("XDG_CACHE_HOME".to_string(), "/cache".to_string())]);
        assert_eq!(probe_folder(&env), probe_folder(&env));
        assert!(probe_folder(&env).ends_with("kybern/opencode-probe"));
        if cfg!(target_os = "linux") {
            assert_eq!(probe_folder(&env), PathBuf::from("/cache/kybern/opencode-probe"));
        }
    }

    #[test]
    fn parse_model_catalog_joins_variants_on_provider_and_id() {
        let models = vec![
            json!({"id": "m1", "providerID": "prov", "variants": [{"id": "high"}]}),
            json!({"id": "m2", "providerID": "", "variants": []}),
            json!({"id": "m3", "providerID": "prov"}),
            json!({"providerID": "prov", "variants": [{"id": "x"}]}),
            json!({"id": "m5", "providerID": "prov", "variants": "not-an-array"}),
        ];
        let catalog = parse_model_catalog(&models);
        assert_eq!(catalog.get("prov/m1").unwrap(), &[json!({"id": "high"})]);
        assert_eq!(catalog.get("m2").unwrap(), &Vec::<Value>::new());
        assert!(!catalog.contains_key("prov/m3"));
        assert!(!catalog.contains_key("prov/m5"));
        assert_eq!(catalog.len(), 2);
    }

    #[test]
    fn variant_cache_ages_full_reads_and_misses_differently() {
        let key: VariantCacheKey = ("/bin/opencode".into(), BTreeMap::new());
        let now = std::time::Instant::now();
        let mut cache: VariantCache = HashMap::new();
        cache.insert(key.clone(), (now, HashMap::from([("prov/m".to_string(), vec![json!({"id": "high"})])])));
        assert!(cached_variants(&cache, &key, now).is_some());
        assert!(cached_variants(&cache, &key, now + VARIANT_CACHE_TTL).is_none());
        cache.insert(key.clone(), (now, HashMap::new()));
        assert!(cached_variants(&cache, &key, now + VARIANT_CACHE_MISS_TTL - std::time::Duration::from_secs(1)).is_some());
        assert!(cached_variants(&cache, &key, now + VARIANT_CACHE_MISS_TTL).is_none());
        let other_env = (key.0.clone(), BTreeMap::from([("OTHER".to_string(), "1".to_string())]));
        assert!(cached_variants(&cache, &other_env, now).is_none());
    }

    #[test]
    fn variant_efforts_lists_variant_ids_in_order_without_duplicates() {
        let variants = vec![
            json!({ "id": "max", "settings": { "reasoningEffort": "max" } }),
            json!({ "id": "low", "settings": { "reasoningEffort": "low" } }),
            json!({ "id": "low2", "settings": { "reasoningEffort": "low" } }),
            json!({ "id": "off", "settings": { "reasoningEffort": "" } }),
            json!({ "id": "bare", "settings": {} }),
        ];
        // Variant ids, not their reasoningEffort values: what a session
        // accepts is the variant, and ids cover variants without an effort.
        assert_eq!(variant_efforts(&variants), ["max", "low", "low2", "off", "bare"]);
        assert!(variant_efforts(&[]).is_empty());
    }

    #[test]
    fn merge_efforts_joins_on_the_full_catalog_id_and_keeps_plain_rows() {
        let catalog = HashMap::from([
            ("prov/m1".to_string(), vec![json!({"id": "high", "settings": {"reasoningEffort": "high"}})]),
            ("prov/bare".to_string(), vec![]),
        ]);
        let row = |id: &str, provider: &str| ProviderModel {
            id: id.into(),
            display_name: id.into(),
            resolved_id: None,
            description: None,
            provider: Some(provider.into()),
            efforts: Vec::new(),
            default_effort: None,
            is_default: false,
            parameters: Vec::new(),
            variants: Vec::new(),
        };
        let mut models = vec![row("prov/m1", "prov"), row("prov/bare", "prov"), row("other/m2", "other")];
        merge_efforts(&mut models, &catalog);
        assert_eq!(models[0].efforts, ["high"]);
        assert!(models[1].efforts.is_empty());
        assert!(models[2].efforts.is_empty());
    }
}
