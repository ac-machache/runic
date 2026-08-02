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
    /// Collision-free id used in the index and by `read_skill`: `"namespace:name"`
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

pub(crate) const DEFAULT_TAG: &str = "available-skills";
pub(crate) const TOOL_NAME: &str = "read_skill";

/// What one agent can see. Build a different one per tenant/agent.
#[derive(Clone, Default)]
pub struct SkillSet {
    skills: Vec<Skill>,
    sources: HashMap<String, Arc<dyn SkillSource>>,
    tag: Option<String>,
    intro: Option<String>,
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
        Self {
            skills,
            sources,
            ..Self::default()
        }
    }

    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    pub fn intro(mut self, text: impl Into<String>) -> Self {
        self.intro = Some(text.into());
        self
    }

    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    /// Single-namespace local convenience.
    pub async fn load_dir(namespace: &str, dir: impl Into<PathBuf>) -> Self {
        let sources = HashMap::from([(namespace.to_string(), source::local(dir))]);
        Self::load(sources).await
    }

    /// Narrow to an allow-list of ids — a finer per-agent filter.
    pub fn merge<Sets>(sets: Sets) -> SkillSet
    where
        Sets: IntoIterator<Item = Arc<SkillSet>>,
    {
        let mut skills: Vec<Skill> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut sources: HashMap<String, Arc<dyn SkillSource>> = HashMap::new();
        let mut merged = SkillSet::default();
        for set in sets {
            for skill in &set.skills {
                if seen.insert(skill.id()) {
                    skills.push(skill.clone());
                }
            }
            for (namespace, source) in &set.sources {
                sources
                    .entry(namespace.clone())
                    .or_insert_with(|| source.clone());
            }
            merged.tag = merged.tag.or_else(|| set.tag.clone());
            merged.intro = merged.intro.or_else(|| set.intro.clone());
        }
        merged.skills = skills;
        merged.sources = sources;
        merged
    }

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
        SkillSet {
            skills,
            sources,
            tag: self.tag.clone(),
            intro: self.intro.clone(),
        }
    }

    pub fn scope_glob<S: AsRef<str>>(&self, patterns: &[S]) -> SkillSet {
        let pats: Vec<&str> = patterns.iter().map(|s| s.as_ref()).collect();
        let allowed = |skill: &Skill| {
            let id = skill.id();
            pats.iter().any(|p| {
                *p == "*"
                    || p.strip_suffix(":*") == Some(skill.namespace.as_str())
                    || *p == id.as_str()
            })
        };
        let skills: Vec<Skill> = self.skills.iter().filter(|s| allowed(s)).cloned().collect();
        let used: HashSet<&str> = skills.iter().map(|s| s.namespace.as_str()).collect();
        let sources = self
            .sources
            .iter()
            .filter(|(k, _)| used.contains(k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        SkillSet {
            skills,
            sources,
            tag: self.tag.clone(),
            intro: self.intro.clone(),
        }
    }

    /// The compact index injected into the system prompt (id + description).
    pub fn prompt_section(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let tag = self.tag.as_deref().unwrap_or(DEFAULT_TAG);
        let intro = match &self.intro {
            Some(text) => text.clone(),
            None => format!(
                "Each skill is a focused workflow. To read a skill's full instructions \
                 call `{TOOL_NAME}` with its `name`; for a file inside the skill pass \
                 `name` + a relative `path`."
            ),
        };
        let mut out = format!("<{tag}>\n{intro}\n");
        for s in &self.skills {
            out.push_str(&format!("- {}: {}\n", s.id(), s.description));
        }
        out.push_str(&format!("</{tag}>"));
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
    async fn scope_glob_supports_wildcards_namespaces_and_exact_ids() {
        let src = MapSource::arc(&[
            ("pipeline/SKILL.md", &skill_md("pipeline", "crm p")),
            ("followup/SKILL.md", &skill_md("followup", "crm f")),
        ]);
        let other = MapSource::arc(&[("deep/SKILL.md", &skill_md("deep", "research d"))]);
        let set = SkillSet::load(HashMap::from([
            ("crm".to_string(), src),
            ("research".to_string(), other),
        ]))
        .await;

        assert_eq!(set.scope_glob(&["*"]).len(), 3);

        let ns = set.scope_glob(&["crm:*"]);
        let mut ids = ns.ids();
        ids.sort();
        assert_eq!(ids, vec!["crm:followup", "crm:pipeline"]);

        let exact = set.scope_glob(&["crm:pipeline", "research:deep"]);
        let mut ids = exact.ids();
        ids.sort();
        assert_eq!(ids, vec!["crm:pipeline", "research:deep"]);

        assert!(set.scope_glob::<&str>(&[]).is_empty());
        assert!(set.scope_glob(&["ghost:*"]).is_empty());
    }

    #[tokio::test]
    async fn scope_glob_prunes_sources_to_used_namespaces() {
        let crm = MapSource::arc(&[
            ("pipeline/SKILL.md", &skill_md("pipeline", "p")),
            ("pipeline/extra.md", "detail"),
        ]);
        let research = MapSource::arc(&[("deep/SKILL.md", &skill_md("deep", "d"))]);
        let set = SkillSet::load(HashMap::from([
            ("crm".to_string(), crm),
            ("research".to_string(), research),
        ]))
        .await;

        let scoped = set.scope_glob(&["crm:*"]);
        let skill = scoped.get("crm:pipeline").unwrap().clone();
        assert_eq!(
            scoped.read_subfile(&skill, "extra.md").await.unwrap(),
            "detail"
        );
        assert!(scoped.get("research:deep").is_none());
    }

    #[tokio::test]
    async fn scope_glob_bare_namespace_name_is_not_a_wildcard() {
        let src = MapSource::arc(&[("pipeline/SKILL.md", &skill_md("pipeline", "p"))]);
        let set = SkillSet::load(HashMap::from([("crm".to_string(), src)])).await;
        assert!(set.scope_glob(&["crm"]).is_empty());
        assert_eq!(set.scope_glob(&["crm:pipeline"]).len(), 1);
    }

    #[tokio::test]
    async fn scope_glob_overlapping_patterns_do_not_duplicate() {
        let src = MapSource::arc(&[("pipeline/SKILL.md", &skill_md("pipeline", "p"))]);
        let set = SkillSet::load(HashMap::from([("crm".to_string(), src)])).await;
        let scoped = set.scope_glob(&["*", "crm:*", "crm:pipeline"]);
        assert_eq!(scoped.ids(), vec!["crm:pipeline"]);
    }

    #[tokio::test]
    async fn prompt_section_lists_ids() {
        let src = MapSource::arc(&[("greeter/SKILL.md", &skill_md("greeter", "says hi"))]);
        let set = SkillSet::load(HashMap::from([("core".to_string(), src)])).await;
        let section = set.prompt_section();
        assert!(section.starts_with("<available-skills>"));
        assert!(section.contains("read_skill"));
        assert!(section.contains("- core:greeter: says hi"));
        assert!(SkillSet::default().prompt_section().is_empty());
    }

    #[tokio::test]
    async fn customized_voice_renders_tag_and_intro() {
        let src = MapSource::arc(&[("deploy/SKILL.md", &skill_md("deploy", "ship it"))]);
        let set = SkillSet::load(HashMap::from([("core".to_string(), src)]))
            .await
            .tag("playbooks")
            .intro("Consult the relevant playbook before acting:");

        let section = set.prompt_section();
        assert!(section.starts_with("<playbooks>\n"));
        assert!(section.ends_with("</playbooks>"));
        assert!(section.contains("Consult the relevant playbook before acting:"));
        assert!(section.contains("- core:deploy: ship it"));

        let tool = Arc::new(set).skill_tool().unwrap();
        assert_eq!(
            tool.name(),
            TOOL_NAME,
            "the tool's identity is fixed; only the prompt voice is configurable"
        );
    }

    #[tokio::test]
    async fn the_default_intro_names_the_tool_the_model_will_actually_see() {
        let src = MapSource::arc(&[("deploy/SKILL.md", &skill_md("deploy", "ship it"))]);
        let set = Arc::new(SkillSet::load(HashMap::from([("core".to_string(), src)])).await);

        let advertised = set.skill_tool().unwrap().name().to_string();
        assert_eq!(advertised, TOOL_NAME);
        assert!(
            set.prompt_section()
                .contains(&format!("call `{advertised}` with its `name`")),
            "the prompt must name the tool that is actually registered"
        );
    }

    #[tokio::test]
    async fn scoping_preserves_the_configured_voice() {
        let src = MapSource::arc(&[
            ("a/SKILL.md", &skill_md("a", "da")),
            ("b/SKILL.md", &skill_md("b", "db")),
        ]);
        let set = SkillSet::load(HashMap::from([("ns".to_string(), src)]))
            .await
            .tag("playbooks");

        for narrowed in [set.scope(&["ns:a"]), set.scope_glob(&["ns:*"])] {
            let section = narrowed.prompt_section();
            assert!(section.starts_with("<playbooks>"));
            assert_eq!(Arc::new(narrowed).skill_tool().unwrap().name(), TOOL_NAME);
        }
    }

    #[tokio::test]
    async fn merge_keeps_the_first_configured_voice_and_can_be_restated() {
        let first = MapSource::arc(&[("a/SKILL.md", &skill_md("a", "da"))]);
        let second = MapSource::arc(&[("b/SKILL.md", &skill_md("b", "db"))]);
        let plain = SkillSet::load(HashMap::from([("one".to_string(), first)])).await;
        let voiced = SkillSet::load(HashMap::from([("two".to_string(), second)]))
            .await
            .tag("playbooks")
            .intro("second voice");

        let merged = SkillSet::merge([Arc::new(plain), Arc::new(voiced)]);
        assert!(merged.prompt_section().starts_with("<playbooks>"));
        assert!(merged.prompt_section().contains("second voice"));

        let restated = merged.intro("one voice for all");
        assert!(restated.prompt_section().contains("one voice for all"));
        assert!(!restated.prompt_section().contains("second voice"));
    }

    #[tokio::test]
    async fn the_docs_examples_load_as_real_skills() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/examples");
        let set = SkillSet::load_dir("docs", dir).await;
        let mut ids = set.ids();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "docs:code-review",
                "docs:customer-onboarding",
                "docs:deploy"
            ]
        );

        let tool = Arc::new(set).skill_tool().unwrap();
        let ctx = runic_tool::ToolContext::new("u", "s", "r");
        let body = tool
            .execute(serde_json::json!({ "name": "docs:deploy" }), &ctx)
            .await
            .unwrap();
        assert!(body.text().contains("canary"));
        let sub = tool
            .execute(
                serde_json::json!({ "name": "docs:deploy", "path": "checklist.md" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(sub.text().contains("Pre-flight"));
    }

    #[tokio::test]
    async fn skills_accessor_supports_hand_rolled_sections() {
        let src = MapSource::arc(&[("deploy/SKILL.md", &skill_md("deploy", "ship it"))]);
        let set = SkillSet::load(HashMap::from([("core".to_string(), src)])).await;
        let mine: Vec<String> = set
            .skills()
            .iter()
            .map(|s| format!("* {} — {}", s.id(), s.description))
            .collect();
        assert_eq!(mine, vec!["* core:deploy — ship it"]);
    }
}
