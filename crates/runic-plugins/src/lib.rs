//! `runic-plugins` — bundle skills, subagents, and commands into discoverable
//! plugins (folder-bundle model; deliberately **not** WASM).
//!
//! A "plugin" is a directory under `<root>/<name>/` that ships any combination
//! of:
//!   - `skills/<skill>/SKILL.md` — skills (progressive-disclosure)
//!   - `agents/<agent>/AGENT.md` — delegatable subagents
//!   - `commands/<cmd>/COMMAND.md` — slash-command prompt templates
//!
//! and, optionally, a `plugin.json` manifest declaring its `name` (the
//! namespace its skills are loaded under), `version`, `description`, and an
//! `enabled` kill-switch.
//!
//! ```text
//! <root>/code-review/
//!   plugin.json                       (optional)
//!   skills/review-diff/SKILL.md
//!   agents/reviewer/AGENT.md
//!   commands/review/COMMAND.md
//! ```
//!
//! Zero-code: drop a folder in and restart. The manager namespaces each
//! plugin's skills by plugin name (`code-review:review-diff`) and aggregates
//! every plugin's contributions for the app to wire.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use runic_commands::{Command, CommandRegistry};
use runic_skills::{SkillSet, source};
use runic_subagent::{AgentDef, AgentRoster};

/// Max chars for a plugin name (it becomes a skill namespace).
const MAX_PLUGIN_NAME: usize = 64;

/// Optional `plugin.json` manifest. Every field is optional; a missing or
/// malformed manifest is treated as absent (folder name, enabled).
#[derive(Debug, Deserialize)]
#[serde(default)]
struct Manifest {
    name: Option<String>,
    version: Option<String>,
    description: Option<String>,
    enabled: bool,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            name: None,
            version: None,
            description: None,
            enabled: true,
        }
    }
}

/// Validate a plugin name → namespace: collapse whitespace, reject empty,
/// reject `:` (the namespace separator), cap length.
fn namespace_of(raw: &str) -> Option<String> {
    let s = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    (!s.is_empty() && !s.contains(':') && s.chars().count() <= MAX_PLUGIN_NAME).then_some(s)
}

/// Read `<dir>/plugin.json` if present; malformed → warn + defaults.
fn read_manifest(dir: &Path) -> Manifest {
    let path = dir.join("plugin.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), error = %e, "invalid plugin.json — using defaults");
            Manifest::default()
        }),
        Err(_) => Manifest::default(), // no manifest is fine
    }
}

/// One plugin's contributions. Skills are loaded lazily (async) at aggregation;
/// agents/commands are loaded eagerly here.
#[derive(Debug, Clone)]
pub struct Plugin {
    /// The namespace its skills load under (manifest `name` or folder name).
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub enabled: bool,
    dir: PathBuf,
    pub agents: AgentRoster,
    pub commands: CommandRegistry,
}

impl Plugin {
    /// Load a plugin directory. Returns `None` (with a warning) if the name is
    /// not a valid namespace.
    fn load(dir: &Path) -> Option<Self> {
        let folder = dir.file_name()?.to_string_lossy().into_owned();
        let manifest = read_manifest(dir);
        let raw = manifest.name.unwrap_or(folder);
        let Some(name) = namespace_of(&raw) else {
            tracing::warn!(
                dir = %dir.display(),
                name = %raw,
                "invalid plugin name (empty / contains ':' / too long) — skipping"
            );
            return None;
        };
        Some(Self {
            name,
            version: manifest.version,
            description: manifest.description,
            enabled: manifest.enabled,
            agents: AgentRoster::from_dir(dir.join("agents")).unwrap_or_default(),
            commands: CommandRegistry::from_dir(dir.join("commands")).unwrap_or_default(),
            dir: dir.to_path_buf(),
        })
    }
}

/// All discovered plugins (sorted by name, deterministic).
#[derive(Debug, Clone, Default)]
pub struct PluginManager {
    plugins: Vec<Plugin>,
}

impl PluginManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Discover every immediate sub-directory of `root` as a plugin.
    pub fn from_dir(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let mut plugins = Vec::new();
        for entry in std::fs::read_dir(root)?.flatten() {
            let path = entry.path();
            if path.is_dir()
                && let Some(p) = Plugin::load(&path)
            {
                plugins.push(p);
            }
        }
        plugins.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self { plugins })
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
    pub fn len(&self) -> usize {
        self.plugins.len()
    }
    pub fn get(&self, name: &str) -> Option<&Plugin> {
        self.plugins.iter().find(|p| p.name == name)
    }
    pub fn names(&self) -> Vec<&str> {
        self.plugins.iter().map(|p| p.name.as_str()).collect()
    }

    /// Load every enabled plugin's skills into one namespaced [`SkillSet`]
    /// (`<plugin>:<skill>`). Disabled plugins contribute nothing.
    pub async fn skills(&self) -> SkillSet {
        let mut sources = HashMap::new();
        for p in self.plugins.iter().filter(|p| p.enabled) {
            let skills_dir = p.dir.join("skills");
            if skills_dir.is_dir() {
                sources.insert(p.name.clone(), source::local(skills_dir));
            }
        }
        SkillSet::load(sources).await
    }

    /// Merge every enabled plugin's subagents (last-by-name wins; warns).
    pub fn agents(&self) -> AgentRoster {
        let mut by_name: BTreeMap<String, AgentDef> = BTreeMap::new();
        for p in self.plugins.iter().filter(|p| p.enabled) {
            for d in p.agents.all() {
                if by_name.insert(d.name.clone(), d.clone()).is_some() {
                    tracing::warn!(agent = %d.name, plugin = %p.name, "agent name collision; later plugin wins");
                }
            }
        }
        AgentRoster::new(by_name.into_values().collect())
    }

    /// Merge every enabled plugin's commands (last-by-name wins; warns).
    pub fn commands(&self) -> CommandRegistry {
        let mut by_name: BTreeMap<String, Command> = BTreeMap::new();
        for p in self.plugins.iter().filter(|p| p.enabled) {
            for c in p.commands.all() {
                if by_name.insert(c.name.clone(), c.clone()).is_some() {
                    tracing::warn!(command = %c.name, plugin = %p.name, "command name collision; later plugin wins");
                }
            }
        }
        CommandRegistry::new(by_name.into_values().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[tokio::test]
    async fn loads_and_aggregates_a_plugin_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // plugin "alpha": one skill + one agent + one command
        write(
            &root.join("alpha/skills/greet/SKILL.md"),
            "---\nname: greet\ndescription: greeting skill\n---\nSay hello.",
        );
        write(
            &root.join("alpha/agents/reviewer/AGENT.md"),
            "---\nname: reviewer\ndescription: reviews\n---\nReview.",
        );
        write(
            &root.join("alpha/commands/hi/COMMAND.md"),
            "---\nname: hi\ndescription: say hi\n---\nSay hi to $ARGUMENTS",
        );
        // plugin "beta": another skill
        write(
            &root.join("beta/skills/calc/SKILL.md"),
            "---\nname: calc\ndescription: math\n---\nDo math.",
        );

        let mgr = PluginManager::from_dir(root).unwrap();
        assert_eq!(mgr.len(), 2);
        assert!(mgr.get("alpha").is_some());

        // skills are namespaced by plugin name
        let skills = mgr.skills().await;
        let mut ids = skills.ids();
        ids.sort();
        assert_eq!(ids, vec!["alpha:greet", "beta:calc"]);

        assert!(mgr.agents().get("reviewer").is_some());
        assert_eq!(
            mgr.commands().resolve("/hi there").as_deref(),
            Some("Say hi to there")
        );
    }

    #[tokio::test]
    async fn manifest_name_overrides_and_disable_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // folder "raw" but manifest renames the namespace to "acme"
        write(&root.join("raw/plugin.json"), r#"{"name": "acme"}"#);
        write(
            &root.join("raw/skills/deploy/SKILL.md"),
            "---\nname: deploy\ndescription: ship\n---\nShip it.",
        );
        // a disabled plugin contributes nothing
        write(&root.join("off/plugin.json"), r#"{"enabled": false}"#);
        write(
            &root.join("off/skills/secret/SKILL.md"),
            "---\nname: secret\ndescription: nope\n---\nNope.",
        );

        let mgr = PluginManager::from_dir(root).unwrap();
        let ids = mgr.skills().await.ids();
        assert_eq!(ids, vec!["acme:deploy"]); // renamed; disabled one absent
        assert!(mgr.get("acme").is_some());
    }

    #[tokio::test]
    async fn malformed_manifest_falls_back_to_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(&root.join("plug/plugin.json"), "{ not valid json ");
        write(
            &root.join("plug/skills/x/SKILL.md"),
            "---\nname: x\ndescription: d\n---\nbody",
        );
        let mgr = PluginManager::from_dir(root).unwrap();
        assert_eq!(mgr.skills().await.ids(), vec!["plug:x"]); // folder name used
    }

    #[tokio::test]
    async fn invalid_namespace_skips_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(&root.join("bad/plugin.json"), r#"{"name": "a:b"}"#); // ':' is reserved
        write(
            &root.join("bad/skills/x/SKILL.md"),
            "---\nname: x\ndescription: d\n---\nbody",
        );
        let mgr = PluginManager::from_dir(root).unwrap();
        assert!(mgr.is_empty());
    }

    #[tokio::test]
    async fn empty_root_is_fine() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = PluginManager::from_dir(tmp.path()).unwrap();
        assert!(mgr.is_empty());
        assert!(mgr.skills().await.is_empty());
    }
}
