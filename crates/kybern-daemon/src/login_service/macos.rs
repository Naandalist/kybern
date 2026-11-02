//! macOS: a LaunchAgent in `~/Library/LaunchAgents`, loaded into this user's GUI domain.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use kybern_protocol::LoginServiceKind;

use super::files;
use super::{Registration, ServiceManager, ServiceSpec};

/// What `scripts/install-remote.sh` writes for an SSH install.
const REMOTE_INSTALL_LABEL: &str = "dev.kybern.daemon";

pub struct LaunchAgent;

fn agents_dir() -> Result<PathBuf> {
    let home = directories::BaseDirs::new().context("no home directory")?.home_dir().to_path_buf();
    Ok(home.join("Library").join("LaunchAgents"))
}

fn plist_path(label: &str) -> Result<PathBuf> {
    Ok(agents_dir()?.join(format!("{label}.plist")))
}

fn domain() -> String {
    // SAFETY: getuid has no preconditions and cannot fail.
    format!("gui/{}", unsafe { libc::getuid() })
}

fn launchctl(args: &[&str]) -> Result<std::process::Output> {
    Command::new("/bin/launchctl").args(args).output().map_err(|error| anyhow!("could not run launchctl: {error}"))
}

fn loaded(label: &str) -> bool {
    launchctl(&["print", &format!("{}/{label}", domain())]).is_ok_and(|output| output.status.success())
}

impl ServiceManager for LaunchAgent {
    fn kind(&self) -> Option<LoginServiceKind> {
        Some(LoginServiceKind::LaunchAgent)
    }

    fn registration(&self, spec: &ServiceSpec) -> Result<Registration> {
        match std::fs::read_to_string(plist_path(&spec.name)?) {
            Ok(text) if text == files::launch_agent_plist(spec) => Ok(Registration::Current),
            // Another PATH alone does not make it stale: the item keeps the PATH it was given.
            Ok(text) if text.contains(&format!("<string>{}</string>", files::xml_escape(&spec.exe.to_string_lossy()))) => {
                Ok(Registration::Current)
            }
            Ok(_) => Ok(Registration::Stale),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Registration::Missing),
            Err(error) => Err(error.into()),
        }
    }

    /// Write the plist, and load it when it is not loaded yet so it is known before the next login.
    /// Loading runs it once (`RunAtLoad`); that daemon finds this one serving and exits at once.
    fn install(&self, spec: &ServiceSpec) -> Result<()> {
        let path = plist_path(&spec.name)?;
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new("/")))?;
        let temporary = path.with_extension("plist.new");
        std::fs::write(&temporary, files::launch_agent_plist(spec))?;
        std::fs::rename(&temporary, &path)?;
        if !loaded(&spec.name) {
            let output = launchctl(&["bootstrap", &domain(), &path.to_string_lossy()])?;
            if !output.status.success() {
                tracing::warn!(target: super::TARGET, stderr = %String::from_utf8_lossy(&output.stderr).trim(), "launchctl could not load the agent now; it loads at the next login");
            }
        }
        Ok(())
    }

    /// Remove the plist. The loaded job is unloaded too, unless it is this daemon: unloading would stop
    /// it mid-request. It then stays loaded until logout and is not started again.
    fn uninstall(&self, spec: &ServiceSpec, running_here: bool) -> Result<()> {
        let path = plist_path(&spec.name)?;
        if !running_here && loaded(&spec.name) {
            let output = launchctl(&["bootout", &format!("{}/{}", domain(), spec.name)])?;
            if !output.status.success() && loaded(&spec.name) {
                bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
            }
        }
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn external(&self, data_dir: &Path) -> Option<String> {
        let text = std::fs::read_to_string(plist_path(REMOTE_INSTALL_LABEL).ok()?).ok()?;
        text.contains(&format!("<string>{}</string>", files::xml_escape(&data_dir.to_string_lossy())))
            .then(|| format!("The LaunchAgent {REMOTE_INSTALL_LABEL} already starts Kybern at login."))
    }
}
