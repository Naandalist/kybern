//! Locate provider executables. Never installs anything.

use kybern_protocol::process::NoWindow;
use std::path::{Path, PathBuf};

use kybern_protocol::ProviderKind;

use crate::{DriverError, Result};

pub fn resolve(kind: ProviderKind, override_path: Option<&PathBuf>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        if p.is_file() {
            return Ok(p.clone());
        }
        return Err(DriverError::BinaryNotFound(p.display().to_string()));
    }
    which::which(kind.default_binary())
        .ok()
        .or_else(|| installed_location(kind))
        .ok_or_else(|| DriverError::BinaryNotFound(kind.default_binary().to_string()))
}

/// Where a harness's own installer puts its binary when that folder is not on
/// the daemon's PATH (a desktop app starts with a short PATH).
fn installed_location(kind: ProviderKind) -> Option<PathBuf> {
    if kind != ProviderKind::Antigravity {
        return None;
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from);
    let local_app_data = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    antigravity_candidates(home.as_deref(), local_app_data.as_deref()).into_iter().find(|path| path.is_file())
}

pub(crate) fn antigravity_candidates(home: Option<&Path>, local_app_data: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = home {
        paths.push(home.join(".local").join("bin").join("agy"));
    }
    if let Some(local) = local_app_data {
        paths.push(local.join("agy").join("bin").join("agy.exe"));
    }
    paths
}

/// A command that runs a provider binary.
///
/// On Windows, npm, pnpm and Yarn install a CLI as a `.cmd` shim that runs
/// `node <script>`. Rust starts a `.cmd` through `cmd.exe` and refuses any
/// argument it cannot pass through it safely ("batch file arguments are
/// invalid"), which includes every multi-line instructions argument. So the
/// shim is skipped: the command runs the shim's own target directly.
pub fn command(bin: impl AsRef<Path>) -> tokio::process::Command {
    let bin = bin.as_ref();
    #[cfg(windows)]
    if let Some(target) = unwrap_cmd_shim(bin) {
        let mut command = tokio::process::Command::new(target.program);
        command.args(target.args);
        return command;
    }
    tokio::process::Command::new(bin)
}

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ShimTarget {
    pub program: PathBuf,
    pub args: Vec<PathBuf>,
}

#[cfg(windows)]
fn unwrap_cmd_shim(bin: &Path) -> Option<ShimTarget> {
    let ext = bin.extension()?.to_str()?.to_ascii_lowercase();
    if ext != "cmd" && ext != "bat" {
        return None;
    }
    let text = std::fs::read_to_string(bin).ok()?;
    let dir = bin.parent()?;
    let local_node = dir.join("node.exe");
    let node = if local_node.is_file() { Some(local_node) } else { which::which("node").ok() };
    shim_target(&text, dir, node.as_deref())
}

/// The program a package-manager `.cmd` shim runs. `node` is the Node.js to
/// use when the target is a script; `None` when the shim is not recognised,
/// its target is missing, or it needs a Node.js that cannot be found.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn shim_target(text: &str, dir: &Path, node: Option<&Path>) -> Option<ShimTarget> {
    // npm/Yarn (`cmd-shim`): `... "%_prog%"  "%dp0%\node_modules\pkg\cli.js" %*`
    // or, for a native target, `"%dp0%\node_modules\pkg\bin\tool.exe"   %*`.
    // pnpm: `node  "%~dp0\..\pkg\cli.js" %*` (its last `%*` line).
    let line = text.lines().rev().map(str::trim).find(|line| line.contains("%*"))?;
    let (start, quoted) = quoted_segments(line)
        .into_iter()
        .rev()
        .find(|(_, q)| relative_to_shim(q).is_some() && !q.to_ascii_lowercase().ends_with("node.exe"))?;
    let mut target = dir.to_path_buf();
    for part in relative_to_shim(quoted)?.split(['\\', '/']).filter(|part| !part.is_empty()) {
        target.push(part);
    }
    let before = line[..start].to_ascii_lowercase();
    let runs_node = before.contains("_prog") || before.contains("node");
    if !target.is_file() {
        return None;
    }
    let shim = match runs_node {
        true => ShimTarget { program: node?.to_path_buf(), args: vec![target] },
        false => ShimTarget { program: target, args: Vec::new() },
    };
    Some(shim)
}

/// Rust refuses some arguments to a Windows `.cmd`/`.bat` (a newline, for
/// example) with "batch file arguments are invalid", which tells the user
/// nothing. `command` avoids that for package-manager shims; this explains the
/// error when a script could not be skipped.
pub(crate) fn explain_spawn_error(error: std::io::Error, program: &std::ffi::OsStr) -> std::io::Error {
    let path = Path::new(program);
    let is_script =
        path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"));
    if error.kind() != std::io::ErrorKind::InvalidInput || !is_script {
        return error;
    }
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "Kybern can’t pass this conversation’s instructions to {}, a Windows command script. \
             Install the agent’s standalone version, or make sure Node.js is on PATH, then try again.",
            path.display()
        ),
    )
}

fn relative_to_shim(quoted: &str) -> Option<&str> {
    quoted.strip_prefix("%dp0%").or_else(|| quoted.strip_prefix("%~dp0"))
}

/// `"…"` segments of a line, with the byte offset of each opening quote.
fn quoted_segments(line: &str) -> Vec<(usize, &str)> {
    let mut segments = Vec::new();
    let mut open = None;
    for (index, ch) in line.char_indices() {
        if ch == '"' {
            match open.take() {
                Some(start) => segments.push((start, &line[start + 1..index])),
                None => open = Some(index),
            }
        }
    }
    segments
}

/// Run `<bin> --version` and return the first line, trimmed.
pub async fn version_of(bin: &Path, args: &[&str]) -> Option<String> {
    let run = async {
        // Linux refuses to run a file that a freshly forked child of another
        // thread still holds open for writing (ETXTBSY) until that child execs,
        // e.g. a harness just installed or updated. Retry briefly.
        for _ in 0..25 {
            match crate::process_tree::output(command(bin).no_window().args(args)).await {
                Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await
                }
                result => return result,
            }
        }
        crate::process_tree::output(command(bin).no_window().args(args)).await
    };
    let out = tokio::time::timeout(std::time::Duration::from_secs(10), run).await.ok()?.ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next()?.trim();
    if line.is_empty() { None } else { Some(line.to_string()) }
}

/// Parse a leading `x.y.z` from a version line. Returns `(major, minor, patch)`.
pub fn parse_semver(line: &str) -> Option<(u64, u64, u64)> {
    let token = line.split(|c: char| !(c.is_ascii_digit() || c == '.')).find(|t| t.contains('.'))?;
    let mut it = token.split('.').map(|p| p.parse::<u64>().ok());
    Some((it.next()??, it.next()??, it.next().flatten().unwrap_or(0)))
}

pub fn at_least(found: &str, required: (u64, u64, u64)) -> bool {
    parse_semver(found).map(|v| v >= required).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NPM_NODE: &str = "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n  SET PATHEXT=%PATHEXT:;.JS;=;%\r\n)\r\n\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\node_modules\\@anthropic-ai\\claude-code\\cli.js\" %*\r\n";
    const NPM_NATIVE: &str = "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\"%dp0%\\node_modules\\@anthropic-ai\\claude-code\\bin\\claude.exe\"   %*\r\n";
    const PNPM: &str = "@SETLOCAL\r\n@IF NOT DEFINED NODE_PATH (\r\n  @SET \"NODE_PATH=C:\\pnpm\\global\\5\\node_modules\"\r\n)\r\n@IF EXIST \"%~dp0\\node.exe\" (\r\n  \"%~dp0\\node.exe\"  \"%~dp0\\global\\5\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n) ELSE (\r\n  @SET PATHEXT=%PATHEXT:;.JS;=;%\r\n  node  \"%~dp0\\global\\5\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n)\r\n";

    fn file(dir: &Path, parts: &[&str]) -> PathBuf {
        let path = parts.iter().fold(dir.to_path_buf(), |path, part| path.join(part));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        path
    }

    #[test]
    fn npm_shim_runs_its_script_with_node() {
        let dir = tempfile::tempdir().unwrap();
        let script = file(dir.path(), &["node_modules", "@anthropic-ai", "claude-code", "cli.js"]);
        let node = Path::new("C:/node/node.exe");
        assert_eq!(shim_target(NPM_NODE, dir.path(), Some(node)), Some(ShimTarget { program: node.to_path_buf(), args: vec![script] }));
        assert_eq!(shim_target(NPM_NODE, dir.path(), None), None, "a script needs Node.js");
    }

    #[test]
    fn npm_shim_runs_a_native_target_directly() {
        let dir = tempfile::tempdir().unwrap();
        let exe = file(dir.path(), &["node_modules", "@anthropic-ai", "claude-code", "bin", "claude.exe"]);
        assert_eq!(shim_target(NPM_NATIVE, dir.path(), None), Some(ShimTarget { program: exe, args: Vec::new() }));
    }

    #[test]
    fn pnpm_shim_runs_its_script_with_node() {
        let dir = tempfile::tempdir().unwrap();
        let script = file(dir.path(), &["global", "5", "node_modules", "@openai", "codex", "bin", "codex.js"]);
        let node = Path::new("C:/node/node.exe");
        assert_eq!(shim_target(PNPM, dir.path(), Some(node)), Some(ShimTarget { program: node.to_path_buf(), args: vec![script] }));
    }

    #[test]
    fn explains_refused_script_arguments() {
        let refused = || std::io::Error::new(std::io::ErrorKind::InvalidInput, "batch file arguments are invalid");
        let explained = explain_spawn_error(refused(), std::ffi::OsStr::new("C:/npm/claude.CMD")).to_string();
        assert!(explained.contains("claude.CMD") && explained.contains("Node.js"), "{explained}");
        assert_eq!(explain_spawn_error(refused(), std::ffi::OsStr::new("C:/x/claude.exe")).to_string(), "batch file arguments are invalid");
    }

    #[test]
    fn unknown_or_dangling_shims_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(shim_target(NPM_NODE, dir.path(), Some(Path::new("node"))), None, "missing target");
        assert_eq!(shim_target("@echo off\r\npython C:\\tools\\x.py %*\r\n", dir.path(), Some(Path::new("node"))), None);
        assert_eq!(shim_target("", dir.path(), None), None);
    }
}
