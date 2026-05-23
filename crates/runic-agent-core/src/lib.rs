//! Runic agent kernel.
//!
//! Headless runtime: agent loop, tool dispatch, hook system, conversation
//! state. No TUI, no CLI, no UI — callers build their own surface on top.
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
pub mod approval;
pub mod background;
pub mod error;
pub mod event;
pub mod hooks;
pub mod state;
pub mod subagent;
pub mod tool;

pub use agent::{Agent, AgentBuilder, AgentConfig, RunOutcome};
pub use approval::{ApprovalRequest, Approver, ApproverHandle, Draft, HitlTool, UserDecision};
pub use background::{
    AbortOutcome, BackgroundAdapter, BackgroundCancelTool, BackgroundManager,
    BackgroundStatusTool, BackgroundTool, TaskStatusView,
};
pub use error::AgentError;
pub use event::{AgentEvent, TokenUsage};
pub use hooks::{Hook, HookOutcome};
pub use state::AgentState;
pub use subagent::{AsyncSubagentTool, SubagentTool};
pub use tool::{Tool, ToolContext, ToolRegistry, ToolResult};
