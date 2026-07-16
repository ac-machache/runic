//! Run streaming.
//!
//! - `POST /threads/:id/runs/stream` — drive a fresh turn, stream events live.
//! - `GET  /threads/:id/runs/:run_id/stream` — replay a past run's persisted
//!   events and, if it's still in flight, attach to the live broadcast.
//! - `POST /threads/:id/asks/:ask_id` — deliver an answer to a deferred
//!   `ask_user`; the suspended run resumes.
//!
//! The wire format is in [`crate::wire`]. Each SSE event carries the
//! `WireEvent` JSON body, the matching `event:` field, and (for replay) the
//! `id:` field from the store's seq — that's what `Last-Event-ID` resumes on.

use std::convert::Infallible;
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

    // Live token channel (AgentEvent) + a side channel for the run task to
    // report a terminal failure, merged into one SSE stream below.
    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let (err_tx, mut ask_rx) = mpsc::unbounded_channel::<WireEvent>();

    let run_id = runic_state::new_run_id();
    let run_input = runic_substrate::RunInput {
        input: serde_json::to_value(&user_msg).ok(),
        context: (!ctx_json.is_null()).then(|| ctx_json.clone()),
        queued: false,
    };
    state
        .session_store
        .create_run(&tenant, &thread_id, &run_id, &agent_name, &run_input)
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
        .with_mode("stream");
    if !state.agents.factory(&agent_name)?.stateless() {
        run_ctx = run_ctx.with_child_persistence(crate::child::child_persistence(
            state.session_store.clone(),
            &tenant,
            &thread_id,
        ));
    }

    tracing::info!(%tenant, %thread_id, agent = %agent_name, %run_id, "run stream accepted");

    let registry = state.runs.clone();
    let store = state.session_store.clone();
    let factory = state.agents.factory(&agent_name)?.clone();
    tokio::spawn(async move {
        let lock = registry.thread_lock(&tenant, &thread_id).await;
        let _guard = lock.lock().await;
        if !crate::registry::acquire_thread_lease(
            &store,
            &registry,
            &tenant,
            &thread_id,
            &begun.cancel,
        )
        .await
        {
            let _ = store
                .set_run_status(&run_id, RunStatus::Cancelled, None)
                .await;
            registry
                .end(&tenant, &thread_id, &run_id, begun.persist.clone())
                .await;
            return;
        }
        let claim = crate::registry::claim_lease(
            &store,
            &registry,
            crate::registry::HeartbeatRun {
                tenant: tenant.clone(),
                thread_id: thread_id.clone(),
                run_id: run_id.clone(),
                cancel: begun.cancel.clone(),
                steering: begun.steering_tx.clone(),
            },
        )
        .await;
        if matches!(claim, crate::registry::Claim::Lost) {
            tracing::warn!(%tenant, %thread_id, %run_id, "run already claimed elsewhere");
            let _ = err_tx.send(WireEvent::RunError {
                run_id: Some(run_id.clone()),
                message: "run was claimed by another instance".into(),
            });
            registry
                .end(&tenant, &thread_id, &run_id, begun.persist.clone())
                .await;
            crate::registry::release_thread_lease(&store, &registry, &tenant, &thread_id).await;
            return;
        }
        let outcome =
            match crate::registry::hydrate_agent(&store, &factory, &tenant, &thread_id, &mut begun)
                .await
            {
                Ok(mut agent) => agent.run_message_with(user_msg, run_ctx).await,
                Err(e) => Err(runic_agent::AgentError::Build(e.to_string())),
            };
        claim.release();
        let (status, error) = match &outcome {
            Ok(o) if o.stop_reason.as_deref() == Some("cancelled") => (RunStatus::Cancelled, None),
            Ok(o) if o.stop_reason.as_deref() == Some("suspended") => (RunStatus::Paused, None),
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
        crate::registry::release_thread_lease(&store, &registry, &tenant, &thread_id).await;
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
    let run_input = runic_substrate::RunInput {
        input: serde_json::to_value(&user_msg).ok(),
        context: (!ctx_json.is_null()).then(|| ctx_json.clone()),
        queued: false,
    };
    state
        .session_store
        .create_run(&tenant, &thread_id, &run_id, &agent_name, &run_input)
        .await?;
    let mut begun = state.runs.begin(&tenant, &thread_id, &run_id).await?;
    let steering_rx = std::mem::replace(&mut begun.steering_rx, mpsc::unbounded_channel().1);
    run_ctx = run_ctx
        .with_cancel(begun.cancel.clone())
        .with_steering(steering_rx)
        .with_agent(&agent_name)
        .with_run_id(&run_id)
        .with_mode("wait");
    if !state.agents.factory(&agent_name)?.stateless() {
        run_ctx = run_ctx.with_child_persistence(crate::child::child_persistence(
            state.session_store.clone(),
            &tenant,
            &thread_id,
        ));
    }

    tracing::info!(%tenant, %thread_id, agent = %agent_name, %run_id, "wait run accepted");

    let registry = state.runs.clone();
    let store = state.session_store.clone();
    let factory = state.agents.factory(&agent_name)?.clone();
    let task = tokio::spawn(async move {
        let lock = registry.thread_lock(&tenant, &thread_id).await;
        let _guard = lock.lock().await;
        if !crate::registry::acquire_thread_lease(
            &store,
            &registry,
            &tenant,
            &thread_id,
            &begun.cancel,
        )
        .await
        {
            let _ = store
                .set_run_status(&run_id, RunStatus::Cancelled, None)
                .await;
            registry
                .end(&tenant, &thread_id, &run_id, begun.persist.clone())
                .await;
            return Err("run cancelled before it started".to_string());
        }
        let claim = crate::registry::claim_lease(
            &store,
            &registry,
            crate::registry::HeartbeatRun {
                tenant: tenant.clone(),
                thread_id: thread_id.clone(),
                run_id: run_id.clone(),
                cancel: begun.cancel.clone(),
                steering: begun.steering_tx.clone(),
            },
        )
        .await;
        if matches!(claim, crate::registry::Claim::Lost) {
            tracing::warn!(%tenant, %thread_id, %run_id, "run already claimed elsewhere");
            registry
                .end(&tenant, &thread_id, &run_id, begun.persist.clone())
                .await;
            crate::registry::release_thread_lease(&store, &registry, &tenant, &thread_id).await;
            return Err("run was claimed by another instance".to_string());
        }
        let (agent, result) =
            match crate::registry::hydrate_agent(&store, &factory, &tenant, &thread_id, &mut begun)
                .await
            {
                Ok(mut agent) => {
                    let result = agent.run_message_with(user_msg, run_ctx).await;
                    (Some(agent), result)
                }
                Err(e) => (None, Err(runic_agent::AgentError::Build(e.to_string()))),
            };
        claim.release();
        let (status, error) = match &result {
            Ok(o) if o.stop_reason.as_deref() == Some("cancelled") => (RunStatus::Cancelled, None),
            Ok(o) if o.stop_reason.as_deref() == Some("suspended") => (RunStatus::Paused, None),
            Ok(_) => (RunStatus::Success, None),
            Err(e) => (RunStatus::Error, Some(e.to_string())),
        };
        flush_persist(&begun.persist).await;
        if let Err(e) = store
            .set_run_status(&run_id, status, error.as_deref())
            .await
        {
            tracing::warn!(%tenant, %thread_id, %run_id, error = %e, "run row update failed");
        }
        registry
            .end(&tenant, &thread_id, &run_id, begun.persist.clone())
            .await;
        crate::registry::release_thread_lease(&store, &registry, &tenant, &thread_id).await;
        match result {
            Ok(outcome) => {
                let text = agent
                    .as_ref()
                    .and_then(|agent| agent.state().last_assistant_text())
                    .unwrap_or_default();
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

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BackgroundRunResponse {
    pub run_id: String,
    pub status: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RunStatusResponse {
    pub run_id: String,
    pub agent: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// `POST /threads/{thread_id}/runs`
///
/// Fire-and-forget: accept the turn, return `202` with the `run_id`
/// immediately, and execute detached. Attach to the live/replayed events via
/// `GET .../runs/{run_id}/stream` or poll `GET .../runs/{run_id}`.
#[utoipa::path(
    post,
    path = "/threads/{thread_id}/runs",
    tag = "runs",
    request_body = RunMessageRequest,
    params(
        ("thread_id" = String, Path, description = "Thread id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 202, description = "Run accepted and executing in the background", body = BackgroundRunResponse),
        (status = 400, description = "Invalid body or artifact reference", body = ErrorBody),
        (status = 429, description = "This instance is at its concurrent run limit", body = ErrorBody)
    )
)]
pub async fn background_run(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(thread_id): Path<String>,
    Json(req): Json<RunMessageRequest>,
) -> Result<(StatusCode, Json<BackgroundRunResponse>), ServeError> {
    let agent_name = state.agents.resolve_agent(req.agent.as_deref())?;
    state
        .runs
        .check_persist_capacity(&tenant, &thread_id)
        .await?;
    let ctx_json = req.context.clone().unwrap_or(serde_json::Value::Null);
    let user_msg = req.into_message()?;
    let user_msg = normalize_message(&state, &tenant, &thread_id, user_msg).await?;

    let run_id = runic_state::new_run_id();
    let run_input = runic_substrate::RunInput {
        input: serde_json::to_value(&user_msg).ok(),
        context: (!ctx_json.is_null()).then(|| ctx_json.clone()),
        queued: state.queue_runs,
    };
    state
        .session_store
        .create_run(&tenant, &thread_id, &run_id, &agent_name, &run_input)
        .await?;

    if state.queue_runs {
        if let Some(nudge) = &state.nudge {
            nudge.nudge().await;
        }
        tracing::info!(%tenant, %thread_id, agent = %agent_name, %run_id, "background run queued");
        return Ok((
            StatusCode::ACCEPTED,
            Json(BackgroundRunResponse {
                run_id,
                status: RunStatus::Queued.as_str().to_string(),
            }),
        ));
    }

    let mut run_ctx = state
        .agents
        .factory(&agent_name)?
        .build_run_context(&tenant, &thread_id, &ctx_json)
        .await;
    let mut begun = state.runs.begin(&tenant, &thread_id, &run_id).await?;
    let steering_rx = std::mem::replace(&mut begun.steering_rx, mpsc::unbounded_channel().1);
    run_ctx = run_ctx
        .with_cancel(begun.cancel.clone())
        .with_steering(steering_rx)
        .with_agent(&agent_name)
        .with_run_id(&run_id)
        .with_mode("background");
    if !state.agents.factory(&agent_name)?.stateless() {
        run_ctx = run_ctx.with_child_persistence(crate::child::child_persistence(
            state.session_store.clone(),
            &tenant,
            &thread_id,
        ));
    }

    tracing::info!(%tenant, %thread_id, agent = %agent_name, %run_id, "background run accepted");

    let registry = state.runs.clone();
    let store = state.session_store.clone();
    let factory = state.agents.factory(&agent_name)?.clone();
    let response_run_id = run_id.clone();
    tokio::spawn(async move {
        let lock = registry.thread_lock(&tenant, &thread_id).await;
        let _guard = lock.lock().await;
        if !crate::registry::acquire_thread_lease(
            &store,
            &registry,
            &tenant,
            &thread_id,
            &begun.cancel,
        )
        .await
        {
            let _ = store
                .set_run_status(&run_id, RunStatus::Cancelled, None)
                .await;
            registry
                .end(&tenant, &thread_id, &run_id, begun.persist.clone())
                .await;
            return;
        }
        let claim = crate::registry::claim_lease(
            &store,
            &registry,
            crate::registry::HeartbeatRun {
                tenant: tenant.clone(),
                thread_id: thread_id.clone(),
                run_id: run_id.clone(),
                cancel: begun.cancel.clone(),
                steering: begun.steering_tx.clone(),
            },
        )
        .await;
        if matches!(claim, crate::registry::Claim::Lost) {
            tracing::warn!(%tenant, %thread_id, %run_id, "run already claimed elsewhere");
            registry
                .end(&tenant, &thread_id, &run_id, begun.persist.clone())
                .await;
            crate::registry::release_thread_lease(&store, &registry, &tenant, &thread_id).await;
            return;
        }
        let outcome =
            match crate::registry::hydrate_agent(&store, &factory, &tenant, &thread_id, &mut begun)
                .await
            {
                Ok(mut agent) => agent.run_message_with(user_msg, run_ctx).await,
                Err(e) => Err(runic_agent::AgentError::Build(e.to_string())),
            };
        claim.release();
        let (status, error) = match &outcome {
            Ok(o) if o.stop_reason.as_deref() == Some("cancelled") => (RunStatus::Cancelled, None),
            Ok(o) if o.stop_reason.as_deref() == Some("suspended") => (RunStatus::Paused, None),
            Ok(_) => (RunStatus::Success, None),
            Err(e) => (RunStatus::Error, Some(e.to_string())),
        };
        if let Err(e) = &outcome {
            tracing::error!(%tenant, %thread_id, %run_id, error = %e, "background run failed");
        }
        flush_persist(&begun.persist).await;
        if let Err(e) = store
            .set_run_status(&run_id, status, error.as_deref())
            .await
        {
            tracing::warn!(%tenant, %thread_id, %run_id, error = %e, "run row update failed");
        }
        registry
            .end(&tenant, &thread_id, &run_id, begun.persist.clone())
            .await;
        crate::registry::release_thread_lease(&store, &registry, &tenant, &thread_id).await;
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(BackgroundRunResponse {
            run_id: response_run_id,
            status: RunStatus::Pending.as_str().to_string(),
        }),
    ))
}

/// `GET /threads/{thread_id}/runs/{run_id}`
///
/// The run row: status (`pending`/`running`/`success`/`error`/`cancelled`),
/// error detail, and timestamps — the polling counterpart to the SSE attach.
#[utoipa::path(
    get,
    path = "/threads/{thread_id}/runs/{run_id}",
    tag = "runs",
    params(
        ("thread_id" = String, Path, description = "Thread id"),
        ("run_id" = String, Path, description = "Run id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "The run row", body = RunStatusResponse),
        (status = 404, description = "Unknown run", body = ErrorBody)
    )
)]
pub async fn run_status(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path((thread_id, run_id)): Path<(String, String)>,
) -> Result<Json<RunStatusResponse>, ServeError> {
    let record = state
        .session_store
        .get_run(&tenant, &run_id)
        .await?
        .filter(|r| r.session_id == thread_id)
        .ok_or(ServeError::RunNotFound {
            id: run_id,
            thread: thread_id,
        })?;
    Ok(Json(RunStatusResponse {
        run_id: record.run_id,
        agent: record.agent,
        status: record.status.as_str().to_string(),
        error: record.error,
        created_at: record.created_at,
        updated_at: record.updated_at,
    }))
}

#[derive(Debug, serde::Deserialize, utoipa::IntoParams)]
pub struct RunListQuery {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

fn decode_run_cursor(cursor: &str) -> Option<(chrono::DateTime<chrono::Utc>, String)> {
    let (at, run_id) = cursor.split_once('|')?;
    let at = chrono::DateTime::parse_from_rfc3339(at).ok()?;
    Some((at.with_timezone(&chrono::Utc), run_id.to_string()))
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RunSummary {
    pub run_id: String,
    pub agent: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RunListResponse {
    pub runs: Vec<RunSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/threads/{thread_id}/runs",
    tag = "runs",
    params(
        ("thread_id" = String, Path, description = "Thread id"),
        RunListQuery,
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "Run summaries, newest first, keyset-paginated via `before`", body = RunListResponse)
    )
)]
pub async fn list_thread_runs(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(thread_id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<RunListQuery>,
) -> Result<Json<RunListResponse>, ServeError> {
    let limit = query.limit.unwrap_or(20).min(100);
    let before = query.cursor.as_deref().and_then(decode_run_cursor);
    let records = state
        .session_store
        .list_runs(&tenant, &thread_id, limit, before)
        .await?;
    let next_cursor = (records.len() == limit)
        .then(|| {
            records
                .last()
                .map(|r| format!("{}|{}", r.created_at.to_rfc3339(), r.run_id))
        })
        .flatten();
    Ok(Json(RunListResponse {
        runs: records
            .into_iter()
            .map(|r| RunSummary {
                run_id: r.run_id,
                agent: r.agent,
                status: r.status.as_str().to_string(),
                error: r.error,
                created_at: r.created_at,
                updated_at: r.updated_at,
            })
            .collect(),
        next_cursor,
    }))
}

#[utoipa::path(
    get,
    path = "/threads/{thread_id}/runs/{run_id}/timeline",
    tag = "runs",
    params(
        ("thread_id" = String, Path, description = "Thread id"),
        ("run_id" = String, Path, description = "Run id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "The run's execution tree (turns → tools/delegations, with timing and usage)", body = serde_json::Value),
        (status = 404, description = "Unknown run", body = ErrorBody)
    )
)]
pub async fn run_timeline(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path((thread_id, run_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ServeError> {
    let events = state
        .session_store
        .read_run_after(&tenant, &thread_id, &run_id, 0)
        .await?;
    let trace = runic_state::timeline::project(events.iter().map(|entry| &entry.event))
        .into_iter()
        .next()
        .ok_or(ServeError::RunNotFound {
            id: run_id,
            thread: thread_id,
        })?;
    Ok(Json(serde_json::to_value(trace).map_err(|e| {
        ServeError::Internal(format!("timeline serialization failed: {e}"))
    })?))
}

const FLUSH_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) async fn flush_persist(persist: &crate::registry::PersistHandle) {
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
        return Ok(StatusCode::ACCEPTED);
    }
    if let Some(run) = active_run(&state, &tenant, &thread_id).await
        && state
            .session_store
            .request_cancel_run(&tenant, &run.run_id)
            .await
            .unwrap_or(false)
    {
        tracing::info!(%tenant, %thread_id, run_id = %run.run_id, "cancel signalled via run row");
        return Ok(StatusCode::ACCEPTED);
    }
    Err(ServeError::NoRunInFlight { thread_id })
}

async fn active_run(
    state: &AppState,
    tenant: &str,
    thread_id: &str,
) -> Option<runic_substrate::RunRecord> {
    match state
        .session_store
        .latest_active_run(tenant, thread_id)
        .await
    {
        Ok(run) => run,
        Err(runic_substrate::Error::Unsupported(_)) => state
            .session_store
            .latest_run(tenant, thread_id)
            .await
            .ok()
            .flatten()
            .filter(|r| !r.status.is_terminal()),
        Err(_) => None,
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
    if state
        .runs
        .steer_run(&tenant, &thread_id, req.text.clone())
        .await
    {
        return Ok(StatusCode::ACCEPTED);
    }
    if let Some(run) = active_run(&state, &tenant, &thread_id).await
        && state
            .session_store
            .push_steering(&tenant, &run.run_id, &req.text)
            .await
            .unwrap_or(false)
    {
        tracing::info!(%tenant, %thread_id, run_id = %run.run_id, "steering signalled via run row");
        return Ok(StatusCode::ACCEPTED);
    }
    Err(ServeError::NoRunInFlight { thread_id })
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

    let live_rx = state.runs.live_events(&tenant, &thread_id, &run_id).await;

    // A run executing on another instance: subscribe to the broker BEFORE
    // reading the store, so nothing published in between is lost (the replay
    // set below dedups the overlap).
    let mut remote_rx = None;
    if live_rx.is_none()
        && let Some(broker) = state.runs.broker()
        && state
            .session_store
            .get_run(&tenant, &run_id)
            .await
            .ok()
            .flatten()
            .is_some_and(|r| r.session_id == thread_id && !r.status.is_terminal())
    {
        remote_rx = broker.subscribe(&tenant, &thread_id).await;
    }

    // All persisted events for this run (from seq 0) — for existence + the real
    // terminal turn count; the replay payload is the slice after `after_seq`.
    let all = state
        .session_store
        .read_run_after(&tenant, &thread_id, &run_id, 0)
        .await?;

    let is_live = live_rx.is_some() || remote_rx.is_some();

    if all.is_empty() && !is_live {
        return Err(ServeError::RunNotFound {
            id: run_id,
            thread: thread_id,
        });
    }

    tracing::info!(
        %tenant, %thread_id, %run_id, after_seq,
        live = live_rx.is_some(),
        remote = remote_rx.is_some(),
        "replay attached"
    );

    let seen: std::collections::HashSet<String> = if remote_rx.is_some() {
        all.iter()
            .filter_map(|s| serde_json::to_string(&s.event).ok())
            .collect()
    } else {
        std::collections::HashSet::new()
    };

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
        } else if let Some(mut rx) = remote_rx {
            // Broker-fed tail from the executing instance, with a run-row poll
            // as the backstop when the publisher dies without a RunEnd.
            let mut check = tokio::time::interval(Duration::from_secs(5));
            check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            check.tick().await;
            loop {
                tokio::select! {
                    event = rx.recv() => {
                        let Some(event) = event else { break };
                        if event.run_id() != run_id {
                            continue;
                        }
                        if serde_json::to_string(&event)
                            .is_ok_and(|key| seen.contains(&key))
                        {
                            continue;
                        }
                        let end = match &event {
                            SessionEvent::RunEnd { outcome, .. } => {
                                Some((outcome.total_turns, outcome.stop_reason.clone()))
                            }
                            _ => None,
                        };
                        if let Some(wire) = from_session_event(event) {
                            yield Ok(to_sse(&wire, None));
                        }
                        if let Some((t, s)) = end {
                            total_turns = Some(t);
                            stop_reason = s;
                            break;
                        }
                    }
                    _ = check.tick() => {
                        let terminal = state
                            .session_store
                            .get_run(&tenant, &run_id)
                            .await
                            .ok()
                            .flatten()
                            .is_none_or(|r| r.status.is_terminal());
                        if terminal {
                            break;
                        }
                    }
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
/// Deliver an operator's answer to a deferred `ask_user`. The answer is written
/// as the tool result and the suspended run is re-queued to resume.
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
    resolve_answer(state, tenant, thread_id, ask_id, body.answer).await
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
    resolve_answer(state, tenant, thread_id, ask_id, body.answer).await
}

async fn resolve_answer(
    state: AppState,
    tenant: String,
    thread_id: String,
    ask_id: String,
    answer: String,
) -> Result<StatusCode, ServeError> {
    let events = state.session_store.read(&tenant, &thread_id).await?;

    let Some(run_id) = events.iter().rev().find_map(|e| match &e.event {
        SessionEvent::ToolDeferred {
            call_id, run_id, ..
        } if *call_id == ask_id => Some(run_id.clone()),
        _ => None,
    }) else {
        return Err(ServeError::BadRequest(format!(
            "no deferred call for ask_id '{ask_id}'"
        )));
    };

    let Some(tool_name) = events.iter().find_map(|e| match &e.event {
        SessionEvent::Message {
            run_id: msg_run,
            msg,
            ..
        } if *msg_run == run_id => match &msg.content {
            MessageContent::Blocks(blocks) => blocks.iter().find_map(|b| match b {
                ContentBlock::ToolUse { id, name, .. } if *id == ask_id => Some(name.clone()),
                _ => None,
            }),
            _ => None,
        },
        _ => None,
    }) else {
        return Err(ServeError::BadRequest(format!(
            "deferred call '{ask_id}' has no matching tool_use in run '{run_id}'"
        )));
    };

    let event = SessionEvent::Message {
        run_id: run_id.clone(),
        msg: Message::user_with_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: ask_id,
            tool_name,
            content: answer.into(),
            is_error: false,
            provenance: Vec::new(),
        }]),
        at: chrono::Utc::now(),
    };
    if !state
        .session_store
        .deliver_and_resume(&tenant, &run_id, &event)
        .await?
    {
        return Err(ServeError::BadRequest(format!(
            "run '{run_id}' is not awaiting an answer"
        )));
    }

    if let Some(nudge) = &state.nudge {
        nudge.nudge().await;
    }
    Ok(StatusCode::ACCEPTED)
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
