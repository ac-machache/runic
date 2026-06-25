//! `runic-skills` — progressive-disclosure skills (the Anthropic Agent Skills
//! pattern, runic's design).
//!
//! A skill is a `SKILL.md`: YAML frontmatter (`name`, `description`) + a
//! Markdown body of full instructions. The model sees only a compact **index**
//! (name + one-line description) in its system prompt — rendered by
//! [`skills_prompt_section`] — and calls the [`SkillViewTool`] (`skill_view`)
//! to load a skill's full body, or a sub-file inside its directory, on demand.
//! Cheap context, lazy load — the same shape as MCP-deferred tools and the
//! subagent roster.

mod loader;
pub use loader::{Skills, skills};

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;

use runic_tool::{Tool, ToolContext, ToolResult};

/// A loaded skill.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Full Markdown instructions (the `SKILL.md` body).
    pub body: String,
    /// The skill's directory — sub-file reads (`references/…`) resolve here.
    pub dir: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Frontmatter {
    name: String,
    #[serde(default)]
    description: String,
}

impl Skill {
    /// Parse a `SKILL.md` document located at `dir`.
    pub fn parse(dir: PathBuf, src: &str) -> anyhow::Result<Self> {
        let src = src.trim_start_matches('\u{feff}').trim_start();
        let rest = src
            .strip_prefix("---")
            .ok_or_else(|| anyhow::anyhow!("SKILL.md must start with `---` frontmatter"))?;
        let end = rest
            .find("\n---")
            .ok_or_else(|| anyhow::anyhow!("SKILL.md frontmatter is not terminated by `---`"))?;
        let fm: Frontmatter = serde_yml::from_str(&rest[..end])
            .map_err(|e| anyhow::anyhow!("invalid SKILL.md frontmatter: {e}"))?;
        if fm.name.trim().is_empty() {
            anyhow::bail!("SKILL.md frontmatter is missing `name`");
        }
        let after = &rest[end + 4..];
        let body = after.strip_prefix('\n').unwrap_or(after).trim().to_string();
        Ok(Self {
            name: fm.name,
            description: fm.description,
            body,
            dir,
        })
    }
}

/// A set of skills — the index the model browses + the source `skill_view`
/// reads from.
#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: Vec<Skill>,
}

impl SkillRegistry {
    pub fn new(skills: Vec<Skill>) -> Self {
        Self { skills }
    }

    /// Load every `<root>/<name>/SKILL.md`. Directories without one are
    /// skipped; an invalid `SKILL.md` is logged and skipped (best-effort).
    pub fn from_dir(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let mut skills = Vec::new();
        for entry in std::fs::read_dir(root)?.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let manifest = dir.join("SKILL.md");
            if !manifest.is_file() {
                continue;
            }
            match std::fs::read_to_string(&manifest) {
                Ok(text) => match Skill::parse(dir.clone(), &text) {
                    Ok(skill) => skills.push(skill),
                    Err(e) => {
                        tracing::warn!(path = %manifest.display(), error = %e, "skipping invalid SKILL.md")
                    }
                },
                Err(e) => {
                    tracing::warn!(path = %manifest.display(), error = %e, "cannot read SKILL.md")
                }
            }
        }
        Ok(Self { skills })
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
    pub fn len(&self) -> usize {
        self.skills.len()
    }
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }
    pub fn names(&self) -> Vec<&str> {
        self.skills.iter().map(|s| s.name.as_str()).collect()
    }

    /// All skills (for aggregation, e.g. by the plugin manager).
    pub fn all(&self) -> &[Skill] {
        &self.skills
    }

    /// Narrow to an allow-list (used by subagents declaring `skills: [...]`).
    pub fn scope<S: AsRef<str>>(&self, allowed: &[S]) -> Self {
        let allow: Vec<&str> = allowed.iter().map(|s| s.as_ref()).collect();
        Self {
            skills: self
                .skills
                .iter()
                .filter(|s| allow.contains(&s.name.as_str()))
                .cloned()
                .collect(),
        }
    }
}

/// Render the skills index for the system prompt. The app concatenates this;
/// the loop stays skill-agnostic (same pattern as the MCP-deferred / subagent
/// sections).
pub fn skills_prompt_section(registry: &SkillRegistry) -> String {
    if registry.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "<available-skills>\nEach skill is a focused workflow. To load a skill's \
         full instructions call `skill_view` with its `name`; for a file inside \
         the skill pass `name` + a relative `path`.\n",
    );
    for s in &registry.skills {
        out.push_str(&format!("- {}: {}\n", s.name, s.description));
    }
    out.push_str("</available-skills>");
    out
}

/// `skill_view` — loads a skill's full body, or a sub-file inside its directory.
pub struct SkillViewTool {
    registry: std::sync::Arc<SkillRegistry>,
}

impl SkillViewTool {
    pub fn new(registry: std::sync::Arc<SkillRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for SkillViewTool {
    fn name(&self) -> &str {
        "skill_view"
    }

    fn description(&self) -> &str {
        "Load a skill's full instructions by `name`, or a file inside the \
         skill's directory by also passing a relative `path`."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Skill name from the index." },
                "path": { "type": "string", "description": "Optional file path relative to the skill directory." }
            },
            "required": ["name"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("skill_view requires `name`"));
        };
        let Some(skill) = self.registry.get(name) else {
            return Ok(ToolResult::error(format!("unknown skill '{name}'")));
        };

        match args.get("path").and_then(|v| v.as_str()) {
            None => Ok(ToolResult::ok(skill.body.clone())),
            Some(rel) => match read_subfile(&skill.dir, rel) {
                Ok(content) => Ok(ToolResult::ok(content)),
                Err(e) => Ok(ToolResult::error(e)),
            },
        }
    }
}

/// Read a file inside a skill directory, refusing path traversal / escapes.
fn read_subfile(skill_dir: &Path, rel: &str) -> Result<String, String> {
    if rel.is_empty()
        || rel.starts_with('/')
        || rel
            .split(['/', '\\'])
            .any(|seg| seg == ".." || seg.is_empty())
    {
        return Err(format!("invalid skill path '{rel}'"));
    }
    let target = skill_dir.join(rel);
    // Defense in depth: ensure the resolved path stays under the skill dir.
    match (target.canonicalize(), skill_dir.canonicalize()) {
        (Ok(t), Ok(base)) if t.starts_with(&base) => {
            std::fs::read_to_string(&t).map_err(|e| format!("cannot read '{rel}': {e}"))
        }
        (Ok(_), Ok(_)) => Err(format!("path '{rel}' escapes the skill directory")),
        _ => Err(format!("cannot resolve skill path '{rel}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, desc: &str) -> Skill {
        Skill::parse(
            PathBuf::from("/tmp/x"),
            &format!("---\nname: {name}\ndescription: {desc}\n---\nFull body for {name}."),
        )
        .unwrap()
    }

    #[test]
    fn parses_and_indexes() {
        let s = skill("greeter", "says hello");
        assert_eq!(s.name, "greeter");
        assert_eq!(s.description, "says hello");
        assert_eq!(s.body, "Full body for greeter.");
    }

    #[test]
    fn scope_filters() {
        let reg = SkillRegistry::new(vec![
            skill("a", "does a"),
            skill("b", "does b"),
            skill("c", "does c"),
        ]);
        let scoped = reg.scope(&["a", "c"]);
        assert_eq!(scoped.len(), 2);
        assert!(scoped.get("a").is_some());
        assert!(scoped.get("b").is_none());
    }

    #[test]
    fn prompt_section_lists_index() {
        let reg = SkillRegistry::new(vec![skill("greeter", "says hello")]);
        let section = skills_prompt_section(&reg);
        assert!(section.contains("skill_view"));
        assert!(section.contains("- greeter: says hello"));
        assert!(skills_prompt_section(&SkillRegistry::default()).is_empty());
    }

    #[tokio::test]
    async fn skill_view_returns_body_and_rejects_traversal() {
        let reg = std::sync::Arc::new(SkillRegistry::new(vec![skill("greeter", "hi")]));
        let tool = SkillViewTool::new(reg);
        let ctx = ToolContext::new("u", "s", "r");

        let r = tool
            .execute(serde_json::json!({ "name": "greeter" }), &ctx)
            .await
            .unwrap();
        assert!(r.success);
        assert!(r.output.contains("Full body for greeter"));

        let r = tool
            .execute(serde_json::json!({ "name": "ghost" }), &ctx)
            .await
            .unwrap();
        assert!(!r.success);

        let r = tool
            .execute(
                serde_json::json!({ "name": "greeter", "path": "../../etc/passwd" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!r.success);
        assert!(r.output.contains("invalid skill path"));
    }
}
