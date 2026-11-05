//! Workspace-aware skill discovery for providers that do not expose a native
//! catalog API. Codex is queried through app-server first; this scanner is the
//! fallback and the source for Claude, Cursor, pi, and omp. OpenCode is also
//! queried through its CLI first so its configured roots and precedence win.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use directories::BaseDirs;
use kybern_protocol::{ProviderKind, SkillHome, SkillInfo, SkillScope};

const MAX_DEPTH: usize = 10;
const MAX_ENTRIES: usize = 10_000;
const MAX_SKILL_BYTES: u64 = 1_000_000;
const MAX_TOTAL_BYTES: u64 = 8_000_000;

#[derive(Clone)]
struct Root {
    path: PathBuf,
    scope: SkillScope,
    depth: usize,
}

#[derive(Default)]
struct Budget {
    entries: usize,
    bytes: u64,
}

#[derive(Default)]
pub(crate) struct Frontmatter {
    pub(crate) display_name: Option<String>,
    pub(crate) description: Option<String>,
    user_invocable: bool,
}

/// Discover invocable skills in the same provider-specific roots used by the
/// corresponding CLIs. Unreadable or malformed entries are skipped so opening
/// a composer never fails because one local skill is broken.
pub async fn list(cwd: &Path, provider: ProviderKind, env: &BTreeMap<String, String>) -> Result<Vec<SkillInfo>> {
    let cwd = cwd.to_path_buf();
    let env = env.clone();
    Ok(tokio::task::spawn_blocking(move || scan(&cwd, provider, &env)).await?)
}

pub async fn plugin_skills(roots: Vec<(String, PathBuf)>) -> Result<Vec<SkillInfo>> {
    Ok(tokio::task::spawn_blocking(move || {
        let mut found = Vec::new();
        let mut visited = HashSet::new();
        let mut budget = Budget::default();
        for (namespace, path) in roots {
            let root = Root { path: path.join("skills"), scope: SkillScope::App, depth: MAX_DEPTH };
            let start = found.len();
            visit(&root.path, &root, 0, &mut visited, &mut budget, &mut found);
            for skill in &mut found[start..] {
                skill.name = format!("{namespace}:{}", skill.name);
            }
            if budget.entries >= MAX_ENTRIES || budget.bytes >= MAX_TOTAL_BYTES {
                break;
            }
        }
        found
    })
    .await?)
}

fn scan(cwd: &Path, provider: ProviderKind, env: &BTreeMap<String, String>) -> Vec<SkillInfo> {
    let home = env.get("HOME").map(PathBuf::from).or_else(|| BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()));
    let roots = roots(cwd, provider, env, home.as_deref());
    let mut found = Vec::new();
    let mut seen_names = HashSet::new();
    let mut visited = HashSet::new();
    let mut budget = Budget::default();

    for root in roots {
        visit(&root.path, &root, 0, &mut visited, &mut budget, &mut found);
        if budget.entries >= MAX_ENTRIES || budget.bytes >= MAX_TOTAL_BYTES {
            break;
        }
    }

    found.retain(|skill| seen_names.insert(skill.name.to_ascii_lowercase()));
    found.sort_by_key(|skill| skill.name.to_ascii_lowercase());
    found
}

fn roots(cwd: &Path, provider: ProviderKind, env: &BTreeMap<String, String>, home: Option<&Path>) -> Vec<Root> {
    let mut roots = Vec::new();
    let mut add = |path: PathBuf, scope, depth| roots.push(Root { path, scope, depth });

    match provider {
        ProviderKind::ClaudeCode => {
            if let Some(config) = claude_config_dir(cwd, env, home) {
                add(config.join("skills"), SkillScope::User, 1);
            }
            add(cwd.join(".claude/skills"), SkillScope::Project, 1);
        }
        ProviderKind::Cursor => {
            for relative in [".cursor/skills", ".agents/skills", ".codex/skills", ".claude/skills"] {
                add(cwd.join(relative), SkillScope::Project, MAX_DEPTH);
            }
            if let Some(home) = home {
                for relative in [".cursor/skills", ".agents/skills", ".codex/skills", ".claude/skills"] {
                    add(home.join(relative), SkillScope::User, MAX_DEPTH);
                }
            }
        }
        ProviderKind::Codex => {
            for relative in [".codex/skills", ".agents/skills"] {
                add(cwd.join(relative), SkillScope::Project, MAX_DEPTH);
            }
            if let Some(home) = home {
                let codex =
                    env.get("CODEX_HOME").filter(|value| !value.is_empty()).map(PathBuf::from).unwrap_or_else(|| home.join(".codex"));
                add(codex.join("skills"), SkillScope::User, MAX_DEPTH);
                add(home.join(".agents/skills"), SkillScope::User, MAX_DEPTH);
            }
        }
        ProviderKind::Antigravity => {
            // `<workspace>/.agents/skills/<name>/SKILL.md` is in `agy`'s own help; its per-user
            // folder was seen on disk (`~/.gemini/antigravity-cli/skills`).
            add(cwd.join(".agents/skills"), SkillScope::Project, MAX_DEPTH);
            if let Some(home) = home {
                add(home.join(".agents/skills"), SkillScope::User, MAX_DEPTH);
                add(home.join(".gemini/antigravity-cli/skills"), SkillScope::User, MAX_DEPTH);
            }
        }
        ProviderKind::GrokBuild => {
            add(cwd.join(".agents/skills"), SkillScope::Project, MAX_DEPTH);
            if let Some(home) = home {
                add(home.join(".agents/skills"), SkillScope::User, MAX_DEPTH);
            }
        }
        ProviderKind::Opencode => {
            for relative in [".opencode/skills", ".claude/skills", ".agents/skills"] {
                add(cwd.join(relative), SkillScope::Project, MAX_DEPTH);
            }
            if let Some(home) = home {
                let config =
                    env.get("XDG_CONFIG_HOME").filter(|value| !value.is_empty()).map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
                add(config.join("opencode/skills"), SkillScope::User, MAX_DEPTH);
                add(home.join(".claude/skills"), SkillScope::User, MAX_DEPTH);
                add(home.join(".agents/skills"), SkillScope::User, MAX_DEPTH);
            }
        }
        ProviderKind::Pi => {
            add(cwd.join(".pi/skills"), SkillScope::Project, MAX_DEPTH);
            let agent_dir = pi_agent_dir(cwd, env, home, ".pi/agent");
            if let Some(agent_dir) = &agent_dir {
                add(agent_dir.join("skills"), SkillScope::User, MAX_DEPTH);
                for path in configured_skill_paths(&agent_dir.join("settings.json"), home) {
                    add(path, SkillScope::User, MAX_DEPTH);
                }
            }
            for path in configured_skill_paths(&cwd.join(".pi/settings.json"), home) {
                add(path, SkillScope::Project, MAX_DEPTH);
            }
        }
        ProviderKind::Omp => {
            for relative in [".agents/skills", ".pi/skills", ".omp/skills"] {
                add(cwd.join(relative), SkillScope::Project, MAX_DEPTH);
            }
            if let Some(home) = home {
                add(home.join(".agents/skills"), SkillScope::User, MAX_DEPTH);
                add(home.join(".pi/agent/skills"), SkillScope::User, MAX_DEPTH);
            }
            let profile = kybern_drivers::omp_profile::resolve(env).ok();
            let agent_dir = match profile.as_deref() {
                Some(profile) if !profile.is_empty() => home.map(|home| home.join(".omp/profiles").join(profile).join("agent")),
                Some(_) => pi_agent_dir(cwd, env, home, ".omp/agent"),
                None => None,
            };
            if let Some(agent_dir) = agent_dir {
                add(agent_dir.join("skills"), SkillScope::User, MAX_DEPTH);
            }
        }
    }
    roots
}

/// A harness's user skill folder (`SkillHome`), resolved the way [`roots`] resolves the user roots of the
/// harness that owns it. `home` is the user's home folder; the variables come from `env`. Used by the skill
/// copies between machines (`network/skill_sync.rs`), which only touch these folders.
pub(crate) fn user_skill_home(home_kind: SkillHome, env: &BTreeMap<String, String>, home: &Path) -> Option<PathBuf> {
    let var = |name: &str| env.get(name).map(|value| value.trim()).filter(|value| !value.is_empty());
    // Relative overrides are taken from the home folder: the daemon has no project here.
    let path = match home_kind {
        SkillHome::Agents => home.join(".agents/skills"),
        SkillHome::Claude => claude_config_dir(home, env, Some(home))?.join("skills"),
        SkillHome::Codex => {
            var("CODEX_HOME").map(|value| expand_home(Path::new(value), Some(home))).unwrap_or_else(|| home.join(".codex")).join("skills")
        }
        SkillHome::Cursor => home.join(".cursor/skills"),
        SkillHome::Opencode => var("XDG_CONFIG_HOME")
            .map(|value| expand_home(Path::new(value), Some(home)))
            .unwrap_or_else(|| home.join(".config"))
            .join("opencode/skills"),
        SkillHome::Pi => pi_agent_dir(home, env, Some(home), ".pi/agent")?.join("skills"),
        SkillHome::Omp => match kybern_drivers::omp_profile::resolve(env).ok().as_deref() {
            Some(profile) if !profile.is_empty() => home.join(".omp/profiles").join(profile).join("agent/skills"),
            _ => home.join(".omp/agent/skills"),
        },
    };
    Some(if path.is_absolute() { path } else { home.join(path) })
}

fn pi_agent_dir(cwd: &Path, env: &BTreeMap<String, String>, home: Option<&Path>, fallback: &str) -> Option<PathBuf> {
    if let Some(value) = env.get("PI_CODING_AGENT_DIR").map(|value| value.trim()).filter(|value| !value.is_empty()) {
        let path = expand_home(Path::new(value), home);
        return Some(if path.is_absolute() { path } else { cwd.join(path) });
    }
    home.map(|home| home.join(fallback))
}

fn configured_skill_paths(settings_path: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let Ok(contents) = std::fs::read_to_string(settings_path) else { return Vec::new() };
    let Ok(settings) = serde_json::from_str::<serde_json::Value>(&contents) else { return Vec::new() };
    let Some(paths) = settings.get("skills").and_then(serde_json::Value::as_array) else { return Vec::new() };
    let base = settings_path.parent().unwrap_or_else(|| Path::new("."));
    paths
        .iter()
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(|path| {
            let expanded = expand_home(Path::new(path), home);
            if expanded.is_absolute() { expanded } else { base.join(expanded) }
        })
        .collect()
}

fn expand_home(path: &Path, home: Option<&Path>) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return home.map(Path::to_path_buf).unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = text.strip_prefix("~/")
        && let Some(home) = home
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

fn claude_config_dir(cwd: &Path, env: &BTreeMap<String, String>, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(value) = env.get("CLAUDE_CONFIG_DIR").map(|value| value.trim()).filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        return Some(if path.is_absolute() { path } else { cwd.join(path) });
    }
    home.map(|home| home.join(".claude"))
}

fn visit(directory: &Path, root: &Root, depth: usize, visited: &mut HashSet<PathBuf>, budget: &mut Budget, found: &mut Vec<SkillInfo>) {
    if depth > root.depth || budget.entries >= MAX_ENTRIES || budget.bytes >= MAX_TOTAL_BYTES {
        return;
    }
    let canonical = match std::fs::canonicalize(directory) {
        Ok(path) => path,
        Err(_) => return,
    };
    if !visited.insert(canonical.clone()) {
        return;
    }

    let skill_path = canonical.join("SKILL.md");
    if let Ok(metadata) = std::fs::metadata(&skill_path)
        && metadata.is_file()
        && metadata.len() <= MAX_SKILL_BYTES
        && budget.bytes.saturating_add(metadata.len()) <= MAX_TOTAL_BYTES
        && let Ok(contents) = std::fs::read_to_string(&skill_path)
    {
        budget.bytes += metadata.len();
        let frontmatter = parse_frontmatter(&contents);
        if frontmatter.user_invocable
            && let Some(name) = canonical.file_name().and_then(|name| name.to_str()).map(str::trim).filter(|name| !name.is_empty())
        {
            found.push(SkillInfo {
                name: name.to_string(),
                display_name: frontmatter.display_name.filter(|value| value != name),
                description: frontmatter.description,
                path: skill_path.to_string_lossy().to_string(),
                scope: effective_scope(&skill_path, root.scope),
                enabled: true,
            });
        }
    }

    let Ok(entries) = std::fs::read_dir(&canonical) else { return };
    let mut children: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).filter(|path| path.is_dir()).collect();
    children.sort();
    for child in children {
        budget.entries += 1;
        if budget.entries >= MAX_ENTRIES {
            return;
        }
        visit(&child, root, depth + 1, visited, budget, found);
    }
}

fn effective_scope(path: &Path, fallback: SkillScope) -> SkillScope {
    let normalized = path.to_string_lossy().replace('\\', "/");
    if normalized.contains("/.codex/plugins/") || normalized.contains("/.agents/plugins/") {
        SkillScope::App
    } else if normalized.contains("/.system/") {
        SkillScope::System
    } else {
        fallback
    }
}

pub(crate) fn parse_frontmatter(contents: &str) -> Frontmatter {
    let mut result = Frontmatter { user_invocable: true, ..Frontmatter::default() };
    let normalized = contents.replace("\r\n", "\n");
    let Some(rest) = normalized.strip_prefix("---\n") else { return result };
    let Some((frontmatter, _)) = rest.split_once("\n---") else { return result };
    let lines: Vec<&str> = frontmatter.lines().collect();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if line.starts_with(char::is_whitespace) || line.trim_start().starts_with('#') {
            index += 1;
            continue;
        }
        let Some((key, raw_value)) = line.split_once(':') else {
            index += 1;
            continue;
        };
        let key = key.trim();
        let raw_value = raw_value.trim();
        let mut value = scalar(raw_value);
        if (raw_value.starts_with('|') || raw_value.starts_with('>')) && key == "description" {
            let mut chunks = Vec::new();
            index += 1;
            while index < lines.len() && (lines[index].starts_with(' ') || lines[index].is_empty()) {
                let chunk = lines[index].trim();
                if !chunk.is_empty() {
                    chunks.push(chunk);
                }
                index += 1;
            }
            value = chunks.join(" ");
            index = index.saturating_sub(1);
        }
        match key {
            "name" if !value.is_empty() => result.display_name = Some(value),
            "description" if !value.is_empty() => result.description = Some(compact_description(&value)),
            "user-invocable" if is_false(&value) => result.user_invocable = false,
            _ => {}
        }
        index += 1;
    }
    result
}

fn scalar(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 && matches!((value.as_bytes()[0], value.as_bytes()[value.len() - 1]), (b'\'', b'\'') | (b'"', b'"')) {
        value[1..value.len() - 1].trim().to_string()
    } else {
        value.to_string()
    }
}

fn is_false(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "false" | "no" | "off" | "n" | "0")
}

fn compact_description(value: &str) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= 240 {
        compact
    } else {
        let shortened: String = compact.chars().take(239).collect();
        format!("{}…", shortened.trim_end())
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_frontmatter, roots};
    use kybern_protocol::ProviderKind;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    struct TempTree(PathBuf);

    impl TempTree {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("kybern-skills-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("create temp tree");
            Self(path)
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reads_common_skill_frontmatter() {
        let parsed = parse_frontmatter("---\nname: Better UI\ndescription: >\n  Make dense interfaces\n  easier to scan.\n---\nBody");
        assert_eq!(parsed.display_name.as_deref(), Some("Better UI"));
        assert_eq!(parsed.description.as_deref(), Some("Make dense interfaces easier to scan."));
        assert!(parsed.user_invocable);
    }

    #[test]
    fn hides_provider_reserved_skills() {
        let parsed = parse_frontmatter("---\nuser-invocable: no\n---\nBody");
        assert!(!parsed.user_invocable);
    }

    #[test]
    fn every_supported_harness_has_a_skill_catalog() {
        let cwd = PathBuf::from("/workspace");
        let home = PathBuf::from("/home/test");
        for provider in [
            ProviderKind::ClaudeCode,
            ProviderKind::Codex,
            ProviderKind::Opencode,
            ProviderKind::Pi,
            ProviderKind::Omp,
            ProviderKind::Cursor,
            ProviderKind::Antigravity,
        ] {
            assert!(!roots(&cwd, provider, &BTreeMap::new(), Some(&home)).is_empty(), "{provider} has no skill roots");
        }
    }

    #[test]
    fn opencode_catalog_includes_its_compatible_skill_roots() {
        let cwd = PathBuf::from("/workspace");
        let home = PathBuf::from("/home/test");
        let paths: Vec<_> = roots(&cwd, ProviderKind::Opencode, &BTreeMap::new(), Some(&home)).into_iter().map(|root| root.path).collect();
        assert!(paths.contains(&cwd.join(".opencode/skills")));
        assert!(paths.contains(&cwd.join(".claude/skills")));
        assert!(paths.contains(&cwd.join(".agents/skills")));
        assert!(paths.contains(&home.join(".config/opencode/skills")));
    }

    #[test]
    fn pi_catalog_follows_configured_external_skill_directories() {
        let tree = TempTree::new();
        let cwd = tree.0.join("workspace");
        let home = tree.0.join("home");
        std::fs::create_dir_all(cwd.join(".pi")).expect("project settings dir");
        std::fs::create_dir_all(home.join(".pi/agent")).expect("user settings dir");
        std::fs::write(cwd.join(".pi/settings.json"), r#"{"skills":["../.agents/skills"]}"#).expect("project settings");
        std::fs::write(home.join(".pi/agent/settings.json"), r#"{"skills":["~/.claude/skills"]}"#).expect("user settings");

        let paths: Vec<_> = roots(&cwd, ProviderKind::Pi, &BTreeMap::new(), Some(&home)).into_iter().map(|root| root.path).collect();
        assert!(paths.contains(&cwd.join(".pi/../.agents/skills")));
        assert!(paths.contains(&home.join(".claude/skills")));
        assert!(!paths.contains(&home.join(".agents/skills")), "Pi should not advertise unconfigured cross-provider roots");
    }
    #[test]
    fn omp_named_profile_skills_do_not_fall_back_to_the_default_profile() {
        let cwd = PathBuf::from("/workspace");
        let home = PathBuf::from("/home/test");
        let env = [("OMP_PROFILE".into(), "work".into()), ("PI_CODING_AGENT_DIR".into(), "/other/agent".into())].into();
        let paths: Vec<_> = roots(&cwd, ProviderKind::Omp, &env, Some(&home)).into_iter().map(|root| root.path).collect();
        assert!(paths.contains(&home.join(".omp/profiles/work/agent/skills")));
        assert!(!paths.contains(&home.join(".omp/agent/skills")));
        assert!(!paths.contains(&PathBuf::from("/other/agent/skills")));
    }
}
