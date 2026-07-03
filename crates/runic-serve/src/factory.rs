//! `AgentFactory` — the contract between `runic-serve` and the binary.
//!
//! The serve crate doesn't know what tools / hooks / provider you wired —
//! that's the binary's job. When a new thread arrives, the serve crate asks
//! the factory to build a fresh `Agent` for the given session id (so
//! persistence + replay land under the right path). The factory captures
//! whatever Arc-shared state it needs (provider, subagent pool, backends)
//! inside.

use async_trait::async_trait;
use std::sync::Arc;

use runic_agent::{Agent, RunContext};

#[async_trait]
pub trait AgentFactory: Send + Sync {
    async fn build(&self, tenant: &str, session_id: &str) -> Agent;

    fn describe(&self) -> Option<&str> {
        None
    }

    async fn build_run_context(
        &self,
        _tenant: &str,
        _session_id: &str,
        _context: &serde_json::Value,
    ) -> RunContext {
        RunContext::default()
    }
}

pub type BoxedAgentFactory = Arc<dyn AgentFactory>;
