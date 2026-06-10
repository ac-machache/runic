//! `AgentFactory` — the contract between `runic-serve` and the binary.
//!
//! The serve crate doesn't know what tools / hooks / provider you wired —
//! that's the binary's job. When a new thread arrives, the serve crate
//! asks the factory to build a fresh `Agent` with the given session id
//! (so persistence + replay land under the right path). The factory
//! captures whatever Arc-shared state it needs (provider, subagent pool,
//! storage backend, etc.) inside.

use async_trait::async_trait;
use std::sync::Arc;

use runic_agent_core::Agent;

#[async_trait]
pub trait AgentFactory: Send + Sync {
    /// Build a fresh Agent for `(tenant, session_id)`. The serve crate
    /// calls this once per thread on first use, then keeps the Agent
    /// alive in the [`crate::ThreadPool`] for subsequent runs.
    async fn build(&self, tenant: &str, session_id: &str) -> Agent;
}

/// Type alias for what `runic-serve` actually stores — `Arc<dyn ...>`
/// so the same factory can be cloned across thread spawns cheaply.
pub type BoxedAgentFactory = Arc<dyn AgentFactory>;
