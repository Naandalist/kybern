//! Delegate harness updates to the installed CLI or its existing Homebrew package.
//! No installers, package migrations, or user-provided command strings.
use anyhow::{Result, bail};
use kybern_protocol::ProviderKind;
use kybern_protocol::process::NoWindow;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

#[derive(Debug)]
pub struct UpdateCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

fn brew_package(kind: ProviderKind, path: &Path) -> Option<(PathBuf, String, bool)> {
    let components: Vec<_> = path.components().collect();
    for (index, component) in components.iter().enumerate() {
        let cask = component.as_os_str() == "Caskroom";
        if !cask && component.as_os_str() != "Cellar" {
            continue;
        }
        let package = components.get(index + 1)?.as_os_str().to_str()?;
        let valid = match kind {
            ProviderKind::ClaudeCode => matches!(package, "claude-code" | "claude-code@latest"),
            ProviderKind::Codex => package == "codex",
            ProviderKind::Opencode => package == "opencode",
            ProviderKind::Omp => package == "omp",
            _ => false,
        };
        if valid {
            let prefix: PathBuf = components[..index].iter().collect();
            return Some((prefix.join("bin/brew"), package.into(), cask));
        }
    }
    None
}

pub async fn plan(kind: ProviderKind, binary: &Path, env: &BTreeMap<String, String>, cwd: &Path) -> Result<UpdateCommand> {
    if kind == ProviderKind::Cursor {
        bail!(
            "Each Kybern release uses its own Cursor SDK version. Install or update it under Cursor SDK in Settings → Agent providers, or run `kybern cursor install`."
        );
    }
    let canonical = std::fs::canonicalize(binary)?;
    let paths = format!("{} {}", binary.display(), canonical.display());
    if ["/nix/store/", "/.local/share/mise/", "/.asdf/", "/.local/share/rtx/"].iter().any(|part| paths.contains(part)) {
        bail!("Managed or pinned installation. Update it through its version manager.");
    }
    if let Some((program, package, cask)) = brew_package(kind, &canonical) {
        if !program.is_file() {
            bail!("Homebrew was not found beside this installation. Update it from your terminal.");
        }
        let mut args = vec!["upgrade".into()];
        if cask {
            args.push("--cask".into());
        }
        args.push(package);
        return Ok(UpdateCommand { program, args });
    }
    let mut help = Command::new(binary).no_window();
    help.arg("--help").envs(env).current_dir(cwd).stdin(Stdio::null()).kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), crate::process_tree::output(&mut help)).await??;
    // Some CLIs (including OpenCode) write successful help to stderr.
    let text = format!("{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    let command = if kind == ProviderKind::Opencode { "upgrade" } else { "update" };
    if !output.status.success() || !advertises(&text, command) {
        bail!("This CLI does not advertise a supported self-updater. Update it with its original installer.");
    }
    let mut args = vec![command.into()];
    if kind == ProviderKind::Pi {
        // Older pi releases use `update` only for extensions. Never update those by accident.
        if !text.contains("--self") {
            bail!("This pi version does not advertise --self. Update the CLI through its package manager.");
        }
        args.push("--self".into());
    }
    Ok(UpdateCommand { program: binary.to_owned(), args })
}

fn advertises(help: &str, command: &str) -> bool {
    help.lines().any(|line| {
        let words: Vec<_> = line.split_whitespace().take(2).collect();
        words.iter().any(|word| word.split('|').any(|part| part == command))
    })
}

async fn tail(mut stream: impl tokio::io::AsyncRead + Unpin) -> String {
    let mut bytes = Vec::new();
    let mut buf = [0; 4096];
    while let Ok(count) = stream.read(&mut buf).await {
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..count]);
        if bytes.len() > 8192 {
            bytes.drain(..bytes.len() - 8192);
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

use crate::process_tree::ProcessTree;

pub async fn run(plan: &UpdateCommand, env: &BTreeMap<String, String>, cwd: &Path) -> Result<()> {
    run_with_timeout(plan, env, cwd, Duration::from_secs(300)).await
}

async fn run_with_timeout(plan: &UpdateCommand, env: &BTreeMap<String, String>, cwd: &Path, timeout: Duration) -> Result<()> {
    let mut command = Command::new(&plan.program).no_window();
    command
        .args(&plan.args)
        .envs(env)
        .env("CI", "1")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    let tree = ProcessTree::new(child.id().expect("spawned updater"));
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let result = tokio::time::timeout(timeout, async { tokio::join!(child.wait(), tail(stdout), tail(stderr)) }).await;
    drop(tree);
    let (status, _, stderr) = match result {
        Ok(value) => value,
        Err(_) => {
            let _ = child.wait().await;
            bail!("Update timed out. Check the installation in your terminal before retrying.");
        }
    };
    if !status?.success() {
        let detail: String = stderr.chars().filter(|c| !c.is_control() || *c == '\n').take(1500).collect();
        bail!("Update failed. Run the harness updater in your terminal to resolve it. {}", detail.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[tokio::test]
    async fn native_updater_uses_arguments_and_reports_failure_without_installing_other_tools() {
        let root = tempfile::tempdir().unwrap();
        let binary = root.path().join("fixture with spaces");
        kybern_protocol::process::write_executable(
            &binary,
            "#!/bin/sh\ncase \"$1\" in\n--help) echo '  update  Update the CLI' >&2;;\nupdate) printf done > updated;;\nesac\n",
        )
        .unwrap();
        let env = BTreeMap::new();
        let command = unbusy(|| plan(ProviderKind::Codex, &binary, &env, root.path())).await.unwrap();
        unbusy(|| run(&command, &env, root.path())).await.unwrap();
        assert_eq!(std::fs::read_to_string(root.path().join("updated")).unwrap(), "done");
        assert!(unbusy(|| plan(ProviderKind::Pi, &binary, &env, root.path())).await.unwrap_err().to_string().contains("--self"));
        std::fs::write(&binary, "#!/bin/sh\necho 'fixture permission denied' >&2\nexit 1\n").unwrap();
        let error = unbusy(|| run(&command, &env, root.path())).await.unwrap_err();
        assert!(error.to_string().contains("fixture permission denied"), "{error}");
    }

    /// Linux refuses to run a file that another test's freshly forked child
    /// still holds open for writing (ETXTBSY) until that child execs. Retry.
    #[cfg(unix)]
    async fn unbusy<T, F: std::future::Future<Output = Result<T>>>(mut call: impl FnMut() -> F) -> Result<T> {
        for _ in 0..50 {
            match call().await {
                Err(error) if error.to_string().contains("Text file busy") => tokio::time::sleep(Duration::from_millis(20)).await,
                result => return result,
            }
        }
        call().await
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_stops_the_launcher_and_its_background_children() {
        let root = tempfile::tempdir().unwrap();
        let binary = root.path().join("updater");
        kybern_protocol::process::write_executable(&binary, "#!/bin/sh\n(sleep 0.2; touch escaped) &\nwait\n").unwrap();
        let command = UpdateCommand { program: binary, args: vec![] };
        assert!(
            run_with_timeout(&command, &BTreeMap::new(), root.path(), Duration::from_millis(30))
                .await
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!root.path().join("escaped").exists());
    }

    #[test]
    fn preserves_homebrew_channel_and_rejects_similar_package_names() {
        let (_, name, cask) =
            brew_package(ProviderKind::ClaudeCode, Path::new("/opt/homebrew/Caskroom/claude-code@latest/2.1/claude")).unwrap();
        assert_eq!(name, "claude-code@latest");
        assert!(cask);
        assert!(brew_package(ProviderKind::Codex, Path::new("/opt/homebrew/Cellar/not-codex/1/bin/codex")).is_none());
    }
    #[test]
    fn help_requires_an_actual_command_not_a_description() {
        assert!(advertises("  update|upgrade   Check for updates", "update"));
        assert!(advertises("  opencode upgrade [target]   upgrade", "upgrade"));
        assert!(!advertises("  commit  Generate messages and update changelogs", "update"));
    }
}
