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

use crate::model::{Attachment, ThreadInfo};

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

    /// A page of threads (id + label). `next_cursor` is `Some` when more remain;
    /// pass it back to fetch the next page.
    pub async fn list_threads(
        &self,
        cursor: Option<&str>,
    ) -> Result<(Vec<ThreadInfo>, Option<String>), String> {
        let mut req = Request::get(&format!("{}/threads", self.base));
        if let Some(c) = cursor {
            req = req.query([("cursor", c)]);
        }
        let v: Value = req
            .header("x-runic-tenant", &self.tenant)
            .send()
            .await
            .map_err(e2s)?
            .json()
            .await
            .map_err(e2s)?;
        let threads = v
            .get("threads")
            .and_then(|t| t.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|t| {
                        let id = t.get("thread_id").and_then(|x| x.as_str())?.to_string();
                        let label = t.get("label").and_then(|x| x.as_str()).map(String::from);
                        Some(ThreadInfo { id, label })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let next_cursor = v
            .get("next_cursor")
            .and_then(|c| c.as_str())
            .map(String::from);
        Ok((threads, next_cursor))
    }

    /// Upload a file to the thread's artifact store (raw body). Returns the
    /// stored [`Attachment`] (id + size) the message will reference.
    pub async fn upload_artifact(
        &self,
        thread: &str,
        bytes: Vec<u8>,
        media_type: &str,
        filename: &str,
    ) -> Result<Attachment, String> {
        let url = format!("{}/threads/{thread}/artifacts", self.base);
        let arr = js_sys::Uint8Array::from(bytes.as_slice());
        let v: Value = Request::post(&url)
            .header("x-runic-tenant", &self.tenant)
            .header("content-type", media_type)
            .header("x-runic-filename", filename)
            .body(arr)
            .map_err(e2s)?
            .send()
            .await
            .map_err(e2s)?
            .json()
            .await
            .map_err(e2s)?;
        let id = v
            .get("id")
            .and_then(|x| x.as_str())
            .ok_or_else(|| "upload response missing id".to_string())?
            .to_string();
        let size = v.get("size").and_then(|x| x.as_u64()).unwrap_or(0);
        Ok(Attachment {
            id,
            name: filename.to_string(),
            media_type: media_type.to_string(),
            size,
        })
    }

    /// Transcribe audio to text (preprocessing step — no thread, nothing stored).
    pub async fn transcribe(
        &self,
        bytes: Vec<u8>,
        media_type: &str,
        filename: &str,
    ) -> Result<String, String> {
        let url = format!("{}/transcribe", self.base);
        let arr = js_sys::Uint8Array::from(bytes.as_slice());
        let resp = Request::post(&url)
            .header("x-runic-tenant", &self.tenant)
            .header("content-type", media_type)
            .header("x-runic-filename", filename)
            .body(arr)
            .map_err(e2s)?
            .send()
            .await
            .map_err(e2s)?;
        let status = resp.status();
        let v: Value = resp.json().await.map_err(e2s)?;
        if !(200..300).contains(&status) {
            let message = v
                .get("message")
                .and_then(|m| m.as_str())
                .or_else(|| v.get("error").and_then(|m| m.as_str()))
                .unwrap_or("transcribe failed");
            return Err(format!("{message} ({status})"));
        }
        v.get("text")
            .and_then(|t| t.as_str())
            .map(String::from)
            .ok_or_else(|| "transcribe response missing text".to_string())
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
        let mut all = Vec::new();
        let mut after_seq = 0_u64;

        loop {
            let url = format!("{}/threads/{id}/events", self.base);
            let v: Value = Request::get(&url)
                .query([("after_seq", after_seq.to_string())])
                .header("x-runic-tenant", &self.tenant)
                .send()
                .await
                .map_err(e2s)?
                .json()
                .await
                .map_err(e2s)?;

            if let Some(events) = v.get("events").and_then(|e| e.as_array()) {
                all.extend(events.iter().cloned());
            }

            let has_more = v.get("has_more").and_then(|x| x.as_bool()).unwrap_or(false);
            let next = v.get("next_after_seq").and_then(|x| x.as_u64());
            if !has_more {
                break;
            }
            let Some(next_after_seq) = next else {
                return Err("events page missing next_after_seq".into());
            };
            if next_after_seq <= after_seq {
                return Err("events pagination did not advance".into());
            }
            after_seq = next_after_seq;
        }

        Ok(all)
    }

    /// Full thread state: system prompt + messages (as sent to the model) +
    /// counts. Returned verbatim as JSON for the state inspector.
    pub async fn thread_state(&self, id: &str) -> Result<Value, String> {
        let url = format!("{}/threads/{id}/state", self.base);
        let resp = Request::get(&url)
            .header("x-runic-tenant", &self.tenant)
            .send()
            .await
            .map_err(e2s)?;
        resp.json().await.map_err(e2s)
    }

    /// Answer a parked HITL `ask_user`. The server scopes the answer by
    /// `(tenant, thread_id, ask_id)`.
    pub async fn submit_answer(
        &self,
        thread: &str,
        ask_id: &str,
        answer: String,
    ) -> Result<(), String> {
        let url = format!("{}/threads/{thread}/asks/{ask_id}", self.base);
        let resp = Request::post(&url)
            .header("x-runic-tenant", &self.tenant)
            .json(&serde_json::json!({ "answer": answer }))
            .map_err(e2s)?
            .send()
            .await
            .map_err(e2s)?;
        if resp.ok() {
            Ok(())
        } else {
            Err(format!("answer rejected: HTTP {}", resp.status()))
        }
    }

    /// POST a run and invoke `on_event` for every parsed SSE event as it
    /// streams in. Resolves when the stream closes.
    pub async fn stream_run(
        &self,
        thread: &str,
        message: &str,
        attachments: &[Attachment],
        context: Option<Value>,
        abort: Option<&web_sys::AbortSignal>,
        mut on_event: impl FnMut(Value),
    ) -> Result<(), String> {
        let url = format!("{}/threads/{thread}/runs/stream", self.base);
        let mut body = if attachments.is_empty() {
            serde_json::json!({ "message": message })
        } else {
            // A text block + one artifact_ref pointer per attachment (the bytes
            // already live in the store; the run body keeps only the reference).
            let mut content = vec![serde_json::json!({ "type": "text", "text": message })];
            for a in attachments {
                content.push(serde_json::json!({
                    "type": "artifact_ref",
                    "id": a.id,
                    "media_type": a.media_type,
                    "filename": a.name,
                }));
            }
            serde_json::json!({ "content": content })
        };
        // Per-run context (user_id, provider, allow_web_search, …) — sent
        // verbatim; the server's build_run_context decides what keys mean.
        if let Some(ctx) = context {
            body["context"] = ctx;
        }
        let resp = Request::post(&url)
            .header("x-runic-tenant", &self.tenant)
            .abort_signal(abort)
            .json(&body)
            .map_err(e2s)?
            .send()
            .await
            .map_err(e2s)?;

        let raw = resp
            .body()
            .ok_or_else(|| "response has no body".to_string())?;
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
                    let Some(data) = line.strip_prefix("data:") else {
                        continue;
                    };
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
