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
pub use types::{
    AgentDef, DispatchMode, FilesystemConfig, FilesystemConfigError, FilesystemMode,
};

use std::sync::Arc;

use runic_agent_core::{Agent, AgentConfig, AsyncSubagentTool, SessionEvent, SubagentTool};
use runic_provider_core::Provider;
use runic_skills::{SkillRegistry, SkillViewTool};
use runic_storage_backend::StorageBackend;
use runic_tool_core::ToolRegistry;
use tokio::sync::broadcast;

/// Called once per sub-agent spawn so the host can persist the child's
/// event stream alongside the parent's. The receiver is fresh from the
/// child agent's broadcast — subscribe BEFORE the agent runs to catch
/// the opening `RunStart`.
///
/// Args, in order:
///   - sub-agent name (matches the AGENT.md `name` field)
///   - child session_id (auto-generated UUID on `Agent::builder().build()`)
///   - broadcast receiver for the child's `SessionEvent`s
pub type SubagentPersisterFn = Arc<
    dyn Fn(&str, String, broadcast::Receiver<SessionEvent>) + Send + Sync,
>;

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
    /// the given provider, with NO tools and NO skills.
    ///
    /// Convenience wrapper around [`make_subagent_tool_with_context`] for
    /// the simple case (the prompt is purely textual; no DB / tool surface
    /// is needed). For coral-style sub-agents that need scoped tools and
    /// skills, call [`make_subagent_tool_with_context`] directly.
    pub fn make_subagent_tool(&self, provider: Arc<dyn Provider>) -> SubagentTool {
        self.make_subagent_tool_with_context(
            provider,
            Arc::new(ToolRegistry::new()),
            Arc::new(SkillRegistry::new()),
            Arc::new(runic_storage_backend::MemoryBackend::new()),
            "skills",
            None,
        )
    }

    /// Turn this markdown agent into a runnable [`SubagentTool`] with a
    /// scoped tool surface AND a scoped skill registry, both filtered
    /// from what the parent has set up.
    ///
    /// What the child sees:
    ///   - **System prompt**: the markdown body, prefixed with a small
    ///     `<skills>` block listing the allowlisted skills (so the child
    ///     model knows what `skill_view` will return).
    ///   - **Tools**: only those whose names appear in
    ///     `self.def.allowed_tools` and exist in `parent_pool`. Names that
    ///     don't resolve are logged and dropped — they're a config
    ///     mistake, not a runtime failure. If `self.def.skills` is
    ///     non-empty, a scoped `skill_view` tool is added too.
    ///   - **Max turns**: `max-turns` frontmatter (default 16).
    ///
    /// The factory closure clones the precomputed handles each call —
    /// every sub-agent spawn produces a fresh child `Agent` with the
    /// same scoped surface.
    pub fn make_subagent_tool_with_context(
        &self,
        provider: Arc<dyn Provider>,
        parent_pool: Arc<ToolRegistry>,
        parent_skills: Arc<SkillRegistry>,
        storage: Arc<dyn StorageBackend>,
        skills_root: &'static str,
        persister: Option<SubagentPersisterFn>,
    ) -> SubagentTool {
        let agent_name = self.def.name.clone();
        let description = self.def.description.clone();
        let max_turns = self.def.max_turns.unwrap_or(16);
        let allowed_tools = self.def.allowed_tools.clone();
        let allowed_skills = self.def.skills.clone();

        // Build the scoped skill registry once (skills are immutable per
        // session). Same for the prompt prefix that lists them.
        let scoped_skills = Arc::new(parent_skills.scope(&allowed_skills));
        let system_prompt = compose_system_prompt(&self.system_prompt, &scoped_skills);

        let factory_name = agent_name.clone();
        let factory = move || {
            let mut child_tools = ToolRegistry::new();
            for name in &allowed_tools {
                match parent_pool.get(name) {
                    Some(dispatch) => child_tools.insert_dispatch(dispatch),
                    None => tracing::warn!(
                        agent = factory_name.as_str(),
                        tool = name.as_str(),
                        "allowed-tool not found in parent pool — skipping",
                    ),
                }
            }
            if !scoped_skills.is_empty() {
                let view = SkillViewTool::new(
                    scoped_skills.clone(),
                    storage.clone(),
                    skills_root,
                );
                child_tools.register(Arc::new(view));
            }

            let mut builder = Agent::builder(provider.clone())
                .system_prompt(system_prompt.clone())
                .config(AgentConfig {
                    max_turns,
                    ..Default::default()
                });
            if !child_tools.is_empty() {
                builder = builder.tools(child_tools);
            }
            let agent = builder.build();
            // Subscribe BEFORE returning so the persister catches the
            // opening RunStart. The host's closure spawns the writer
            // task; we discard the JoinHandle — dropping it doesn't
            // kill the task, it runs until the child agent's broadcast
            // channel closes.
            if let Some(persister_fn) = persister.as_ref() {
                let session_id = agent.state().session_id.clone();
                persister_fn(&factory_name, session_id, agent.subscribe_events());
            }
            agent
        };

        SubagentTool::new(agent_name, description, factory)
    }

    /// Same as [`make_subagent_tool_with_context`] but returns an
    /// [`AsyncSubagentTool`] — the parent gets a `task_id` immediately
    /// and polls via `background_status`. Use for children that run
    /// long enough that synchronous waiting would block the parent's
    /// other work.
    ///
    /// Description gets a one-line prefix telling the parent model the
    /// call is fire-and-forget so it knows to expect a task_id and use
    /// `background_status` to check on it.
    pub fn make_async_subagent_tool_with_context(
        &self,
        provider: Arc<dyn Provider>,
        parent_pool: Arc<ToolRegistry>,
        parent_skills: Arc<SkillRegistry>,
        storage: Arc<dyn StorageBackend>,
        skills_root: &'static str,
        persister: Option<SubagentPersisterFn>,
    ) -> AsyncSubagentTool {
        let agent_name = self.def.name.clone();
        let description = augment_async_description(&self.def.description);
        let max_turns = self.def.max_turns.unwrap_or(16);
        let allowed_tools = self.def.allowed_tools.clone();
        let allowed_skills = self.def.skills.clone();

        let scoped_skills = Arc::new(parent_skills.scope(&allowed_skills));
        let system_prompt = compose_system_prompt(&self.system_prompt, &scoped_skills);

        let factory_name = agent_name.clone();
        let factory = move || {
            let mut child_tools = ToolRegistry::new();
            for name in &allowed_tools {
                match parent_pool.get(name) {
                    Some(dispatch) => child_tools.insert_dispatch(dispatch),
                    None => tracing::warn!(
                        agent = factory_name.as_str(),
                        tool = name.as_str(),
                        "allowed-tool not found in parent pool — skipping",
                    ),
                }
            }
            if !scoped_skills.is_empty() {
                let view = SkillViewTool::new(
                    scoped_skills.clone(),
                    storage.clone(),
                    skills_root,
                );
                child_tools.register(Arc::new(view));
            }

            let mut builder = Agent::builder(provider.clone())
                .system_prompt(system_prompt.clone())
                .config(AgentConfig {
                    max_turns,
                    ..Default::default()
                });
            if !child_tools.is_empty() {
                builder = builder.tools(child_tools);
            }
            let agent = builder.build();
            if let Some(persister_fn) = persister.as_ref() {
                let session_id = agent.state().session_id.clone();
                persister_fn(&factory_name, session_id, agent.subscribe_events());
            }
            agent
        };

        AsyncSubagentTool::new(agent_name, description, factory)
    }
}

/// Prepend a short note to the AGENT.md description so the PARENT model
/// knows this sub-agent is asynchronous — it should expect a `task_id`
/// and check progress with `background_status`. We can't trust the
/// human-written description to spell this out; better to bake it in.
fn augment_async_description(raw: &str) -> String {
    let prefix = "[async] Returns a task_id immediately — check progress with \
                  `background_status(task_id)` and read the final answer when status is 'done'. ";
    format!("{prefix}{}", raw.trim_start())
}

/// Prepend a `<skills>` block listing the scoped skills (name + one-line
/// description) to the markdown body. The child model uses this as its
/// trigger table for the `skill_view` tool. No skills → no block, body
/// returned untouched.
fn compose_system_prompt(body: &str, scoped: &SkillRegistry) -> String {
    if scoped.is_empty() {
        return body.to_string();
    }
    let mut out = String::from(
        "<skills>\n\
         Skills available to you. Call `skill_view(skill_name=...)` to load the full body before acting on a skill's domain.\n\n",
    );
    for skill in scoped.list() {
        out.push_str("- ");
        out.push_str(&skill.meta.name);
        out.push_str(" — ");
        out.push_str(skill.meta.description.trim());
        out.push('\n');
    }
    out.push_str("</skills>\n\n");
    out.push_str(body);
    out
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
