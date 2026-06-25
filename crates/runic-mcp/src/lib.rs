//! `runic-mcp` — Model Context Protocol client.
//!
//! Connects to MCP servers and wraps each one's tools as a
//! [`runic_tool::Tool`] so the agent can call them uniformly alongside
//! native tools.
//!
//! ## Transports
//!
//! Two transports are supported:
//!
//! - **stdio** ([`transport::StdioTransport`]) — spawns a binary as a
//!   subprocess and talks line-delimited JSON over stdin/stdout. Use for
//!   local servers (the github/filesystem/etc MCP servers Claude Desktop
//!   speaks to).
//!
//! - **HTTP** ([`transport_http::HttpTransport`]) — Streamable HTTP per the
//!   2025-03-26 MCP spec. POSTs JSON-RPC to a URL; reads JSON or
//!   text/event-stream responses; tracks `Mcp-Session-Id`. Use for remote
//!   servers running on another host.
//!
//! Both implement the [`transport::Transport`] trait; [`client::McpHandle`]
//! is an `Arc<dyn Transport>` wrapper so the rest of the crate doesn't need
//! to care which one is in use.
//!
//! ## Modules
//!
//! - [`config`] — `~/.runic/mcp.json` parsing
//! - [`protocol`] — JSON-RPC 2.0 + MCP message types
//! - [`transport`] / [`transport_http`] — the transports
//! - [`client`] — `McpClient` + `McpHandle`
//! - [`tool`] — `McpTool` adapter (impls `runic_tool::Tool`)
//! - [`manager`] — per-session multi-server orchestration
//! - [`pool`] — daemon-wide shared pool for stateless servers
//!
//! Tool names are prefixed `mcp__{server}__{tool}` to avoid collisions
//! with native tools or with each other.

pub mod client;
pub mod config;
pub mod deferred;
pub mod error;
pub mod loader;
pub mod manager;
pub mod pool;
pub mod protocol;
pub mod tool;
pub mod tool_search;
pub mod transport;
pub mod transport_http;

pub use client::{McpClient, McpHandle};
pub use config::{HttpServerConfig, McpConfig, McpServerConfig, StdioServerConfig};
pub use deferred::{
    DeferredMcpToolSet, DeferredMcpToolStub, ToolAccessPolicy, deferred_tools_prompt_section,
};
pub use error::McpError;
pub use loader::{Mcp, McpConnection, mcp_file, mcp_json};
pub use manager::McpManager;
pub use pool::SharedMcpPool;
pub use protocol::{
    CallToolParams, CallToolResult, ClientCapabilities, ClientInfo, ContentBlock, InitializeParams,
    InitializeResult, JSONRPC_VERSION, JsonRpcError, JsonRpcRequest, JsonRpcResponse,
    ListToolsResult, MCP_PROTOCOL_VERSION, McpToolDef, PromptsCapability, ResourceContent,
    ResourcesCapability, ServerCapabilities, ServerInfo, ToolsCapability,
};
pub use tool::McpTool;
pub use tool_search::ToolSearchTool;
pub use transport::{StdioTransport, Transport};
pub use transport_http::HttpTransport;
