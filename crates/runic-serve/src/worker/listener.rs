use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::PgListener;

use super::tracker::Tracker;

pub const CHANNEL: &str = "runic_claimable";

const REATTACH_AFTER: Duration = Duration::from_secs(1);

pub fn watch(pool: PgPool, tracker: Arc<Tracker>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match attach(&pool).await {
                Ok(mut listener) => {
                    tracing::info!(channel = CHANNEL, "ready listener attached");
                    tracker.wake();
                    while listener.recv().await.is_ok() {
                        tracker.wake();
                    }
                    tracing::warn!(channel = CHANNEL, "ready listener dropped, reattaching");
                }
                Err(error) => {
                    tracing::warn!(%error, channel = CHANNEL, "ready listener could not attach");
                }
            }
            tracker.wake();
            tokio::time::sleep(REATTACH_AFTER).await;
        }
    })
}

async fn attach(pool: &PgPool) -> Result<PgListener, sqlx::Error> {
    let mut listener = PgListener::connect_with(pool).await?;
    listener.listen(CHANNEL).await?;
    Ok(listener)
}
