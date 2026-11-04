//! Where agent CLIs live on Windows.
//!
//! A Windows process keeps the Path its parent had when it started. The desktop app, and the
//! daemon it starts, can hold one from before Node, Scoop or an npm global was installed, or one
//! that never had the user's own entries. The registry holds the Path a new terminal gets, so it
//! is read at startup, and the folders the usual Node installs put global commands in are added.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The system Path, then the user's, with `%VAR%` references expanded. Empty off Windows or when
/// the registry cannot be read.
pub(crate) fn registry_path() -> Vec<PathBuf> {
    // Plays a daemon started before an install changed the registry Path (end-to-end checks in
    // debug builds only; a shipped daemon always reads the registry).
    if cfg!(debug_assertions) && std::env::var_os("KYBERN_NO_REGISTRY_PATH").is_some() {
        return Vec::new();
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
        let lookup = |name: &str| std::env::var(name).ok();
        [(HKEY_LOCAL_MACHINE, r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment"), (HKEY_CURRENT_USER, "Environment")]
            .into_iter()
            .filter_map(|(root, key)| read_path_value(root, key))
            .flat_map(|value| split(&expand(&value, lookup)))
            .collect()
    }
    #[cfg(not(windows))]
    Vec::new()
}

/// The raw `Path` value under `key`, unexpanded.
#[cfg(windows)]
fn read_path_value(root: windows_sys::Win32::System::Registry::HKEY, key: &str) -> Option<String> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW};
    let wide = |text: &str| text.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (key, name) = (wide(key), wide("Path"));
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
    let mut bytes = 0u32;
    // SAFETY: both strings are NUL-terminated and outlive the call; a null data pointer asks for the size only.
    let status = unsafe { RegGetValueW(root, key.as_ptr(), name.as_ptr(), flags, std::ptr::null_mut(), std::ptr::null_mut(), &mut bytes) };
    if status != ERROR_SUCCESS {
        return None;
    }
    let mut buffer = vec![0u16; (bytes as usize).div_ceil(2)];
    // SAFETY: `buffer` holds `bytes` bytes, and `bytes` tells the call so.
    let status =
        unsafe { RegGetValueW(root, key.as_ptr(), name.as_ptr(), flags, std::ptr::null_mut(), buffer.as_mut_ptr().cast(), &mut bytes) };
    if status != ERROR_SUCCESS {
        return None;
    }
    let end = buffer.iter().position(|&unit| unit == 0).unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..end]))
}

/// Replace each `%NAME%` that `lookup` knows. Unknown names stay as written, as Windows leaves them.
#[cfg_attr(not(windows), allow(dead_code))]
fn expand(value: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => match lookup(&after[..end]).filter(|_| end > 0) {
                Some(found) => {
                    out.push_str(&found);
                    rest = &after[end + 1..];
                }
                None => {
                    out.push('%');
                    rest = after;
                }
            },
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// A Path value's entries, without empty ones or the quotes some installers add.
#[cfg_attr(not(windows), allow(dead_code))]
fn split(value: &str) -> Vec<PathBuf> {
    value.split(';').map(|entry| entry.trim().trim_matches('"')).filter(|entry| !entry.is_empty()).map(PathBuf::from).collect()
}

/// The folders npm, pnpm, Volta, Scoop, nvm-windows and the Node installer put commands in.
pub(crate) fn tool_dirs(lookup: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let var = |name: &str| lookup(name).filter(|value| !value.is_empty()).map(PathBuf::from);
    let mut dirs = Vec::new();
    if let Some(roaming) = var("APPDATA") {
        dirs.push(roaming.join("npm"));
    }
    let local = var("LOCALAPPDATA");
    dirs.extend(var("PNPM_HOME").or_else(|| local.as_ref().map(|local| local.join("pnpm"))));
    dirs.extend(var("VOLTA_HOME").or_else(|| local.as_ref().map(|local| local.join("Volta"))).map(|volta| volta.join("bin")));
    if let Some(scoop) = var("SCOOP").or_else(|| var("USERPROFILE").map(|home| home.join("scoop"))) {
        dirs.push(scoop.join("shims"));
        // Scoop's Node keeps npm's global commands in the app's own folder, not in shims.
        for app in ["nodejs-lts", "nodejs"] {
            let current = scoop.join("apps").join(app).join("current");
            dirs.extend([current.join("bin"), current]);
        }
    }
    dirs.extend(var("NVM_SYMLINK"));
    dirs.extend(var("ProgramFiles").map(|programs| programs.join("nodejs")));
    dirs
}

/// `current` with each of `candidates` that `is_dir` and is not on it yet appended, or `None` when
/// nothing is new.
pub(crate) fn widened(current: &[PathBuf], candidates: &[PathBuf], is_dir: impl Fn(&Path) -> bool) -> Option<Vec<PathBuf>> {
    let mut seen: std::collections::HashSet<String> = current.iter().map(|dir| dedupe_key(dir)).collect();
    let added: Vec<PathBuf> = candidates.iter().filter(|dir| is_dir(dir) && seen.insert(dedupe_key(dir))).cloned().collect();
    (!added.is_empty()).then(|| current.iter().cloned().chain(added).collect())
}

/// The key two Path entries share when they name the same folder: Windows paths ignore case and a
/// trailing separator.
pub(crate) fn dedupe_key(dir: &std::path::Path) -> String {
    let text = dir.to_string_lossy();
    let text = text.trim_end_matches(['\\', '/']);
    if cfg!(windows) { text.to_lowercase() } else { text.to_string() }
}

#[cfg(test)]
mod tests {
    use super::{expand, split, tool_dirs, widened};
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn expands_known_names_and_keeps_unknown_ones() {
        let vars = HashMap::from([("USERPROFILE", r"C:\Users\Mesha"), ("SystemRoot", r"C:\Windows")]);
        let lookup = |name: &str| vars.get(name).map(|value| value.to_string());
        assert_eq!(expand(r"%USERPROFILE%\scoop\shims;%SystemRoot%\system32", lookup), r"C:\Users\Mesha\scoop\shims;C:\Windows\system32");
        assert_eq!(expand(r"%NOPE%\bin;100%;%%", lookup), r"%NOPE%\bin;100%;%%");
        assert_eq!(expand("plain", lookup), "plain");
    }

    #[test]
    fn splits_a_path_value() {
        assert_eq!(split(r#"C:\a; ;"C:\Program Files\b";;C:\c"#), [r"C:\a", r"C:\Program Files\b", r"C:\c"].map(PathBuf::from));
    }

    #[test]
    fn finds_scoop_node_globals_and_the_usual_installs() {
        let vars = HashMap::from([
            ("APPDATA", "/u/AppData/Roaming"),
            ("LOCALAPPDATA", "/u/AppData/Local"),
            ("USERPROFILE", "/u"),
            ("ProgramFiles", "/pf"),
        ]);
        let dirs = tool_dirs(|name| vars.get(name).map(OsString::from));
        for expected in [
            "/u/AppData/Roaming/npm",
            "/u/AppData/Local/pnpm",
            "/u/AppData/Local/Volta/bin",
            "/u/scoop/shims",
            "/u/scoop/apps/nodejs-lts/current/bin",
            "/u/scoop/apps/nodejs-lts/current",
            "/pf/nodejs",
        ] {
            assert!(dirs.contains(&PathBuf::from(expected)), "{expected} missing from {dirs:?}");
        }
        assert!(tool_dirs(|_| None).is_empty());
    }

    #[test]
    fn widening_adds_only_new_folders_that_exist() {
        let current = vec![PathBuf::from("/bin"), PathBuf::from("/usr/bin")];
        let existing: HashSet<PathBuf> = ["/bin", "/gh", "/winget"].into_iter().map(PathBuf::from).collect();
        let candidates = ["/bin", "/gh", "/missing", "/winget", "/gh"].map(PathBuf::from);
        let wide = widened(&current, &candidates, |dir| existing.contains(dir)).unwrap();
        assert_eq!(wide, ["/bin", "/usr/bin", "/gh", "/winget"].map(PathBuf::from));
        assert!(widened(&current, &[PathBuf::from("/bin"), PathBuf::from("/missing")], |dir| existing.contains(dir)).is_none());
    }
}
