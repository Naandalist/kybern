use kybern_protocol::process::NoWindow;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use kybern_protocol::ProviderModel;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};

use crate::{DriverError, ProbeContext, Result};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);
const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Discovery {
    pub models: Vec<ProviderModel>,
}

/// Ask Pi for the effective model catalog in the supplied project and environment.
///
/// `--no-session` keeps this probe out of the user's session history. The process
/// lives in its own process group so cancellation and timeouts also stop descendants
/// launched by extensions.
pub(super) async fn discover(bin: &Path, context: &ProbeContext) -> Result<Discovery> {
    discover_with_timeout(bin, context, DISCOVERY_TIMEOUT).await
}

async fn discover_with_timeout(bin: &Path, context: &ProbeContext, timeout: Duration) -> Result<Discovery> {
    tokio::time::timeout(timeout, discover_inner(bin, context))
        .await
        .map_err(|_| DriverError::Protocol(format!("Pi model discovery timed out after {} milliseconds", timeout.as_millis())))?
}

async fn discover_inner(bin: &Path, context: &ProbeContext) -> Result<Discovery> {
    let mut command = Command::new(bin).no_window();
    // Preserve Pi's saved/default project-trust policy. In RPC mode an
    // undecided project is safely probed without loading its local resources.
    command.args(["--mode", "rpc", "--no-session"]);
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
    if let Some(cwd) = &context.cwd {
        command.current_dir(cwd);
    }
    for (key, value) in &context.env {
        command.env(key, value);
    }
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command.spawn()?;
    let tree = crate::process_tree::ProcessTree::new(child.id().expect("spawned Pi discovery process"));
    let mut stdin = child.stdin.take().ok_or_else(|| DriverError::Protocol("Pi RPC discovery did not provide stdin".into()))?;
    let stdout = child.stdout.take().ok_or_else(|| DriverError::Protocol("Pi RPC discovery did not provide stdout".into()))?;
    let mut lines = BufReader::new(stdout).lines();

    write_command(&mut stdin, &json!({ "type": "get_state" })).await?;
    let state = read_response(&mut lines, &mut stdin, "get_state").await?;
    write_command(&mut stdin, &json!({ "type": "get_available_models" })).await?;
    let available = read_response(&mut lines, &mut stdin, "get_available_models").await?;

    let discovery = parse_discovery(&state, &available)?;
    let _ = child.kill().await;
    drop(tree);
    Ok(discovery)
}

async fn write_command(stdin: &mut ChildStdin, command: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(command).map_err(|error| DriverError::Protocol(error.to_string()))?;
    bytes.push(b'\n');
    stdin.write_all(&bytes).await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_response(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    stdin: &mut ChildStdin,
    expected_command: &str,
) -> Result<Value> {
    loop {
        let line = lines
            .next_line()
            .await?
            .ok_or_else(|| DriverError::Protocol(format!("Pi RPC exited before responding to {expected_command}")))?;
        let frame: Value = serde_json::from_str(&line).map_err(|error| {
            DriverError::Protocol(format!("Pi RPC returned malformed JSON while waiting for {expected_command}: {error}"))
        })?;

        if frame.get("type").and_then(Value::as_str) == Some("extension_ui_request") {
            handle_extension_request(stdin, &frame).await?;
            continue;
        }
        if frame.get("type").and_then(Value::as_str) != Some("response")
            || frame.get("command").and_then(Value::as_str) != Some(expected_command)
        {
            continue;
        }
        if frame.get("success").and_then(Value::as_bool) != Some(true) {
            let detail = frame
                .get("error")
                .and_then(|error| error.as_str().or_else(|| error.get("message").and_then(Value::as_str)))
                .unwrap_or("unknown error");
            return Err(DriverError::Protocol(format!("Pi RPC {expected_command} failed: {detail}")));
        }
        return frame.get("data").cloned().ok_or_else(|| DriverError::Protocol(format!("Pi RPC {expected_command} response omitted data")));
    }
}

async fn handle_extension_request(stdin: &mut ChildStdin, frame: &Value) -> Result<()> {
    let Some(method) = frame.get("method").and_then(Value::as_str) else {
        return Ok(());
    };
    if !matches!(method, "select" | "confirm" | "input" | "editor") {
        return Ok(());
    }
    let id = frame
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| DriverError::Protocol(format!("Pi extension {method} request omitted its id")))?;
    write_command(stdin, &json!({ "type": "extension_ui_response", "id": id, "cancelled": true })).await?;
    let title = frame.get("title").and_then(Value::as_str).unwrap_or(method);
    Err(DriverError::Protocol(format!("Pi model discovery was cancelled because extension startup requested {method}: {title}")))
}

fn parse_discovery(state: &Value, available: &Value) -> Result<Discovery> {
    let default_model = state.get("model").and_then(model_selector);
    let configured_effort =
        state.get("thinkingLevel").and_then(Value::as_str).filter(|level| THINKING_LEVELS.contains(level)).map(str::to_string);
    let items = available
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| DriverError::Protocol("Pi RPC get_available_models response omitted models".into()))?;

    let mut seen = std::collections::HashSet::new();
    let mut models = Vec::new();
    for item in items {
        let Some(model) = parse_model(item, default_model.as_deref(), configured_effort.as_deref()) else {
            continue;
        };
        if !seen.insert(model.id.clone()) {
            continue;
        }
        models.push(model);
    }
    // A selected custom model can be omitted by an older catalog endpoint.
    // get_state returns the same full Model shape, so retain it as the native
    // default instead of leaving the catalog without a usable fallback.
    if !models.iter().any(|model| model.is_default)
        && let Some(model) = state.get("model").and_then(|model| parse_model(model, default_model.as_deref(), configured_effort.as_deref()))
        && seen.insert(model.id.clone())
    {
        models.push(model);
    }
    models.sort_by(|a, b| a.provider.cmp(&b.provider).then_with(|| a.display_name.cmp(&b.display_name)).then_with(|| a.id.cmp(&b.id)));

    Ok(Discovery { models })
}

fn parse_model(model: &Value, default_model: Option<&str>, configured_effort: Option<&str>) -> Option<ProviderModel> {
    let id = model_selector(model)?;
    let provider = model.get("provider").and_then(Value::as_str).map(str::trim).filter(|provider| !provider.is_empty()).map(str::to_string);
    let display_name = model.get("name").and_then(Value::as_str).map(str::trim).filter(|name| !name.is_empty()).unwrap_or(&id).to_string();
    let efforts = model_supported_efforts(model);
    let default_effort = effective_effort(configured_effort, &efforts);
    Some(ProviderModel {
        display_name,
        is_default: default_model == Some(id.as_str()),
        parameters: Vec::new(),
        variants: Vec::new(),
        id,
        resolved_id: None,
        description: None,
        provider,
        efforts,
        default_effort,
    })
}

fn model_selector(model: &Value) -> Option<String> {
    let provider = model.get("provider").and_then(Value::as_str)?.trim();
    let id = model.get("id").and_then(Value::as_str)?.trim();
    if provider.is_empty() || id.is_empty() {
        return None;
    }
    Some(format!("{provider}/{id}"))
}

/// Mirror Pi's advertised thinking ladder without guessing provider-specific
/// values. Base levels are available unless explicitly disabled with `null`;
/// `xhigh` and `max` must be explicitly enabled by the model's map.
pub(super) fn model_supported_efforts(model: &Value) -> Vec<String> {
    if model.get("reasoning").and_then(Value::as_bool) != Some(true) {
        return Vec::new();
    }
    let level_map = model.get("thinkingLevelMap").and_then(Value::as_object);
    THINKING_LEVELS
        .into_iter()
        .filter(|level| {
            let mapped = level_map.and_then(|map| map.get(*level));
            if mapped == Some(&Value::Null) {
                return false;
            }
            !matches!(*level, "xhigh" | "max") || mapped.is_some()
        })
        .map(str::to_string)
        .collect()
}

/// Clamp a configured Pi thinking level exactly as Pi does when a model omits
/// it: prefer the next supported higher level, then the nearest lower level.
pub(super) fn effective_effort(requested: Option<&str>, supported: &[String]) -> Option<String> {
    if supported.is_empty() {
        return None;
    }
    let requested = requested?;
    let index = THINKING_LEVELS.iter().position(|level| *level == requested)?;
    if supported.iter().any(|level| level == requested) {
        return Some(requested.to_string());
    }
    THINKING_LEVELS[index + 1..]
        .iter()
        .chain(THINKING_LEVELS[..index].iter().rev())
        .find(|candidate| supported.iter().any(|level| level == **candidate))
        .map(|level| (*level).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_models_defaults_and_actual_thinking_capabilities() {
        let state = json!({
            "model": { "provider": "custom", "id": "deep-model" },
            "thinkingLevel": "max"
        });
        let available = json!({ "models": [
            {
                "provider": "plain",
                "id": "chat-only",
                "name": "Chat only",
                "reasoning": false,
                "thinkingLevelMap": { "max": "max" }
            },
            {
                "provider": "custom",
                "id": "deep-model",
                "name": "Locally configured Deep Model",
                "reasoning": true,
                "thinkingLevelMap": {
                    "off": null,
                    "minimal": null,
                    "medium": null,
                    "xhigh": "extra_high",
                    "max": null
                },
                "customProviderField": true
            },
            { "provider": "custom", "id": "deep-model", "name": "duplicate" },
            { "provider": "broken" },
            "malformed"
        ] });

        let discovery = parse_discovery(&state, &available).unwrap();
        assert_eq!(discovery.models.len(), 2);
        let custom = discovery.models.iter().find(|model| model.id == "custom/deep-model").unwrap();
        assert_eq!(custom.display_name, "Locally configured Deep Model");
        assert_eq!(custom.efforts, ["low", "high", "xhigh"]);
        assert_eq!(custom.default_effort.as_deref(), Some("xhigh"));
        assert!(custom.is_default);
        let plain = discovery.models.iter().find(|model| model.id == "plain/chat-only").unwrap();
        assert!(plain.efforts.is_empty());
        assert_eq!(plain.default_effort, None);
    }

    #[test]
    fn thinking_map_defaults_base_levels_and_can_disable_each_one() {
        assert_eq!(model_supported_efforts(&json!({ "reasoning": true })), ["off", "minimal", "low", "medium", "high"]);
        assert_eq!(
            model_supported_efforts(&json!({
                "reasoning": true,
                "thinkingLevelMap": { "off": null, "low": null, "xhigh": "x", "max": "m" }
            })),
            ["minimal", "medium", "high", "xhigh", "max"]
        );
        assert!(model_supported_efforts(&json!({ "reasoning": false })).is_empty());
        assert!(model_supported_efforts(&json!({ "reasoning": "yes" })).is_empty());
    }

    #[test]
    fn malformed_catalog_is_a_discovery_error() {
        let error = parse_discovery(&json!({}), &json!({ "models": "nope" })).unwrap_err();
        assert!(error.to_string().contains("omitted models"));
    }

    #[test]
    fn retains_the_configured_model_when_the_catalog_omits_it() {
        let state = json!({
            "model": {
                "provider": "local",
                "id": "custom-model",
                "name": "Custom model",
                "reasoning": true,
                "thinkingLevelMap": { "medium": null }
            },
            "thinkingLevel": "medium"
        });
        let discovery = parse_discovery(&state, &json!({ "models": [] })).unwrap();

        assert_eq!(discovery.models.len(), 1);
        assert_eq!(discovery.models[0].id, "local/custom-model");
        assert!(discovery.models[0].is_default);
        assert_eq!(discovery.models[0].default_effort.as_deref(), Some("high"));
    }

    #[cfg(unix)]
    fn executable_script(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fake-pi");
        kybern_protocol::process::write_executable(&path, contents).unwrap();
        (directory, path)
    }

    /// Linux refuses to run a file that another test's freshly forked child
    /// still holds open for writing (ETXTBSY) until that child execs. Retry.
    #[cfg(unix)]
    async fn discover_fixture(binary: &std::path::Path, context: &ProbeContext) -> Result<Discovery> {
        for _ in 0..50 {
            match discover(binary, context).await {
                Err(error) if error.to_string().contains("Text file busy") => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await
                }
                result => return result,
            }
        }
        discover(binary, context).await
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rpc_probe_is_ephemeral_and_uses_project_context() {
        let (directory, binary) = executable_script(
            r###"#!/bin/sh
[ "$1" = "--mode" ] && [ "$2" = "rpc" ] && [ "$3" = "--no-session" ] && [ "$#" = 3 ] || exit 9
IFS= read -r state_request
[ "$state_request" = '{"type":"get_state"}' ] || exit 6
printf '%s\n' '{"type":"response","command":"get_state","success":true,"data":{"model":{"provider":"custom","id":"from-context"},"thinkingLevel":"medium"}}'
IFS= read -r models_request
[ "$models_request" = '{"type":"get_available_models"}' ] || exit 5
printf '{"type":"response","command":"get_available_models","success":true,"data":{"models":[{"provider":"custom","id":"%s","name":"%s","reasoning":true}]}}\n' "$PI_DISCOVERY_FIXTURE" "$(pwd)"
"###,
        );
        let mut context = ProbeContext { cwd: Some(directory.path().to_path_buf()), ..ProbeContext::default() };
        context.env.insert("PI_DISCOVERY_FIXTURE".into(), "from-context".into());

        let discovery = discover_fixture(&binary, &context).await.unwrap();
        assert_eq!(discovery.models.len(), 1);
        assert_eq!(discovery.models[0].id, "custom/from-context");
        assert_eq!(discovery.models[0].display_name, directory.path().canonicalize().unwrap().display().to_string());
        assert!(discovery.models[0].is_default);
        assert_eq!(discovery.models[0].default_effort.as_deref(), Some("medium"));
        assert!(!directory.path().join("session.jsonl").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_dialog_is_cancelled_without_approval_and_reported() {
        let (_directory, binary) = executable_script(
            r###"#!/bin/sh
IFS= read -r state_request
printf '%s\n' '{"type":"extension_ui_request","id":"startup-1","method":"confirm","title":"Trust extension?"}'
IFS= read -r answer
case "$answer" in *'"type":"extension_ui_response"'*'"id":"startup-1"'*'"cancelled":true'*) exit 0 ;; *) exit 4 ;; esac
"###,
        );

        let error = discover_fixture(&binary, &ProbeContext::default()).await.unwrap_err();
        assert!(error.to_string().contains("Trust extension?"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn malformed_rpc_frame_is_reported() {
        let (_directory, binary) = executable_script(
            r###"#!/bin/sh
IFS= read -r state_request
printf '%s\n' 'not-json'
"###,
        );

        let error = discover_fixture(&binary, &ProbeContext::default()).await.unwrap_err();
        assert!(error.to_string().contains("malformed JSON"));
        assert!(error.to_string().contains("get_state"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_extension_descendants() {
        let (directory, binary) = executable_script(
            r###"#!/bin/sh
(sleep 0.3; touch escaped) &
sleep 10
"###,
        );
        let context = ProbeContext { cwd: Some(directory.path().to_path_buf()), ..ProbeContext::default() };

        let error = discover_with_timeout(&binary, &context, Duration::from_millis(40)).await.unwrap_err();
        assert!(error.to_string().contains("timed out after 40 milliseconds"));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!directory.path().join("escaped").exists());
    }
}
