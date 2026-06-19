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

use serde::Deserialize;

/// YAML frontmatter fields (kebab-case on the wire).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct Frontmatter {
    name: String,
    #[serde(default)]
    description: String,
    /// Named provider override (resolved by the app's `ChildBuilder`).
    #[serde(default)]
    provider: Option<String>,
    /// Model id override.
    #[serde(default)]
    model: Option<String>,
    /// Tool allow-list by name. Must be a subset of the parent's tools — the
    /// `ChildBuilder` rejects any name not in the parent pool (no escalation).
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
        if fm.name.trim().is_empty() {
            anyhow::bail!("AGENT.md frontmatter is missing `name`");
        }

        Ok(Self {
            name: fm.name,
            description: fm.description,
            provider: fm.provider,
            model: fm.model,
            allowed_tools: fm.allowed_tools,
            skills: fm.skills,
            max_turns: fm.max_turns,
            system_prompt: body.trim().to_string(),
        })
    }
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
                    Err(e) => tracing::warn!(path = %path.display(), error = %e, "skipping invalid AGENT.md"),
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
