//! `agy` tool names and parameters onto the names the shared client already
//! classifies (`toolActivity.ts`) and `edit_paths.rs` understands.
//!
//! `tool_info.parameters` is thin (write and edit carry only `Path`), and the
//! key names beyond `Path` and `CommandLine` were not all seen, so they are read
//! case-insensitively with fallbacks. The original parameters stay under
//! `input.raw`.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// The value under the first of `names`, ignoring case and underscores.
fn lookup<'a>(params: &'a Value, names: &[&str]) -> Option<&'a Value> {
    let object = params.as_object()?;
    let squash = |key: &str| key.chars().filter(|c| *c != '_').flat_map(char::to_lowercase).collect::<String>();
    names.iter().find_map(|name| {
        let wanted = squash(name);
        object.iter().find(|(key, value)| squash(key) == wanted && !value.is_null()).map(|(_, value)| value)
    })
}

fn text(params: &Value, names: &[&str]) -> Option<String> {
    lookup(params, names).and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty()).map(str::to_string)
}

const FILE_KEYS: [&str; 7] = ["Path", "AbsolutePath", "file_path", "FilePath", "TargetFile", "File", "FileName"];

fn put(input: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        input.insert(key.to_string(), Value::String(value));
    }
}

/// The Kybern name and input for one agy tool call. `fallback` is asked for the
/// transcript's arguments when the step carries too little to show the call.
pub(crate) fn normalize(agy_name: &str, params: &Value, fallback: impl Fn(&str) -> Option<Value>) -> (String, Value) {
    let (name, mut input) = map(agy_name, params);
    if let Some(key) = required_key(&name)
        && input.get(key).is_none()
        && let Some(args) = fallback(agy_name)
    {
        let (_, mapped) = map(agy_name, &args);
        if mapped.get(key).is_some() {
            return (name, mapped);
        }
        input = mapped;
    }
    (name, input)
}

/// The key a card needs to say what the call did.
fn required_key(kybern_name: &str) -> Option<&'static str> {
    match kybern_name {
        "Bash" => Some("command"),
        "Read" | "Write" | "Edit" => Some("file_path"),
        "Grep" | "Glob" => Some("pattern"),
        "LS" => Some("path"),
        "WebSearch" => Some("query"),
        "WebFetch" => Some("url"),
        _ => None,
    }
}

fn map(agy_name: &str, params: &Value) -> (String, Value) {
    let mut input = Map::new();
    let name = match agy_name {
        "run_command" => {
            put(&mut input, "command", text(params, &["CommandLine", "Command", "command_line"]));
            put(&mut input, "cwd", text(params, &["Cwd", "WorkingDirectory", "Dir"]));
            "Bash".to_string()
        }
        "read_file" | "view_file" => {
            put(&mut input, "file_path", text(params, &FILE_KEYS));
            "Read".into()
        }
        "write_file" | "write_to_file" => {
            put(&mut input, "file_path", text(params, &FILE_KEYS));
            "Write".into()
        }
        "edit_file" | "replace_file_content" | "multi_replace_file_content" | "sed_file" => {
            put(&mut input, "file_path", text(params, &FILE_KEYS));
            "Edit".into()
        }
        "grep_search" => {
            put(&mut input, "pattern", text(params, &["Query", "Pattern", "SearchTerm"]));
            put(&mut input, "path", text(params, &["SearchPath", "Path", "Directory", "SearchDirectory"]));
            "Grep".into()
        }
        "find_by_name" => {
            put(&mut input, "pattern", text(params, &["Pattern", "Query", "Name"]));
            put(&mut input, "path", text(params, &["SearchDirectory", "SearchPath", "Path", "Directory"]));
            "Glob".into()
        }
        "list_dir" => {
            put(&mut input, "path", text(params, &["DirectoryPath", "Path", "Directory"]));
            "LS".into()
        }
        "search_web" => {
            put(&mut input, "query", text(params, &["query", "Query"]));
            "WebSearch".into()
        }
        "read_url_content" => {
            put(&mut input, "url", text(params, &["Url", "URL"]));
            "WebFetch".into()
        }
        "call_mcp_tool" => {
            let server = text(params, &["ServerName", "Server", "server_name"]).unwrap_or_else(|| "mcp".into());
            let tool = text(params, &["ToolName", "Tool", "tool_name", "Name"]).unwrap_or_else(|| "tool".into());
            match lookup(params, &["Arguments", "Args", "Input"]) {
                Some(Value::Object(arguments)) => input.extend(arguments.clone()),
                Some(Value::String(encoded)) => {
                    if let Ok(Value::Object(arguments)) = serde_json::from_str(encoded) {
                        input.extend(arguments);
                    }
                }
                _ => {}
            }
            format!("mcp__{}__{}", mcp_part(&server), mcp_part(&tool))
        }
        "invoke_subagent" | "browser_subagent" | "define_subagent" => {
            put(
                &mut input,
                "description",
                text(params, &["Description", "TaskName", "Name", "Title"]).or_else(|| Some(agy_name.replace('_', " "))),
            );
            put(&mut input, "prompt", text(params, &["Prompt", "Task", "Instructions", "Message", "TaskSummary"]));
            "Task".into()
        }
        other => {
            if let Value::Object(object) = params {
                input.extend(object.clone());
            }
            other.to_string()
        }
    };
    input.insert("raw".into(), params.clone());
    (name, Value::Object(input))
}

fn mcp_part(part: &str) -> String {
    let cleaned: String = part.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    if cleaned.is_empty() { "tool".into() } else { cleaned }
}

/// Whether a normalized call is `agy` reading the schema it cached for an MCP tool
/// (`~/.gemini/antigravity-cli/mcp/<server>/<tool>.json` or `instructions.md`). The model
/// does this before it calls a deferred MCP tool; it is not the user's work, so it is not shown.
pub(crate) fn reads_own_mcp_schema(kybern_name: &str, input: &Value) -> bool {
    const CACHE: &str = "/.gemini/antigravity-cli/mcp/";
    kybern_name == "Read"
        && input
            .get("file_path")
            .and_then(Value::as_str)
            .is_some_and(|path| path.replace('\\', "/").contains(CACHE) && !path.contains(".."))
}

/// A shell result that says it failed: "Command exited with code 2." (not 0).
pub(crate) fn exited_nonzero(output: &str) -> bool {
    const MARKER: &str = "Command exited with code ";
    output
        .find(MARKER)
        .map(|at| output[at + MARKER.len()..].chars().take_while(char::is_ascii_digit).collect::<String>())
        .and_then(|code| code.parse::<u32>().ok())
        .is_some_and(|code| code != 0)
}

/// Best effort: the arguments agy logged for a call it made, from
/// `~/.gemini/antigravity-cli/brain/<conversation>/.system_generated/logs/transcript.jsonl`.
/// Read only; never fails a turn.
pub(crate) fn transcript_args(home: Option<&Path>, conversation: &str, before_step: u64, tool_name: &str) -> Option<Value> {
    if conversation.is_empty() || conversation.contains(['/', '\\']) || conversation.contains("..") {
        return None;
    }
    let path: PathBuf = home?.join(".gemini/antigravity-cli/brain").join(conversation).join(".system_generated/logs/transcript.jsonl");
    let meta = std::fs::metadata(&path).ok()?;
    if meta.len() > 16 * 1024 * 1024 {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    find_in_transcript(&text, before_step, tool_name)
}

pub(crate) fn find_in_transcript(transcript: &str, before_step: u64, tool_name: &str) -> Option<Value> {
    let mut found = None;
    for line in transcript.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        if entry.get("step_index").and_then(Value::as_u64).is_some_and(|step| step >= before_step) {
            continue;
        }
        for call in entry.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
            if call.get("name").and_then(Value::as_str) != Some(tool_name) {
                continue;
            }
            let Some(args) = call.get("args").and_then(Value::as_object) else { continue };
            found = Some(decode_args(args));
        }
    }
    found
}

/// A transcript call's `args`: values are often JSON-encoded strings, so
/// `"\"/abs\""` is the path `/abs`.
pub(crate) fn decode_args(args: &Map<String, Value>) -> Value {
    let decoded: Map<String, Value> = args
        .iter()
        .map(|(key, value)| {
            let value = match value.as_str() {
                Some(encoded) => serde_json::from_str(encoded).unwrap_or_else(|_| Value::String(encoded.to_string())),
                None => value.clone(),
            };
            (key.clone(), value)
        })
        .collect();
    Value::Object(decoded)
}
