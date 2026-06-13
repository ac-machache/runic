//! HTTP/SSE client for `runic serve`.
//!
//! Non-streaming calls use `gloo-net`; the run stream reads the response
//! body as a `ReadableStream` (via `wasm-streams`) and parses SSE frames
//! incrementally so tokens render as they arrive. SSE `data:` payloads are
//! handed back as raw `serde_json::Value` — the UI matches on the `type`
//! field, staying decoupled from the server's internal event enum.

use futures::StreamExt;
use gloo_net::http::Request;
use serde_json::Value;

#[derive(Clone)]
pub struct ApiClient {
    base: String,
    tenant: String,
}

impl ApiClient {
    pub fn new(base: String, tenant: String) -> Self {
        let base = base.trim_end_matches('/').to_string();
        Self { base, tenant }
    }

    pub async fn list_threads(&self) -> Result<Vec<String>, String> {
        let url = format!("{}/threads", self.base);
        let resp = Request::get(&url)
            .header("x-runic-tenant", &self.tenant)
            .send()
            .await
            .map_err(e2s)?;
        let v: Value = resp.json().await.map_err(e2s)?;
        Ok(v.get("threads")
            .and_then(|t| t.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.get("thread_id").and_then(|x| x.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default())
    }

    pub async fn create_thread(&self, id: Option<&str>) -> Result<String, String> {
        let url = format!("{}/threads", self.base);
        let body = match id {
            Some(i) => serde_json::json!({ "thread_id": i }),
            None => serde_json::json!({}),
        };
        let resp = Request::post(&url)
            .header("x-runic-tenant", &self.tenant)
            .json(&body)
            .map_err(e2s)?
            .send()
            .await
            .map_err(e2s)?;
        let v: Value = resp.json().await.map_err(e2s)?;
        v.get("thread_id")
            .and_then(|x| x.as_str())
            .map(String::from)
            .ok_or_else(|| "response missing thread_id".to_string())
    }

    /// Full stored event log for a thread (snapshot, not a stream).
    pub async fn thread_events(&self, id: &str) -> Result<Vec<Value>, String> {
        let url = format!("{}/threads/{id}/events", self.base);
        let resp = Request::get(&url)
            .header("x-runic-tenant", &self.tenant)
            .send()
            .await
            .map_err(e2s)?;
        let v: Value = resp.json().await.map_err(e2s)?;
        Ok(v.get("events")
            .and_then(|e| e.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// Deliver a HITL approval decision for a parked tool call. `decision`
    /// is `{"decision":"submit","final_input":{…}}` or
    /// `{"decision":"cancel","reason":"…"}`. The `run_id` path segment is
    /// ignored server-side (the decision is keyed by `call_id`), so a
    /// placeholder is fine.
    pub async fn submit_approval(
        &self,
        thread: &str,
        call_id: &str,
        decision: serde_json::Value,
    ) -> Result<(), String> {
        let url = format!("{}/threads/{thread}/runs/live/approvals/{call_id}", self.base);
        let resp = Request::post(&url)
            .header("x-runic-tenant", &self.tenant)
            .json(&decision)
            .map_err(e2s)?
            .send()
            .await
            .map_err(e2s)?;
        if resp.ok() {
            Ok(())
        } else {
            Err(format!("approval rejected: HTTP {}", resp.status()))
        }
    }

    /// POST a run and invoke `on_event` for every parsed SSE event as it
    /// streams in. Resolves when the stream closes.
    pub async fn stream_run(
        &self,
        thread: &str,
        message: &str,
        mut on_event: impl FnMut(Value),
    ) -> Result<(), String> {
        let url = format!("{}/threads/{thread}/runs/stream", self.base);
        let body = serde_json::json!({ "message": message });
        let resp = Request::post(&url)
            .header("x-runic-tenant", &self.tenant)
            .json(&body)
            .map_err(e2s)?
            .send()
            .await
            .map_err(e2s)?;

        let raw = resp.body().ok_or_else(|| "response has no body".to_string())?;
        let mut stream = wasm_streams::ReadableStream::from_raw(raw).into_stream();

        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| "stream read error".to_string())?;
            let arr = js_sys::Uint8Array::new(&chunk);
            let mut bytes = vec![0u8; arr.length() as usize];
            arr.copy_to(&mut bytes);
            buf.extend_from_slice(&bytes);

            // SSE frames are separated by a blank line ("\n\n").
            while let Some(pos) = find_sub(&buf, b"\n\n") {
                let frame: Vec<u8> = buf.drain(..pos + 2).collect();
                let frame = String::from_utf8_lossy(&frame[..frame.len() - 2]);
                for line in frame.lines() {
                    let Some(data) = line.strip_prefix("data:") else { continue };
                    let data = data.trim();
                    if data.is_empty() {
                        continue;
                    }
                    if let Ok(v) = serde_json::from_str::<Value>(data) {
                        on_event(v);
                    }
                }
            }
        }
        Ok(())
    }
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn e2s<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}
