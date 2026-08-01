use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::broadcast;

use super::keys;

const NUDGE_BACKLOG: usize = 1024;
const REATTACH_AFTER: Duration = Duration::from_secs(1);

pub struct Wake {
    nudges: broadcast::Sender<String>,
}

impl Wake {
    pub fn spawn(client: redis::Client) -> Arc<Self> {
        let (nudges, _) = broadcast::channel(NUDGE_BACKLOG);
        let wake = Arc::new(Self {
            nudges: nudges.clone(),
        });
        tokio::spawn(async move {
            loop {
                match listen(&client, &nudges).await {
                    Ok(()) => tracing::warn!(
                        channel = keys::CHANNEL,
                        "event listener dropped, reattaching"
                    ),
                    Err(error) => {
                        tracing::warn!(%error, channel = keys::CHANNEL, "event listener could not attach");
                    }
                }
                tokio::time::sleep(REATTACH_AFTER).await;
            }
        });
        wake
    }

    pub fn watch(&self) -> broadcast::Receiver<String> {
        self.nudges.subscribe()
    }
}

async fn listen(
    client: &redis::Client,
    nudges: &broadcast::Sender<String>,
) -> redis::RedisResult<()> {
    let mut pubsub = client.get_async_pubsub().await?;
    pubsub.subscribe(keys::CHANNEL).await?;
    tracing::info!(channel = keys::CHANNEL, "event listener attached");
    let mut messages = pubsub.on_message();
    while let Some(message) = messages.next().await {
        if let Ok(run_id) = message.get_payload::<String>() {
            let _ = nudges.send(run_id);
        }
    }
    Ok(())
}
