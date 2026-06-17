//! `ThreadPool` — one Agent per (tenant, thread_id), Mutex-guarded.
//!
//! The pool is the bridge between HTTP requests (stateless) and Agent
//! instances (stateful). When `POST /threads/:id/runs/stream` arrives,
//! we look up the agent for that thread, lock it, run a turn, release.
//! Concurrent requests for the same thread queue on the Mutex (intended
//! — runs on the same thread should serialize).
//!
//! Concurrent requests for DIFFERENT threads run in parallel (their
//! mutexes are independent). The outer map is `RwLock`d so insertion
//! doesn't block readers.

use std::collections::HashMap;
use std::sync::Arc;

use runic_agent_core::Agent;
use runic_sessions::{spawn_persister, SessionStore};
use tokio::sync::{Mutex, RwLock};

use crate::factory::BoxedAgentFactory;

#[derive(Debug, Hash, Eq, PartialEq, Clone)]
struct ThreadKey {
    tenant: String,
    thread_id: String,
}

pub struct ThreadPool {
    /// One slot per active (tenant, thread). The inner is `Option<Agent>`
    /// because `Agent::run_streaming` takes `mut self` by value and
    /// returns it via the JoinHandle — handlers take it out at the
    /// start of a run and put it back when done. Outer RwLock so reads
    /// (the common case once threads are warm) don't block each other;
    /// only first-insert takes the write lock briefly.
    agents: RwLock<HashMap<ThreadKey, Arc<Mutex<Option<Agent>>>>>,
    factory: BoxedAgentFactory,
    /// Where each agent's event stream gets persisted. A persister is
    /// spawned ONCE per agent (at build), subscribing to the agent's
    /// long-lived broadcast — so events from every run on the thread land
    /// in the store without double-writing.
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

    /// The agent factory backing this pool. The runs handler uses it to
    /// build a per-request [`runic_agent_core::RunContext`] via
    /// [`crate::AgentFactory::build_run_context`].
    pub fn factory(&self) -> &BoxedAgentFactory {
        &self.factory
    }

    /// Get (or lazily build) the Agent for this thread. The caller
    /// receives an `Arc<Mutex<Agent>>` — lock it to run a turn, release
    /// when done. Concurrent calls for the same thread share the same
    /// Arc; concurrent calls for different threads get independent ones.
    pub async fn get_or_build(
        &self,
        tenant: &str,
        thread_id: &str,
    ) -> Arc<Mutex<Option<Agent>>> {
        let key = ThreadKey {
            tenant: tenant.to_string(),
            thread_id: thread_id.to_string(),
        };

        // Fast path — already exists. Bare read.
        {
            let map = self.agents.read().await;
            if let Some(existing) = map.get(&key) {
                return existing.clone();
            }
        }

        // Slow path — needs creation. Re-check under write lock to
        // handle the race where two requests for the same thread arrive
        // simultaneously and both miss the read.
        let mut map = self.agents.write().await;
        if let Some(existing) = map.get(&key) {
            return existing.clone();
        }

        // For now we use the thread_id as the agent's session_id. This
        // gives clean alignment between "the HTTP thread the client sees"
        // and "the session under which events get persisted" —
        // sessions/<tenant>/<thread_id>/events.jsonl.
        let agent = self.factory.build(tenant, thread_id).await;

        // Persist this agent's events for the life of the thread. Subscribe
        // BEFORE the first run so the opening RunStart is captured; one
        // persister per agent (not per run) so events aren't double-written.
        let rx = agent.subscribe_events();
        spawn_persister(
            rx,
            self.session_store.clone(),
            tenant.to_string(),
            thread_id.to_string(),
        );

        let arc = Arc::new(Mutex::new(Some(agent)));
        map.insert(key, arc.clone());
        arc
    }

    /// Drop the Agent for this thread — next request will rebuild it.
    /// Useful for `DELETE /threads/:id`. Returns true if there was one.
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

#[cfg(test)]
mod tests {
    // Pool semantics are exercised end-to-end in `tests/integration.rs`
    // where we wire a real Provider — keeping unit tests here would
    // require duplicating a `dyn Provider` mock and the contract is
    // simple enough that an integration round-trip is the better signal.

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
