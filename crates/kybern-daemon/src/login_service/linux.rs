//! Linux: a `systemd --user` unit, or an XDG autostart entry where this user has no systemd manager.
//! Every start also gives a unit the remote installers wrote before `OOMPolicy=continue` the drop-in
//! that adds it ([`Systemd::migrate`]).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use kybern_protocol::LoginServiceKind;

use super::files;
use super::{Registration, ServiceManager, ServiceSpec};

/// The unit `scripts/install-remote.sh` and the desktop's remote installer write.
const REMOTE_INSTALL_UNIT: &str = "kybernd.service";

pub struct Systemd {
    /// `systemctl --user` reaches a manager; otherwise the autostart entry is used.
    user_manager: bool,
}

fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    Ok(directories::BaseDirs::new().context("no home directory")?.home_dir().join(".config"))
}

fn units_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join("systemd").join("user"))
}

fn systemctl(args: &[&str]) -> Result<std::process::Output> {
    Command::new("systemctl").arg("--user").args(args).output().map_err(|error| anyhow!("could not run systemctl: {error}"))
}

fn checked(args: &[&str]) -> Result<()> {
    let output = systemctl(args)?;
    if !output.status.success() {
        bail!("systemctl --user {}: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

fn write_atomically(path: &Path, text: &str) -> Result<()> {
    std::fs::create_dir_all(path.parent().unwrap_or(Path::new("/")))?;
    let temporary = path.with_extension("new");
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

/// The `.conf` drop-ins of a unit.
fn drop_ins(unit: &Path) -> Vec<String> {
    let dir = PathBuf::from(format!("{}.d", unit.display()));
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "conf"))
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .collect()
}

/// The units of Kybern's own that start `data_dir`'s daemon and need the OOM drop-in. Units of other
/// data folders are left to their own daemons.
fn units_missing_oom_policy(data_dir: &Path) -> Vec<PathBuf> {
    let data_dir = data_dir.to_string_lossy();
    let Ok(units) = units_dir() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(&units) else { return Vec::new() };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "service"))
        .filter(|path| {
            std::fs::read_to_string(path)
                .is_ok_and(|unit| files::starts_data_dir(&unit, &data_dir) && files::needs_oom_drop_in(&unit, &drop_ins(path)))
        })
        .collect()
}

impl Systemd {
    pub fn detect() -> Self {
        Self { user_manager: systemctl(&["show-environment"]).is_ok_and(|output| output.status.success()) }
    }

    fn file(&self, spec: &ServiceSpec) -> Result<PathBuf> {
        Ok(if self.user_manager {
            units_dir()?.join(format!("{}.service", spec.name))
        } else {
            config_dir()?.join("autostart").join(format!("{}.desktop", spec.name))
        })
    }

    fn text(&self, spec: &ServiceSpec) -> String {
        if self.user_manager { files::systemd_unit(spec) } else { files::autostart_desktop(spec) }
    }

    /// Give each Kybern unit without an `OOMPolicy` the drop-in, then reload. The running daemon is
    /// not restarted: the policy applies from the reload on.
    pub fn migrate_oom_policy(&self, data_dir: &Path) -> Vec<String> {
        if !self.user_manager {
            return Vec::new();
        }
        let mut migrated = Vec::new();
        for unit in units_missing_oom_policy(data_dir) {
            let drop_in = PathBuf::from(format!("{}.d", unit.display())).join("oom.conf");
            match write_atomically(&drop_in, files::OOM_DROP_IN) {
                Ok(()) => migrated.push(unit.file_name().unwrap_or_default().to_string_lossy().into_owned()),
                Err(error) => {
                    tracing::warn!(target: super::TARGET, unit = %unit.display(), %error, "could not add OOMPolicy=continue to the unit")
                }
            }
        }
        if !migrated.is_empty() {
            if let Err(error) = checked(&["daemon-reload"]) {
                tracing::warn!(target: super::TARGET, %error, "could not reload systemd after adding OOMPolicy=continue");
            }
            tracing::info!(target: super::TARGET, units = ?migrated, "added OOMPolicy=continue, so an agent that runs out of memory no longer stops the daemon");
        }
        migrated
    }
}

impl ServiceManager for Systemd {
    fn kind(&self) -> Option<LoginServiceKind> {
        Some(if self.user_manager { LoginServiceKind::SystemdUser } else { LoginServiceKind::XdgAutostart })
    }

    fn registration(&self, spec: &ServiceSpec) -> Result<Registration> {
        match std::fs::read_to_string(self.file(spec)?) {
            Ok(text) if text == self.text(spec) => Ok(Registration::Current),
            // Another PATH alone does not make it stale: the item keeps the PATH it was given.
            Ok(text) if text.contains(spec.exe.to_string_lossy().as_ref()) && text.contains(files::OOM_POLICY) == self.user_manager => {
                Ok(Registration::Current)
            }
            Ok(_) => Ok(Registration::Stale),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Registration::Missing),
            Err(error) => Err(error.into()),
        }
    }

    /// Write and enable the unit for the next login without starting it: this daemon is already up.
    fn install(&self, spec: &ServiceSpec) -> Result<()> {
        write_atomically(&self.file(spec)?, &self.text(spec))?;
        if self.user_manager {
            checked(&["daemon-reload"])?;
            checked(&["enable", &format!("{}.service", spec.name)])?;
        }
        Ok(())
    }

    /// Disable and remove the unit. It is stopped too, unless it is this daemon.
    fn uninstall(&self, spec: &ServiceSpec, running_here: bool) -> Result<()> {
        let unit = format!("{}.service", spec.name);
        if self.user_manager {
            let args: Vec<&str> = if running_here { vec!["disable", &unit] } else { vec!["disable", "--now", &unit] };
            if let Err(error) = checked(&args) {
                tracing::debug!(target: super::TARGET, %error, "systemd could not disable the unit; removing its file");
            }
        }
        remove_if_present(&self.file(spec)?)?;
        if self.user_manager {
            checked(&["daemon-reload"])?;
        }
        Ok(())
    }

    fn external(&self, data_dir: &Path) -> Option<String> {
        let text = std::fs::read_to_string(units_dir().ok()?.join(REMOTE_INSTALL_UNIT)).ok()?;
        files::starts_data_dir(&text, &data_dir.to_string_lossy())
            .then(|| format!("The systemd user service {REMOTE_INSTALL_UNIT} already starts Kybern at login."))
    }

    fn migrate(&self, data_dir: &Path) -> Vec<String> {
        self.migrate_oom_policy(data_dir)
    }

    fn warnings(&self, data_dir: &Path) -> Vec<(String, String, String)> {
        units_missing_oom_policy(data_dir)
            .into_iter()
            .map(|unit| {
                let name = unit.file_name().unwrap_or_default().to_string_lossy().into_owned();
                (
                    "service.oom_policy".to_string(),
                    format!("The systemd unit {name} stops Kybern and every agent when one agent runs out of memory"),
                    format!(
                        "Restart the daemon so it adds OOMPolicy=continue, or add ~/.config/systemd/user/{name}.d/oom.conf with [Service] and OOMPolicy=continue, then run systemctl --user daemon-reload."
                    ),
                )
            })
            .collect()
    }
}
