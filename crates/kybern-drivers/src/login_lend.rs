//! Lending and borrowing a login between machines (ADE-86 S4, design §9.4).
//!
//! **Lender side** (the broker): read the access token a harness keeps for an account, without ever
//! touching the refresh token, and ask Claude Code to refresh its own login when it is due. Kybern
//! never redeems a refresh token itself: Anthropic rotates it on every use, and a Claude process that is
//! killed mid-refresh is what logged users out every morning (issue #86).
//!
//! **Borrower side**: a [`LoginSource`] answers for one session. The daemon registers it and puts only an
//! opaque handle in the session's environment ([`BORROW_ENV`]); the Claude and Codex drivers resolve the
//! handle themselves, so a lent token is never in the daemon's session config, its fingerprint or its logs.
//!
//! * Claude Code: `CLAUDE_CODE_OAUTH_TOKEN` in the session's environment. Nothing is written to disk and
//!   no Keychain item is created.
//! * Codex: the app-server's external auth, `account/login/start { type: "chatgptAuthTokens" }`, answering
//!   `account/chatgptAuthTokens/refresh` with a fresh borrow. The token is held in memory only.
//! * omp: its own broker (`omp auth-broker serve`); the daemon sets the two `OMP_AUTH_BROKER_*` variables.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use kybern_protocol::process::NoWindow;
use kybern_protocol::{BorrowedLogin, ProviderKind};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::ProbeContext;

/// The session environment variable that carries a handle to the session's [`LoginSource`]. It is a
/// random id, not a secret, and the drivers remove it before spawning the harness.
pub const BORROW_ENV: &str = "KYBERN_BORROWED_LOGIN";
/// Claude Code's mid-session token swap (undocumented, first-party entrypoints only). Off unless the
/// session sets this to `1`: spoofing `CLAUDE_CODE_ENTRYPOINT` may change other behavior or telemetry.
pub const CLAUDE_HOTSWAP_ENV: &str = "KYBERN_CLAUDE_LOGIN_HOTSWAP";
/// What the daemon sets for omp next to `PI_CODING_AGENT_DIR`.
pub const OMP_BROKER_URL_ENV: &str = "OMP_AUTH_BROKER_URL";
pub const OMP_BROKER_TOKEN_ENV: &str = "OMP_AUTH_BROKER_TOKEN";

/// Why a session asks for a new credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenewReason {
    /// The harness got a 401 and has no refresh token of its own.
    Unauthorized,
    /// The access token's own expiry is near.
    Expiring,
}

/// Where one session's borrowed credential comes from. Implemented by the daemon over `peer.login.*`.
#[async_trait]
pub trait LoginSource: Send + Sync {
    /// The credential this session currently holds. Never a network call.
    async fn current(&self) -> std::result::Result<BorrowedLogin, String>;
    /// Borrow again: the broker rotated, or the harness was rejected. The text of an error is for the
    /// user ("Claude signs in through the VPS, which is offline.").
    async fn renew(&self, reason: RenewReason) -> std::result::Result<BorrowedLogin, String>;
}

fn registry() -> &'static Mutex<HashMap<String, Arc<dyn LoginSource>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<dyn LoginSource>>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// Register a session's source; returns the handle for [`BORROW_ENV`].
pub fn register(source: Arc<dyn LoginSource>) -> String {
    let handle = uuid::Uuid::new_v4().simple().to_string();
    registry().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(handle.clone(), source);
    handle
}

pub fn lookup(handle: &str) -> Option<Arc<dyn LoginSource>> {
    registry().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(handle).cloned()
}

/// Drop a handle once its session is gone.
pub fn unregister(handle: &str) {
    registry().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(handle);
}

/// The source a session's environment names, if any.
pub fn source_of(env: &HashMap<String, String>) -> Option<Arc<dyn LoginSource>> {
    env.get(BORROW_ENV).and_then(|handle| lookup(handle))
}

// ---- errors that mean "the lent login no longer works" ----

/// Whether a failed turn's text says the harness' login was rejected. Used to re-borrow, resume and
/// send the message again. The strings are the ones the spike saw (`Failed to authenticate: OAuth
/// token revoked…`, Codex `workspace routing discovery unauthorized (401)`); an expired token reads
/// "expired" instead of "revoked" on the same `authentication_failed` path.
pub fn is_login_failure(kind: ProviderKind, text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    match kind {
        ProviderKind::ClaudeCode => {
            text.contains("oauth token revoked")
                || text.contains("oauth token has expired")
                || text.contains("authentication_failed")
                || text.contains("failed to authenticate")
                || text.contains("invalid bearer token")
        }
        ProviderKind::Codex => {
            text.contains("unauthorized (401)") || text.contains("empty_string") || text.contains("failed to refresh token")
        }
        _ => false,
    }
}

// ---- borrower side: what each harness gets ----

/// The environment Claude Code needs for a borrowed login: the access token, nothing else. The
/// session's `CLAUDE_CONFIG_DIR` (the named account's folder) keeps its transcripts, so `--resume`
/// finds them; it holds no credential.
pub fn claude_env(login: &BorrowedLogin) -> Option<Vec<(&'static str, String)>> {
    match login {
        BorrowedLogin::Claude { access_token, .. } => Some(vec![("CLAUDE_CODE_OAUTH_TOKEN", access_token.clone())]),
        _ => None,
    }
}

/// The two variables that point omp at the broker's `omp auth-broker serve`.
pub fn omp_env(login: &BorrowedLogin) -> Option<Vec<(&'static str, String)>> {
    match login {
        BorrowedLogin::Omp { url, token } => Some(vec![(OMP_BROKER_URL_ENV, url.clone()), (OMP_BROKER_TOKEN_ENV, token.clone())]),
        _ => None,
    }
}

/// The params of Codex's `account/login/start` for external auth, or of the answer to
/// `account/chatgptAuthTokens/refresh` (without the `type`).
pub fn codex_external_auth(login: &BorrowedLogin) -> Option<Value> {
    match login {
        BorrowedLogin::Codex { access_token, account_id, plan_type, .. } => Some(serde_json::json!({
            "accessToken": access_token,
            "chatgptAccountId": account_id,
            "chatgptPlanType": plan_type,
        })),
        _ => None,
    }
}

// ---- lender side: reading what the harness keeps ----

/// A Claude Code login as the broker reads it. Only the access token travels; `Debug` shows none.
#[derive(Clone)]
pub struct ClaudeStored {
    pub access_token: String,
    pub expires_at_ms: Option<i64>,
    pub plan: Option<String>,
    /// `user:profile` is in the scopes, so the token can read plan limits.
    pub has_profile_scope: bool,
}

impl std::fmt::Debug for ClaudeStored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeStored").field("expires_at_ms", &self.expires_at_ms).field("plan", &self.plan).finish_non_exhaustive()
    }
}

/// Parse the JSON Claude Code stores (`.credentials.json` or the Keychain item).
pub fn claude_stored_from_json(value: &Value) -> Option<ClaudeStored> {
    let oauth = value.get("claudeAiOauth")?;
    let access_token = oauth.get("accessToken")?.as_str().filter(|token| !token.is_empty())?.to_string();
    let scopes: Vec<&str> = match oauth.get("scopes") {
        Some(Value::Array(scopes)) => scopes.iter().filter_map(Value::as_str).collect(),
        _ => oauth.get("scope").and_then(Value::as_str).map(|scope| scope.split_whitespace().collect()).unwrap_or_default(),
    };
    Some(ClaudeStored {
        access_token,
        expires_at_ms: oauth.get("expiresAt").and_then(Value::as_i64),
        plan: plan_name(oauth.get("subscriptionType").and_then(Value::as_str), oauth.get("rateLimitTier").and_then(Value::as_str)),
        has_profile_scope: scopes.is_empty() || scopes.contains(&"user:profile"),
    })
}

fn plan_name(subscription: Option<&str>, tier: Option<&str>) -> Option<String> {
    let subscription = subscription.filter(|value| !value.is_empty())?;
    let mut name = subscription[..1].to_uppercase() + &subscription[1..];
    if let Some(multiple) = tier
        .and_then(|tier| tier.rsplit('_').next())
        .filter(|part| part.ends_with('x') && part.len() > 1 && part[..part.len() - 1].chars().all(|c| c.is_ascii_digit()))
    {
        name.push_str(&format!(" ({multiple})"));
    }
    Some(name)
}

fn env_value(context: &ProbeContext, key: &str) -> Option<String> {
    context.env.get(key).cloned().or_else(|| std::env::var(key).ok()).filter(|value| !value.trim().is_empty())
}

fn claude_config_dir(context: &ProbeContext) -> Option<PathBuf> {
    env_value(context, "CLAUDE_CONFIG_DIR").map(PathBuf::from).or_else(|| {
        env_value(context, "HOME").or_else(|| env_value(context, "USERPROFILE")).map(|home| PathBuf::from(home).join(".claude"))
    })
}

/// The macOS Keychain service Claude Code uses for a config directory: the plain name for the default
/// directory, and the name plus the first 8 hex digits of the SHA-256 of the path for a custom one.
pub fn claude_keychain_service(custom_config_dir: Option<&str>) -> String {
    match custom_config_dir {
        Some(dir) => {
            let digest = Sha256::digest(dir.as_bytes());
            let suffix: String = digest.iter().take(4).map(|byte| format!("{byte:02x}")).collect();
            format!("Claude Code-credentials-{suffix}")
        }
        None => "Claude Code-credentials".into(),
    }
}

/// Read the broker's Claude login for the account the context describes: `.credentials.json` first,
/// then the macOS Keychain. The one that expires last is the live one.
pub async fn read_claude(context: &ProbeContext) -> Option<ClaudeStored> {
    let from_file = claude_config_dir(context)
        .and_then(|dir| std::fs::read(dir.join(".credentials.json")).ok())
        .and_then(|bytes| decode_json_or_hex(&bytes))
        .and_then(|value| claude_stored_from_json(&value));
    let from_keychain = read_claude_keychain(context).await;
    match (from_file, from_keychain) {
        (Some(file), Some(keychain)) => Some(if file.expires_at_ms > keychain.expires_at_ms { file } else { keychain }),
        (file, keychain) => file.or(keychain),
    }
}

#[cfg(target_os = "macos")]
async fn read_claude_keychain(context: &ProbeContext) -> Option<ClaudeStored> {
    let account = env_value(context, "USER").or_else(|| std::env::var("USER").ok())?;
    let custom = env_value(context, "CLAUDE_CONFIG_DIR");
    let service = claude_keychain_service(custom.as_deref());
    let mut command = tokio::process::Command::new("/usr/bin/security").no_window();
    command.args(["find-generic-password", "-a", &account, "-w", "-s", &service]);
    let output = tokio::time::timeout(Duration::from_secs(2), crate::process_tree::output(&mut command)).await.ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    decode_json_or_hex(&output.stdout).and_then(|value| claude_stored_from_json(&value))
}

#[cfg(not(target_os = "macos"))]
async fn read_claude_keychain(_context: &ProbeContext) -> Option<ClaudeStored> {
    None
}

/// A stored login's JSON; the Keychain item may hold the JSON itself or its hex encoding.
pub fn decode_json_or_hex(bytes: &[u8]) -> Option<Value> {
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

/// Ask Claude Code on the broker to refresh its own login, the one way Kybern ever causes a rotation:
/// by running Claude. The process is closed through stdin and left to finish, never killed: a SIGKILL
/// in the middle of a refresh loses the new token pair (issue #86).
pub async fn refresh_claude(context: &ProbeContext, binary: Option<&Path>) -> std::result::Result<(), String> {
    let binary =
        crate::binary::resolve(ProviderKind::ClaudeCode, binary.map(Path::to_path_buf).as_ref()).map_err(|error| error.to_string())?;
    let mut command = crate::binary::command(binary).no_window();
    command.args(["-p", "ok", "--output-format", "json"]).env_remove("NODE_OPTIONS").envs(&context.env).stdin(std::process::Stdio::null());
    if let Some(cwd) = &context.cwd {
        command.current_dir(cwd);
    }
    let output = tokio::time::timeout(Duration::from_secs(120), crate::process_tree::output(&mut command))
        .await
        .map_err(|_| "Claude did not finish refreshing the login in time.".to_string())?
        .map_err(|error| error.to_string())?;
    if output.status.success() { Ok(()) } else { Err("Claude could not refresh the login. Sign in again on this machine.".into()) }
}

/// A Codex login as the broker reads it from `auth.json`.
#[derive(Clone)]
pub struct CodexStored {
    pub access_token: String,
    pub id_token: Option<String>,
    pub account_id: String,
    pub plan_type: Option<String>,
    /// The access token's own expiry (JWT `exp`), in seconds.
    pub expires_at: Option<i64>,
}

impl std::fmt::Debug for CodexStored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexStored").field("plan_type", &self.plan_type).field("expires_at", &self.expires_at).finish_non_exhaustive()
    }
}

fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(text.trim_end_matches('=')).ok()
}

fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&base64url_decode(payload)?).ok()
}

/// Read the broker's Codex login from `<CODEX_HOME>/auth.json`. Codex 0.165 keeps it there when
/// `cli_auth_credentials_store = "file"`; with the OS keyring the broker cannot lend it.
pub fn read_codex(env: &BTreeMap<String, String>) -> std::result::Result<CodexStored, String> {
    let home = env
        .get("CODEX_HOME")
        .cloned()
        .or_else(|| std::env::var("CODEX_HOME").ok())
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env.get("HOME")
                .cloned()
                .or_else(|| std::env::var("HOME").ok())
                .or_else(|| std::env::var("USERPROFILE").ok())
                .map(|home| PathBuf::from(home).join(".codex"))
        })
        .ok_or_else(|| "Codex has no home folder on this machine.".to_string())?;
    let bytes = std::fs::read(home.join("auth.json")).map_err(|_| {
        "Codex is not signed in here, or keeps its login in the system keyring. Set cli_auth_credentials_store = \"file\" in Codex's config and sign in again.".to_string()
    })?;
    codex_stored_from_json(&serde_json::from_slice::<Value>(&bytes).map_err(|_| "Codex's auth.json could not be read.".to_string())?)
}

pub fn codex_stored_from_json(value: &Value) -> std::result::Result<CodexStored, String> {
    let tokens = value.get("tokens").ok_or("Codex is signed in with an API key, which cannot be lent.")?;
    let access_token =
        tokens.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty()).ok_or("Codex is not signed in here.")?.to_string();
    let id_token = tokens.get("id_token").and_then(Value::as_str).map(str::to_string);
    let account_id = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            id_token.as_deref().and_then(jwt_claims).and_then(|claims| {
                claims.pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id").and_then(Value::as_str).map(str::to_string)
            })
        })
        .ok_or("Codex's login has no account id.")?;
    let plan_type = id_token
        .as_deref()
        .and_then(jwt_claims)
        .and_then(|claims| claims.pointer("/https:~1~1api.openai.com~1auth/chatgpt_plan_type").and_then(Value::as_str).map(str::to_string));
    let expires_at = jwt_claims(&access_token).and_then(|claims| claims.get("exp").and_then(Value::as_i64));
    Ok(CodexStored { access_token, id_token, account_id, plan_type, expires_at })
}

/// A plan name for a client: "pro" reads "Pro".
pub fn codex_plan_label(plan_type: Option<&str>) -> Option<String> {
    let plan = plan_type.filter(|plan| !plan.is_empty())?;
    Some(plan[..1].to_uppercase() + &plan[1..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jwt(claims: Value) -> String {
        use base64::Engine;
        let part = |value: &Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap());
        format!("{}.{}.sig", part(&json!({"alg":"none"})), part(&claims))
    }

    #[test]
    fn claude_logins_decode_from_json_or_hex_without_a_debug_leak() {
        let json = br#"{"claudeAiOauth":{"accessToken":"sk-ant-oat-fake","expiresAt":1790000000000,"scopes":["user:inference","user:profile"],"subscriptionType":"max","rateLimitTier":"default_claude_max_20x"}}"#;
        let hex: String = json.iter().map(|byte| format!("{byte:02x}")).collect();
        for bytes in [json.to_vec(), hex.into_bytes()] {
            let login = claude_stored_from_json(&decode_json_or_hex(&bytes).unwrap()).unwrap();
            assert_eq!(login.access_token, "sk-ant-oat-fake");
            assert_eq!(login.expires_at_ms, Some(1_790_000_000_000));
            assert_eq!(login.plan.as_deref(), Some("Max (20x)"));
            assert!(login.has_profile_scope);
            assert!(!format!("{login:?}").contains("sk-ant"), "Debug never shows the token");
        }
        assert!(claude_stored_from_json(&json!({"claudeAiOauth": {"accessToken": ""}})).is_none());
    }

    #[test]
    fn keychain_services_match_what_claude_code_derives() {
        assert_eq!(claude_keychain_service(None), "Claude Code-credentials");
        // sha256("/x") starts 0x2d... ; only the shape is fixed here, the digest is Claude's own.
        let service = claude_keychain_service(Some("/some/dir"));
        assert!(service.starts_with("Claude Code-credentials-") && service.len() == "Claude Code-credentials-".len() + 8);
        assert_eq!(service, claude_keychain_service(Some("/some/dir")));
        assert_ne!(service, claude_keychain_service(Some("/some/other")));
    }

    #[test]
    fn codex_logins_come_from_auth_json_and_the_jwts() {
        let id_token = jwt(json!({"https://api.openai.com/auth": {"chatgpt_plan_type": "pro", "chatgpt_account_id": "acct-1"}}));
        let access = jwt(json!({"exp": 1_791_000_000i64}));
        let stored = codex_stored_from_json(
            &json!({"tokens": {"id_token": id_token, "access_token": access, "refresh_token": "r", "account_id": "acct-9"}}),
        )
        .unwrap();
        assert_eq!(
            (stored.account_id.as_str(), stored.plan_type.as_deref(), stored.expires_at),
            ("acct-9", Some("pro"), Some(1_791_000_000))
        );
        assert_eq!(codex_plan_label(stored.plan_type.as_deref()).as_deref(), Some("Pro"));
        assert!(!format!("{stored:?}").contains("sig"), "Debug never shows a token");
        // The account id falls back to the id token's claim.
        let from_claim = codex_stored_from_json(&json!({"tokens": {"id_token": id_token, "access_token": access}})).unwrap();
        assert_eq!(from_claim.account_id, "acct-1");
        assert!(codex_stored_from_json(&json!({"OPENAI_API_KEY": "sk-x"})).unwrap_err().contains("API key"));
    }

    #[test]
    fn login_failures_are_recognized_per_harness() {
        assert!(is_login_failure(ProviderKind::ClaudeCode, "Failed to authenticate: OAuth token revoked. Please log in again"));
        assert!(is_login_failure(ProviderKind::ClaudeCode, "API error: authentication_failed"));
        assert!(!is_login_failure(ProviderKind::ClaudeCode, "claude: error_max_turns"));
        assert!(is_login_failure(ProviderKind::Codex, "workspace routing discovery unauthorized (401)"));
        assert!(!is_login_failure(ProviderKind::Pi, "OAuth token revoked"));
    }

    #[test]
    fn borrowed_logins_map_to_each_harness_and_never_cross() {
        let claude = BorrowedLogin::Claude { access_token: "t".into(), expires_at_ms: None };
        assert_eq!(claude_env(&claude).unwrap(), vec![("CLAUDE_CODE_OAUTH_TOKEN", "t".to_string())]);
        assert!(omp_env(&claude).is_none() && codex_external_auth(&claude).is_none());
        let codex =
            BorrowedLogin::Codex { access_token: "a".into(), account_id: "acct".into(), plan_type: Some("pro".into()), id_token: None };
        assert_eq!(codex_external_auth(&codex).unwrap()["chatgptAccountId"], "acct");
        assert!(claude_env(&codex).is_none());
        let omp = BorrowedLogin::Omp { url: "http://100.64.0.1:7878".into(), token: "b".into() };
        assert_eq!(omp_env(&omp).unwrap().len(), 2);
        assert!(!format!("{omp:?}").contains("7878"));
    }

    struct Fixed;
    #[async_trait]
    impl LoginSource for Fixed {
        async fn current(&self) -> std::result::Result<BorrowedLogin, String> {
            Ok(BorrowedLogin::Claude { access_token: "x".into(), expires_at_ms: None })
        }
        async fn renew(&self, _: RenewReason) -> std::result::Result<BorrowedLogin, String> {
            Err("offline".into())
        }
    }

    #[tokio::test]
    async fn the_registry_hands_a_session_its_source_by_handle() {
        let handle = register(Arc::new(Fixed));
        let env: HashMap<String, String> = [(BORROW_ENV.to_string(), handle.clone())].into();
        assert!(matches!(source_of(&env).unwrap().current().await, Ok(BorrowedLogin::Claude { .. })));
        unregister(&handle);
        assert!(source_of(&env).is_none());
        assert!(source_of(&HashMap::new()).is_none());
    }
}
