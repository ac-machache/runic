//! `runic-agents` — markdown-defined subagents.
//!
//! Same shape as `runic-skills`, but each `AGENT.md` file declares a fresh
//! sub-agent the user can invoke. Conceptually identical to Claude Code's
//! `agents/*.md` files: drop a file in `~/.runic/agents/{name}/AGENT.md`,
//! the binary picks it up at startup, the model can call it as a tool.
//!
//! Structure:
//! ```text
//! ~/.runic/agents/
//!   code-reviewer/
//!     AGENT.md
//!   researcher/
//!     AGENT.md
//! ```
//!
//! Each `AGENT.md`:
//! ```text
//! ---
//! name: code-reviewer
//! description: Reviews a diff for bugs, style, and security
//! max-turns: 8
//! ---
//! You are a focused code reviewer. When given a diff, ...
//! ```
//!
//! The markdown body becomes the sub-agent's system prompt; the
//! frontmatter is its metadata. Convert one to a runnable
//! [`runic_agent_core::SubagentTool`] via [`MdAgent::make_subagent_tool`].

pub mod registry;
pub mod types;

pub use registry::{AgentRegistry, LoadError};
pub use types::AgentDef;

use std::sync::Arc;

use runic_agent_core::{Agent, AgentConfig, SubagentTool};
use runic_provider_core::Provider;

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("missing or malformed frontmatter (expected '---' delimiters)")]
    MissingFrontmatter,

    #[error("invalid YAML in frontmatter: {0}")]
    InvalidYaml(#[from] serde_yml::Error),
}

#[derive(Debug, Clone)]
pub struct MdAgent {
    pub def: AgentDef,
    /// The markdown body — becomes the sub-agent's system prompt verbatim.
    pub system_prompt: String,
    /// Leaf directory name (e.g. "code-reviewer"). Set by the registry,
    /// empty after a fresh `parse`.
    pub dir: String,
}

impl MdAgent {
    /// Parse one `AGENT.md` from raw text. Splits frontmatter from body,
    /// parses the YAML into [`AgentDef`].
    pub fn parse(raw: &str) -> Result<Self, ParseError> {
        let rest = raw
            .strip_prefix("---\n")
            .ok_or(ParseError::MissingFrontmatter)?;

        let close = rest
            .find("\n---\n")
            .ok_or(ParseError::MissingFrontmatter)?;

        let frontmatter = &rest[..close];
        let body = &rest[close + "\n---\n".len()..];

        let def: AgentDef = serde_yml::from_str(frontmatter)?;

        Ok(Self {
            def,
            system_prompt: body.to_string(),
            dir: String::new(),
        })
    }

    /// Turn this markdown agent into a runnable [`SubagentTool`] bound to
    /// the given provider.
    ///
    /// The returned tool spawns a fresh child [`Agent`] on every invocation
    /// (matching `SubagentTool`'s factory pattern). The child uses:
    ///   - the markdown body as its system prompt
    ///   - the `max-turns` field as `AgentConfig::max_turns` (default 16
    ///     for sub-agents, smaller than the parent's 64)
    pub fn make_subagent_tool(&self, provider: Arc<dyn Provider>) -> SubagentTool {
        let system_prompt = self.system_prompt.clone();
        let max_turns = self.def.max_turns.unwrap_or(16);
        let name = self.def.name.clone();
        let description = self.def.description.clone();

        let factory = move || {
            Agent::builder(provider.clone())
                .system_prompt(system_prompt.clone())
                .config(AgentConfig {
                    max_turns,
                    ..Default::default()
                })
                .build()
        };

        SubagentTool::new(name, description, factory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_minimal_valid_agent() {
        let raw = "\
---
name: greeter
description: says hi
---
You are a warm greeter. Respond with one sentence.
";
        let agent = MdAgent::parse(raw).expect("should parse");
        assert_eq!(agent.def.name, "greeter");
        assert_eq!(agent.def.description, "says hi");
        assert!(agent.def.max_turns.is_none());
        assert!(agent.system_prompt.starts_with("You are a warm greeter"));
    }

    #[test]
    fn parses_agent_with_optional_max_turns() {
        let raw = "\
---
name: researcher
description: investigates
max-turns: 8
---
Investigate the topic.
";
        let agent = MdAgent::parse(raw).expect("should parse");
        assert_eq!(agent.def.max_turns, Some(8));
    }

    #[test]
    fn rejects_missing_frontmatter() {
        let err = MdAgent::parse("no delimiters here").unwrap_err();
        assert!(matches!(err, ParseError::MissingFrontmatter));
    }

    #[test]
    fn rejects_unclosed_frontmatter() {
        let raw = "---\nname: x\ndescription: y\nbody";
        let err = MdAgent::parse(raw).unwrap_err();
        assert!(matches!(err, ParseError::MissingFrontmatter));
    }

    #[test]
    fn rejects_frontmatter_missing_required_name() {
        let raw = "---\ndescription: I have no name\n---\nbody";
        let err = MdAgent::parse(raw).unwrap_err();
        assert!(matches!(err, ParseError::InvalidYaml(_)));
    }

    #[test]
    fn rejects_frontmatter_missing_required_description() {
        let raw = "---\nname: nameless\n---\nbody";
        let err = MdAgent::parse(raw).unwrap_err();
        assert!(matches!(err, ParseError::InvalidYaml(_)));
    }

    #[test]
    fn parses_body_that_contains_horizontal_rules() {
        let raw = "\
---
name: ruler
description: uses dashes
---
# Section A

Text.

---

# Section B

More text.
";
        let agent = MdAgent::parse(raw).expect("should parse");
        assert!(agent.system_prompt.contains("Section A"));
        assert!(agent.system_prompt.contains("Section B"));
    }

    #[test]
    fn ignores_unknown_frontmatter_keys() {
        // Future-compat: unknown fields like `model` or `allowed-tools`
        // must not break parsing. Serde silently drops them.
        let raw = "\
---
name: future
description: shipped with extra metadata
model: claude-opus-4-7
allowed-tools: [grep, read]
---
body
";
        let agent = MdAgent::parse(raw).expect("unknown fields must be ignored");
        assert_eq!(agent.def.name, "future");
    }
}
