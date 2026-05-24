//! `runic-mcp` — Model Context Protocol client.
//!
//! Connects to MCP servers over stdio, discovers the tools they expose, and
//! wraps each one as a [`runic_tool_core::Tool`] so the agent can call them
//! uniformly alongside native tools and skills.
//!
//! Architecture mirrors jcode's MCP layer:
//!   - [`config::McpConfig`] — the on-disk JSON (matches Claude Desktop /
//!     Cursor's `mcpServers` shape so existing configs paste in)
//!   - [`protocol`] — JSON-RPC 2.0 envelope + MCP message types
//!   - [`client::McpClient`] — owns one subprocess; [`client::McpHandle`] is
//!     a cloneable async-safe handle for concurrent callers
//!   - [`tool::McpTool`] — adapts one remote tool into a local `Tool`
//!   - [`manager::McpManager`] — orchestrates N servers in parallel
//!
//! Tool names are prefixed `mcp__{server}__{tool}` to avoid collisions with
//! native tools or with each other.

pub mod client;
pub mod config;
pub mod error;
pub mod manager;
pub mod pool;
pub mod protocol;
pub mod tool;

pub use client::{McpClient, McpHandle};
pub use config::{McpConfig, McpServerConfig};
pub use error::McpError;
pub use manager::McpManager;
pub use pool::SharedMcpPool;
pub use protocol::{
    CallToolParams, CallToolResult, ClientCapabilities, ClientInfo, ContentBlock,
    InitializeParams, InitializeResult, JsonRpcError, JsonRpcRequest, JsonRpcResponse,
    ListToolsResult, McpToolDef, PromptsCapability, ResourceContent, ResourcesCapability,
    ServerCapabilities, ServerInfo, ToolsCapability, JSONRPC_VERSION, MCP_PROTOCOL_VERSION,
};
pub use tool::McpTool;
