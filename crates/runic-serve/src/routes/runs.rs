//! Run streaming.
//!
//! - `POST /threads/:id/runs/stream` — drive a fresh turn, stream events live.
//! - `GET  /threads/:id/runs/:run_id/stream` — replay a past run's persisted
//!   events and, if it's still in flight, attach to the live broadcast.
//! - `POST /threads/:id/asks/:ask_id` — answer a parked `ask_user` (HITL).
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

use base64::Engine;
use runic_agent::AgentEvent;
use runic_state::SessionEvent;
use runic_substrate::{ArtifactSource, RunStatus};
use runic_types::{ContentBlock, Message, MessageContent};

use crate::app::AppState;
use crate::error::{ErrorBody, ServeError};
use crate::human::HumanChannel;
use crate::routes::artifacts::MAX_ARTIFACT_BYTES;
use crate::tenant::Tenant;
use crate::wire::{WireEvent, from_agent_event, from_session_event};

/// The user turn for a run. Two shapes, checked in order:
///
///   {"message": "plain text"}                          // text shorthand
///   {"content": [{"type":"text","text":"..."},         // full content blocks
///                {"type":"image","media_type":"image/png","data":"<base64>"}]}
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct RunMessageRequest {
    /// Which registered agent runs this turn; defaults to `default`.
    #[serde(default)]
    pub agent: Option<String>,
    /// Plain-text shorthand for the user turn. Ignored when `content` is a
    /// non-empty array.
    #[serde(default)]
    pub message: Option<String>,
    /// Full content blocks (text / image / file / artifact_ref). Inline media is
    /// stored and replaced with a reference before it reaches the event log.
    #[serde(default)]
    #[schema(value_type = Option<Vec<Object>>)]
    pub content: Option<Vec<ContentBlock>>,
    /// Open per-request context, passed verbatim to the factory's
    /// `build_run_context` (e.g. `user_id`, provider override).
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
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

/// One block after validation, before any write.
enum Planned {
    /// Inline media to store (decoded, not yet written).
    Inline { media_type: String, bytes: Vec<u8> },
    /// A block to keep as-is (text, or an already-validated ref).
    Keep(ContentBlock),
}

/// Replace inline media with stored `ArtifactRef`s and validate any
/// client-supplied `ArtifactRef` against `(tenant, thread)` — so the event log
/// only ever receives references, regardless of what the client posted.
///
/// Validates *every* block (base64 decodes, ref ownership) before writing
/// anything, so a request that's ultimately rejected stores no orphan bytes.
async fn normalize_message(
    state: &AppState,
    tenant: &str,
    thread_id: &str,
    msg: Message,
) -> Result<Message, ServeError> {
    if !matches!(msg.content, MessageContent::Blocks(_)) {
        return Ok(msg);
    }
    let MessageContent::Blocks(blocks) = msg.content else {
        unreachable!()
    };

    let has_ref = blocks
        .iter()
        .any(|b| matches!(b, ContentBlock::ArtifactRef { .. }));
    let owned = if has_ref {
        state.artifact_store.list(tenant, thread_id).await?
    } else {
        Vec::new()
    };

    // Pass 1 — validate everything, write nothing.
    let mut plan = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            ContentBlock::Image { media_type, data } | ContentBlock::File { media_type, data } => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data.as_bytes())
                    .map_err(|_| {
                        ServeError::BadRequest("invalid base64 in content block".into())
                    })?;
                if bytes.len() > MAX_ARTIFACT_BYTES {
                    return Err(ServeError::BadRequest(
                        "inline media exceeds size limit".into(),
                    ));
                }
                plan.push(Planned::Inline { media_type, bytes });
            }
            ContentBlock::ArtifactRef { id, filename, .. } => {
                let Some(art) = owned.iter().find(|a| a.id == id) else {
                    return Err(ServeError::BadRequest(
                        "artifact_ref does not belong to this thread".into(),
                    ));
                };
                // Persist the canonical stored MIME, not the client's claim.
                plan.push(Planned::Keep(ContentBlock::ArtifactRef {
                    media_type: art.mime_type.clone(),
                    id,
                    filename,
                }));
            }
            other => plan.push(Planned::Keep(other)),
        }
    }

    // Pass 2 — request is fully valid; now store inline media.
    let mut out = Vec::with_capacity(plan.len());
    for planned in plan {
        match planned {
            Planned::Keep(block) => out.push(block),
            Planned::Inline { media_type, bytes } => {
                let art = state
                    .artifact_store
                    .put(
                        tenant,
                        thread_id,
                        &media_type,
                        ArtifactSource::UserUpload,
                        &bytes,
                    )
                    .await?;
                out.push(ContentBlock::ArtifactRef {
                    id: art.id,
                    media_type,
                    filename: None,
                });
            }
        }
    }
    Ok(Message::user_with_blocks(out))
}

/// Body for `POST .../asks/:ask_id` — the operator's answer to an `ask_user`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct AnswerRequest {
    pub answer: String,
}

#[derive(Debug, Serialize)]
struct StreamErrorEvent {
    error: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct WaitRunResponse {
    pub run_id: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    pub total_turns: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub structured: Option<serde_json::Value>,
}

/// `POST /threads/:id/runs/stream`
///
/// Kicks off a streaming run in a detached task that locks the thread's Agent
/// for the whole turn (so concurrent POSTs on the same thread serialize), and
/// merges the agent's live `AgentEvent` stream with any HITL `ask_required`
/// prompts onto one SSE response. If the client disconnects, the response
/// stream is dropped; the run keeps going to completion in the task.
#[utoipa::path(
    post,
    path = "/threads/{thread_id}/runs/stream",
    tag = "runs",
    request_body = RunMessageRequest,
    params(
        ("thread_id" = String, Path, description = "Thread id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200,
         description = "SSE stream (`text/event-stream`). Event names: run_start, \
            assistant_text_delta, assistant_thinking_delta, tool_start, tool_finish, \
            turn_complete, usage, ask_required, escalated, warning, run_error, hook_fired, \
            done. A provider failure emits `run_error` then `done`.",
         content_type = "text/event-stream", body = WireEvent),
        (status = 400, description = "Invalid body or artifact reference", body = ErrorBody),
        (status = 429, description = "This instance is at its concurrent run limit", body = ErrorBody)
    )
)]
pub async fn create_and_stream_run(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(thread_id): Path<String>,
    Json(req): Json<RunMessageRequest>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ServeError> {
    // Extract context before `into_message` consumes the request, and validate
    // the body BEFORE building/locking anything (clean 400 vs half-open SSE).
    let agent_name = state.agents.resolve_agent(req.agent.as_deref())?;
    state
        .runs
        .check_persist_capacity(&tenant, &thread_id)
        .await?;
    let ctx_json = req.context.clone().unwrap_or(serde_json::Value::Null);
    let user_msg = req.into_message()?;
    // Inline media → stored refs; client refs validated. State only sees refs.
    let user_msg = normalize_message(&state, &tenant, &thread_id, user_msg).await?;

    // App-resolved per-run context (provider override, identity keys, …).
    let mut run_ctx = state
        .agents
        .factory(&agent_name)?
        .build_run_context(&tenant, &thread_id, &ctx_json)
        .await;

    // Live token channel (AgentEvent) + HITL ask channel (WireEvent), merged
    // into one SSE stream below. Both close when the run ends (the agent drops
    // its event sender and the human channel).
    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let (ask_tx, mut ask_rx) = mpsc::unbounded_channel::<WireEvent>();
    // Clone the wire sender so the run task can report a failure on the same
    // channel the HITL asks use (no third channel needed).
    let err_tx = ask_tx.clone();

    let run_id = runic_state::new_run_id();
    state
        .session_store
        .create_run(&tenant, &thread_id, &run_id, &agent_name)
        .await?;
    let mut begun = state.runs.begin(&tenant, &thread_id, &run_id).await?;
    let persist = begun.persist.clone();
    let steering_rx = std::mem::replace(&mut begun.steering_rx, mpsc::unbounded_channel().1);
    run_ctx = run_ctx
        .with_events(evt_tx)
        .with_cancel(begun.cancel.clone())
        .with_steering(steering_rx)
        .with_agent(&agent_name)
        .with_run_id(&run_id)
        .with_human(Arc::new(HumanChannel::new(
            state.human_hub.clone(),
            ask_tx,
            tenant.clone(),
            thread_id.clone(),
        )));

    tracing::info!(%tenant, %thread_id, agent = %agent_name, %run_id, "run stream accepted");

    let registry = state.runs.clone();
    let store = state.session_store.clone();
    let factory = state.agents.factory(&agent_name)?.clone();
    tokio::spawn(async move {
        let lock = registry.thread_lock(&tenant, &thread_id).await;
        let _guard = lock.lock().await;
        let claim =
            crate::registry::claim_lease(&store, &registry, &run_id, begun.cancel.clone()).await;
        if matches!(claim, crate::registry::Claim::Lost) {
            tracing::warn!(%tenant, %thread_id, %run_id, "run already claimed elsewhere");
            let _ = err_tx.send(WireEvent::RunError {
                run_id: Some(run_id.clone()),
                message: "run was claimed by another instance".into(),
            });
            registry
                .end(&tenant, &thread_id, &run_id, begun.persist.clone())
                .await;
            return;
        }
        let mut agent =
            crate::registry::hydrate_agent(&store, &factory, &tenant, &thread_id, &mut begun).await;
        let outcome = agent.run_message_with(user_msg, run_ctx).await;
        claim.release();
        let (status, error) = match &outcome {
            Ok(o) if o.stop_reason.as_deref() == Some("cancelled") => (RunStatus::Cancelled, None),
            Ok(_) => (RunStatus::Success, None),
            Err(e) => (RunStatus::Error, Some(e.to_string())),
        };
        if let Err(e) = store
            .set_run_status(&run_id, status, error.as_deref())
            .await
        {
            tracing::warn!(%tenant, %thread_id, %run_id, error = %e, "run row update failed");
        }
        if let Err(e) = outcome {
            tracing::error!(%tenant, %thread_id, error = %e, "run task failed");
            let _ = err_tx.send(WireEvent::RunError {
                run_id: Some(run_id.clone()),
                message: e.to_string(),
            });
        }
        registry
            .end(&tenant, &thread_id, &run_id, begun.persist.clone())
            .await;
        // Guard drops → the next queued run on this thread proceeds. The agent
        // clears its event sender + human channel here, closing both rx ends.
    });

    let stream = stream! {
        let mut evt_open = true;
        let mut ask_open = true;
        let mut done_sent = false;
        let mut pending_error: Option<WireEvent> = None;
        while evt_open || ask_open {
            tokio::select! {
                evt = evt_rx.recv(), if evt_open => match evt {
                    Some(e) => {
                        for w in from_agent_event(e) {
                            if matches!(w, WireEvent::Done { .. }) {
                                done_sent = true;
                                flush_persist(&persist).await;
                            }
                            yield Ok(to_sse(&w, None));
                        }
                    }
                    None => evt_open = false,
                },
                ask = ask_rx.recv(), if ask_open => match ask {
                    Some(w @ WireEvent::RunError { .. }) => pending_error = Some(w),
                    Some(w) => yield Ok(to_sse(&w, None)),
                    None => ask_open = false,
                },
            }
        }
        if let Some(err) = pending_error {
            yield Ok(to_sse(&err, None));
        }
        if !done_sent {
            flush_persist(&persist).await;
            yield Ok(to_sse(
                &WireEvent::Done {
                    total_turns: None,
                    stop_reason: None,
                },
                None,
            ));
        }
    };

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text(":keepalive"),
    ))
}

/// `POST /threads/:id/runs/wait`
///
/// Run a turn to completion and return the final answer as one JSON body — no
/// SSE. The run executes in a detached task (same as the streaming route), so
/// a client disconnect never aborts it mid-turn. No human channel is wired:
/// an `ask_user` raised mid-run fails in-band and the run continues.
#[utoipa::path(
    post,
    path = "/threads/{thread_id}/runs/wait",
    tag = "runs",
    request_body = RunMessageRequest,
    params(
        ("thread_id" = String, Path, description = "Thread id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "The completed run", body = WaitRunResponse),
        (status = 400, description = "Invalid body or artifact reference", body = ErrorBody),
        (status = 429, description = "This instance is at its concurrent run limit", body = ErrorBody),
        (status = 500, description = "The run failed (provider error, max turns, ...)", body = ErrorBody)
    )
)]
pub async fn wait_run(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(thread_id): Path<String>,
    Json(req): Json<RunMessageRequest>,
) -> Result<Json<WaitRunResponse>, ServeError> {
    let agent_name = state.agents.resolve_agent(req.agent.as_deref())?;
    state
        .runs
        .check_persist_capacity(&tenant, &thread_id)
        .await?;
    let ctx_json = req.context.clone().unwrap_or(serde_json::Value::Null);
    let user_msg = req.into_message()?;
    let user_msg = normalize_message(&state, &tenant, &thread_id, user_msg).await?;

    let mut run_ctx = state
        .agents
        .factory(&agent_name)?
        .build_run_context(&tenant, &thread_id, &ctx_json)
        .await;

    let run_id = runic_state::new_run_id();
    state
        .session_store
        .create_run(&tenant, &thread_id, &run_id, &agent_name)
        .await?;
    let mut begun = state.runs.begin(&tenant, &thread_id, &run_id).await?;
    let steering_rx = std::mem::replace(&mut begun.steering_rx, mpsc::unbounded_channel().1);
    run_ctx = run_ctx
        .with_cancel(begun.cancel.clone())
        .with_steering(steering_rx)
        .with_agent(&agent_name)
        .with_run_id(&run_id);

    tracing::info!(%tenant, %thread_id, agent = %agent_name, %run_id, "wait run accepted");

    let registry = state.runs.clone();
    let store = state.session_store.clone();
    let factory = state.agents.factory(&agent_name)?.clone();
    let task = tokio::spawn(async move {
        let lock = registry.thread_lock(&tenant, &thread_id).await;
        let _guard = lock.lock().await;
        let claim =
            crate::registry::claim_lease(&store, &registry, &run_id, begun.cancel.clone()).await;
        if matches!(claim, crate::registry::Claim::Lost) {
            tracing::warn!(%tenant, %thread_id, %run_id, "run already claimed elsewhere");
            registry
                .end(&tenant, &thread_id, &run_id, begun.persist.clone())
                .await;
            return Err("run was claimed by another instance".to_string());
        }
        let mut agent =
            crate::registry::hydrate_agent(&store, &factory, &tenant, &thread_id, &mut begun).await;
        let result = agent.run_message_with(user_msg, run_ctx).await;
        claim.release();
        let (status, error) = match &result {
            Ok(o) if o.stop_reason.as_deref() == Some("cancelled") => (RunStatus::Cancelled, None),
            Ok(_) => (RunStatus::Success, None),
            Err(e) => (RunStatus::Error, Some(e.to_string())),
        };
        if let Err(e) = store
            .set_run_status(&run_id, status, error.as_deref())
            .await
        {
            tracing::warn!(%tenant, %thread_id, %run_id, error = %e, "run row update failed");
        }
        flush_persist(&begun.persist).await;
        registry
            .end(&tenant, &thread_id, &run_id, begun.persist.clone())
            .await;
        match result {
            Ok(outcome) => {
                let text = agent.state().last_assistant_text().unwrap_or_default();
                Ok(WaitRunResponse {
                    run_id,
                    text,
                    stop_reason: outcome.stop_reason,
                    total_turns: outcome.total_turns,
                    input_tokens: outcome.usage.input_tokens,
                    output_tokens: outcome.usage.output_tokens,
                    structured: outcome.structured,
                })
            }
            Err(e) => Err(e.to_string()),
        }
    });

    match task.await {
        Ok(Ok(response)) => Ok(Json(response)),
        Ok(Err(e)) => Err(ServeError::Agent(e)),
        Err(e) => Err(ServeError::Internal(format!("run task panicked: {e}"))),
    }
}

const FLUSH_TIMEOUT: Duration = Duration::from_secs(15);

async fn flush_persist(persist: &crate::registry::PersistHandle) {
    if tokio::time::timeout(FLUSH_TIMEOUT, persist.flush())
        .await
        .is_err()
    {
        tracing::error!(
            backlog = persist.backlog(),
            "run finished but events are still unflushed after {FLUSH_TIMEOUT:?}"
        );
    }
}

/// `POST /threads/:id/runs/cancel`
///
/// Requests cancellation of the thread's in-flight run, if any. The run
/// finishes its current turn gracefully rather than stopping immediately —
/// see [`runic_agent::CancelToken`].
#[utoipa::path(
    post,
    path = "/threads/{thread_id}/runs/cancel",
    tag = "runs",
    params(
        ("thread_id" = String, Path, description = "Thread id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 202, description = "Cancellation requested"),
        (status = 409, description = "No run in flight on this thread", body = ErrorBody)
    )
)]
pub async fn cancel_run(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(thread_id): Path<String>,
) -> Result<StatusCode, ServeError> {
    if state.runs.cancel_run(&tenant, &thread_id).await {
        Ok(StatusCode::ACCEPTED)
    } else {
        Err(ServeError::NoRunInFlight { thread_id })
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct SteerRequest {
    pub text: String,
}

/// `POST /threads/:id/runs/steer`
///
/// Injects `text` as a user message into the thread's in-flight run at its
/// next turn boundary — a mid-run nudge, not a new run.
#[utoipa::path(
    post,
    path = "/threads/{thread_id}/runs/steer",
    tag = "runs",
    request_body = SteerRequest,
    params(
        ("thread_id" = String, Path, description = "Thread id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 202, description = "Steering text queued for the next turn"),
        (status = 400, description = "Empty text", body = ErrorBody),
        (status = 409, description = "No run in flight on this thread", body = ErrorBody)
    )
)]
pub async fn steer_run(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(thread_id): Path<String>,
    Json(req): Json<SteerRequest>,
) -> Result<StatusCode, ServeError> {
    if req.text.trim().is_empty() {
        return Err(ServeError::BadRequest(
            "steer requires non-empty text".into(),
        ));
    }
    if state.runs.steer_run(&tenant, &thread_id, req.text).await {
        Ok(StatusCode::ACCEPTED)
    } else {
        Err(ServeError::NoRunInFlight { thread_id })
    }
}

/// `GET /threads/:id/runs/:run_id/stream`
///
/// Emit persisted events for the run with seq > the `Last-Event-ID` header,
/// then attach to the agent's live broadcast if it's still warm — so a client
/// that dropped mid-run can reconnect and pick up where it left off.
#[utoipa::path(
    get,
    path = "/threads/{thread_id}/runs/{run_id}/stream",
    tag = "runs",
    params(
        ("thread_id" = String, Path, description = "Thread id"),
        ("run_id" = String, Path, description = "Run id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`"),
        ("Last-Event-ID" = Option<u64>, Header, description = "Resume: replay only events with seq greater than this")
    ),
    responses(
        (status = 200,
         description = "SSE replay (`text/event-stream`) of persisted events after \
            `Last-Event-ID`, then the live tail if still in flight, ending with `done`.",
         content_type = "text/event-stream", body = WireEvent),
        (status = 404, description = "Unknown thread or run", body = ErrorBody)
    )
)]
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

    // Thread must exist.
    if state
        .session_store
        .session_meta(&tenant, &thread_id)
        .await?
        .is_none()
    {
        return Err(ServeError::ThreadNotFound { id: thread_id });
    }

    // All persisted events for this run (from seq 0) — for existence + the real
    // terminal turn count; the replay payload is the slice after `after_seq`.
    let all = state
        .session_store
        .read_run_after(&tenant, &thread_id, &run_id, 0)
        .await?;

    let live_rx = state.runs.live_events(&tenant, &thread_id, &run_id).await;
    let is_live = live_rx.is_some();

    if all.is_empty() && !is_live {
        return Err(ServeError::RunNotFound {
            id: run_id,
            thread: thread_id,
        });
    }

    tracing::info!(%tenant, %thread_id, %run_id, after_seq, live = is_live, "replay attached");

    let completed = all.iter().rev().find_map(|s| match &s.event {
        SessionEvent::RunEnd { outcome, .. } => {
            Some((outcome.total_turns, outcome.stop_reason.clone()))
        }
        _ => None,
    });

    let replay: Vec<(u64, WireEvent)> = all
        .into_iter()
        .filter(|s| s.seq > after_seq)
        .filter_map(|s| from_session_event(s.event).map(|w| (s.seq, w)))
        .collect();

    let stream = stream! {
        // 1) replay the persisted events the client missed.
        for (seq, wire) in replay {
            yield Ok(to_sse(&wire, Some(seq)));
        }

        // 2) attach to the live broadcast only if still in flight, following
        // until this run's RunEnd (capturing its real turn count).
        let rx = live_rx;
        let (mut total_turns, mut stop_reason) = match completed {
            Some((t, s)) => (Some(t), s),
            None => (None, None),
        };
        if let Some(rx) = rx {
            let mut live = BroadcastStream::new(rx);
            while let Some(received) = live.next().await {
                let Ok(event) = received else { continue }; // skip Lagged
                if event.run_id() != run_id {
                    continue;
                }
                let end = match event.as_ref() {
                    SessionEvent::RunEnd { outcome, .. } => {
                        Some((outcome.total_turns, outcome.stop_reason.clone()))
                    }
                    _ => None,
                };
                if let Some(wire) = from_session_event((*event).clone()) {
                    yield Ok(to_sse(&wire, None));
                }
                if let Some((t, s)) = end {
                    total_turns = Some(t);
                    stop_reason = s;
                    break;
                }
            }
        }

        yield Ok(to_sse(
            &WireEvent::Done {
                total_turns,
                stop_reason,
            },
            None,
        ));
    };

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text(":keepalive"),
    ))
}

/// `POST /threads/:id/asks/:ask_id`
///
/// Deliver an operator's answer to a parked `ask_user`. The parked tool wakes,
/// returns the answer into the conversation, and the run streams on.
#[utoipa::path(
    post,
    path = "/threads/{thread_id}/asks/{ask_id}",
    tag = "runs",
    request_body = AnswerRequest,
    params(
        ("thread_id" = String, Path, description = "Thread id"),
        ("ask_id" = String, Path, description = "Ask id from the `ask_required` event"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 202, description = "Answer delivered; the parked run resumes"),
        (status = 400, description = "No pending ask for this (tenant, thread, ask_id)", body = ErrorBody)
    )
)]
pub async fn submit_answer(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path((thread_id, ask_id)): Path<(String, String)>,
    Json(body): Json<AnswerRequest>,
) -> Result<StatusCode, ServeError> {
    resolve_answer(state, tenant, thread_id, ask_id, body.answer)
}

/// Legacy alias for clients still posting through the old run-shaped path.
#[utoipa::path(
    post,
    path = "/threads/{thread_id}/runs/{run_id}/asks/{ask_id}",
    tag = "runs",
    request_body = AnswerRequest,
    params(
        ("thread_id" = String, Path, description = "Thread id"),
        ("run_id" = String, Path, description = "Run id (ignored; kept for the legacy path shape)"),
        ("ask_id" = String, Path, description = "Ask id from the `ask_required` event"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 202, description = "Answer delivered; the parked run resumes"),
        (status = 400, description = "No pending ask for this (tenant, thread, ask_id)", body = ErrorBody)
    )
)]
pub async fn submit_answer_legacy(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path((thread_id, _run_id, ask_id)): Path<(String, String, String)>,
    Json(body): Json<AnswerRequest>,
) -> Result<StatusCode, ServeError> {
    resolve_answer(state, tenant, thread_id, ask_id, body.answer)
}

fn resolve_answer(
    state: AppState,
    tenant: String,
    thread_id: String,
    ask_id: String,
    answer: String,
) -> Result<StatusCode, ServeError> {
    if state
        .human_hub
        .resolve(&tenant, &thread_id, &ask_id, answer)
    {
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
