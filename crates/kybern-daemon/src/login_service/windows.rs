//! Windows: a Task Scheduler task in the `\Kybern` folder, registered with `schtasks` from XML. A standard
//! user may register a logon task for themselves without elevation; the task never asks for it.

use std::path::PathBuf;
use std::process::{Command, Output};

use anyhow::{Result, anyhow, bail};
use kybern_protocol::LoginServiceKind;
use kybern_protocol::process::NoWindow;

use super::files;
use super::{Registration, ServiceManager, ServiceSpec};

const FOLDER: &str = r"\Kybern";

pub struct TaskScheduler;

fn task_path(spec: &ServiceSpec) -> String {
    format!(r"{FOLDER}\{}", spec.name)
}

fn schtasks(args: &[&str]) -> Result<Output> {
    Command::new("schtasks.exe").no_window().args(args).output().map_err(|error| anyhow!("could not run schtasks: {error}"))
}

fn failure(output: &Output) -> String {
    let text = decode(&output.stderr);
    let text = if text.trim().is_empty() { decode(&output.stdout) } else { text };
    text.trim().trim_start_matches("ERROR:").trim().to_string()
}

/// `schtasks` writes the console code page, or UTF-16 for `/xml`.
fn decode(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let units: Vec<u16> = bytes[2..].as_chunks::<2>().0.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
        return String::from_utf16_lossy(&units);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn unescape(xml: &str) -> String {
    xml.replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// `DOMAIN\user` of this process.
fn user() -> Result<String> {
    match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(domain), Ok(name)) if !domain.is_empty() && !name.is_empty() => Ok(format!(r"{domain}\{name}")),
        (_, Ok(name)) if !name.is_empty() => Ok(name),
        _ => bail!("this process does not know its user name"),
    }
}

fn conhost() -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    PathBuf::from(root).join("System32").join("conhost.exe").to_string_lossy().into_owned()
}

impl ServiceManager for TaskScheduler {
    fn kind(&self) -> Option<LoginServiceKind> {
        Some(LoginServiceKind::TaskScheduler)
    }

    fn registration(&self, spec: &ServiceSpec) -> Result<Registration> {
        let output = schtasks(&["/query", "/tn", &task_path(spec), "/xml"])?;
        if !output.status.success() {
            return Ok(Registration::Missing);
        }
        let xml = unescape(&decode(&output.stdout));
        let current = xml.to_lowercase().contains(&files::task_arguments(spec).to_lowercase());
        Ok(if current { Registration::Current } else { Registration::Stale })
    }

    fn install(&self, spec: &ServiceSpec) -> Result<()> {
        let xml = files::task_xml(spec, &user()?, &conhost());
        // `schtasks /xml` reads UTF-16 with a byte order mark.
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
        let file = std::env::temp_dir().join(format!("kybern-login-{}.xml", uuid::Uuid::now_v7()));
        std::fs::write(&file, bytes)?;
        let output = schtasks(&["/create", "/tn", &task_path(spec), "/xml", &file.to_string_lossy(), "/f"]);
        let _ = std::fs::remove_file(&file);
        let output = output?;
        if !output.status.success() {
            bail!("{}", failure(&output));
        }
        Ok(())
    }

    /// Deleting the task leaves a daemon it started running.
    fn uninstall(&self, spec: &ServiceSpec, _running_here: bool) -> Result<()> {
        let output = schtasks(&["/delete", "/tn", &task_path(spec), "/f"])?;
        if !output.status.success() && self.registration(spec)? != Registration::Missing {
            bail!("{}", failure(&output));
        }
        // The folder goes too once no task of another data folder is left in it.
        if let Ok(list) = schtasks(&["/query", "/fo", "csv", "/nh"])
            && list.status.success()
            && !decode(&list.stdout).to_lowercase().contains(&format!("\"{}\\", FOLDER.to_lowercase()))
        {
            let _ = schtasks(&["/delete", "/tn", FOLDER, "/f"]);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schtasks_output_decodes_from_utf16_and_the_task_arguments_match_the_xml() {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend("<Arguments>x</Arguments>".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode(&bytes), "<Arguments>x</Arguments>");
        let spec = ServiceSpec {
            name: "kybernd".into(),
            exe: r"C:\Program Files\Kybern\kybernd.exe".into(),
            data_dir: r"C:\d".into(),
            path_env: None,
        };
        let xml = files::task_xml(&spec, r"D\u", r"C:\Windows\System32\conhost.exe");
        assert!(unescape(&xml).contains(&files::task_arguments(&spec)), "registration() must recognise the task install() writes");
        assert_eq!(task_path(&spec), r"\Kybern\kybernd");
    }
}
