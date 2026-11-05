//! Account identity probes and sign-in command builders for every harness.
//!
//! Identity answers "who is signed in here" for one native config root without
//! ever returning or logging a credential. The JWT in Codex's `auth.json` is
//! decoded only for its public claims, with no signature check.

use kybern_protocol::process::NoWindow;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use kybern_protocol::ProviderKind;
use kybern_protocol::methods::{AccountIdentity, AccountLoginMode, AccountStatus};
use serde_json::Value;

use crate::{DriverError, ProbeContext, Result};

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

fn env_value(context: &ProbeContext, key: &str) -> Option<String> {
    match context.env.get(key) {
        // An empty override means "suppressed" for named accounts.
        Some(value) => (!value.is_empty()).then(|| value.clone()),
        None => std::env::var(key).ok().filter(|value| !value.is_empty()),
    }
}

fn home_dir(context: &ProbeContext) -> Option<PathBuf> {
    env_value(context, "HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(PathBuf::from))
}

async fn run_json(binary: &Path, args: &[&str], context: &ProbeContext) -> Option<(bool, Value)> {
    let mut command = crate::binary::command(binary).no_window();
    command.args(args).env_remove("NODE_OPTIONS").stdin(std::process::Stdio::null());
    if let Some(cwd) = context.cwd.as_deref() {
        command.current_dir(cwd);
    }
    command.envs(&context.env);
    let output = tokio::time::timeout(PROBE_TIMEOUT, crate::process_tree::output(&mut command)).await.ok()?.ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let value = serde_json::from_str(text.trim()).ok().or_else(|| {
        let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
        serde_json::from_str(line).ok()
    })?;
    Some((output.status.success(), value))
}

/// Who is signed in for this account root. Never errors: an unreadable state
/// is `Unknown`.
pub async fn identity(kind: ProviderKind, context: &ProbeContext) -> (AccountStatus, Option<AccountIdentity>) {
    match kind {
        ProviderKind::ClaudeCode => claude_identity(context).await,
        ProviderKind::Codex => codex_identity(context).await,
        ProviderKind::Cursor => cursor_identity(context).await,
        ProviderKind::Omp => omp_identity(context).await,
        ProviderKind::Opencode => opencode_identity(context),
        ProviderKind::Pi => pi_identity(context),
        ProviderKind::Antigravity => antigravity_identity(context).await,
        ProviderKind::GrokBuild => grok_identity(context),
    }
}

// ---- Grok Build ----

fn grok_identity(context: &ProbeContext) -> (AccountStatus, Option<AccountIdentity>) {
    if env_value(context, "XAI_API_KEY").is_some() {
        return (AccountStatus::SignedIn, Some(AccountIdentity { email: None, plan: Some("API key".into()), organization: None }));
    }
    (AccountStatus::Unknown, None)
}

// ---- Antigravity ----

/// Who is signed in to `agy` on this machine. Only the email and the plan label
/// leave this function: the stored token is read for its claims and dropped.
async fn antigravity_identity(context: &ProbeContext) -> (AccountStatus, Option<AccountIdentity>) {
    if let Some(login) = crate::antigravity::read_login(home_dir(context).as_deref()).await {
        return (AccountStatus::SignedIn, Some(AccountIdentity { email: login.email.clone(), plan: None, organization: None }));
    }
    if env_value(context, "GEMINI_API_KEY").is_some() {
        return (AccountStatus::SignedIn, Some(AccountIdentity { email: None, plan: Some("Gemini API key".into()), organization: None }));
    }
    // No login Kybern can read (a keychain it cannot reach, or none). `agy` itself knows.
    let Ok(binary) = crate::binary::resolve(ProviderKind::Antigravity, context.binary.as_ref()) else {
        return (AccountStatus::Unknown, None);
    };
    if crate::antigravity::signed_in_by_models(&binary, context).await {
        (AccountStatus::SignedIn, Some(AccountIdentity { email: None, plan: None, organization: None }))
    } else {
        (AccountStatus::SignedOut, None)
    }
}

// ---- Claude ----

async fn claude_identity(context: &ProbeContext) -> (AccountStatus, Option<AccountIdentity>) {
    let Ok(binary) = crate::binary::resolve(ProviderKind::ClaudeCode, context.binary.as_ref()) else {
        return (AccountStatus::Unknown, None);
    };
    if let Some(oauth) = crate::claude_config::login_due_for_refresh(context).await {
        return claude_identity_from_login(&oauth);
    }
    let Some((_, status)) = run_json(&binary, &["auth", "status", "--json"], context).await else {
        return (AccountStatus::Unknown, None);
    };
    let credentials_file = env_value(context, "CLAUDE_CONFIG_DIR").map(|dir| Path::new(&dir).join(".credentials.json").is_file());
    claude_identity_from_status(&status, credentials_file.unwrap_or(false))
}

pub fn claude_identity_from_status(status: &Value, credentials_file: bool) -> (AccountStatus, Option<AccountIdentity>) {
    if status.get("loggedIn").and_then(Value::as_bool) != Some(true) {
        let state = if credentials_file { AccountStatus::NeedsSignIn } else { AccountStatus::SignedOut };
        return (state, None);
    }
    let text = |key: &str| status.get(key).and_then(Value::as_str).map(str::trim).filter(|value| !value.is_empty()).map(str::to_string);
    let auth_method = text("authMethod").unwrap_or_default().to_ascii_lowercase();
    let plan = match text("subscriptionType").map(|plan| plan.to_ascii_lowercase()) {
        Some(plan) => Some(claude_plan_label(&plan)),
        None if auth_method.contains("api") => Some("API key".into()),
        None => None,
    };
    (AccountStatus::SignedIn, Some(AccountIdentity { email: text("email"), plan, organization: text("orgName") }))
}

/// A login due for a refresh, read from Claude Code's stored `claudeAiOauth`
/// without running it. Its refresh token lets the next session renew it, so it
/// is still signed in; the store has no email, which stays unknown.
pub fn claude_identity_from_login(oauth: &Value) -> (AccountStatus, Option<AccountIdentity>) {
    let plan = oauth
        .get("subscriptionType")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|plan| !plan.is_empty())
        .map(|plan| claude_plan_label(&plan.to_ascii_lowercase()));
    (AccountStatus::SignedIn, Some(AccountIdentity { plan, ..Default::default() }))
}

pub fn claude_plan_label(plan: &str) -> String {
    match plan {
        "max" => "Max".into(),
        "pro" => "Pro".into(),
        "team" => "Team".into(),
        "enterprise" => "Enterprise".into(),
        other => title_case(other),
    }
}

// ---- Codex ----

async fn codex_identity(context: &ProbeContext) -> (AccountStatus, Option<AccountIdentity>) {
    let home = env_value(context, "CODEX_HOME").map(PathBuf::from).or_else(|| home_dir(context).map(|home| home.join(".codex")));
    let Some(home) = home else { return (AccountStatus::Unknown, None) };
    let file = home.join("auth.json");
    let Ok(text) = std::fs::read_to_string(&file) else { return (AccountStatus::SignedOut, None) };
    let Some(identity) = codex_identity_from_auth(&text) else { return (AccountStatus::Unknown, None) };
    if identity.plan.as_deref() == Some("API key") {
        return (AccountStatus::SignedIn, Some(identity));
    }
    let Ok(binary) = crate::binary::resolve(ProviderKind::Codex, context.binary.as_ref()) else {
        return (AccountStatus::SignedIn, Some(identity));
    };
    let mut command = crate::binary::command(&binary).no_window();
    command.args(["login", "status"]).stdin(std::process::Stdio::null()).envs(&context.env);
    let valid = match tokio::time::timeout(PROBE_TIMEOUT, crate::process_tree::output(&mut command)).await {
        Ok(Ok(output)) => output.status.success(),
        _ => true,
    };
    (if valid { AccountStatus::SignedIn } else { AccountStatus::NeedsSignIn }, Some(identity))
}

/// Public claims from `auth.json`. The token is decoded locally and never kept.
pub fn codex_identity_from_auth(text: &str) -> Option<AccountIdentity> {
    let auth: Value = serde_json::from_str(text).ok()?;
    let api_key = auth.get("OPENAI_API_KEY").and_then(Value::as_str).filter(|key| !key.is_empty()).is_some();
    let claims = auth.pointer("/tokens/id_token").and_then(Value::as_str).and_then(jwt_claims);
    if claims.is_none() && api_key {
        return Some(AccountIdentity { email: None, plan: Some("API key".into()), organization: None });
    }
    let claims = claims?;
    let email = claims.get("email").and_then(Value::as_str).filter(|email| !email.is_empty()).map(str::to_string);
    let plan = claims
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_plan_type")
        .and_then(Value::as_str)
        .map(|plan| codex_plan_label(&plan.to_ascii_lowercase()));
    Some(AccountIdentity { email, plan, organization: None })
}

pub fn codex_plan_label(plan: &str) -> String {
    match plan {
        "plus" => "Plus".into(),
        "pro" => "Pro".into(),
        "team" => "Team".into(),
        "business" => "Business".into(),
        "enterprise" => "Enterprise".into(),
        other => title_case(other),
    }
}

fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

// ---- Cursor ----

async fn cursor_identity(context: &ProbeContext) -> (AccountStatus, Option<AccountIdentity>) {
    match crate::cursor::auth_status(context).await {
        Ok(value) => cursor_identity_from_status(&value),
        Err(_) => (AccountStatus::Unknown, None),
    }
}

pub fn cursor_identity_from_status(value: &Value) -> (AccountStatus, Option<AccountIdentity>) {
    let email = value["email"].as_str().filter(|email| !email.is_empty()).map(str::to_string);
    match value["status"].as_str() {
        Some("logged-in") => (AccountStatus::SignedIn, Some(AccountIdentity { email, ..Default::default() })),
        Some("api-key") => (AccountStatus::SignedIn, Some(AccountIdentity { plan: Some("API key".into()), ..Default::default() })),
        Some("logged-out") => (AccountStatus::SignedOut, None),
        _ => (AccountStatus::Unknown, None),
    }
}

// ---- omp ----

/// omp has no account of its own: it runs on whichever providers it holds a login
/// or key for. `omp auth-broker status` only describes an auth broker (a credential
/// vault most installs never set up), so a local login (`omp login
/// google-antigravity`, `omp login anthropic`, …) or an API key is read from the
/// models omp lists, which are only those it can run.
async fn omp_identity(context: &ProbeContext) -> (AccountStatus, Option<AccountIdentity>) {
    let Ok(binary) = crate::binary::resolve(ProviderKind::Omp, context.binary.as_ref()) else {
        return (AccountStatus::Unknown, None);
    };
    let broker = run_json(&binary, &["auth-broker", "status", "--json"], context).await.map(|(_, value)| omp_identity_from_status(&value));
    if let Some(signed_in @ (AccountStatus::SignedIn, _)) = broker {
        return signed_in;
    }
    match crate::pi::omp_catalog(&binary, context).await {
        Some(models) => omp_identity_from_models(&models).unwrap_or_else(|| broker.unwrap_or((AccountStatus::SignedOut, None))),
        None => broker.unwrap_or((AccountStatus::Unknown, None)),
    }
}

/// Signed in when omp lists a model of a provider the user set up; the providers
/// are named as the plan ("Google Antigravity", "ChatGPT, Claude"). `None` when
/// only on-device models are listed.
pub fn omp_identity_from_models(models: &[kybern_protocol::ProviderModel]) -> Option<(AccountStatus, Option<AccountIdentity>)> {
    let providers = crate::pi::omp_configured_providers(models);
    if providers.is_empty() {
        return None;
    }
    let mut names: Vec<String> = providers.iter().take(2).map(|provider| omp_provider_label(provider)).collect();
    if providers.len() > 2 {
        names.push(format!("{} more", providers.len() - 2));
    }
    Some((AccountStatus::SignedIn, Some(AccountIdentity { email: None, plan: Some(names.join(", ")), organization: None })))
}

/// A readable name for an omp provider id.
pub fn omp_provider_label(provider: &str) -> String {
    match provider {
        "google-antigravity" => "Google Antigravity".into(),
        "google-gemini-cli" => "Gemini CLI".into(),
        "google" => "Google".into(),
        "anthropic" => "Claude".into(),
        "openai-codex" | "openai-codex-device" => "ChatGPT".into(),
        "openai" => "OpenAI".into(),
        "github-copilot" => "GitHub Copilot".into(),
        "openrouter" => "OpenRouter".into(),
        "xai" | "xai-oauth" => "xAI".into(),
        other => title_case(other),
    }
}

/// `omp auth-broker status --json` answers `{"ok":false,"reason":"not_configured"}` when
/// nothing is saved. The signed-in shape is unverified: `ok: true` counts as
/// signed in, and a provider label is shown as the plan when one is present.
pub fn omp_identity_from_status(value: &Value) -> (AccountStatus, Option<AccountIdentity>) {
    match value.get("ok").and_then(Value::as_bool) {
        Some(true) => {
            let label = ["plan", "label", "provider"]
                .iter()
                .find_map(|key| value.get(*key).and_then(Value::as_str))
                .filter(|label| !label.is_empty())
                .map(str::to_string);
            let email = value.get("email").and_then(Value::as_str).filter(|email| !email.is_empty()).map(str::to_string);
            (AccountStatus::SignedIn, Some(AccountIdentity { email, plan: label, organization: None }))
        }
        Some(false) => match value.get("reason").and_then(Value::as_str) {
            Some("not_configured") | None => (AccountStatus::SignedOut, None),
            Some(_) => (AccountStatus::NeedsSignIn, None),
        },
        None => (AccountStatus::Unknown, None),
    }
}

// ---- OpenCode and Pi: key names only ----

fn opencode_identity(context: &ProbeContext) -> (AccountStatus, Option<AccountIdentity>) {
    let data = env_value(context, "XDG_DATA_HOME").map(PathBuf::from).or_else(|| home_dir(context).map(|home| home.join(".local/share")));
    let Some(data) = data else { return (AccountStatus::Unknown, None) };
    let Ok(text) = std::fs::read_to_string(data.join("opencode/auth.json")) else { return (AccountStatus::SignedOut, None) };
    key_names_identity(&text)
}

fn pi_identity(context: &ProbeContext) -> (AccountStatus, Option<AccountIdentity>) {
    let dir = env_value(context, "PI_CODING_AGENT_DIR").map(PathBuf::from).or_else(|| home_dir(context).map(|home| home.join(".pi/agent")));
    let Some(dir) = dir else { return (AccountStatus::Unknown, None) };
    let Ok(text) = std::fs::read_to_string(dir.join("auth.json")) else { return (AccountStatus::SignedOut, None) };
    // Shape unverified (Pi is not installed on the build machine): only report
    // that something is saved.
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) if !map.is_empty() => (AccountStatus::SignedIn, Some(AccountIdentity::default())),
        Ok(Value::Object(_)) => (AccountStatus::SignedOut, None),
        _ => (AccountStatus::Unknown, None),
    }
}

/// Only the provider names in an `auth.json` object; values are never read.
pub fn key_names_identity(text: &str) -> (AccountStatus, Option<AccountIdentity>) {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) else { return (AccountStatus::Unknown, None) };
    let Some(first) = map.keys().next() else { return (AccountStatus::SignedOut, None) };
    let label = match first.as_str() {
        "anthropic" => "Anthropic".to_string(),
        "openai" => "OpenAI".to_string(),
        "google" => "Google".to_string(),
        "openrouter" => "OpenRouter".to_string(),
        other => title_case(other),
    };
    (AccountStatus::SignedIn, Some(AccountIdentity { plan: Some(format!("API key · {label}")), ..Default::default() }))
}

fn title_case(text: &str) -> String {
    text.split(['_', '-', ' ', '.', '+'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map(|first| first.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---- Sign-in commands ----

/// Arguments (after the binary) for a harness sign-in in `mode`. Cursor signs
/// in through `cursor::login_command`. Harnesses without a mode return an error
/// so the caller can fall back to the terminal.
pub fn login_args(kind: ProviderKind, mode: AccountLoginMode, upstream: Option<&str>) -> Result<Vec<String>> {
    let unsupported = || DriverError::Unsupported(format!("{} can't sign in with that method here.", kind.display_name()));
    let strings = |args: &[&str]| args.iter().map(|arg| (*arg).to_string()).collect::<Vec<_>>();
    match (kind, mode) {
        (ProviderKind::ClaudeCode, AccountLoginMode::Browser | AccountLoginMode::Paste | AccountLoginMode::Terminal) => {
            Ok(strings(&["auth", "login"]))
        }
        (ProviderKind::Codex, AccountLoginMode::Browser | AccountLoginMode::Terminal) => Ok(strings(&["login"])),
        (ProviderKind::Codex, AccountLoginMode::DeviceCode) => Ok(strings(&["login", "--device-auth"])),
        (ProviderKind::Omp, AccountLoginMode::Browser | AccountLoginMode::Paste) => {
            let mut args = strings(&["login"]);
            if let Some(upstream) = upstream.filter(|upstream| !upstream.is_empty()) {
                args.push(upstream.to_string());
            }
            Ok(args)
        }
        (ProviderKind::Omp, AccountLoginMode::Terminal) => Ok(strings(&["auth-broker", "login"])),
        (ProviderKind::Opencode, AccountLoginMode::Terminal) => Ok(strings(&["auth", "login"])),
        // Interactive native /login selects the upstream provider; the caller types it.
        (ProviderKind::Pi, AccountLoginMode::Terminal) => Ok(Vec::new()),
        // Bare `agy` opens its interface and starts Google's browser sign-in when signed out.
        (ProviderKind::Antigravity, AccountLoginMode::Terminal) => Ok(Vec::new()),
        _ => Err(unsupported()),
    }
}

/// Arguments for signing out of the account root, when the harness supports it.
pub fn sign_out_args(kind: ProviderKind, upstream: Option<&str>) -> Option<Vec<String>> {
    match kind {
        ProviderKind::ClaudeCode => Some(vec!["auth".into(), "logout".into()]),
        ProviderKind::Codex => Some(vec!["logout".into()]),
        ProviderKind::Omp => upstream.map(|upstream| vec!["logout".into(), upstream.into()]),
        _ => None,
    }
}

// ---- Output parsing ----

/// Remove ANSI escape sequences (CSI and OSC) from terminal output.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Every `https://` URL in the text, in order, trimmed of trailing punctuation.
pub fn extract_https_urls(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("https://") {
        let tail = &rest[start..];
        let end = tail.find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '\\')).unwrap_or(tail.len());
        let url = tail[..end].trim_end_matches(['.', ',', ')', ']']);
        if url.len() > "https://".len() {
            urls.push(url.to_string());
        }
        rest = &tail[end..];
    }
    urls
}

/// The host of an `http(s)` URL, lowercased.
pub fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?.split(':').next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// A URL whose OAuth redirect does not point back at this machine: the
/// manual page used by paste mode.
pub fn is_manual_redirect_url(url: &str) -> bool {
    let decoded = url.replace("%3A", ":").replace("%2F", "/").to_ascii_lowercase();
    !(decoded.contains("redirect_uri=http://localhost") || decoded.contains("redirect_uri=http://127.0.0.1"))
}

/// The URL and one-time code from `codex login --device-auth` output.
pub fn parse_device_code(text: &str) -> (Option<String>, Option<String>) {
    let clean = strip_ansi(text);
    let url = extract_https_urls(&clean).into_iter().next();
    let code = clean.split_whitespace().find_map(|word| {
        let word = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-');
        let (left, right) = word.split_once('-')?;
        let ok = |part: &str| (3..=8).contains(&part.len()) && part.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
        (ok(left) && ok(right) && word.chars().any(|c| c.is_ascii_digit() || c.is_ascii_uppercase())).then(|| word.to_string())
    });
    (url, code)
}

/// Remove secret-looking query values (`code`, `state`, `code_challenge`,
/// `access_token` and so on) and long opaque tokens from a diagnostic line.
pub fn redact(text: &str) -> String {
    const KEYS: &[&str] =
        &["code=", "state=", "code_challenge=", "access_token=", "refresh_token=", "id_token=", "token=", "challenge=", "key="];
    let mut out = String::new();
    for (index, word) in text.split(' ').enumerate() {
        if index > 0 {
            out.push(' ');
        }
        let mut word = word.to_string();
        for key in KEYS {
            let mut from = 0;
            while let Some(position) = word[from..].find(key) {
                let start = from + position;
                let boundary = start == 0 || matches!(word.as_bytes()[start - 1], b'?' | b'&' | b'"' | b'\'');
                let value_start = start + key.len();
                if !boundary {
                    from = value_start;
                    continue;
                }
                let value_end = word[value_start..].find(['&', '"', '\'']).map(|end| value_start + end).unwrap_or(word.len());
                word.replace_range(value_start..value_end, "[redacted]");
                from = value_start + "[redacted]".len();
            }
        }
        let opaque = word.len() >= 32 && word.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        out.push_str(if opaque { "[redacted]" } else { &word });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jwt(claims: &Value) -> String {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!("{}.{}.sig", engine.encode("{}"), engine.encode(claims.to_string()))
    }

    #[test]
    fn claude_status_maps_plan_and_organization() {
        let status = json!({"loggedIn": true, "authMethod": "claude.ai", "email": "dev@arunika.co", "orgName": "Arunika Studio", "subscriptionType": "pro"});
        let (state, identity) = claude_identity_from_status(&status, false);
        assert_eq!(state, AccountStatus::SignedIn);
        let identity = identity.unwrap();
        assert_eq!(identity.email.as_deref(), Some("dev@arunika.co"));
        assert_eq!(identity.plan.as_deref(), Some("Pro"));
        assert_eq!(identity.organization.as_deref(), Some("Arunika Studio"));
        for (raw, label) in [("max", "Max"), ("team", "Team"), ("enterprise", "Enterprise")] {
            assert_eq!(claude_plan_label(raw), label);
        }
        let api = json!({"loggedIn": true, "authMethod": "api_key"});
        assert_eq!(claude_identity_from_status(&api, false).1.unwrap().plan.as_deref(), Some("API key"));
    }

    #[test]
    fn claude_login_due_for_refresh_stays_signed_in_with_its_plan() {
        let (state, identity) =
            claude_identity_from_login(&json!({"accessToken": "a", "refreshToken": "r", "expiresAt": 1, "subscriptionType": "max"}));
        assert_eq!(state, AccountStatus::SignedIn);
        assert_eq!(identity, Some(AccountIdentity { plan: Some("Max".into()), ..Default::default() }));
        assert_eq!(claude_identity_from_login(&json!({"refreshToken": "r"})).1.unwrap().plan, None);
    }

    #[tokio::test]
    async fn claude_identity_never_runs_auth_status_for_a_login_due_for_refresh() {
        let temp = tempfile::tempdir().unwrap();
        // Not a program: running it fails, which would make the identity unknown.
        let binary = temp.path().join("claude.txt");
        std::fs::write(&binary, "not a program").unwrap();
        let context = ProbeContext {
            binary: Some(binary),
            cwd: None,
            env: [("CLAUDE_CONFIG_DIR".to_string(), temp.path().to_string_lossy().into_owned())].into(),
        };
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        let write = |expires_at: u64| {
            let oauth = json!({"accessToken": "a", "refreshToken": "r", "expiresAt": expires_at, "subscriptionType": "pro"});
            std::fs::write(temp.path().join(".credentials.json"), json!({"claudeAiOauth": oauth}).to_string()).unwrap();
        };

        write(now - 1);
        let (state, identity) = claude_identity(&context).await;
        assert_eq!(state, AccountStatus::SignedIn, "an expired access token with a refresh token is not signed out");
        assert_eq!(identity.unwrap().plan.as_deref(), Some("Pro"));

        write(now + 8 * 60 * 60 * 1000);
        assert_eq!(claude_identity(&context).await.0, AccountStatus::Unknown, "a login that is not due still asks Claude Code");
    }

    #[test]
    fn claude_logged_out_distinguishes_needs_sign_in_by_credentials_file() {
        let status = json!({"loggedIn": false, "authMethod": "none"});
        assert_eq!(claude_identity_from_status(&status, false).0, AccountStatus::SignedOut);
        assert_eq!(claude_identity_from_status(&status, true).0, AccountStatus::NeedsSignIn);
    }

    #[test]
    fn codex_identity_reads_public_claims_only() {
        let token = jwt(&json!({"email": "Tresh.Pro@gmail.com", "https://api.openai.com/auth": {"chatgpt_plan_type": "plus"}}));
        let auth = json!({"tokens": {"id_token": token, "access_token": "secret-access", "refresh_token": "secret-refresh"}}).to_string();
        let identity = codex_identity_from_auth(&auth).unwrap();
        assert_eq!(identity.email.as_deref(), Some("Tresh.Pro@gmail.com"));
        assert_eq!(identity.plan.as_deref(), Some("Plus"));
        assert!(!format!("{identity:?}").contains("secret"));
        let api = json!({"OPENAI_API_KEY": "sk-secret"}).to_string();
        let identity = codex_identity_from_auth(&api).unwrap();
        assert_eq!(identity.plan.as_deref(), Some("API key"));
        assert_eq!(identity.email, None);
        assert!(codex_identity_from_auth("not json").is_none());
    }

    #[test]
    fn cursor_omp_and_key_name_shapes() {
        assert_eq!(cursor_identity_from_status(&json!({"status": "logged-in", "email": "a@b.c"})).0, AccountStatus::SignedIn);
        assert_eq!(cursor_identity_from_status(&json!({"status": "logged-out"})).0, AccountStatus::SignedOut);
        assert_eq!(omp_identity_from_status(&json!({"ok": false, "reason": "not_configured"})).0, AccountStatus::SignedOut);
        assert_eq!(omp_identity_from_status(&json!({"ok": true, "provider": "Claude Max"})).1.unwrap().plan.as_deref(), Some("Claude Max"));
        let (state, identity) = key_names_identity(r#"{"anthropic": {"type": "api", "key": "sk-secret"}}"#);
        assert_eq!(state, AccountStatus::SignedIn);
        assert_eq!(identity.unwrap().plan.as_deref(), Some("API key · Anthropic"));
        assert_eq!(key_names_identity("{}").0, AccountStatus::SignedOut);
    }

    #[test]
    fn omp_is_signed_in_through_any_provider_it_lists() {
        let models = crate::pi::omp_models_from_json(include_bytes!("../tests/fixtures/omp-models-antigravity.json")).unwrap();
        let (state, identity) = omp_identity_from_models(&models).unwrap();
        assert_eq!(state, AccountStatus::SignedIn);
        assert_eq!(identity.unwrap().plan.as_deref(), Some("Google Antigravity"));
        let model = |provider: &str| kybern_protocol::ProviderModel {
            id: format!("{provider}/m"),
            display_name: "m".into(),
            resolved_id: None,
            description: None,
            provider: Some(provider.into()),
            efforts: Vec::new(),
            default_effort: None,
            is_default: false,
            parameters: Vec::new(),
            variants: Vec::new(),
        };
        let many = ["openai-codex", "apple", "anthropic", "openrouter"].map(model);
        assert_eq!(omp_identity_from_models(&many).unwrap().1.unwrap().plan.as_deref(), Some("ChatGPT, Claude, 1 more"));
        assert!(omp_identity_from_models(&[model("apple")]).is_none());
        assert_eq!(omp_provider_label("kimi-code"), "Kimi Code");
    }

    /// A fake omp with no auth broker but a local Antigravity login: Kybern must
    /// report it signed in, not ask for a Claude (or any) sign-in.
    #[cfg(unix)]
    #[tokio::test]
    async fn omp_without_a_broker_reads_its_local_logins() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = directory.path().join("models.json");
        std::fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/omp-models-antigravity.json"), &catalog).unwrap();
        let binary = directory.path().join("omp");
        kybern_protocol::process::write_executable(
            &binary,
            format!(
                "#!/bin/sh\ncase \"$1 $2\" in\n  'auth-broker status') echo '{{\"ok\":false,\"reason\":\"not_configured\"}}' ;;\n  'models ls') cat '{}' ;;\n  *) exit 3 ;;\nesac\n",
                catalog.display()
            ),
        )
        .unwrap();
        let context = ProbeContext { binary: Some(binary.clone()), ..ProbeContext::default() };
        let mut probed = identity(ProviderKind::Omp, &context).await;
        for _ in 0..50 {
            // A sibling test's freshly forked child can hold the new script busy (ETXTBSY).
            if probed.0 != AccountStatus::Unknown {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            probed = identity(ProviderKind::Omp, &context).await;
        }
        assert_eq!(probed.0, AccountStatus::SignedIn);
        assert_eq!(probed.1.unwrap().plan.as_deref(), Some("Google Antigravity"));

        // Nothing set up: only the broker's answer is left, and it says signed out.
        std::fs::write(&catalog, r#"{"models":[{"provider":"apple","selector":"apple/on-device","name":"Apple AFM 3 Core"}]}"#).unwrap();
        assert_eq!(identity(ProviderKind::Omp, &context).await.0, AccountStatus::SignedOut);
    }

    #[test]
    fn parses_captured_login_output() {
        // Captured outputs with the OAuth state and challenge values replaced.
        let claude = "Opening browser to sign in\u{2026}\nIf the browser didn't open, visit: https://claude.com/cai/oauth/authorize?code=true&client_id=x&response_type=code&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&code_challenge=CHALLENGE&state=STATE\nPaste code here if prompted > ";
        let urls = extract_https_urls(&strip_ansi(claude));
        assert_eq!(urls.len(), 1);
        assert!(is_manual_redirect_url(&urls[0]));
        assert_eq!(url_host(&urls[0]).as_deref(), Some("claude.com"));
        let shim = "https://claude.com/cai/oauth/authorize?code=true&redirect_uri=http%3A%2F%2Flocalhost%3A56096%2Fcallback&state=S";
        assert!(!is_manual_redirect_url(shim));
        let device = "\n\u{1b}[90mWelcome\u{1b}[0m\n1. Open this link\n   \u{1b}[94mhttps://auth.openai.com/codex/device\u{1b}[0m\n\n2. Enter this one-time code \u{1b}[90m(expires in 15 minutes)\u{1b}[0m\n   \u{1b}[94mKE4S-OF0CW\u{1b}[0m\n";
        let (url, code) = parse_device_code(device);
        assert_eq!(url.as_deref(), Some("https://auth.openai.com/codex/device"));
        assert_eq!(code.as_deref(), Some("KE4S-OF0CW"));
    }

    #[test]
    fn redacts_oauth_values() {
        let line =
            "failed https://x.test/cb?code=abc123&state=secretstate&code_challenge=zzz token eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9abcdefgh";
        let clean = redact(line);
        for secret in ["abc123", "secretstate", "zzz", "eyJhbGci"] {
            assert!(!clean.contains(secret), "{clean}");
        }
    }

    #[test]
    fn login_arguments_per_harness() {
        assert_eq!(login_args(ProviderKind::ClaudeCode, AccountLoginMode::Paste, None).unwrap(), ["auth", "login"]);
        assert_eq!(login_args(ProviderKind::Codex, AccountLoginMode::DeviceCode, None).unwrap(), ["login", "--device-auth"]);
        assert_eq!(login_args(ProviderKind::Omp, AccountLoginMode::Browser, Some("anthropic")).unwrap(), ["login", "anthropic"]);
        assert!(login_args(ProviderKind::Opencode, AccountLoginMode::Browser, None).is_err());
        assert_eq!(sign_out_args(ProviderKind::Codex, None).unwrap(), ["logout"]);
        assert!(sign_out_args(ProviderKind::Opencode, None).is_none());
    }
}
