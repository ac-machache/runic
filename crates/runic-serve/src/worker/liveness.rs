use std::sync::Arc;
use std::time::Duration;

use super::tracker::Tracker;
use crate::app::AppState;

pub const STALE_AFTER: Duration = Duration::from_secs(300);

const HEARTBEAT_EVERY: Duration = Duration::from_secs(75);
const SWEEP_EVERY: Duration = Duration::from_secs(30);
const RECLAIM_DELAY: Duration = Duration::from_secs(5);

pub fn spawn_heartbeat(
    state: AppState,
    tracker: Arc<Tracker>,
    worker_id: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(HEARTBEAT_EVERY).await;
            let live = tracker.live_ids();
            if live.is_empty() {
                continue;
            }
            if let Err(error) = state.runs().heartbeat(&worker_id, &live).await {
                tracing::warn!(%error, held = live.len(), "could not refresh the run heartbeat");
            }
        }
    })
}

pub fn spawn_reclaimer(
    state: AppState,
    tracker: Arc<Tracker>,
    worker_id: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SWEEP_EVERY).await;
            let live = tracker.live_ids();
            let swept = state
                .runs()
                .reclaim(
                    &worker_id,
                    &live,
                    STALE_AFTER.as_secs_f64(),
                    RECLAIM_DELAY.as_secs_f64(),
                )
                .await;
            match swept {
                Ok(recovered) => {
                    for (run_id, status) in recovered {
                        tracing::warn!(
                            %run_id, status = status.as_str(),
                            "reclaimed a run whose worker went away"
                        );
                    }
                }
                Err(error) => tracing::warn!(%error, "could not sweep for lost runs"),
            }
        }
    })
}

pub async fn release(state: &AppState, worker_id: &str) {
    match state.runs().release_all(worker_id).await {
        Ok(released) if !released.is_empty() => {
            tracing::info!(
                count = released.len(),
                "released in-flight runs on shutdown"
            );
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "could not release in-flight runs on shutdown"),
    }
}
