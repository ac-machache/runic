//! Runic agent kernel.
//!
//! Headless runtime: agent loop, hook system, conversation state. No TUI,
//! no CLI, no UI — callers build their own surface on top.
//!
//! Tool primitives (`Tool`, `HitlTool`, `BackgroundTool`, the dispatch
//! registry, etc.) live in the sibling crate [`runic_tool_core`] and are
//! re-exported here for convenience.
//!
//! ```ignore
//! use runic_agent_core::{Agent, AgentConfig};
//! use runic_provider_anthropic::{AnthropicConfig, AnthropicProvider};
//!
//! let provider = AnthropicProvider::new(AnthropicConfig::new("sk-..."));
//! let mut agent = Agent::builder(provider)
//!     .system_prompt("You are a focused coding assistant.")
//!     .build();
//! let outcome = agent.run("hi").await?;
//! ```

pub mod agent;
pub mod error;
pub mod event;
pub mod hooks;
pub mod state;
pub mod structured;
pub mod subagent;

pub use agent::{Agent, AgentBuilder, AgentConfig, RunContext, RunOutcome};
pub use error::AgentError;
pub use structured::StructuredOutputTool;
pub use event::{AgentEvent, TokenUsage};
pub use hooks::{CallLimitHook, Hook, HookOutcome};
pub use state::{AgentState, HookLifecycle, RunTimeContext, SessionEvent, EVENT_BROADCAST_CAPACITY};
pub use subagent::{AsyncSubagentTool, SubagentTool};

// Re-export tool primitives from the sibling crate so existing call sites
// (`runic_agent_core::Tool`, `runic_agent_core::BackgroundTool`, etc.) keep
// working. New code can depend on `runic-tool-core` directly to avoid pulling
// in the agent kernel.
pub use runic_tool_core::{
    AbortOutcome, ApprovalRequest, Approver, ApproverHandle, BackgroundAdapter,
    BackgroundCancelTool, BackgroundManager, BackgroundStatusTool, BackgroundTool, Draft,
    HitlAdapter, HitlTool, PlainAdapter, TaskStatusView, Tool, ToolContext, ToolDispatch,
    ToolDispatchError, ToolRegistry, ToolResult, UserDecision,
};
