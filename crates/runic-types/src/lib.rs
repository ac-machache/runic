//! `runic-types` — the deepest layer of the runtime: conversation message /
//! content types and tool-schema types. Everything else (providers, tools,
//! state, the agent loop) stands on these.
//!
//! ## Provenance
//! The message/content model and the cross-provider tool-schema normalization
//! are adapted from **OpenFang** (dual MIT / Apache-2.0,
//! <https://github.com/RightNow-AI/openfang>), chosen for their production
//! maturity: stable server-assigned message ids, per-block `provider_metadata`,
//! `Thinking`/`RedactedThinking` round-tripping (Anthropic extended thinking,
//! Gemini thought signatures, DeepSeek/Qwen `reasoning_content`), a forward-
//! compatible `Unknown` block, and `normalize_schema_for_provider` for
//! Gemini/Groq schema quirks. Adapted into runic's naming; behavior preserved.

pub mod message;
pub mod tool;

pub use message::{
    ContentBlock, Message, MessageContent, ReplyDirectives, Role, StopReason, TokenUsage,
    validate_image,
};
pub use tool::{ToolCall, ToolDefinition, ToolResult, normalize_schema_for_provider};
