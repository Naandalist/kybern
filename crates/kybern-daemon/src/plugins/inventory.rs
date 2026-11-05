use super::*;
use serde_json::Value;

fn base(provider: ProviderKind, id: &str, kind: PluginKind) -> MachinePlugin {
    MachinePlugin {
        id: id.into(),
        provider,
        kind,
        name: id.trim_start_matches("marketplace:").into(),
        marketplace: None,
        source: None,
        version: None,
        enabled: Some(true),
        resources: BTreeMap::new(),
        contents: PluginContents::default(),
        problem: None,
        fingerprint: String::new(),
    }
}
fn dirs(path: &Path) -> Vec<PathBuf> {
    let mut result: Vec<_> =
        std::fs::read_dir(path).into_iter().flatten().filter_map(Result::ok).take(1000).map(|e| e.path()).filter(|p| p.is_dir()).collect();
    result.sort();
    result
}
fn count(path: &Path) -> u32 {
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .take(1000)
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .count() as u32
}
fn manifest(path: &Path) -> Value {
    [
        ".claude-plugin/plugin.json",
        ".codex-plugin/plugin.json",
        ".cursor-plugin/plugin.json",
        ".omp-plugin/plugin.json",
        "plugin.json",
        "package.json",
    ]
    .into_iter()
    .filter_map(|name| json(&path.join(name)).ok())
    .find(|v| v.get("name").is_some())
    .unwrap_or_default()
}
fn size(value: &Value) -> u32 {
    match value {
        Value::Array(a) => a.len() as u32,
        Value::Object(o) => o.len() as u32,
        Value::String(_) => 1,
        _ => 0,
    }
}
fn contents(path: &Path) -> PluginContents {
    let m = manifest(path);
    let hooks = json(&path.join("hooks/hooks.json")).unwrap_or_default();
    let mcp = json(&path.join(".mcp.json")).unwrap_or_default();
    let mut c = PluginContents {
        hooks: hooks.get("hooks").and_then(Value::as_object).map(|o| o.values().map(size).sum()).unwrap_or(0).max(size(&m["hooks"])),
        mcp_servers: size(&mcp["mcpServers"]).max(size(&m["mcpServers"])),
        skills: count(&path.join("skills")).max(size(&m["skills"])),
        agents: count(&path.join("agents")).max(size(&m["agents"])),
        commands: count(&path.join("commands")).max(size(&m["commands"])),
        extensions: count(&path.join("extensions")),
        ..Default::default()
    };
    let pkg = json(&path.join("package.json")).unwrap_or_default();
    c.extensions = c.extensions.max(size(&pkg["omp"]["extensions"])).max(size(&pkg["pi"]["extensions"]));
    if let Some(deps) = pkg["dependencies"].as_object() {
        c.npm_packages.extend(deps.keys().filter(|k| npm_spec(k)).take(100).cloned());
    }
    if m.get("lspServers").is_some() || path.join(".lsp.json").exists() {
        c.notes.push("Adds LSP servers".into());
    }
    if m.get("dapAdapters").is_some() {
        c.notes.push("Adds debug adapters".into());
    }
    if count(&path.join("rules")) > 0 {
        c.notes.push("Adds rules".into());
    }
    c.notes.push("Based on local metadata; packages may execute additional code".into());
    c
}
fn source(item: &mut MachinePlugin, value: Option<&str>, npm: bool) {
    if let Some(value) = value.filter(|v| if npm { npm_spec(v) } else { portable_source(v) }) {
        item.source = Some(value.into());
    } else {
        item.problem = Some("Local, private or unknown source; set it up on the target machine.".into());
    }
}
fn marketplace(provider: ProviderKind, name: &str, uri: Option<&str>) -> MachinePlugin {
    let mut item = base(provider, &format!("marketplace:{name}"), PluginKind::Marketplace);
    item.marketplace = Some(name.into());
    source(&mut item, uri, false);
    item.contents.notes.push("Adds a marketplace; its installer may resolve dependencies".into());
    item
}
fn claude(ctx: &Context, provider: ProviderKind) -> Result<Vec<MachinePlugin>, String> {
    let root = if provider == ProviderKind::Omp { ctx.omp_data() } else { ctx.root(provider) };
    let mut items = Vec::new();
    if provider == ProviderKind::Omp {
        let reg = json(&root.join("marketplaces.json"))?;
        for entry in reg["marketplaces"].as_array().into_iter().flatten() {
            if let Some(name) = entry["name"].as_str() {
                items.push(marketplace(provider, name, entry["sourceUri"].as_str()));
            }
        }
    } else {
        let reg = json(&root.join("plugins/known_marketplaces.json"))?;
        for (name, v) in reg.as_object().into_iter().flatten() {
            let s = &v["source"];
            let uri = s["repo"].as_str().or(s["url"].as_str());
            let uri = uri.map(|u| match s["ref"].as_str() {
                Some(r) => format!("{u}#{r}"),
                None => u.into(),
            });
            items.push(marketplace(provider, name, uri.as_deref()));
        }
    }
    let settings = if provider == ProviderKind::ClaudeCode { json(&root.join("settings.json"))? } else { Value::Null };
    let reg = json(&root.join("plugins/installed_plugins.json"))?;
    for (id, records) in reg["plugins"].as_object().into_iter().flatten() {
        let Some(v) = records.as_array().and_then(|a| a.iter().filter(|r| r["scope"] == "user").max_by_key(|v| v["lastUpdated"].as_str()))
        else {
            continue;
        };
        let mut item = base(provider, id, PluginKind::Plugin);
        item.marketplace = id.rsplit_once('@').map(|(_, m)| m.into());
        if let Some(m) = items.iter().find(|m| m.marketplace == item.marketplace) {
            item.source = m.source.clone();
            item.problem = m.problem.clone();
        } else {
            item.problem = Some("Marketplace source is not recorded on this machine.".into());
        }
        item.version = v["version"].as_str().map(Into::into);
        item.enabled = Some(if provider == ProviderKind::Omp {
            v["enabled"].as_bool().unwrap_or(true)
        } else {
            settings["enabledPlugins"][id].as_bool().unwrap_or(false)
        });
        if let Some(path) = v["installPath"].as_str().map(PathBuf::from).filter(|p| p.starts_with(&root)) {
            item.contents = contents(&path);
        }
        item.contents.notes.push("The target installs the marketplace version available there; it may differ from this version".into());
        items.push(item);
    }
    if provider == ProviderKind::Omp {
        let pkg = json(&root.join("plugins/package.json"))?;
        let lock = json(&root.join("plugins/omp-plugins.lock.json"))?;
        for (name, spec) in pkg["dependencies"].as_object().into_iter().flatten() {
            if items.iter().any(|i| i.id == *name) {
                continue;
            }
            let mut item = base(provider, name, PluginKind::Plugin);
            item.version = lock["plugins"][name]["version"].as_str().map(Into::into);
            let pin = item.version.as_ref().map(|v| format!("{name}@{v}"));
            let spec = spec.as_str().unwrap_or("");
            let registry = if spec.starts_with("npm:") {
                npm_spec(spec) && package_name(spec) == name
            } else {
                semver::VersionReq::parse(spec).is_ok()
                    || spec.starts_with(|c: char| c.is_ascii_alphabetic())
                        && spec.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
            };
            if !registry {
                source(&mut item, None, true);
            } else {
                source(&mut item, pin.as_deref(), true);
            }
            item.enabled = Some(lock["plugins"][name]["enabled"].as_bool().unwrap_or(true));
            if npm_spec(name) {
                item.contents = contents(&root.join("plugins/node_modules").join(name));
            }
            item.contents.npm_packages.insert(0, name.clone());
            if !lock["plugins"][name]["enabledFeatures"].is_null() {
                item.problem = Some("Custom feature selection; configure this package on the target.".into());
            }
            items.push(item);
        }
    }
    Ok(items)
}
fn active_codex_version(base: &Path) -> Option<PathBuf> {
    if base.join("local").is_dir() {
        return Some(base.join("local"));
    }
    dirs(base).into_iter().max_by(|a, b| {
        let a = a.file_name().unwrap().to_string_lossy();
        let b = b.file_name().unwrap().to_string_lossy();
        match (semver::Version::parse(&a), semver::Version::parse(&b)) {
            (Ok(a), Ok(b)) => a.cmp(&b),
            _ => a.cmp(&b),
        }
    })
}
fn codex(ctx: &Context) -> Result<Vec<MachinePlugin>, String> {
    let provider = ProviderKind::Codex;
    let root = ctx.root(provider);
    let doc = text(&root.join("config.toml"))?
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| "Cannot read config.toml. Fix its TOML syntax on this machine.".to_string())?;
    let mut items = Vec::new();
    if let Some(markets) = doc.get("marketplaces").and_then(toml_edit::Item::as_table_like) {
        for (name, m) in markets.iter() {
            let uri = m.get("source").and_then(toml_edit::Item::as_str);
            let mut item = marketplace(provider, name, uri);
            // --ref is reconstructed separately from the source suffix by the install adapter.
            if let Some(r) = m.get("ref").and_then(toml_edit::Item::as_str)
                && let Some(s) = item.source.as_mut()
            {
                s.push('#');
                s.push_str(r);
            }
            items.push(item);
        }
    }
    let enabled = doc.get("plugins").and_then(toml_edit::Item::as_table_like);
    for market in dirs(&root.join("plugins/cache")) {
        for plugin in dirs(&market) {
            let Some(path) = active_codex_version(&plugin) else {
                continue;
            };
            let m = manifest(&path);
            let name = plugin.file_name().unwrap().to_string_lossy();
            let market_name = market.file_name().unwrap().to_string_lossy();
            let id = format!("{name}@{market_name}");
            let mut item = base(provider, &id, PluginKind::Plugin);
            item.marketplace = Some(market_name.to_string());
            if let Some(src) = items.iter().find(|s| s.marketplace == item.marketplace && s.kind == PluginKind::Marketplace) {
                item.source = src.source.clone();
                item.problem = src.problem.clone();
            } else {
                item.problem = Some("Account-managed or unknown marketplace; install it in Codex on the target.".into());
            }
            let flag = |key: &str| enabled.and_then(|t| t.get(key)).and_then(|v| v.get("enabled")).and_then(toml_edit::Item::as_bool);
            item.enabled = Some(flag(&id).or_else(|| flag("_default")).unwrap_or(true));
            item.version = m["version"].as_str().map(Into::into).or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()));
            item.contents = contents(&path);
            item.contents.notes.push("Installs the marketplace version available on the target".into());
            items.push(item);
        }
    }
    Ok(items)
}
pub(super) fn opencode_path(ctx: &Context) -> PathBuf {
    let root = ctx.root(ProviderKind::Opencode);
    if root.join("opencode.jsonc").exists() { root.join("opencode.jsonc") } else { root.join("opencode.json") }
}
fn packages(ctx: &Context, provider: ProviderKind) -> Result<Vec<MachinePlugin>, String> {
    let root = ctx.root(provider);
    let pi = provider == ProviderKind::Pi;
    let config = if pi { root.join("settings.json") } else { opencode_path(ctx) };
    let doc = json(&config)?;
    let mut items = Vec::new();
    let key = if pi {
        "packages"
    } else if doc.get("plugins").is_some() {
        "plugins"
    } else {
        "plugin"
    };
    if doc.get(key).is_some_and(|v| !v.is_array()) {
        return Err(format!("{key} must be an array; fix the config before copying"));
    }
    if !pi && doc.get("plugin").is_some() && doc.get("plugins").is_some() {
        return Err("Both plugin and plugins are configured. Resolve them on this machine before copying.".into());
    }
    for entry in doc[key].as_array().into_iter().flatten() {
        let s = entry.as_str().or_else(|| entry[if pi { "source" } else { "package" }].as_str()).unwrap_or("");
        let spec = s.strip_prefix("npm:").unwrap_or(s);
        let name = package_name(spec);
        let git = pi && pi_git_source(s);
        // Pi reads a source without `npm:`, `git:` or a URL as a path relative to its settings.
        let npm = npm_spec(spec) && (!pi || s.starts_with("npm:"));
        let local = s.starts_with(['.', '/', '~']) || s.starts_with("file:") || pi && !npm && !git && !s.contains(':');
        // A rejected URL can include credentials even in its apparent package name. Do not send
        // any part of it as a name/id; the opaque identity only keeps separate rows stable.
        let display = if local {
            Path::new(s).file_name().and_then(|n| n.to_str()).unwrap_or("local")
        } else if npm || git {
            name
        } else {
            "Uncopyable package"
        };
        let id = if local {
            format!("local:{display}")
        } else if npm || git {
            name.to_string()
        } else {
            let digest: String = Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
            format!("unavailable:{digest}")
        };
        let mut item = base(provider, &id, PluginKind::Plugin);
        item.name = display.to_string();
        if npm {
            let pkgroot = if pi { root.join("npm/node_modules") } else { root.join("node_modules") };
            let pkg = json(&pkgroot.join(name).join("package.json"))?;
            item.version = spec
                .strip_prefix(&format!("{name}@"))
                .filter(|v| semver::Version::parse(v).is_ok())
                .map(Into::into)
                .or_else(|| pkg["version"].as_str().map(Into::into));
            let pinned = item.version.as_ref().map(|v| format!("{}{}@{v}", if pi { "npm:" } else { "" }, name)).unwrap_or_else(|| s.into());
            source(&mut item, Some(&pinned), true);
            item.contents = contents(&pkgroot.join(name));
            item.contents.npm_packages.insert(0, name.into());
        } else if git {
            item.source = Some(s.into());
        } else {
            source(&mut item, None, false);
        }
        if let Some(obj) = entry.as_object() {
            if pi && obj.keys().all(|k| ["source", "extensions", "skills", "prompts", "themes"].contains(&k.as_str())) {
                for key in ["extensions", "skills", "prompts", "themes"] {
                    if let Some(v) = obj.get(key) {
                        let Some(filters) =
                            v.as_array().and_then(|a| a.iter().map(|v| v.as_str().map(String::from)).collect::<Option<Vec<_>>>())
                        else {
                            item.problem = Some("Unsupported resource filters".into());
                            continue;
                        };
                        item.resources.insert(key.into(), filters);
                    }
                }
                item.enabled =
                    Some(!["extensions", "skills", "prompts", "themes"].iter().all(|k| item.resources.get(*k).is_some_and(Vec::is_empty)));
            } else {
                item.problem = Some("Contains custom options; configure it on the target without copying secrets.".into());
            }
        }
        item.contents.notes.push(
            if pi {
                "Extensions can run code, add tools and hooks"
            } else {
                "OpenCode installs this package on its next start; plugin code can register hooks and tools"
            }
            .into(),
        );
        items.push(item);
    }
    Ok(items)
}
fn cursor(ctx: &Context) -> Vec<MachinePlugin> {
    let mut items = Vec::new();
    for market in dirs(&ctx.root(ProviderKind::Cursor).join("plugins/cache")) {
        for plugin in dirs(&market) {
            if let Some(path) = dirs(&plugin).last() {
                let id = format!("{}@{}", plugin.file_name().unwrap().to_string_lossy(), market.file_name().unwrap().to_string_lossy());
                let mut item = base(ProviderKind::Cursor, &id, PluginKind::Plugin);
                item.version = manifest(path)["version"].as_str().map(Into::into);
                item.enabled = None;
                item.contents = contents(path);
                item.problem =
                    Some("Cached only; Cursor has no supported noninteractive plugin installer. Install in Cursor’s marketplace.".into());
                items.push(item);
            }
        }
    }
    items
}
fn local_extensions(ctx: &Context, provider: ProviderKind) -> Vec<MachinePlugin> {
    let root = ctx.root(provider);
    let dir = root.join(if provider == ProviderKind::Opencode { "plugins" } else { "extensions" });
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .take(1000)
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let mut item = base(provider, &format!("local:{name}"), PluginKind::Plugin);
            item.name = name;
            item.enabled = None;
            item.contents.extensions = 1;
            item.problem = Some("Local extension; install from a published package on the target.".into());
            item
        })
        .collect()
}
pub(super) fn read(ctx: &Context) -> PluginInventory {
    let mut result = PluginInventory::default();
    for provider in ProviderKind::ALL {
        let items = match provider {
            ProviderKind::ClaudeCode | ProviderKind::Omp => claude(ctx, provider),
            ProviderKind::Codex => codex(ctx),
            ProviderKind::Pi | ProviderKind::Opencode => packages(ctx, provider),
            ProviderKind::Cursor => Ok(cursor(ctx)),
            // Kybern does not read or change the user's Antigravity plugins.
            ProviderKind::Antigravity => Ok(Vec::new()),
            ProviderKind::GrokBuild => Ok(Vec::new()),
        };
        match items {
            Ok(items) => result.plugins.extend(items),
            Err(e) => result.warnings.push(format!("{}: {e}", provider.as_str())),
        }
        if matches!(provider, ProviderKind::Omp | ProviderKind::Pi | ProviderKind::Opencode) {
            result.plugins.extend(local_extensions(ctx, provider));
        }
    }
    result.warnings.push("cursor: Cached plugins only; active installation and enabled state are not available from Cursor’s local metadata. Manage them in Cursor.".into());
    for item in &mut result.plugins {
        if !safe_id(&item.id) {
            // Local extensions can have filenames (including spaces) that are not CLI selectors.
            // Keep them visible with an opaque identity, but never offer to install them remotely.
            item.id = format!("unavailable:{}", fingerprint(item));
            item.problem.get_or_insert_with(|| "Unsupported plugin identifier; configure it on the target machine.".into());
        }
        item.fingerprint = fingerprint(item);
    }
    result.plugins.sort_by(|a, b| (a.provider.as_str(), &a.id).cmp(&(b.provider.as_str(), &b.id)));
    result
}
