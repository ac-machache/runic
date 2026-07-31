use apalis::layers::retry::RetryPolicy;
use apalis::layers::retry::backoff::{ExponentialBackoffMaker, MakeBackoff};
use apalis::prelude::{
    BoxDynError, Data, ParallelizeExt, TaskBuilder, TaskSink, WorkerBuilder, WorkerBuilderExt,
};
use apalis_postgres::PgPool;
use apalis_sql::ext::TaskBuilderExt;
use runic_substrate::RunStatus;

use crate::app::AppState;
use crate::queue::{DEFER_AFTER, MAX_ATTEMPTS, MAX_WAVES, RunJob, TURN_POLL, TURN_TRIES, listener};
use crate::routes::runs::input::{input_from_message, with_context};

pub const COMPLETION_CHANNEL: &str = "runic_run_done";

#[derive(Debug, thiserror::Error)]
#[error("store unavailable: {0}")]
pub struct Transient(String);

fn is_transient(error: &BoxDynError) -> bool {
    error.downcast_ref::<Transient>().is_some()
}

pub async fn run_job(job: RunJob, state: Data<AppState>) -> Result<(), BoxDynError> {
    let store = state.store();
    let hosted = state.agents.get(&job.agent)?.clone();

    if !take_turn(&state, &job).await? {
        return defer(&state, job).await;
    }

    let RunJob {
        tenant,
        thread_id,
        run_id,
        agent,
        message,
        context,
        ..
    } = job;

    let context = context.unwrap_or(serde_json::Value::Null);
    let input = with_context(
        input_from_message(&state, &tenant, &thread_id, message).await?,
        &context,
    )
    .agent_name(&agent)
    .run_id(&run_id)
    .mode("queued");

    let outcome = state
        .thread(&tenant, &thread_id)
        .invoke(&hosted.agent, input)
        .await;

    let (status, failure) = match &outcome {
        Ok(out) if out.outcome.stop_reason.as_deref() == Some("cancelled") => {
            (RunStatus::Cancelled, None)
        }
        Ok(out) if out.outcome.stop_reason.as_deref() == Some("suspended") => {
            (RunStatus::Waiting, None)
        }
        Ok(_) => (RunStatus::Successful, None),
        Err(error) => (RunStatus::Failed, Some(error.to_string())),
    };
    store
        .set_run_status(&run_id, status, failure.as_deref())
        .await
        .map_err(|error| Transient(error.to_string()))?;
    announce(&state.pool, &run_id).await;

    match &outcome {
        Ok(_) => {
            tracing::info!(%tenant, %thread_id, %agent, %run_id, status = status.as_str(), "run finished")
        }
        Err(error) => {
            tracing::error!(%tenant, %thread_id, %agent, %run_id, %error, "run failed")
        }
    }

    Ok(())
}

async fn take_turn(state: &AppState, job: &RunJob) -> Result<bool, BoxDynError> {
    let store = state.store();
    for _ in 0..TURN_TRIES {
        let started = store
            .try_start_run(&job.tenant, &job.run_id)
            .await
            .map_err(|error| Transient(error.to_string()))?;
        if started {
            return Ok(true);
        }
        tokio::time::sleep(TURN_POLL).await;
    }
    Ok(false)
}

async fn defer(state: &AppState, job: RunJob) -> Result<(), BoxDynError> {
    if job.wave >= MAX_WAVES {
        tracing::error!(
            tenant = %job.tenant, thread_id = %job.thread_id, run_id = %job.run_id,
            waves = job.wave, "gave up waiting for the thread"
        );
        state
            .store()
            .set_run_status(
                &job.run_id,
                RunStatus::Failed,
                Some("the thread never freed up"),
            )
            .await
            .map_err(|error| Transient(error.to_string()))?;
        announce(&state.pool, &job.run_id).await;
        return Ok(());
    }

    let next = RunJob {
        wave: job.wave + 1,
        ..job
    };
    tracing::info!(
        tenant = %next.tenant, thread_id = %next.thread_id, run_id = %next.run_id,
        wave = next.wave, "thread busy, deferring"
    );
    let key = next.key();
    state
        .queue()
        .push_task(
            TaskBuilder::new(next)
                .run_after(DEFER_AFTER)
                .max_attempts(MAX_ATTEMPTS)
                .with_idempotency_key(&key)
                .build(),
        )
        .await
        .map_err(|error| Transient(error.to_string()))?;
    Ok(())
}

async fn announce(pool: &PgPool, run_id: &str) {
    if let Err(error) = sqlx::query("SELECT pg_notify($1, $2)")
        .bind(COMPLETION_CHANNEL)
        .bind(run_id)
        .execute(pool)
        .await
    {
        tracing::warn!(%run_id, %error, "could not announce run completion");
    }
}

pub fn spawn_run_worker(state: AppState, pool: &PgPool) -> tokio::task::JoinHandle<()> {
    let backoff = ExponentialBackoffMaker::default().make_backoff();
    let worker = WorkerBuilder::new("runic-runs")
        .backend(listener(pool))
        .data(state)
        .retry(
            RetryPolicy::retries(MAX_ATTEMPTS as usize)
                .with_backoff(backoff)
                .retry_if(is_transient),
        )
        .parallelize(tokio::spawn)
        .build(run_job);
    tokio::spawn(async move {
        if let Err(error) = worker.run().await {
            tracing::error!(%error, "run worker stopped");
        }
    })
}
