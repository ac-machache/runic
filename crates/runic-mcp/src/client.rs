//! One MCP server connection.
//!
//! [`McpClient`] owns the spawned subprocess and the reader/writer tasks.
//! [`McpHandle`] is a cheap `Clone`able handle into the client's request
//! pipeline — cloneable across sessions and concurrent callers. The split
//! mirrors jcode (their `McpClient` vs `McpHandle`).
//!
//! Request/response correlation: each request gets a monotonic `u64` id;
//! a `HashMap<id, oneshot::Sender>` carries the eventual response back to
//! the caller. The reader task deserializes one JSON line at a time from
//! stdout, looks up the id, and delivers the response. Notifications (no
//! `id` field) are logged and dropped — we don't act on server-pushed
//! events yet (matches jcode).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, warn};

use crate::config::McpServerConfig;
use crate::error::McpError;
use crate::protocol::{
    CallToolParams, CallToolResult, ClientCapabilities, ClientInfo, InitializeParams,
    InitializeResult, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, ListToolsResult,
    McpToolDef, ServerCapabilities, ServerInfo, JSONRPC_VERSION, MCP_PROTOCOL_VERSION,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const WRITER_CHANNEL_CAPACITY: usize = 32;

/// Cheap, cloneable handle into a connected server. All concurrent callers
/// (manager, every `McpTool` instance) share one of these.
#[derive(Clone, Debug)]
pub struct McpHandle {
    pub(crate) server_name: Arc<String>,
    pub(crate) writer_tx: mpsc::Sender<String>,
    pub(crate) pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
    pub(crate) request_id: Arc<AtomicU64>,
}

impl McpHandle {
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Send a request, await the matching response. Timeout: 30s.
    pub async fn request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, McpError> {
        let id = self.request_id.fetch_add(1, Ordering::SeqCst);
        let req = JsonRpcRequest::new(id, method, params);

        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self.pending.lock().await;
            guard.insert(id, tx);
        }

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

        if let Some(err) = response.error {
            return Err(McpError::JsonRpc {
                code: err.code,
                message: err.message,
                data: err.data,
            });
        }
        response
            .result
            .ok_or_else(|| McpError::protocol("response missing both `result` and `error`"))
    }

    /// Send a notification (no `id`, no response expected).
    pub async fn notify(
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

    /// Convenience wrapper for `tools/list`.
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>, McpError> {
        let raw = self.request("tools/list", None).await?;
        let parsed: ListToolsResult = serde_json::from_value(raw)?;
        Ok(parsed.tools)
    }

    /// Convenience wrapper for `tools/call`.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult, McpError> {
        let params = CallToolParams {
            name: name.to_string(),
            arguments: Some(arguments),
        };
        let raw = self
            .request("tools/call", Some(serde_json::to_value(params)?))
            .await?;
        Ok(serde_json::from_value(raw)?)
    }
}

/// Owns the spawned subprocess. Drop kicks the child via `start_kill`.
#[derive(Debug)]
pub struct McpClient {
    handle: McpHandle,
    child: Child,
    server_info: ServerInfo,
    capabilities: ServerCapabilities,
    tools: Vec<McpToolDef>,
}

impl McpClient {
    /// Spawn the server, perform the initialize handshake, list its tools.
    pub async fn connect(server_name: &str, config: &McpServerConfig) -> Result<Self, McpError> {
        debug!(server = server_name, command = %config.command, "spawning MCP server");

        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .envs(&config.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|source| McpError::Spawn {
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
        let request_id = Arc::new(AtomicU64::new(1));
        let (writer_tx, writer_rx) = mpsc::channel::<String>(WRITER_CHANNEL_CAPACITY);

        // ─── Writer task: drain channel into stdin ──────────────────────
        spawn_writer_task(stdin, writer_rx, server_name_arc.clone());

        // ─── Reader task: parse stdout into responses ───────────────────
        spawn_reader_task(stdout, pending.clone(), server_name_arc.clone());

        // ─── Stderr task: log everything the server writes to stderr ────
        spawn_stderr_task(stderr, server_name_arc.clone());

        let handle = McpHandle {
            server_name: server_name_arc.clone(),
            writer_tx,
            pending,
            request_id,
        };

        // ─── Handshake ──────────────────────────────────────────────────
        let init_params = InitializeParams {
            protocol_version: MCP_PROTOCOL_VERSION.to_string(),
            capabilities: ClientCapabilities::default(),
            client_info: ClientInfo::default(),
        };
        let init_raw = handle
            .request("initialize", Some(serde_json::to_value(init_params)?))
            .await?;
        let init: InitializeResult = serde_json::from_value(init_raw)?;

        handle.notify("notifications/initialized", None).await?;

        debug!(
            server = server_name,
            negotiated_version = %init.protocol_version,
            server_name = %init.server_info.name,
            server_version = %init.server_info.version,
            "MCP initialize complete"
        );

        // ─── Tools discovery ────────────────────────────────────────────
        let tools = if init.capabilities.tools.is_some() {
            handle.list_tools().await.unwrap_or_else(|err| {
                warn!(server = server_name, error = %err, "tools/list failed");
                Vec::new()
            })
        } else {
            Vec::new()
        };

        Ok(Self {
            handle,
            child,
            server_info: init.server_info,
            capabilities: init.capabilities,
            tools,
        })
    }

    pub fn handle(&self) -> &McpHandle {
        &self.handle
    }

    pub fn server_name(&self) -> &str {
        self.handle.server_name()
    }

    pub fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    pub fn capabilities(&self) -> &ServerCapabilities {
        &self.capabilities
    }

    pub fn tools(&self) -> &[McpToolDef] {
        &self.tools
    }

    /// Graceful shutdown: send the `shutdown` notification, give the server
    /// a moment to cleanup, then kill the child unconditionally. Returns
    /// when the child is reaped.
    pub async fn shutdown(mut self) {
        // Best-effort `shutdown` notification — many servers ignore it
        // because the spec is still loose on lifecycle messages.
        let _ = self.handle.notify("shutdown", None).await;
        let _ = tokio::time::timeout(Duration::from_millis(200), async {
            // Give the writer a tick to flush.
            tokio::task::yield_now().await;
        })
        .await;
        // `kill_on_drop` would also handle this, but explicit is clearer.
        let _ = self.child.kill().await;
    }
}

// ─── Background tasks ──────────────────────────────────────────────────────

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
                    // First try as a response (has id + result/error).
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
                    // Otherwise it's a notification or unknown payload —
                    // log it for visibility but don't act.
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
        // Drain any remaining pending requests so they fail fast instead of
        // timing out.
        let mut guard = pending.lock().await;
        for (_, tx) in guard.drain() {
            // Build a synthetic "disconnected" response so callers get an
            // explicit error rather than waiting 30s.
            let _ = tx.send(JsonRpcResponse {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: 0,
                result: None,
                error: Some(crate::protocol::JsonRpcError {
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

    #[test]
    fn handle_is_cloneable() {
        // Sanity check: a cloned handle shares its pending map + request id.
        let (tx, _rx) = mpsc::channel::<String>(1);
        let handle = McpHandle {
            server_name: Arc::new("t".to_string()),
            writer_tx: tx,
            pending: Arc::new(Mutex::new(HashMap::new())),
            request_id: Arc::new(AtomicU64::new(5)),
        };
        let clone = handle.clone();
        assert_eq!(clone.server_name(), "t");
        // Both clones increment the SAME atomic.
        let a = handle.request_id.fetch_add(1, Ordering::SeqCst);
        let b = clone.request_id.fetch_add(1, Ordering::SeqCst);
        assert_eq!(a, 5);
        assert_eq!(b, 6);
    }

    #[tokio::test]
    async fn request_on_dropped_writer_returns_disconnected() {
        let (tx, rx) = mpsc::channel::<String>(1);
        drop(rx); // simulate the writer task already exited
        let handle = McpHandle {
            server_name: Arc::new("dead".to_string()),
            writer_tx: tx,
            pending: Arc::new(Mutex::new(HashMap::new())),
            request_id: Arc::new(AtomicU64::new(1)),
        };
        let err = handle.request("ping", None).await.unwrap_err();
        match err {
            McpError::Disconnected(name) => assert_eq!(name, "dead"),
            other => panic!("expected Disconnected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn connect_to_missing_binary_returns_spawn_error() {
        let config = McpServerConfig {
            command: "/this/binary/does/not/exist".to_string(),
            args: vec![],
            env: Default::default(),
            shared: true,
        };
        let err = McpClient::connect("ghost", &config).await.unwrap_err();
        match err {
            McpError::Spawn { server, .. } => assert_eq!(server, "ghost"),
            other => panic!("expected Spawn error, got {other:?}"),
        }
    }
}
