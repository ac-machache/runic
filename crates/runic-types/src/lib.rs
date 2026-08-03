//! Shared conversation, content-block, and tool-schema wire types.

pub mod message;
pub mod provenance;
pub mod tool;

pub use message::{
    ContentBlock, Message, MessageContent, ReplyDirectives, Role, Source, StopReason, TokenUsage,
    ToolResultPayload, validate_image,
};
pub use provenance::{ProvenanceSource, sanitize_provenance};
pub use tool::{ToolCall, ToolDefinition, normalize_schema_for_provider};
