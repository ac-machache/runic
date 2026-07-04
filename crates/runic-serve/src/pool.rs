//! `ThreadPool` — one warm Agent per (tenant, thread_id), Mutex-guarded.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use runic_agent::{Agent, CancelToken};
use runic_state::{EVENT_BROADCAST_CAPACITY, PersistSink, SessionEvent};
use runic_substrate::SessionStore;
use tokio::sync::{Mutex, Notify, RwLock, broadcast, mpsc};

use crate::error::ServeError;
use crate::factory::BoxedAgentFactory;

pub const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(30 * 60);
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

pub const DEFAULT_PERSIST_BACKLOG_MAX: u64 = 10_000;
const RETRY_BASE: Duration = Duration::from_millis(100);
const RETRY_CAP: Duration = Duration::from_secs(5);
const RETRY_ESCALATE_AFTER: u32 = 5;

#[derive(Debug, Hash, Eq, PartialEq, Clone)]
struct ThreadKey {
    tenant: String,
    thread_id: String,
    agent: String,
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

struct WarmEntry {
    agent: Arc<Mutex<Agent>>,
    last_active: AtomicU64,
    persist: Arc<PersistHandle>,
}

pub struct ThreadPool {
    agents: RwLock<HashMap<ThreadKey, WarmEntry>>,
    cancel_tokens: RwLock<HashMap<(String, String), CancelToken>>,
    steering_senders: RwLock<HashMap<(String, String), mpsc::UnboundedSender<String>>>,
    factories: HashMap<String, BoxedAgentFactory>,
    session_store: Arc<dyn SessionStore>,
    started_at: Instant,
    idle_ttl: Duration,
    persist_backlog_max: u64,
}

impl ThreadPool {
    pub fn new(
        factories: HashMap<String, BoxedAgentFactory>,
        session_store: Arc<dyn SessionStore>,
    ) -> Self {
        assert!(
            !factories.is_empty(),
            "runic-serve needs at least one agent registered"
        );
        Self {
            agents: RwLock::new(HashMap::new()),
            cancel_tokens: RwLock::new(HashMap::new()),
            steering_senders: RwLock::new(HashMap::new()),
            factories,
            session_store,
            started_at: Instant::now(),
            idle_ttl: DEFAULT_IDLE_TTL,
            persist_backlog_max: DEFAULT_PERSIST_BACKLOG_MAX,
        }
    }

    pub fn with_idle_ttl(mut self, ttl: Duration) -> Self {
        self.idle_ttl = ttl;
        self
    }

    pub fn with_persist_backlog_max(mut self, max: u64) -> Self {
        self.persist_backlog_max = max;
        self
    }

    pub fn spawn_eviction_sweep(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(SWEEP_INTERVAL);
            interval.tick().await;
            loop {
                interval.tick().await;
                let Some(pool) = weak.upgrade() else {
                    break;
                };
                pool.evict_idle().await;
            }
        });
    }

    fn now(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    pub async fn evict_idle(&self) {
        let now = self.now();
        let ttl = self.idle_ttl.as_secs();
        let mut agents = self.agents.write().await;
        let before = agents.len();
        agents.retain(|_key, entry| {
            let idle_for = now.saturating_sub(entry.last_active.load(Ordering::Relaxed));
            idle_for < ttl || entry.agent.try_lock().is_err()
        });
        let evicted = before - agents.len();
        if evicted > 0 {
            tracing::info!(evicted, remaining = agents.len(), "thread pool idle sweep");
        }
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

    /// Get (or lazily build) the Agent running this thread as `agent_name`.
    pub async fn get_or_build(
        &self,
        tenant: &str,
        thread_id: &str,
        agent_name: &str,
    ) -> Result<Arc<Mutex<Agent>>, ServeError> {
        let factory = self.factory(agent_name)?.clone();

        // Stateless agents are never warmed, persisted, or replayed —
        // reconstructed from scratch on every run.
        if factory.stateless() {
            let mut agent = factory.build(tenant, thread_id).await;
            let (tx, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
            agent.state_mut().set_events_tx(tx);
            tracing::debug!(%tenant, %thread_id, agent = %agent_name, "stateless agent built");
            return Ok(Arc::new(Mutex::new(agent)));
        }

        let key = ThreadKey {
            tenant: tenant.to_string(),
            thread_id: thread_id.to_string(),
            agent: agent_name.to_string(),
        };

        let now = self.now();

        // Fast path — already warm.
        {
            let map = self.agents.read().await;
            if let Some(existing) = map.get(&key) {
                existing.last_active.store(now, Ordering::Relaxed);
                tracing::debug!(%tenant, %thread_id, agent = %agent_name, "thread pool warm hit");
                return Ok(existing.agent.clone());
            }
        }

        // Slow path — re-check under the write lock to handle the race where
        // two requests for the same thread both miss the read.
        let mut map = self.agents.write().await;
        if let Some(existing) = map.get(&key) {
            existing.last_active.store(now, Ordering::Relaxed);
            tracing::debug!(%tenant, %thread_id, agent = %agent_name, "thread pool warm hit");
            return Ok(existing.agent.clone());
        }
        tracing::debug!(%tenant, %thread_id, agent = %agent_name, "thread pool warm miss");

        // thread_id == session_id, so persisted events land under
        // sessions/<tenant>/<thread_id>.
        let mut agent = factory.build(tenant, thread_id).await;
        if let Ok(Some(meta)) = self.session_store.session_meta(tenant, thread_id).await {
            agent.state_mut().label = meta.label;
        }

        // Replay the working set (last snapshot + tail) into the fresh state.
        // RunEnd rebuilds stats; RunStart is skipped so an orphaned run can't
        // look in-flight on a fresh agent.
        match self.session_store.read_tail(tenant, thread_id).await {
            Ok(stored) => {
                let mut replayed = 0usize;
                for entry in stored {
                    if matches!(
                        entry.event,
                        SessionEvent::Message { .. }
                            | SessionEvent::StateSnapshot { .. }
                            | SessionEvent::RunEnd { .. }
                            | SessionEvent::TaskSpawned { .. }
                            | SessionEvent::TaskFinished { .. }
                    ) {
                        agent.state_mut().push_event(entry.event);
                        replayed += 1;
                    }
                }
                if replayed > 0 {
                    tracing::debug!(%tenant, %thread_id, replayed, "replayed history into cold agent");
                }
            }
            Err(e) => {
                tracing::warn!(%tenant, %thread_id, error = %e, "history replay failed — starting cold");
            }
        }

        // Install both sinks AFTER replay and BEFORE the first run, so replayed
        // events are never re-persisted but the opening RunStart is captured:
        // a (lossy) broadcast for live UI subscribers and a lossless mpsc for
        // the durable persister.
        let (tx, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        agent.state_mut().set_events_tx(tx);
        let (persist_tx, persist_rx) = mpsc::unbounded_channel();
        let sink = PersistSink::new(persist_tx);
        let persist = Arc::new(PersistHandle {
            enqueued: sink.enqueued(),
            committed: Arc::new(AtomicU64::new(0)),
            notify: Arc::new(Notify::new()),
        });
        agent.state_mut().set_persist_tx(sink);
        spawn_persister(
            persist_rx,
            self.session_store.clone(),
            tenant.to_string(),
            thread_id.to_string(),
            persist.clone(),
        );

        let arc = Arc::new(Mutex::new(agent));
        map.insert(
            key,
            WarmEntry {
                agent: arc.clone(),
                last_active: AtomicU64::new(now),
                persist,
            },
        );
        tracing::info!(%tenant, %thread_id, agent = %agent_name, "agent built");
        Ok(arc)
    }

    pub async fn check_persist_capacity(
        &self,
        tenant: &str,
        thread_id: &str,
    ) -> Result<(), ServeError> {
        let map = self.agents.read().await;
        let worst = map
            .iter()
            .filter(|(k, _)| k.tenant == tenant && k.thread_id == thread_id)
            .map(|(_, e)| e.persist.backlog())
            .max()
            .unwrap_or(0);
        if worst > self.persist_backlog_max {
            return Err(ServeError::PersistenceDegraded {
                thread: thread_id.to_string(),
                backlog: worst,
            });
        }
        Ok(())
    }

    pub async fn persist_handle(
        &self,
        tenant: &str,
        thread_id: &str,
        agent_name: &str,
    ) -> Option<Arc<PersistHandle>> {
        let key = ThreadKey {
            tenant: tenant.to_string(),
            thread_id: thread_id.to_string(),
            agent: agent_name.to_string(),
        };
        self.agents
            .read()
            .await
            .get(&key)
            .map(|e| e.persist.clone())
    }

    pub async fn warm_agents(&self, tenant: &str, thread_id: &str) -> Vec<Arc<Mutex<Agent>>> {
        let map = self.agents.read().await;
        map.iter()
            .filter(|(k, _)| k.tenant == tenant && k.thread_id == thread_id)
            .map(|(_, e)| e.agent.clone())
            .collect()
    }

    /// Find the warm agent (any name) whose in-flight run is `run_id`.
    pub async fn find_live_run(
        &self,
        tenant: &str,
        thread_id: &str,
        run_id: &str,
    ) -> Option<Arc<Mutex<Agent>>> {
        let candidates: Vec<Arc<Mutex<Agent>>> = {
            let map = self.agents.read().await;
            map.iter()
                .filter(|(k, _)| k.tenant == tenant && k.thread_id == thread_id)
                .map(|(_, e)| e.agent.clone())
                .collect()
        };
        for candidate in candidates {
            let is_live = candidate
                .lock()
                .await
                .state()
                .current_run()
                .is_some_and(|run| run.id == run_id);
            if is_live {
                return Some(candidate);
            }
        }
        None
    }

    /// Drop every warm Agent for this thread — next request rebuilds. Returns
    /// true if any existed.
    pub async fn evict(&self, tenant: &str, thread_id: &str) -> bool {
        let mut map = self.agents.write().await;
        let before = map.len();
        map.retain(|k, _| !(k.tenant == tenant && k.thread_id == thread_id));
        let evicted = before != map.len();
        tracing::info!(%tenant, %thread_id, evicted, "thread pool evict");
        evicted
    }

    pub async fn begin_run(
        &self,
        tenant: &str,
        thread_id: &str,
    ) -> (CancelToken, mpsc::UnboundedReceiver<String>) {
        let key = (tenant.to_string(), thread_id.to_string());
        let token = CancelToken::new();
        let (steer_tx, steer_rx) = mpsc::unbounded_channel();
        self.cancel_tokens
            .write()
            .await
            .insert(key.clone(), token.clone());
        self.steering_senders.write().await.insert(key, steer_tx);
        (token, steer_rx)
    }

    pub async fn end_run(&self, tenant: &str, thread_id: &str, token: &CancelToken) {
        let key = (tenant.to_string(), thread_id.to_string());
        let mut tokens = self.cancel_tokens.write().await;
        if tokens.get(&key).is_some_and(|t| t.is_same(token)) {
            tokens.remove(&key);
            self.steering_senders.write().await.remove(&key);
        }
    }

    pub async fn steer_run(&self, tenant: &str, thread_id: &str, text: String) -> bool {
        let key = (tenant.to_string(), thread_id.to_string());
        let steered = match self.steering_senders.read().await.get(&key) {
            Some(tx) => tx.send(text).is_ok(),
            None => false,
        };
        tracing::info!(%tenant, %thread_id, steered, "run steer requested");
        steered
    }

    pub async fn cancel_run(&self, tenant: &str, thread_id: &str) -> bool {
        let key = (tenant.to_string(), thread_id.to_string());
        let cancelled = match self.cancel_tokens.read().await.get(&key) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        };
        tracing::info!(%tenant, %thread_id, cancelled, "run cancel requested");
        cancelled
    }

    /// Mirror a persisted label into every warm agent on this thread.
    pub async fn set_warm_label(&self, tenant: &str, thread_id: &str, label: Option<String>) {
        let existing: Vec<Arc<Mutex<Agent>>> = {
            let map = self.agents.read().await;
            map.iter()
                .filter(|(k, _)| k.tenant == tenant && k.thread_id == thread_id)
                .map(|(_, e)| e.agent.clone())
                .collect()
        };
        for agent in existing {
            agent.lock().await.state_mut().label = label.clone();
        }
    }

    /// How many (tenant, thread) agents are currently warm.
    pub async fn len(&self) -> usize {
        self.agents.read().await.len()
    }

    /// True iff no agents are warm.
    pub async fn is_empty(&self) -> bool {
        self.agents.read().await.is_empty()
    }
}

/// Drain a thread's `SessionEvent` broadcast into the store for the life of the
/// agent. `Lagged` is skipped (the store has the durable copy of older events
/// via earlier appends); `Closed` ends the task when the agent is dropped.
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
        async fn build(&self, tenant: &str, session_id: &str) -> Agent {
            Agent::builder(Arc::new(TestProvider), tenant, session_id)
                .system_prompt("test")
                .build()
        }
    }

    fn pool_with_ttl(ttl: Duration) -> ThreadPool {
        let factories = HashMap::from([(
            "solo".to_string(),
            Arc::new(TestFactory) as BoxedAgentFactory,
        )]);
        ThreadPool::new(factories, Arc::new(MemorySessionStore::new())).with_idle_ttl(ttl)
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

    fn message_event(i: usize) -> SessionEvent {
        SessionEvent::Message {
            run_id: "r1".into(),
            msg: runic_types::Message::user(format!("m{i}")),
            at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn cold_rebuild_restores_stats_from_the_tail() {
        let store = Arc::new(MemorySessionStore::new());
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
                        stats: Some(snapshot_stats),
                        open_tasks: None,
                        data: None,
                        at: chrono::Utc::now(),
                    },
                    message_event(0),
                    SessionEvent::RunEnd {
                        run_id: "r8".into(),
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

        let factories = HashMap::from([(
            "solo".to_string(),
            Arc::new(TestFactory) as BoxedAgentFactory,
        )]);
        let pool = ThreadPool::new(factories, store);
        let agent = pool.get_or_build("t", "s", "solo").await.unwrap();
        let agent = agent.lock().await;

        assert_eq!(agent.state().stats.runs, 8);
        assert_eq!(agent.state().stats.total_tool_calls, 12);
        assert_eq!(agent.state().stats.turns, 2);
        assert_eq!(agent.state().messages_for_provider().len(), 2);
        assert!(agent.state().current_run().is_none());
    }

    #[tokio::test]
    async fn persister_retries_until_the_store_recovers() {
        let store = Arc::new(FlakyStore {
            inner: MemorySessionStore::new(),
            failures_left: std::sync::atomic::AtomicU32::new(3),
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let sink = PersistSink::new(tx);
        let handle = Arc::new(PersistHandle {
            enqueued: sink.enqueued(),
            committed: Arc::new(AtomicU64::new(0)),
            notify: Arc::new(Notify::new()),
        });
        spawn_persister(rx, store.clone(), "t".into(), "s".into(), handle.clone());

        for i in 0..5 {
            sink.send(Arc::new(message_event(i)));
        }
        handle.flush().await;

        let stored = store.inner.read("t", "s").await.unwrap();
        let texts: Vec<String> = stored
            .iter()
            .filter_map(|s| match &s.event {
                SessionEvent::Message { msg, .. } => Some(msg.content.text_content()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["m0", "m1", "m2", "m3", "m4"]);
        assert_eq!(handle.backlog(), 0);
    }

    #[tokio::test]
    async fn a_drowning_persister_refuses_new_runs() {
        let store = Arc::new(FlakyStore {
            inner: MemorySessionStore::new(),
            failures_left: std::sync::atomic::AtomicU32::new(u32::MAX),
        });
        let factories = HashMap::from([(
            "solo".to_string(),
            Arc::new(TestFactory) as BoxedAgentFactory,
        )]);
        let pool = ThreadPool::new(factories, store).with_persist_backlog_max(3);

        pool.check_persist_capacity("t", "s").await.unwrap();

        let agent = pool.get_or_build("t", "s", "solo").await.unwrap();
        {
            let mut agent = agent.lock().await;
            for i in 0..5 {
                agent.state_mut().push_event(message_event(i));
            }
        }

        assert!(matches!(
            pool.check_persist_capacity("t", "s").await,
            Err(ServeError::PersistenceDegraded { backlog: 5, .. })
        ));
        pool.check_persist_capacity("t", "other").await.unwrap();
    }

    #[tokio::test]
    async fn fresh_entry_is_not_evicted() {
        let pool = pool_with_ttl(Duration::from_secs(3600));
        pool.get_or_build("t", "s", "solo").await.unwrap();
        pool.evict_idle().await;
        assert_eq!(pool.len().await, 1);
    }

    #[tokio::test]
    async fn idle_past_ttl_entry_is_evicted() {
        let pool = pool_with_ttl(Duration::from_millis(20));
        pool.get_or_build("t", "s", "solo").await.unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        pool.evict_idle().await;
        assert_eq!(pool.len().await, 0);
    }

    #[tokio::test]
    async fn locked_entry_is_not_evicted_even_if_idle() {
        let pool = pool_with_ttl(Duration::from_millis(20));
        let agent = pool.get_or_build("t", "s", "solo").await.unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;

        let guard = agent.lock().await;
        pool.evict_idle().await;
        assert_eq!(pool.len().await, 1, "in-use agent must survive the sweep");
        drop(guard);

        pool.evict_idle().await;
        assert_eq!(pool.len().await, 0, "released agent is swept next pass");
    }

    struct StatelessTestFactory;

    #[async_trait]
    impl AgentFactory for StatelessTestFactory {
        async fn build(&self, tenant: &str, session_id: &str) -> Agent {
            Agent::builder(Arc::new(TestProvider), tenant, session_id)
                .system_prompt("test")
                .build()
        }

        fn stateless(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn stateless_agent_is_never_pooled() {
        let factories = HashMap::from([(
            "flash".to_string(),
            Arc::new(StatelessTestFactory) as BoxedAgentFactory,
        )]);
        let pool = ThreadPool::new(factories, Arc::new(MemorySessionStore::new()));

        let a = pool.get_or_build("t", "s", "flash").await.unwrap();
        let b = pool.get_or_build("t", "s", "flash").await.unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(pool.len().await, 0);
    }

    #[tokio::test]
    async fn resolve_agent_picks_the_only_agent_or_demands_a_name() {
        let pool = pool_with_ttl(Duration::from_secs(3600));
        assert_eq!(pool.resolve_agent(None).unwrap(), "solo");
        assert_eq!(pool.resolve_agent(Some("solo")).unwrap(), "solo");
        assert!(matches!(
            pool.resolve_agent(Some("ghost")),
            Err(ServeError::AgentNotFound { .. })
        ));

        let factories = HashMap::from([
            (
                "coral".to_string(),
                Arc::new(TestFactory) as BoxedAgentFactory,
            ),
            (
                "scout".to_string(),
                Arc::new(TestFactory) as BoxedAgentFactory,
            ),
        ]);
        let pool = ThreadPool::new(factories, Arc::new(MemorySessionStore::new()));
        match pool.resolve_agent(None) {
            Err(ServeError::BadRequest(message)) => {
                assert!(message.contains("coral") && message.contains("scout"));
            }
            other => panic!("expected bad request, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_agent_is_not_found() {
        let pool = pool_with_ttl(Duration::from_secs(3600));
        assert!(matches!(
            pool.get_or_build("t", "s", "ghost").await,
            Err(ServeError::AgentNotFound { .. })
        ));
    }

    #[test]
    fn thread_key_equality_uses_all_fields() {
        let a = ThreadKey {
            tenant: "alice".into(),
            thread_id: "t1".into(),
            agent: "default".into(),
        };
        let b = ThreadKey {
            tenant: "alice".into(),
            thread_id: "t1".into(),
            agent: "default".into(),
        };
        let c = ThreadKey {
            tenant: "bob".into(),
            thread_id: "t1".into(),
            agent: "default".into(),
        };
        let d = ThreadKey {
            tenant: "alice".into(),
            thread_id: "t2".into(),
            agent: "default".into(),
        };
        let e = ThreadKey {
            tenant: "alice".into(),
            thread_id: "t1".into(),
            agent: "coral".into(),
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert_ne!(a, e);
    }
}
