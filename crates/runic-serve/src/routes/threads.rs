//! Thread CRUD — backed by the [`runic_substrate::SessionStore`].
//!
//! A "thread" in the HTTP surface == a "session" internally. We expose the
//! resource with the conventional HTTP name; it routes to the same store.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use runic_substrate::SessionMeta;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::app::AppState;
use crate::error::{ErrorBody, ServeError};
use crate::tenant::Tenant;

/// One thread's current shape.
#[derive(Debug, Serialize, ToSchema)]
pub struct Thread {
    pub thread_id: String,
    pub tenant: String,
    pub label: Option<String>,
    pub event_count: usize,
}

/// A page of a tenant's threads, most-recently-active first.
#[derive(Debug, Serialize, ToSchema)]
pub struct ThreadList {
    pub threads: Vec<ThreadSummary>,
    /// Present only when more threads remain; pass it back as `?cursor=`.
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

/// `GET /threads/{id}/events` — a page of the stored event log.
#[derive(Debug, Serialize, ToSchema)]
pub struct ThreadEventsResponse {
    pub thread_id: String,
    pub tenant: String,
    pub events: Vec<StoredEventEnvelope>,
    /// Seq to pass as `?after_seq=` for the next page; null when empty.
    pub next_after_seq: Option<u64>,
    pub has_more: bool,
}

/// `GET /threads/{id}/state` — the agent's view of the thread. When a run is in
/// flight the slot is locked, so `busy` is true and `system_prompt`/`run_count`
/// are null (unreadable without the lock); `messages` is reconstructed from the
/// store.
#[derive(Debug, Serialize, ToSchema)]
pub struct ThreadStateResponse {
    pub thread_id: String,
    pub tenant: String,
    pub busy: bool,
    pub label: Option<String>,
    pub system_prompt: Option<String>,
    #[schema(value_type = Vec<Object>)]
    pub messages: Vec<runic_types::Message>,
    pub event_count: u64,
    pub run_count: Option<usize>,
}

fn default_threads_limit() -> usize {
    50
}

fn default_events_limit() -> usize {
    200
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListThreadsQuery {
    /// Page size, clamped to 1..=200.
    #[serde(default = "default_threads_limit")]
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
pub struct ThreadSummary {
    pub thread_id: String,
    pub label: Option<String>,
    pub event_count: u64,
}

#[derive(Debug, Deserialize, Default, ToSchema)]
pub struct CreateThreadRequest {
    /// If provided, the thread is created with this id; otherwise the server
    /// generates a UUID. Either way the id is returned so the client can stash
    /// it.
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Deserialize, Default, ToSchema)]
pub struct UpdateThreadRequest {
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

fn thread_from_meta(tenant: String, meta: SessionMeta) -> Thread {
    Thread {
        thread_id: meta.session_id,
        tenant,
        label: meta.label,
        event_count: meta.event_count as usize,
    }
}

/// `POST /threads` — create an empty thread. Idempotent on an existing id.
///
/// A label materialises the metadata row immediately; otherwise the store lazily
/// creates per-session state on first event append.
#[utoipa::path(
    post,
    path = "/threads",
    tag = "threads",
    request_body = CreateThreadRequest,
    params(("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")),
    responses((status = 201, description = "Created (idempotent on an existing id)", body = Thread))
)]
pub async fn create_thread(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Json(req): Json<CreateThreadRequest>,
) -> Result<(StatusCode, Json<Thread>), ServeError> {
    let thread_id = req
        .thread_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let label = normalize_label(req.label);

    // Materialize the metadata row so the thread is distinguishable from one
    // that never existed — without clobbering an existing label on re-create.
    let existed = state
        .session_store
        .session_meta(&tenant, &thread_id)
        .await?
        .is_some();
    if label.is_some() || !existed {
        state
            .session_store
            .set_label(&tenant, &thread_id, label.as_deref())
            .await?;
    }
    if label.is_some() {
        state
            .pool
            .set_warm_label(&tenant, &thread_id, label.clone())
            .await;
    }

    let meta = state
        .session_store
        .session_meta(&tenant, &thread_id)
        .await?
        .ok_or_else(|| ServeError::Internal("thread metadata not materialized".into()))?;
    Ok((StatusCode::CREATED, Json(thread_from_meta(tenant, meta))))
}

/// `GET /threads?limit=&cursor=` — a page of the tenant's threads,
/// most-recently-active first. `next_cursor` is present when more remain.
#[utoipa::path(
    get,
    path = "/threads",
    tag = "threads",
    params(ListThreadsQuery, ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")),
    responses(
        (status = 200, description = "A page of threads", body = ThreadList),
        (status = 400, description = "Invalid cursor", body = ErrorBody)
    )
)]
pub async fn list_threads(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Query(q): Query<ListThreadsQuery>,
) -> Result<Json<ThreadList>, ServeError> {
    let limit = q.limit.clamp(1, 200);
    let after = match q.cursor.as_deref() {
        Some(cursor) => Some(
            decode_cursor(cursor)
                .ok_or_else(|| ServeError::BadRequest("invalid thread cursor".into()))?,
        ),
        None => None,
    };
    let mut metas = state
        .session_store
        .list_sessions_page(&tenant, after, limit + 1)
        .await?;

    let next_cursor = (metas.len() > limit).then(|| {
        metas.truncate(limit);
        encode_cursor(metas.last().expect("non-empty page"))
    });
    let threads = metas
        .into_iter()
        .map(|m| ThreadSummary {
            thread_id: m.session_id,
            label: m.label,
            event_count: m.event_count,
        })
        .collect();
    Ok(Json(ThreadList {
        threads,
        next_cursor,
    }))
}

/// `GET /threads/:id` — current shape of one thread (event count etc.).
#[utoipa::path(
    get,
    path = "/threads/{thread_id}",
    tag = "threads",
    params(
        ("thread_id" = String, Path, description = "Thread id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "The thread", body = Thread),
        (status = 404, description = "Unknown thread", body = ErrorBody)
    )
)]
pub async fn get_thread(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(thread_id): Path<String>,
) -> Result<Json<Thread>, ServeError> {
    let meta = state
        .session_store
        .session_meta(&tenant, &thread_id)
        .await?
        .ok_or(ServeError::ThreadNotFound { id: thread_id })?;
    Ok(Json(thread_from_meta(tenant, meta)))
}

/// `PATCH /threads/:id` — update thread metadata. The DB metadata row is the
/// source of truth; a warm agent mirrors the label after the write succeeds.
#[utoipa::path(
    patch,
    path = "/threads/{thread_id}",
    tag = "threads",
    request_body = UpdateThreadRequest,
    params(
        ("thread_id" = String, Path, description = "Thread id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "Updated thread", body = Thread),
        (status = 404, description = "Unknown thread", body = ErrorBody)
    )
)]
pub async fn update_thread(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(thread_id): Path<String>,
    Json(req): Json<UpdateThreadRequest>,
) -> Result<Json<Thread>, ServeError> {
    // PATCH updates an existing thread; it never creates one.
    if state
        .session_store
        .session_meta(&tenant, &thread_id)
        .await?
        .is_none()
    {
        return Err(ServeError::ThreadNotFound { id: thread_id });
    }

    if let Some(label) = req.label {
        let label = normalize_label(label);
        state
            .session_store
            .set_label(&tenant, &thread_id, label.as_deref())
            .await?;
        state.pool.set_warm_label(&tenant, &thread_id, label).await;
    }

    let meta = state
        .session_store
        .session_meta(&tenant, &thread_id)
        .await?
        .ok_or_else(|| ServeError::Internal("thread metadata vanished".into()))?;
    Ok(Json(thread_from_meta(tenant, meta)))
}

/// `GET /threads/:id/events` — the full stored event log as a JSON snapshot
/// (not SSE). Each entry is `{seq, event}` with the raw `SessionEvent`. Powers
/// a dev UI's history load.
#[utoipa::path(
    get,
    path = "/threads/{thread_id}/events",
    tag = "threads",
    params(
        EventsQuery,
        ("thread_id" = String, Path, description = "Thread id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "A page of the stored event log", body = ThreadEventsResponse),
        (status = 404, description = "Unknown thread", body = ErrorBody)
    )
)]
pub async fn thread_events(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(thread_id): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<ThreadEventsResponse>, ServeError> {
    if state
        .session_store
        .session_meta(&tenant, &thread_id)
        .await?
        .is_none()
    {
        return Err(ServeError::ThreadNotFound { id: thread_id });
    }

    let limit = q.limit.clamp(1, 1000);
    let mut stored = state
        .session_store
        .read_after_limited(&tenant, &thread_id, q.after_seq, limit + 1)
        .await?;
    let has_more = stored.len() > limit;
    stored.truncate(limit);
    let next_after_seq = stored.last().map(|s| s.seq);
    let events = stored
        .into_iter()
        .map(|s| StoredEventEnvelope {
            seq: s.seq,
            event: serde_json::to_value(s.event).unwrap_or(serde_json::Value::Null),
        })
        .collect();
    Ok(Json(ThreadEventsResponse {
        thread_id,
        tenant,
        events,
        next_after_seq,
        has_more,
    }))
}

/// `GET /threads/:id/state` — agent state for inspection: the system prompt,
/// the message list as sent to the model, and run / event counts. Reads the
/// warm agent when idle; if a run is in flight (slot locked) it reports `busy`
/// and reconstructs the message list from the event store.
#[utoipa::path(
    get,
    path = "/threads/{thread_id}/state",
    tag = "threads",
    params(
        ("thread_id" = String, Path, description = "Thread id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "Agent view of the thread (see `busy`)", body = ThreadStateResponse),
        (status = 404, description = "Unknown thread", body = ErrorBody)
    )
)]
pub async fn thread_state(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(thread_id): Path<String>,
) -> Result<Json<ThreadStateResponse>, ServeError> {
    // Authoritative label + event_count from metadata; 404 if the thread was
    // never created (don't build a warm agent for a phantom thread).
    let Some(meta) = state
        .session_store
        .session_meta(&tenant, &thread_id)
        .await?
    else {
        return Err(ServeError::ThreadNotFound { id: thread_id });
    };
    let label = meta.label;
    let event_count = meta.event_count;

    let agent_arc = state.pool.get_or_build(&tenant, &thread_id).await;
    if let Ok(agent) = agent_arc.try_lock() {
        let st = agent.state();
        return Ok(Json(ThreadStateResponse {
            thread_id,
            tenant,
            busy: false,
            label,
            system_prompt: Some(st.system_prompt.clone()),
            messages: st.messages_for_provider(),
            event_count,
            run_count: Some(st.runs().len()),
        }));
    }

    // Busy (run in progress) — reconstruct messages from the store; the system
    // prompt and run count aren't readable without the lock, so they're null
    // (not a "" placeholder that looks like truth).
    let messages =
        runic_substrate::replay_messages(state.session_store.as_ref(), &tenant, &thread_id)
            .await
            .unwrap_or_default();
    Ok(Json(ThreadStateResponse {
        thread_id,
        tenant,
        busy: true,
        label,
        system_prompt: None,
        messages,
        event_count,
        run_count: None,
    }))
}

/// `DELETE /threads/:id` — drop the thread's session AND its in-pool Agent so
/// the next run starts fresh.
#[utoipa::path(
    delete,
    path = "/threads/{thread_id}",
    tag = "threads",
    params(
        ("thread_id" = String, Path, description = "Thread id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses((status = 204, description = "Thread, artifacts, and warm agent dropped"))
)]
pub async fn delete_thread(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(thread_id): Path<String>,
) -> Result<StatusCode, ServeError> {
    state.pool.evict(&tenant, &thread_id).await;
    for artifact in state.artifact_store.list(&tenant, &thread_id).await? {
        state.artifact_store.delete(&artifact.id).await?;
    }
    state
        .session_store
        .delete_session(&tenant, &thread_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
