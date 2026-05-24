//! Error type for MCP operations.

use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// I/O failure on the underlying transport (stdin/stdout pipe).
    #[error("transport I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON encoding or decoding failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// The server returned a JSON-RPC error envelope (`error` field set).
    #[error("jsonrpc error {code}: {message}")]
    JsonRpc {
        code: i64,
        message: String,
        data: Option<serde_json::Value>,
    },

    /// Response did not arrive within the timeout window.
    #[error("request timed out after {0:?}")]
    Timeout(std::time::Duration),

    /// The writer side closed — the subprocess died or its stdin was dropped.
    #[error("server '{0}' is disconnected")]
    Disconnected(String),

    /// `tokio::process::Command::spawn` failed (binary missing, perms, etc.).
    #[error("failed to spawn server '{server}': {source}")]
    Spawn {
        server: String,
        #[source]
        source: std::io::Error,
    },

    /// The handshake completed but the server's response shape was unexpected.
    #[error("protocol error: {0}")]
    Protocol(String),
}

impl McpError {
    pub fn protocol(msg: impl fmt::Display) -> Self {
        Self::Protocol(msg.to_string())
    }
}
