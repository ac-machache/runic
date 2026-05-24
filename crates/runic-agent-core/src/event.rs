use runic_message_types::ToolCall;
use serde::{Deserialize, Serialize};

use runic_tool_core::ToolResult;

/// Token-usage snapshot reported by the provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
}

/// Events surfaced from `Agent::run_streaming`.
///
/// Callers consume these to build any presentation layer (TUI, web, daemon, ...).
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Assistant text delta from the active turn.
    AssistantTextDelta(String),
    /// Assistant extended-thinking delta.
    AssistantThinkingDelta(String),
    /// Provider has begun emitting a tool_use block.
    ToolUseStart { id: String, name: String },
    /// Tool input JSON has finished assembling — about to dispatch.
    ToolDispatching(ToolCall),
    /// Tool finished executing.
    ToolFinished {
        call: ToolCall,
        result: ToolResult,
        duration_ms: u64,
    },
    /// Token usage update from the provider.
    Usage(TokenUsage),
    /// A model turn completed.
    TurnComplete {
        stop_reason: Option<String>,
        tool_calls_this_turn: u32,
    },
    /// The full run finished (no more tool calls outstanding, or hard stop).
    RunComplete { total_turns: u32 },
    /// Non-fatal warning surfaced for observability.
    Warning(String),
}
