//! Runnable examples. Each lives under `examples/` and runs via
//! `cargo run --example NAME` (most need `ANTHROPIC_API_KEY`).
//!
//! Catalog:
//! - `minimal`          — smallest agent: provider in, reply out.
//! - `with_tools`       — give the model a custom [`Tool`].
//! - `with_hooks`       — a [`Hook`] that logs every lifecycle event.
//! - `with_mcp`         — connect tools from an MCP server.
//! - `with_blob`        — pass binary content via the blob store.
//! - `custom_reminder`  — inject ambient notes via the reminder engine.
//! - `with_run_context` — per-run [`RunContext`] read by a layer and a tool.
//! - `with_interceptor` — a [`ToolInterceptor`] that stamps per-run identity.
//! - `with_call_limit`  — [`CallLimitHook`] caps per-tool calls within a run.
//!
//! [`Tool`]: runic_tool_core::Tool
//! [`Hook`]: runic_agent_core::Hook
//! [`RunContext`]: runic_agent_core::RunContext
//! [`ToolInterceptor`]: runic_tool_core::ToolInterceptor
//! [`CallLimitHook`]: runic_agent_core::CallLimitHook
