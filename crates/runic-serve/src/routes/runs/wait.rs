use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use runic_substrate::SessionEvent;
use runic_types::Role;
use serde::Serialize;

use super::input::RunMessageRequest;
use crate::app::AppState;
use crate::completion::Ticket;
use crate::error::{ErrorBody, ServeError};
use crate::store::{RunSpec, RunStatus};
use crate::tenant::Tenant;
use runic_state::Deferral;

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
        (status = 500, description = "The run failed (provider error, max turns, ...)", body = ErrorBody),
        (status = 504, description = "The run did not finish within the wait window", body = ErrorBody)
    )
)]
pub async fn wait_run(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(thread_id): Path<String>,
    Json(req): Json<RunMessageRequest>,
) -> Result<Json<WaitRunResponse>, ServeError> {
    let agent = state.agents.resolve_agent(req.agent.as_deref())?;
    let context = req.context.clone();
    let message = req.into_message()?;

    let run_id = runic_state::new_run_id();
    let payload = serde_json::to_value(&message)
        .map_err(|error| ServeError::Internal(format!("could not encode the turn: {error}")))?;
    let spec = RunSpec::new(&tenant, &thread_id, &run_id, &agent)
        .input(payload)
        .context(context);

    let done = state.completions.ticket(&run_id);
    state
        .runs()
        .enqueue(&spec)
        .await
        .map_err(|error| ServeError::Internal(format!("could not queue the run: {error}")))?;

    tracing::info!(%tenant, %thread_id, %agent, %run_id, "wait run queued");

    await_completion(&state, &tenant, &thread_id, &run_id, done).await
}

async fn await_completion(
    state: &AppState,
    tenant: &str,
    thread_id: &str,
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
                return Ok(Json(collect(state, tenant, thread_id, run_id).await?));
            }
            RunStatus::Idle | RunStatus::Running => {}
        }
    }
}

pub(crate) async fn collect(
    state: &AppState,
    tenant: &str,
    thread_id: &str,
    run_id: &str,
) -> Result<WaitRunResponse, ServeError> {
    let events = state
        .store()
        .read_run_after(tenant, thread_id, run_id, 0)
        .await?;

    let awaiting = state
        .thread(tenant, thread_id)
        .awaiting()
        .await?
        .map(Awaiting::from);

    let mut text = String::new();
    let mut outcome = None;
    for stored in events {
        match stored.event {
            SessionEvent::Message { msg, .. } if matches!(msg.role, Role::Assistant) => {
                let spoken = msg.content.text_content();
                if !spoken.trim().is_empty() {
                    text = spoken;
                }
            }
            SessionEvent::RunEnd {
                outcome: finished, ..
            } => outcome = Some(finished),
            _ => {}
        }
    }

    let outcome = outcome.unwrap_or_default();
    let stop_reason = match (&awaiting, outcome.stop_reason) {
        (Some(_), None) => Some("suspended".to_string()),
        (_, settled) => settled,
    };
    Ok(WaitRunResponse {
        run_id: run_id.to_string(),
        text,
        stop_reason,
        total_turns: outcome.total_turns,
        input_tokens: outcome.usage.input_tokens,
        output_tokens: outcome.usage.output_tokens,
        structured: outcome.structured,
        awaiting,
    })
}
