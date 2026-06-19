//! `runic-plugins` — bundle skills, subagents, and commands into discoverable
//! plugins (folder-bundle model; deliberately **not** WASM).
//!
//! A "plugin" is a directory under `<root>/<name>/` that ships any combination
//! of:
//!   - `skills/<skill>/SKILL.md` — skills (progressive-disclosure)
//!   - `agents/<agent>/AGENT.md` — delegatable subagents
//!   - `commands/<cmd>/COMMAND.md` — slash-command prompt templates
//!
//! ```text
//! <root>/code-review/
//!   skills/review-diff/SKILL.md
//!   agents/reviewer/AGENT.md
//!   commands/review/COMMAND.md
//! ```
//!
//! Zero-code: drop a folder in and restart. The manager aggregates every
//! plugin's contributions into one registry of each kind for the app to wire.

use std::collections::BTreeMap;
use std::path::Path;

use runic_commands::{Command, CommandRegistry};
use runic_skills::{Skill, SkillRegistry};
use runic_subagent::{AgentDef, AgentRoster};

/// One plugin's contributions.
#[derive(Debug, Clone, Default)]
pub struct Plugin {
    pub name: String,
    pub skills: SkillRegistry,
    pub agents: AgentRoster,
    pub commands: CommandRegistry,
}

impl Plugin {
    /// Load a single plugin directory (`<dir>/{skills,agents,commands}`).
    /// Missing sub-directories are simply empty.
    pub fn from_dir(name: impl Into<String>, dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref();
        Self {
            name: name.into(),
            skills: SkillRegistry::from_dir(dir.join("skills")).unwrap_or_default(),
            agents: AgentRoster::from_dir(dir.join("agents")).unwrap_or_default(),
            commands: CommandRegistry::from_dir(dir.join("commands")).unwrap_or_default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty() && self.agents.is_empty() && self.commands.is_empty()
    }
}

/// All discovered plugins, keyed by name (sorted, deterministic).
#[derive(Debug, Clone, Default)]
pub struct PluginManager {
    plugins: BTreeMap<String, Plugin>,
}

impl PluginManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Discover every immediate sub-directory of `root` as a plugin.
    pub fn from_dir(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let mut plugins = BTreeMap::new();
        for entry in std::fs::read_dir(root)?.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            plugins.insert(name.clone(), Plugin::from_dir(name, &path));
        }
        Ok(Self { plugins })
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
    pub fn len(&self) -> usize {
        self.plugins.len()
    }
    pub fn get(&self, name: &str) -> Option<&Plugin> {
        self.plugins.get(name)
    }
    pub fn names(&self) -> Vec<&str> {
        self.plugins.keys().map(String::as_str).collect()
    }

    /// Merge every plugin's skills (last-by-name wins; warns on collision).
    pub fn aggregate_skills(&self) -> SkillRegistry {
        let mut by_name: BTreeMap<String, Skill> = BTreeMap::new();
        for (plugin, p) in &self.plugins {
            for s in p.skills.all() {
                if by_name.insert(s.name.clone(), s.clone()).is_some() {
                    tracing::warn!(skill = %s.name, plugin = %plugin, "skill name collision; later plugin wins");
                }
            }
        }
        SkillRegistry::new(by_name.into_values().collect())
    }

    /// Merge every plugin's subagents (last-by-name wins).
    pub fn aggregate_agents(&self) -> AgentRoster {
        let mut by_name: BTreeMap<String, AgentDef> = BTreeMap::new();
        for (plugin, p) in &self.plugins {
            for d in p.agents.all() {
                if by_name.insert(d.name.clone(), d.clone()).is_some() {
                    tracing::warn!(agent = %d.name, plugin = %plugin, "agent name collision; later plugin wins");
                }
            }
        }
        AgentRoster::new(by_name.into_values().collect())
    }

    /// Merge every plugin's commands (last-by-name wins).
    pub fn aggregate_commands(&self) -> CommandRegistry {
        let mut by_name: BTreeMap<String, Command> = BTreeMap::new();
        for (plugin, p) in &self.plugins {
            for c in p.commands.all() {
                if by_name.insert(c.name.clone(), c.clone()).is_some() {
                    tracing::warn!(command = %c.name, plugin = %plugin, "command name collision; later plugin wins");
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

    #[test]
    fn loads_and_aggregates_a_plugin_bundle() {
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

        let skills = mgr.aggregate_skills();
        assert_eq!(skills.len(), 2); // greet + calc
        assert!(skills.get("greet").is_some());
        assert!(skills.get("calc").is_some());

        let agents = mgr.aggregate_agents();
        assert!(agents.get("reviewer").is_some());

        let commands = mgr.aggregate_commands();
        assert_eq!(commands.resolve("/hi there").as_deref(), Some("Say hi to there"));
    }

    #[test]
    fn empty_root_is_fine() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = PluginManager::from_dir(tmp.path()).unwrap();
        assert!(mgr.is_empty());
        assert!(mgr.aggregate_skills().is_empty());
    }
}
