//! Run streaming.
//!
//! Two endpoints share most of the logic:
//!
//! - `POST /threads/:id/runs/stream` — drive a fresh turn, stream events
//!   in real time.
//! - `GET  /threads/:id/runs/:run_id/stream` — replay a past run's
//!   persisted events (and, if it's still in flight, attach to the live
//!   broadcast for the rest).
//!
//! The wire format is documented in [`crate::wire`]. Each SSE event
//! carries the `WireEvent` JSON body, the matching `event:` field, and
//! (for replay) the `id:` field from the store's seq number — that's
//! what lets `Last-Event-ID` work for resume.

use std::convert::Infallible;
use std::time::Duration;

use async_stream::stream;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::Json;
use futures::stream::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::BroadcastStream;

use runic_message_types::{ContentBlock, Message, Role};

use crate::app::AppState;
use crate::error::ServeError;
use crate::tenant::Tenant;
use crate::wire::{from_agent_event, from_session_event, WireEvent};

/// The user turn for a run. Two shapes, checked in order:
///
///   {"message": "plain text"}                         // text shorthand
///   {"content": [{"type":"text","text":"..."},        // full blocks
///                {"type":"blob","id":"sha256…", "mime":"image/png", "size":1234}]}
///
/// `content` carries `ContentBlock`s directly, so an upload is just a
/// `blob` block referencing bytes already in the BlobStore — the
/// provider adapter materializes it on the way to the model. No separate
/// upload endpoint: multipart content rides inside the message, the way
/// the providers' own APIs model it.
#[derive(Debug, Deserialize)]
pub struct RunMessageRequest {
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub content: Option<Vec<ContentBlock>>,
    /// When set, the run is given a synthesized finish tool with this JSON
    /// schema; it ends when the model calls it with valid args, and the
    /// result arrives as a `structured_output` event.
    #[serde(default)]
    pub output_schema: Option<serde_json::Value>,
}

impl RunMessageRequest {
    /// Resolve into the user `Message` to run, or a `BadRequest` if the
    /// body carried neither a non-empty `message` nor any `content`.
    fn into_message(self) -> Result<Message, ServeError> {
        match (self.content, self.message) {
            (Some(blocks), _) if !blocks.is_empty() => Ok(Message {
                role: Role::User,
                content: blocks,
                timestamp: None,
                tool_duration_ms: None,
            }),
            (_, Some(text)) if !text.trim().is_empty() => Ok(Message::user(&text)),
            _ => Err(ServeError::BadRequest(
                "run request needs a non-empty `message` string or a non-empty `content` array".into(),
            )),
        }
    }
}

#[derive(Debug, Serialize)]
struct StreamErrorEvent {
    error: String,
}

/// `POST /threads/:id/runs/stream`
///
/// Locks the thread's Agent for the duration of the run, kicks off a
/// streaming completion, and pipes every `AgentEvent` to the client as
/// an SSE `WireEvent`. Concurrent runs on the same thread serialize on
/// the pool's Mutex; concurrent runs on different threads parallelise.
pub async fn create_and_stream_run(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(thread_id): Path<String>,
    Json(req): Json<RunMessageRequest>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ServeError> {
    // Per-request structured output schema (None clears it on the warm agent).
    let output_schema = req.output_schema.clone();
    // Validate the body BEFORE building/locking anything, so a malformed
    // request gets a clean 400 instead of a half-open SSE stream.
    let user_msg = req.into_message()?;

    let agent_arc = state.pool.get_or_build(&tenant, &thread_id).await;

    // The run executes in a DETACHED task that owns the agent's slot for
    //
    // (See below: before running, we point this thread's HITL approvals at
    // `sse_tx` so an approval raised mid-run surfaces on this same stream.)
    // the whole turn (so concurrent POSTs on the same thread still
    // serialize) and ALWAYS returns the agent when finished. The SSE
    // response only *observes* via `sse_rx`. If the client disconnects,
    // the response stream is dropped and `sse_tx.send` starts erroring —
    // we ignore that and keep draining the agent to completion, so the
    // agent is returned to its slot regardless. Previously the return
    // happened inside the response stream, so a mid-run disconnect left
    // the slot permanently empty and bricked the thread.
    let (sse_tx, mut sse_rx) = tokio::sync::mpsc::channel::<WireEvent>(256);

    // Route this thread's HITL approval prompts onto this run's stream, and
    // tear that down when the run task ends (below).
    state.approval_hub.set_wire(&thread_id, sse_tx.clone());

    let hub = state.approval_hub.clone();
    let thread_for_task = thread_id.clone();
    tokio::spawn(async move {
        let mut slot = agent_arc.lock().await;
        let mut agent = match slot.take() {
            Some(a) => a,
            None => {
                let _ = sse_tx
                    .send(WireEvent::Warning {
                        message: "agent slot was empty — a prior run panicked; DELETE the thread to reset".into(),
                    })
                    .await;
                return;
            }
        };

        // Apply (or clear) structured output for this run.
        agent.set_structured_output(output_schema);

        let (mut events, handle) = agent.run_streaming_message(user_msg);
        while let Some(event) = events.next().await {
            // Ignore send errors (client gone) but keep draining so the
            // agent runs to completion and is returned below.
            let _ = sse_tx.send(from_agent_event(event)).await;
        }

        match handle.await {
            Ok((returned_agent, _outcome)) => {
                *slot = Some(returned_agent);
            }
            Err(join_err) => {
                // The run task panicked — the agent is lost. Leave the slot
                // empty; the next request surfaces a clean error and the
                // client can DELETE/recreate the thread.
                let _ = sse_tx
                    .send(WireEvent::Warning { message: format!("run task failed: {join_err}") })
                    .await;
            }
        }
        // Stop routing approvals at this (now-closing) stream.
        hub.clear_wire(&thread_for_task);
        // slot guard drops here → the next queued run on this thread proceeds.
    });

    let stream = stream! {
        while let Some(wire) = sse_rx.recv().await {
            yield Ok(to_sse(&wire, None));
        }
    };

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text(":keepalive"),
    ))
}

/// `GET /threads/:id/runs/:run_id/stream`
///
/// Read persisted events for the run, filtered to those with seq > the
/// `Last-Event-ID` header (if present). When the stored events run
/// out, attach to the live broadcast if the corresponding Agent is
/// still warm in the pool — so a client that disconnected mid-run can
/// reconnect with `Last-Event-ID` and pick up where it left off.
pub async fn replay_run(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path((thread_id, run_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ServeError> {
    let after_seq = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    let stored = state
        .session_store
        .read_after(&tenant, &thread_id, after_seq)
        .await?;

    // Filter to events for THIS run_id, drop the internal-only ones.
    let filtered: Vec<(u64, WireEvent)> = stored
        .into_iter()
        .filter(|s| match &s.event {
            runic_agent_core::SessionEvent::RunStart { run_id: rid, .. }
            | runic_agent_core::SessionEvent::RunEnd { run_id: rid, .. }
            | runic_agent_core::SessionEvent::Message { run_id: rid, .. }
            | runic_agent_core::SessionEvent::TurnBoundary { run_id: rid, .. }
            | runic_agent_core::SessionEvent::HookRan { run_id: rid, .. }
            | runic_agent_core::SessionEvent::StateSnapshot { run_id: rid, .. } => rid == &run_id,
        })
        .filter_map(|s| from_session_event(s.event).map(|w| (s.seq, w)))
        .collect();

    let agent_arc = state.pool.get_or_build(&tenant, &thread_id).await;

    let stream = stream! {
        // 1) emit the persisted events the client missed.
        for (seq, wire) in filtered {
            yield Ok(to_sse(&wire, Some(seq)));
        }

        // 2) attach to the agent's live broadcast for anything still
        //    in flight. If the run already terminated we get a
        //    Closed/Lagged immediately and stop.
        let slot = agent_arc.lock().await;
        let rx = match slot.as_ref().and_then(|a| a.state().subscribe_events()) {
            Some(rx) => rx,
            None => {
                yield Ok(to_sse(&WireEvent::Done { total_turns: 0 }, None));
                return;
            }
        };
        drop(slot);

        let mut live = BroadcastStream::new(rx);
        while let Some(received) = live.next().await {
            let session_event = match received {
                Ok(e) => e,
                Err(_lagged) => continue,
            };
            // Only pass-through events for THIS run.
            let same_run = matches!(
                &session_event,
                runic_agent_core::SessionEvent::RunStart { run_id: rid, .. }
                | runic_agent_core::SessionEvent::RunEnd { run_id: rid, .. }
                | runic_agent_core::SessionEvent::Message { run_id: rid, .. }
                | runic_agent_core::SessionEvent::TurnBoundary { run_id: rid, .. }
                | runic_agent_core::SessionEvent::HookRan { run_id: rid, .. }
                | runic_agent_core::SessionEvent::StateSnapshot { run_id: rid, .. }
                if rid == &run_id
            );
            if !same_run {
                continue;
            }
            if let Some(wire) = from_session_event(session_event) {
                yield Ok(to_sse(&wire, None));
            }
        }

        yield Ok(to_sse(&WireEvent::Done { total_turns: 0 }, None));
    };

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text(":keepalive"),
    ))
}

/// `POST /threads/:id/runs/:run_id/approvals/:call_id`
///
/// Deliver an operator decision to a HITL tool parked inside a running
/// agent (see [`crate::approval`]). Body is a [`UserDecision`]:
///   `{"decision":"submit","final_input":{…}}` or
///   `{"decision":"cancel","reason":"…"}`.
/// The parked `review()` wakes, the tool runs (or is cancelled), and the
/// rest of the turn streams on the original run connection.
pub async fn submit_approval(
    State(state): State<AppState>,
    Tenant(_tenant): Tenant,
    Path((_thread_id, _run_id, call_id)): Path<(String, String, String)>,
    Json(decision): Json<runic_agent_core::UserDecision>,
) -> Result<StatusCode, ServeError> {
    if state.approval_hub.submit_decision(&call_id, decision) {
        Ok(StatusCode::ACCEPTED)
    } else {
        // No approval is parked under this call_id — already decided, timed
        // out, or never existed.
        Err(ServeError::BadRequest(format!(
            "no pending approval for call_id '{call_id}'"
        )))
    }
}

fn to_sse(wire: &WireEvent, id: Option<u64>) -> SseEvent {
    let body = serde_json::to_string(wire).unwrap_or_else(|err| {
        serde_json::to_string(&StreamErrorEvent {
            error: format!("serialise wire event: {err}"),
        })
        .unwrap_or_else(|_| "{}".into())
    });
    let mut event = SseEvent::default().event(wire.event_kind()).data(body);
    if let Some(seq) = id {
        event = event.id(seq.to_string());
    }
    event
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> RunMessageRequest {
        serde_json::from_str(json).expect("valid request json")
    }

    #[test]
    fn text_shorthand_becomes_a_user_text_message() {
        let msg = parse(r#"{"message":"hello"}"#).into_message().unwrap();
        assert!(matches!(msg.role, Role::User));
        match &msg.content[0] {
            ContentBlock::Text { text, .. } => assert_eq!(text, "hello"),
            other => panic!("expected text block, got {other:?}"),
        }
    }

    #[test]
    fn content_blocks_pass_through_including_a_blob_ref() {
        let req = parse(
            r#"{"content":[
                {"type":"text","text":"look at this"},
                {"type":"blob","id":"abc123","mime":"image/png","size":42}
            ]}"#,
        );
        let msg = req.into_message().unwrap();
        assert_eq!(msg.content.len(), 2);
        assert!(matches!(&msg.content[0], ContentBlock::Text { text, .. } if text == "look at this"));
        match &msg.content[1] {
            ContentBlock::Blob(b) => {
                assert_eq!(b.id, "abc123");
                assert_eq!(b.mime, "image/png");
                assert_eq!(b.size, 42);
            }
            other => panic!("expected blob block, got {other:?}"),
        }
    }

    #[test]
    fn content_takes_precedence_over_message_when_both_present() {
        let req = parse(r#"{"message":"ignored","content":[{"type":"text","text":"win"}]}"#);
        let msg = req.into_message().unwrap();
        assert_eq!(msg.content.len(), 1);
        assert!(matches!(&msg.content[0], ContentBlock::Text { text, .. } if text == "win"));
    }

    #[test]
    fn empty_content_array_falls_back_to_message_text() {
        let msg = parse(r#"{"message":"fallback","content":[]}"#).into_message().unwrap();
        assert!(matches!(&msg.content[0], ContentBlock::Text { text, .. } if text == "fallback"));
    }

    #[test]
    fn neither_field_is_a_bad_request() {
        assert!(matches!(parse(r#"{}"#).into_message(), Err(ServeError::BadRequest(_))));
        assert!(matches!(parse(r#"{"message":"   "}"#).into_message(), Err(ServeError::BadRequest(_))));
        assert!(matches!(parse(r#"{"content":[]}"#).into_message(), Err(ServeError::BadRequest(_))));
    }
}
