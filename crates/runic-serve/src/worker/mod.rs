mod listener;
mod liveness;
mod poller;
mod runner;
mod tracker;

use std::sync::Arc;
use std::time::Duration;

use tracker::Tracker;

use crate::app::AppState;

pub const MIN_RUNS: usize = 64;
pub const MAX_RUNS: usize = 64;

const DRAIN_POLL: Duration = Duration::from_millis(100);

pub struct Worker {
    state: AppState,
    id: String,
    tracker: Arc<Tracker>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

pub fn spawn(state: AppState) -> Worker {
    let id = uuid::Uuid::new_v4().to_string();
    let tracker = Arc::new(Tracker::new(MIN_RUNS, MAX_RUNS));
    let tasks = vec![
        listener::watch(state.pool.clone(), Arc::clone(&tracker)),
        poller::spawn(state.clone(), Arc::clone(&tracker), id.clone()),
        liveness::spawn_heartbeat(state.clone(), Arc::clone(&tracker), id.clone()),
        liveness::spawn_reclaimer(state.clone(), Arc::clone(&tracker), id.clone()),
    ];
    tracing::info!(worker_id = %id, min = MIN_RUNS, max = MAX_RUNS, "run worker started");
    Worker {
        state,
        id,
        tracker,
        tasks,
    }
}

impl Worker {
    pub fn detach(self) {}

    pub async fn shutdown(self, grace: Duration) {
        for task in &self.tasks {
            task.abort();
        }
        let deadline = tokio::time::Instant::now() + grace;
        while !self.tracker.live_ids().is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(DRAIN_POLL).await;
        }
        liveness::release(&self.state, &self.id).await;
    }
}
