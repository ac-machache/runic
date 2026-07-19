//! `runic-subagent` — delegation as a single `delegate` tool.
//!
//! Design (best-of-three): **ZeroClaw's delegation shape and safeguards on
//! runic's primitives**. One `delegate` tool selects a [`Subagent`] from its
//! roster; the child runs as a fresh [`Agent`] and its final answer comes back
//! as the tool result. Safeguards: depth limit, no-escalation tool scoping
//! (in the app's [`SubagentBuilder`]), spawn budget, cancellation cascade.
//!
//! The app supplies a [`SubagentBuilder`] (provider resolution + tool scoping);
//! this crate owns the orchestration. The loop needs no special-casing —
//! `delegate` is an ordinary [`runic_tool::Tool`].
//!
//! [`Agent`]: runic_agent::Agent

pub mod delegate;
pub mod subagent;

pub use delegate::{
    BackgroundTask, DEFAULT_MAX_CONCURRENT, DEFAULT_MAX_DEPTH, DEFAULT_MAX_TOTAL_SPAWNS,
    DelegateTool, DelegationCtx, SpawnBudget, SubagentBuilder, SubagentReq, TaskStatus,
    assemble_subagent,
};
pub use subagent::{RosterVoice, Subagent, roster_prompt_section};
