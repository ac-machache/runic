//! Tool primitives shared by the agent kernel and every extension that wants
//! to register tools (skills, MCP, future surfaces).
//!
//! Three tool kinds, one universal dispatch trait:
//!   - [`Tool`]                  — plain synchronous tool
//!   - [`approval::HitlTool`]    — gated by user approval
//!   - [`background::BackgroundTool`] — spawned task, returns `task_id`
//!
//! Each kind has a thin adapter (`PlainAdapter`, `HitlAdapter`,
//! `BackgroundAdapter`) that wraps it into the universal [`ToolDispatch`].
//! [`ToolRegistry`] only ever stores `Arc<dyn ToolDispatch>` — adding a new
//! kind means writing a new adapter without touching the registry.

pub mod approval;
pub mod background;
pub mod tool;

pub use approval::{ApprovalRequest, Approver, ApproverHandle, Draft, HitlTool, UserDecision};
pub use background::{
    AbortOutcome, BackgroundAdapter, BackgroundCancelTool, BackgroundManager,
    BackgroundStatusTool, BackgroundTool, TaskStatusView,
};
pub use tool::{
    HitlAdapter, PlainAdapter, Tool, ToolContext, ToolDispatch, ToolDispatchError, ToolRegistry,
    ToolResult,
};
