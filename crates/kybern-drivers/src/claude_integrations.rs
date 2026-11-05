//! Claude's CLI owns installation scopes, dependency checks and OAuth storage.
use crate::{DriverError, ProbeContext, Result};
use kybern_protocol::process::NoWindow;
use kybern_protocol::{
    Integration, IntegrationAction as Action, IntegrationChangeResult, IntegrationKind, IntegrationsCatalog, ProviderKind,
};
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;

async fn run(context: &ProbeContext, args: &[&str]) -> Result<String> {
    let bin = crate::binary::resolve(ProviderKind::ClaudeCode, context.binary.as_ref())?;
    let mut command = crate::binary::command(bin).no_window();
    command.args(args).env_remove("NODE_OPTIONS").envs(&context.env);
    if let Some(cwd) = &context.cwd {
        command.current_dir(cwd);
    }
    let output = tokio::time::timeout(Duration::from_secs(90), crate::process_tree::output(&mut command))
        .await
        .map_err(|_| DriverError::Protocol("Claude took too long. Refresh the catalog before retrying.".into()))??;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().chars().take(2000).collect::<String>();
        return Err(DriverError::Protocol(if message.is_empty() {
            "Claude could not complete this command. Finish any required setup in Claude Code, then refresh.".into()
        } else {
            message
        }));
    }
    if output.stdout.len() > 8 * 1024 * 1024 {
        return Err(DriverError::Protocol("Claude's catalog is too large to load.".into()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub async fn list(context: &ProbeContext) -> Result<IntegrationsCatalog> {
    let (plugins, connectors) = tokio::join!(run(context, &["plugin", "list", "--available", "--json"]), run(context, &["mcp", "list"]));
    let mut catalog = IntegrationsCatalog::default();
    match plugins {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(value) => catalog.items.extend(parse_plugins(&value)),
            Err(_) => catalog.warnings.push("Claude returned an unreadable plugin catalog. Update Claude Code and refresh.".into()),
        },
        Err(error) => catalog.warnings.push(format!("Plugins: {error}")),
    }
    match connectors {
        Ok(text) => catalog.items.extend(parse_connectors(&text)),
        Err(_) => catalog.warnings.push("Connections could not be checked. Refresh or check Claude Code on the computer.".into()),
    }
    Ok(catalog)
}

/// Enabled installation roots from Claude's effective catalog, including project scopes.
pub async fn skill_roots(context: &ProbeContext) -> Result<Vec<(String, std::path::PathBuf)>> {
    let text = run(context, &["plugin", "list", "--json"]).await?;
    let value: Value = serde_json::from_str(&text).map_err(|e| DriverError::Protocol(e.to_string()))?;
    Ok(value
        .get("installed")
        .and_then(Value::as_array)
        .or_else(|| value.as_array())
        .into_iter()
        .flatten()
        .filter(|p| p.get("enabled").and_then(Value::as_bool) != Some(false))
        .filter_map(|p| {
            let id = p.get("id")?.as_str()?;
            let path = p.get("installPath")?.as_str()?;
            Some((id.split('@').next()?.to_string(), std::path::PathBuf::from(path)))
        })
        .collect())
}

pub async fn change(context: &ProbeContext, id: &str, scope: Option<&str>, action: Action) -> Result<IntegrationChangeResult> {
    let catalog = list(context).await?;
    let item = catalog
        .items
        .iter()
        .find(|item| item.id == id && item.scope.as_deref() == scope && item.kind == IntegrationKind::Plugin)
        .ok_or_else(|| DriverError::Protocol("This plugin is no longer in the catalog. Refresh and try again.".into()))?;
    if !item.actions.contains(&action) {
        return Err(DriverError::Unsupported("This action is not available for this plugin.".into()));
    }
    let verb = match action {
        Action::Install => "install",
        Action::Uninstall => "uninstall",
        Action::Enable => "enable",
        Action::Disable => "disable",
        Action::Update => "update",
    };
    // Do not pass --yes: a marketplace command installer must retain Claude's
    // additional consent. Standard archive/git plugins work without bypassing it.
    run(context, &["plugin", verb, "--scope", scope.unwrap_or("user"), "--", id]).await?;
    Ok(IntegrationChangeResult { message: "Plugin updated. Start a new agent session to apply the change.".into(), connections: vec![] })
}

pub fn parse_plugins(value: &Value) -> Vec<Integration> {
    let available = value.get("available").and_then(Value::as_array);
    let installed = value.get("installed").and_then(Value::as_array).or_else(|| value.as_array());
    let mut out = Vec::new();
    let mut ids = HashSet::new();
    for plugin in installed.into_iter().flatten() {
        let Some(id) = plugin.get("id").and_then(Value::as_str) else { continue };
        let scope = plugin.get("scope").and_then(Value::as_str).unwrap_or("user");
        if !ids.insert((id.to_string(), scope.to_string())) {
            continue;
        }
        let enabled = plugin.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        let meta = available.into_iter().flatten().find(|p| p.get("pluginId").and_then(Value::as_str) == Some(id));
        let editable = matches!(scope, "user" | "project" | "local");
        out.push(Integration {
            id: id.into(),
            name: meta.and_then(|p| p.get("name")).and_then(Value::as_str).unwrap_or(id).into(),
            kind: IntegrationKind::Plugin,
            description: meta.and_then(|p| p.get("description")).and_then(Value::as_str).map(str::to_string),
            scope: Some(scope.into()),
            installed: true,
            enabled,
            status: if enabled { "Enabled" } else { "Disabled" }.into(),
            actions: if editable {
                vec![if enabled { Action::Disable } else { Action::Enable }, Action::Update, Action::Uninstall]
            } else {
                vec![]
            },
            connect_url: None,
            can_login: false,
        });
    }
    for plugin in available.into_iter().flatten() {
        let Some(id) = plugin.get("pluginId").and_then(Value::as_str) else { continue };
        if ids.iter().any(|(installed, _)| installed == id) {
            continue;
        }
        ids.insert((id.into(), String::new()));
        out.push(Integration {
            id: id.into(),
            name: plugin.get("name").and_then(Value::as_str).unwrap_or(id).into(),
            kind: IntegrationKind::Plugin,
            description: plugin.get("description").and_then(Value::as_str).map(str::to_string),
            scope: None,
            installed: false,
            enabled: false,
            status: "Available".into(),
            actions: vec![Action::Install],
            connect_url: None,
            can_login: false,
        });
    }
    out
}

pub fn parse_connectors(text: &str) -> Vec<Integration> {
    // The CLI has no JSON list flag. Match only its status suffixes, discard
    // command lines/URLs and all other output so configured headers stay private.
    text.lines()
        .filter_map(|line| {
            let (name, details) = line.split_once(": ")?;
            let status = if details.contains("✔ Connected") {
                "Connected"
            } else if details.contains("Needs authentication") {
                "Sign in required"
            } else if details.contains("Failed to connect") || details.contains("✘") {
                "Connection failed"
            } else if details.contains("Pending approval") {
                "Approval required"
            } else if details.contains("Disabled") {
                "Disabled"
            } else {
                return None;
            };
            if name.starts_with('[') || name.len() > 256 {
                return None;
            }
            Some(Integration {
                id: name.into(),
                name: name.strip_prefix("claude.ai ").unwrap_or(name).into(),
                kind: IntegrationKind::Connector,
                description: Some(if name.starts_with("claude.ai ") { "Claude account connector" } else { "MCP server" }.into()),
                scope: None,
                installed: true,
                enabled: status != "Disabled",
                status: status.into(),
                actions: vec![],
                connect_url: None,
                can_login: status == "Sign in required",
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn native_scopes_disabled_plugins_and_available_entries_are_preserved() {
        let items = parse_plugins(
            &json!({"installed":[{"id":"a@market","scope":"user","enabled":false},{"id":"a@market","scope":"project","enabled":true}],"available":[{"pluginId":"a@market","name":"A","description":"Works"},{"pluginId":"b@market","name":"B"}]}),
        );
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].actions[0], Action::Enable);
        assert_eq!(items[1].scope.as_deref(), Some("project"));
        assert_eq!(items[2].actions, vec![Action::Install]);
    }
    #[test]
    fn connector_catalog_never_returns_urls_commands_or_headers() {
        let text = "Checking MCP server health…\nclaude.ai Drive: https://example.test?secret=hidden - ✔ Connected\nplugin:x:y: https://example.test - ! Needs authentication\ncustom: command --token hidden - ✘ Failed to connect\n[mcp-sdk] warning: Needs authentication";
        let items = parse_connectors(text);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].name, "Drive");
        assert!(items[1].can_login);
        assert!(!serde_json::to_string(&items).unwrap().contains("hidden"));
    }
}

#[cfg(all(test, unix))]
mod native_command_tests {
    use super::*;

    #[tokio::test]
    async fn mutations_preserve_scope_and_never_bypass_installer_consent() {
        let root = std::env::temp_dir().join(format!("kybern-claude-integrations-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&root).unwrap();
        let binary = root.join("claude");
        kybern_protocol::process::write_executable(
            &binary,
            r#"#!/bin/sh
if [ "$1" = plugin ] && [ "$2" = list ]; then
  printf '%s\n' '{"installed":[{"id":"one@fixture","scope":"project","enabled":true}],"available":[]}'
elif [ "$1" = mcp ]; then
  printf '%s\n' 'Checking MCP server health…'
else
  printf '%s\n' "$@" > "$MOCK_ARGS"
fi
"#,
        )
        .unwrap();
        let args = root.join("args");
        let context = ProbeContext {
            binary: Some(binary),
            cwd: Some(root.clone()),
            env: [("MOCK_ARGS".into(), args.to_string_lossy().into_owned())].into(),
        };
        change(&context, "one@fixture", Some("project"), Action::Disable).await.unwrap();
        assert_eq!(std::fs::read_to_string(&args).unwrap(), "plugin\ndisable\n--scope\nproject\n--\none@fixture\n");
        assert!(change(&context, "one@fixture", Some("user"), Action::Disable).await.is_err());
        assert!(change(&context, "missing@fixture", None, Action::Install).await.is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
