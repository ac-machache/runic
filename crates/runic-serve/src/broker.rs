use std::sync::Arc;

use async_trait::async_trait;
use redis::AsyncCommands;
use runic_state::SessionEvent;
use tokio::sync::{broadcast, mpsc};

#[async_trait]
pub trait EventBroker: Send + Sync {
    async fn publish(&self, tenant: &str, thread_id: &str, event: &SessionEvent);
    async fn subscribe(
        &self,
        tenant: &str,
        thread_id: &str,
    ) -> Option<mpsc::UnboundedReceiver<SessionEvent>>;
}

pub struct RedisBroker {
    client: redis::Client,
    publisher: redis::aio::ConnectionManager,
    prefix: String,
}

impl RedisBroker {
    pub async fn connect(url: &str) -> Result<Arc<Self>, redis::RedisError> {
        let client = redis::Client::open(url)?;
        let publisher = client.get_connection_manager().await?;
        Ok(Arc::new(Self {
            client,
            publisher,
            prefix: "runic:events".to_string(),
        }))
    }

    pub fn with_prefix(self: Arc<Self>, prefix: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            client: self.client.clone(),
            publisher: self.publisher.clone(),
            prefix: prefix.into(),
        })
    }

    fn channel(&self, tenant: &str, thread_id: &str) -> String {
        format!(
            "{}:{}:{}",
            self.prefix,
            escape_segment(tenant),
            escape_segment(thread_id)
        )
    }
}

fn escape_segment(value: &str) -> String {
    value.replace('%', "%25").replace(':', "%3A")
}

#[async_trait]
impl EventBroker for RedisBroker {
    async fn publish(&self, tenant: &str, thread_id: &str, event: &SessionEvent) {
        let Ok(payload) = serde_json::to_string(event) else {
            return;
        };
        let channel = self.channel(tenant, thread_id);
        let mut publisher = self.publisher.clone();
        if let Err(e) = publisher.publish::<_, _, ()>(&channel, payload).await {
            tracing::warn!(%channel, error = %e, "broker publish failed");
        }
    }

    async fn subscribe(
        &self,
        tenant: &str,
        thread_id: &str,
    ) -> Option<mpsc::UnboundedReceiver<SessionEvent>> {
        let channel = self.channel(tenant, thread_id);
        let mut pubsub = match self.client.get_async_pubsub().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(%channel, error = %e, "broker subscribe failed");
                return None;
            }
        };
        if let Err(e) = pubsub.subscribe(&channel).await {
            tracing::warn!(%channel, error = %e, "broker subscribe failed");
            return None;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            use futures::StreamExt;
            let mut stream = pubsub.on_message();
            while let Some(msg) = stream.next().await {
                let Ok(payload) = msg.get_payload::<String>() else {
                    continue;
                };
                let Ok(event) = serde_json::from_str::<SessionEvent>(&payload) else {
                    continue;
                };
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        Some(rx)
    }
}

pub(crate) fn spawn_broker_forwarder(
    broker: Arc<dyn EventBroker>,
    tenant: String,
    thread_id: String,
    mut events: broadcast::Receiver<Arc<SessionEvent>>,
) {
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    let is_end = matches!(event.as_ref(), SessionEvent::RunEnd { .. });
                    broker.publish(&tenant, &thread_id, &event).await;
                    if is_end {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(%tenant, %thread_id, missed, "broker forwarder lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}
