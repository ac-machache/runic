mod def;
mod delegate;

pub use def::{DelegationLabels, Invocation, Subagent, prompt_section};
pub use delegate::{
    BackgroundTask, DEFAULT_MAX_CONCURRENT, DEFAULT_MAX_DEPTH, DEFAULT_MAX_TOTAL_SPAWNS,
    DelegateTool, DelegationCtx, SpawnBudget, TaskStatus,
};
