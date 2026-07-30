use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use runic_state::{AgentEvent, Emitter, SubRun, SubSession};
use tokio::sync::{Notify, mpsc};
use tracing::Instrument;

use crate::{SessionEvent, SessionStore, project};

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_retries: Option<u32>,
    pub base: Duration,
    pub cap: Duration,
}

impl RetryPolicy {
    pub fn bounded() -> Self {
        Self {
            max_retries: Some(5),
            base: Duration::from_millis(50),
            cap: Duration::from_secs(5),
        }
    }

    pub fn forever() -> Self {
        Self {
            max_retries: None,
            base: Duration::from_millis(100),
            cap: Duration::from_secs(5),
        }
    }
}

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

/// The write end of a persist channel: hand it already-projected
/// [`SessionEvent`]s. `Session` reaches it through a [`PersistEmitter`];
/// serve feeds it straight from its event tee.
#[derive(Debug, Clone)]
pub struct PersistDrain {
    tx: mpsc::UnboundedSender<Arc<SessionEvent>>,
    enqueued: Arc<AtomicU64>,
}

impl PersistDrain {
    pub fn send(&self, event: Arc<SessionEvent>) {
        self.enqueued.fetch_add(1, Ordering::SeqCst);
        let _ = self.tx.send(event);
    }
}

/// The read end plus commit bookkeeping, consumed once by [`spawn_persist`].
/// Split from [`PersistDrain`] so a caller can register the drain before it
/// has the store to spawn against (serve builds the live run, then hydrates).
pub struct PersistPipe {
    rx: mpsc::UnboundedReceiver<Arc<SessionEvent>>,
    committed: Arc<AtomicU64>,
    error: Arc<Mutex<Option<String>>>,
    notify: Arc<Notify>,
}

pub fn persist_channel() -> (PersistDrain, PersistPipe, Arc<PersistHandle>) {
    let (tx, rx) = mpsc::unbounded_channel::<Arc<SessionEvent>>();
    let enqueued = Arc::new(AtomicU64::new(0));
    let committed = Arc::new(AtomicU64::new(0));
    let error = Arc::new(Mutex::new(None));
    let notify = Arc::new(Notify::new());
    let drain = PersistDrain {
        tx,
        enqueued: enqueued.clone(),
    };
    let pipe = PersistPipe {
        rx,
        committed: committed.clone(),
        error: error.clone(),
        notify: notify.clone(),
    };
    let handle = Arc::new(PersistHandle {
        enqueued,
        committed,
        error,
        notify,
    });
    (drain, pipe, handle)
}

pub fn spawn_persist(
    pipe: PersistPipe,
    store: Arc<dyn SessionStore>,
    tenant: String,
    session_id: String,
    policy: RetryPolicy,
) {
    tokio::spawn(drain_loop(pipe, store, tenant, session_id, policy));
}

#[derive(Debug)]
struct PersistEmitter {
    drain: PersistDrain,
    sink: Option<Arc<dyn Emitter>>,
}

impl Emitter for PersistEmitter {
    fn emit(&self, event: AgentEvent) {
        // Ahead of `project`, so an observer still sees the deltas the log drops.
        if let Some(sink) = &self.sink {
            sink.emit(event.clone());
        }
        if let Some(se) = project(&event) {
            self.drain.send(Arc::new(se));
        }
    }
}

pub fn attach(
    store: Arc<dyn SessionStore>,
    tenant: String,
    session_id: String,
    sink: Option<Arc<dyn Emitter>>,
) -> (Arc<dyn Emitter>, Arc<PersistHandle>) {
    let (drain, pipe, handle) = persist_channel();
    spawn_persist(pipe, store, tenant, session_id, RetryPolicy::bounded());
    let emitter: Arc<dyn Emitter> = Arc::new(PersistEmitter { drain, sink });
    (emitter, handle)
}

async fn drain_loop(
    mut pipe: PersistPipe,
    store: Arc<dyn SessionStore>,
    tenant: String,
    session_id: String,
    policy: RetryPolicy,
) {
    while let Some(first) = pipe.rx.recv().await {
        let mut batch = vec![first];
        while let Ok(event) = pipe.rx.try_recv() {
            batch.push(event);
        }
        let count = batch.len() as u64;
        let owned: Vec<SessionEvent> = batch.iter().map(|event| (**event).clone()).collect();
        if persist_batch(&store, &tenant, &session_id, policy, owned, count, &pipe).await {
            return;
        }
    }
}

async fn persist_batch(
    store: &Arc<dyn SessionStore>,
    tenant: &str,
    session_id: &str,
    policy: RetryPolicy,
    owned: Vec<SessionEvent>,
    count: u64,
    pipe: &PersistPipe,
) -> bool {
    let span = tracing::info_span!(
        "persist_batch",
        tenant = %tenant,
        session_id = %session_id,
        batch_size = count,
        attempt = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
    );
    async {
        let mut attempt = 0u32;
        loop {
            match store.append_batch(tenant, session_id, &owned).await {
                Ok(()) => {
                    tracing::Span::current().record("attempt", attempt + 1);
                    pipe.committed.fetch_add(count, Ordering::SeqCst);
                    pipe.notify.notify_waiters();
                    return false;
                }
                Err(err) => {
                    attempt += 1;
                    if policy.max_retries.is_some_and(|max| attempt >= max) {
                        tracing::Span::current().record("attempt", attempt);
                        tracing::Span::current().record("otel.status_code", "ERROR");
                        tracing::error!(%tenant, %session_id, attempt, error = %err, "persist gave up");
                        *pipe.error.lock().unwrap() = Some(err.to_string());
                        pipe.notify.notify_waiters();
                        return true;
                    }
                    tracing::warn!(%tenant, %session_id, attempt, error = %err, "persist batch failed — retrying");
                    let delay = policy
                        .base
                        .saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1)))
                        .min(policy.cap);
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
    .instrument(span)
    .await
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
        let (emitter, handle) =
            attach(self.store.clone(), self.tenant.clone(), child.clone(), None);
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
    handle: Arc<PersistHandle>,
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
