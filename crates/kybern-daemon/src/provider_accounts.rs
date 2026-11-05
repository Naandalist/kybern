//! Native account isolation. Default never changes the regular CLI's credentials.
use crate::state::AppState;
use anyhow::{Result, anyhow, ensure};
use kybern_protocol::process::NoWindow;
use kybern_protocol::{methods::*, *};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Account color keys, in the order they are handed out. The protocol stores
/// the key so clients can adapt the color to the theme.
pub const ACCOUNT_COLORS: [&str; 6] = ["blue", "green", "purple", "pink", "teal", "amber"];

pub fn validate_color(color: &str) -> Result<()> {
    ensure!(ACCOUNT_COLORS.contains(&color), "Choose one of the account colors: {}.", ACCOUNT_COLORS.join(", "));
    Ok(())
}

/// Antigravity signs in once per machine; there is nothing to isolate a second login in.
pub const ANTIGRAVITY_ONE_ACCOUNT: &str =
    "Antigravity has one account per machine. Sign in from Settings → Agents → Antigravity, or use a Gemini API key.";

/// The first color no other account of this agent uses, cycling once all are taken.
pub fn next_color(provider: &ProviderSettings) -> String {
    let used: Vec<&str> = provider.accounts.values().filter_map(|account| account.color.as_deref()).collect();
    ACCOUNT_COLORS
        .iter()
        .find(|color| !used.contains(color))
        .copied()
        .unwrap_or(ACCOUNT_COLORS[provider.accounts.len() % ACCOUNT_COLORS.len()])
        .to_string()
}

pub fn resolve(settings: &ProviderSettings, project: Option<&str>, explicit: Option<&str>) -> String {
    explicit
        .or_else(|| project.and_then(|path| settings.project_accounts.get(path).map(String::as_str)))
        .or(settings.default_account.as_deref())
        .unwrap_or("default")
        .to_string()
}

/// The environment a provider process gets. Credential values saved in the secret store
/// (`kybern-secret:` references, design §9.3) are replaced by their values here, so this is the
/// result to spawn with, never to save or send to a client.
pub fn environment(settings: &ProviderSettings, kind: ProviderKind, instance: &str) -> Result<BTreeMap<String, String>> {
    let mut env = build_environment(settings, kind, instance)?;
    crate::secret_store::resolve_env(&mut env);
    Ok(env)
}

/// Environment variables that carry a provider's API credentials. A named native account
/// suppresses them, and the secret store keeps their values out of `settings.json`.
pub fn credential_keys(kind: ProviderKind) -> &'static [&'static str] {
    match kind {
        ProviderKind::ClaudeCode => &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN"],
        ProviderKind::Codex => &["OPENAI_API_KEY", "CODEX_API_KEY"],
        ProviderKind::Cursor => &["CURSOR_API_KEY"],
        ProviderKind::Antigravity => &["GEMINI_API_KEY"],
        ProviderKind::GrokBuild => &["XAI_API_KEY"],
        ProviderKind::Pi | ProviderKind::Omp | ProviderKind::Opencode => &[
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "GROQ_API_KEY",
            "OPENROUTER_API_KEY",
            "MISTRAL_API_KEY",
            "XAI_API_KEY",
            "AI_GATEWAY_API_KEY",
            "CEREBRAS_API_KEY",
            "TOGETHER_API_KEY",
            "DEEPSEEK_API_KEY",
        ],
    }
}

fn build_environment(settings: &ProviderSettings, kind: ProviderKind, instance: &str) -> Result<BTreeMap<String, String>> {
    let mut env = settings.env.clone();
    if instance == "default" {
        return Ok(env);
    }
    let account = settings
        .accounts
        .get(instance)
        .ok_or_else(|| anyhow!("Account '{instance}' is unavailable. Choose another account in Settings."))?;
    let dir = PathBuf::from(&account.directory);
    ensure!(dir.is_absolute(), "Account directory must be absolute.");
    let path = |child: &str| dir.join(child).to_string_lossy().to_string();
    // A named native account must not be silently replaced by the regular
    // process/global provider API key. Empty overrides also suppress inherited
    // keys for native probes and PTY sign-in without altering the user's shell.
    let credentials = credential_keys(kind);
    for key in credentials {
        env.insert((*key).into(), String::new());
    }
    match kind {
        ProviderKind::ClaudeCode => {
            env.insert("CLAUDE_CONFIG_DIR".into(), account.directory.clone());
        }
        ProviderKind::Codex => {
            env.insert("CODEX_HOME".into(), account.directory.clone());
        }
        ProviderKind::Cursor => {
            env.insert("KYBERN_CURSOR_AUTH_FILE".into(), path("auth.json"));
            env.insert("KYBERN_CURSOR_STATE_DIR".into(), path("sessions"));
        }
        ProviderKind::Pi => {
            env.insert("PI_CODING_AGENT_DIR".into(), account.directory.clone());
        }
        ProviderKind::Omp => {
            env.insert("PI_CODING_AGENT_DIR".into(), account.directory.clone());
            // Native named profiles ignore PI_CODING_AGENT_DIR. Select the
            // default profile inside this account root to keep credentials isolated.
            env.insert("OMP_PROFILE".into(), String::new());
        }
        ProviderKind::Opencode => {
            env.insert("XDG_DATA_HOME".into(), path("data"));
            env.insert("XDG_CONFIG_HOME".into(), path("config"));
            env.insert("XDG_STATE_HOME".into(), path("state"));
            env.insert("XDG_CACHE_HOME".into(), path("cache"));
        }
        // `agy` has no supported config-root override, so a second account cannot be isolated.
        ProviderKind::Antigravity => return Err(anyhow!(ANTIGRAVITY_ONE_ACCOUNT)),
        // Grok Build uses XAI_API_KEY or grok login; named accounts not supported yet.
        ProviderKind::GrokBuild => return Err(anyhow!("Grok Build does not support named accounts yet")),
    }
    Ok(env)
}

pub fn validate(settings: &Settings) -> Result<()> {
    for (kind, provider) in &settings.providers {
        ensure!(*kind != ProviderKind::Antigravity || provider.accounts.is_empty(), "{ANTIGRAVITY_ONE_ACCOUNT}");
        for (id, account) in &provider.accounts {
            ensure!(
                id != "default" && !id.is_empty() && id.len() <= 80 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
                "Invalid account identity."
            );
            ensure!(!account.name.trim().is_empty() && account.name.len() <= 120, "Enter an account name up to 120 characters.");
            ensure!(PathBuf::from(&account.directory).is_absolute(), "{} account directory must be absolute.", kind.display_name());
            if let Some(color) = &account.color {
                validate_color(color)?;
            }
        }
        for id in provider.default_account.iter().chain(provider.project_accounts.values()) {
            build_environment(provider, *kind, id)?;
        }
    }
    Ok(())
}

pub fn create(state: &AppState, params: AccountsCreateParams) -> Result<ProviderInstance> {
    ensure!(params.kind != ProviderKind::Antigravity, "{ANTIGRAVITY_ONE_ACCOUNT}");
    ensure!(!params.name.trim().is_empty() && params.name.len() <= 120, "Enter an account name up to 120 characters.");
    let id = uuid::Uuid::now_v7().to_string();
    let directory = match params.directory {
        Some(directory) => {
            let path = PathBuf::from(directory);
            ensure!(path.is_absolute() && path.is_dir(), "Choose an existing absolute account directory.");
            path.canonicalize()?
        }
        None => state.settings.dir().join("accounts").join(params.kind.as_str()).join(&id),
    };
    let mut settings = state.settings.get();
    let provider = settings.providers.entry(params.kind).or_default();
    ensure!(
        !provider.accounts.values().any(|account| std::path::Path::new(&account.directory) == directory),
        "This directory is already registered. Choose its existing account."
    );
    let created = !directory.exists();
    if created {
        std::fs::create_dir_all(&directory)?;
    }
    #[cfg(unix)]
    if created {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let color = next_color(provider);
    provider.accounts.insert(
        id.clone(),
        ProviderAccount {
            name: params.name.trim().into(),
            directory: directory.to_string_lossy().into_owned(),
            color: Some(color),
            ..Default::default()
        },
    );
    crate::provider_assets::prepare(provider, params.kind)?;
    if let Err(error) = state.settings.set(settings) {
        if created {
            let _ = std::fs::remove_dir(&directory);
        }
        return Err(error);
    }
    Ok(ProviderInstance { kind: params.kind, instance: id })
}

pub fn context(state: &AppState, instance: &ProviderInstance) -> Result<kybern_drivers::ProbeContext> {
    let settings = state.settings.get();
    let provider = settings.providers.get(&instance.kind).cloned().unwrap_or_default();
    crate::provider_assets::prepare(&provider, instance.kind)?;
    Ok(kybern_drivers::ProbeContext {
        binary: provider.binary.clone().map(Into::into),
        cwd: Some(state.settings.dir().into()),
        env: environment(&provider, instance.kind, &instance.instance)?,
    })
}

pub fn sign_in(state: &AppState, instance: ProviderInstance) -> Result<TerminalInfo> {
    ensure!(instance.instance != "default", "The default uses your regular CLI login. Create a named account to sign in separately.");
    let context = context(state, &instance)?;
    let mut terminal_env = context.env.clone();
    let command = if instance.kind == ProviderKind::Cursor {
        let command = kybern_drivers::cursor::login_command(&context)?;
        let raw = command.as_std();
        for (key, value) in raw.get_envs() {
            if let Some(value) = value {
                terminal_env.insert(key.to_string_lossy().into_owned(), value.to_string_lossy().into_owned());
            }
        }
        std::iter::once(raw.get_program()).chain(raw.get_args()).map(|arg| arg.to_string_lossy().into_owned()).collect()
    } else {
        let binary = kybern_drivers::binary::resolve(instance.kind, context.binary.as_ref())?;
        let args: &[&str] = match instance.kind {
            ProviderKind::ClaudeCode => &["auth", "login"],
            ProviderKind::Codex => &["login"],
            ProviderKind::Opencode => &["auth", "login"],
            // Interactive native /login selects the upstream provider.
            ProviderKind::Pi => &[],
            ProviderKind::Omp => &["auth-broker", "login"],
            ProviderKind::Cursor => unreachable!(),
            ProviderKind::Antigravity => return Err(anyhow!(ANTIGRAVITY_ONE_ACCOUNT)),
            ProviderKind::GrokBuild => return Err(anyhow!("Grok Build does not support named accounts yet")),
        };
        std::iter::once(binary.to_string_lossy().into_owned()).chain(args.iter().map(|s| s.to_string())).collect()
    };
    let terminal = state.terminals.create_with_env(
        None,
        None,
        state.settings.dir().to_string_lossy().into_owned(),
        100,
        30,
        Some(command),
        &terminal_env,
    )?;
    if instance.kind == ProviderKind::Pi {
        terminal.write(b"/login\r")?;
    }
    Ok(terminal.info())
}

/// Whether Kybern created this folder, so removing the account may delete it.
fn is_managed(data_dir: &Path, directory: &str) -> bool {
    let root = data_dir.join("accounts");
    let root = root.canonicalize().unwrap_or(root);
    let directory = PathBuf::from(directory);
    let directory = directory.canonicalize().unwrap_or(directory);
    // A path that could not be resolved keeps its `..` parts, and `starts_with` compares
    // components, so `accounts/../elsewhere` would otherwise count as inside the root.
    if directory.components().any(|part| matches!(part, std::path::Component::ParentDir)) {
        return false;
    }
    directory.starts_with(&root) && directory != root
}

const CLI_ACCOUNT: &str = "CLI account";

fn summarize(
    state: &AppState,
    settings: &Settings,
    kind: ProviderKind,
    instance: &str,
    probed: (AccountStatus, Option<AccountIdentity>),
) -> AccountSummary {
    let provider = settings.providers.get(&kind).cloned().unwrap_or_default();
    let account = provider.accounts.get(instance);
    let (status, mut identity) = probed;
    // A signed-out or unreadable account still shows the identity it last had.
    if identity.is_none()
        && let Some(account) = account
        && (account.email.is_some() || account.plan.is_some())
    {
        identity = Some(AccountIdentity { email: account.email.clone(), plan: account.plan.clone(), organization: None });
    }
    let named = account.is_some();
    AccountSummary {
        provider: ProviderInstance { kind, instance: instance.to_string() },
        name: account.map(|account| account.name.clone()).unwrap_or_else(|| CLI_ACCOUNT.to_string()),
        color: account.and_then(|account| account.color.clone()),
        identity,
        status,
        is_default: resolve(&provider, None, None) == instance,
        projects: provider.project_accounts.iter().filter(|(_, id)| id.as_str() == instance).map(|(path, _)| path.clone()).collect(),
        directory: account.map(|account| account.directory.clone()),
        managed: account.is_some_and(|account| is_managed(state.settings.dir(), &account.directory)),
        can_sign_out: named
            && matches!(status, AccountStatus::SignedIn | AccountStatus::NeedsSignIn)
            && matches!(kind, ProviderKind::ClaudeCode | ProviderKind::Codex | ProviderKind::Cursor),
        broker: None,
    }
}

/// An account that signs in through the login broker is signed in as far as this machine is concerned
/// while the broker answers; the harness' own probe (an empty folder) says nothing about it.
fn through_broker(state: &AppState, summary: &mut AccountSummary) {
    let Some(view) =
        state.orchestrator.login_broker().and_then(|broker| broker.account_view(summary.provider.kind, &summary.provider.instance))
    else {
        return;
    };
    summary.status = match view.state {
        BrokerLoginState::Borrowing | BrokerLoginState::Lending => AccountStatus::SignedIn,
        _ => AccountStatus::NeedsSignIn,
    };
    summary.can_sign_out = false;
    summary.broker = Some(view);
}

async fn summary_for(state: &AppState, kind: ProviderKind, instance: &str, refresh: bool) -> AccountSummary {
    let provider_instance = ProviderInstance { kind, instance: instance.to_string() };
    let probed = state.account_identities.probe(state, &provider_instance, refresh).await;
    let mut summary = summarize(state, &state.settings.get(), kind, instance, probed);
    through_broker(state, &mut summary);
    summary
}

/// Every account with its identity and sign-in state. Identity probes are
/// cached for five minutes; `refresh` bypasses the cache.
pub async fn list(state: &AppState, params: AccountsListParams) -> Result<AccountsListResult> {
    let settings = state.settings.get();
    let kinds: Vec<ProviderKind> = params.kind.map(|kind| vec![kind]).unwrap_or_else(|| ProviderKind::ALL.to_vec());
    let mut reads = Vec::new();
    for kind in kinds {
        let provider = settings.providers.get(&kind).cloned().unwrap_or_default();
        let instances = std::iter::once("default".to_string()).chain(provider.accounts.keys().cloned()).collect::<Vec<_>>();
        for instance in instances {
            reads.push(async move { summary_for(state, kind, &instance, params.refresh).await });
        }
    }
    Ok(AccountsListResult { accounts: futures::future::join_all(reads).await })
}

fn valid_name(name: &str) -> Result<String> {
    let name = name.trim();
    ensure!(!name.is_empty() && name.len() <= 120, "Enter a name up to 120 characters.");
    Ok(name.to_string())
}

pub async fn update(state: &AppState, params: AccountsUpdateParams) -> Result<AccountSummary> {
    ensure!(params.instance != "default", "The CLI account can't be renamed or colored.");
    let name = params.name.as_deref().map(valid_name).transpose()?;
    if let Some(color) = &params.color {
        validate_color(color)?;
    }
    let mut settings = state.settings.get();
    let account = settings
        .providers
        .get_mut(&params.kind)
        .and_then(|provider| provider.accounts.get_mut(&params.instance))
        .ok_or_else(|| anyhow!("That account no longer exists. Refresh Settings › Accounts."))?;
    if let Some(name) = name {
        account.name = name;
    }
    if let Some(color) = params.color {
        account.color = Some(color);
    }
    state.settings.set(settings)?;
    Ok(summary_for(state, params.kind, &params.instance, false).await)
}

/// Run the harness's own sign-out against the account's folder.
async fn harness_sign_out(state: &AppState, instance: &ProviderInstance) -> Result<()> {
    let mut context = context(state, instance)?;
    if instance.kind == ProviderKind::Cursor {
        context.binary = None;
        return kybern_drivers::cursor::sign_out(&context).await.map_err(|error| anyhow!(kybern_drivers::cursor::reason(error)));
    }
    let args = kybern_drivers::account_auth::sign_out_args(instance.kind, None).ok_or_else(|| {
        anyhow!("{} can't sign out from Kybern. Remove its credentials from the agent instead.", instance.kind.display_name())
    })?;
    let binary = kybern_drivers::binary::resolve(instance.kind, context.binary.as_ref()).map_err(|error| anyhow!(error.to_string()))?;
    let mut command = tokio::process::Command::new(binary).no_window();
    command.args(args).envs(&context.env).stdin(std::process::Stdio::null()).kill_on_drop(true);
    if let Some(cwd) = &context.cwd {
        command.current_dir(cwd);
    }
    let output = tokio::time::timeout(std::time::Duration::from_secs(20), command.output())
        .await
        .map_err(|_| anyhow!("{} didn't finish signing out. Try again.", instance.kind.display_name()))??;
    if !output.status.success() {
        let text = String::from_utf8_lossy(&output.stderr);
        let line = text.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("it reported no reason");
        let line = kybern_drivers::account_auth::redact(&kybern_drivers::account_auth::strip_ansi(line));
        anyhow::bail!("{} couldn't sign out: {}", instance.kind.display_name(), line.chars().take(200).collect::<String>());
    }
    Ok(())
}

fn forget_account_caches(state: &AppState, instance: &ProviderInstance) {
    state.account_identities.invalidate(instance);
    state.orchestrator.usage().forget_account(instance.kind, &instance.instance);
}

pub async fn sign_out(state: &AppState, instance: ProviderInstance) -> Result<AccountSummary> {
    ensure!(instance.instance != "default", "Kybern can't sign out the CLI account. Sign out in your terminal instead.");
    let name = state
        .settings
        .get()
        .providers
        .get(&instance.kind)
        .and_then(|provider| provider.accounts.get(&instance.instance))
        .map(|account| account.name.clone())
        .ok_or_else(|| anyhow!("That account no longer exists. Refresh Settings › Accounts."))?;
    harness_sign_out(state, &instance).await.map_err(|error| anyhow!("Couldn't sign {name} out. {error}"))?;
    state.orchestrator.mark_account_signed_out(&instance, true)?;
    forget_account_caches(&state.clone(), &instance);
    state.provider_catalogs.invalidate().await;
    let mut summary = summary_for(state, instance.kind, &instance.instance, true).await;
    // A harness that keeps a stale credential file still counts as signed out.
    summary.status = AccountStatus::SignedOut;
    summary.can_sign_out = false;
    Ok(summary)
}

pub async fn remove(state: &AppState, instance: ProviderInstance) -> Result<()> {
    ensure!(instance.instance != "default", "The CLI account can't be removed.");
    let settings = state.settings.get();
    let account = settings
        .providers
        .get(&instance.kind)
        .and_then(|provider| provider.accounts.get(&instance.instance))
        .cloned()
        .ok_or_else(|| anyhow!("That account no longer exists. Refresh Settings › Accounts."))?;
    let running = state.orchestrator.running_turns_on(&instance).await;
    ensure!(
        running == 0,
        "{} is running in {running} {}. Stop {}, then remove it.",
        account.name,
        if running == 1 { "thread" } else { "threads" },
        if running == 1 { "it" } else { "them" }
    );
    let managed = is_managed(state.settings.dir(), &account.directory);
    if managed {
        // The harness may keep credentials outside the folder (macOS Keychain).
        let _ = harness_sign_out(state, &instance).await;
    }
    let mut next = state.settings.get();
    if let Some(provider) = next.providers.get_mut(&instance.kind) {
        provider.accounts.remove(&instance.instance);
        if provider.default_account.as_deref() == Some(instance.instance.as_str()) {
            provider.default_account = None;
        }
        provider.project_accounts.retain(|_, id| id != &instance.instance);
    }
    state.settings.set(next)?;
    state.orchestrator.clear_account_overrides(&instance)?;
    if managed {
        let _ = std::fs::remove_dir_all(&account.directory);
    }
    forget_account_caches(state, &instance);
    state.provider_catalogs.invalidate().await;
    Ok(())
}

pub async fn catalog(state: &AppState, params: AccountsCatalogParams) -> Result<ProviderStatus> {
    let mut context = context(state, &params.provider)?;
    if let Some(project_id) = params.project_id {
        context.cwd = Some(state.store.project_get(project_id)?.ok_or_else(|| anyhow!("Project not found."))?.path.into());
    }
    let cache_key = serde_json::to_string(&(params.provider.clone(), &context.env, &context.cwd, &context.binary))?;
    let driver = state.drivers.get(params.provider.kind).ok_or_else(|| anyhow!("Harness is unavailable."))?;
    let mut statuses = state
        .provider_catalogs
        .get_or_refresh(cache_key, params.force_refresh, || async move {
            vec![if params.force_refresh {
                driver.probe_fresh_with_context(&context).await
            } else {
                driver.probe_with_context(&context).await
            }]
        })
        .await;
    let mut status = statuses.remove(0);
    let settings = state.settings.get();
    status.instances = std::iter::once("default".to_string())
        .chain(settings.providers.get(&params.provider.kind).into_iter().flat_map(|p| p.accounts.keys().cloned()))
        .collect();
    Ok(status)
}

pub async fn usage(state: &AppState, instance: ProviderInstance) -> Result<ProviderUsage> {
    let context = context(state, &instance)?;
    let cwd = context.cwd.as_deref().unwrap();
    let limits = match instance.kind {
        ProviderKind::ClaudeCode => {
            kybern_drivers::claude::read_account_usage(cwd, context.binary.as_ref(), &context.env).await.ok().map(|(limits, _)| limits)
        }
        ProviderKind::Codex => kybern_drivers::codex::read_account_limits(cwd, context.binary.as_ref(), &context.env).await,
        ProviderKind::Cursor => kybern_drivers::cursor::usage::read_account_usage(&context.env).await.map(|usage| usage.limits),
        ProviderKind::Antigravity => {
            kybern_drivers::antigravity::read_account_usage(cwd, context.binary.as_ref(), &context.env).await.ok().map(|(limits, _)| limits)
        }
        _ => None,
    };
    Ok(ProviderUsage { limits, ..Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn precedence_and_default_isolation() {
        let mut p = ProviderSettings { default_account: Some("work".into()), ..Default::default() };
        p.project_accounts.insert("/project".into(), "project".into());
        assert_eq!(resolve(&p, Some("/project"), Some("thread")), "thread");
        assert_eq!(resolve(&p, Some("/project"), None), "project");
        assert_eq!(resolve(&p, None, None), "work");
        assert_eq!(environment(&p, ProviderKind::ClaudeCode, "default").unwrap(), p.env);
        let isolated = std::env::temp_dir().join("isolated");
        p.accounts.insert(
            "work".into(),
            ProviderAccount { name: "Work".into(), directory: isolated.to_string_lossy().into_owned(), ..Default::default() },
        );
        let claude = environment(&p, ProviderKind::ClaudeCode, "work").unwrap();
        assert_eq!(Path::new(&claude["CLAUDE_CONFIG_DIR"]), isolated);
        assert!(!claude.contains_key("HOME"));
        let cursor = environment(&p, ProviderKind::Cursor, "work").unwrap();
        assert_eq!(Path::new(&cursor["KYBERN_CURSOR_AUTH_FILE"]), isolated.join("auth.json"));
        assert_eq!(Path::new(&cursor["KYBERN_CURSOR_STATE_DIR"]), isolated.join("sessions"));
        assert!(environment(&p, ProviderKind::Codex, "missing").is_err());
    }
}

#[cfg(test)]
mod native_environment_tests {
    use super::*;
    #[test]
    fn every_harness_uses_its_native_account_root_without_replacing_regular_home() {
        let mut provider = ProviderSettings::default();
        provider.env.insert("HOME".into(), "/regular-home-sentinel".into());
        provider.env.insert("OMP_PROFILE".into(), "regular-profile".into());
        provider.accounts.insert(
            "work".into(),
            ProviderAccount {
                name: "Work".into(),
                directory: std::env::temp_dir().join("scratch-work-account").to_string_lossy().into_owned(),
                ..Default::default()
            },
        );
        for (kind, expected) in [
            (ProviderKind::ClaudeCode, "CLAUDE_CONFIG_DIR=/scratch-work-account"),
            (ProviderKind::Codex, "CODEX_HOME=/scratch-work-account"),
            (ProviderKind::Cursor, "KYBERN_CURSOR_STATE_DIR=/scratch-work-account/sessions"),
            (ProviderKind::Pi, "PI_CODING_AGENT_DIR=/scratch-work-account"),
            (ProviderKind::Omp, "PI_CODING_AGENT_DIR=/scratch-work-account"),
            (ProviderKind::Opencode, "XDG_DATA_HOME=/scratch-work-account/data"),
        ] {
            let named = environment(&provider, kind, "work").unwrap();
            let output = crate::test_support::environment_command().env_clear().envs(&named).output().unwrap();
            let captured = String::from_utf8(output.stdout).unwrap();
            let (key, suffix) = expected.split_once("=/scratch-work-account").unwrap();
            let wanted = std::env::temp_dir().join("scratch-work-account").join(suffix.trim_start_matches('/'));
            let actual = captured.lines().find_map(|line| line.strip_prefix(&format!("{key}="))).unwrap();
            assert_eq!(Path::new(actual), wanted, "{kind}");
            assert!(captured.contains("HOME=/regular-home-sentinel"));
            assert_eq!(environment(&provider, kind, "default").unwrap(), provider.env);
            if kind == ProviderKind::Omp {
                assert_eq!(named["OMP_PROFILE"], "");
            }
        }
    }

    #[test]
    fn named_native_process_does_not_inherit_regular_cli_api_credentials() {
        let mut provider = ProviderSettings::default();
        provider.env.insert("CURSOR_API_KEY".into(), "regular-cli-sentinel".into());
        provider.accounts.insert(
            "work".into(),
            ProviderAccount {
                name: "Work".into(),
                directory: std::env::temp_dir().join("scratch-work-account").to_string_lossy().into_owned(),
                ..Default::default()
            },
        );
        let named = environment(&provider, ProviderKind::Cursor, "work").unwrap();
        let output = crate::test_support::environment_command()
            .env_clear()
            .env("CURSOR_API_KEY", "inherited-cli-sentinel")
            .envs(&named)
            .output()
            .unwrap();
        let captured = String::from_utf8(output.stdout).unwrap();
        assert!(!captured.contains("regular-cli-sentinel"));
        assert!(!captured.contains("inherited-cli-sentinel"));
        let actual = captured.lines().find_map(|line| line.strip_prefix("KYBERN_CURSOR_AUTH_FILE=")).unwrap();
        assert_eq!(Path::new(actual), std::env::temp_dir().join("scratch-work-account/auth.json"));
        assert_eq!(environment(&provider, ProviderKind::Cursor, "default").unwrap()["CURSOR_API_KEY"], "regular-cli-sentinel");
    }
}

#[cfg(test)]
mod management_tests {
    use super::*;
    const FAKE_CLAUDE: &str = r#"#!/bin/sh
case "$1 $2" in
"auth status")
  if [ -e "$CLAUDE_CONFIG_DIR/.signed-in" ]; then
    printf '{"loggedIn":true,"authMethod":"claude.ai","email":"%s","subscriptionType":"max","orgName":"Org"}\n' "$(cat "$CLAUDE_CONFIG_DIR/.signed-in")"
    exit 0
  fi
  echo '{"loggedIn":false}'
  exit 1 ;;
"auth logout") rm -f "$CLAUDE_CONFIG_DIR/.signed-in"; exit 0 ;;
esac
exit 2
"#;

    struct Fixture {
        state: AppState,
        root: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn fixture() -> Fixture {
        let root = std::env::temp_dir().join(format!("kybern-accounts-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let binary = crate::test_support::powershell_script(
            &root.join("bin"),
            "claude",
            FAKE_CLAUDE,
            r#"
$dir = $env:CLAUDE_CONFIG_DIR
if ($args[0] -eq 'auth' -and $args[1] -eq 'status') {
    if (Test-Path "$dir/.signed-in") {
        @{ loggedIn = $true; authMethod = 'claude.ai'; email = ([IO.File]::ReadAllText([IO.Path]::Combine($dir, '.signed-in'))); subscriptionType = 'max'; orgName = 'Org' } | ConvertTo-Json -Compress
        exit 0
    }
    '{"loggedIn":false}'
    exit 1
}
if ($args[0] -eq 'auth' -and $args[1] -eq 'logout') {
    Remove-Item -Force -ErrorAction SilentlyContinue "$dir/.signed-in"
    exit 0
}
exit 2
"#,
        );
        let paths = crate::config::Paths::resolve(Some(root.join("data"))).unwrap();
        let state = AppState::initialize(&paths).unwrap();
        let mut settings = state.settings.get();
        settings.providers.entry(ProviderKind::ClaudeCode).or_default().binary = Some(binary.to_string_lossy().into_owned());
        state.settings.set(settings).unwrap();
        Fixture { state, root }
    }

    fn add(fixture: &Fixture, id: &str, directory: &Path, color: Option<&str>) {
        std::fs::create_dir_all(directory).unwrap();
        let mut settings = fixture.state.settings.get();
        settings.providers.entry(ProviderKind::ClaudeCode).or_default().accounts.insert(
            id.into(),
            ProviderAccount {
                name: id.into(),
                directory: directory.to_string_lossy().into_owned(),
                color: color.map(str::to_string),
                ..Default::default()
            },
        );
        fixture.state.settings.set(settings).unwrap();
    }

    fn claude(instance: &str) -> ProviderInstance {
        ProviderInstance { kind: ProviderKind::ClaudeCode, instance: instance.into() }
    }

    #[test]
    fn colors_are_validated_and_assigned_in_palette_order_then_cycle() {
        assert!(validate_color("blue").is_ok());
        assert!(validate_color("red").is_err(), "red carries error semantics");
        assert!(validate_color("#0169cc").is_err(), "the protocol stores keys, not hex values");
        let mut provider = ProviderSettings::default();
        assert_eq!(next_color(&provider), "blue");
        for (index, color) in ACCOUNT_COLORS.iter().enumerate() {
            provider.accounts.insert(format!("a{index}"), ProviderAccount { color: Some((*color).into()), ..Default::default() });
        }
        assert_eq!(next_color(&provider), ACCOUNT_COLORS[0], "cycles once all six are taken");
        provider.accounts.remove("a1");
        assert_eq!(next_color(&provider), "green");
        let mut settings = Settings::default();
        settings.providers.entry(ProviderKind::Codex).or_default().accounts.insert(
            "x".into(),
            ProviderAccount { name: "X".into(), directory: "/x".into(), color: Some("red".into()), ..Default::default() },
        );
        assert!(validate(&settings).is_err());
    }

    #[tokio::test]
    async fn list_puts_the_cli_account_first_with_identity_and_status() {
        let fixture = fixture();
        let work = fixture.root.join("data/accounts/claude/work");
        add(&fixture, "work", &work, Some("green"));
        std::fs::write(work.join(".signed-in"), "dev@arunika.co").unwrap();
        let list = list(&fixture.state, AccountsListParams { kind: Some(ProviderKind::ClaudeCode), refresh: false }).await.unwrap();
        assert_eq!(list.accounts.len(), 2);
        let cli = &list.accounts[0];
        assert_eq!(
            (cli.name.as_str(), cli.provider.instance.as_str(), cli.is_default, cli.can_sign_out),
            ("CLI account", "default", true, false)
        );
        let named = &list.accounts[1];
        assert_eq!(named.status, AccountStatus::SignedIn);
        assert_eq!(named.identity.as_ref().unwrap().plan.as_deref(), Some("Max"));
        assert_eq!(named.color.as_deref(), Some("green"));
        assert!(named.managed && named.can_sign_out && !named.is_default);
        // The probe wrote the identity back for instant rendering.
        let stored = &fixture.state.settings.get().providers[&ProviderKind::ClaudeCode].accounts["work"];
        assert_eq!(stored.email.as_deref(), Some("dev@arunika.co"));
    }

    #[tokio::test]
    async fn update_validates_and_the_cli_account_is_fixed() {
        let fixture = fixture();
        add(&fixture, "work", &fixture.root.join("work"), Some("blue"));
        let renamed = update(
            &fixture.state,
            AccountsUpdateParams {
                kind: ProviderKind::ClaudeCode,
                instance: "work".into(),
                name: Some("  Client  ".into()),
                color: Some("pink".into()),
            },
        )
        .await
        .unwrap();
        assert_eq!((renamed.name.as_str(), renamed.color.as_deref()), ("Client", Some("pink")));
        for (name, color) in [(Some(String::new()), None), (Some("x".repeat(121)), None), (None, Some("red".to_string()))] {
            let result =
                update(&fixture.state, AccountsUpdateParams { kind: ProviderKind::ClaudeCode, instance: "work".into(), name, color }).await;
            assert!(result.is_err());
        }
        let default = update(
            &fixture.state,
            AccountsUpdateParams { kind: ProviderKind::ClaudeCode, instance: "default".into(), name: Some("x".into()), color: None },
        )
        .await;
        assert!(default.is_err());
        assert!(sign_out(&fixture.state, claude("default")).await.is_err());
        assert!(remove(&fixture.state, claude("default")).await.is_err());
    }

    #[tokio::test]
    async fn sign_out_signs_the_folder_out_and_marks_the_account() {
        let fixture = fixture();
        let work = fixture.root.join("data/accounts/claude/work");
        add(&fixture, "work", &work, None);
        std::fs::write(work.join(".signed-in"), "dev@arunika.co").unwrap();
        let summary = sign_out(&fixture.state, claude("work")).await.unwrap();
        assert_eq!(summary.status, AccountStatus::SignedOut);
        assert!(!work.join(".signed-in").exists());
        assert!(fixture.state.orchestrator.admit_signed_out_for_test(&claude("work")));
    }

    #[tokio::test]
    async fn remove_deletes_only_managed_folders_and_clears_references() {
        let fixture = fixture();
        let managed = fixture.root.join("data/accounts/claude/managed");
        let outside = fixture.root.join("existing-folder");
        add(&fixture, "managed", &managed, None);
        add(&fixture, "outside", &outside, None);
        std::fs::write(outside.join("keep"), "x").unwrap();
        let mut settings = fixture.state.settings.get();
        let provider = settings.providers.get_mut(&ProviderKind::ClaudeCode).unwrap();
        provider.default_account = Some("managed".into());
        provider.project_accounts.insert("/projects/a".into(), "managed".into());
        provider.project_accounts.insert("/projects/b".into(), "outside".into());
        fixture.state.settings.set(settings).unwrap();

        remove(&fixture.state, claude("managed")).await.unwrap();
        assert!(!managed.exists(), "a Kybern-created folder is deleted");
        let provider = fixture.state.settings.get().providers[&ProviderKind::ClaudeCode].clone();
        assert!(provider.default_account.is_none());
        assert_eq!(provider.project_accounts.get("/projects/a"), None);
        assert_eq!(provider.project_accounts.get("/projects/b").map(String::as_str), Some("outside"));

        remove(&fixture.state, claude("outside")).await.unwrap();
        assert!(outside.join("keep").exists(), "an existing folder stays");
        assert!(fixture.state.settings.get().providers[&ProviderKind::ClaudeCode].accounts.is_empty());
        assert!(remove(&fixture.state, claude("outside")).await.is_err());
    }

    #[test]
    fn managed_folders_are_only_those_under_the_accounts_directory() {
        let root = std::env::temp_dir().join(format!("kybern-managed-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(root.join("accounts/claude/a")).unwrap();
        // Resolve symlinks (macOS temp dirs) so unresolvable paths compare like they do on Linux.
        let root = root.canonicalize().unwrap();
        assert!(is_managed(&root, &root.join("accounts/claude/a").to_string_lossy()));
        assert!(!is_managed(&root, &root.join("accounts").to_string_lossy()));
        assert!(!is_managed(&root, &root.join("accounts/../elsewhere").to_string_lossy()));
        assert!(!is_managed(&root, "/Users/someone/.claude-work"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
