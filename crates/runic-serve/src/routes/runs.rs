pub mod control;
pub mod input;
pub mod stream;
pub mod wait;

use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;

use crate::app::AppState;
use crate::error::{ErrorBody, ServeError};
use crate::tenant::Tenant;

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
        .runs()
        .get(&tenant, &run_id)
        .await
        .map_err(|error| ServeError::Store(error.to_string()))?
        .filter(|record| record.session_id == thread_id)
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
        (status = 200, description = "Run summaries, newest first, keyset-paginated via `cursor`", body = RunListResponse)
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
        .runs()
        .list(&tenant, &thread_id, limit as i64, before)
        .await
        .map_err(|error| ServeError::Store(error.to_string()))?;
    let next_cursor = (records.len() == limit)
        .then(|| {
            records
                .last()
                .map(|record| format!("{}|{}", record.created_at.to_rfc3339(), record.run_id))
        })
        .flatten();
    Ok(Json(RunListResponse {
        runs: records
            .into_iter()
            .map(|record| RunSummary {
                run_id: record.run_id,
                agent: record.agent,
                status: record.status.as_str().to_string(),
                error: record.error,
                created_at: record.created_at,
                updated_at: record.updated_at,
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
        .store()
        .read_run_after(&tenant, &thread_id, &run_id, 0)
        .await?;
    let trace = runic_substrate::timeline::project(events.iter().map(|entry| &entry.event))
        .into_iter()
        .next()
        .ok_or(ServeError::RunNotFound {
            id: run_id,
            thread: thread_id,
        })?;
    Ok(Json(serde_json::to_value(trace).map_err(|error| {
        ServeError::Internal(format!("timeline serialization failed: {error}"))
    })?))
}
