use super::*;
use crate::network::testutil::TempDir;
use serde_json::json;

fn put(home: &Path, path: &str, body: &str) {
    let p = home.join(path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}
fn context(dir: &TempDir) -> Context {
    Context { home: std::fs::canonicalize(dir.path()).unwrap(), env: BTreeMap::new() }
}
fn seed(ctx: &Context) {
    let h = &ctx.home;
    put(h, ".claude/plugins/known_marketplaces.json", r#"{"tools":{"source":{"source":"github","repo":"acme/tools"}}}"#);
    put(h, ".claude/settings.json", r#"{"enabledPlugins":{"review@tools":false}}"#);
    put(h,".claude/plugins/installed_plugins.json",&json!({"version":2,"plugins":{"review@tools":[{"scope":"project","version":"99"},{"scope":"user","version":"1","installPath":h.join(".claude/plugins/cache/tools/review/1")} ]}}).to_string());
    put(h, ".claude/plugins/cache/tools/review/1/.claude-plugin/plugin.json", r#"{"name":"review","version":"1"}"#);
    put(h, ".claude/plugins/cache/tools/review/1/hooks/hooks.json", r#"{"hooks":{"PreToolUse":[{},{}]}}"#);
    put(h, ".claude/plugins/cache/tools/review/1/.mcp.json", r#"{"mcpServers":{"review":{}}}"#);
    put(
        h,
        ".codex/config.toml",
        "# keep\n[marketplaces.tools]\nsource_type='git'\nsource='https://github.com/acme/tools.git'\n[plugins._default]\nenabled=false\n",
    );
    put(h, ".codex/plugins/cache/tools/review/1/.claude-plugin/plugin.json", r#"{"name":"review","version":"1"}"#);
    put(
        h,
        ".config/opencode/opencode.jsonc",
        r#"{ // keep
 "model": "test/model", "plugin": ["@acme/review@1.2.3", "./local.ts", {"package":"custom","options":{"secret":"never-send"}}],
}"#,
    );
    put(
        h,
        ".pi/agent/settings.json",
        r#"{"packages":[{"source":"npm:pi-review@2.0.0","extensions":[],"skills":[],"prompts":[],"themes":[]},"./local tools"]}"#,
    );
    put(h, ".omp/plugins/package.json", r#"{"dependencies":{"omp-review":"npm:omp-review@1.0.0"}}"#);
    put(
        h,
        ".omp/plugins/omp-plugins.lock.json",
        r#"{"plugins":{"omp-review":{"version":"1.0.0","enabled":false,"enabledFeatures":null}}}"#,
    );
    put(h, ".cursor/plugins/cache/tools/review/abc/.cursor-plugin/plugin.json", r#"{"name":"review","version":"1"}"#);
}
#[test]
fn inventory_covers_six_harnesses_without_credentials_or_cli() {
    let dir = TempDir::new("plugins-inventory");
    let ctx = context(&dir);
    seed(&ctx);
    let inv = inventory::read(&ctx);
    assert_eq!(inv.warnings.len(), 1, "{:?}", inv.warnings);
    // Kybern reads nobody's Antigravity plugins (it never touches `agy`'s setup) and Grok Build has no plugin support yet.
    let unread = [ProviderKind::Antigravity, ProviderKind::GrokBuild];
    for provider in ProviderKind::ALL.into_iter().filter(|provider| !unread.contains(provider)) {
        assert!(inv.plugins.iter().any(|p| p.provider == provider), "{provider:?}");
    }
    assert!(inv.plugins.iter().all(|p| !unread.contains(&p.provider)));
    let cc = inv.plugins.iter().find(|p| p.provider == ProviderKind::ClaudeCode && p.kind == PluginKind::Plugin).unwrap();
    assert_eq!(cc.version.as_deref(), Some("1"));
    assert_eq!(cc.enabled, Some(false));
    assert_eq!(cc.contents.hooks, 2);
    assert_eq!(cc.contents.mcp_servers, 1);
    let pi = inv.plugins.iter().find(|p| p.id == "pi-review").unwrap();
    assert_eq!(pi.enabled, Some(false));
    assert_eq!(pi.resources.len(), 4);
    let local = inv.plugins.iter().find(|p| p.name == "local tools").unwrap();
    assert!(local.problem.is_some() && local.source.is_none());
    assert!(inv.plugins.iter().find(|p| p.provider == ProviderKind::Cursor).unwrap().problem.is_some());
    assert!(!serde_json::to_string(&inv).unwrap().contains("never-send"));
    assert!(!serde_json::to_string(&inv).unwrap().contains(dir.path().to_str().unwrap()));
}
#[test]
fn sources_and_fingerprints_fail_closed() {
    for bad in
        ["/tmp/local", "../local", "file:///tmp/p", "--help", "https://user:secret@host/repo", "https://host/repo?token=x", "acme/$(cmd)"]
    {
        assert!(!portable_source(bad), "{bad}");
    }
    for good in ["https://github.com/acme/tools.git", "acme/tools", "acme/tools#v1"] {
        assert!(portable_source(good));
    }
    for bad in ["-x", "../local", "pkg;touch", "pkg@https://host"] {
        assert!(!npm_spec(bad));
    }
    let dir = TempDir::new("plugins-hash");
    let ctx = context(&dir);
    seed(&ctx);
    for mut p in inventory::read(&ctx).plugins.into_iter().filter(|p| p.problem.is_none()) {
        install::validate(&p).unwrap();
        p.enabled = Some(!p.enabled.unwrap());
        assert!(install::validate(&p).is_err());
    }
}
#[test]
fn jsonc_edits_preserve_other_bytes_and_refuse_duplicates() {
    let input = "{ // comment\n\"auth\":{\"nested\":true}, \"plugin\": [\"a\",], }";
    let out = json_config::replace(input, "plugin", &json!(["b"])).unwrap();
    assert!(out.contains("// comment"));
    assert!(out.contains("\"auth\":{\"nested\":true}"));
    assert_eq!(json_config::parse(&out).unwrap()["plugin"], json!(["b"]));
    assert!(json_config::replace(r#"{"plugin":[],"plugin":[]}"#, "plugin", &json!([])).is_err());
    let out = json_config::replace("{\"other\":true,}", "plugin", &json!([])).unwrap();
    assert_eq!(json_config::parse(&out).unwrap()["other"], true);
}
fn state(dir: &TempDir) -> crate::state::AppState {
    let paths = crate::config::Paths::resolve(Some(dir.path().join("data"))).unwrap();
    let state = AppState::initialize_in_memory(&paths).unwrap();
    state.skill_homes.set(BTreeMap::new(), dir.path().join("home"));
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    state
}
#[tokio::test]
async fn configured_packages_are_idempotent_and_refuse_stale_replacements() {
    let source = TempDir::new("plugin-source");
    let ctx = context(&source);
    seed(&ctx);
    let p = inventory::read(&ctx).plugins.into_iter().find(|p| p.id == "@acme/review").unwrap();
    let target = TempDir::new("plugin-target");
    let state = state(&target);
    let request = PluginInstallRequest { operation_id: uuid::Uuid::now_v7(), plugin: p.clone(), replaces: None };
    let result = install(&state, "a", request.clone()).await.unwrap();
    assert_eq!(result.state, PluginInstallState::Configured, "{}", result.message);
    let again = install(&state, "a", request.clone()).await.unwrap();
    assert_eq!(again.message, result.message);
    let again = install(&state, "a", PluginInstallRequest { operation_id: uuid::Uuid::now_v7(), ..request.clone() }).await.unwrap();
    assert_eq!(again.state, PluginInstallState::AlreadyThere);
    let mut changed = p.clone();
    changed.source = Some("@acme/review@2.0.0".into());
    changed.version = Some("2.0.0".into());
    changed.fingerprint = fingerprint(&changed);
    let refused =
        install(&state, "a", PluginInstallRequest { operation_id: uuid::Uuid::now_v7(), plugin: changed.clone(), replaces: None })
            .await
            .unwrap();
    assert_eq!(refused.state, PluginInstallState::Failed);
    let replaced =
        install(&state, "a", PluginInstallRequest { operation_id: uuid::Uuid::now_v7(), plugin: changed, replaces: Some(p.fingerprint) })
            .await
            .unwrap();
    assert_eq!(replaced.state, PluginInstallState::Configured, "{}", replaced.message);
    let tampered = PluginInstallRequest { plugin: inventory(&state).await.unwrap().plugins[0].clone(), ..request };
    assert!(install(&state, "a", tampered).await.is_err());
}

#[tokio::test]
async fn replacement_preserves_unknown_target_options_and_their_credentials() {
    let source = TempDir::new("plugin-options-source");
    let ctx = context(&source);
    seed(&ctx);
    let plugin = inventory::read(&ctx).plugins.into_iter().find(|p| p.id == "@acme/review").unwrap();
    let target = TempDir::new("plugin-options-target");
    let state = state(&target);
    let home = target.path().join("home");
    let before = r#"{"plugin":[{"package":"@acme/review@1.0.0","options":{"token":"keep-this-private"}}]}"#;
    put(&home, ".config/opencode/opencode.json", before);
    let existing = inventory(&state).await.unwrap().plugins.into_iter().find(|p| p.id == plugin.id).unwrap();
    let result =
        install(&state, "a", PluginInstallRequest { operation_id: uuid::Uuid::now_v7(), plugin, replaces: Some(existing.fingerprint) })
            .await
            .unwrap();
    assert_eq!(result.state, PluginInstallState::Failed);
    assert_eq!(text(&home.join(".config/opencode/opencode.json")).unwrap(), before);
    assert!(!serde_json::to_string(&result).unwrap().contains("keep-this-private"));
}
#[cfg(unix)]
#[tokio::test]
async fn fake_installers_receive_only_the_target_environment_and_report_failures() {
    let dir = TempDir::new("plugins-fake");
    let state = state(&dir);
    let home = dir.path().join("home");
    let fake = dir.path().join("fake.py");
    kybern_protocol::process::write_executable(
        &fake,
        r#"#!/usr/bin/python3
import os,sys,json
from pathlib import Path
h=Path(os.environ['HOME'])
assert os.environ['USERPROFILE']==str(h)
assert os.environ['CODEX_HOME']==str(h/'.codex')
assert os.environ['CLAUDE_CONFIG_DIR']==str(h/'.claude')
assert os.environ['XDG_CONFIG_HOME']==str(h/'.config')
assert os.environ['PI_CODING_AGENT_DIR']==str(h/'.pi/agent')
assert os.environ['APPDATA']==str(h/'AppData/Roaming')
with (h/'calls.jsonl').open('a') as f:f.write(json.dumps(sys.argv[1:])+'\n')
print('fake harness refuses this package',file=sys.stderr)
sys.exit(7)
"#,
    )
    .unwrap();
    let src = TempDir::new("plugins-fake-source");
    let ctx = context(&src);
    seed(&ctx);
    for provider in [ProviderKind::ClaudeCode, ProviderKind::Codex, ProviderKind::Pi, ProviderKind::Omp] {
        state.plugin_runtime.binaries.write().unwrap().insert(provider, fake.clone());
        let p = inventory::read(&ctx)
            .plugins
            .into_iter()
            .find(|p| p.provider == provider && p.kind == PluginKind::Plugin && p.problem.is_none())
            .unwrap();
        let r = install(&state, "a", PluginInstallRequest { operation_id: uuid::Uuid::now_v7(), plugin: p, replaces: None }).await.unwrap();
        assert_eq!(r.state, PluginInstallState::Failed);
        assert!(r.message.contains("fake harness refuses"), "{}", r.message);
    }
    let calls = std::fs::read_to_string(home.join("calls.jsonl")).unwrap();
    assert_eq!(calls.lines().count(), 4);
    assert!(calls.contains("marketplace"));
    assert!(calls.contains("npm:pi-review@2.0.0"));
    assert!(calls.contains("omp-review@1.0.0"));
    let cursor = inventory::read(&ctx).plugins.into_iter().find(|p| p.provider == ProviderKind::Cursor).unwrap();
    assert!(
        install(&state, "a", PluginInstallRequest { operation_id: uuid::Uuid::now_v7(), plugin: cursor, replaces: None }).await.is_err()
    );
}
#[tokio::test]
async fn unconfigured_test_state_never_reads_real_harness_data() {
    let dir = TempDir::new("plugin-isolation");
    let paths = crate::config::Paths::resolve(Some(dir.path().to_path_buf())).unwrap();
    let state = AppState::initialize_in_memory(&paths).unwrap();
    assert!(inventory(&state).await.unwrap().plugins.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn fake_installers_install_then_disable_and_repeated_requests_do_not_execute_again() {
    for provider in [ProviderKind::ClaudeCode, ProviderKind::Codex, ProviderKind::Pi, ProviderKind::Omp] {
        let src = TempDir::new("plugin-success-source");
        let ctx = context(&src);
        seed(&ctx);
        let plugin = inventory::read(&ctx)
            .plugins
            .into_iter()
            .find(|p| p.provider == provider && p.kind == PluginKind::Plugin && p.problem.is_none())
            .unwrap();
        let dst = TempDir::new("plugin-success-target");
        let state = state(&dst);
        let home = dst.path().join("home");
        // Each fake invocation consumes a fixture step: argument contract and resulting on-disk metadata are independent assertions.
        let mut steps = Vec::new();
        let record=json!({"version":2,"plugins":{&plugin.id:[{"scope":"user","version":"1","installPath":home.join(".claude/plugins/cache/tools/review/1")}]}}).to_string();
        match provider {
            ProviderKind::ClaudeCode => {
                steps.push(json!({"args":["plugin","marketplace","add","acme/tools"],"files":{".claude/plugins/known_marketplaces.json":"{\"tools\":{\"source\":{\"source\":\"github\",\"repo\":\"acme/tools\"}}}"}}));
                steps.push(json!({"args":["plugin","install","review@tools","--scope","user"],"files":{".claude/plugins/installed_plugins.json":record,".claude/plugins/cache/tools/review/1/.claude-plugin/plugin.json":"{\"name\":\"review\",\"version\":\"1\"}",".claude/plugins/cache/tools/review/1/hooks/hooks.json":"{\"hooks\":{\"PreToolUse\":[{},{}]}}",".claude/plugins/cache/tools/review/1/.mcp.json":"{\"mcpServers\":{\"review\":{}}}"}}));
                steps.push(json!({"args":["plugin","disable","review@tools","--scope","user"],"files":{".claude/settings.json":"{\"enabledPlugins\":{\"review@tools\":false},\"keep\":true}"}}));
            }
            ProviderKind::Codex => {
                steps.push(json!({"args":["plugin","marketplace","add","https://github.com/acme/tools.git"],"files":{".codex/config.toml":"# keep this comment\n[marketplaces.tools]\nsource_type='git'\nsource='https://github.com/acme/tools.git'\n"}}));
                steps.push(json!({"args":["plugin","add","review@tools","--json"],"files":{".codex/plugins/cache/tools/review/1/.claude-plugin/plugin.json":"{\"name\":\"review\",\"version\":\"1\"}"}}));
            }
            ProviderKind::Pi => {
                steps.push(json!({"args":["install","npm:pi-review@2.0.0"],"files":{".pi/agent/settings.json":"{\"packages\":[\"npm:pi-review@2.0.0\"],\"keep\":true}"}}));
            }
            ProviderKind::Omp => {
                steps.push(json!({"args":["plugin","install","omp-review@1.0.0"],"files":{".omp/plugins/package.json":"{\"dependencies\":{\"omp-review\":\"npm:omp-review@1.0.0\"}}",".omp/plugins/omp-plugins.lock.json":"{\"plugins\":{\"omp-review\":{\"version\":\"1.0.0\",\"enabled\":true}}}"}}));
                steps.push(json!({"args":["plugin","disable","omp-review"],"files":{".omp/plugins/omp-plugins.lock.json":"{\"plugins\":{\"omp-review\":{\"version\":\"1.0.0\",\"enabled\":false}}}"}}));
            }
            _ => unreachable!(),
        }
        let count = steps.len();
        put(&home, "steps.json", &json!(steps).to_string());
        let fake = dst.path().join("fake-success.py");
        kybern_protocol::process::write_executable(
            &fake,
            r#"#!/usr/bin/python3
import os,sys,json
from pathlib import Path
h=Path(os.environ['HOME'])
f=h/'steps.json'
steps=json.loads(f.read_text())
assert steps, 'Unexpected extra invocation'
step=steps.pop(0)
assert sys.argv[1:]==step['args'], (sys.argv[1:],step['args'])
for rel,body in step['files'].items():
 p=h/rel;p.parent.mkdir(parents=True,exist_ok=True);p.write_text(body)
f.write_text(json.dumps(steps))
with (h/'calls').open('a') as log:log.write('called\n')
"#,
        )
        .unwrap();
        state.plugin_runtime.binaries.write().unwrap().insert(provider, fake);
        let request = PluginInstallRequest { operation_id: uuid::Uuid::now_v7(), plugin: plugin.clone(), replaces: None };
        let result = install(&state, "source", request.clone()).await.unwrap();
        assert_eq!(result.state, PluginInstallState::Installed, "{provider:?}: {}", result.message);
        assert_eq!(result.actual.as_ref().unwrap().enabled, Some(false));
        assert_eq!(result.actual.as_ref().unwrap().fingerprint, plugin.fingerprint, "{provider:?}");
        install(&state, "source", request.clone()).await.unwrap();
        let again = install(&state, "source", PluginInstallRequest { operation_id: uuid::Uuid::now_v7(), ..request }).await.unwrap();
        assert_eq!(again.state, PluginInstallState::AlreadyThere);
        assert_eq!(std::fs::read_to_string(home.join("calls")).unwrap().lines().count(), count);
        if provider == ProviderKind::Codex {
            assert!(text(&home.join(".codex/config.toml")).unwrap().contains("# keep this comment"));
        }
    }
}
#[test]
fn codex_selects_the_active_version_and_package_pins_override_stale_cache() {
    let dir = TempDir::new("plugins-versions");
    let ctx = context(&dir);
    seed(&ctx);
    for v in ["1.9.0", "1.10.0"] {
        put(
            &ctx.home,
            &format!(".codex/plugins/cache/tools/review/{v}/.claude-plugin/plugin.json"),
            &format!("{{\"name\":\"review\",\"version\":\"{v}\"}}"),
        );
    }
    let get =
        || inventory::read(&ctx).plugins.into_iter().find(|p| p.provider == ProviderKind::Codex && p.kind == PluginKind::Plugin).unwrap();
    assert_eq!(get().version.as_deref(), Some("1.10.0"));
    put(&ctx.home, ".codex/plugins/cache/tools/review/local/.claude-plugin/plugin.json", r#"{"name":"review","version":"local"}"#);
    assert_eq!(get().version.as_deref(), Some("local"));
    put(&ctx.home, ".config/opencode/node_modules/@acme/review/package.json", r#"{"name":"@acme/review","version":"0.9.0"}"#);
    let p = inventory::read(&ctx).plugins.into_iter().find(|p| p.id == "@acme/review").unwrap();
    assert_eq!(p.version.as_deref(), Some("1.2.3"));
}
#[test]
fn malformed_configs_never_become_an_empty_inventory_or_leak_their_contents() {
    let dir = TempDir::new("plugin-bad-config");
    let ctx = context(&dir);
    seed(&ctx);
    put(&ctx.home, ".codex/config.toml", "token = 'never-send' broken");
    put(&ctx.home, ".config/opencode/opencode.jsonc", r#"{"plugin":"wrong-type"}"#);
    let inv = inventory::read(&ctx);
    assert!(inv.warnings.iter().any(|w| w.starts_with("codex:")));
    assert!(inv.warnings.iter().any(|w| w.starts_with("opencode:")));
    assert!(!serde_json::to_string(&inv).unwrap().contains("never-send"));
}

#[test]
fn credential_urls_never_leave_inventory_even_as_names_and_public_pi_git_is_copyable() {
    let dir = TempDir::new("plugin-credential-source");
    let ctx = context(&dir);
    for provider in [ProviderKind::Pi, ProviderKind::Opencode] {
        let (path, key) = if provider == ProviderKind::Pi {
            (".pi/agent/settings.json", "packages")
        } else {
            (".config/opencode/opencode.json", "plugin")
        };
        put(&ctx.home, path, &json!({key:["git:https://user:never-send@host/repo", "https://user:never-send@host/repo"]}).to_string());
    }
    let inv = inventory::read(&ctx);
    assert_eq!(inv.plugins.len(), 4);
    assert!(inv.plugins.iter().all(|p| p.source.is_none() && p.problem.is_some()));
    assert!(!serde_json::to_string(&inv).unwrap().contains("never-send"));
    put(&ctx.home, ".pi/agent/settings.json", r#"{"packages":["git:github.com/acme/review@v1"]}"#);
    let p = inventory::read(&ctx).plugins.into_iter().find(|p| p.provider == ProviderKind::Pi).unwrap();
    assert_eq!(p.source.as_deref(), Some("git:github.com/acme/review@v1"));
    install::validate(&p).unwrap();
}

#[test]
fn omp_git_and_alias_packages_are_not_replaced_with_an_unrelated_npm_package() {
    let dir = TempDir::new("plugin-omp-sources");
    let ctx = context(&dir);
    put(
        &ctx.home,
        ".omp/plugins/package.json",
        r#"{"dependencies":{"review":"git+https://example.com/private/review.git","alias":"npm:different-package@1.0.0","registry":"^1.0.0"}}"#,
    );
    put(
        &ctx.home,
        ".omp/plugins/omp-plugins.lock.json",
        r#"{"plugins":{"review":{"version":"1.0.0"},"alias":{"version":"1.0.0"},"registry":{"version":"1.0.0"}}}"#,
    );
    let inv = inventory::read(&ctx);
    for id in ["review", "alias"] {
        let p = inv.plugins.iter().find(|p| p.id == id).unwrap();
        assert!(p.source.is_none() && p.problem.is_some());
    }
    let p = inv.plugins.iter().find(|p| p.id == "registry").unwrap();
    assert_eq!(p.source.as_deref(), Some("registry@1.0.0"));
    install::validate(p).unwrap();
}

#[test]
fn refs_hosts_and_package_identities_cannot_smuggle_options_or_other_packages() {
    for bad in [
        "acme/tools#--upload-pack=x",
        "acme/tools@-x",
        "https://github.com/acme/tools.git#-x",
        "git@-oProxyCommand=x:repo",
        "acme/..",
        "acme/tools#v1@v2",
        "acme/tools#../../x",
    ] {
        assert!(!portable_source(bad), "{bad}");
    }
    for good in ["acme/tools@v1.2", "git@github.com:acme/tools.git#main", "https://github.com/acme/tools.git#release/1"] {
        assert!(portable_source(good), "{good}");
    }
    let dir = TempDir::new("plugin-identity");
    let ctx = context(&dir);
    seed(&ctx);
    let inv = inventory::read(&ctx);
    let pi = inv.plugins.iter().find(|p| p.id == "pi-review").unwrap().clone();
    let mut other = pi.clone();
    other.source = Some("npm:other-package@1.0.0".into());
    other.fingerprint = fingerprint(&other);
    assert!(install::validate(&other).is_err());
    // Pi reads an unprefixed source as a local path, so it is never an npm install.
    let mut bare = pi.clone();
    bare.source = Some("pi-review@2.0.0".into());
    bare.fingerprint = fingerprint(&bare);
    assert!(install::validate(&bare).is_err());
    let mut market = inv.plugins.iter().find(|p| p.id == "@acme/review").unwrap().clone();
    market.kind = PluginKind::Marketplace;
    market.source = Some("acme/tools".into());
    market.fingerprint = fingerprint(&market);
    assert!(install::validate(&market).is_err());
    put(&ctx.home, ".pi/agent/settings.json", r#"{"packages":["pi-review"]}"#);
    let p = inventory::read(&ctx).plugins.into_iter().find(|p| p.provider == ProviderKind::Pi).unwrap();
    assert!(p.source.is_none() && p.problem.is_some(), "{p:?}");
}

#[tokio::test]
async fn codex_plugins_setting_of_another_type_fails_without_panicking() {
    let dir = TempDir::new("plugin-codex-shape");
    let ctx = context(&dir);
    put(&ctx.home, ".codex/config.toml", "plugins = \"odd\"\n");
    let mut p = MachinePlugin {
        id: "review@tools".into(),
        provider: ProviderKind::Codex,
        kind: PluginKind::Plugin,
        name: "review".into(),
        marketplace: Some("tools".into()),
        source: Some("acme/tools".into()),
        version: None,
        enabled: Some(true),
        resources: BTreeMap::new(),
        contents: PluginContents::default(),
        problem: None,
        fingerprint: String::new(),
    };
    p.fingerprint = fingerprint(&p);
    assert!(install::codex_enabled(&ctx, &p).is_err());
    assert_eq!(text(&ctx.home.join(".codex/config.toml")).unwrap(), "plugins = \"odd\"\n");
}
