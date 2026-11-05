use super::*;
use kybern_protocol::process::NoWindow;
use serde_json::{Value, json};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;

pub(super) fn validate(p: &MachinePlugin) -> Result<(), String> {
    if p.fingerprint != fingerprint(p) || !safe_id(&p.id) || p.problem.is_some() || p.enabled.is_none() {
        return Err(p
            .problem
            .clone()
            .unwrap_or_else(|| "The plugin declaration changed or cannot be copied. Refresh and review it again.".into()));
    }
    if serde_json::to_vec(p).map_err(|e| e.to_string())?.len() > 64 * 1024 {
        return Err("Plugin declaration is too large".into());
    }
    let source = p.source.as_deref().ok_or("No installable source")?;
    let valid = if p.kind == PluginKind::Marketplace || p.marketplace.is_some() {
        // Only these harnesses have marketplace installers; others would be started with unknown arguments.
        matches!(p.provider, ProviderKind::ClaudeCode | ProviderKind::Codex | ProviderKind::Omp) && portable_source(source)
    } else {
        // A package row installs exactly the package it names. Pi reads an unprefixed source as a local path.
        let named = |spec: &str| package_name(spec) == p.id;
        match p.provider {
            ProviderKind::Pi => (source.starts_with("npm:") && npm_spec(source) || pi_git_source(source)) && named(source),
            ProviderKind::Omp | ProviderKind::Opencode => npm_spec(source) && named(source),
            _ => false,
        }
    };
    if !valid || p.provider == ProviderKind::Cursor {
        return Err("This source has no supported portable installer.".into());
    }
    if let Some(m) = &p.marketplace
        && (!safe_id(m) || m.contains(['@', '/', ':']) || (p.kind == PluginKind::Plugin && !p.id.ends_with(&format!("@{m}"))))
    {
        return Err("Invalid marketplace selector".into());
    }
    if p.resources.keys().any(|k| !["extensions", "skills", "prompts", "themes"].contains(&k.as_str())) {
        return Err("Unsupported resource settings".into());
    }
    Ok(())
}

async fn run(state: &AppState, ctx: &Context, provider: ProviderKind, args: Vec<String>) -> Result<(), String> {
    let fake = state.plugin_runtime.binaries.read().unwrap_or_else(|e| e.into_inner()).get(&provider).cloned();
    let binary = if cfg!(test) {
        fake.ok_or("Tests must supply a fake harness executable")?
    } else {
        fake.or_else(|| state.settings.get().providers.get(&provider).and_then(|p| p.binary.as_ref()).map(PathBuf::from)).unwrap_or_else(
            || {
                PathBuf::from(match provider {
                    ProviderKind::ClaudeCode => "claude",
                    ProviderKind::Codex => "codex",
                    ProviderKind::Omp => "omp",
                    ProviderKind::Pi => "pi",
                    ProviderKind::Opencode => "opencode",
                    ProviderKind::Cursor => "agent",
                    ProviderKind::Antigravity => "agy",
                    ProviderKind::GrokBuild => "grok",
                })
            },
        )
    };
    let mut command = tokio::process::Command::new(binary).no_window();
    command.args(&args).current_dir(&ctx.home).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    // Never inherit the test runner's HOME, credentials, or executable search path.
    if cfg!(test) {
        command
            .env_clear()
            .envs(&ctx.env)
            .env("HOME", &ctx.home)
            .env("USERPROFILE", &ctx.home)
            .env("CODEX_HOME", ctx.root(ProviderKind::Codex))
            .env("CLAUDE_CONFIG_DIR", ctx.root(ProviderKind::ClaudeCode))
            .env("XDG_CONFIG_HOME", ctx.home.join(".config"))
            .env("PI_CODING_AGENT_DIR", ctx.root(ProviderKind::Pi))
            .env("APPDATA", ctx.home.join("AppData/Roaming"));
    }
    let mut child =
        command.spawn().map_err(|e| format!("Could not start {}: {e}. Install the harness on this machine.", provider.as_str()))?;
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    // Drain, but only retain a bounded diagnostic; large installer output cannot fill memory or deadlock.
    async fn drain(mut stream: impl tokio::io::AsyncRead + Unpin) -> Vec<u8> {
        let mut kept = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = stream.read(&mut buf).await {
            if n == 0 {
                break;
            }
            let take = n.min(8192usize.saturating_sub(kept.len()));
            kept.extend_from_slice(&buf[..take]);
        }
        kept
    }
    let result = tokio::time::timeout(Duration::from_secs(180), async { tokio::join!(child.wait(), drain(out), drain(err)) }).await;
    match result {
        Ok((Ok(status), _, _)) if status.success() => Ok(()),
        Ok((status, out, err)) => Err(format!(
            "{} installer failed ({status:?}): {}",
            provider.as_str(),
            String::from_utf8_lossy(if err.is_empty() { &out } else { &err })
                .chars()
                .filter(|c| !c.is_control() || *c == '\n')
                .take(2000)
                .collect::<String>()
        )),
        Err(_) => {
            let _ = child.kill().await;
            Err("Installer timed out after 3 minutes. Check the harness on the target and retry.".into())
        }
    }
}
fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

fn write(path: &Path, before: &str, after: &str) -> Result<(), String> {
    // Renaming over a linked config would replace the user's link with a file. Linked folders
    // (macOS /var, a dotfiles-managed ~/.config) are fine: the rename stays in that folder.
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("Config file is a symlink; edit it on this machine instead.".into());
    }
    if text(path)? != before {
        return Err("Config changed while preparing the copy. Refresh and try again.".into());
    }
    let parent = path.parent().ok_or("Config has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let tmp = parent.join(format!(".kybern-plugin-{}.tmp", uuid::Uuid::now_v7()));
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&tmp)?;
        file.write_all(after.as_bytes())?;
        file.sync_all()?;
        if let Ok(meta) = std::fs::metadata(path) {
            std::fs::set_permissions(&tmp, meta.permissions())?;
        }
        std::fs::rename(&tmp, path)
    })()
    .map_err(|e: std::io::Error| e.to_string());
    let _ = std::fs::remove_file(tmp);
    result
}
fn package_config(ctx: &Context, p: &MachinePlugin) -> Result<(), String> {
    let pi = p.provider == ProviderKind::Pi;
    let path = if pi { ctx.root(p.provider).join("settings.json") } else { inventory::opencode_path(ctx) };
    let before = text(&path)?;
    let doc = if before.is_empty() { json!({}) } else { json_config::parse(&before)? };
    let key = if pi {
        "packages"
    } else if doc.get("plugins").is_some() {
        "plugins"
    } else {
        "plugin"
    };
    let mut entries = doc[key].as_array().cloned().unwrap_or_default();
    let source = p.source.as_deref().ok_or("No package source")?;
    entries.retain(|v| {
        let s = v.as_str().or_else(|| v[if pi { "source" } else { "package" }].as_str()).unwrap_or("");
        package_name(s) != p.id && s != source
    });
    if pi || p.enabled == Some(true) {
        let value = if pi && !p.resources.is_empty() {
            let mut obj = serde_json::Map::new();
            obj.insert("source".into(), json!(source));
            for (key, filters) in &p.resources {
                obj.insert(key.clone(), json!(filters));
            }
            Value::Object(obj)
        } else {
            json!(source)
        };
        entries.push(value);
    }
    let after = json_config::replace(&before, key, &json!(entries))?;
    write(&path, &before, &after)
}
pub(super) fn codex_enabled(ctx: &Context, p: &MachinePlugin) -> Result<(), String> {
    let path = ctx.root(p.provider).join("config.toml");
    let before = text(&path)?;
    let mut doc =
        before.parse::<toml_edit::DocumentMut>().map_err(|_| "Cannot edit config.toml. Fix its TOML syntax first.".to_string())?;
    // Indexing a non-table panics in toml_edit; an unexpected shape is the user's to fix.
    let unexpected = || "config.toml has an unexpected plugins setting. Fix it on this machine.".to_string();
    let plugins = doc.entry("plugins").or_insert(toml_edit::table()).as_table_like_mut().ok_or_else(unexpected)?;
    let entry = plugins.entry(&p.id).or_insert(toml_edit::table()).as_table_like_mut().ok_or_else(unexpected)?;
    entry.insert("enabled", toml_edit::value(p.enabled.unwrap_or(false)));
    write(&path, &before, &doc.to_string())
}
async fn apply(
    state: &AppState,
    ctx: &Context,
    p: &MachinePlugin,
    existing: Option<&MachinePlugin>,
    inventory: &PluginInventory,
) -> Result<(), String> {
    let source = p.source.as_deref().ok_or("No source")?;
    if let Some(market) = &p.marketplace {
        let found = inventory
            .plugins
            .iter()
            .find(|v| v.provider == p.provider && v.kind == PluginKind::Marketplace && v.marketplace.as_ref() == Some(market));
        if let Some(found) = found {
            if found.source != p.source {
                return Err("The target uses a different source for this marketplace. Resolve it there first.".into());
            }
        } else {
            let mut a = args(&["plugin", "marketplace", "add", source]);
            if p.provider == ProviderKind::Codex
                && let Some((url, r)) = source.rsplit_once('#')
            {
                a = args(&["plugin", "marketplace", "add", url, &format!("--ref={r}")]);
            }
            run(state, ctx, p.provider, a).await?;
            // Do not install an ID against an unexpected catalog of the same name.
            let fresh = inventory::read(ctx);
            if !fresh.plugins.iter().any(|v| {
                v.provider == p.provider && v.kind == PluginKind::Marketplace && v.marketplace == p.marketplace && v.source == p.source
            }) {
                return Err("The installer did not register the expected marketplace. Review it on the target.".into());
            }
        }
    }
    if p.kind == PluginKind::Marketplace {
        return Ok(());
    }
    let needs_install = existing.is_none_or(|v| v.source != p.source || v.version != p.version);
    match p.provider {
        ProviderKind::ClaudeCode => {
            if needs_install {
                run(
                    state,
                    ctx,
                    p.provider,
                    args(&["plugin", if existing.is_some() { "update" } else { "install" }, &p.id, "--scope", "user"]),
                )
                .await?;
            }
            run(
                state,
                ctx,
                p.provider,
                args(&["plugin", if p.enabled == Some(true) { "enable" } else { "disable" }, &p.id, "--scope", "user"]),
            )
            .await?;
        }
        ProviderKind::Codex => {
            if needs_install {
                run(state, ctx, p.provider, args(&["plugin", "add", &p.id, "--json"])).await?;
            }
            codex_enabled(ctx, p)?;
        }
        ProviderKind::Opencode => package_config(ctx, p)?,
        ProviderKind::Pi => {
            if needs_install {
                run(state, ctx, p.provider, args(&["install", source])).await?;
            }
            package_config(ctx, p)?;
        }
        ProviderKind::Omp => {
            if needs_install {
                let mut a = args(&["plugin", "install", if p.marketplace.is_some() { &p.id } else { source }]);
                if p.marketplace.is_some() {
                    a.extend(args(&["--scope", "user"]));
                    if existing.is_some() {
                        a.push("--force".into());
                    }
                }
                run(state, ctx, p.provider, a).await?;
            }
            let mut a = args(&["plugin", if p.enabled == Some(true) { "enable" } else { "disable" }, &p.id]);
            if p.marketplace.is_some() {
                a.extend(args(&["--scope", "user"]));
            }
            run(state, ctx, p.provider, a).await?;
        }
        ProviderKind::Cursor => return Err("Install this plugin in Cursor’s marketplace.".into()),
        // Kybern never edits the user's Antigravity plugins, hooks or settings.
        ProviderKind::Antigravity => return Err("Install this plugin with `agy plugin install` on the target.".into()),
        ProviderKind::GrokBuild => return Err("Plugin install is not supported for Grok Build yet.".into()),
    }
    Ok(())
}

pub async fn install(state: &AppState, from: &str, request: PluginInstallRequest) -> Result<PluginInstallResult, RpcError> {
    validate(&request.plugin).map_err(RpcError::invalid_params)?;
    let _lock = state.plugin_runtime.lock.lock().await;
    let key = format!("plugin-install.{from}.{}", request.operation_id);
    let body = serde_json::to_value(&request).map_err(RpcError::internal)?;
    if let Some(saved) = state.store.meta_get(&key).map_err(RpcError::internal)? {
        let saved: Value = serde_json::from_str(&saved).map_err(RpcError::internal)?;
        if saved["request"] != body {
            return Err(RpcError::new(codes::CONFLICT, "Operation ID was already used for another declaration"));
        }
        return serde_json::from_value(saved["result"].clone()).map_err(RpcError::internal);
    }
    let Some((env, home)) = state.skill_homes.context() else {
        return Err(RpcError::new(codes::NOT_AVAILABLE, "No plugin home configured"));
    };
    let ctx = Context { env, home };
    let inventory = inventory::read(&ctx);
    let p = &request.plugin;
    let existing = inventory.plugins.iter().find(|v| v.provider == p.provider && v.id == p.id);
    let mut result = PluginInstallResult { state: PluginInstallState::Failed, message: String::new(), actual: existing.cloned() };
    if let Some(warning) = inventory.warnings.iter().find(|w| w.starts_with(p.provider.as_str())) {
        result.message = warning.clone();
    } else if existing.is_some_and(|v| v.fingerprint == p.fingerprint) {
        result.state = PluginInstallState::AlreadyThere;
        result.message = "Already there".into();
    } else if existing.is_some_and(|v| v.problem.is_some()) {
        result.message = "The target has custom or unsupported plugin settings. Configure this plugin on that machine.".into();
    } else if existing.is_some_and(|v| request.replaces.as_ref() != Some(&v.fingerprint))
        || existing.is_none() && request.replaces.is_some()
    {
        result.message = "Target changed or has a different declaration. Refresh and explicitly replace the version shown.".into();
    } else {
        match apply(state, &ctx, p, existing, &inventory).await {
            Err(e) => result.message = kybern_drivers::account_auth::redact(&e),
            Ok(()) => {
                result.actual = inventory::read(&ctx).plugins.into_iter().find(|v| v.provider == p.provider && v.id == p.id);
                if result.actual.as_ref().is_some_and(|v| v.enabled == p.enabled && v.source == p.source) {
                    result.state =
                        if p.provider == ProviderKind::Opencode { PluginInstallState::Configured } else { PluginInstallState::Installed };
                    result.message = if p.provider == ProviderKind::Opencode {
                        "Configured; OpenCode installs it on its next start"
                    } else if result.actual.as_ref().is_some_and(|v| v.version != p.version) {
                        "Installed the version available from the marketplace; versions still differ"
                    } else {
                        "Installed"
                    }
                    .into();
                } else {
                    result.message = "Installer finished, but the requested state could not be verified. Review it on the target.".into();
                }
            }
        }
    }
    state
        .store
        .meta_set(&key, &serde_json::to_string(&json!({"request":body,"result":result})).map_err(RpcError::internal)?)
        .map_err(RpcError::internal)?;
    Ok(result)
}
