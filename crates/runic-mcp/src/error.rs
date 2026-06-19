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

    /// An HTTP MCP session expired (server returned 404/410 for a request that
    /// carried an `Mcp-Session-Id`). Recoverable by re-initializing.
    #[error("server '{0}' session expired")]
    StaleSession(String),

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

    /// Whether this error is worth a transport reconnect + retry. A dead
    /// subprocess or an expired HTTP session can be recovered by rebuilding
    /// the connection; a timeout or a JSON-RPC tool error cannot (retrying
    /// the same call won't help, and we'd mask a real failure).
    pub fn is_recoverable(&self) -> bool {
        matches!(self, McpError::Disconnected(_) | McpError::StaleSession(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn only_transport_death_is_recoverable() {
        assert!(McpError::Disconnected("s".into()).is_recoverable());
        assert!(McpError::StaleSession("s".into()).is_recoverable());
        // These must NOT trigger a reconnect:
        assert!(!McpError::Timeout(Duration::from_secs(1)).is_recoverable());
        assert!(!McpError::Protocol("bad shape".into()).is_recoverable());
        assert!(!McpError::JsonRpc {
            code: -32000,
            message: "tool failed".into(),
            data: None,
        }
        .is_recoverable());
    }
}
