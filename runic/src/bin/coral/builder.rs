use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use runic_agent::Agent;
use runic_subagent::{AgentDef, DelegationCtx, SubagentBuilder};
use runic_tool::Tool;

use crate::providers::{HAIKU, Providers};

/// Builds each specialist child: resolves its `provider:` to a driver and
/// registers the MCP toolset already loaded for its name.
pub struct CoralBuilder {
    providers: Providers,
    toolsets: HashMap<String, Vec<Arc<dyn Tool>>>,
}

impl CoralBuilder {
    pub fn new(providers: Providers, toolsets: HashMap<String, Vec<Arc<dyn Tool>>>) -> Self {
        Self {
            providers,
            toolsets,
        }
    }
}

#[async_trait]
impl SubagentBuilder for CoralBuilder {
    async fn build(&self, def: &AgentDef, dctx: &DelegationCtx) -> Result<Agent> {
        let provider = self.providers.resolve(def.provider.as_deref());
        let model = def.model.clone().unwrap_or_else(|| HAIKU.to_string());
        let user_id = dctx
            .config
            .get("user_id")
            .and_then(|v| v.as_str())
            .unwrap_or("coral");

        let mut b = Agent::builder(provider, user_id, &def.name)
            .model(model)
            .system_prompt(&def.system_prompt);
        if let Some(tools) = self.toolsets.get(&def.name) {
            for tool in tools {
                b = b.tool(tool.clone());
            }
        }
        let ids: Vec<&str> = match def.name.as_str() {
            "purchase-expert" => vec!["user_id"],
            "crm-expert" => vec!["user_id", "org_id"],
            _ => vec![],
        };
        if !ids.is_empty() {
            let prefix = format!("mcp__{}__", def.name);
            b = b.write_hook(Arc::new(crate::hooks::InjectIds::new(prefix, ids)));
        }
        if let Some(max_turns) = def.max_turns {
            b = b.max_turns(max_turns);
        }
        Ok(b.build())
    }
}
