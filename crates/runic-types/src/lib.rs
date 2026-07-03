//! Shared conversation, content-block, and tool-schema wire types.

pub mod message;
pub mod tool;

pub use message::{
    ContentBlock, Message, MessageContent, ReplyDirectives, Role, StopReason, TokenUsage,
    validate_image,
};
pub use tool::{ToolCall, ToolDefinition, ToolResult, normalize_schema_for_provider};
