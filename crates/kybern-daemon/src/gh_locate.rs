//! Finding a `gh` installed after the daemon started (Windows).
//!
//! The daemon reads the registry Path once at startup (`widen_path`), and it outlives desktop
//! windows, so a `winget install GitHub.cli` made later stays invisible to it: every `gh` call
//! fails with "program not found" until a restart. When a spawn of the bare `gh` reports
//! NotFound, `refresh` reads the registry Path again, adds the folders the GitHub CLI installers
//! use, and widens the process PATH when that finds `gh.exe`. Terminals the daemon opens later
//! inherit the wider PATH too. The look-up runs at most once per `THROTTLE`; macOS and Linux
//! never do any of this.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::Mutex;
use std::time::Duration;
#[cfg(windows)]
use std::time::Instant;

/// How long a look-up that found nothing stands before the next NotFound reads again.
#[cfg_attr(not(windows), allow(dead_code))]
const THROTTLE: Duration = Duration::from_secs(5);

/// The folders the GitHub CLI's MSI (machine and per-user), winget and Scoop put `gh.exe` in.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn install_dirs(lookup: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let var = |name: &str| lookup(name).filter(|value| !value.is_empty()).map(PathBuf::from);
    let mut dirs = Vec::new();
    dirs.extend(var("ProgramFiles").map(|programs| programs.join("GitHub CLI")));
    dirs.extend(var("ProgramFiles(x86)").map(|programs| programs.join("GitHub CLI")));
    if let Some(local) = var("LOCALAPPDATA") {
        dirs.push(local.join("Programs").join("GitHub CLI"));
        dirs.push(local.join("Microsoft").join("WinGet").join("Links"));
    }
    if let Some(scoop) = var("SCOOP").or_else(|| var("USERPROFILE").map(|home| home.join("scoop"))) {
        dirs.push(scoop.join("shims"));
    }
    dirs
}

/// The first `<dir>\gh.exe` that `exists`.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn find_gh(dirs: &[PathBuf], exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    dirs.iter().map(|dir| dir.join("gh.exe")).find(|path| exists(path))
}

/// Whether `program` is the bare `gh` this module looks for (not a path a test or setting names).
#[cfg_attr(not(windows), allow(dead_code))]
fn is_bare_gh(program: &str) -> bool {
    program.eq_ignore_ascii_case("gh") || program.eq_ignore_ascii_case("gh.exe")
}

/// After a spawn of `program` reported NotFound: widen the process PATH from the registry and the
/// GitHub CLI's install folders, and say whether `gh.exe` is on it now, so the caller retries once.
/// Always `false` off Windows and for any program but the bare `gh`.
pub(crate) fn refresh(program: &str) -> bool {
    #[cfg(windows)]
    {
        static LAST_MISS: Mutex<Option<Instant>> = Mutex::new(None);
        if !is_bare_gh(program) {
            return false;
        }
        let mut last_miss = LAST_MISS.lock().unwrap_or_else(|error| error.into_inner());
        if last_miss.is_some_and(|at| at.elapsed() < THROTTLE) {
            return false;
        }
        let current: Vec<PathBuf> = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
        let mut candidates = crate::windows_path::registry_path();
        candidates.extend(install_dirs(|name| std::env::var_os(name)));
        let entries = match crate::windows_path::widened(&current, &candidates, Path::is_dir) {
            Some(entries) => {
                if let Ok(joined) = std::env::join_paths(&entries) {
                    tracing::info!("gh was not on PATH; added the folders a new terminal would have");
                    // SAFETY: Windows keeps the environment behind its own lock, so setting it while other threads
                    // run is sound there (std documents `set_var` as always safe on Windows).
                    unsafe { std::env::set_var("PATH", joined) };
                }
                entries
            }
            None => current,
        };
        let found = find_gh(&entries, Path::is_file).is_some();
        *last_miss = (!found).then(Instant::now);
        found
    }
    #[cfg(not(windows))]
    {
        let _ = program;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    #[test]
    fn install_dirs_cover_msi_winget_and_scoop() {
        let vars = HashMap::from([
            ("ProgramFiles", r"C:\Program Files"),
            ("LOCALAPPDATA", r"C:\Users\ana\AppData\Local"),
            ("USERPROFILE", r"C:\Users\ana"),
        ]);
        let dirs = install_dirs(|name| vars.get(name).map(OsString::from));
        for expected in [
            Path::new(r"C:\Program Files").join("GitHub CLI"),
            Path::new(r"C:\Users\ana\AppData\Local").join("Programs").join("GitHub CLI"),
            Path::new(r"C:\Users\ana\AppData\Local").join("Microsoft").join("WinGet").join("Links"),
            Path::new(r"C:\Users\ana").join("scoop").join("shims"),
        ] {
            assert!(dirs.contains(&expected), "{expected:?} missing from {dirs:?}");
        }
        let scoop = install_dirs(|name| (name == "SCOOP").then(|| OsString::from(r"D:\scoop")));
        assert_eq!(scoop, [Path::new(r"D:\scoop").join("shims")]);
        assert!(install_dirs(|_| None).is_empty());
    }

    #[test]
    fn finds_the_first_gh_exe() {
        let dirs = ["/a", "/winget", "/scoop"].map(PathBuf::from);
        let files: HashSet<PathBuf> = [Path::new("/winget").join("gh.exe"), Path::new("/scoop").join("gh.exe")].into_iter().collect();
        assert_eq!(find_gh(&dirs, |path| files.contains(path)), Some(Path::new("/winget").join("gh.exe")));
        assert_eq!(find_gh(&dirs[..1], |path| files.contains(path)), None);
    }

    #[test]
    fn only_the_bare_gh_is_looked_up() {
        assert!(is_bare_gh("gh") && is_bare_gh("GH.exe"));
        assert!(!is_bare_gh(r"C:\fake\gh") && !is_bare_gh("git"));
        assert!(!refresh("git"));
    }
}
