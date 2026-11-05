//! Claude Code's effective model and effort settings.
//!
//! Claude does not expose a `model/list` RPC like Codex. Its defaults are a
//! layered configuration problem, so probing must follow the same inputs the
//! spawned CLI receives instead of inventing an "auto" value.

use kybern_protocol::process::NoWindow;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::ProbeContext;

const MODEL_ENV: &str = "ANTHROPIC_MODEL";
const DEFAULT_MODEL_ENV: &str = "ANTHROPIC_DEFAULT_MODEL";
const EFFORT_ENV: &str = "CLAUDE_CODE_EFFORT_LEVEL";

#[derive(Debug, Clone)]
pub(super) struct ClaudeConfig {
    /// Selector Claude Code will use (`fable[1m]`, `opus`, or a full id).
    pub model: String,
    /// Effective effort after environment, per-model, global, and model
    /// defaults have been applied.
    pub effort: String,
    /// A deployment-specific target for a family alias, when configured.
    pub alias_target: Option<String>,
    preferences: ClaudePreferences,
    environment_effort: Option<String>,
}

impl ClaudeConfig {
    /// A config with no settings files or environment behind it.
    #[cfg(test)]
    pub fn for_model(model: &str) -> Self {
        Self {
            model: model.into(),
            effort: model_default_effort(model).into(),
            alias_target: None,
            preferences: ClaudePreferences::default(),
            environment_effort: None,
        }
    }

    pub fn effort_for(&self, model: &str) -> String {
        self.environment_effort
            .clone()
            .or_else(|| self.preferences.effort_for(model))
            .unwrap_or_else(|| model_default_effort(model).to_string())
    }
}

#[derive(Debug, Clone, Default)]
struct ClaudePreferences {
    model: Option<String>,
    effort_level: Option<String>,
    model_efforts: BTreeMap<String, String>,
}

impl ClaudePreferences {
    fn merge(&mut self, value: &Value) {
        if let Some(model) = non_empty_string(value.get("model")) {
            self.model = Some(model.to_string());
        }
        if let Some(effort) = value.get("effortLevel").and_then(Value::as_str).and_then(valid_effort) {
            self.effort_level = Some(effort.to_string());
        }
        if let Some(settings) = value.get("modelSettings").and_then(Value::as_object) {
            for (model, settings) in settings {
                if let Some(effort) = settings.get("effortLevel").and_then(Value::as_str).and_then(valid_effort) {
                    self.model_efforts.insert(model.to_ascii_lowercase(), effort.to_string());
                }
            }
        }
    }

    fn effort_for(&self, model: &str) -> Option<String> {
        model_lookup_keys(model).into_iter().find_map(|key| self.model_efforts.get(&key).cloned()).or_else(|| self.effort_level.clone())
    }
}

/// `catalog_default` is the selector Claude Code's own catalog marks as the
/// account default. Without it, the default is inferred from `auth status`.
pub(super) async fn resolve(context: &ProbeContext, binary: &Path, catalog_default: Option<String>) -> ClaudeConfig {
    let preferences = load_preferences(context);
    let environment_model = environment_value(context, MODEL_ENV).and_then(non_empty_owned);
    let default_model = environment_value(context, DEFAULT_MODEL_ENV).and_then(non_empty_owned);
    let configured_model = environment_model.or_else(|| preferences.model.clone());
    let needs_account_default = configured_model.as_deref().is_none_or(is_default_selector) && default_model.is_none();
    let account_default = match catalog_default {
        Some(selector) if needs_account_default => Some(selector),
        _ if needs_account_default => account_default_model(context, binary).await,
        _ => None,
    };
    let model = match configured_model {
        Some(model) if !is_default_selector(&model) => model,
        _ => default_model.or(account_default).unwrap_or_else(|| "sonnet".to_string()),
    };
    let environment_effort = environment_value(context, EFFORT_ENV).as_deref().and_then(valid_effort).map(str::to_string);
    let effort =
        environment_effort.clone().or_else(|| preferences.effort_for(&model)).unwrap_or_else(|| model_default_effort(&model).to_string());
    let alias_target = alias_target(context, &model);

    ClaudeConfig { model, effort, alias_target, preferences, environment_effort }
}

fn load_preferences(context: &ProbeContext) -> ClaudePreferences {
    let mut preferences = ClaudePreferences::default();
    for path in settings_paths(context) {
        let Ok(contents) = std::fs::read_to_string(&path) else { continue };
        match serde_json::from_str::<Value>(&contents) {
            Ok(value) => preferences.merge(&value),
            Err(error) => tracing::warn!(path = %path.display(), %error, "ignored invalid Claude Code settings"),
        }
    }
    preferences
}

fn config_dir(context: &ProbeContext) -> Option<PathBuf> {
    let non_empty = |key| environment_value(context, key).filter(|value| !value.trim().is_empty());
    non_empty("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        // Claude Code uses the OS home directory, which Windows names USERPROFILE.
        .or_else(|| non_empty("HOME").or_else(|| non_empty("USERPROFILE")).map(|home| PathBuf::from(home).join(".claude")))
}

/// Whether Claude Code's stored login is due for a refresh, so the next
/// Claude Code process would rotate its OAuth tokens. Claude Code refreshes
/// five minutes before the access token expires. Only the expiry is used.
/// Linux and Windows keep the login in `.credentials.json`; macOS keeps it in
/// the Keychain. Windows Credential Manager logins (behind a Claude Code flag)
/// are not read and count as not due.
pub(crate) async fn login_needs_refresh(context: &ProbeContext) -> bool {
    stored_login(context).await.as_deref().and_then(login_expiry).is_some_and(refresh_due)
}

/// Claude Code's stored OAuth login (`claudeAiOauth`) when starting Claude
/// Code would redeem its refresh token: the login is due for a refresh and
/// holds one. `claude auth status` can redeem it and exit before saving the
/// replacement, which loses the login (#107), so informational probes read
/// this instead of running Claude Code.
pub(crate) async fn login_due_for_refresh(context: &ProbeContext) -> Option<Value> {
    let login = decode_login(&stored_login(context).await?)?;
    let oauth = login.get("claudeAiOauth")?;
    let due = oauth.get("expiresAt").and_then(Value::as_u64).is_some_and(refresh_due);
    (due && non_empty_string(oauth.get("refreshToken")).is_some()).then(|| oauth.clone())
}

/// Whether an access token expiring at `expires_at` (Unix milliseconds) is
/// inside Claude Code's five-minute refresh margin.
fn refresh_due(expires_at: u64) -> bool {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |now| now.as_millis() as u64);
    now + 5 * 60 * 1000 >= expires_at
}

/// A stored login's JSON. The Keychain item may hold the JSON itself or its hex encoding.
pub(crate) fn decode_login(bytes: &[u8]) -> Option<serde_json::Value> {
    let text = std::str::from_utf8(bytes).ok()?.trim();
    if let Ok(value) = serde_json::from_str(text) {
        return Some(value);
    }
    let decoded = (0..text.len())
        .step_by(2)
        .map(|index| text.get(index..index + 2).and_then(|pair| u8::from_str_radix(pair, 16).ok()))
        .collect::<Option<Vec<u8>>>()?;
    serde_json::from_slice(&decoded).ok()
}

/// When a stored login's access token expires, in Unix milliseconds.
fn login_expiry(bytes: &[u8]) -> Option<u64> {
    decode_login(bytes)?.pointer("/claudeAiOauth/expiresAt")?.as_u64()
}

/// Claude Code's stored login JSON: `.credentials.json` on Linux and Windows,
/// the Keychain item on macOS. Read only; Kybern never refreshes it, because
/// Anthropic rotates the single-use refresh token on every redemption.
///
/// A Mac can have both: a file left by an older Claude Code or copied from
/// another machine sits beside the Keychain item Claude Code keeps current.
/// The login that expires last is the live one; reading the file alone would
/// see an expired token forever and never read usage again.
pub(crate) async fn stored_login(context: &ProbeContext) -> Option<Vec<u8>> {
    let file = config_dir(context).and_then(|dir| std::fs::read(dir.join(".credentials.json")).ok());
    fresher_login(file, keychain_login(context).await)
}

/// Of the credentials file and the Keychain item, the login that expires last
/// (the Keychain item when neither says).
fn fresher_login(file: Option<Vec<u8>>, keychain: Option<Vec<u8>>) -> Option<Vec<u8>> {
    match (file, keychain) {
        (Some(file), Some(keychain)) => Some(if login_expiry(&file) > login_expiry(&keychain) { file } else { keychain }),
        (file, keychain) => file.or(keychain),
    }
}

/// Claude Code's macOS login item, read the way Claude Code reads it. A
/// custom config directory gets a hashed item name, which is not derived
/// here, so those logins are treated as not due.
#[cfg(target_os = "macos")]
async fn keychain_login(context: &ProbeContext) -> Option<Vec<u8>> {
    let custom_dir = ["CLAUDE_CONFIG_DIR", "CLAUDE_SECURESTORAGE_CONFIG_DIR"].iter().any(|key| environment_value(context, key).is_some());
    let account = environment_value(context, "USER").filter(|user| !user.is_empty() && !custom_dir)?;
    let mut command = tokio::process::Command::new("/usr/bin/security").no_window();
    command.args(["find-generic-password", "-a", &account, "-w", "-s", "Claude Code-credentials"]);
    let output = tokio::time::timeout(Duration::from_secs(2), crate::process_tree::output(&mut command)).await.ok()?.ok()?;
    output.status.success().then_some(output.stdout)
}

#[cfg(not(target_os = "macos"))]
async fn keychain_login(_context: &ProbeContext) -> Option<Vec<u8>> {
    None
}

pub(super) fn settings_paths(context: &ProbeContext) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(config_dir) = config_dir(context) {
        paths.push(config_dir.join("settings.json"));
    }

    if let Some(cwd) = context.cwd.as_deref() {
        paths.push(cwd.join(".claude/settings.json"));
        paths.push(cwd.join(".claude/settings.local.json"));
        if let Some(root) = repository_root(cwd) {
            paths.push(root.join(".claude/settings.local.json"));
        }
    }

    if let Some(managed) = managed_settings_path(context) {
        paths.push(managed);
    }

    let mut seen = HashSet::new();
    paths.retain(|path| seen.insert(path.clone()));
    paths
}

fn repository_root(path: &Path) -> Option<&Path> {
    path.ancestors().find(|ancestor| ancestor.join(".git").exists())
}

fn managed_settings_path(context: &ProbeContext) -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        Some(PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json"))
    } else if cfg!(target_os = "linux") {
        Some(PathBuf::from("/etc/claude-code/managed-settings.json"))
    } else if cfg!(target_os = "windows") {
        environment_value(context, "PROGRAMDATA").map(|root| PathBuf::from(root).join("ClaudeCode/managed-settings.json"))
    } else {
        None
    }
}

async fn account_default_model(context: &ProbeContext, binary: &Path) -> Option<String> {
    // The stored login carries the plan, so a login that is due never runs `auth status`.
    if let Some(oauth) = login_due_for_refresh(context).await {
        return account_default_from_status(&oauth);
    }
    let mut command = crate::binary::command(binary).no_window();
    command.args(["auth", "status", "--json"]).env_remove("NODE_OPTIONS");
    if let Some(cwd) = context.cwd.as_deref() {
        command.current_dir(cwd);
    }
    command.envs(&context.env);
    let output = tokio::time::timeout(Duration::from_secs(2), crate::process_tree::output(&mut command)).await.ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    let status: Value = serde_json::from_slice(&output.stdout).ok()?;
    account_default_from_status(&status)
}

fn account_default_from_status(status: &Value) -> Option<String> {
    let provider = status.get("apiProvider").and_then(Value::as_str).unwrap_or_default().to_ascii_lowercase();
    if provider.contains("foundry") {
        return Some("sonnet".into());
    }
    if provider.contains("bedrock") || provider.contains("vertex") || provider.contains("agent") {
        return Some("opus".into());
    }

    let subscription = status.get("subscriptionType").and_then(Value::as_str).unwrap_or_default().to_ascii_lowercase();
    if matches!(subscription.as_str(), "max" | "enterprise" | "team_premium" | "team-premium") {
        return Some("opus".into());
    }
    if matches!(subscription.as_str(), "pro" | "team" | "team_standard" | "team-standard") {
        return Some("sonnet".into());
    }

    let auth_method = status.get("authMethod").and_then(Value::as_str).unwrap_or_default().to_ascii_lowercase();
    (auth_method.contains("api") || provider.contains("firstparty") || provider.contains("first_party")).then(|| "opus".into())
}

fn alias_target(context: &ProbeContext, model: &str) -> Option<String> {
    let family = model_family(model)?;
    let key = match family {
        "fable" => "ANTHROPIC_DEFAULT_FABLE_MODEL",
        "opus" => "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "sonnet" => "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "haiku" => "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        _ => return None,
    };
    environment_value(context, key).and_then(non_empty_owned)
}

fn model_family(model: &str) -> Option<&'static str> {
    let model = model.to_ascii_lowercase();
    ["fable", "opus", "sonnet", "haiku"].into_iter().find(|family| model == *family || model.starts_with(&format!("{family}[")))
}

fn model_lookup_keys(model: &str) -> Vec<String> {
    let lower = model.to_ascii_lowercase();
    let mut keys = vec![lower.clone()];
    if let Some(index) = lower.find('[') {
        keys.push(lower[..index].to_string());
    }
    keys
}

fn model_default_effort(model: &str) -> &'static str {
    if model.to_ascii_lowercase().contains("opus-4-7") { "xhigh" } else { "high" }
}

fn environment_value(context: &ProbeContext, key: &str) -> Option<String> {
    context.env.get(key).cloned().or_else(|| std::env::var(key).ok())
}

fn non_empty_string(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).map(str::trim).filter(|value| !value.is_empty())
}

fn non_empty_owned(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn valid_effort(value: &str) -> Option<&str> {
    let value = value.trim();
    matches!(value, "low" | "medium" | "high" | "xhigh" | "max").then_some(value)
}

fn is_default_selector(model: &str) -> bool {
    matches!(model.trim().to_ascii_lowercase().as_str(), "" | "default" | "inherit")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_only_the_login_expiry_to_decide_whether_a_refresh_is_due() {
        let temp = tempfile::tempdir().unwrap();
        let context = ProbeContext {
            binary: None,
            cwd: None,
            env: [("CLAUDE_CONFIG_DIR".to_string(), temp.path().to_string_lossy().into_owned())].into(),
        };
        let credentials = temp.path().join(".credentials.json");
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        let write = |expires_at: u64| {
            let login = serde_json::json!({ "claudeAiOauth": { "accessToken": "a", "refreshToken": "r", "expiresAt": expires_at } });
            std::fs::write(&credentials, login.to_string()).unwrap();
        };
        assert!(!login_needs_refresh(&context).await, "no stored login");
        write(now + 8 * 60 * 60 * 1000);
        assert!(!login_needs_refresh(&context).await);
        write(now + 60 * 1000);
        assert!(login_needs_refresh(&context).await, "inside Claude Code's five-minute refresh margin");
        write(now - 1);
        assert!(login_needs_refresh(&context).await);
        std::fs::write(&credentials, "{").unwrap();
        assert!(!login_needs_refresh(&context).await, "unreadable credentials");
    }

    #[tokio::test]
    async fn a_login_due_for_refresh_infers_the_default_model_without_running_claude() {
        let temp = tempfile::tempdir().unwrap();
        let context = ProbeContext {
            binary: None,
            cwd: None,
            env: [("CLAUDE_CONFIG_DIR".to_string(), temp.path().to_string_lossy().into_owned())].into(),
        };
        // Not a program: running it fails, so a default can only come from the stored login.
        let binary = temp.path().join("claude.txt");
        std::fs::write(&binary, "not a program").unwrap();
        let credentials = temp.path().join(".credentials.json");
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        let write = |oauth: Value| std::fs::write(&credentials, serde_json::json!({ "claudeAiOauth": oauth }).to_string()).unwrap();

        write(serde_json::json!({ "accessToken": "a", "refreshToken": "r", "expiresAt": now - 1, "subscriptionType": "max" }));
        assert_eq!(login_due_for_refresh(&context).await.unwrap()["subscriptionType"], "max");
        assert_eq!(account_default_model(&context, &binary).await.as_deref(), Some("opus"));

        write(serde_json::json!({ "accessToken": "a", "refreshToken": "r", "expiresAt": now + 60 * 1000, "subscriptionType": "pro" }));
        assert_eq!(account_default_model(&context, &binary).await.as_deref(), Some("sonnet"), "inside the refresh margin");

        let later = now + 8 * 60 * 60 * 1000;
        write(serde_json::json!({ "accessToken": "a", "refreshToken": "r", "expiresAt": later, "subscriptionType": "max" }));
        assert!(login_due_for_refresh(&context).await.is_none());
        assert_eq!(account_default_model(&context, &binary).await, None, "a login that is not due still asks Claude Code");

        write(serde_json::json!({ "accessToken": "a", "expiresAt": now - 1, "subscriptionType": "max" }));
        assert!(login_due_for_refresh(&context).await.is_none(), "no refresh token to lose");
    }

    #[test]
    fn a_leftover_credentials_file_never_shadows_a_fresher_keychain_login() {
        let login = |expires_at: u64| {
            serde_json::json!({ "claudeAiOauth": { "accessToken": expires_at.to_string(), "expiresAt": expires_at } }).to_string()
        };
        let hex = |text: String| text.bytes().map(|byte| format!("{byte:02x}")).collect::<String>().into_bytes();
        let old = login(1_000).into_bytes();
        let fresh = login(9_000).into_bytes();
        assert_eq!(
            fresher_login(Some(old.clone()), Some(fresh.clone())),
            Some(fresh.clone()),
            "the Keychain item Claude Code keeps current"
        );
        assert_eq!(fresher_login(Some(fresh.clone()), Some(old.clone())), Some(fresh.clone()), "a file Claude Code still writes");
        assert_eq!(
            fresher_login(Some(old.clone()), Some(hex(login(9_000)))),
            Some(hex(login(9_000))),
            "hex-encoded Keychain items compare by expiry too"
        );
        assert_eq!(fresher_login(Some(old.clone()), None), Some(old.clone()));
        assert_eq!(fresher_login(None, Some(fresh.clone())), Some(fresh));
        assert_eq!(login_expiry(&hex(login(9_000))), Some(9_000));
    }

    #[test]
    fn merges_user_project_local_and_managed_settings_in_precedence_order() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(project.join(".claude")).unwrap();
        std::fs::write(home.join(".claude/settings.json"), r#"{"model":"sonnet","effortLevel":"low"}"#).unwrap();
        std::fs::write(project.join(".claude/settings.json"), r#"{"model":"opus","effortLevel":"medium"}"#).unwrap();
        std::fs::write(project.join(".claude/settings.local.json"), r#"{"model":"fable[1m]"}"#).unwrap();

        let context = ProbeContext {
            cwd: Some(project),
            env: BTreeMap::from([("HOME".into(), home.to_string_lossy().into_owned())]),
            ..ProbeContext::default()
        };
        let preferences = load_preferences(&context);

        assert_eq!(preferences.model.as_deref(), Some("fable[1m]"));
        assert_eq!(preferences.effort_for("fable[1m]").as_deref(), Some("medium"));
    }

    #[test]
    fn exact_and_context_family_effort_precede_the_global_effort() {
        let mut preferences = ClaudePreferences::default();
        preferences.merge(&serde_json::json!({
            "effortLevel": "medium",
            "modelSettings": { "fable": { "effortLevel": "xhigh" } }
        }));

        assert_eq!(preferences.effort_for("fable[1m]").as_deref(), Some("xhigh"));
        assert_eq!(preferences.effort_for("sonnet").as_deref(), Some("medium"));
    }

    #[test]
    fn account_type_resolves_claude_codes_documented_default_family() {
        assert_eq!(account_default_from_status(&serde_json::json!({ "subscriptionType": "max" })).as_deref(), Some("opus"));
        assert_eq!(account_default_from_status(&serde_json::json!({ "subscriptionType": "pro" })).as_deref(), Some("sonnet"));
        assert_eq!(account_default_from_status(&serde_json::json!({ "apiProvider": "foundry" })).as_deref(), Some("sonnet"));
        assert_eq!(account_default_from_status(&serde_json::json!({ "apiProvider": "bedrock" })).as_deref(), Some("opus"));
    }

    #[test]
    fn opus_4_7_keeps_its_exception_to_the_normal_high_default() {
        assert_eq!(model_default_effort("claude-opus-4-7"), "xhigh");
        assert_eq!(model_default_effort("fable[1m]"), "high");
    }
}
