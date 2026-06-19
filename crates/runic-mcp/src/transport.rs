//! The `Transport` abstraction and the stdio implementation.
//!
//! Each MCP server connection uses ONE transport. Today there are two:
//!   - [`StdioTransport`] — wraps a spawned subprocess (stdin/stdout pipes).
//!   - [`crate::transport_http::HttpTransport`] — POSTs to a remote URL,
//!     handles both single JSON and SSE-stream responses.
//!
//! [`McpHandle`] holds an `Arc<dyn Transport>` and just forwards. Adding a
//! third transport (websocket, plugin host, etc.) is a new `impl Transport`,
//! no surgery to the rest of the crate.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, warn};

use crate::error::McpError;
use crate::protocol::{
    JsonRpcError, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, JSONRPC_VERSION,
};

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const STDIO_WRITER_CHANNEL_CAPACITY: usize = 32;

/// Parent-environment variables passed through to a spawned MCP server. We
/// `env_clear()` first so a server can't read unrelated secrets from our
/// environment; only these (plus the server's explicitly-configured `env`)
/// reach the child.
const ENV_ALLOWLIST: &[&str] = &[
    "PATH", "HOME", "TMPDIR", "TMP", "TEMP", "USER", "LOGNAME", "LANG", "LC_ALL", "SHELL",
];

// ─── Transport trait ────────────────────────────────────────────────────────

/// One bidirectional MCP transport. Implementations need only to:
///   - send a JSON-RPC request and await the matching response
///   - send a notification (no response)
///   - identify the server (used in error messages and tool prefixes)
///   - shut down cleanly
#[async_trait]
pub trait Transport: Send + Sync + std::fmt::Debug {
    fn server_name(&self) -> &str;

    /// Send a JSON-RPC request, await the response, return the `result` body.
    /// Errors map to [`McpError`] (timeout, disconnect, JSON-RPC error, …).
    async fn request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, McpError>;

    /// Send a JSON-RPC notification (no `id`, no response expected).
    async fn notify(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<(), McpError>;

    /// Best-effort graceful shutdown. After this returns, the transport
    /// is considered dead.
    async fn close(&self);
}

// ─── Shared request-id counter ──────────────────────────────────────────────

/// Monotonic id source used by both transports. Starts at 1 (some MCP
/// servers treat id=0 as a notification ambiguously).
#[derive(Debug, Default)]
pub(crate) struct RequestIdCounter {
    next: AtomicU64,
}

impl RequestIdCounter {
    pub fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
        }
    }
    pub fn next(&self) -> u64 {
        self.next.fetch_add(1, Ordering::SeqCst)
    }
}

// ─── StdioTransport ─────────────────────────────────────────────────────────

/// Talks to a spawned subprocess over its stdin/stdout pipes.
///
/// Internally: a writer task drains an mpsc channel into stdin; a reader
/// task parses one JSON line at a time off stdout, looks up the matching
/// response id in `pending`, and delivers via `oneshot`. Notifications and
/// unknown payloads are logged and dropped.
#[derive(Debug)]
pub struct StdioTransport {
    server_name: Arc<String>,
    writer_tx: mpsc::Sender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
    request_id: Arc<RequestIdCounter>,
    /// Held so it can be killed at shutdown. `kill_on_drop` is also set.
    child: Mutex<Option<Child>>,
}

impl StdioTransport {
    /// Spawn the binary and start the reader/writer/stderr tasks.
    /// Caller must follow up with `initialize` via [`Transport::request`].
    pub async fn spawn(
        server_name: &str,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<Self, McpError> {
        debug!(server = server_name, command, "spawning MCP stdio server");

        // Path-traversal guard: a server command must be a bare binary name or
        // an absolute path, never a `..`-relative escape.
        if command.contains("..") {
            return Err(McpError::protocol(format!(
                "refusing to spawn server '{server_name}': command '{command}' contains '..'"
            )));
        }

        let mut cmd = Command::new(command);
        // Don't leak the parent's whole environment into the child. Start from
        // a minimal allow-list, then layer the explicitly-configured vars on top.
        cmd.env_clear();
        for key in ENV_ALLOWLIST {
            if let Ok(val) = std::env::var(key) {
                cmd.env(key, val);
            }
        }
        cmd.args(args)
            .envs(env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|source| McpError::Spawn {
            server: server_name.to_string(),
            source,
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::protocol("spawned child has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::protocol("spawned child has no stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| McpError::protocol("spawned child has no stderr"))?;

        let server_name_arc = Arc::new(server_name.to_string());
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let request_id = Arc::new(RequestIdCounter::new());
        let (writer_tx, writer_rx) = mpsc::channel::<String>(STDIO_WRITER_CHANNEL_CAPACITY);

        spawn_writer_task(stdin, writer_rx, server_name_arc.clone());
        spawn_reader_task(stdout, pending.clone(), server_name_arc.clone());
        spawn_stderr_task(stderr, server_name_arc.clone());

        Ok(Self {
            server_name: server_name_arc,
            writer_tx,
            pending,
            request_id,
            child: Mutex::new(Some(child)),
        })
    }
}

#[async_trait]
impl Transport for StdioTransport {
    fn server_name(&self) -> &str {
        &self.server_name
    }

    async fn request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, McpError> {
        let id = self.request_id.next();
        let req = JsonRpcRequest::new(id, method, params);

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let line = format!("{}\n", serde_json::to_string(&req)?);
        if self.writer_tx.send(line).await.is_err() {
            self.pending.lock().await.remove(&id);
            return Err(McpError::Disconnected((*self.server_name).clone()));
        }

        let response = match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                return Err(McpError::Disconnected((*self.server_name).clone()));
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                return Err(McpError::Timeout(REQUEST_TIMEOUT));
            }
        };

        match (response.result, response.error) {
            (Some(result), _) => Ok(result),
            (_, Some(err)) => Err(McpError::JsonRpc {
                code: err.code,
                message: err.message,
                data: err.data,
            }),
            (None, None) => Err(McpError::protocol(
                "response missing both `result` and `error`",
            )),
        }
    }

    async fn notify(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<(), McpError> {
        let notif = JsonRpcNotification::new(method, params);
        let line = format!("{}\n", serde_json::to_string(&notif)?);
        self.writer_tx
            .send(line)
            .await
            .map_err(|_| McpError::Disconnected((*self.server_name).clone()))
    }

    async fn close(&self) {
        // Best-effort: signal `shutdown`, then kill the child unconditionally.
        let _ = self.notify("shutdown", None).await;
        let mut guard = self.child.lock().await;
        if let Some(mut child) = guard.take() {
            let _ = child.kill().await;
        }
    }
}

// ─── Background tasks (stdio) ──────────────────────────────────────────────

fn spawn_writer_task<W>(mut stdin: W, mut rx: mpsc::Receiver<String>, server_name: Arc<String>)
where
    W: AsyncWriteExt + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if let Err(err) = stdin.write_all(line.as_bytes()).await {
                warn!(server = %server_name, error = %err, "MCP stdin write failed");
                break;
            }
            if let Err(err) = stdin.flush().await {
                warn!(server = %server_name, error = %err, "MCP stdin flush failed");
                break;
            }
        }
        debug!(server = %server_name, "MCP writer task exiting");
    });
}

fn spawn_reader_task<R>(
    stdout: R,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
    server_name: Arc<String>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<JsonRpcResponse>(&line) {
                        Ok(resp) if resp.jsonrpc == JSONRPC_VERSION => {
                            let sender = pending.lock().await.remove(&resp.id);
                            match sender {
                                Some(tx) => {
                                    let _ = tx.send(resp);
                                }
                                None => {
                                    warn!(
                                        server = %server_name,
                                        id = resp.id,
                                        "MCP response with no matching pending request"
                                    );
                                }
                            }
                            continue;
                        }
                        _ => {}
                    }
                    debug!(server = %server_name, line = %line, "MCP unhandled inbound");
                }
                Ok(None) => {
                    debug!(server = %server_name, "MCP server closed stdout (EOF)");
                    break;
                }
                Err(err) => {
                    warn!(server = %server_name, error = %err, "MCP stdout read error");
                    break;
                }
            }
        }
        // Wake any still-pending callers with a synthetic disconnect error.
        let mut guard = pending.lock().await;
        for (_, tx) in guard.drain() {
            let _ = tx.send(JsonRpcResponse {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: 0,
                result: None,
                error: Some(JsonRpcError {
                    code: -32000,
                    message: format!("server '{server_name}' disconnected"),
                    data: None,
                }),
            });
        }
    });
}

fn spawn_stderr_task<R>(stderr: R, server_name: Arc<String>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    warn!(server = %server_name, "MCP stderr: {line}");
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stdio_spawn_with_missing_binary_returns_spawn_error() {
        let env = HashMap::new();
        let result = StdioTransport::spawn("ghost", "/does/not/exist", &[], &env).await;
        match result {
            Err(McpError::Spawn { server, .. }) => assert_eq!(server, "ghost"),
            other => panic!("expected Spawn error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn spawn_rejects_path_traversal_command() {
        let env = HashMap::new();
        let result = StdioTransport::spawn("evil", "../../bin/sh", &[], &env).await;
        match result {
            Err(McpError::Protocol(msg)) => assert!(msg.contains("..")),
            other => panic!("expected path-traversal rejection, got {other:?}"),
        }
    }

    #[test]
    fn request_id_counter_is_monotonic_and_starts_at_one() {
        let c = RequestIdCounter::new();
        assert_eq!(c.next(), 1);
        assert_eq!(c.next(), 2);
        assert_eq!(c.next(), 3);
    }

    #[tokio::test]
    async fn request_on_a_dead_writer_returns_disconnected() {
        // Build a StdioTransport whose writer channel has been closed.
        let (tx, rx) = mpsc::channel::<String>(1);
        drop(rx);
        let t = StdioTransport {
            server_name: Arc::new("dead".to_string()),
            writer_tx: tx,
            pending: Arc::new(Mutex::new(HashMap::new())),
            request_id: Arc::new(RequestIdCounter::new()),
            child: Mutex::new(None),
        };
        let err = t.request("ping", None).await.unwrap_err();
        match err {
            McpError::Disconnected(name) => assert_eq!(name, "dead"),
            other => panic!("expected Disconnected, got {other:?}"),
        }
    }
}
