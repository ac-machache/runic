use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use runic_agent::{Agent, RunContext};
use runic_foundry::{Assembly, assemble};
use runic_mcp::{McpClient, McpTool, mcp_json};
use runic_serve::AgentFactory;
use runic_subagent::{SubagentBuilder, subagents};
use runic_substrate::{ArtifactStore, ReadThreadArtifactTool, SearchChatsTool, SessionStore};
use runic_tool::Tool;
use runic_tools::{TavilyProvider, WebSearchTool, tools};

use crate::providers::Providers;

const ROSTER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/bin/coral/subagents");
const DEFAULT_TOOLBOX_URL: &str = "http://127.0.0.1:5050";

/// Builds Maia (the orchestrator) fresh per thread: the `coral` toolset + the
/// standard surfaces (web/tavily, weather, hitl, memory, search_chats), the
/// specialist roster, and per-subagent toolsets via [`crate::builder`].
pub struct CoralFactory {
    pub providers: Providers,
    pub provider_name: String,
    pub hook_provider_names: HashMap<String, String>,
    pub toolsets: HashMap<String, Vec<Arc<dyn Tool>>>,
    pub builder: Arc<dyn SubagentBuilder>,
    pub store: Arc<dyn SessionStore>,
    pub artifact_store: Arc<dyn ArtifactStore>,
    pub tavily_key: Option<String>,
    pub composio_key: Option<String>,
}

#[async_trait]
impl AgentFactory for CoralFactory {
    async fn build(&self, tenant: &str, session_id: &str) -> Agent {
        let (provider, model) = self.providers.main_model(&self.provider_name);
        let (title_provider, title_model) = self.hook_model("thread-title");

        let mut custom: Vec<Arc<dyn Tool>> =
            self.toolsets.get("coral").cloned().unwrap_or_default();
        custom.push(Arc::new(SearchChatsTool::new(self.store.clone())));
        custom.push(Arc::new(ReadThreadArtifactTool::new(
            self.artifact_store.clone(),
        )));
        if let Some(key) = &self.tavily_key {
            custom.push(Arc::new(WebSearchTool::new(Arc::new(TavilyProvider::new(
                key.clone(),
            )))));
        }

        let mut native = tools().web().weather().hitl();
        if let Some(key) = &self.composio_key {
            native = native.composio(key.clone(), None);
        }

        let assembly = Assembly {
            provider: provider.clone(),
            model: model.into(),
            instructions: include_str!("coral.md").into(),
            memory: Some(crate::memory::coral_memory()),
            skills: None,
            subagents: Some(subagents(ROSTER)),
            subagent_builder: Some(self.builder.clone()),
            mcp: None,
            sessions: None,
            tools: Some(native),
            custom_tools: custom,
            output_schema: None,
            max_turns: Some(16),
            write_hooks: vec![
                Arc::new(crate::hooks::ThreadTitle::new(
                    self.store.clone(),
                    title_provider,
                    title_model,
                )),
                Arc::new(crate::hooks::InjectIds::new("mcp__coral__", ["user_id"])),
                Arc::new(crate::hooks::ComposioEntity),
            ],
            artifact_store: Some(self.artifact_store.clone()),
        };

        assemble(&assembly, tenant, session_id).await
    }

    async fn build_run_context(
        &self,
        _tenant: &str,
        _session_id: &str,
        context: &serde_json::Value,
    ) -> RunContext {
        let mut rc = RunContext::default();
        for key in ["user_id", "org_id"] {
            if let Some(value) = context.get(key) {
                rc = rc.config_value(key, value.clone());
            }
        }
        rc
    }
}

impl CoralFactory {
    fn hook_model(&self, hook_name: &str) -> (Arc<dyn runic_provider::Provider>, &'static str) {
        let provider_name = self
            .hook_provider_names
            .get(hook_name)
            .or_else(|| self.hook_provider_names.get("*"))
            .map(String::as_str)
            .unwrap_or(&self.provider_name);
        self.providers.model(provider_name)
    }
}

pub fn parse_hook_provider_names(raw: Option<String>) -> HashMap<String, String> {
    raw.unwrap_or_default()
        .split(',')
        .filter_map(|entry| {
            let (hook, provider) = entry.split_once('=')?;
            let hook = hook.trim();
            let provider = provider.trim();
            if hook.is_empty() || provider.is_empty() {
                return None;
            }
            Some((hook.to_string(), provider.to_string()))
        })
        .collect()
}

/// Connect each toolset endpoint from `mcp.json` and wrap its remote tools as
/// local tools, keyed by the consuming agent's name.
pub async fn load_toolsets() -> HashMap<String, Vec<Arc<dyn Tool>>> {
    let toolbox = std::env::var("TOOLBOX_URL").unwrap_or_else(|_| DEFAULT_TOOLBOX_URL.to_string());
    let registry = include_str!("mcp.json").replace("${TOOLBOX_URL}", &toolbox);

    let mcp = match serde_json::from_str(&registry) {
        Ok(value) => mcp_json(value),
        Err(e) => {
            tracing::error!(error = %e, "mcp.json is not valid JSON");
            return HashMap::new();
        }
    };

    let mut toolsets: HashMap<String, Vec<Arc<dyn Tool>>> = HashMap::new();
    for (name, config) in mcp.servers() {
        match McpClient::connect(name, config).await {
            Ok(client) => {
                let tools: Vec<Arc<dyn Tool>> = client
                    .tools()
                    .iter()
                    .map(|def| {
                        Arc::new(McpTool::new(client.handle().clone(), def.clone()))
                            as Arc<dyn Tool>
                    })
                    .collect();
                tracing::info!(server = %name, count = tools.len(), "loaded mcp toolset");
                toolsets.insert(name.clone(), tools);
            }
            Err(e) => tracing::warn!(server = %name, error = %e, "mcp connect failed — skipping"),
        }
    }
    toolsets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hook_provider_names() {
        let providers = parse_hook_provider_names(Some(
            "thread-title=haiku, memory-review = flash, broken, *=mistral".into(),
        ));

        assert_eq!(
            providers.get("thread-title").map(String::as_str),
            Some("haiku")
        );
        assert_eq!(
            providers.get("memory-review").map(String::as_str),
            Some("flash")
        );
        assert_eq!(providers.get("*").map(String::as_str), Some("mistral"));
        assert!(!providers.contains_key("broken"));
    }
}
