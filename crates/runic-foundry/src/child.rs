use std::sync::Arc;

use async_trait::async_trait;
use runic_agent::Agent;
use runic_provider::Provider;
use runic_subagent::{AgentDef, ChildBuilder, DelegationCtx};
use runic_tools::default_tools;

/// Builds child agents for the `delegate` tool — base tools, the def's system
/// prompt / model / turn cap. (Lives here, not in `runic-subagent`, because it
/// needs `runic-agent` + `runic-tools`.)
pub struct FoundryChildBuilder {
    pub provider: Arc<dyn Provider>,
    pub model: String,
}

#[async_trait]
impl ChildBuilder for FoundryChildBuilder {
    async fn build(&self, def: &AgentDef, _dctx: &DelegationCtx) -> anyhow::Result<Agent> {
        let model = def.model.clone().unwrap_or_else(|| self.model.clone());
        let mut b = Agent::builder(self.provider.clone(), "subagent", &def.name)
            .model(model)
            .system_prompt(&def.system_prompt);
        // Enforce no-escalation: the def's `allowed_tools` filters the pool
        // (empty allow-list inherits all).
        for t in def.scope_tools(&default_tools()) {
            b = b.tool(t);
        }
        if let Some(max_turns) = def.max_turns {
            b = b.max_turns(max_turns);
        }
        Ok(b.build())
    }
}
