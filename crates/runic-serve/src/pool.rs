//! `ThreadPool` — one warm Agent per (tenant, thread_id), Mutex-guarded.
//!
//! The pool bridges stateless HTTP requests to stateful Agent instances. When
//! `POST /threads/:id/runs/stream` arrives we look up the agent for that
//! thread, lock it, run a turn, release. Concurrent requests for the same
//! thread queue on the Mutex (intended — runs on one thread serialize);
//! different threads run in parallel (independent mutexes). The outer map is
//! `RwLock`d so a warm-thread lookup doesn't block on insertion.
//!
//! At build time the pool installs a `SessionEvent` broadcast into the agent's
//! state and spawns a **persister** that drains it into the [`SessionStore`] —
//! one per agent (not per run), so events from every run on the thread land in
//! the store without double-writing.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use runic_agent::{Agent, CancelToken};
use runic_state::{EVENT_BROADCAST_CAPACITY, SessionEvent};
use runic_substrate::SessionStore;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};

use crate::factory::BoxedAgentFactory;

pub const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(30 * 60);
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Hash, Eq, PartialEq, Clone)]
struct ThreadKey {
    tenant: String,
    thread_id: String,
}

struct WarmEntry {
    agent: Arc<Mutex<Agent>>,
    last_active: AtomicU64,
}

pub struct ThreadPool {
    /// One slot per active (tenant, thread). `run_message_with` takes
    /// `&mut self`, so the handler locks the Mutex for the duration of a run;
    /// the outer RwLock lets warm-thread reads run without contending on
    /// first-insert.
    agents: RwLock<HashMap<ThreadKey, WarmEntry>>,
    cancel_tokens: RwLock<HashMap<ThreadKey, CancelToken>>,
    factory: BoxedAgentFactory,
    session_store: Arc<dyn SessionStore>,
    started_at: Instant,
    idle_ttl: Duration,
}

impl ThreadPool {
    pub fn new(factory: BoxedAgentFactory, session_store: Arc<dyn SessionStore>) -> Self {
        Self {
            agents: RwLock::new(HashMap::new()),
            cancel_tokens: RwLock::new(HashMap::new()),
            factory,
            session_store,
            started_at: Instant::now(),
            idle_ttl: DEFAULT_IDLE_TTL,
        }
    }

    pub fn with_idle_ttl(mut self, ttl: Duration) -> Self {
        self.idle_ttl = ttl;
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

    /// The agent factory backing this pool. The runs handler uses it to build
    /// a per-request [`runic_agent::RunContext`] via
    /// [`crate::AgentFactory::build_run_context`].
    pub fn factory(&self) -> &BoxedAgentFactory {
        &self.factory
    }

    /// Get (or lazily build) the Agent for this thread.
    pub async fn get_or_build(&self, tenant: &str, thread_id: &str) -> Arc<Mutex<Agent>> {
        let key = ThreadKey {
            tenant: tenant.to_string(),
            thread_id: thread_id.to_string(),
        };

        let now = self.now();

        // Fast path — already warm.
        {
            let map = self.agents.read().await;
            if let Some(existing) = map.get(&key) {
                existing.last_active.store(now, Ordering::Relaxed);
                tracing::debug!(%tenant, %thread_id, "thread pool warm hit");
                return existing.agent.clone();
            }
        }

        // Slow path — re-check under the write lock to handle the race where
        // two requests for the same thread both miss the read.
        let mut map = self.agents.write().await;
        if let Some(existing) = map.get(&key) {
            existing.last_active.store(now, Ordering::Relaxed);
            tracing::debug!(%tenant, %thread_id, "thread pool warm hit");
            return existing.agent.clone();
        }
        tracing::debug!(%tenant, %thread_id, "thread pool warm miss");

        // thread_id == session_id, so persisted events land under
        // sessions/<tenant>/<thread_id>.
        let mut agent = self.factory.build(tenant, thread_id).await;
        if let Ok(Some(meta)) = self.session_store.session_meta(tenant, thread_id).await {
            agent.state_mut().label = meta.label;
        }

        // Install both sinks BEFORE the first run so the opening RunStart is
        // captured: a (lossy) broadcast for live UI subscribers and a lossless
        // mpsc for the durable persister.
        let (tx, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        agent.state_mut().set_events_tx(tx);
        let (persist_tx, persist_rx) = mpsc::unbounded_channel();
        agent.state_mut().set_persist_tx(persist_tx);
        spawn_persister(
            persist_rx,
            self.session_store.clone(),
            tenant.to_string(),
            thread_id.to_string(),
        );

        let arc = Arc::new(Mutex::new(agent));
        map.insert(
            key,
            WarmEntry {
                agent: arc.clone(),
                last_active: AtomicU64::new(now),
            },
        );
        tracing::info!(%tenant, %thread_id, "agent built");
        arc
    }

    /// Drop the Agent for this thread — next request rebuilds it. Returns true
    /// if there was one.
    pub async fn evict(&self, tenant: &str, thread_id: &str) -> bool {
        let key = ThreadKey {
            tenant: tenant.to_string(),
            thread_id: thread_id.to_string(),
        };
        let evicted = self.agents.write().await.remove(&key).is_some();
        tracing::info!(%tenant, %thread_id, evicted, "thread pool evict");
        evicted
    }

    pub async fn begin_run(&self, tenant: &str, thread_id: &str) -> CancelToken {
        let key = ThreadKey {
            tenant: tenant.to_string(),
            thread_id: thread_id.to_string(),
        };
        let token = CancelToken::new();
        self.cancel_tokens.write().await.insert(key, token.clone());
        token
    }

    pub async fn end_run(&self, tenant: &str, thread_id: &str, token: &CancelToken) {
        let key = ThreadKey {
            tenant: tenant.to_string(),
            thread_id: thread_id.to_string(),
        };
        let mut tokens = self.cancel_tokens.write().await;
        if tokens.get(&key).is_some_and(|t| t.is_same(token)) {
            tokens.remove(&key);
        }
    }

    pub async fn cancel_run(&self, tenant: &str, thread_id: &str) -> bool {
        let key = ThreadKey {
            tenant: tenant.to_string(),
            thread_id: thread_id.to_string(),
        };
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

    /// Mirror a persisted label into the warm agent, if this thread is loaded.
    pub async fn set_warm_label(&self, tenant: &str, thread_id: &str, label: Option<String>) {
        let key = ThreadKey {
            tenant: tenant.to_string(),
            thread_id: thread_id.to_string(),
        };
        let existing = {
            let map = self.agents.read().await;
            map.get(&key).map(|entry| entry.agent.clone())
        };
        if let Some(agent) = existing {
            agent.lock().await.state_mut().label = label;
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
    mut rx: mpsc::UnboundedReceiver<SessionEvent>,
    store: Arc<dyn SessionStore>,
    tenant: String,
    session_id: String,
) {
    tokio::spawn(async move {
        // Block for one event, then drain everything else already buffered so a
        // burst persists in a single batched transaction. The channel is
        // unbounded, so nothing is ever dropped.
        while let Some(first) = rx.recv().await {
            let mut batch = vec![first];
            while let Ok(event) = rx.try_recv() {
                batch.push(event);
            }
            let batch_size = batch.len();
            match store.append_batch(&tenant, &session_id, &batch).await {
                Ok(()) => {
                    tracing::debug!(%tenant, %session_id, batch_size, "persister batch append")
                }
                Err(e) => tracing::warn!(
                    %tenant,
                    %session_id,
                    batch_size,
                    error = %e,
                    "persist session events failed"
                ),
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
        ThreadPool::new(Arc::new(TestFactory), Arc::new(MemorySessionStore::new()))
            .with_idle_ttl(ttl)
    }

    #[tokio::test]
    async fn fresh_entry_is_not_evicted() {
        let pool = pool_with_ttl(Duration::from_secs(3600));
        pool.get_or_build("t", "s").await;
        pool.evict_idle().await;
        assert_eq!(pool.len().await, 1);
    }

    #[tokio::test]
    async fn idle_past_ttl_entry_is_evicted() {
        let pool = pool_with_ttl(Duration::from_millis(20));
        pool.get_or_build("t", "s").await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        pool.evict_idle().await;
        assert_eq!(pool.len().await, 0);
    }

    #[tokio::test]
    async fn locked_entry_is_not_evicted_even_if_idle() {
        let pool = pool_with_ttl(Duration::from_millis(20));
        let agent = pool.get_or_build("t", "s").await;
        tokio::time::sleep(Duration::from_millis(40)).await;

        let guard = agent.lock().await;
        pool.evict_idle().await;
        assert_eq!(pool.len().await, 1, "in-use agent must survive the sweep");
        drop(guard);

        pool.evict_idle().await;
        assert_eq!(pool.len().await, 0, "released agent is swept next pass");
    }

    #[test]
    fn thread_key_equality_uses_both_fields() {
        let a = ThreadKey {
            tenant: "alice".into(),
            thread_id: "t1".into(),
        };
        let b = ThreadKey {
            tenant: "alice".into(),
            thread_id: "t1".into(),
        };
        let c = ThreadKey {
            tenant: "bob".into(),
            thread_id: "t1".into(),
        };
        let d = ThreadKey {
            tenant: "alice".into(),
            thread_id: "t2".into(),
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
    }
}
