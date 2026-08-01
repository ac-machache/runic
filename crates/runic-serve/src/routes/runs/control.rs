use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;

use crate::app::AppState;
use crate::error::{ErrorBody, ServeError};
use crate::store::Cancelled;
use crate::tenant::Tenant;

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct SteerRequest {
    pub message: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ResumeRequest {
    pub call_id: String,
    #[serde(deserialize_with = "answer_payload")]
    #[schema(value_type = Object)]
    pub answer: serde_json::Value,
}

fn answer_payload<'de, D>(deserializer: D) -> Result<serde_json::Value, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(_)
        | serde_json::Value::Object(_)
        | serde_json::Value::Array(_) => Ok(value),
        other => Err(serde::de::Error::custom(format!(
            "answer must be text or a structured answer, got {other}"
        ))),
    }
}

#[utoipa::path(
    post,
    path = "/sessions/{session_id}/runs/{run_id}/cancel",
    tag = "runs",
    params(
        ("session_id" = String, Path, description = "Session id"),
        ("run_id" = String, Path, description = "Run id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 202, description = "Cancellation requested; a running run stops at its next turn"),
        (status = 400, description = "The run already finished", body = ErrorBody),
        (status = 404, description = "Unknown run", body = ErrorBody)
    )
)]
pub async fn cancel_run(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path((session_id, run_id)): Path<(String, String)>,
) -> Result<StatusCode, ServeError> {
    on_session(&state, &tenant, &session_id, &run_id).await?;
    let accepted = state
        .runs()
        .request_cancel(&tenant, &run_id)
        .await
        .map_err(|error| ServeError::Store(error.to_string()))?;
    match accepted {
        Cancelled::Dropped => {
            state.events.finish(&run_id);
            Ok(StatusCode::ACCEPTED)
        }
        Cancelled::Flagged => Ok(StatusCode::ACCEPTED),
        Cancelled::Gone => Err(ServeError::BadRequest(format!(
            "run {run_id:?} has already finished"
        ))),
    }
}

#[utoipa::path(
    post,
    path = "/sessions/{session_id}/runs/{run_id}/steer",
    tag = "runs",
    request_body = SteerRequest,
    params(
        ("session_id" = String, Path, description = "Session id"),
        ("run_id" = String, Path, description = "Run id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 202, description = "Queued; the run reads it at its next turn"),
        (status = 400, description = "The run already finished, or the message is empty", body = ErrorBody),
        (status = 404, description = "Unknown run", body = ErrorBody)
    )
)]
pub async fn steer_run(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path((session_id, run_id)): Path<(String, String)>,
    Json(body): Json<SteerRequest>,
) -> Result<StatusCode, ServeError> {
    if body.message.trim().is_empty() {
        return Err(ServeError::BadRequest("message must not be empty".into()));
    }
    on_session(&state, &tenant, &session_id, &run_id).await?;
    let accepted = state
        .runs()
        .push_steering(&tenant, &run_id, &body.message)
        .await
        .map_err(|error| ServeError::Store(error.to_string()))?;
    match accepted {
        true => Ok(StatusCode::ACCEPTED),
        false => Err(ServeError::BadRequest(format!(
            "run {run_id:?} has already finished"
        ))),
    }
}

#[utoipa::path(
    post,
    path = "/sessions/{session_id}/runs/{run_id}/resume",
    tag = "runs",
    request_body = ResumeRequest,
    params(
        ("session_id" = String, Path, description = "Session id"),
        ("run_id" = String, Path, description = "Run id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 202, description = "Answer delivered; the parked run resumes"),
        (status = 400, description = "The run is not parked on that call", body = ErrorBody),
        (status = 404, description = "Unknown run", body = ErrorBody)
    )
)]
pub async fn resume_run(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path((session_id, run_id)): Path<(String, String)>,
    Json(body): Json<ResumeRequest>,
) -> Result<StatusCode, ServeError> {
    on_session(&state, &tenant, &session_id, &run_id).await?;

    let parked = state
        .session(&tenant, &session_id)
        .awaiting()
        .await?
        .is_some_and(|deferral| deferral.call_id == body.call_id);
    if !parked {
        return Err(ServeError::BadRequest(format!(
            "run {run_id:?} is not parked on call {:?}",
            body.call_id
        )));
    }

    let resumed = state
        .runs()
        .resume(&tenant, &run_id, &body.answer)
        .await
        .map_err(|error| ServeError::Store(error.to_string()))?;
    if !resumed {
        return Err(ServeError::BadRequest(format!(
            "run {run_id:?} is not awaiting an answer"
        )));
    }

    Ok(StatusCode::ACCEPTED)
}

async fn on_session(
    state: &AppState,
    tenant: &str,
    session_id: &str,
    run_id: &str,
) -> Result<(), ServeError> {
    let known = state
        .runs()
        .get(tenant, run_id)
        .await
        .map_err(|error| ServeError::Store(error.to_string()))?
        .is_some_and(|record| record.session_id.as_deref() == Some(session_id));
    match known {
        true => Ok(()),
        false => Err(ServeError::RunNotFound {
            id: run_id.to_string(),
            session: session_id.to_string(),
        }),
    }
}
