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

use runic_agent::Agent;
use runic_state::SessionEvent;
use runic_substrate::SessionStore;
use tokio::sync::{Mutex, RwLock, broadcast};

use crate::factory::BoxedAgentFactory;

/// Capacity of the per-agent `SessionEvent` broadcast (persister + any live
/// replay subscribers). A subscriber that falls this far behind sees `Lagged`.
const EVENT_CHANNEL_CAP: usize = 256;

#[derive(Debug, Hash, Eq, PartialEq, Clone)]
struct ThreadKey {
    tenant: String,
    thread_id: String,
}

pub struct ThreadPool {
    /// One slot per active (tenant, thread). `run_message_with` takes
    /// `&mut self`, so the handler locks the Mutex for the duration of a run;
    /// the outer RwLock lets warm-thread reads run without contending on
    /// first-insert.
    agents: RwLock<HashMap<ThreadKey, Arc<Mutex<Agent>>>>,
    factory: BoxedAgentFactory,
    session_store: Arc<dyn SessionStore>,
}

impl ThreadPool {
    pub fn new(factory: BoxedAgentFactory, session_store: Arc<dyn SessionStore>) -> Self {
        Self {
            agents: RwLock::new(HashMap::new()),
            factory,
            session_store,
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

        // Fast path — already warm.
        {
            let map = self.agents.read().await;
            if let Some(existing) = map.get(&key) {
                return existing.clone();
            }
        }

        // Slow path — re-check under the write lock to handle the race where
        // two requests for the same thread both miss the read.
        let mut map = self.agents.write().await;
        if let Some(existing) = map.get(&key) {
            return existing.clone();
        }

        // thread_id == session_id, so persisted events land under
        // sessions/<tenant>/<thread_id>.
        let mut agent = self.factory.build(tenant, thread_id).await;

        // Install the event broadcast + persister BEFORE the first run so the
        // opening RunStart is captured. One persister per agent.
        let (tx, rx) = broadcast::channel(EVENT_CHANNEL_CAP);
        agent.state_mut().set_events_tx(tx);
        spawn_persister(
            rx,
            self.session_store.clone(),
            tenant.to_string(),
            thread_id.to_string(),
        );

        let arc = Arc::new(Mutex::new(agent));
        map.insert(key, arc.clone());
        arc
    }

    /// Drop the Agent for this thread — next request rebuilds it. Returns true
    /// if there was one.
    pub async fn evict(&self, tenant: &str, thread_id: &str) -> bool {
        let key = ThreadKey {
            tenant: tenant.to_string(),
            thread_id: thread_id.to_string(),
        };
        self.agents.write().await.remove(&key).is_some()
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
    mut rx: broadcast::Receiver<SessionEvent>,
    store: Arc<dyn SessionStore>,
    tenant: String,
    session_id: String,
) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Err(e) = store.append(&tenant, &session_id, &event).await {
                        tracing::warn!(%tenant, %session_id, error = %e, "persist session event failed");
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(%tenant, %session_id, skipped = n, "persister lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    // Pool semantics are exercised end-to-end in integration tests where a real
    // Provider is wired; the contract here is simple enough that the key test
    // is the thread-key identity.
    use super::*;

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
