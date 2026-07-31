use std::time::Duration;

use apalis::prelude::{TaskBuilder, TaskSink};
use apalis_postgres::PgListener;
use apalis_sql::ext::TaskBuilderExt;
use axum::Json;
use axum::extract::{Path, State};
use runic_substrate::{RunStatus, SessionEvent};
use runic_types::Role;
use serde::Serialize;

use super::input::RunMessageRequest;
use crate::app::AppState;
use crate::error::{ErrorBody, ServeError};
use crate::queue::{MAX_ATTEMPTS, RunJob};
use crate::tenant::Tenant;
use crate::worker::COMPLETION_CHANNEL;

const WAIT_TIMEOUT: Duration = Duration::from_secs(600);
const RECHECK_EVERY: Duration = Duration::from_secs(5);

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
    state
        .store()
        .create_run(&tenant, &thread_id, &run_id, &agent)
        .await?;

    let job = RunJob {
        tenant: tenant.clone(),
        thread_id: thread_id.clone(),
        run_id: run_id.clone(),
        agent: agent.clone(),
        message,
        context,
        wave: 0,
    };
    let done = match dispatch(&state, job, &run_id).await {
        Ok(done) => done,
        Err(error) => return Err(abandon(&state, &run_id, error).await),
    };

    tracing::info!(%tenant, %thread_id, %agent, %run_id, "wait run queued");

    await_completion(&state, &tenant, &thread_id, &run_id, done).await
}

async fn dispatch(state: &AppState, job: RunJob, run_id: &str) -> Result<PgListener, ServeError> {
    let mut done = PgListener::connect_with(&state.pool)
        .await
        .map_err(|error| {
            ServeError::Internal(format!("could not watch for completion: {error}"))
        })?;
    done.listen(COMPLETION_CHANNEL).await.map_err(|error| {
        ServeError::Internal(format!("could not watch for completion: {error}"))
    })?;

    state
        .queue()
        .push_task(
            TaskBuilder::new(job)
                .max_attempts(MAX_ATTEMPTS)
                .with_idempotency_key(run_id)
                .build(),
        )
        .await
        .map_err(|error| ServeError::Internal(format!("could not queue the run: {error}")))?;
    Ok(done)
}

async fn abandon(state: &AppState, run_id: &str, error: ServeError) -> ServeError {
    if let Err(cleanup) = state
        .store()
        .set_run_status(run_id, RunStatus::Failed, Some(&error.to_string()))
        .await
    {
        tracing::error!(%run_id, %cleanup, "could not release the thread after a failed dispatch");
    }
    error
}

async fn await_completion(
    state: &AppState,
    tenant: &str,
    thread_id: &str,
    run_id: &str,
    mut done: PgListener,
) -> Result<Json<WaitRunResponse>, ServeError> {
    let deadline = tokio::time::Instant::now() + WAIT_TIMEOUT;
    loop {
        if let Some(record) = state.store().get_run(tenant, run_id).await? {
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

        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Err(ServeError::Timeout {
                run_id: run_id.to_string(),
            });
        }
        settle(&mut done, run_id, left.min(RECHECK_EVERY)).await;
    }
}

async fn settle(done: &mut PgListener, run_id: &str, budget: Duration) {
    let until = tokio::time::Instant::now() + budget;
    loop {
        let left = until.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return;
        }
        match tokio::time::timeout(left, done.recv()).await {
            Ok(Ok(note)) if note.payload() == run_id => return,
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::warn!(%run_id, %error, "completion listener dropped, polling instead");
                tokio::time::sleep(left).await;
                return;
            }
            Err(_) => return,
        }
    }
}

async fn collect(
    state: &AppState,
    tenant: &str,
    thread_id: &str,
    run_id: &str,
) -> Result<WaitRunResponse, ServeError> {
    let events = state
        .store()
        .read_run_after(tenant, thread_id, run_id, 0)
        .await?;

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
    Ok(WaitRunResponse {
        run_id: run_id.to_string(),
        text,
        stop_reason: outcome.stop_reason,
        total_turns: outcome.total_turns,
        input_tokens: outcome.usage.input_tokens,
        output_tokens: outcome.usage.output_tokens,
        structured: outcome.structured,
    })
}
