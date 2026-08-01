use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::PgListener;

use super::tracker::Tracker;
use crate::app::AppState;
use crate::store::SIGNAL_CHANNEL;

const REATTACH_AFTER: Duration = Duration::from_secs(1);

pub fn watch(state: AppState, tracker: Arc<Tracker>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match attach(&state).await {
                Ok(mut listener) => {
                    tracing::info!(channel = SIGNAL_CHANNEL, "signal listener attached");
                    sweep(&state, &tracker).await;
                    while let Ok(note) = listener.recv().await {
                        apply(&state, &tracker, note.payload()).await;
                    }
                    tracing::warn!(
                        channel = SIGNAL_CHANNEL,
                        "signal listener dropped, reattaching"
                    );
                }
                Err(error) => {
                    tracing::warn!(%error, channel = SIGNAL_CHANNEL, "signal listener could not attach");
                }
            }
            tokio::time::sleep(REATTACH_AFTER).await;
        }
    })
}

async fn attach(state: &AppState) -> Result<PgListener, sqlx::Error> {
    let mut listener = PgListener::connect_with(&state.pool).await?;
    listener.listen(SIGNAL_CHANNEL).await?;
    Ok(listener)
}

async fn sweep(state: &AppState, tracker: &Tracker) {
    for run_id in tracker.live_ids() {
        apply(state, tracker, &run_id).await;
    }
}

async fn apply(state: &AppState, tracker: &Tracker, run_id: &str) {
    if !tracker.holds(run_id) {
        return;
    }
    match state.runs().take_signals(run_id).await {
        Ok(Some(signals)) => {
            if signals.to_cancel && tracker.cancel(run_id) {
                tracing::info!(%run_id, "cancel requested");
            }
            for text in &signals.steering {
                if tracker.steer(run_id, text) {
                    tracing::info!(%run_id, "steering delivered");
                }
            }
        }
        Ok(None) => {}
        Err(error) => tracing::warn!(%run_id, %error, "could not read the run's signals"),
    }
}
