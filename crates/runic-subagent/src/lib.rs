//! `runic-subagent` — delegation as a single `delegate` tool.
//!
//! Design (best-of-three): **ZeroClaw's delegation shape and safeguards on
//! runic's primitives**. One `delegate` tool selects a subagent from a roster
//! of Markdown `AGENT.md` definitions; the child runs as a fresh [`Agent`] and
//! its final answer comes back as the tool result. Safeguards: depth limit,
//! no-escalation tool scoping (in the app's [`SubagentBuilder`]), spawn budget,
//! cancellation cascade.
//!
//! The app supplies a [`SubagentBuilder`] (provider resolution + tool scoping);
//! this crate owns the orchestration. The loop needs no special-casing —
//! `delegate` is an ordinary [`runic_tool::Tool`].
//!
//! [`Agent`]: runic_agent::Agent

pub mod def;
pub mod delegate;
pub mod dirs;
pub mod loader;
mod security;

pub use dirs::Dirs;

pub use def::{AgentDef, AgentRoster};
pub use delegate::{
    BackgroundTask, DEFAULT_MAX_CONCURRENT, DEFAULT_MAX_DEPTH, DEFAULT_MAX_TOTAL_SPAWNS,
    DelegateTool, DelegationCtx, SpawnBudget, SubagentBuilder, TaskStatus,
};
pub use loader::{Subagents, subagents};

/// Render the roster into a system-prompt section so the model knows which
/// subagents it can delegate to (the `delegate` tool's `&str` description
/// can't carry the dynamic roster). The app concatenates this into the
/// agent's system prompt.
pub fn roster_prompt_section(roster: &AgentRoster) -> String {
    if roster.is_empty() {
        return String::new();
    }
    format!(
        "<subagents>\nYou can delegate self-contained tasks to these subagents \
         via the `delegate` tool (they do NOT see this conversation):\n{}\n</subagents>",
        roster.roster_lines()
    )
}
