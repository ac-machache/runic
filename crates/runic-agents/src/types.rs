//! Frontmatter type for `AGENT.md` files.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AgentDef {
    /// Registry-unique name; also what the model sees when calling the
    /// agent as a tool.
    pub name: String,

    /// One-line "when to use this" description — shown to the parent
    /// model so it knows when this sub-agent is relevant.
    pub description: String,

    /// Optional cap on model turns inside the sub-agent. Defaults to 16
    /// at construction time (smaller than the parent's 64). Set lower to
    /// keep a focused sub-agent from running away.
    #[serde(default)]
    pub max_turns: Option<u32>,
}
