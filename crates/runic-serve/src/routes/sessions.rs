//! SessionKey CRUD — backed by the [`runic::store::SessionStore`].
//!
//! A "session" in the HTTP surface == a "session" internally. We expose the
//! resource with the conventional HTTP name; it routes to the same store.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use runic::store::SessionMeta;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::app::AppState;
use crate::error::{ErrorBody, ServeError};
use crate::tenant::Tenant;

/// One session's current shape.
#[derive(Debug, Serialize, ToSchema)]
pub struct SessionKey {
    pub session_id: String,
    pub tenant: String,
    pub label: Option<String>,
    pub event_count: usize,
}

/// A page of a tenant's sessions, most-recently-active first.
#[derive(Debug, Serialize, ToSchema)]
pub struct SessionList {
    pub sessions: Vec<SessionSummary>,
    /// Present only when more sessions remain; pass it back as `?cursor=`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// One `{seq, event}` entry of the stored log. `event` is a raw `SessionEvent`.
#[derive(Debug, Serialize, ToSchema)]
pub struct StoredEventEnvelope {
    pub seq: u64,
    #[schema(value_type = Object)]
    pub event: serde_json::Value,
}

/// `GET /sessions/{id}/events` — a page of the stored event log.
#[derive(Debug, Serialize, ToSchema)]
pub struct SessionEventsResponse {
    pub session_id: String,
    pub tenant: String,
    pub events: Vec<StoredEventEnvelope>,
    /// Seq to pass as `?after_seq=` for the next page; null when empty.
    pub next_after_seq: Option<u64>,
    pub has_more: bool,
}

/// `GET /sessions/{id}/state` — the agent's view of the session, folded from the
/// store. `busy` reports whether a run is in flight.
#[derive(Debug, Serialize, ToSchema)]
pub struct SessionStateResponse {
    pub session_id: String,
    pub tenant: String,
    pub busy: bool,
    pub label: Option<String>,
    pub system_prompt: Option<String>,
    #[schema(value_type = Vec<Object>)]
    pub messages: Vec<runic::types::Message>,
    pub event_count: u64,
    pub stats: ThreadStatsView,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ThreadStatsView {
    pub runs: u64,
    pub errored_runs: u64,
    pub cancelled_runs: u64,
    pub turns: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub model_ms: u64,
    pub last_prompt_tokens: u64,
    pub total_tool_calls: u64,
    #[schema(value_type = Object)]
    pub tools: std::collections::HashMap<String, runic::state::ToolStat>,
    pub delegations: u64,
    pub delegation_errors: u64,
    pub delegated_input_tokens: u64,
    pub delegated_output_tokens: u64,
    pub tasks_spawned: u64,
    pub tasks_finished: u64,
    pub tasks_failed: u64,
}

impl From<&runic::state::SessionStats> for ThreadStatsView {
    fn from(s: &runic::state::SessionStats) -> Self {
        Self {
            runs: s.runs,
            errored_runs: s.errored_runs,
            cancelled_runs: s.cancelled_runs,
            turns: s.turns,
            input_tokens: s.input_tokens,
            output_tokens: s.output_tokens,
            cache_read_tokens: s.cache_read_tokens,
            cache_write_tokens: s.cache_write_tokens,
            model_ms: s.model_ms,
            last_prompt_tokens: s.last_prompt_tokens,
            total_tool_calls: s.total_tool_calls,
            tools: s.tools.clone(),
            delegations: s.delegations,
            delegation_errors: s.delegation_errors,
            delegated_input_tokens: s.delegated_usage.input_tokens,
            delegated_output_tokens: s.delegated_usage.output_tokens,
            tasks_spawned: s.tasks_spawned,
            tasks_finished: s.tasks_finished,
            tasks_failed: s.tasks_failed,
        }
    }
}

fn default_sessions_limit() -> usize {
    50
}

fn default_events_limit() -> usize {
    200
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListThreadsQuery {
    /// Page size, clamped to 1..=200.
    #[serde(default = "default_sessions_limit")]
    pub limit: usize,
    /// Opaque keyset cursor from a previous page's `next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct EventsQuery {
    /// Return events with `seq` greater than this.
    #[serde(default)]
    pub after_seq: u64,
    /// Page size, clamped to 1..=1000.
    #[serde(default = "default_events_limit")]
    pub limit: usize,
}

fn encode_cursor(meta: &SessionMeta) -> String {
    format!("{}|{}", meta.last_activity.to_rfc3339(), meta.session_id)
}

fn decode_cursor(s: &str) -> Option<(DateTime<Utc>, String)> {
    let (ts, id) = s.split_once('|')?;
    let at = DateTime::parse_from_rfc3339(ts).ok()?.with_timezone(&Utc);
    Some((at, id.to_string()))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SessionSummary {
    pub session_id: String,
    pub label: Option<String>,
    pub event_count: u64,
    pub run_count: u64,
    pub errored_runs: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

fn summary_from_meta(meta: runic::store::SessionMeta) -> SessionSummary {
    SessionSummary {
        session_id: meta.session_id,
        label: meta.label,
        event_count: meta.event_count,
        run_count: meta.run_count,
        errored_runs: meta.errored_runs,
        input_tokens: meta.input_tokens,
        output_tokens: meta.output_tokens,
        last_run_status: meta.last_run_status,
        last_run_at: meta.last_run_at,
        agent: meta.agent,
        parent_session: meta.parent_session,
    }
}

#[derive(Debug, Deserialize, Default, ToSchema)]
pub struct CreateSessionRequest {
    /// If provided, the session is created with this id; otherwise the server
    /// generates a UUID. Either way the id is returned so the client can stash
    /// it.
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Deserialize, Default, ToSchema)]
pub struct UpdateSessionRequest {
    /// Omit to leave unchanged, string to set, null to clear.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>)]
    pub label: Option<Option<String>>,
}

fn double_option<'de, D>(de: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::deserialize(de)?))
}

fn normalize_label(label: Option<String>) -> Option<String> {
    label
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn session_from_meta(tenant: String, meta: SessionMeta) -> SessionKey {
    SessionKey {
        session_id: meta.session_id,
        tenant,
        label: meta.label,
        event_count: meta.event_count as usize,
    }
}

/// `POST /sessions` — create an empty session. Idempotent on an existing id.
///
/// A label materialises the metadata row immediately; otherwise the store lazily
/// creates per-session state on first event append.
#[utoipa::path(
    post,
    path = "/sessions",
    tag = "sessions",
    request_body = CreateSessionRequest,
    params(("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")),
    responses((status = 201, description = "Created (idempotent on an existing id)", body = SessionKey))
)]
pub async fn create_session(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Json(req): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<SessionKey>), ServeError> {
    let session_id = req
        .session_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let label = normalize_label(req.label);

    // Materialize the metadata row so the session is distinguishable from one
    // that never existed — without clobbering an existing label on re-create.
    let session = state.session(&tenant, &session_id);
    let existed = session.meta().await?.is_some();
    if label.is_some() || !existed {
        session.set_label(label.as_deref()).await?;
    }

    let meta = session
        .meta()
        .await?
        .ok_or_else(|| ServeError::Internal("session metadata not materialized".into()))?;
    Ok((StatusCode::CREATED, Json(session_from_meta(tenant, meta))))
}

/// `GET /sessions?limit=&cursor=` — a page of the tenant's sessions,
/// most-recently-active first. `next_cursor` is present when more remain.
#[utoipa::path(
    get,
    path = "/sessions",
    tag = "sessions",
    params(ListThreadsQuery, ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")),
    responses(
        (status = 200, description = "A page of sessions", body = SessionList),
        (status = 400, description = "Invalid cursor", body = ErrorBody)
    )
)]
pub async fn list_sessions(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Query(q): Query<ListThreadsQuery>,
) -> Result<Json<SessionList>, ServeError> {
    let limit = q.limit.clamp(1, 200);
    let after = match q.cursor.as_deref() {
        Some(cursor) => Some(
            decode_cursor(cursor)
                .ok_or_else(|| ServeError::BadRequest("invalid session cursor".into()))?,
        ),
        None => None,
    };
    let mut metas = state
        .sessions()
        .list_sessions_page(&tenant, after, limit + 1, runic::store::SessionScope::Roots)
        .await?;

    let next_cursor = (metas.len() > limit).then(|| {
        metas.truncate(limit);
        encode_cursor(metas.last().expect("non-empty page"))
    });
    let sessions = metas.into_iter().map(summary_from_meta).collect();
    Ok(Json(SessionList {
        sessions,
        next_cursor,
    }))
}

/// `GET /sessions/:id/children?limit=&cursor=` — a page of the session's child
/// sessions (subagent transcripts), most-recently-active first.
#[utoipa::path(
    get,
    path = "/sessions/{session_id}/children",
    tag = "sessions",
    params(
        ListThreadsQuery,
        ("session_id" = String, Path, description = "Parent session id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "A page of child sessions", body = SessionList),
        (status = 400, description = "Invalid cursor", body = ErrorBody),
        (status = 404, description = "Unknown session", body = ErrorBody)
    )
)]
pub async fn list_session_children(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(session_id): Path<String>,
    Query(q): Query<ListThreadsQuery>,
) -> Result<Json<SessionList>, ServeError> {
    state
        .session(&tenant, &session_id)
        .meta()
        .await?
        .ok_or_else(|| ServeError::SessionNotFound {
            id: session_id.clone(),
        })?;

    let limit = q.limit.clamp(1, 200);
    let after = match q.cursor.as_deref() {
        Some(cursor) => Some(
            decode_cursor(cursor)
                .ok_or_else(|| ServeError::BadRequest("invalid session cursor".into()))?,
        ),
        None => None,
    };
    let mut metas = state
        .sessions()
        .list_sessions_page(
            &tenant,
            after,
            limit + 1,
            runic::store::SessionScope::ChildrenOf(session_id),
        )
        .await?;

    let next_cursor = (metas.len() > limit).then(|| {
        metas.truncate(limit);
        encode_cursor(metas.last().expect("non-empty page"))
    });
    let sessions = metas.into_iter().map(summary_from_meta).collect();
    Ok(Json(SessionList {
        sessions,
        next_cursor,
    }))
}

/// `GET /sessions/:id` — current shape of one session (event count etc.).
#[utoipa::path(
    get,
    path = "/sessions/{session_id}",
    tag = "sessions",
    params(
        ("session_id" = String, Path, description = "Session id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "The session", body = SessionKey),
        (status = 404, description = "Unknown session", body = ErrorBody)
    )
)]
pub async fn get_session(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(session_id): Path<String>,
) -> Result<Json<SessionKey>, ServeError> {
    let meta = state
        .session(&tenant, &session_id)
        .meta()
        .await?
        .ok_or(ServeError::SessionNotFound { id: session_id })?;
    Ok(Json(session_from_meta(tenant, meta)))
}

/// `PATCH /sessions/:id` — update session metadata.
#[utoipa::path(
    patch,
    path = "/sessions/{session_id}",
    tag = "sessions",
    request_body = UpdateSessionRequest,
    params(
        ("session_id" = String, Path, description = "Session id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "Updated session", body = SessionKey),
        (status = 404, description = "Unknown session", body = ErrorBody)
    )
)]
pub async fn update_session(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(session_id): Path<String>,
    Json(req): Json<UpdateSessionRequest>,
) -> Result<Json<SessionKey>, ServeError> {
    // PATCH updates an existing session; it never creates one.
    let session = state.session(&tenant, &session_id);
    if session.meta().await?.is_none() {
        return Err(ServeError::SessionNotFound { id: session_id });
    }

    if let Some(label) = req.label {
        session.set_label(normalize_label(label).as_deref()).await?;
    }

    let meta = session
        .meta()
        .await?
        .ok_or_else(|| ServeError::Internal("session metadata vanished".into()))?;
    Ok(Json(session_from_meta(tenant, meta)))
}

/// `GET /sessions/:id/events` — the full stored event log as a JSON snapshot
/// (not SSE). Each entry is `{seq, event}` with the raw `SessionEvent`. Powers
/// a dev UI's history load.
#[utoipa::path(
    get,
    path = "/sessions/{session_id}/events",
    tag = "sessions",
    params(
        EventsQuery,
        ("session_id" = String, Path, description = "Session id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "A page of the stored event log", body = SessionEventsResponse),
        (status = 404, description = "Unknown session", body = ErrorBody)
    )
)]
pub async fn session_events(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(session_id): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<SessionEventsResponse>, ServeError> {
    let session = state.session(&tenant, &session_id);
    if session.meta().await?.is_none() {
        return Err(ServeError::SessionNotFound { id: session_id });
    }

    let limit = q.limit.clamp(1, 1000);
    let (stored, has_more) = session.events_after(q.after_seq, limit).await?;
    let next_after_seq = stored.last().map(|s| s.seq);
    let events = stored
        .into_iter()
        .map(|s| StoredEventEnvelope {
            seq: s.seq,
            event: serde_json::to_value(s.event).unwrap_or(serde_json::Value::Null),
        })
        .collect();
    Ok(Json(SessionEventsResponse {
        session_id,
        tenant,
        events,
        next_after_seq,
        has_more,
    }))
}

/// `GET /sessions/:id/state` — the session as the agent would see it: the message
/// list, run / event counts, and whether a run is in flight.
#[utoipa::path(
    get,
    path = "/sessions/{session_id}/state",
    tag = "sessions",
    params(
        ("session_id" = String, Path, description = "Session id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "Runner view of the session (see `busy`)", body = SessionStateResponse),
        (status = 404, description = "Unknown session", body = ErrorBody)
    )
)]
pub async fn session_state(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(session_id): Path<String>,
) -> Result<Json<SessionStateResponse>, ServeError> {
    // Authoritative label + event_count from metadata; 404 if the session was
    // never created (don't build a warm agent for a phantom session).
    let session = state.session(&tenant, &session_id);
    let Some(meta) = session.meta().await? else {
        return Err(ServeError::SessionNotFound { id: session_id });
    };
    let label = meta.label;
    let event_count = meta.event_count;

    let messages = session.messages().await.unwrap_or_default();
    let stats = session.stats().await.unwrap_or_default();
    let busy = active_run(&state, &tenant, &session_id).await.is_some();
    Ok(Json(SessionStateResponse {
        session_id,
        tenant,
        busy,
        label,
        system_prompt: None,
        messages,
        event_count,
        stats: (&stats).into(),
    }))
}

/// `DELETE /sessions/:id` — drop the session, its descendants, and their artifacts.
#[utoipa::path(
    delete,
    path = "/sessions/{session_id}",
    tag = "sessions",
    params(
        ("session_id" = String, Path, description = "Session id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 204, description = "SessionKey, descendants, and artifacts dropped"),
        (status = 409, description = "A run is active on this session; cancel it first", body = ErrorBody)
    )
)]
pub async fn delete_session(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ServeError> {
    if active_run(&state, &tenant, &session_id).await.is_some() {
        return Err(ServeError::SessionBusy { session_id });
    }
    delete_tree(&state, &tenant, &session_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn active_run(
    state: &AppState,
    tenant: &str,
    session_id: &str,
) -> Option<crate::store::RunRecord> {
    state
        .runs()
        .latest_active(tenant, session_id)
        .await
        .ok()
        .flatten()
}

async fn delete_tree(state: &AppState, tenant: &str, session_id: &str) -> Result<(), ServeError> {
    let order = state.session(tenant, session_id).descendants().await?;
    let descendant_count = order.len() - 1;
    let mut artifact_count = 0usize;
    for session in order.iter().rev() {
        artifact_count += state
            .artifacts()
            .delete_session_artifacts(tenant, session)
            .await?;
        state.sessions().delete_session(tenant, session).await?;
    }

    match state.sessions().delete_orphan_children(tenant).await {
        Ok(reaped) => {
            for orphan in &reaped {
                if let Err(e) = state
                    .artifacts()
                    .delete_session_artifacts(tenant, orphan)
                    .await
                {
                    tracing::warn!(%tenant, %orphan, error = %e, "orphan artifact cleanup failed");
                }
            }
            if !reaped.is_empty() {
                tracing::info!(%tenant, count = reaped.len(), "orphaned child sessions reaped");
            }
        }
        Err(e) => tracing::warn!(%tenant, error = %e, "orphan sweep failed"),
    }

    tracing::info!(%tenant, %session_id, artifact_count, descendant_count, "session deleted");
    Ok(())
}
