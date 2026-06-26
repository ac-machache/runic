//! Run streaming.
//!
//! - `POST /threads/:id/runs/stream` — drive a fresh turn, stream events live.
//! - `GET  /threads/:id/runs/:run_id/stream` — replay a past run's persisted
//!   events and, if it's still in flight, attach to the live broadcast.
//! - `POST /threads/:id/runs/:run_id/asks/:ask_id` — answer a parked
//!   `ask_user` (HITL).
//!
//! The wire format is in [`crate::wire`]. Each SSE event carries the
//! `WireEvent` JSON body, the matching `event:` field, and (for replay) the
//! `id:` field from the store's seq — that's what `Last-Event-ID` resumes on.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use futures::stream::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_stream::wrappers::BroadcastStream;

use runic_agent::AgentEvent;
use runic_types::{ContentBlock, Message};

use crate::app::AppState;
use crate::error::ServeError;
use crate::human::HumanChannel;
use crate::tenant::Tenant;
use crate::wire::{WireEvent, from_agent_event, from_session_event};

/// The user turn for a run. Two shapes, checked in order:
///
///   {"message": "plain text"}                          // text shorthand
///   {"content": [{"type":"text","text":"..."},         // full content blocks
///                {"type":"image","media_type":"image/png","data":"<base64>"}]}
#[derive(Debug, Deserialize)]
pub struct RunMessageRequest {
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub content: Option<Vec<ContentBlock>>,
    /// Open per-request context, passed verbatim to the factory's
    /// `build_run_context`. The app decides what keys mean (user_id, provider,
    /// allow_web_search, …); the serve crate is agnostic to its contents.
    #[serde(default)]
    pub context: Option<serde_json::Value>,
}

impl RunMessageRequest {
    /// Resolve into the user `Message`, or a `BadRequest` if the body carried
    /// neither a non-empty `message` nor any `content`.
    fn into_message(self) -> Result<Message, ServeError> {
        match (self.content, self.message) {
            (Some(blocks), _) if !blocks.is_empty() => Ok(Message::user_with_blocks(blocks)),
            (_, Some(text)) if !text.trim().is_empty() => Ok(Message::user(text)),
            _ => Err(ServeError::BadRequest(
                "run request needs a non-empty `message` string or a non-empty `content` array"
                    .into(),
            )),
        }
    }
}

/// Body for `POST .../asks/:ask_id` — the operator's answer to an `ask_user`.
#[derive(Debug, Deserialize)]
pub struct AnswerRequest {
    pub answer: String,
}

#[derive(Debug, Serialize)]
struct StreamErrorEvent {
    error: String,
}

/// `POST /threads/:id/runs/stream`
///
/// Kicks off a streaming run in a detached task that locks the thread's Agent
/// for the whole turn (so concurrent POSTs on the same thread serialize), and
/// merges the agent's live `AgentEvent` stream with any HITL `ask_required`
/// prompts onto one SSE response. If the client disconnects, the response
/// stream is dropped; the run keeps going to completion in the task.
pub async fn create_and_stream_run(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(thread_id): Path<String>,
    Json(req): Json<RunMessageRequest>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ServeError> {
    // Extract context before `into_message` consumes the request, and validate
    // the body BEFORE building/locking anything (clean 400 vs half-open SSE).
    let ctx_json = req.context.clone().unwrap_or(serde_json::Value::Null);
    let user_msg = req.into_message()?;

    // App-resolved per-run context (provider override, identity keys, …).
    let mut run_ctx = state
        .pool
        .factory()
        .build_run_context(&tenant, &thread_id, &ctx_json)
        .await;

    // Live token channel (AgentEvent) + HITL ask channel (WireEvent), merged
    // into one SSE stream below. Both close when the run ends (the agent drops
    // its event sender and the human channel).
    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let (ask_tx, mut ask_rx) = mpsc::unbounded_channel::<WireEvent>();
    run_ctx = run_ctx
        .with_events(evt_tx)
        .with_human(Arc::new(HumanChannel::new(state.human_hub.clone(), ask_tx)));

    let agent_arc = state.pool.get_or_build(&tenant, &thread_id).await;
    tokio::spawn(async move {
        let mut agent = agent_arc.lock().await;
        let _ = agent.run_message_with(user_msg, run_ctx).await;
        // Guard drops → the next queued run on this thread proceeds. The agent
        // clears its event sender + human channel here, closing both rx ends.
    });

    let stream = stream! {
        let mut evt_open = true;
        let mut ask_open = true;
        while evt_open || ask_open {
            tokio::select! {
                evt = evt_rx.recv(), if evt_open => match evt {
                    Some(e) => {
                        for w in from_agent_event(e) {
                            yield Ok(to_sse(&w, None));
                        }
                    }
                    None => evt_open = false,
                },
                ask = ask_rx.recv(), if ask_open => match ask {
                    Some(w) => yield Ok(to_sse(&w, None)),
                    None => ask_open = false,
                },
            }
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
/// Emit persisted events for the run with seq > the `Last-Event-ID` header,
/// then attach to the agent's live broadcast if it's still warm — so a client
/// that dropped mid-run can reconnect and pick up where it left off.
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

    // Events for THIS run, internal-only kinds dropped.
    let filtered: Vec<(u64, WireEvent)> = stored
        .into_iter()
        .filter(|s| s.event.run_id() == run_id)
        .filter_map(|s| from_session_event(s.event).map(|w| (s.seq, w)))
        .collect();

    let agent_arc = state.pool.get_or_build(&tenant, &thread_id).await;

    let stream = stream! {
        // 1) replay the persisted events the client missed.
        for (seq, wire) in filtered {
            yield Ok(to_sse(&wire, Some(seq)));
        }

        // 2) attach to the live broadcast only if this run is still in flight.
        let rx = {
            let agent = agent_arc.lock().await;
            let is_live = agent
                .state()
                .current_run()
                .is_some_and(|run| run.id == run_id);
            if is_live {
                agent.state().subscribe_events()
            } else {
                None
            }
        };
        let Some(rx) = rx else {
            yield Ok(to_sse(&WireEvent::Done { total_turns: 0 }, None));
            return;
        };

        let mut live = BroadcastStream::new(rx);
        while let Some(received) = live.next().await {
            let Ok(event) = received else { continue }; // skip Lagged
            if event.run_id() != run_id {
                continue;
            }
            if let Some(wire) = from_session_event(event) {
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

/// `POST /threads/:id/runs/:run_id/asks/:ask_id`
///
/// Deliver an operator's answer to a parked `ask_user`. The parked tool wakes,
/// returns the answer into the conversation, and the run streams on.
pub async fn submit_answer(
    State(state): State<AppState>,
    Tenant(_tenant): Tenant,
    Path((_thread_id, _run_id, ask_id)): Path<(String, String, String)>,
    Json(body): Json<AnswerRequest>,
) -> Result<StatusCode, ServeError> {
    if state.human_hub.resolve(&ask_id, body.answer) {
        Ok(StatusCode::ACCEPTED)
    } else {
        Err(ServeError::BadRequest(format!(
            "no pending ask for ask_id '{ask_id}'"
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
        assert!(matches!(msg.role, runic_types::Role::User));
        assert!(msg.content.text_content().contains("hello"));
    }

    #[test]
    fn content_blocks_pass_through() {
        let req = parse(
            r#"{"content":[
                {"type":"text","text":"look at this"},
                {"type":"image","media_type":"image/png","data":"YWJj"}
            ]}"#,
        );
        let msg = req.into_message().unwrap();
        assert!(msg.content.text_content().contains("look at this"));
    }

    #[test]
    fn content_takes_precedence_over_message_when_both_present() {
        let req = parse(r#"{"message":"ignored","content":[{"type":"text","text":"win"}]}"#);
        let msg = req.into_message().unwrap();
        assert!(msg.content.text_content().contains("win"));
        assert!(!msg.content.text_content().contains("ignored"));
    }

    #[test]
    fn empty_content_array_falls_back_to_message_text() {
        let msg = parse(r#"{"message":"fallback","content":[]}"#)
            .into_message()
            .unwrap();
        assert!(msg.content.text_content().contains("fallback"));
    }

    #[test]
    fn neither_field_is_a_bad_request() {
        assert!(matches!(
            parse(r#"{}"#).into_message(),
            Err(ServeError::BadRequest(_))
        ));
        assert!(matches!(
            parse(r#"{"message":"   "}"#).into_message(),
            Err(ServeError::BadRequest(_))
        ));
        assert!(matches!(
            parse(r#"{"content":[]}"#).into_message(),
            Err(ServeError::BadRequest(_))
        ));
    }

    #[test]
    fn per_request_context_parses_when_present() {
        let req = parse(r#"{"message":"hi","context":{"user_id":"u1","allow_web_search":true}}"#);
        let ctx = req.context.expect("context present");
        assert_eq!(ctx["user_id"], "u1");
        assert_eq!(ctx["allow_web_search"], true);
    }

    #[test]
    fn context_is_none_when_absent() {
        assert!(parse(r#"{"message":"hi"}"#).context.is_none());
    }
}
