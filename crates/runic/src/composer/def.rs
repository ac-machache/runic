use async_trait::async_trait;

use super::Agent;

/// A named agent a host can address without knowing how it was wired.
/// `#[agent]` writes this impl; `runic-serve` resolves the name off it.
#[async_trait]
pub trait AgentDef: Send + Sync {
    fn name(&self) -> &str;

    fn description(&self) -> Option<&str> {
        None
    }

    async fn build_agent(&self) -> anyhow::Result<Agent>;
}
