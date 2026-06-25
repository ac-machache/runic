//! `SkillSet` — the skills one agent can see, loaded from a map of namespaced
//! [`SkillSource`]s. Different tenant → different map → different set.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;

use crate::security;
use crate::source::{self, SkillSource};

/// One loaded skill — static, read-only config.
#[derive(Debug, Clone)]
pub struct Skill {
    /// The source it came from (the map key), e.g. `"acme"`.
    pub namespace: String,
    /// From `SKILL.md` frontmatter (or the folder name as fallback).
    pub name: String,
    /// One-line description shown in the prompt index.
    pub description: String,
    /// Full `SKILL.md` instructions (read once at load).
    pub body: String,
    /// The source-relative folder, for sub-file reads.
    pub(crate) entry: String,
}

impl Skill {
    /// Collision-free id used in the index and by `skill_view`: `"namespace:name"`
    /// (or just `"name"` when the namespace is empty).
    pub fn id(&self) -> String {
        if self.namespace.is_empty() {
            self.name.clone()
        } else {
            format!("{}:{}", self.namespace, self.name)
        }
    }
}

#[derive(Debug, Deserialize)]
struct Frontmatter {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
}

/// Parse one `SKILL.md`, applying the safety checks. `entry` is the folder name
/// (used as the name fallback).
fn parse_skill(namespace: &str, entry: &str, src: &str) -> anyhow::Result<Skill> {
    let src = src.trim_start_matches('\u{feff}').trim_start();
    let rest = src
        .strip_prefix("---")
        .ok_or_else(|| anyhow::anyhow!("SKILL.md must start with `---` frontmatter"))?;
    let end = rest
        .find("\n---")
        .ok_or_else(|| anyhow::anyhow!("SKILL.md frontmatter is not terminated by `---`"))?;
    let fm: Frontmatter = serde_yml::from_str(&rest[..end])
        .map_err(|e| anyhow::anyhow!("invalid SKILL.md frontmatter: {e}"))?;

    let mut name = security::sanitize_single_line(&fm.name);
    if name.is_empty() {
        name = security::sanitize_single_line(entry); // fall back to the folder name
    }
    let description = security::sanitize_single_line(&fm.description);

    security::validate_len(&name, security::MAX_NAME, "name")?;
    security::validate_len(&description, security::MAX_DESCRIPTION, "description")?;
    let qualified = if namespace.is_empty() {
        name.clone()
    } else {
        format!("{namespace}:{name}")
    };
    security::validate_len(&qualified, security::MAX_QUALIFIED_NAME, "qualified name")?;

    let after = &rest[end + 4..];
    let body = after.strip_prefix('\n').unwrap_or(after).trim().to_string();
    Ok(Skill {
        namespace: namespace.to_string(),
        name,
        description,
        body,
        entry: entry.to_string(),
    })
}

/// What one agent can see. Build a different one per tenant/agent.
#[derive(Clone, Default)]
pub struct SkillSet {
    skills: Vec<Skill>,
    sources: HashMap<String, Arc<dyn SkillSource>>,
}

impl SkillSet {
    /// Load skills from a map of `namespace -> source`. Sources can be any mix
    /// of local/S3. Best-effort: unreadable sources and non-conforming
    /// `SKILL.md` files are logged and skipped.
    pub async fn load(sources: HashMap<String, Arc<dyn SkillSource>>) -> Self {
        let mut skills = Vec::new();
        for (namespace, src) in &sources {
            let entries = match src.entries().await {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(namespace, error = %e, "skill source unreadable — skipping");
                    continue;
                }
            };
            let (mut loaded, mut dropped) = (0usize, 0usize);
            for entry in entries {
                let manifest = format!("{entry}/SKILL.md");
                let text = match src.read(&manifest).await {
                    Ok(t) => t,
                    Err(_) => continue, // a folder without a SKILL.md just isn't a skill
                };
                match parse_skill(namespace, &entry, &text) {
                    Ok(skill) => {
                        skills.push(skill);
                        loaded += 1;
                    }
                    Err(e) => {
                        tracing::warn!(namespace, entry, error = %e, "non-conforming SKILL.md — dropped");
                        dropped += 1;
                    }
                }
            }
            tracing::debug!(namespace, loaded, dropped, "loaded skills from source");
        }
        tracing::info!(
            sources = sources.len(),
            skills = skills.len(),
            "skills loaded"
        );
        Self { skills, sources }
    }

    /// Single-namespace local convenience.
    pub async fn load_dir(namespace: &str, dir: impl Into<PathBuf>) -> Self {
        let sources = HashMap::from([(namespace.to_string(), source::local(dir))]);
        Self::load(sources).await
    }

    /// Narrow to an allow-list of ids — a finer per-agent filter.
    pub fn scope<S: AsRef<str>>(&self, allowed: &[S]) -> SkillSet {
        let allow: Vec<&str> = allowed.iter().map(|s| s.as_ref()).collect();
        let skills: Vec<Skill> = self
            .skills
            .iter()
            .filter(|s| allow.contains(&s.id().as_str()))
            .cloned()
            .collect();
        let used: HashSet<&str> = skills.iter().map(|s| s.namespace.as_str()).collect();
        let sources = self
            .sources
            .iter()
            .filter(|(k, _)| used.contains(k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        SkillSet { skills, sources }
    }

    /// The compact index injected into the system prompt (id + description).
    pub fn prompt_section(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "<available-skills>\nEach skill is a focused workflow. To load a skill's \
             full instructions call `skill_view` with its `name`; for a file inside \
             the skill pass `name` + a relative `path`.\n",
        );
        for s in &self.skills {
            out.push_str(&format!("- {}: {}\n", s.id(), s.description));
        }
        out.push_str("</available-skills>");
        out
    }

    pub fn get(&self, id: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.id() == id)
    }

    pub fn ids(&self) -> Vec<String> {
        self.skills.iter().map(|s| s.id()).collect()
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Read a sub-file inside a skill's folder, through that skill's source.
    pub(crate) async fn read_subfile(&self, skill: &Skill, rel: &str) -> anyhow::Result<String> {
        security::safe_rel(rel)?;
        let src = self
            .sources
            .get(&skill.namespace)
            .ok_or_else(|| anyhow::anyhow!("no source for namespace '{}'", skill.namespace))?;
        src.read(&format!("{}/{}", skill.entry, rel)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    /// An in-memory skill source — stands in for "the cloud" in multi-source tests.
    struct MapSource {
        files: HashMap<String, String>,
    }
    impl MapSource {
        fn arc(files: &[(&str, &str)]) -> Arc<dyn SkillSource> {
            Arc::new(Self {
                files: files
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            })
        }
    }
    #[async_trait]
    impl SkillSource for MapSource {
        async fn entries(&self) -> anyhow::Result<Vec<String>> {
            let mut top: HashSet<String> = HashSet::new();
            for k in self.files.keys() {
                if let Some((dir, _)) = k.split_once('/') {
                    top.insert(dir.to_string());
                }
            }
            Ok(top.into_iter().collect())
        }
        async fn read(&self, rel: &str) -> anyhow::Result<String> {
            crate::security::safe_rel(rel)?;
            self.files
                .get(rel)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("not found: {rel}"))
        }
    }

    fn skill_md(name: &str, desc: &str) -> String {
        format!("---\nname: {name}\ndescription: {desc}\n---\nFull body for {name}.")
    }

    #[tokio::test]
    async fn loads_namespaces_and_parses() {
        let src = MapSource::arc(&[("greeter/SKILL.md", &skill_md("greeter", "says hi"))]);
        let set = SkillSet::load(HashMap::from([("core".to_string(), src)])).await;
        assert_eq!(set.len(), 1);
        let s = set.get("core:greeter").unwrap();
        assert_eq!(s.name, "greeter");
        assert_eq!(s.description, "says hi");
        assert_eq!(s.body, "Full body for greeter.");
    }

    #[tokio::test]
    async fn load_mixes_two_sources() {
        let local_like = MapSource::arc(&[("deploy/SKILL.md", &skill_md("deploy", "ship it"))]);
        let cloud_like = MapSource::arc(&[("onboard/SKILL.md", &skill_md("onboard", "welcome"))]);
        let set = SkillSet::load(HashMap::from([
            ("core".to_string(), local_like),
            ("acme".to_string(), cloud_like),
        ]))
        .await;
        let mut ids = set.ids();
        ids.sort();
        assert_eq!(ids, vec!["acme:onboard", "core:deploy"]);
    }

    #[tokio::test]
    async fn sanitizes_and_drops_oversize() {
        // multi-line description collapses to one line
        let messy = MapSource::arc(&[(
            "x/SKILL.md",
            "---\nname: x\ndescription: |\n  line one\n  line two\n---\nbody",
        )]);
        let set = SkillSet::load(HashMap::from([("ns".to_string(), messy)])).await;
        assert_eq!(set.get("ns:x").unwrap().description, "line one line two");

        // an over-long name is dropped
        let big = MapSource::arc(&[("y/SKILL.md", &skill_md(&"n".repeat(65), "d"))]);
        let set = SkillSet::load(HashMap::from([("ns".to_string(), big)])).await;
        assert!(set.is_empty());
    }

    #[tokio::test]
    async fn scope_filters_and_prunes_sources() {
        let src = MapSource::arc(&[
            ("a/SKILL.md", &skill_md("a", "da")),
            ("b/SKILL.md", &skill_md("b", "db")),
        ]);
        let set = SkillSet::load(HashMap::from([("ns".to_string(), src)])).await;
        let scoped = set.scope(&["ns:a"]);
        assert_eq!(scoped.len(), 1);
        assert!(scoped.get("ns:a").is_some());
        assert!(scoped.get("ns:b").is_none());
    }

    #[tokio::test]
    async fn prompt_section_lists_ids() {
        let src = MapSource::arc(&[("greeter/SKILL.md", &skill_md("greeter", "says hi"))]);
        let set = SkillSet::load(HashMap::from([("core".to_string(), src)])).await;
        let section = set.prompt_section();
        assert!(section.contains("skill_view"));
        assert!(section.contains("- core:greeter: says hi"));
        assert!(SkillSet::default().prompt_section().is_empty());
    }
}
