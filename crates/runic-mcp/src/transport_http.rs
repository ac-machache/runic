//! Streamable HTTP transport for MCP (the 2025-03-26 spec successor to
//! the older HTTP+SSE transport).
//!
//! Protocol summary:
//!   - All messages flow through ONE endpoint (the URL the user configures).
//!   - The client POSTs a JSON-RPC request and reads the response.
//!   - The server can answer in two formats, distinguished by `Content-Type`:
//!       1. `application/json` — single JSON-RPC response
//!       2. `text/event-stream` — SSE stream of JSON-RPC messages; we walk
//!          events until we find one whose `id` matches our request
//!   - Sessions are tracked via an opaque `Mcp-Session-Id` header. The
//!     server sets it on initialize; we echo it on every subsequent request.
//!
//! Notifications (no `id`) are POSTed and any response body is discarded.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::error::McpError;
use crate::protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
use crate::transport::{RequestIdCounter, Transport, REQUEST_TIMEOUT};

const SESSION_ID_HEADER: &str = "mcp-session-id";

#[derive(Debug)]
pub struct HttpTransport {
    server_name: Arc<String>,
    url: String,
    client: reqwest::Client,
    request_id: Arc<RequestIdCounter>,
    /// Set after `initialize` if the server returned an `Mcp-Session-Id`
    /// header. Echoed on every subsequent request.
    session_id: Mutex<Option<String>>,
    /// Extra headers from config (e.g. auth tokens). Sent on every request.
    extra_headers: HeaderMap,
}

impl HttpTransport {
    pub fn new(
        server_name: &str,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<Self, McpError> {
        guard_ssrf(server_name, url)?;

        let mut extra_headers = HeaderMap::new();
        for (k, v) in headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| McpError::protocol(format!("invalid header name '{k}': {e}")))?;
            let value = HeaderValue::from_str(v)
                .map_err(|e| McpError::protocol(format!("invalid header value for '{k}': {e}")))?;
            extra_headers.insert(name, value);
        }

        // Build a reqwest client tuned for the agent loop's responsiveness
        // expectations. The per-request timeout we enforce ourselves below.
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT + Duration::from_secs(5))
            .user_agent(concat!("runic-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| McpError::protocol(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            server_name: Arc::new(server_name.to_string()),
            url: url.to_string(),
            client,
            request_id: Arc::new(RequestIdCounter::new()),
            session_id: Mutex::new(None),
            extra_headers,
        })
    }

    fn build_headers(&self, session_id: Option<&str>) -> HeaderMap {
        let mut headers = self.extra_headers.clone();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        // Tell the server we can handle either response shape.
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        if let Some(sid) = session_id
            && let Ok(v) = HeaderValue::from_str(sid)
        {
            headers.insert(HeaderName::from_static(SESSION_ID_HEADER), v);
        }
        headers
    }
}

#[async_trait]
impl Transport for HttpTransport {
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
        let body = serde_json::to_vec(&req)?;

        let session_id = self.session_id.lock().await.clone();
        let headers = self.build_headers(session_id.as_deref());

        let resp = match tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.client.post(&self.url).headers(headers).body(body).send(),
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(err)) => {
                return Err(McpError::protocol(format!("HTTP request failed: {err}")));
            }
            Err(_) => return Err(McpError::Timeout(REQUEST_TIMEOUT)),
        };

        // Capture an Mcp-Session-Id from the response. The initialize call
        // is the usual place this lands, but the spec says we should accept
        // it on any response.
        if let Some(sid) = resp.headers().get(SESSION_ID_HEADER)
            && let Ok(s) = sid.to_str()
        {
            let mut guard = self.session_id.lock().await;
            if guard.as_deref() != Some(s) {
                debug!(server = %self.server_name, session_id = s, "set MCP session id");
                *guard = Some(s.to_string());
            }
        }

        if !resp.status().is_success() {
            let status = resp.status();
            // A 404/410 on a request that carried a session id means the
            // server dropped our session — recoverable by re-initializing.
            if (status.as_u16() == 404 || status.as_u16() == 410) && session_id.is_some() {
                return Err(McpError::StaleSession((*self.server_name).clone()));
            }
            let body = resp.text().await.unwrap_or_default();
            return Err(McpError::protocol(format!(
                "HTTP {status} from server '{}': {body}",
                self.server_name
            )));
        }

        // Pick a path based on content type.
        let ctype = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        if ctype.starts_with("application/json") {
            let response: JsonRpcResponse = resp.json().await.map_err(|err| {
                McpError::protocol(format!("invalid JSON response body: {err}"))
            })?;
            return jsonrpc_result(response, id);
        }

        if ctype.starts_with("text/event-stream") {
            return read_sse_until_id(resp, id, &self.server_name).await;
        }

        // Some servers return text/plain or omit Content-Type. Try to parse
        // as JSON-RPC anyway — if it works, great; if not, surface the
        // body in the error so the user can see what they're dealing with.
        let body = resp.text().await.unwrap_or_default();
        match serde_json::from_str::<JsonRpcResponse>(&body) {
            Ok(response) => jsonrpc_result(response, id),
            Err(_) => Err(McpError::protocol(format!(
                "unexpected response content-type '{ctype}' and body could not be parsed as JSON-RPC: {body}"
            ))),
        }
    }

    async fn notify(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<(), McpError> {
        let notif = JsonRpcNotification::new(method, params);
        let body = serde_json::to_vec(&notif)?;

        let session_id = self.session_id.lock().await.clone();
        let headers = self.build_headers(session_id.as_deref());

        let resp = match tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.client.post(&self.url).headers(headers).body(body).send(),
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(err)) => {
                return Err(McpError::protocol(format!(
                    "HTTP notification failed: {err}"
                )));
            }
            Err(_) => return Err(McpError::Timeout(REQUEST_TIMEOUT)),
        };
        // A 202 Accepted is the standard answer to a notification, but some
        // servers send 200 with an empty body. Either is fine. Anything 4xx/5xx
        // is a problem worth reporting.
        if !resp.status().is_success() {
            warn!(
                server = %self.server_name,
                status = %resp.status(),
                "MCP notification returned non-success"
            );
        }
        Ok(())
    }

    async fn close(&self) {
        // Best effort — many servers accept `shutdown` as a notification.
        let _ = self.notify("shutdown", None).await;
        // No persistent connection to tear down; reqwest::Client handles
        // its own pool drop.
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Reject MCP server URLs that point at cloud-metadata / link-local endpoints
/// — a server config shouldn't be able to turn the agent into an SSRF vector
/// against the host's instance-metadata service.
fn guard_ssrf(server_name: &str, url: &str) -> Result<(), McpError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| McpError::protocol(format!("invalid MCP url '{url}': {e}")))?;
    let host = parsed.host_str().unwrap_or("").to_lowercase();

    const BLOCKED_HOSTS: &[&str] = &[
        "169.254.169.254",              // AWS/GCP/Azure IMDS (link-local)
        "metadata.google.internal",     // GCP metadata
        "metadata.goog",                // GCP metadata (short)
    ];
    let blocked = BLOCKED_HOSTS.iter().any(|h| host == *h)
        || host == "metadata"
        || host.starts_with("169.254."); // link-local range

    if blocked {
        return Err(McpError::protocol(format!(
            "refusing MCP server '{server_name}': url host '{host}' is a blocked metadata/link-local endpoint"
        )));
    }
    Ok(())
}

fn jsonrpc_result(
    response: JsonRpcResponse,
    expected_id: u64,
) -> Result<serde_json::Value, McpError> {
    // The server SHOULD echo our id. Warn if it doesn't, but trust the
    // server's response either way — some servers (notably ones bridging
    // multiple backends) re-numbers responses.
    if response.id != expected_id {
        warn!(
            "MCP server returned id={} when we expected id={expected_id}",
            response.id
        );
    }
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

async fn read_sse_until_id(
    resp: reqwest::Response,
    expected_id: u64,
    server_name: &str,
) -> Result<serde_json::Value, McpError> {
    // `eventsource_stream` wants a stream of bytes/string chunks → we feed it
    // reqwest's byte stream.
    let mut events = resp.bytes_stream().eventsource();

    loop {
        let next = tokio::time::timeout(REQUEST_TIMEOUT, events.next()).await;
        match next {
            Ok(Some(Ok(event))) => {
                if event.data.trim().is_empty() {
                    continue;
                }
                // Each event's `data` is a JSON-RPC message. Skip ones that
                // aren't responses (notifications) or whose id doesn't match.
                match serde_json::from_str::<JsonRpcResponse>(&event.data) {
                    Ok(response) => {
                        if response.id == expected_id {
                            return jsonrpc_result(response, expected_id);
                        } else {
                            debug!(
                                server = %server_name,
                                id = response.id,
                                "SSE event with mismatched id (likely a notification or out-of-band response)"
                            );
                        }
                    }
                    Err(_) => {
                        debug!(
                            server = %server_name,
                            "SSE event was not a JSON-RPC response, ignoring"
                        );
                    }
                }
            }
            Ok(Some(Err(err))) => {
                return Err(McpError::protocol(format!("SSE stream error: {err}")));
            }
            Ok(None) => {
                return Err(McpError::protocol(format!(
                    "SSE stream closed before response with id={expected_id} arrived"
                )));
            }
            Err(_) => return Err(McpError::Timeout(REQUEST_TIMEOUT)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_transport_can_be_constructed_with_no_headers() {
        let t = HttpTransport::new("h", "https://example.com/mcp", &HashMap::new()).unwrap();
        assert_eq!(t.server_name(), "h");
    }

    #[test]
    fn http_transport_accepts_valid_headers() {
        let mut headers = HashMap::new();
        headers.insert("Authorization".into(), "Bearer abc".into());
        headers.insert("X-Tenant".into(), "acme".into());
        let t = HttpTransport::new("h", "https://example.com/mcp", &headers).unwrap();
        assert_eq!(t.server_name(), "h");
    }

    #[test]
    fn http_transport_rejects_invalid_header_name() {
        let mut headers = HashMap::new();
        // Newline in header name is invalid.
        headers.insert("Bad\nName".into(), "v".into());
        let result = HttpTransport::new("h", "https://example.com/mcp", &headers);
        assert!(result.is_err());
    }

    #[test]
    fn http_transport_blocks_metadata_ssrf() {
        let h = HashMap::new();
        // Cloud metadata / link-local endpoints must be refused.
        assert!(HttpTransport::new("s", "http://169.254.169.254/latest/meta-data", &h).is_err());
        assert!(HttpTransport::new("s", "http://metadata.google.internal/x", &h).is_err());
        assert!(HttpTransport::new("s", "http://169.254.1.2/foo", &h).is_err());
        // A normal remote endpoint is fine.
        assert!(HttpTransport::new("s", "https://api.example.com/mcp", &h).is_ok());
    }

    #[tokio::test]
    async fn http_transport_request_to_unreachable_url_errors_out() {
        let t = HttpTransport::new(
            "unreachable",
            "http://127.0.0.1:1/no-server-here",
            &HashMap::new(),
        )
        .unwrap();
        let err = t.request("ping", None).await.unwrap_err();
        match err {
            McpError::Protocol(_) | McpError::Timeout(_) => {}
            other => panic!("expected Protocol/Timeout, got {other:?}"),
        }
    }
}
