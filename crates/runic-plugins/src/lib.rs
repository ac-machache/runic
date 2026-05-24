//! `runic-plugins` — bundle skills and markdown agents into discoverable plugins.
//!
//! A "plugin" is a directory under `~/.runic/plugins/{name}/` that ships any
//! combination of:
//!   - `skills/{skill-name}/SKILL.md` — skills the plugin contributes
//!   - `agents/{agent-name}/AGENT.md` — markdown-defined sub-agents the
//!     plugin contributes
//!
//! ```text
//! ~/.runic/plugins/
//!   code-review/
//!     skills/
//!       review-diff/SKILL.md
//!       review-style/SKILL.md
//!     agents/
//!       reviewer/AGENT.md
//!   ops-toolkit/
//!     skills/
//!       deploy/SKILL.md
//! ```
//!
//! [`PluginManager::load`] discovers every plugin, parses its contents into
//! per-plugin [`runic_skills::SkillRegistry`] / [`runic_agents::AgentRegistry`],
//! and exposes aggregate methods that merge them into a single combined view
//! for handing to the agent.
//!
//! The shape mirrors Claude Code's plugins: drop a folder in, restart,
//! everything inside (skills + agents) becomes available. No Rust required.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use runic_agents::{AgentRegistry, MdAgent};
use runic_skills::{Skill, SkillRegistry};
use runic_storage_backend::StorageBackend;
use tracing::{debug, warn};

/// One discovered plugin.
#[derive(Debug, Clone)]
pub struct Plugin {
    /// Plugin name = its directory name under the plugins root.
    pub name: String,
    /// All skills shipped by this plugin (parsed from its `skills/`).
    pub skills: SkillRegistry,
    /// All markdown sub-agents shipped by this plugin (parsed from its `agents/`).
    pub agents: AgentRegistry,
}

impl Plugin {
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty() && self.agents.is_empty()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("storage error: {0}")]
    Storage(#[from] runic_storage_backend::StorageError),

    #[error("plugin '{plugin}': failed to load skills: {source}")]
    Skills {
        plugin: String,
        #[source]
        source: runic_skills::LoadError,
    },

    #[error("plugin '{plugin}': failed to load agents: {source}")]
    Agents {
        plugin: String,
        #[source]
        source: runic_agents::LoadError,
    },
}

/// All discovered plugins under one root.
///
/// Plugins are kept separately (so callers can introspect "where did this
/// skill come from?") and there are convenience methods that produce merged
/// registries suitable for plugging into the agent.
#[derive(Debug, Clone, Default)]
pub struct PluginManager {
    plugins: BTreeMap<String, Plugin>,
}

impl PluginManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Discover every plugin directly under `root`. Each plugin's `skills/`
    /// and `agents/` subdirs are parsed independently. A plugin whose
    /// `SKILL.md` is malformed surfaces an error referencing that plugin
    /// by name — other plugins still load.
    pub async fn load(
        storage: Arc<dyn StorageBackend>,
        root: &str,
    ) -> Result<Self, LoadError> {
        let names = discover_plugin_names(storage.as_ref(), root).await?;
        let mut plugins = BTreeMap::new();

        for name in names {
            let plugin_root = format!("{root}/{name}");

            let skills = SkillRegistry::load(storage.clone(), &format!("{plugin_root}/skills"))
                .await
                .map_err(|source| LoadError::Skills {
                    plugin: name.clone(),
                    source,
                })?;

            let agents = AgentRegistry::load(storage.clone(), &format!("{plugin_root}/agents"))
                .await
                .map_err(|source| LoadError::Agents {
                    plugin: name.clone(),
                    source,
                })?;

            let plugin = Plugin {
                name: name.clone(),
                skills,
                agents,
            };

            if plugin.is_empty() {
                debug!(
                    plugin = %name,
                    "plugin directory is empty (no skills/ or agents/) — registering anyway"
                );
            } else {
                debug!(
                    plugin = %name,
                    skill_count = plugin.skills.len(),
                    agent_count = plugin.agents.len(),
                    "plugin loaded"
                );
            }
            plugins.insert(name, plugin);
        }

        Ok(Self { plugins })
    }

    pub fn names(&self) -> Vec<&str> {
        self.plugins.keys().map(String::as_str).collect()
    }

    pub fn plugins(&self) -> Vec<&Plugin> {
        self.plugins.values().collect()
    }

    pub fn get(&self, name: &str) -> Option<&Plugin> {
        self.plugins.get(name)
    }

    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    pub fn total_skills(&self) -> usize {
        self.plugins.values().map(|p| p.skills.len()).sum()
    }

    pub fn total_agents(&self) -> usize {
        self.plugins.values().map(|p| p.agents.len()).sum()
    }

    /// All plugin-contributed skills as one merged registry. If two plugins
    /// declare a skill with the same name, the later-loaded one wins (and
    /// a warning is logged).
    pub fn aggregate_skills(&self) -> SkillRegistry {
        let mut combined = SkillRegistry::new();
        let mut seen: BTreeMap<String, String> = BTreeMap::new();
        for (plugin_name, plugin) in &self.plugins {
            for skill in plugin.skills.list() {
                let skill_name = skill.meta.name.clone();
                if let Some(prev) = seen.get(&skill_name) {
                    warn!(
                        skill = %skill_name,
                        previous_plugin = %prev,
                        new_plugin = %plugin_name,
                        "duplicate skill name across plugins — later plugin wins"
                    );
                }
                seen.insert(skill_name, plugin_name.clone());
                combined.insert(clone_skill(skill));
            }
        }
        combined
    }

    /// All plugin-contributed agents as one merged registry. Same collision
    /// semantics as [`Self::aggregate_skills`].
    pub fn aggregate_agents(&self) -> AgentRegistry {
        let mut combined = AgentRegistry::new();
        let mut seen: BTreeMap<String, String> = BTreeMap::new();
        for (plugin_name, plugin) in &self.plugins {
            for agent in plugin.agents.list() {
                let agent_name = agent.def.name.clone();
                if let Some(prev) = seen.get(&agent_name) {
                    warn!(
                        agent = %agent_name,
                        previous_plugin = %prev,
                        new_plugin = %plugin_name,
                        "duplicate agent name across plugins — later plugin wins"
                    );
                }
                seen.insert(agent_name, plugin_name.clone());
                combined.insert(clone_agent(agent));
            }
        }
        combined
    }
}

/// Discover plugin names under `root`. Works for both backend semantics:
///   - hierarchical (LocalFs): `list(root)` returns Directory entries
///     whose leaf names are plugin names
///   - flat KV (Memory/S3-style): `list(root)` returns File entries with
///     full keys like `plugins/foo/skills/bar/SKILL.md`; we extract the
///     first segment after `root` and dedupe
async fn discover_plugin_names(
    storage: &dyn StorageBackend,
    root: &str,
) -> Result<Vec<String>, runic_storage_backend::StorageError> {
    let entries = storage.list(root).await?;
    let mut names = BTreeSet::new();

    let trimmed_root = root.trim_end_matches('/');

    for entry in &entries {
        // Strip the root prefix and take the first segment as the plugin name.
        let after_root = entry
            .key
            .strip_prefix(trimmed_root)
            .map(|s| s.trim_start_matches('/'))
            .unwrap_or(&entry.key);

        let plugin_name = match after_root.split_once('/') {
            Some((head, _)) => head,
            None => after_root,
        };

        if plugin_name.is_empty() {
            continue;
        }
        names.insert(plugin_name.to_string());
    }

    Ok(names.into_iter().collect())
}

// ─── Cloning helpers ────────────────────────────────────────────────────────
// The upstream SkillRegistry / AgentRegistry only expose `&Skill` / `&MdAgent`
// via `list()`. For aggregation we need to deep-clone each one into the
// merged registry. Skill and MdAgent both impl Clone, so this is trivial —
// these helpers exist mostly to give the call site a name.

fn clone_skill(s: &Skill) -> Skill {
    s.clone()
}

fn clone_agent(a: &MdAgent) -> MdAgent {
    a.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use runic_storage_backend::MemoryBackend;

    fn valid_skill_md(name: &str, description: &str, body: &str) -> Vec<u8> {
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}").into_bytes()
    }

    fn valid_agent_md(name: &str, description: &str, body: &str) -> Vec<u8> {
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}").into_bytes()
    }

    #[tokio::test]
    async fn new_manager_is_empty() {
        let m = PluginManager::new();
        assert!(m.is_empty());
        assert_eq!(m.total_skills(), 0);
        assert_eq!(m.total_agents(), 0);
    }

    #[tokio::test]
    async fn load_from_empty_root_yields_empty_manager() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let m = PluginManager::load(storage, "plugins").await.unwrap();
        assert!(m.is_empty());
    }

    #[tokio::test]
    async fn load_discovers_one_plugin_with_skills_and_agents() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        storage
            .write(
                "plugins/code-review/skills/diff-check/SKILL.md",
                &valid_skill_md("diff-check", "checks a diff", "Look at the diff."),
            )
            .await
            .unwrap();
        storage
            .write(
                "plugins/code-review/agents/reviewer/AGENT.md",
                &valid_agent_md("reviewer", "reviews code", "You are a reviewer."),
            )
            .await
            .unwrap();

        let m = PluginManager::load(storage, "plugins").await.unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m.names(), vec!["code-review"]);

        let cr = m.get("code-review").expect("plugin present");
        assert_eq!(cr.skills.len(), 1);
        assert_eq!(cr.agents.len(), 1);
        assert!(!cr.is_empty());
    }

    #[tokio::test]
    async fn load_discovers_multiple_plugins() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        storage
            .write(
                "plugins/alpha/skills/foo/SKILL.md",
                &valid_skill_md("foo", "foo skill", "body"),
            )
            .await
            .unwrap();
        storage
            .write(
                "plugins/beta/agents/bar/AGENT.md",
                &valid_agent_md("bar", "bar agent", "body"),
            )
            .await
            .unwrap();
        storage
            .write(
                "plugins/gamma/skills/baz/SKILL.md",
                &valid_skill_md("baz", "baz skill", "body"),
            )
            .await
            .unwrap();

        let m = PluginManager::load(storage, "plugins").await.unwrap();
        assert_eq!(m.len(), 3);
        assert_eq!(m.names(), vec!["alpha", "beta", "gamma"]);
        assert_eq!(m.total_skills(), 2); // alpha + gamma
        assert_eq!(m.total_agents(), 1); // beta
    }

    #[tokio::test]
    async fn aggregate_skills_combines_across_plugins() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        storage
            .write(
                "plugins/p1/skills/a/SKILL.md",
                &valid_skill_md("a", "a", "body"),
            )
            .await
            .unwrap();
        storage
            .write(
                "plugins/p2/skills/b/SKILL.md",
                &valid_skill_md("b", "b", "body"),
            )
            .await
            .unwrap();

        let m = PluginManager::load(storage, "plugins").await.unwrap();
        let combined = m.aggregate_skills();
        assert_eq!(combined.len(), 2);
        assert!(combined.get("a").is_some());
        assert!(combined.get("b").is_some());
    }

    #[tokio::test]
    async fn aggregate_agents_combines_across_plugins() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        storage
            .write(
                "plugins/p1/agents/a/AGENT.md",
                &valid_agent_md("a", "a", "body"),
            )
            .await
            .unwrap();
        storage
            .write(
                "plugins/p2/agents/b/AGENT.md",
                &valid_agent_md("b", "b", "body"),
            )
            .await
            .unwrap();

        let m = PluginManager::load(storage, "plugins").await.unwrap();
        let combined = m.aggregate_agents();
        assert_eq!(combined.len(), 2);
        assert!(combined.get("a").is_some());
        assert!(combined.get("b").is_some());
    }

    #[tokio::test]
    async fn aggregate_handles_duplicate_skill_names_last_wins() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        // Both plugins ship a skill named "shared" — second one wins by sort order.
        storage
            .write(
                "plugins/p1/skills/shared/SKILL.md",
                &valid_skill_md("shared", "from p1", "p1 body"),
            )
            .await
            .unwrap();
        storage
            .write(
                "plugins/p2/skills/shared/SKILL.md",
                &valid_skill_md("shared", "from p2", "p2 body"),
            )
            .await
            .unwrap();

        let m = PluginManager::load(storage, "plugins").await.unwrap();
        let combined = m.aggregate_skills();
        // Plugins iterate in BTreeMap order — p2 comes after p1, so p2 wins.
        let s = combined.get("shared").expect("present");
        assert_eq!(s.meta.description, "from p2");
        assert_eq!(combined.len(), 1);
    }

    #[tokio::test]
    async fn plugin_with_no_subdirs_is_loaded_as_empty() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        // A bare plugin dir with only a README — should still register
        // (just empty).
        storage
            .write("plugins/empty/README.md", b"# Empty Plugin")
            .await
            .unwrap();

        let m = PluginManager::load(storage, "plugins").await.unwrap();
        assert_eq!(m.len(), 1);
        let p = m.get("empty").expect("present");
        assert!(p.is_empty());
    }

    #[tokio::test]
    async fn malformed_skill_in_a_plugin_returns_targeted_error() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        storage
            .write("plugins/broken/skills/bad/SKILL.md", b"no frontmatter here")
            .await
            .unwrap();

        let err = PluginManager::load(storage, "plugins").await.unwrap_err();
        match err {
            LoadError::Skills { plugin, .. } => assert_eq!(plugin, "broken"),
            other => panic!("expected Skills error for 'broken', got {other:?}"),
        }
    }
}
