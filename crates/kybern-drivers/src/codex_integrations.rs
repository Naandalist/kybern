//! Codex app-server owns its plugin catalog, installation and config policy.
use crate::{DriverError, ProbeContext, Result, ndjson::NdjsonChild};
use kybern_protocol::process::NoWindow;
use kybern_protocol::{
    Integration, IntegrationAction as Action, IntegrationChangeResult, IntegrationKind, IntegrationsCatalog, ProviderKind,
};
use serde_json::{Value, json};
use std::time::Duration;

async fn call(child: &NdjsonChild, id: i64, method: &str, params: Value) -> Result<Value> {
    child.write(&json!({"id":id,"method":method,"params":params})).await?;
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let frame = child
                .lines
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| DriverError::Protocol("Codex closed its connection. Refresh and try again.".into()))?;
            if frame.get("id").and_then(Value::as_i64) == Some(id) && frame.get("method").is_none() {
                if let Some(result) = frame.get("result") {
                    return Ok(result.clone());
                }
                return Err(DriverError::Protocol(
                    frame
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("Codex rejected this request.")
                        .chars()
                        .take(2000)
                        .collect(),
                ));
            }
            if let (Some(request), Some(_)) = (frame.get("id"), frame.get("method")) {
                child
                    .write(&json!({"id":request,"error":{"code":-32601,"message":"This operation requires an interactive agent session."}}))
                    .await?;
            }
        }
    })
    .await
    .map_err(|_| DriverError::Protocol("Codex took too long. Refresh its catalog before retrying.".into()))?
}

async fn open(context: &ProbeContext) -> Result<NdjsonChild> {
    let bin = crate::binary::resolve(ProviderKind::Codex, context.binary.as_ref())?;
    let mut command = crate::binary::command(bin).no_window();
    command.arg("app-server").envs(&context.env);
    if let Some(cwd) = &context.cwd {
        command.current_dir(cwd);
    }
    let child = NdjsonChild::spawn(command)?;
    let result = call(&child, 1, "initialize", json!({"clientInfo":{"name":"kybern","title":"Kybern","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}})).await;
    if let Err(error) = result {
        child.kill().await;
        return Err(error);
    }
    child.write(&json!({"method":"initialized"})).await?;
    Ok(child)
}

async fn plugins(child: &NdjsonChild, context: &ProbeContext, id: i64) -> Result<Value> {
    call(child, id, "plugin/list", json!({"cwds":context.cwd.as_ref().map(|p|vec![p]),"forceRefetch":false})).await
}

pub async fn list(context: &ProbeContext) -> Result<IntegrationsCatalog> {
    let child = open(context).await?;
    let mut catalog = IntegrationsCatalog::default();
    match plugins(&child, context, 2).await {
        Ok(value) => {
            catalog.items.extend(parse_plugins(&value));
            if value.get("marketplaceLoadErrors").and_then(Value::as_array).is_some_and(|errors| !errors.is_empty()) {
                catalog.warnings.push("Some plugin marketplaces could not be loaded. Refresh to retry.".into());
            }
        }
        Err(error) => catalog.warnings.push(format!("Plugins: {error}")),
    }
    let mut cursor = Value::Null;
    for page in 0..10 {
        match call(&child, 10 + page, "app/list", json!({"cursor":cursor,"limit":100,"forceRefetch":false})).await {
            Ok(value) => {
                catalog.items.extend(value.get("data").and_then(Value::as_array).into_iter().flatten().filter_map(parse_app));
                cursor = value.get("nextCursor").cloned().unwrap_or(Value::Null);
                if cursor.is_null() {
                    break;
                }
                if page == 9 {
                    catalog.warnings.push("The connector catalog is larger than 1,000 entries; only the first pages are shown.".into());
                }
            }
            Err(error) => {
                catalog.warnings.push(format!("Connectors: {error}"));
                break;
            }
        }
    }
    child.kill().await;
    Ok(catalog)
}

pub async fn change(context: &ProbeContext, id: &str, kind: IntegrationKind, action: Action) -> Result<IntegrationChangeResult> {
    let child = open(context).await?;
    let result = async {
        if kind == IntegrationKind::Connector {
            // Only exact provider-issued app ids may select a config subtree.
            if !id.starts_with("app-") || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') || id.len() > 100 {
                return Err(DriverError::Protocol("Invalid connector id. Refresh and try again.".into()));
            }
            if !matches!(action, Action::Enable | Action::Disable) { return Err(DriverError::Unsupported("Use the provider's setup page to connect this app.".into())); }
            let mut cursor = Value::Null;
            let mut allowed = false;
            for page in 0..10 {
                let value = call(&child, 10 + page, "app/list", json!({"cursor":cursor,"limit":100})).await?;
                if let Some(item) = value.get("data").and_then(Value::as_array).into_iter().flatten().filter_map(parse_app).find(|app|app.id == id) {
                    allowed = item.actions.contains(&action);
                    break;
                }
                cursor = value.get("nextCursor").cloned().unwrap_or(Value::Null);
                if cursor.is_null() { break; }
            }
            if !allowed { return Err(DriverError::Unsupported("This action is not available for this connector. Refresh its catalog.".into())); }
            call(&child, 2, "config/value/write", json!({"keyPath":format!("apps.{}.enabled",serde_json::to_string(id).unwrap()),"value":action == Action::Enable,"mergeStrategy":"replace"})).await?;
            return Ok(IntegrationChangeResult { message:"Connector updated. Start a new agent session to apply it.".into(), connections:vec![] });
        }
        let value = plugins(&child, context, 2).await?;
        let item = parse_plugins(&value).into_iter().find(|p|p.id == id).ok_or_else(|| DriverError::Protocol("This plugin is no longer in the catalog. Refresh and try again.".into()))?;
        if !item.actions.contains(&action) { return Err(DriverError::Unsupported("This action is not available for this plugin.".into())); }
        let (market, plugin) = value.get("marketplaces").and_then(Value::as_array).into_iter().flatten()
            .flat_map(|m|m.get("plugins").and_then(Value::as_array).into_iter().flatten().map(move |p|(m,p)))
            .find(|(_,p)|p.get("id").and_then(Value::as_str)==Some(id)).unwrap();
        let response = match action {
            Action::Install => call(&child, 3, "plugin/install", json!({"pluginName":plugin["name"],"marketplacePath":market.get("path"),"remoteMarketplaceName":if market.get("path").is_none_or(Value::is_null){market.get("name")}else{None},"installAttemptId":uuid::Uuid::now_v7().to_string()})).await?,
            Action::Uninstall => call(&child, 3, "plugin/uninstall", json!({"pluginId":id})).await?,
            Action::Enable | Action::Disable => call(&child, 3, "config/value/write", json!({"keyPath":format!("plugins.{}.enabled",serde_json::to_string(id).unwrap()),"value":action==Action::Enable,"mergeStrategy":"replace"})).await?,
            Action::Update => return Err(DriverError::Unsupported("Codex owns plugin updates.".into())),
        };
        let connections = response.get("appsNeedingAuth").and_then(Value::as_array).into_iter().flatten().filter_map(parse_app).collect();
        Ok(IntegrationChangeResult {message:"Plugin updated. Start a new agent session to apply it.".into(),connections})
    }.await;
    child.kill().await;
    result
}

fn https_url(value: Option<&Value>) -> Option<String> {
    let value = value?.as_str()?;
    let url = reqwest::Url::parse(value).ok()?;
    (url.scheme() == "https" && url.host_str().is_some() && url.username().is_empty() && url.password().is_none()).then(|| value.into())
}

fn parse_app(app: &Value) -> Option<Integration> {
    let id = app.get("id")?.as_str()?;
    let accessible = app.get("isAccessible").and_then(Value::as_bool).unwrap_or(false);
    let enabled = app.get("isEnabled").and_then(Value::as_bool).unwrap_or(true);
    Some(Integration {
        id: id.into(),
        name: app.get("name")?.as_str()?.into(),
        kind: IntegrationKind::Connector,
        description: app.get("description").and_then(Value::as_str).map(str::to_string),
        scope: None,
        installed: accessible,
        enabled,
        status: if !accessible {
            "Not connected"
        } else if enabled {
            "Connected"
        } else {
            "Disabled"
        }
        .into(),
        actions: if accessible { vec![if enabled { Action::Disable } else { Action::Enable }] } else { vec![] },
        connect_url: https_url(app.get("installUrl")),
        can_login: false,
    })
}

pub fn parse_plugins(value: &Value) -> Vec<Integration> {
    value
        .get("marketplaces")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|m| m.get("plugins").and_then(Value::as_array).into_iter().flatten())
        .filter_map(|plugin| {
            let id = plugin.get("id")?.as_str()?;
            let installed = plugin.get("installed").and_then(Value::as_bool).unwrap_or(false);
            let enabled = plugin.get("enabled").and_then(Value::as_bool).unwrap_or(false);
            let blocked = plugin.get("availability").and_then(Value::as_str) == Some("DISABLED_BY_ADMIN")
                || plugin.get("disabledReason").is_some_and(|v| !v.is_null());
            let mut actions = vec![];
            if installed && !blocked {
                actions.extend([if enabled { Action::Disable } else { Action::Enable }, Action::Uninstall]);
            } else if !installed && !blocked && plugin.get("installPolicy").and_then(Value::as_str) != Some("NOT_AVAILABLE") {
                actions.push(Action::Install);
            }
            // Required installation interstitials are provider-owned consent,
            // not a dialog Kybern can silently skip.
            if plugin.get("mustShowInstallationInterstitial").and_then(Value::as_bool) == Some(true) {
                actions.retain(|a| *a != Action::Install);
            }
            Some(Integration {
                id: id.into(),
                name: plugin
                    .pointer("/interface/displayName")
                    .and_then(Value::as_str)
                    .or_else(|| plugin.get("name").and_then(Value::as_str))
                    .unwrap_or(id)
                    .into(),
                kind: IntegrationKind::Plugin,
                description: plugin.pointer("/interface/shortDescription").and_then(Value::as_str).map(str::to_string),
                scope: None,
                installed,
                enabled,
                status: if blocked {
                    "Unavailable for this account"
                } else if installed && enabled {
                    "Enabled"
                } else if installed {
                    "Disabled"
                } else if actions.is_empty() {
                    "Install in Codex"
                } else {
                    "Available"
                }
                .into(),
                actions,
                connect_url: None,
                can_login: false,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_respects_provider_restrictions_and_includes_nameless_interfaces() {
        let value = json!({"marketplaces":[{"plugins":[{"id":"one@m","name":"one","installed":true,"enabled":false},{"id":"two@m","name":"two","installed":false,"availability":"DISABLED_BY_ADMIN"},{"id":"three@m","name":"three","mustShowInstallationInterstitial":true}]}]});
        let items = parse_plugins(&value);
        assert_eq!(items[0].name, "one");
        assert_eq!(items[0].actions[0], Action::Enable);
        assert!(items[1].actions.is_empty());
        assert!(items[2].actions.is_empty());
    }
    #[test]
    fn connection_links_cannot_launch_local_protocols_or_carry_basic_auth() {
        for url in ["javascript:alert(1)", "file:///etc/passwd", "https://user:secret@example.com/"] {
            assert!(parse_app(&json!({"id":"app-one","name":"One","installUrl":url})).unwrap().connect_url.is_none());
        }
        assert!(parse_app(&json!({"id":"app-one","name":"One","installUrl":"https://example.com/connect"})).unwrap().connect_url.is_some());
    }
}
