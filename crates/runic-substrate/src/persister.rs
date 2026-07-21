use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use runic_state::{AgentEvent, Emitter, SubRun, SubSession};
use tokio::sync::{Notify, mpsc};

use crate::{SessionEvent, SessionStore, project};

const MAX_APPEND_RETRIES: u32 = 5;

pub struct PersistHandle {
    enqueued: Arc<AtomicU64>,
    committed: Arc<AtomicU64>,
    error: Arc<Mutex<Option<String>>>,
    notify: Arc<Notify>,
}

impl PersistHandle {
    pub async fn flush(&self) -> anyhow::Result<()> {
        let target = self.enqueued.load(Ordering::SeqCst);
        loop {
            let notified = self.notify.notified();
            if let Some(err) = self.error.lock().unwrap().clone() {
                anyhow::bail!("persist failed: {err}");
            }
            if self.committed.load(Ordering::SeqCst) >= target {
                return Ok(());
            }
            notified.await;
        }
    }

    pub fn backlog(&self) -> u64 {
        self.enqueued
            .load(Ordering::SeqCst)
            .saturating_sub(self.committed.load(Ordering::SeqCst))
    }
}

#[derive(Debug)]
struct PersistEmitter {
    tx: mpsc::UnboundedSender<SessionEvent>,
    enqueued: Arc<AtomicU64>,
}

impl Emitter for PersistEmitter {
    fn emit(&self, event: AgentEvent) {
        if let Some(se) = project(&event) {
            self.enqueued.fetch_add(1, Ordering::SeqCst);
            let _ = self.tx.send(se);
        }
    }
}

pub fn attach(
    store: Arc<dyn SessionStore>,
    tenant: String,
    session_id: String,
) -> (Arc<dyn Emitter>, PersistHandle) {
    let (tx, rx) = mpsc::unbounded_channel::<SessionEvent>();
    let enqueued = Arc::new(AtomicU64::new(0));
    let committed = Arc::new(AtomicU64::new(0));
    let error = Arc::new(Mutex::new(None));
    let notify = Arc::new(Notify::new());
    spawn_drain(
        rx,
        store,
        tenant,
        session_id,
        committed.clone(),
        error.clone(),
        notify.clone(),
    );
    let emitter: Arc<dyn Emitter> = Arc::new(PersistEmitter {
        tx,
        enqueued: enqueued.clone(),
    });
    (
        emitter,
        PersistHandle {
            enqueued,
            committed,
            error,
            notify,
        },
    )
}

fn spawn_drain(
    mut rx: mpsc::UnboundedReceiver<SessionEvent>,
    store: Arc<dyn SessionStore>,
    tenant: String,
    session_id: String,
    committed: Arc<AtomicU64>,
    error: Arc<Mutex<Option<String>>>,
    notify: Arc<Notify>,
) {
    tokio::spawn(async move {
        while let Some(first) = rx.recv().await {
            let mut batch = vec![first];
            while let Ok(event) = rx.try_recv() {
                batch.push(event);
            }
            let count = batch.len() as u64;
            let mut attempt = 0u32;
            loop {
                match store.append_batch(&tenant, &session_id, &batch).await {
                    Ok(()) => {
                        committed.fetch_add(count, Ordering::SeqCst);
                        notify.notify_waiters();
                        break;
                    }
                    Err(err) => {
                        attempt += 1;
                        if attempt >= MAX_APPEND_RETRIES {
                            *error.lock().unwrap() = Some(err.to_string());
                            notify.notify_waiters();
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(50 * attempt as u64)).await;
                    }
                }
            }
        }
    });
}

pub struct StoreSubSession {
    store: Arc<dyn SessionStore>,
    tenant: String,
    parent: String,
}

impl StoreSubSession {
    pub fn new(store: Arc<dyn SessionStore>, tenant: String, parent: String) -> Self {
        Self {
            store,
            tenant,
            parent,
        }
    }
}

#[async_trait]
impl SubSession for StoreSubSession {
    async fn begin(&self, agent: &str) -> anyhow::Result<Box<dyn SubRun>> {
        let child = format!(
            "{}::{}::{}",
            self.parent,
            agent,
            uuid::Uuid::new_v4().simple()
        );
        let (emitter, handle) = attach(self.store.clone(), self.tenant.clone(), child.clone());
        Ok(Box::new(StoreSubRun {
            store: self.store.clone(),
            tenant: self.tenant.clone(),
            session_id: child,
            emitter,
            handle,
        }))
    }
}

struct StoreSubRun {
    store: Arc<dyn SessionStore>,
    tenant: String,
    session_id: String,
    emitter: Arc<dyn Emitter>,
    handle: PersistHandle,
}

#[async_trait]
impl SubRun for StoreSubRun {
    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn emitter(&self) -> Arc<dyn Emitter> {
        self.emitter.clone()
    }

    fn nested(&self) -> Arc<dyn SubSession> {
        Arc::new(StoreSubSession::new(
            self.store.clone(),
            self.tenant.clone(),
            self.session_id.clone(),
        ))
    }

    async fn flush(&self) -> anyhow::Result<()> {
        self.handle.flush().await
    }
}
