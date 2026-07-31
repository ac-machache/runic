use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::runner;
use super::tracker::Tracker;
use crate::app::AppState;

const IDLE_CEILING: Duration = Duration::from_secs(30);
const CLAIM_BACKOFF: Duration = Duration::from_secs(1);

struct Slot {
    tracker: Arc<Tracker>,
    run_id: String,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.tracker.finished(&self.run_id);
    }
}

pub fn spawn(
    state: AppState,
    tracker: Arc<Tracker>,
    worker_id: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(poll_loop(state, tracker, worker_id))
}

async fn poll_loop(state: AppState, tracker: Arc<Tracker>, worker_id: String) {
    loop {
        let Some(batch) = tracker.next_batch_size() else {
            tracker.woken().await;
            continue;
        };

        let claim = match state.runs().claim(&worker_id, batch as i64).await {
            Ok(claim) => claim,
            Err(error) => {
                tracing::warn!(%error, "could not claim runs");
                tokio::time::sleep(CLAIM_BACKOFF).await;
                continue;
            }
        };

        if claim.runs.is_empty() {
            let nap = claim.next_due.map_or(IDLE_CEILING, until).min(IDLE_CEILING);
            let _ = tokio::time::timeout(nap, tracker.woken()).await;
            continue;
        }

        for run in claim.runs {
            tracker.started(&run.run_id);
            let slot = Slot {
                tracker: Arc::clone(&tracker),
                run_id: run.run_id.clone(),
            };
            let state = state.clone();
            tokio::spawn(async move {
                let _slot = slot;
                runner::execute(state, run).await;
            });
        }
    }
}

fn until(at: DateTime<Utc>) -> Duration {
    (at - Utc::now()).to_std().unwrap_or(Duration::ZERO)
}
