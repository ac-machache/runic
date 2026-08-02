use std::sync::Arc;

use runic::types::Message;
use runic::{CancelToken, Input};
use tokio::sync::mpsc;

use crate::app::AppState;
use crate::completion;
use crate::hook::FinishedRun;
use crate::routes::runs::input::{input_from_message, with_context};
use crate::store::{ClaimedRun, RunStatus};
use crate::stream::RunEmitter;

pub async fn execute(
    state: AppState,
    run: ClaimedRun,
    cancel: CancelToken,
    steering: mpsc::UnboundedReceiver<String>,
) {
    let (status, failure, output) = match turn(&state, &run, cancel, steering).await {
        Ok(settled) => settled,
        Err(error) => (RunStatus::Failed, Some(error), None),
    };
    let session = run.session_id.as_deref().unwrap_or("-");

    match state
        .runs()
        .finish(&run.run_id, status, failure.as_deref(), output.as_ref())
        .await
    {
        Ok(_) => match &failure {
            None => tracing::info!(
                tenant = %run.tenant, session_id = %session, agent = %run.agent,
                run_id = %run.run_id, status = status.as_str(), "run finished"
            ),
            Some(error) => tracing::error!(
                tenant = %run.tenant, session_id = %session, agent = %run.agent,
                run_id = %run.run_id, %error, "run failed"
            ),
        },
        Err(error) => {
            tracing::error!(run_id = %run.run_id, %error, "could not record the run outcome");
        }
    }

    state.events.finish(&run.run_id);
    announce(&state, &run.run_id).await;

    if status.is_terminal() {
        notify(&state, &run, status, failure, output).await;
    }
}

async fn notify(
    state: &AppState,
    run: &ClaimedRun,
    status: RunStatus,
    error: Option<String>,
    output: Option<serde_json::Value>,
) {
    let Some(name) = run.hook.as_deref() else {
        return;
    };
    let Some(hook) = state.hooks.get(name) else {
        tracing::error!(run_id = %run.run_id, hook = name, "no such run hook is registered");
        return;
    };
    let finished = FinishedRun {
        tenant: run.tenant.clone(),
        run_id: run.run_id.clone(),
        session_id: run.session_id.clone(),
        agent: run.agent.clone(),
        status,
        error,
        output: output
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default(),
    };
    crate::hook::fire(hook, name, finished).await;
}

async fn turn(
    state: &AppState,
    run: &ClaimedRun,
    cancel: CancelToken,
    steering: mpsc::UnboundedReceiver<String>,
) -> Result<(RunStatus, Option<String>, Option<serde_json::Value>), String> {
    let hosted = state
        .agents
        .get(&run.agent)
        .map_err(|error| error.to_string())?
        .clone();

    let scratch = format!("run-{}", run.run_id);
    let session_id = run.session_id.as_deref().unwrap_or(&scratch);

    let fresh = run.input.clone();
    let carries_turn = fresh.is_some();
    let turn = match fresh {
        Some(payload) => {
            let message: Message = serde_json::from_value(payload)
                .map_err(|error| format!("unreadable turn: {error}"))?;
            input_from_message(state, &run.tenant, session_id, message)
                .await
                .map_err(|error| error.to_string())?
        }
        None => Input::new().answer(
            run.answer
                .clone()
                .ok_or_else(|| "a resumed run carries no answer".to_string())?,
        ),
    };

    let context = run.context.clone().unwrap_or(serde_json::Value::Null);
    let input = with_context(turn, &context)
        .agent_name(&run.agent)
        .run_id(&run.run_id)
        .mode("queued")
        .cancel(cancel)
        .steering(steering)
        .events(Arc::new(RunEmitter::new(
            &run.run_id,
            Arc::clone(&state.events),
        )));

    let session = match run.session_id.is_some() {
        true => state.session(&run.tenant, session_id),
        false => state.scratch(&run.tenant, session_id),
    };
    let outcome = match carries_turn {
        true => session.invoke(&hosted.agent, input).await,
        false => session.resume(&hosted.agent, input).await,
    };

    Ok(match outcome {
        Ok(done) => {
            let status = match done.outcome.stop_reason.as_deref() {
                Some("cancelled") => RunStatus::Cancelled,
                Some("suspended") => RunStatus::Waiting,
                _ => RunStatus::Successful,
            };
            (status, None, answer_of(&done))
        }
        Err(error) => (RunStatus::Failed, Some(error.to_string()), None),
    })
}

fn answer_of(done: &runic::AgentOutput) -> Option<serde_json::Value> {
    serde_json::to_value(crate::store::RunOutput::from(done)).ok()
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
