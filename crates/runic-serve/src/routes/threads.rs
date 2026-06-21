//! Thread CRUD — backed by the [`runic_substrate::SessionStore`].
//!
//! A "thread" in the HTTP surface == a "session" internally. We expose the
//! resource with the conventional HTTP name; it routes to the same store.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use runic_substrate::StoredEvent;
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::error::ServeError;
use crate::tenant::Tenant;

#[derive(Debug, Serialize)]
pub struct Thread {
    pub thread_id: String,
    pub tenant: String,
    pub event_count: usize,
}

#[derive(Debug, Serialize)]
pub struct ThreadList {
    pub threads: Vec<ThreadSummary>,
}

#[derive(Debug, Serialize)]
pub struct ThreadSummary {
    pub thread_id: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct CreateThreadRequest {
    /// If provided, the thread is created with this id; otherwise the server
    /// generates a UUID. Either way the id is returned so the client can stash
    /// it.
    #[serde(default)]
    pub thread_id: Option<String>,
}

/// `POST /threads` — create an empty thread. Idempotent on an existing id.
///
/// Nothing is materialised in the store for an empty thread — the store lazily
/// creates per-session state on first event append. The response just confirms
/// the id to use on subsequent calls.
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

    let events: Vec<StoredEvent> = state
        .session_store
        .read(&tenant, &thread_id)
        .await
        .unwrap_or_default();

    Ok((
        StatusCode::CREATED,
        Json(Thread {
            thread_id: thread_id.clone(),
            tenant,
            event_count: events.len(),
        }),
    ))
}

/// `GET /threads` — list every thread for the authed tenant.
pub async fn list_threads(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
) -> Result<Json<ThreadList>, ServeError> {
    let metas = state.session_store.list_sessions(&tenant).await?;
    Ok(Json(ThreadList {
        threads: metas
            .into_iter()
            .map(|m| ThreadSummary {
                thread_id: m.session_id,
            })
            .collect(),
    }))
}

/// `GET /threads/:id` — current shape of one thread (event count etc.).
pub async fn get_thread(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(thread_id): Path<String>,
) -> Result<Json<Thread>, ServeError> {
    let events: Vec<StoredEvent> = state.session_store.read(&tenant, &thread_id).await?;
    // No events yet → empty shape rather than 404 (a real server would have a
    // separate threads-metadata table to tell "created but empty" from "never
    // existed").
    Ok(Json(Thread {
        thread_id,
        tenant,
        event_count: events.len(),
    }))
}

/// `GET /threads/:id/events` — the full stored event log as a JSON snapshot
/// (not SSE). Each entry is `{seq, event}` with the raw `SessionEvent`. Powers
/// a dev UI's history load.
pub async fn thread_events(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(thread_id): Path<String>,
) -> Result<Json<serde_json::Value>, ServeError> {
    let stored: Vec<StoredEvent> = state.session_store.read(&tenant, &thread_id).await?;
    let events: Vec<serde_json::Value> = stored
        .into_iter()
        .map(|s| serde_json::json!({ "seq": s.seq, "event": s.event }))
        .collect();
    Ok(Json(serde_json::json!({
        "thread_id": thread_id,
        "tenant": tenant,
        "events": events,
    })))
}

/// `GET /threads/:id/state` — agent state for inspection: the system prompt,
/// the message list as sent to the model, and run / event counts. Reads the
/// warm agent when idle; if a run is in flight (slot locked) it reports `busy`
/// and reconstructs the message list from the event store.
pub async fn thread_state(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(thread_id): Path<String>,
) -> Result<Json<serde_json::Value>, ServeError> {
    let agent_arc = state.pool.get_or_build(&tenant, &thread_id).await;
    if let Ok(agent) = agent_arc.try_lock() {
        let st = agent.state();
        return Ok(Json(serde_json::json!({
            "thread_id": thread_id,
            "tenant": tenant,
            "busy": false,
            "system_prompt": st.system_prompt,
            "messages": st.messages_for_provider(),
            "event_count": st.events.len(),
            "run_count": st.runs().len(),
        })));
    }

    // Busy (run in progress) — reconstruct messages from the store.
    let messages =
        runic_substrate::replay_messages(state.session_store.as_ref(), &tenant, &thread_id)
            .await
            .unwrap_or_default();
    let event_count = state
        .session_store
        .read(&tenant, &thread_id)
        .await
        .map(|e| e.len())
        .unwrap_or(0);
    Ok(Json(serde_json::json!({
        "thread_id": thread_id,
        "tenant": tenant,
        "busy": true,
        "system_prompt": "",
        "messages": messages,
        "event_count": event_count,
        "run_count": serde_json::Value::Null,
    })))
}

/// `DELETE /threads/:id` — drop the thread's session AND its in-pool Agent so
/// the next run starts fresh.
pub async fn delete_thread(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(thread_id): Path<String>,
) -> Result<StatusCode, ServeError> {
    state.pool.evict(&tenant, &thread_id).await;
    state
        .session_store
        .delete_session(&tenant, &thread_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
