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
use axum::http::HeaderMap;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::Json;
use futures::stream::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::BroadcastStream;

use crate::app::AppState;
use crate::error::ServeError;
use crate::tenant::Tenant;
use crate::wire::{from_agent_event, from_session_event, WireEvent};

#[derive(Debug, Deserialize)]
pub struct RunMessageRequest {
    /// The user message body. Free-form text for now — multipart and
    /// multi-block content land in a future phase.
    pub message: String,
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
    let agent_arc = state.pool.get_or_build(&tenant, &thread_id).await;

    let stream = stream! {
        // Hold the mutex for the whole run. Concurrent POSTs on the
        // same thread queue here — intentional, since interleaved
        // turns would corrupt the conversation log.
        let mut slot = agent_arc.lock().await;

        // run_streaming consumes the Agent and returns it via the
        // JoinHandle. Take it out, run it, put it back.
        let agent = match slot.take() {
            Some(a) => a,
            None => {
                let body = WireEvent::Warning {
                    message: "agent slot was empty — concurrent take?".into(),
                };
                yield Ok(to_sse(&body, None));
                return;
            }
        };

        let (mut events, handle) = agent.run_streaming(&req.message);

        while let Some(event) = events.next().await {
            let wire = from_agent_event(event);
            yield Ok(to_sse(&wire, None));
        }

        // Pull the Agent back from the JoinHandle and park it in the
        // slot for the next turn.
        match handle.await {
            Ok((returned_agent, _outcome)) => {
                *slot = Some(returned_agent);
            }
            Err(join_err) => {
                let body = WireEvent::Warning {
                    message: format!("join error: {join_err}"),
                };
                yield Ok(to_sse(&body, None));
                // Slot stays None — next request will see the empty
                // and surface a clean error rather than a panic.
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
