//! `AgentRegistry` — in-memory index of parsed [`crate::MdAgent`]s.
//!
//! Same shape and discovery rules as [`runic_skills::SkillRegistry`]:
//! pure data after construction, no retained backend reference, tolerates
//! both hierarchical and flat-KV storage layouts.

use runic_storage_backend::{EntryKind, StorageBackend};
use std::collections::HashMap;
use std::sync::Arc;

use crate::MdAgent;

#[derive(Debug, Clone, Default)]
pub struct AgentRegistry {
    agents: HashMap<String, MdAgent>,
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("storage error: {0}")]
    Storage(#[from] runic_storage_backend::StorageError),

    #[error("failed to parse agent at '{path}': {source}")]
    Parse {
        path: String,
        #[source]
        source: crate::ParseError,
    },

    #[error("invalid filesystem config in agent at '{path}': {source}")]
    InvalidFilesystem {
        path: String,
        #[source]
        source: crate::FilesystemConfigError,
    },
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan `root` in `storage`, parse every `{dir}/AGENT.md`, return a
    /// populated registry. Directories without an `AGENT.md` are skipped.
    pub async fn load(storage: Arc<dyn StorageBackend>, root: &str) -> Result<Self, LoadError> {
        let mut agents = HashMap::new();
        let entries = storage.list(root).await?;

        // Same dual-mode logic as SkillRegistry::load — see there for
        // the explanation of why both Directory and File branches exist.
        for entry in &entries {
            let agent_path = match entry.kind {
                EntryKind::Directory => format!("{}/AGENT.md", entry.key),
                EntryKind::File if entry.key.ends_with("/AGENT.md") => entry.key.clone(),
                _ => continue,
            };

            let raw = match storage.read_to_string(&agent_path).await {
                Ok(r) => r,
                Err(_) => continue,
            };

            let mut agent = MdAgent::parse(&raw).map_err(|e| LoadError::Parse {
                path: agent_path.clone(),
                source: e,
            })?;

            // Fail at boot, not mid-session, on a misconfigured
            // filesystem block — the model can't recover and silent
            // fallback would hide the misconfig.
            agent
                .def
                .filesystem
                .validate()
                .map_err(|source| LoadError::InvalidFilesystem {
                    path: agent_path.clone(),
                    source,
                })?;

            agent.dir = std::path::Path::new(&agent_path)
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();

            agents.insert(agent.def.name.clone(), agent);
        }

        Ok(Self { agents })
    }

    /// Insert one agent directly — primarily for tests. Production code
    /// loads via [`Self::load`].
    pub fn insert(&mut self, agent: MdAgent) {
        self.agents.insert(agent.def.name.clone(), agent);
    }

    pub fn get(&self, name: &str) -> Option<&MdAgent> {
        self.agents.get(name)
    }

    /// All agents, sorted by name (deterministic registration order).
    pub fn list(&self) -> Vec<&MdAgent> {
        let mut out: Vec<&MdAgent> = self.agents.values().collect();
        out.sort_by(|a, b| a.def.name.cmp(&b.def.name));
        out
    }

    pub fn len(&self) -> usize {
        self.agents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentDef;
    use runic_storage_backend::MemoryBackend;

    fn agent(name: &str, description: &str) -> MdAgent {
        MdAgent {
            def: AgentDef {
                name: name.into(),
                description: description.into(),
                max_turns: None,
                allowed_tools: Vec::new(),
                skills: Vec::new(),
                filesystem: crate::FilesystemConfig::default(),
            },
            system_prompt: format!("# {name}\n\nDo {name} stuff."),
            dir: name.into(),
        }
    }

    fn valid_agent_md(name: &str, description: &str, body: &str) -> Vec<u8> {
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}").into_bytes()
    }

    #[test]
    fn new_registry_is_empty() {
        let reg = AgentRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(reg.list().is_empty());
        assert!(reg.get("anyone").is_none());
    }

    #[test]
    fn insert_and_get_round_trip() {
        let mut reg = AgentRegistry::new();
        reg.insert(agent("greeter", "says hi"));
        let found = reg.get("greeter").expect("should exist");
        assert_eq!(found.def.description, "says hi");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn list_returns_agents_sorted_by_name() {
        let mut reg = AgentRegistry::new();
        reg.insert(agent("zeta", ""));
        reg.insert(agent("alpha", ""));
        reg.insert(agent("mu", ""));

        let names: Vec<&str> = reg.list().iter().map(|a| a.def.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mu", "zeta"]);
    }

    #[tokio::test]
    async fn load_from_empty_root_yields_empty_registry() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let reg = AgentRegistry::load(storage, "agents").await.unwrap();
        assert!(reg.is_empty());
    }

    #[tokio::test]
    async fn load_discovers_and_parses_each_agent() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        storage
            .write(
                "agents/greeter/AGENT.md",
                &valid_agent_md("greeter", "says hi", "Hi.\n"),
            )
            .await
            .unwrap();
        storage
            .write(
                "agents/researcher/AGENT.md",
                &valid_agent_md("research", "looks things up", "# Research\n"),
            )
            .await
            .unwrap();

        let reg = AgentRegistry::load(storage, "agents").await.unwrap();
        assert_eq!(reg.len(), 2);

        let greeter = reg.get("greeter").expect("greeter present");
        assert_eq!(greeter.dir, "greeter");
        assert!(greeter.system_prompt.contains("Hi."));

        let research = reg.get("research").expect("research present");
        assert_eq!(research.dir, "researcher");
        assert_eq!(research.def.name, "research");
    }

    #[tokio::test]
    async fn load_silently_skips_directories_without_an_agent_md() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        storage
            .write("agents/real/AGENT.md", &valid_agent_md("real", "yes", "body"))
            .await
            .unwrap();
        storage
            .write("agents/notes/scratch.txt", b"unrelated content")
            .await
            .unwrap();

        let reg = AgentRegistry::load(storage, "agents").await.unwrap();
        assert_eq!(reg.len(), 1);
        assert!(reg.get("real").is_some());
    }

    #[tokio::test]
    async fn load_errors_with_path_when_an_agent_is_malformed() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        storage
            .write("agents/broken/AGENT.md", b"this file has no frontmatter")
            .await
            .unwrap();

        let err = AgentRegistry::load(storage, "agents").await.unwrap_err();
        match err {
            LoadError::Parse { path, .. } => {
                assert_eq!(path, "agents/broken/AGENT.md");
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }
}
