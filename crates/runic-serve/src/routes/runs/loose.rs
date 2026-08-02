use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Serialize;

use super::input::RunMessageRequest;
use super::queue;
use super::wait::{WaitRunResponse, await_completion, collect};
use crate::app::AppState;
use crate::error::{ErrorBody, ServeError};
use crate::store::RunStatus;
use crate::tenant::Tenant;

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct QueuedRun {
    pub run_id: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct LooseRunResponse {
    pub run_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<WaitRunResponse>,
}

#[utoipa::path(
    post,
    path = "/runs/wait",
    tag = "runs",
    request_body = RunMessageRequest,
    params(
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "The completed run; nothing was written to any session", body = WaitRunResponse),
        (status = 400, description = "Invalid body", body = ErrorBody),
        (status = 500, description = "The run failed (provider error, max turns, ...)", body = ErrorBody),
        (status = 504, description = "The run did not finish within the wait window", body = ErrorBody)
    )
)]
pub async fn wait_run(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Json(req): Json<RunMessageRequest>,
) -> Result<Json<WaitRunResponse>, ServeError> {
    let run_id = runic::state::new_run_id();
    let done = state.completions.ticket(&run_id);
    let agent = queue::enqueue(&state, &tenant, &run_id, None, req).await?;

    tracing::info!(%tenant, %agent, %run_id, "stateless wait run queued");

    await_completion(&state, &tenant, &run_id, done).await
}

#[utoipa::path(
    get,
    path = "/runs/{run_id}",
    tag = "runs",
    params(
        ("run_id" = String, Path, description = "Run id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "Status, plus the answer once the run is terminal", body = LooseRunResponse),
        (status = 404, description = "Unknown run", body = ErrorBody)
    )
)]
pub async fn run_outcome(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(run_id): Path<String>,
) -> Result<Json<LooseRunResponse>, ServeError> {
    let record = state
        .runs()
        .get(&tenant, &run_id)
        .await
        .map_err(|error| ServeError::Store(error.to_string()))?
        .ok_or(ServeError::RunNotFound {
            id: run_id.clone(),
            session: "-".into(),
        })?;

    let answer = match record.status.is_terminal() || record.status == RunStatus::Waiting {
        true => Some(collect(&state, &tenant, &run_id).await?),
        false => None,
    };
    Ok(Json(LooseRunResponse {
        run_id,
        status: record.status.as_str().to_string(),
        error: record.error,
        answer,
    }))
}

#[utoipa::path(
    post,
    path = "/runs/forget",
    tag = "runs",
    request_body = RunMessageRequest,
    params(
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 202, description = "Queued; poll GET /runs/{run_id} for the outcome", body = QueuedRun),
        (status = 400, description = "Invalid body", body = ErrorBody)
    )
)]
pub async fn forget_run(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Json(req): Json<RunMessageRequest>,
) -> Result<(StatusCode, Json<QueuedRun>), ServeError> {
    let run_id = runic::state::new_run_id();
    let agent = queue::enqueue(&state, &tenant, &run_id, None, req).await?;

    tracing::info!(%tenant, %agent, %run_id, "stateless run queued");

    Ok((StatusCode::ACCEPTED, Json(QueuedRun { run_id })))
}
