mod def;
mod delegate;

pub use crate::ability::subagent::{SubagentDraft, subagent};
pub use def::{RosterVoice, Subagent, roster_prompt_section};
pub use delegate::{
    BackgroundTask, DEFAULT_MAX_CONCURRENT, DEFAULT_MAX_DEPTH, DEFAULT_MAX_TOTAL_SPAWNS,
    DelegateTool, DelegationCtx, SpawnBudget, TaskStatus,
};
