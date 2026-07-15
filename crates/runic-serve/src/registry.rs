//! Stateless serving: agents are built per request, state is folded from the
//! store's working set, and only what is alive right now is held in memory.
//!
//! - [`AgentRegistry`] — the named factories (one per agent signature).
//! - [`RunRegistry`] — per-thread run locks + handles for in-flight runs
//!   (cancel, steering, live event broadcast, persist backlog).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use runic_agent::{Agent, CancelToken};
use runic_state::{EVENT_BROADCAST_CAPACITY, PersistSink, SessionEvent};
use runic_substrate::SessionStore;
use tokio::sync::{Mutex, Notify, RwLock, broadcast, mpsc};
use tracing::Instrument;

use crate::error::ServeError;
use crate::factory::BoxedAgentFactory;

pub const DEFAULT_PERSIST_BACKLOG_MAX: u64 = 10_000;
const THREAD_LEASE_POLL: Duration = Duration::from_millis(250);
const RETRY_BASE: Duration = Duration::from_millis(100);
const RETRY_CAP: Duration = Duration::from_secs(5);
const RETRY_ESCALATE_AFTER: u32 = 5;

#[derive(Debug, Clone)]
pub struct RunLimits {
    pub max_concurrent_runs: usize,
    pub persist_backlog_max: u64,
    pub run_lease: Duration,
    pub heartbeat_every: Duration,
    pub reap_every: Duration,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            max_concurrent_runs: 256,
            persist_backlog_max: DEFAULT_PERSIST_BACKLOG_MAX,
            run_lease: Duration::from_secs(30),
            heartbeat_every: Duration::from_secs(10),
            reap_every: Duration::from_secs(30),
        }
    }
}

pub(crate) fn as_chrono(d: Duration) -> chrono::Duration {
    chrono::Duration::from_std(d).unwrap_or_else(|_| chrono::Duration::seconds(30))
}

pub struct AgentRegistry {
    factories: HashMap<String, BoxedAgentFactory>,
}

impl AgentRegistry {
    pub fn new(factories: HashMap<String, BoxedAgentFactory>) -> Self {
        assert!(
            !factories.is_empty(),
            "runic-serve needs at least one agent registered"
        );
        Self { factories }
    }

    pub fn factory(&self, agent: &str) -> Result<&BoxedAgentFactory, ServeError> {
        self.factories
            .get(agent)
            .ok_or_else(|| ServeError::AgentNotFound {
                name: agent.to_string(),
            })
    }

    pub fn resolve_agent(&self, requested: Option<&str>) -> Result<String, ServeError> {
        match requested {
            Some(name) => {
                self.factory(name)?;
                Ok(name.to_string())
            }
            None if self.factories.len() == 1 => Ok(self.factories.keys().next().unwrap().clone()),
            None => {
                let mut names: Vec<_> = self.factories.keys().map(String::as_str).collect();
                names.sort_unstable();
                Err(ServeError::BadRequest(format!(
                    "this server hosts several agents; set \"agent\" to one of: {}",
                    names.join(", ")
                )))
            }
        }
    }

    pub fn agent_names(&self) -> Vec<(&str, Option<&str>)> {
        let mut names: Vec<_> = self
            .factories
            .iter()
            .map(|(name, f)| (name.as_str(), f.describe()))
            .collect();
        names.sort_by_key(|(name, _)| *name);
        names
    }
}

pub struct PersistHandle {
    enqueued: Arc<AtomicU64>,
    committed: Arc<AtomicU64>,
    notify: Arc<Notify>,
}

impl PersistHandle {
    pub fn backlog(&self) -> u64 {
        self.enqueued
            .load(Ordering::SeqCst)
            .saturating_sub(self.committed.load(Ordering::SeqCst))
    }

    pub async fn flush(&self) {
        let target = self.enqueued.load(Ordering::SeqCst);
        loop {
            let notified = self.notify.notified();
            if self.committed.load(Ordering::SeqCst) >= target {
                return;
            }
            notified.await;
        }
    }
}

pub struct BegunRun {
    pub run_id: String,
    pub cancel: CancelToken,
    pub steering_tx: mpsc::UnboundedSender<String>,
    pub steering_rx: mpsc::UnboundedReceiver<String>,
    pub events_tx: broadcast::Sender<Arc<SessionEvent>>,
    pub persist_sink: PersistSink,
    pub persist_rx: mpsc::UnboundedReceiver<Arc<SessionEvent>>,
    pub persist: Arc<PersistHandle>,
}

struct LiveRun {
    run_id: String,
    cancel: CancelToken,
    steering: mpsc::UnboundedSender<String>,
    events: broadcast::Sender<Arc<SessionEvent>>,
}

type ThreadKey = (String, String);

pub struct RunRegistry {
    instance_id: String,
    limits: RunLimits,
    broker: Option<Arc<dyn crate::broker::EventBroker>>,
    locks: Mutex<HashMap<ThreadKey, Arc<Mutex<()>>>>,
    live: RwLock<HashMap<ThreadKey, LiveRun>>,
    persist_watch: RwLock<HashMap<ThreadKey, Arc<PersistHandle>>>,
}

impl Default for RunRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl RunRegistry {
    pub fn new() -> Self {
        Self::with_limits(RunLimits::default())
    }

    pub fn with_limits(limits: RunLimits) -> Self {
        Self {
            instance_id: format!("inst-{}", uuid::Uuid::new_v4().simple()),
            limits,
            broker: None,
            locks: Mutex::new(HashMap::new()),
            live: RwLock::new(HashMap::new()),
            persist_watch: RwLock::new(HashMap::new()),
        }
    }

    pub fn with_broker(mut self, broker: Arc<dyn crate::broker::EventBroker>) -> Self {
        self.broker = Some(broker);
        self
    }

    pub fn broker(&self) -> Option<Arc<dyn crate::broker::EventBroker>> {
        self.broker.clone()
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub fn limits(&self) -> &RunLimits {
        &self.limits
    }

    pub async fn thread_lock(&self, tenant: &str, thread_id: &str) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .await
            .entry((tenant.to_string(), thread_id.to_string()))
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub async fn begin(
        &self,
        tenant: &str,
        thread_id: &str,
        run_id: &str,
    ) -> Result<BegunRun, ServeError> {
        let cancel = CancelToken::new();
        let (steer_tx, steer_rx) = mpsc::unbounded_channel();
        let (events_tx, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let (persist_tx, persist_rx) = mpsc::unbounded_channel();
        let persist_sink = PersistSink::new(persist_tx);
        let persist = Arc::new(PersistHandle {
            enqueued: persist_sink.enqueued(),
            committed: Arc::new(AtomicU64::new(0)),
            notify: Arc::new(Notify::new()),
        });

        {
            let mut live = self.live.write().await;
            let key = (tenant.to_string(), thread_id.to_string());
            if !live.contains_key(&key) && live.len() >= self.limits.max_concurrent_runs {
                return Err(ServeError::TooBusy { active: live.len() });
            }
            live.insert(
                key,
                LiveRun {
                    run_id: run_id.to_string(),
                    cancel: cancel.clone(),
                    steering: steer_tx.clone(),
                    events: events_tx.clone(),
                },
            );
        }

        if let Some(broker) = &self.broker {
            crate::broker::spawn_broker_forwarder(
                broker.clone(),
                tenant.to_string(),
                thread_id.to_string(),
                events_tx.subscribe(),
            );
        }

        Ok(BegunRun {
            run_id: run_id.to_string(),
            cancel,
            steering_tx: steer_tx,
            steering_rx: steer_rx,
            events_tx,
            persist_sink,
            persist_rx,
            persist,
        })
    }

    pub async fn end(
        &self,
        tenant: &str,
        thread_id: &str,
        run_id: &str,
        persist: Arc<PersistHandle>,
    ) {
        let key = (tenant.to_string(), thread_id.to_string());
        {
            let mut live = self.live.write().await;
            if live.get(&key).is_some_and(|l| l.run_id == run_id) {
                live.remove(&key);
            }
        }
        let mut watch = self.persist_watch.write().await;
        watch.retain(|_, handle| handle.backlog() > 0);
        if persist.backlog() > 0 {
            watch.insert(key, persist);
        }
    }

    pub async fn cancel_run(&self, tenant: &str, thread_id: &str) -> bool {
        let key = (tenant.to_string(), thread_id.to_string());
        let cancelled = match self.live.read().await.get(&key) {
            Some(l) => {
                l.cancel.cancel();
                true
            }
            None => false,
        };
        tracing::info!(%tenant, %thread_id, cancelled, "run cancel requested");
        cancelled
    }

    pub async fn steer_run(&self, tenant: &str, thread_id: &str, text: String) -> bool {
        let key = (tenant.to_string(), thread_id.to_string());
        let steered = match self.live.read().await.get(&key) {
            Some(l) => l.steering.send(text).is_ok(),
            None => false,
        };
        tracing::info!(%tenant, %thread_id, steered, "run steer requested");
        steered
    }

    pub async fn is_busy(&self, tenant: &str, thread_id: &str) -> bool {
        self.live
            .read()
            .await
            .contains_key(&(tenant.to_string(), thread_id.to_string()))
    }

    pub async fn live_events(
        &self,
        tenant: &str,
        thread_id: &str,
        run_id: &str,
    ) -> Option<broadcast::Receiver<Arc<SessionEvent>>> {
        let key = (tenant.to_string(), thread_id.to_string());
        self.live
            .read()
            .await
            .get(&key)
            .filter(|l| l.run_id == run_id)
            .map(|l| l.events.subscribe())
    }

    pub async fn check_persist_capacity(
        &self,
        tenant: &str,
        thread_id: &str,
    ) -> Result<(), ServeError> {
        let key = (tenant.to_string(), thread_id.to_string());
        let worst = {
            let watch = self.persist_watch.read().await;
            watch.get(&key).map(|h| h.backlog()).unwrap_or(0)
        };
        if worst > self.limits.persist_backlog_max {
            return Err(ServeError::PersistenceDegraded {
                thread: thread_id.to_string(),
                backlog: worst,
            });
        }
        Ok(())
    }

    pub async fn forget_thread(&self, tenant: &str, thread_id: &str) {
        let key = (tenant.to_string(), thread_id.to_string());
        self.locks.lock().await.remove(&key);
        self.persist_watch.write().await.remove(&key);
    }
}

/// Build a fresh agent for one request: factory build, label, working-set
/// fold, live sinks, persister. This is the whole "warm agent" replaced.
pub async fn hydrate_agent(
    store: &Arc<dyn SessionStore>,
    factory: &BoxedAgentFactory,
    tenant: &str,
    thread_id: &str,
    begun: &mut BegunRun,
) -> anyhow::Result<Agent> {
    let span = tracing::info_span!(
        "hydrate",
        tenant = %tenant,
        thread = %thread_id,
        stateless = factory.stateless(),
        events = tracing::field::Empty,
    );
    hydrate_agent_inner(store, factory, tenant, thread_id, begun, &span)
        .instrument(span.clone())
        .await
}

async fn hydrate_agent_inner(
    store: &Arc<dyn SessionStore>,
    factory: &BoxedAgentFactory,
    tenant: &str,
    thread_id: &str,
    begun: &mut BegunRun,
    span: &tracing::Span,
) -> anyhow::Result<Agent> {
    let mut agent = factory.build(tenant, thread_id).await?;

    if factory.stateless() {
        agent.state_mut().set_events_tx(begun.events_tx.clone());
        return Ok(agent);
    }

    if let Ok(Some(meta)) = store.session_meta(tenant, thread_id).await {
        agent.state_mut().label = meta.label;
    }

    // Replay the working set (last snapshot + tail) into the fresh state —
    // a pure fold of the full log. A crashed run's dangling RunStart stays
    // visible as in-flight; that's the truth, and run rows own orphan
    // handling.
    match store.read_tail(tenant, thread_id).await {
        Ok(stored) => {
            span.record("events", stored.len());
            for entry in stored {
                agent.state_mut().fold_event(entry.event);
            }
        }
        Err(e) => {
            tracing::warn!(%tenant, %thread_id, error = %e, "history fold failed — starting cold");
        }
    }

    agent.state_mut().set_events_tx(begun.events_tx.clone());
    agent.state_mut().set_persist_tx(begun.persist_sink.clone());
    let rx = std::mem::replace(&mut begun.persist_rx, mpsc::unbounded_channel().1);
    spawn_persister(
        rx,
        store.clone(),
        tenant.to_string(),
        thread_id.to_string(),
        begun.persist.clone(),
    );

    Ok(agent)
}

pub enum Claim {
    Held(tokio::task::JoinHandle<()>),
    Unleased,
    Lost,
}

impl Claim {
    pub fn release(self) {
        if let Claim::Held(heartbeat) = self {
            heartbeat.abort();
        }
    }
}

pub(crate) async fn claim_lease(
    store: &Arc<dyn SessionStore>,
    registry: &RunRegistry,
    run: HeartbeatRun,
) -> Claim {
    let limits = registry.limits();
    let lease = as_chrono(limits.run_lease);
    match store
        .claim_run(&run.run_id, registry.instance_id(), lease)
        .await
    {
        Ok(true) => Claim::Held(spawn_heartbeat(
            store.clone(),
            run,
            registry.instance_id().to_string(),
            lease,
            limits.heartbeat_every,
        )),
        Ok(false) => Claim::Lost,
        Err(e) => {
            tracing::warn!(run_id = %run.run_id, error = %e, "run lease claim failed — running unleased");
            Claim::Unleased
        }
    }
}

pub(crate) struct HeartbeatRun {
    pub(crate) tenant: String,
    pub(crate) thread_id: String,
    pub(crate) run_id: String,
    pub(crate) cancel: CancelToken,
    pub(crate) steering: mpsc::UnboundedSender<String>,
}

pub(crate) fn spawn_heartbeat(
    store: Arc<dyn SessionStore>,
    run: HeartbeatRun,
    instance_id: String,
    lease: chrono::Duration,
    every: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            match store.heartbeat_run(&run.run_id, &instance_id, lease).await {
                Ok(Some(signals)) => {
                    if signals.cancel_requested {
                        tracing::info!(run_id = %run.run_id, "cross-instance cancel picked up");
                        run.cancel.cancel();
                    }
                    for text in signals.steering {
                        let _ = run.steering.send(text);
                    }
                }
                Ok(None) => {
                    tracing::warn!(run_id = %run.run_id, "run lease lost — cancelling the run");
                    run.cancel.cancel();
                    break;
                }
                Err(e) => {
                    tracing::warn!(run_id = %run.run_id, error = %e, "run heartbeat failed");
                }
            }
            if let Err(e) = store
                .extend_thread_lease(&run.tenant, &run.thread_id, &instance_id, lease)
                .await
                && !matches!(e, runic_substrate::Error::Unsupported(_))
            {
                tracing::warn!(run_id = %run.run_id, error = %e, "thread lease extension failed");
            }
        }
    })
}

pub(crate) async fn acquire_thread_lease(
    store: &Arc<dyn SessionStore>,
    registry: &RunRegistry,
    tenant: &str,
    thread_id: &str,
    cancel: &CancelToken,
) -> bool {
    let lease = as_chrono(registry.limits().run_lease);
    loop {
        if cancel.is_cancelled() {
            return false;
        }
        match store
            .claim_thread(tenant, thread_id, registry.instance_id(), lease)
            .await
        {
            Ok(true) => return true,
            Ok(false) => {
                tokio::time::sleep(THREAD_LEASE_POLL).await;
            }
            Err(runic_substrate::Error::Unsupported(_)) => return true,
            Err(e) => {
                tracing::warn!(%tenant, %thread_id, error = %e, "thread lease claim failed");
                tokio::time::sleep(THREAD_LEASE_POLL).await;
            }
        }
    }
}

pub(crate) async fn release_thread_lease(
    store: &Arc<dyn SessionStore>,
    registry: &RunRegistry,
    tenant: &str,
    thread_id: &str,
) {
    if let Err(e) = store
        .release_thread(tenant, thread_id, registry.instance_id())
        .await
        && !matches!(e, runic_substrate::Error::Unsupported(_))
    {
        tracing::warn!(%tenant, %thread_id, error = %e, "thread lease release failed");
    }
}

pub fn spawn_lease_reaper(
    store: Arc<dyn SessionStore>,
    every: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match store.reap_expired_runs().await {
                Ok(reaped) => {
                    for run in reaped {
                        tracing::warn!(
                            run_id = %run.run_id,
                            tenant = %run.tenant,
                            session_id = %run.session_id,
                            claimed_by = run.claimed_by.as_deref().unwrap_or("-"),
                            "expired run lease reaped"
                        );
                    }
                }
                Err(runic_substrate::Error::Unsupported(_)) => {
                    tracing::debug!("store has no run rows — lease reaper off");
                    break;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "lease reap failed");
                }
            }
            tokio::time::sleep(every).await;
        }
    })
}

fn spawn_persister(
    mut rx: mpsc::UnboundedReceiver<Arc<SessionEvent>>,
    store: Arc<dyn SessionStore>,
    tenant: String,
    session_id: String,
    handle: Arc<PersistHandle>,
) {
    tokio::spawn(async move {
        // append_batch is one transaction, so retrying a failed batch can't
        // double-write.
        while let Some(first) = rx.recv().await {
            let mut shared = vec![first];
            while let Ok(event) = rx.try_recv() {
                shared.push(event);
            }
            let batch: Vec<SessionEvent> = shared.iter().map(|e| (**e).clone()).collect();
            let batch_size = batch.len();
            let mut attempt = 0u32;
            loop {
                match store.append_batch(&tenant, &session_id, &batch).await {
                    Ok(()) => {
                        handle
                            .committed
                            .fetch_add(batch_size as u64, Ordering::SeqCst);
                        handle.notify.notify_waiters();
                        tracing::debug!(%tenant, %session_id, batch_size, "persister batch append");
                        break;
                    }
                    Err(e) => {
                        attempt += 1;
                        let delay = RETRY_BASE
                            .saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1)))
                            .min(RETRY_CAP);
                        if attempt >= RETRY_ESCALATE_AFTER {
                            tracing::error!(
                                %tenant, %session_id, batch_size, attempt,
                                backlog = handle.backlog(),
                                error = %e,
                                "persist batch still failing — retrying"
                            );
                        } else {
                            tracing::warn!(
                                %tenant, %session_id, batch_size, attempt,
                                error = %e,
                                "persist batch failed — retrying"
                            );
                        }
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
    use runic_substrate::MemorySessionStore;
    use runic_types::{ContentBlock, StopReason, TokenUsage};

    use crate::factory::AgentFactory;

    struct TestProvider;

    #[async_trait]
    impl Provider for TestProvider {
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text {
                    text: "ok".into(),
                    provider_metadata: None,
                }],
                stop_reason: StopReason::EndTurn,
                tool_calls: vec![],
                usage: TokenUsage::default(),
            })
        }
    }

    struct TestFactory;

    #[async_trait]
    impl AgentFactory for TestFactory {
        async fn build(&self, tenant: &str, session_id: &str) -> anyhow::Result<Agent> {
            Ok(Agent::builder(Arc::new(TestProvider), tenant, session_id)
                .system_prompt("test")
                .build())
        }
    }

    struct StatelessTestFactory;

    #[async_trait]
    impl AgentFactory for StatelessTestFactory {
        async fn build(&self, tenant: &str, session_id: &str) -> anyhow::Result<Agent> {
            Ok(Agent::builder(Arc::new(TestProvider), tenant, session_id)
                .system_prompt("test")
                .build())
        }

        fn stateless(&self) -> bool {
            true
        }
    }

    fn solo_registry() -> AgentRegistry {
        AgentRegistry::new(HashMap::from([(
            "solo".to_string(),
            Arc::new(TestFactory) as BoxedAgentFactory,
        )]))
    }

    fn hb(run_id: &str) -> HeartbeatRun {
        let (steer_tx, _steer_rx) = mpsc::unbounded_channel();
        std::mem::forget(_steer_rx);
        HeartbeatRun {
            tenant: "t".into(),
            thread_id: "s".into(),
            run_id: run_id.into(),
            cancel: CancelToken::new(),
            steering: steer_tx,
        }
    }

    fn message_event(i: usize) -> SessionEvent {
        SessionEvent::Message {
            run_id: "r1".into(),
            msg: runic_types::Message::user(format!("m{i}")),
            at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn resolve_agent_picks_the_only_agent_or_demands_a_name() {
        let reg = solo_registry();
        assert_eq!(reg.resolve_agent(None).unwrap(), "solo");
        assert_eq!(reg.resolve_agent(Some("solo")).unwrap(), "solo");
        assert!(matches!(
            reg.resolve_agent(Some("ghost")),
            Err(ServeError::AgentNotFound { .. })
        ));

        let reg = AgentRegistry::new(HashMap::from([
            (
                "coral".to_string(),
                Arc::new(TestFactory) as BoxedAgentFactory,
            ),
            (
                "scout".to_string(),
                Arc::new(TestFactory) as BoxedAgentFactory,
            ),
        ]));
        match reg.resolve_agent(None) {
            Err(ServeError::BadRequest(message)) => {
                assert!(message.contains("coral") && message.contains("scout"));
            }
            other => panic!("expected bad request, got {other:?}"),
        }
    }

    struct NoRunRowsStore;

    #[async_trait]
    impl SessionStore for NoRunRowsStore {
        async fn append(
            &self,
            _tenant: &str,
            _session_id: &str,
            _event: &SessionEvent,
        ) -> runic_substrate::Result<u64> {
            Ok(0)
        }

        async fn append_batch(
            &self,
            _tenant: &str,
            _session_id: &str,
            _events: &[SessionEvent],
        ) -> runic_substrate::Result<()> {
            Ok(())
        }

        async fn read(
            &self,
            _tenant: &str,
            _session_id: &str,
        ) -> runic_substrate::Result<Vec<runic_substrate::StoredEvent>> {
            Ok(Vec::new())
        }

        async fn read_after(
            &self,
            _tenant: &str,
            _session_id: &str,
            _after_seq: u64,
        ) -> runic_substrate::Result<Vec<runic_substrate::StoredEvent>> {
            Ok(Vec::new())
        }

        async fn list_sessions(
            &self,
            _tenant: &str,
        ) -> runic_substrate::Result<Vec<runic_substrate::SessionMeta>> {
            Ok(Vec::new())
        }

        async fn session_meta(
            &self,
            _tenant: &str,
            _session_id: &str,
        ) -> runic_substrate::Result<Option<runic_substrate::SessionMeta>> {
            Ok(None)
        }

        async fn set_label(
            &self,
            _tenant: &str,
            _session_id: &str,
            _label: Option<&str>,
        ) -> runic_substrate::Result<()> {
            Ok(())
        }

        async fn delete_session(
            &self,
            _tenant: &str,
            _session_id: &str,
        ) -> runic_substrate::Result<()> {
            Ok(())
        }
    }

    struct FlakyStore {
        inner: MemorySessionStore,
        failures_left: std::sync::atomic::AtomicU32,
    }

    #[async_trait]
    impl SessionStore for FlakyStore {
        async fn append(
            &self,
            tenant: &str,
            session_id: &str,
            event: &SessionEvent,
        ) -> runic_substrate::Result<u64> {
            self.inner.append(tenant, session_id, event).await
        }

        async fn append_batch(
            &self,
            tenant: &str,
            session_id: &str,
            events: &[SessionEvent],
        ) -> runic_substrate::Result<()> {
            let left = self.failures_left.load(Ordering::SeqCst);
            if left > 0 {
                self.failures_left.store(left - 1, Ordering::SeqCst);
                return Err(runic_substrate::Error::Unsupported("store is down".into()));
            }
            self.inner.append_batch(tenant, session_id, events).await
        }

        async fn read(
            &self,
            tenant: &str,
            session_id: &str,
        ) -> runic_substrate::Result<Vec<runic_substrate::StoredEvent>> {
            self.inner.read(tenant, session_id).await
        }

        async fn read_after(
            &self,
            tenant: &str,
            session_id: &str,
            after_seq: u64,
        ) -> runic_substrate::Result<Vec<runic_substrate::StoredEvent>> {
            self.inner.read_after(tenant, session_id, after_seq).await
        }

        async fn list_sessions(
            &self,
            tenant: &str,
        ) -> runic_substrate::Result<Vec<runic_substrate::SessionMeta>> {
            self.inner.list_sessions(tenant).await
        }

        async fn session_meta(
            &self,
            tenant: &str,
            session_id: &str,
        ) -> runic_substrate::Result<Option<runic_substrate::SessionMeta>> {
            self.inner.session_meta(tenant, session_id).await
        }

        async fn set_label(
            &self,
            tenant: &str,
            session_id: &str,
            label: Option<&str>,
        ) -> runic_substrate::Result<()> {
            self.inner.set_label(tenant, session_id, label).await
        }

        async fn delete_session(
            &self,
            tenant: &str,
            session_id: &str,
        ) -> runic_substrate::Result<()> {
            self.inner.delete_session(tenant, session_id).await
        }
    }

    #[tokio::test]
    async fn persister_retries_until_the_store_recovers() {
        let store = Arc::new(FlakyStore {
            inner: MemorySessionStore::new(),
            failures_left: std::sync::atomic::AtomicU32::new(3),
        });
        let registry = RunRegistry::new();
        let begun = registry.begin("t", "s", "r-1").await.unwrap();
        spawn_persister(
            begun.persist_rx,
            store.clone(),
            "t".into(),
            "s".into(),
            begun.persist.clone(),
        );

        for i in 0..5 {
            begun.persist_sink.send(Arc::new(message_event(i)));
        }
        begun.persist.flush().await;

        let stored = store.inner.read("t", "s").await.unwrap();
        let texts: Vec<String> = stored
            .iter()
            .filter_map(|s| match &s.event {
                SessionEvent::Message { msg, .. } => Some(msg.content.text_content()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["m0", "m1", "m2", "m3", "m4"]);
        assert_eq!(begun.persist.backlog(), 0);
    }

    #[tokio::test]
    async fn a_lingering_backlog_refuses_new_runs_until_it_drains() {
        let store = Arc::new(FlakyStore {
            inner: MemorySessionStore::new(),
            failures_left: std::sync::atomic::AtomicU32::new(u32::MAX),
        });
        let registry = RunRegistry::with_limits(RunLimits {
            persist_backlog_max: 3,
            ..Default::default()
        });
        let begun = registry.begin("t", "s", "r-1").await.unwrap();
        spawn_persister(
            begun.persist_rx,
            store.clone(),
            "t".into(),
            "s".into(),
            begun.persist.clone(),
        );
        for i in 0..5 {
            begun.persist_sink.send(Arc::new(message_event(i)));
        }
        registry.end("t", "s", "r-1", begun.persist.clone()).await;

        assert!(matches!(
            registry.check_persist_capacity("t", "s").await,
            Err(ServeError::PersistenceDegraded { backlog: 5, .. })
        ));
        registry.check_persist_capacity("t", "other").await.unwrap();

        store.failures_left.store(0, Ordering::SeqCst);
        begun.persist.flush().await;
        registry.end("t", "s2", "r-x", begun.persist.clone()).await;
        registry.check_persist_capacity("t", "s").await.unwrap();
    }

    #[tokio::test]
    async fn hydrate_restores_the_working_set_and_stats() {
        let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
        let snapshot_stats = runic_state::ThreadStats {
            runs: 7,
            total_tool_calls: 12,
            ..Default::default()
        };
        store
            .append_batch(
                "t",
                "s",
                &[
                    SessionEvent::StateSnapshot {
                        run_id: "r7".into(),
                        messages: vec![runic_types::Message::assistant("summary")],
                        system_prompt: "sys".into(),
                        reason: "compaction".into(),
                        stats: Some(Box::new(snapshot_stats)),
                        open_tasks: None,
                        data: None,
                        at: chrono::Utc::now(),
                    },
                    SessionEvent::RunStart {
                        run_id: "r8".into(),
                        agent: None,
                        audit: None,
                        at: chrono::Utc::now(),
                    },
                    message_event(0),
                    SessionEvent::TurnEnd {
                        run_id: "r8".into(),
                        turn: 1,
                        model: "m".into(),
                        usage: runic_types::TokenUsage::default(),
                        model_ms: 1,
                        at: chrono::Utc::now(),
                    },
                    SessionEvent::TurnEnd {
                        run_id: "r8".into(),
                        turn: 2,
                        model: "m".into(),
                        usage: runic_types::TokenUsage::default(),
                        model_ms: 1,
                        at: chrono::Utc::now(),
                    },
                    SessionEvent::RunEnd {
                        run_id: "r8".into(),
                        status: runic_state::RunEndStatus::Completed,
                        outcome: runic_state::RunOutcome {
                            total_turns: 2,
                            ..Default::default()
                        },
                        at: chrono::Utc::now(),
                    },
                ],
            )
            .await
            .unwrap();

        let registry = RunRegistry::new();
        let mut begun = registry.begin("t", "s", "r-9").await.unwrap();
        let factory: BoxedAgentFactory = Arc::new(TestFactory);
        let agent = hydrate_agent(&store, &factory, "t", "s", &mut begun)
            .await
            .unwrap();

        assert_eq!(agent.state().stats().runs, 8);
        assert_eq!(agent.state().stats().total_tool_calls, 12);
        assert_eq!(agent.state().stats().turns, 2);
        assert_eq!(agent.state().messages_for_provider().len(), 2);
        assert!(agent.state().current_run().is_none());
    }

    #[tokio::test]
    async fn stateless_factories_skip_replay_and_persistence() {
        let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
        store.append("t", "s", &message_event(0)).await.unwrap();

        let registry = RunRegistry::new();
        let mut begun = registry.begin("t", "s", "r-1").await.unwrap();
        let factory: BoxedAgentFactory = Arc::new(StatelessTestFactory);
        let mut agent = hydrate_agent(&store, &factory, "t", "s", &mut begun)
            .await
            .unwrap();

        assert!(agent.state().messages_for_provider().is_empty());
        agent.state_mut().push_event(message_event(9));
        drop(agent);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(store.read("t", "s").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_thread_lock_serializes_runs() {
        let registry = Arc::new(RunRegistry::new());
        let order = Arc::new(Mutex::new(Vec::new()));

        let mut handles = Vec::new();
        for i in 0..3 {
            let registry = registry.clone();
            let order = order.clone();
            handles.push(tokio::spawn(async move {
                let lock = registry.thread_lock("t", "s").await;
                let _guard = lock.lock().await;
                order.lock().await.push(format!("start-{i}"));
                tokio::time::sleep(Duration::from_millis(20)).await;
                order.lock().await.push(format!("end-{i}"));
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let order = order.lock().await;
        for pair in order.chunks(2) {
            assert_eq!(
                pair[0].replace("start", ""),
                pair[1].replace("end", ""),
                "runs interleaved: {order:?}"
            );
        }
    }

    #[tokio::test]
    async fn begin_refuses_new_threads_past_the_cap() {
        let registry = RunRegistry::with_limits(RunLimits {
            max_concurrent_runs: 1,
            ..Default::default()
        });
        let begun = registry.begin("t", "a", "r-1").await.unwrap();
        assert!(matches!(
            registry.begin("t", "b", "r-2").await,
            Err(ServeError::TooBusy { active: 1 })
        ));
        registry.end("t", "a", "r-1", begun.persist.clone()).await;
        registry.begin("t", "b", "r-2").await.unwrap();
    }

    #[tokio::test]
    async fn claiming_takes_the_lease_or_reports_it_lost() {
        let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
        let registry = RunRegistry::new();

        store
            .create_run("t", "s", "r-mine", "main", &Default::default())
            .await
            .unwrap();
        let claim = claim_lease(&store, &registry, hb("r-mine")).await;
        assert!(matches!(claim, Claim::Held(_)));
        claim.release();
        let rec = store.get_run("t", "r-mine").await.unwrap().unwrap();
        assert_eq!(rec.claimed_by.as_deref(), Some(registry.instance_id()));

        store
            .create_run("t", "s", "r-taken", "main", &Default::default())
            .await
            .unwrap();
        store
            .claim_run("r-taken", "someone-else", chrono::Duration::seconds(30))
            .await
            .unwrap();
        let claim = claim_lease(&store, &registry, hb("r-taken")).await;
        assert!(matches!(claim, Claim::Lost));

        let unsupported: Arc<dyn SessionStore> = Arc::new(NoRunRowsStore);
        let claim = claim_lease(&unsupported, &registry, hb("r-any")).await;
        assert!(matches!(claim, Claim::Unleased));
    }

    #[tokio::test]
    async fn a_lost_lease_cancels_the_run() {
        let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
        store
            .create_run("t", "s", "r-1", "main", &Default::default())
            .await
            .unwrap();
        let registry = RunRegistry::with_limits(RunLimits {
            heartbeat_every: Duration::from_millis(10),
            ..Default::default()
        });
        let cancel = CancelToken::new();
        let mut run = hb("r-1");
        run.cancel = cancel.clone();
        let claim = claim_lease(&store, &registry, run).await;
        assert!(matches!(claim, Claim::Held(_)));

        store
            .set_run_status("r-1", runic_substrate::RunStatus::Error, Some("reaped"))
            .await
            .unwrap();

        for _ in 0..200 {
            if cancel.is_cancelled() {
                claim.release();
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("losing the lease never cancelled the run");
    }

    #[tokio::test]
    async fn the_heartbeat_delivers_cross_instance_signals() {
        let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
        store
            .create_run("t", "s", "r-1", "main", &Default::default())
            .await
            .unwrap();
        let registry = RunRegistry::with_limits(RunLimits {
            heartbeat_every: Duration::from_millis(10),
            ..Default::default()
        });
        let cancel = CancelToken::new();
        let (steer_tx, mut steer_rx) = mpsc::unbounded_channel();
        let claim = claim_lease(
            &store,
            &registry,
            HeartbeatRun {
                tenant: "t".into(),
                thread_id: "s".into(),
                run_id: "r-1".into(),
                cancel: cancel.clone(),
                steering: steer_tx,
            },
        )
        .await;
        assert!(matches!(claim, Claim::Held(_)));

        store.push_steering("t", "r-1", "go left").await.unwrap();
        store.request_cancel_run("t", "r-1").await.unwrap();

        let mut steered = None;
        for _ in 0..200 {
            if let Ok(text) = steer_rx.try_recv() {
                steered = Some(text);
            }
            if cancel.is_cancelled() && steered.is_some() {
                claim.release();
                assert_eq!(steered.as_deref(), Some("go left"));
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("heartbeat never delivered the signals");
    }

    #[tokio::test]
    async fn thread_lease_acquisition_waits_out_a_foreign_lease() {
        let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
        let registry = RunRegistry::new();
        store
            .claim_thread("t", "s", "inst-other", chrono::Duration::milliseconds(200))
            .await
            .unwrap();

        let start = std::time::Instant::now();
        assert!(acquire_thread_lease(&store, &registry, "t", "s", &CancelToken::new()).await);
        assert!(start.elapsed() >= Duration::from_millis(150));

        store
            .claim_thread("t", "s2", "inst-other", chrono::Duration::seconds(60))
            .await
            .unwrap();
        let cancel = CancelToken::new();
        let killer = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            killer.cancel();
        });
        assert!(!acquire_thread_lease(&store, &registry, "t", "s2", &cancel).await);
    }

    #[tokio::test]
    async fn the_reaper_marks_expired_runs() {
        let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
        store
            .create_run("t", "s", "r-dead", "main", &Default::default())
            .await
            .unwrap();
        store
            .claim_run("r-dead", "inst-gone", chrono::Duration::seconds(-1))
            .await
            .unwrap();

        let reaper = spawn_lease_reaper(store.clone(), Duration::from_millis(10));
        for _ in 0..200 {
            let rec = store.get_run("t", "r-dead").await.unwrap().unwrap();
            if rec.status == runic_substrate::RunStatus::Error {
                assert_eq!(rec.error.as_deref(), Some("lease expired"));
                reaper.abort();
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the reaper never swept the expired run");
    }

    #[tokio::test]
    async fn live_run_handles_answer_cancel_steer_and_attach() {
        let registry = RunRegistry::new();
        let begun = registry.begin("t", "s", "r-1").await.unwrap();

        assert!(registry.is_busy("t", "s").await);
        assert!(registry.live_events("t", "s", "r-1").await.is_some());
        assert!(registry.live_events("t", "s", "r-other").await.is_none());
        assert!(registry.steer_run("t", "s", "hey".into()).await);
        assert!(registry.cancel_run("t", "s").await);
        assert!(begun.cancel.is_cancelled());

        registry.end("t", "s", "r-1", begun.persist.clone()).await;
        assert!(!registry.is_busy("t", "s").await);
        assert!(!registry.cancel_run("t", "s").await);
        assert!(!registry.steer_run("t", "s", "hey".into()).await);
    }
}
