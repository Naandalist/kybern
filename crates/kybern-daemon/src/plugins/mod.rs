//! User plugin declarations. Inventory reads metadata only; install uses the target harness, with no shell.
//! Test builds have no real home or executable fallback. Auth and arbitrary plugin options never travel.
mod install;
mod inventory;
mod json_config;
#[cfg(test)]
mod tests;

use crate::state::AppState;
pub use install::install;
use kybern_protocol::*;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

#[derive(Default)]
pub struct PluginRuntime {
    pub lock: tokio::sync::Mutex<()>,
    pub binaries: RwLock<BTreeMap<ProviderKind, PathBuf>>,
}

#[derive(Clone)]
pub(super) struct Context {
    pub home: PathBuf,
    pub env: BTreeMap<String, String>,
}
impl Context {
    pub fn root(&self, provider: ProviderKind) -> PathBuf {
        let kind = match provider {
            ProviderKind::ClaudeCode => SkillHome::Claude,
            ProviderKind::Codex => SkillHome::Codex,
            ProviderKind::Opencode => SkillHome::Opencode,
            ProviderKind::Pi => SkillHome::Pi,
            ProviderKind::Omp => SkillHome::Omp,
            ProviderKind::Cursor => SkillHome::Cursor,
            ProviderKind::Antigravity => SkillHome::Agents,
            ProviderKind::GrokBuild => SkillHome::Agents,
        };
        crate::skills::user_skill_home(kind, &self.env, &self.home)
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| self.home.join(".unavailable"))
    }
    pub fn omp_data(&self) -> PathBuf {
        let profile = self.env.get("OMP_PROFILE").filter(|v| !v.is_empty() && v.as_str() != "default");
        let suffix = profile.map(|p| PathBuf::from("omp/profiles").join(p)).unwrap_or_else(|| PathBuf::from("omp"));
        let xdg = self.env.get("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| self.home.join(".local/share"));
        if xdg.join(&suffix).is_dir() { xdg.join(suffix) } else { self.root(ProviderKind::Omp).parent().unwrap().to_path_buf() }
    }
}

pub async fn inventory(state: &AppState) -> Result<PluginInventory, RpcError> {
    let Some((env, home)) = state.skill_homes.context() else { return Ok(PluginInventory::default()) };
    let mut inventory = tokio::task::spawn_blocking(move || inventory::read(&Context { env, home })).await.map_err(RpcError::internal)?;
    inventory.jobs = state
        .store
        .outbox_list(None, None)
        .map_err(RpcError::internal)?
        .into_iter()
        .rev()
        .filter(|i| i.kind == OutboxKind::PluginInstall)
        .take(100)
        .filter_map(|i| {
            let p: PluginInstallRequest = serde_json::from_value(i.payload).ok()?;
            Some(PluginCopyJob {
                id: i.id,
                provider: p.plugin.provider,
                plugin_id: p.plugin.id,
                target: i.target,
                state: i.state,
                result: i.result.and_then(|v| serde_json::from_value(v).ok()),
                error: i.last_error,
            })
        })
        .collect();
    Ok(inventory)
}
pub(super) fn fingerprint(item: &MachinePlugin) -> String {
    let mut item = item.clone();
    item.fingerprint.clear();
    Sha256::digest(serde_json::to_vec(&item).unwrap_or_default()).iter().map(|b| format!("{b:02x}")).collect()
}
pub(super) fn text(path: &Path) -> Result<String, String> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > 2 * 1024 * 1024 => return Err("Plugin metadata exceeds 2 MiB.".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(e.to_string()),
        _ => (),
    }
    std::fs::read_to_string(path).map_err(|e| e.to_string())
}
pub(super) fn json(path: &Path) -> Result<serde_json::Value, String> {
    let value = text(path)?;
    if value.is_empty() { Ok(serde_json::json!({})) } else { json_config::parse(&value) }
}
pub(super) fn portable_source(value: &str) -> bool {
    if value.is_empty()
        || value.len() > 512
        || value.chars().any(|c| c.is_whitespace() || c.is_control())
        || value.contains(['$', '`', '\\', '?'])
        || value.starts_with(['-', '/', '.', '~'])
    {
        return false;
    }
    // A ref reaches git (and Codex's `--ref`), so it may not look like an option or escape upward.
    let (value, reference) = match value.split_once('#') {
        Some((value, reference)) => (value, Some(reference)),
        None => (value, None),
    };
    if reference.is_some_and(|r| !git_ref(r)) {
        return false;
    }
    if value.starts_with("https://") {
        return reqwest::Url::parse(value).is_ok_and(|u| u.username().is_empty() && u.password().is_none() && u.host_str().is_some());
    }
    if let Some(rest) = value.strip_prefix("git@") {
        return rest.starts_with(|c: char| c.is_ascii_alphanumeric())
            && rest.contains(':')
            && !rest.contains(['@', '#'])
            && !value.contains("://");
    }
    let (base, at_ref) = match value.split_once('@') {
        Some((base, r)) => (base, Some(r)),
        None => (value, None),
    };
    at_ref.is_none_or(git_ref)
        && !(reference.is_some() && at_ref.is_some())
        && base.split('/').count() == 2
        && base.split('/').all(|part| part.starts_with(|c: char| c.is_ascii_alphanumeric()) && !part.contains(".."))
        && base.chars().all(|c| c.is_ascii_alphanumeric() || "-_/.".contains(c))
}
pub(super) fn git_ref(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value.starts_with(|c: char| c.is_ascii_alphanumeric())
        && !value.contains("..")
        && value.chars().all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c))
}
pub(super) fn npm_spec(value: &str) -> bool {
    let value = value.strip_prefix("npm:").unwrap_or(value);
    let name = package_name(value);
    !name.is_empty()
        && !name.starts_with(['-', '.', '/'])
        && !name.contains("..")
        && value.len() <= 256
        && value.chars().all(|c| c.is_ascii_alphanumeric() || "@/._-+^~*".contains(c))
        && name.split('/').count() == if name.starts_with('@') { 2 } else { 1 }
}
pub(super) fn pi_git_source(value: &str) -> bool {
    let uri = value.strip_prefix("git:").unwrap_or(value);
    if uri.contains("://") || uri.starts_with("git@") {
        portable_source(uri)
    } else {
        value.starts_with("git:") && portable_source(&format!("https://{uri}"))
    }
}
pub(super) fn package_name(value: &str) -> &str {
    let value = value.strip_prefix("npm:").unwrap_or(value);
    value.rfind('@').filter(|i| *i > 0).map(|i| &value[..i]).unwrap_or(value)
}
pub(super) fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && !value.starts_with(['-', '.', '/'])
        && !value.contains("..")
        && value.chars().all(|c| c.is_ascii_alphanumeric() || "@/._-:".contains(c))
}
