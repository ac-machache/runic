use std::time::Duration;

use chrono::Utc;

use super::context::RoutineContext;
use crate::app::AppState;
use crate::store::{DueRoutine, next_after};

const BATCH: i64 = 16;
const IDLE_CEILING: Duration = Duration::from_secs(5);
const CLAIM_BACKOFF: Duration = Duration::from_secs(5);
const ROUTINE_TIMEOUT: Duration = Duration::from_secs(900);
const RECLAIM_EVERY: Duration = Duration::from_secs(300);
const STALE_AFTER: f64 = 1800.0;

pub fn spawn(state: AppState) -> Vec<tokio::task::JoinHandle<()>> {
    vec![
        tokio::spawn(sweep(state.clone())),
        tokio::spawn(reclaim(state)),
    ]
}

async fn sweep(state: AppState) {
    loop {
        let due = match state.schedules().claim_due(BATCH).await {
            Ok(due) => due,
            Err(error) => {
                tracing::warn!(%error, "could not claim due schedules");
                tokio::time::sleep(CLAIM_BACKOFF).await;
                continue;
            }
        };

        if due.is_empty() {
            tokio::time::sleep(nap(&state).await).await;
            continue;
        }

        for schedule in due {
            tokio::spawn(fire(state.clone(), schedule));
        }
    }
}

async fn nap(state: &AppState) -> Duration {
    state
        .schedules()
        .next_due()
        .await
        .ok()
        .flatten()
        .map(|at| (at - Utc::now()).to_std().unwrap_or(Duration::ZERO))
        .unwrap_or(IDLE_CEILING)
        .min(IDLE_CEILING)
}

async fn fire(state: AppState, schedule: DueRoutine) {
    let failure = run(&state, &schedule).await;

    match &failure {
        None => tracing::info!(
            schedule_id = %schedule.schedule_id,
            routine = %schedule.routine,
            "routine fired"
        ),
        Some(error) => tracing::error!(
            schedule_id = %schedule.schedule_id,
            routine = %schedule.routine,
            %error,
            "routine failed"
        ),
    }

    let next_at = match next_after(&schedule.cron, &schedule.tz, Utc::now()) {
        Ok(next) => next,
        Err(error) => {
            tracing::error!(
                schedule_id = %schedule.schedule_id, %error,
                "no further occurrence; disabling the schedule"
            );
            let _ = state
                .schedules()
                .set_enabled(&schedule.schedule_id, false)
                .await;
            Utc::now()
        }
    };

    if let Err(error) = state
        .schedules()
        .settle(&schedule.schedule_id, next_at, failure.as_deref())
        .await
    {
        tracing::error!(
            schedule_id = %schedule.schedule_id, %error,
            "could not settle a schedule; it stays held until the reclaim sweep"
        );
    }
}

async fn run(state: &AppState, schedule: &DueRoutine) -> Option<String> {
    let Some(routine) = state.routines.get(&schedule.routine).cloned() else {
        return Some(format!(
            "no routine named {:?} is registered; known: {:?}",
            schedule.routine,
            state.routines.names()
        ));
    };

    let ctx = RoutineContext::new(
        state.clone(),
        schedule.schedule_id.clone(),
        schedule.tenant.clone(),
        schedule.payload.clone(),
    );

    let fired = tokio::spawn(async move { routine.run(&ctx).await });
    match tokio::time::timeout(ROUTINE_TIMEOUT, fired).await {
        Ok(Ok(Ok(()))) => None,
        Ok(Ok(Err(error))) => Some(error.to_string()),
        Ok(Err(panicked)) => Some(format!("routine panicked: {panicked}")),
        Err(_) => Some(format!(
            "routine timed out after {}s",
            ROUTINE_TIMEOUT.as_secs()
        )),
    }
}

async fn reclaim(state: AppState) {
    loop {
        tokio::time::sleep(RECLAIM_EVERY).await;
        match state.schedules().reclaim(STALE_AFTER).await {
            Ok(0) => {}
            Ok(count) => tracing::warn!(count, "reclaimed schedules whose instance went away"),
            Err(error) => tracing::warn!(%error, "could not reclaim schedules"),
        }
    }
}
