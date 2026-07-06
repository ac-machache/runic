use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use redis::AsyncCommands;
use runic_state::SessionEvent;
use tokio::sync::{Mutex, Notify, broadcast, mpsc};

#[async_trait]
pub trait EventBroker: Send + Sync {
    async fn publish(&self, tenant: &str, thread_id: &str, event: &SessionEvent);
    async fn subscribe(
        &self,
        tenant: &str,
        thread_id: &str,
    ) -> Option<mpsc::UnboundedReceiver<SessionEvent>>;
}

#[async_trait]
pub trait QueueNudge: Send + Sync {
    async fn nudge(&self);
    async fn wait(&self, timeout: Duration);
}

#[derive(Default)]
pub struct LocalNudge {
    notify: Notify,
}

impl LocalNudge {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

#[async_trait]
impl QueueNudge for LocalNudge {
    async fn nudge(&self) {
        self.notify.notify_one();
    }

    async fn wait(&self, timeout: Duration) {
        tokio::select! {
            _ = self.notify.notified() => {}
            _ = tokio::time::sleep(timeout) => {}
        }
    }
}

pub struct RedisBroker {
    client: redis::Client,
    publisher: redis::aio::ConnectionManager,
    prefix: String,
    nudge_key: String,
    waiter: Mutex<Option<redis::aio::ConnectionManager>>,
}

impl RedisBroker {
    pub async fn connect(url: &str) -> Result<Arc<Self>, redis::RedisError> {
        let client = redis::Client::open(url)?;
        let publisher = client.get_connection_manager().await?;
        Ok(Arc::new(Self {
            client,
            publisher,
            prefix: "runic:events".to_string(),
            nudge_key: "runic:queue".to_string(),
            waiter: Mutex::new(None),
        }))
    }

    pub fn with_prefix(self: Arc<Self>, prefix: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            client: self.client.clone(),
            publisher: self.publisher.clone(),
            prefix: prefix.into(),
            nudge_key: self.nudge_key.clone(),
            waiter: Mutex::new(None),
        })
    }

    pub fn with_nudge_key(self: Arc<Self>, key: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            client: self.client.clone(),
            publisher: self.publisher.clone(),
            prefix: self.prefix.clone(),
            nudge_key: key.into(),
            waiter: Mutex::new(None),
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

#[async_trait]
impl QueueNudge for RedisBroker {
    async fn nudge(&self) {
        let mut publisher = self.publisher.clone();
        let result: Result<(), _> = redis::pipe()
            .lpush(&self.nudge_key, 1)
            .ltrim(&self.nudge_key, 0, 1023)
            .query_async(&mut publisher)
            .await;
        if let Err(e) = result {
            tracing::warn!(key = %self.nudge_key, error = %e, "queue nudge failed");
        }
    }

    async fn wait(&self, timeout: Duration) {
        let mut guard = self.waiter.lock().await;
        if guard.is_none() {
            match self.client.get_connection_manager().await {
                Ok(conn) => *guard = Some(conn),
                Err(e) => {
                    drop(guard);
                    tracing::warn!(key = %self.nudge_key, error = %e, "nudge wait failed");
                    tokio::time::sleep(timeout).await;
                    return;
                }
            }
        }
        let conn = guard.as_mut().expect("waiter connection just set");
        let seconds = timeout.as_secs_f64().max(0.1);
        match conn
            .blpop::<_, Option<(String, String)>>(&self.nudge_key, seconds)
            .await
        {
            Ok(_) => {}
            Err(e) => {
                *guard = None;
                drop(guard);
                tracing::warn!(key = %self.nudge_key, error = %e, "nudge wait failed");
                tokio::time::sleep(timeout).await;
            }
        }
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
