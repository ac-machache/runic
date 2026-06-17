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

    /// Build the per-run context for a single request from the tenant, the
    /// session id, and the request body's open `context` JSON. Called on
    /// EVERY run (the pooled agent is reused), so request-varying values
    /// (user_id, provider, allow_web_search, …) belong here — not in
    /// [`Self::build`]. The serve crate stays agnostic to the keys; the app
    /// decides what they mean and resolves things like a provider override.
    ///
    /// Default: an empty context, so existing factories keep working.
    async fn build_run_context(
        &self,
        _tenant: &str,
        _session_id: &str,
        _context: &serde_json::Value,
    ) -> runic_agent_core::RunContext {
        runic_agent_core::RunContext::default()
    }
}

/// Type alias for what `runic-serve` actually stores — `Arc<dyn ...>`
/// so the same factory can be cloned across thread spawns cheaply.
pub type BoxedAgentFactory = Arc<dyn AgentFactory>;
