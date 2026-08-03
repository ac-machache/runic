use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;

use super::input::RunMessageRequest;
use super::queue;
use crate::app::AppState;
use crate::completion::Ticket;
use crate::error::{ErrorBody, ServeError};
use crate::store::{RunOutput, RunStatus};
use crate::tenant::Tenant;
use runic::state::Deferral;

const WAIT_TIMEOUT: Duration = Duration::from_secs(600);
const LOST_SIGNAL_GUARD: Duration = Duration::from_secs(30);

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub awaiting: Option<Awaiting>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct Awaiting {
    pub call_id: String,
    pub tool: String,
    #[schema(value_type = Object)]
    pub payload: serde_json::Value,
}

impl From<Deferral> for Awaiting {
    fn from(deferral: Deferral) -> Self {
        Self {
            call_id: deferral.call_id,
            tool: deferral.tool,
            payload: deferral.payload,
        }
    }
}

#[utoipa::path(
    post,
    path = "/sessions/{session_id}/runs/wait",
    tag = "runs",
    request_body = RunMessageRequest,
    params(
        ("session_id" = String, Path, description = "Session id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "The completed run", body = WaitRunResponse),
        (status = 400, description = "Invalid body or stored artifact", body = ErrorBody),
        (status = 500, description = "The run failed (provider error, max turns, ...)", body = ErrorBody),
        (status = 504, description = "The run did not finish within the wait window", body = ErrorBody)
    )
)]
pub async fn wait_run(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(session_id): Path<String>,
    Json(req): Json<RunMessageRequest>,
) -> Result<Json<WaitRunResponse>, ServeError> {
    let run_id = runic::state::new_run_id();
    let done = state.completions.ticket(&run_id);
    let agent = queue::enqueue(&state, &tenant, &run_id, Some(&session_id), req).await?;

    tracing::info!(%tenant, %session_id, %agent, %run_id, "wait run queued");

    await_completion(&state, &tenant, &run_id, done).await
}

pub(crate) async fn await_completion(
    state: &AppState,
    tenant: &str,
    run_id: &str,
    mut done: Ticket,
) -> Result<Json<WaitRunResponse>, ServeError> {
    let deadline = tokio::time::Instant::now() + WAIT_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Err(ServeError::Timeout {
                run_id: run_id.to_string(),
            });
        }
        done.settled(left.min(LOST_SIGNAL_GUARD)).await;

        let found = state
            .runs()
            .get(tenant, run_id)
            .await
            .map_err(|error| ServeError::Store(error.to_string()))?;
        let Some(record) = found else {
            continue;
        };
        match record.status {
            RunStatus::Failed => {
                return Err(ServeError::Runner(
                    record.error.unwrap_or_else(|| "the run failed".into()),
                ));
            }
            RunStatus::Successful | RunStatus::Cancelled | RunStatus::Waiting => {
                return Ok(Json(collect(state, tenant, run_id).await?));
            }
            RunStatus::Idle | RunStatus::Running => {}
        }
    }
}

pub(crate) async fn collect(
    state: &AppState,
    tenant: &str,
    run_id: &str,
) -> Result<WaitRunResponse, ServeError> {
    let record = state
        .runs()
        .get(tenant, run_id)
        .await
        .map_err(|error| ServeError::Store(error.to_string()))?;

    let output: RunOutput = record
        .as_ref()
        .and_then(|record| record.output.clone())
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();

    let awaiting = match record
        .as_ref()
        .and_then(|record| record.session_id.as_deref())
    {
        Some(session) => state
            .session(tenant, session)
            .awaiting()
            .await?
            .map(Awaiting::from),
        None => None,
    };

    let stop_reason = match (&awaiting, output.stop_reason) {
        (Some(_), None) => Some("suspended".to_string()),
        (_, settled) => settled,
    };
    Ok(WaitRunResponse {
        run_id: run_id.to_string(),
        text: output.text,
        stop_reason,
        total_turns: output.total_turns,
        input_tokens: output.input_tokens,
        output_tokens: output.output_tokens,
        structured: output.structured,
        awaiting,
    })
}
