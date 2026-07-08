//! `AgentDef` — a delegatable subagent, declared in a Markdown `AGENT.md`:
//! YAML frontmatter (name, description, provider/model, allowed tools, skills,
//! turn cap) + a Markdown body that becomes the child's system prompt.
//!
//! ```text
//! ---
//! name: code-reviewer
//! description: Reviews a diff for bugs, style, and security
//! provider: haiku
//! max-turns: 8
//! tools: [read_file, grep]
//! ---
//! You are a focused code reviewer. When given a diff, ...
//! ```

use std::path::Path;
use std::sync::Arc;

use runic_tool::Tool;
use serde::Deserialize;

use crate::security;

/// YAML frontmatter fields (kebab-case on the wire).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct Frontmatter {
    name: String,
    #[serde(default)]
    description: String,
    /// Named provider override (resolved by the app's `SubagentBuilder`).
    #[serde(default)]
    provider: Option<String>,
    /// Model id override.
    #[serde(default)]
    model: Option<String>,
    /// Tool allow-list by name. Must be a subset of the parent's tools — the
    /// `SubagentBuilder` rejects any name not in the parent pool (no escalation).
    #[serde(default, alias = "tools")]
    allowed_tools: Vec<String>,
    /// Skill allow-list by name.
    #[serde(default)]
    skills: Vec<String>,
    /// Turn cap for the child run.
    #[serde(default)]
    max_turns: Option<u32>,
}

/// A parsed subagent definition.
#[derive(Debug, Clone)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub allowed_tools: Vec<String>,
    pub skills: Vec<String>,
    pub max_turns: Option<u32>,
    /// The Markdown body — the child's system prompt.
    pub system_prompt: String,
}

impl AgentDef {
    /// Parse an `AGENT.md` document: `---` YAML frontmatter `---` then body.
    pub fn parse_markdown(src: &str) -> anyhow::Result<Self> {
        let src = src.trim_start_matches('\u{feff}').trim_start();
        let rest = src
            .strip_prefix("---")
            .ok_or_else(|| anyhow::anyhow!("AGENT.md must start with `---` frontmatter"))?;
        // Closing fence on its own line.
        let end = rest
            .find("\n---")
            .ok_or_else(|| anyhow::anyhow!("AGENT.md frontmatter is not terminated by `---`"))?;
        let fm_str = &rest[..end];
        // Skip past the closing `\n---` and the rest of that line.
        let after = &rest[end + 4..];
        let body = after.strip_prefix('\n').unwrap_or(after);

        let fm: Frontmatter = serde_yml::from_str(fm_str)
            .map_err(|e| anyhow::anyhow!("invalid AGENT.md frontmatter: {e}"))?;

        let name = security::sanitize_single_line(&fm.name);
        security::validate_len(&name, security::MAX_NAME, "name")?;
        let description = security::sanitize_single_line(&fm.description);
        security::validate_len(&description, security::MAX_DESCRIPTION, "description")?;

        let provider = sanitize_opt(fm.provider, security::MAX_PROVIDER, "provider")?;
        let model = sanitize_opt(fm.model, security::MAX_MODEL, "model")?;
        let allowed_tools =
            security::sanitize_list(fm.allowed_tools, security::MAX_LIST, "allowed_tools")?;
        let skills = security::sanitize_list(fm.skills, security::MAX_LIST, "skills")?;

        Ok(Self {
            name,
            description,
            provider,
            model,
            allowed_tools,
            skills,
            max_turns: fm.max_turns,
            system_prompt: body.trim().to_string(),
        })
    }

    pub fn scope_tools(&self, pool: &[Arc<dyn Tool>]) -> Vec<Arc<dyn Tool>> {
        pool.iter()
            .filter(|t| self.allowed_tools.iter().any(|p| glob_matches(p, t.name())))
            .cloned()
            .collect()
    }
}

fn glob_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

/// Sanitize an optional single-line override, dropping it if it sanitizes to
/// empty and length-checking it otherwise.
fn sanitize_opt(value: Option<String>, max: usize, field: &str) -> anyhow::Result<Option<String>> {
    let Some(raw) = value else { return Ok(None) };
    let s = security::sanitize_single_line(&raw);
    if s.is_empty() {
        return Ok(None);
    }
    security::validate_len(&s, max, field)?;
    Ok(Some(s))
}

/// A set of delegatable subagents — the `delegate` tool's roster.
#[derive(Debug, Clone, Default)]
pub struct AgentRoster {
    defs: Vec<AgentDef>,
}

impl AgentRoster {
    pub fn new(defs: Vec<AgentDef>) -> Self {
        Self { defs }
    }

    /// Load every `<dir>/<name>/AGENT.md` and any top-level `<dir>/*.md`.
    /// Unreadable/invalid files are logged and skipped (best-effort).
    pub fn from_dir(dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let mut defs = Vec::new();
        let mut consider = |path: &Path| {
            if let Ok(text) = std::fs::read_to_string(path) {
                match AgentDef::parse_markdown(&text) {
                    Ok(def) => defs.push(def),
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "skipping invalid AGENT.md")
                    }
                }
            }
        };
        for entry in std::fs::read_dir(dir)?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let agent_md = path.join("AGENT.md");
                if agent_md.is_file() {
                    consider(&agent_md);
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                consider(&path);
            }
        }
        Ok(Self { defs })
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn get(&self, name: &str) -> Option<&AgentDef> {
        self.defs.iter().find(|d| d.name == name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.defs.iter().map(|d| d.name.as_str()).collect()
    }

    /// All definitions (for aggregation, e.g. by the plugin manager).
    pub fn all(&self) -> &[AgentDef] {
        &self.defs
    }

    /// `- name: description` lines for the delegate tool's description.
    pub fn roster_lines(&self) -> String {
        self.defs
            .iter()
            .map(|d| format!("- {}: {}", d.name, d.description))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let src = "---\nname: reviewer\ndescription: reviews diffs\nprovider: haiku\nmax-turns: 8\ntools: [read_file, grep]\n---\nYou are a reviewer.\nBe terse.";
        let def = AgentDef::parse_markdown(src).unwrap();
        assert_eq!(def.name, "reviewer");
        assert_eq!(def.description, "reviews diffs");
        assert_eq!(def.provider.as_deref(), Some("haiku"));
        assert_eq!(def.max_turns, Some(8));
        assert_eq!(def.allowed_tools, vec!["read_file", "grep"]);
        assert_eq!(def.system_prompt, "You are a reviewer.\nBe terse.");
    }

    #[test]
    fn rejects_missing_frontmatter() {
        assert!(AgentDef::parse_markdown("no frontmatter here").is_err());
        assert!(AgentDef::parse_markdown("---\ndescription: x\n---\nbody").is_err()); // no name
    }

    #[test]
    fn hardens_fields() {
        // multi-line description collapses to one line
        let def = AgentDef::parse_markdown(
            "---\nname: a\ndescription: |\n  line one\n  line two\n---\nbody",
        )
        .unwrap();
        assert_eq!(def.description, "line one line two");

        // missing/empty description is rejected
        assert!(AgentDef::parse_markdown("---\nname: a\n---\nbody").is_err());

        // over-long name / model rejected
        let long = "n".repeat(65);
        assert!(
            AgentDef::parse_markdown(&format!("---\nname: {long}\ndescription: d\n---\nb"))
                .is_err()
        );
        let long_model = "m".repeat(129);
        assert!(
            AgentDef::parse_markdown(&format!(
                "---\nname: a\ndescription: d\nmodel: {long_model}\n---\nb"
            ))
            .is_err()
        );

        // allowed_tools deduped + sanitized
        let def = AgentDef::parse_markdown(
            "---\nname: a\ndescription: d\ntools: [read, read, grep]\n---\nb",
        )
        .unwrap();
        assert_eq!(def.allowed_tools, vec!["read", "grep"]);
    }

    #[test]
    fn skills_list_parses_dedups_and_preserves_glob_syntax() {
        let def = AgentDef::parse_markdown(
            "---\nname: a\ndescription: d\nskills: [\"*\", crm:pipeline, crm:pipeline]\n---\nb",
        )
        .unwrap();
        assert_eq!(def.skills, vec!["*", "crm:pipeline"]);

        let none = AgentDef::parse_markdown("---\nname: a\ndescription: d\n---\nb").unwrap();
        assert!(none.skills.is_empty());
    }

    use async_trait::async_trait;
    use runic_tool::{ToolContext, ToolResult};

    struct T(&'static str);
    #[async_trait]
    impl Tool for T {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "t"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            _a: serde_json::Value,
            _c: &ToolContext,
        ) -> anyhow::Result<ToolResult> {
            Ok(ToolResult::ok(""))
        }
    }

    fn tool_pool(names: &[&'static str]) -> Vec<Arc<dyn Tool>> {
        names
            .iter()
            .map(|&n| Arc::new(T(n)) as Arc<dyn Tool>)
            .collect()
    }

    fn scoped(md: &str, pool: &[Arc<dyn Tool>]) -> Vec<String> {
        AgentDef::parse_markdown(md)
            .unwrap()
            .scope_tools(pool)
            .iter()
            .map(|t| t.name().to_string())
            .collect()
    }

    #[test]
    fn scope_tools_matches_exact_names_and_drops_unknowns() {
        let pool = tool_pool(&["a", "b", "mcp__crm__search"]);
        assert_eq!(
            scoped(
                "---\nname: x\ndescription: d\ntools: [a, ghost]\n---\nb",
                &pool
            ),
            vec!["a"]
        );
    }

    #[test]
    fn scope_tools_empty_grants_nothing_and_star_grants_the_whole_pool() {
        let pool = tool_pool(&["a", "b", "mcp__crm__search"]);
        assert!(scoped("---\nname: x\ndescription: d\n---\nb", &pool).is_empty());
        assert_eq!(
            scoped(
                "---\nname: x\ndescription: d\ntools: [\"*\"]\n---\nb",
                &pool
            )
            .len(),
            3
        );
    }

    #[test]
    fn scope_tools_prefix_glob_is_bounded_to_its_namespace() {
        let pool = tool_pool(&[
            "mcp__crm__search",
            "mcp__crm__update",
            "mcp__crmx__thing",
            "mcp__crm_admin__wipe",
        ]);
        assert_eq!(
            scoped(
                "---\nname: x\ndescription: d\ntools: [mcp__crm__*]\n---\nb",
                &pool
            ),
            vec!["mcp__crm__search", "mcp__crm__update"]
        );
    }

    #[test]
    fn scope_tools_only_a_trailing_star_is_a_wildcard() {
        let pool = tool_pool(&["mcp__crm__search", "mcp__docs__read"]);
        assert!(
            scoped(
                "---\nname: x\ndescription: d\ntools: [mcp__*__search]\n---\nb",
                &pool
            )
            .is_empty()
        );
    }

    #[test]
    fn scope_tools_overlapping_patterns_do_not_duplicate() {
        let pool = tool_pool(&["a", "mcp__crm__search"]);
        assert_eq!(
            scoped(
                "---\nname: x\ndescription: d\ntools: [\"*\", mcp__crm__*, a]\n---\nb",
                &pool
            ),
            vec!["a", "mcp__crm__search"]
        );
    }

    #[test]
    fn scope_tools_never_grants_a_tool_outside_the_pool() {
        let pool = tool_pool(&["a", "b"]);
        assert_eq!(
            scoped(
                "---\nname: x\ndescription: d\ntools: [c, mcp__ghost__*, \"*\"]\n---\nb",
                &pool
            ),
            vec!["a", "b"]
        );
    }

    #[test]
    fn roster_lookup_and_lines() {
        let roster = AgentRoster::new(vec![
            AgentDef::parse_markdown("---\nname: a\ndescription: does A\n---\nbody A").unwrap(),
            AgentDef::parse_markdown("---\nname: b\ndescription: does B\n---\nbody B").unwrap(),
        ]);
        assert_eq!(roster.len(), 2);
        assert!(roster.get("a").is_some());
        assert!(roster.get("z").is_none());
        assert!(roster.roster_lines().contains("- a: does A"));
    }
}
