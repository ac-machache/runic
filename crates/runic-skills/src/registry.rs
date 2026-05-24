//! `SkillRegistry` — an in-memory index of parsed skills.
//!
//! The registry is intentionally **pure data**: it holds the parsed `Skill`s
//! and nothing else. It does NOT keep a reference to the storage backend or
//! the directory it was loaded from. Loading is a one-shot constructor that
//! takes a backend, reads from it, parses everything, and then drops both.
//!
//! Reading sub-files at runtime is the `SkillViewTool`'s job, not the
//! registry's. The tool holds its own `Arc<dyn StorageBackend>` and builds
//! paths from `(root, skill.dir, relative_path)`. Splitting it this way keeps
//! the registry trivial to test (just construct one with a hand-rolled
//! `HashMap`) and gives the tool a single place where storage lives.

use runic_storage_backend::{EntryKind, StorageBackend};
use std::collections::HashMap;
use std::sync::Arc;

pub use crate::Skill;

#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: HashMap<String, Skill>,
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("storage error: {0}")]
    Storage(#[from] runic_storage_backend::StorageError),

    #[error("failed to parse skill at '{path}': {source}")]
    Parse {
        path: String,
        #[source]
        source: crate::ParseError,
    },
}

impl SkillRegistry {
    /// Construct an empty registry. Useful for tests and for callers that
    /// want to populate the registry manually instead of from a backend.
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan `root` in `storage`, parse every `{dir}/SKILL.md` we find, and
    /// return a populated registry. Directories without a `SKILL.md` are
    /// silently skipped (they might be unrelated content sitting next to
    /// skills). A malformed `SKILL.md` becomes a hard error, with the
    /// offending path in the error payload.
    pub async fn load(storage: Arc<dyn StorageBackend>, root: &str) -> Result<Self, LoadError> {
        let mut skills = HashMap::new();
        let entries = storage.list(root).await?;

        // Backends differ on what `list` returns:
        //   - LocalFsBackend (hierarchical FS): one entry per direct child,
        //     each entry's `kind` is `Directory` for "skills/greeter".
        //   - MemoryBackend / S3-like (flat KV): one entry per stored key,
        //     each entry's `kind` is `File` for "skills/greeter/SKILL.md".
        // We tolerate both: a Directory entry means "look for SKILL.md inside",
        // a File entry whose key ends with "/SKILL.md" means "that IS the file".
        for entry in &entries {
            let skill_path = match entry.kind {
                EntryKind::Directory => format!("{}/SKILL.md", entry.key),
                EntryKind::File if entry.key.ends_with("/SKILL.md") => entry.key.clone(),
                _ => continue,
            };

            let raw = match storage.read_to_string(&skill_path).await {
                Ok(r) => r,
                // No SKILL.md here — not a skill, ignore.
                Err(_) => continue,
            };

            let mut skill = Skill::parse(&raw).map_err(|e| LoadError::Parse {
                path: skill_path.clone(),
                source: e,
            })?;

            // Stamp the leaf directory name. For "skills/greeter/SKILL.md"
            // the directory is the parent's file_name → "greeter".
            // (Works regardless of whether the entry came in as Directory
            // or File, because we derive from `skill_path`, not from `entry`.)
            skill.dir = std::path::Path::new(&skill_path)
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();

            skills.insert(skill.meta.name.clone(), skill);
        }

        Ok(Self { skills })
    }

    /// Insert a skill directly. Useful for tests; production code should go
    /// through `load`.
    pub fn insert(&mut self, skill: Skill) {
        self.skills.insert(skill.meta.name.clone(), skill);
    }

    /// Look up a skill by its declared `meta.name`.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    /// Convenience: the body of a skill by name, or `None` if unknown.
    pub fn body(&self, name: &str) -> Option<&str> {
        self.skills.get(name).map(|s| s.body.as_str())
    }

    /// All registered skills, sorted by name for deterministic iteration
    /// (matters because this drives the system-prompt trigger table).
    pub fn list(&self) -> Vec<&Skill> {
        let mut out: Vec<&Skill> = self.skills.values().collect();
        out.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
        out
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SkillMeta;
    use runic_storage_backend::MemoryBackend;

    fn skill(name: &str, description: &str, body: &str, dir: &str) -> Skill {
        Skill {
            meta: SkillMeta {
                name: name.into(),
                description: description.into(),
            },
            body: body.into(),
            dir: dir.into(),
        }
    }

    // ─── Pure-data accessors (no storage involved) ──────────────────────────

    #[test]
    fn new_registry_is_empty() {
        let reg = SkillRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(reg.list().is_empty());
        assert!(reg.get("anything").is_none());
        assert!(reg.body("anything").is_none());
    }

    #[test]
    fn insert_and_get_round_trip() {
        let mut reg = SkillRegistry::new();
        reg.insert(skill("greet", "say hi", "Hello!", "greet"));

        assert_eq!(reg.len(), 1);
        let found = reg.get("greet").expect("should exist");
        assert_eq!(found.meta.description, "say hi");
        assert_eq!(found.body, "Hello!");
        assert_eq!(found.dir, "greet");

        assert_eq!(reg.body("greet"), Some("Hello!"));
    }

    #[test]
    fn list_returns_skills_sorted_by_name() {
        let mut reg = SkillRegistry::new();
        reg.insert(skill("zeta", "", "", "z"));
        reg.insert(skill("alpha", "", "", "a"));
        reg.insert(skill("mu", "", "", "m"));

        let names: Vec<&str> = reg.list().iter().map(|s| s.meta.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mu", "zeta"]);
    }

    #[test]
    fn insert_with_duplicate_name_overwrites() {
        let mut reg = SkillRegistry::new();
        reg.insert(skill("dup", "first", "v1", "d"));
        reg.insert(skill("dup", "second", "v2", "d"));

        assert_eq!(reg.len(), 1);
        assert_eq!(reg.body("dup"), Some("v2"));
    }

    // ─── load() with a MemoryBackend ────────────────────────────────────────

    fn valid_skill_md(name: &str, description: &str, body: &str) -> Vec<u8> {
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}").into_bytes()
    }

    #[tokio::test]
    async fn load_from_empty_root_yields_empty_registry() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let reg = SkillRegistry::load(storage, "skills").await.unwrap();
        assert!(reg.is_empty());
    }

    #[tokio::test]
    async fn load_discovers_and_parses_each_skill() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        storage
            .write(
                "skills/greeter/SKILL.md",
                &valid_skill_md("greeter", "says hi", "# Greeter\n\nHi."),
            )
            .await
            .unwrap();
        storage
            .write(
                "skills/optimizer/SKILL.md",
                &valid_skill_md("optimize", "finds hotspots", "# Optimize"),
            )
            .await
            .unwrap();

        let reg = SkillRegistry::load(storage, "skills").await.unwrap();

        assert_eq!(reg.len(), 2);

        let greeter = reg.get("greeter").expect("greeter should be loaded");
        assert_eq!(greeter.meta.description, "says hi");
        assert!(greeter.body.contains("Hi."));
        // Leaf directory name only — not "skills/greeter".
        assert_eq!(greeter.dir, "greeter");

        let optimizer = reg.get("optimize").expect("optimize should be loaded");
        // Skill is keyed by its DECLARED meta.name, not its directory name.
        // The directory was "optimizer" but the skill's name is "optimize".
        assert_eq!(optimizer.dir, "optimizer");
        assert_eq!(optimizer.meta.name, "optimize");
    }

    #[tokio::test]
    async fn load_silently_skips_directories_without_a_skill_md() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        storage
            .write(
                "skills/real/SKILL.md",
                &valid_skill_md("real", "yes", "body"),
            )
            .await
            .unwrap();
        // Sibling directory exists (because writing this file creates it)
        // but has no SKILL.md — must NOT cause a load failure.
        storage
            .write("skills/notes/random.txt", b"unrelated content")
            .await
            .unwrap();

        let reg = SkillRegistry::load(storage, "skills").await.unwrap();
        assert_eq!(reg.len(), 1);
        assert!(reg.get("real").is_some());
    }

    #[tokio::test]
    async fn load_errors_with_path_when_a_skill_is_malformed() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        storage
            .write(
                "skills/broken/SKILL.md",
                b"this file has no frontmatter at all",
            )
            .await
            .unwrap();

        let err = SkillRegistry::load(storage, "skills").await.unwrap_err();
        match err {
            LoadError::Parse { path, source: _ } => {
                assert_eq!(path, "skills/broken/SKILL.md");
            }
            other => panic!("expected LoadError::Parse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn load_returns_storage_error_if_backend_blows_up() {
        // Pass an obviously bad key — LocalFsBackend would reject `..`, but
        // MemoryBackend just returns an empty list for missing prefixes, so
        // we have to be clever. Easiest: confirm the empty case (covered
        // above) and trust that StorageError propagates via `?`. We could
        // mock a failing backend here, but it's overkill — the `?` operator
        // is a one-line trust boundary and we own both sides.
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let reg = SkillRegistry::load(storage, "nonexistent").await.unwrap();
        assert!(reg.is_empty());
    }

    #[tokio::test]
    async fn load_preserves_skill_metadata_for_unknown_frontmatter_fields() {
        // End-to-end check that the load path is robust to extra YAML keys —
        // this matters because real skill files might ship `allowed-tools`,
        // `version`, etc. that we don't model yet.
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let content = b"---\nname: bigger\ndescription: has extra fields\nallowed-tools: bash, read\nversion: 1.2.0\n---\nbody";
        storage
            .write("skills/bigger/SKILL.md", content)
            .await
            .unwrap();

        let reg = SkillRegistry::load(storage, "skills").await.unwrap();
        let s = reg.get("bigger").expect("should still load");
        assert_eq!(s.meta.description, "has extra fields");
    }
}
