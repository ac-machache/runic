use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use runic_state::{ChildPersistence, ChildPersistenceHandle, ChildSink, PersistSink, SessionEvent};
use runic_substrate::SessionStore;
use tokio::sync::{Mutex, Notify, mpsc, watch};

const CHILD_PERSIST_ATTEMPTS: u32 = 5;
const CHILD_RETRY_BASE: Duration = Duration::from_millis(50);
const CHILD_FLUSH_TIMEOUT: Duration = Duration::from_secs(15);

pub fn child_persistence(
    store: Arc<dyn SessionStore>,
    tenant: &str,
    parent_session: &str,
) -> ChildPersistenceHandle {
    ChildPersistenceHandle(Arc::new(ServeChildPersistence {
        store,
        tenant: tenant.to_string(),
        parent_session: parent_session.to_string(),
    }))
}

pub struct ServeChildPersistence {
    store: Arc<dyn SessionStore>,
    tenant: String,
    parent_session: String,
}

struct Progress {
    enqueued: Arc<AtomicU64>,
    committed: AtomicU64,
    failed: Mutex<Option<String>>,
    notify: Notify,
    /// Held here (not on the sink) so a sink drop never kills a draining
    /// persister; only an explicit `send(true)` on flush timeout stops it.
    stop: watch::Sender<bool>,
}

struct ServeChildSink {
    store: Arc<dyn SessionStore>,
    tenant: String,
    session_id: String,
    sink: PersistSink,
    progress: Arc<Progress>,
}

#[async_trait]
impl ChildPersistence for ServeChildPersistence {
    async fn begin(&self, agent: &str) -> anyhow::Result<Box<dyn ChildSink>> {
        let session_id = format!("chd-{}", uuid::Uuid::new_v4().simple());
        self.store
            .create_child_session(&self.tenant, &session_id, &self.parent_session, agent)
            .await?;

        let (tx, rx) = mpsc::unbounded_channel();
        let sink = PersistSink::new(tx);
        let (stop, stop_rx) = watch::channel(false);
        let progress = Arc::new(Progress {
            enqueued: sink.enqueued(),
            committed: AtomicU64::new(0),
            failed: Mutex::new(None),
            notify: Notify::new(),
            stop,
        });
        spawn_child_persister(
            rx,
            self.store.clone(),
            self.tenant.clone(),
            session_id.clone(),
            progress.clone(),
            stop_rx,
        );
        Ok(Box::new(ServeChildSink {
            store: self.store.clone(),
            tenant: self.tenant.clone(),
            session_id,
            sink,
            progress,
        }))
    }
}

#[async_trait]
impl ChildSink for ServeChildSink {
    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn sink(&self) -> PersistSink {
        self.sink.clone()
    }

    fn nested(&self) -> ChildPersistenceHandle {
        child_persistence(self.store.clone(), &self.tenant, &self.session_id)
    }

    async fn flush(&self) -> anyhow::Result<()> {
        let target = self.progress.enqueued.load(Ordering::SeqCst);
        let deadline = tokio::time::Instant::now() + CHILD_FLUSH_TIMEOUT;
        loop {
            let notified = self.progress.notify.notified();
            if let Some(error) = self.progress.failed.lock().await.clone() {
                anyhow::bail!("child transcript flush failed: {error}");
            }
            if self.progress.committed.load(Ordering::SeqCst) >= target {
                return Ok(());
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                let backlog = target.saturating_sub(self.progress.committed.load(Ordering::SeqCst));
                let message = format!(
                    "child transcript flush timed out after {CHILD_FLUSH_TIMEOUT:?} ({backlog} events unflushed)"
                );
                *self.progress.failed.lock().await = Some(message.clone());
                let _ = self.progress.stop.send(true);
                anyhow::bail!(message);
            }
        }
    }
}

fn spawn_child_persister(
    mut rx: mpsc::UnboundedReceiver<Arc<SessionEvent>>,
    store: Arc<dyn SessionStore>,
    tenant: String,
    session_id: String,
    progress: Arc<Progress>,
    mut stop: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        loop {
            let first = tokio::select! {
                biased;
                _ = stop.changed() => return,
                received = rx.recv() => match received {
                    Some(event) => event,
                    None => return,
                },
            };
            let mut shared = vec![first];
            while let Ok(event) = rx.try_recv() {
                shared.push(event);
            }
            let batch: Vec<SessionEvent> = shared.iter().map(|e| (**e).clone()).collect();
            let batch_size = batch.len() as u64;
            let mut attempt = 0u32;
            loop {
                match store
                    .append_batch_strict(&tenant, &session_id, &batch)
                    .await
                {
                    Ok(()) => {
                        progress.committed.fetch_add(batch_size, Ordering::SeqCst);
                        progress.notify.notify_waiters();
                        break;
                    }
                    Err(e) => {
                        attempt += 1;
                        let gone = matches!(e, runic_substrate::Error::NotFound(_));
                        if gone || attempt >= CHILD_PERSIST_ATTEMPTS {
                            tracing::error!(
                                %tenant, %session_id, batch_size, error = %e,
                                "child transcript persist gave up"
                            );
                            *progress.failed.lock().await = Some(e.to_string());
                            progress.notify.notify_waiters();
                            rx.close();
                            return;
                        }
                        let backoff = CHILD_RETRY_BASE
                            .saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1)));
                        tokio::select! {
                            biased;
                            _ = stop.changed() => return,
                            _ = tokio::time::sleep(backoff) => {}
                        }
                    }
                }
            }
        }
    });
}
