use std::sync::Arc;

use runic_types::Message;

use crate::app::AppState;
use crate::completion;
use crate::routes::runs::input::{input_from_message, with_context};
use crate::store::{ClaimedRun, RunStatus};
use crate::stream::RunEmitter;

pub async fn execute(state: AppState, run: ClaimedRun) {
    let (status, failure) = match turn(&state, &run).await {
        Ok(settled) => settled,
        Err(error) => (RunStatus::Failed, Some(error)),
    };

    match state
        .runs()
        .finish(&run.run_id, status, failure.as_deref())
        .await
    {
        Ok(_) => match &failure {
            None => tracing::info!(
                tenant = %run.tenant, thread_id = %run.session_id, agent = %run.agent,
                run_id = %run.run_id, status = status.as_str(), "run finished"
            ),
            Some(error) => tracing::error!(
                tenant = %run.tenant, thread_id = %run.session_id, agent = %run.agent,
                run_id = %run.run_id, %error, "run failed"
            ),
        },
        Err(error) => {
            tracing::error!(run_id = %run.run_id, %error, "could not record the run outcome");
        }
    }

    state.events.finish(&run.run_id);
    announce(&state, &run.run_id).await;
}

async fn turn(state: &AppState, run: &ClaimedRun) -> Result<(RunStatus, Option<String>), String> {
    let hosted = state
        .agents
        .get(&run.agent)
        .map_err(|error| error.to_string())?
        .clone();

    let payload = run
        .input
        .clone()
        .ok_or_else(|| "the run carries no turn to send".to_string())?;
    let message: Message =
        serde_json::from_value(payload).map_err(|error| format!("unreadable turn: {error}"))?;

    let context = run.context.clone().unwrap_or(serde_json::Value::Null);
    let input = with_context(
        input_from_message(state, &run.tenant, &run.session_id, message)
            .await
            .map_err(|error| error.to_string())?,
        &context,
    )
    .agent_name(&run.agent)
    .run_id(&run.run_id)
    .mode("queued")
    .events(Arc::new(RunEmitter::new(
        &run.run_id,
        Arc::clone(&state.events),
    )));

    let outcome = state
        .thread(&run.tenant, &run.session_id)
        .invoke(&hosted.agent, input)
        .await;

    Ok(match outcome {
        Ok(done) => match done.outcome.stop_reason.as_deref() {
            Some("cancelled") => (RunStatus::Cancelled, None),
            Some("suspended") => (RunStatus::Waiting, None),
            _ => (RunStatus::Successful, None),
        },
        Err(error) => (RunStatus::Failed, Some(error.to_string())),
    })
}

async fn announce(state: &AppState, run_id: &str) {
    if let Err(error) = sqlx::query("SELECT pg_notify($1, $2)")
        .bind(completion::CHANNEL)
        .bind(run_id)
        .execute(&state.pool)
        .await
    {
        tracing::warn!(%run_id, %error, "could not announce run completion");
    }
}
