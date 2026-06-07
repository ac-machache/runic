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

    /// Allowlist of tools (by name) this sub-agent can call. Looked up
    /// in the parent's tool pool at spawn time — names that aren't in
    /// the pool are skipped with a warning. Empty / missing = no tools.
    ///
    /// Use the same name the parent uses (e.g. `search_farms`, or
    /// `mcp__toolbox__search_products` for MCP tools — the `mcp__{server}__`
    /// prefix is part of the canonical name).
    #[serde(default, alias = "tools")]
    pub allowed_tools: Vec<String>,

    /// Allowlist of skills (by name) this sub-agent can access. Loaded
    /// from the parent's skill registry; the sub-agent gets a scoped
    /// `skill_view` tool plus a skills-index block prepended to its
    /// system prompt. Empty / missing = no skills.
    #[serde(default)]
    pub skills: Vec<String>,
}
