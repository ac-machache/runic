use std::sync::Arc;

use runic_agent::Agent;
use runic_provider::Provider;
use runic_tool::Tool;

pub struct HookAgent {
    provider: Arc<dyn Provider>,
    model: String,
    prompt: String,
    tools: Vec<Arc<dyn Tool>>,
    max_turns: u32,
}

impl HookAgent {
    pub fn new(provider: Arc<dyn Provider>, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
            prompt: String::new(),
            tools: Vec::new(),
            max_turns: 8,
        }
    }

    pub fn prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = prompt.into();
        self
    }

    pub fn tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    pub fn tools(mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> Self {
        self.tools.extend(tools);
        self
    }

    pub fn max_turns(mut self, n: u32) -> Self {
        self.max_turns = n;
        self
    }

    pub async fn run(self, input: impl Into<String>) -> anyhow::Result<String> {
        let mut builder = Agent::builder(self.provider, "hook-agent", "hook")
            .model(self.model)
            .system_prompt(self.prompt)
            .max_turns(self.max_turns)
            .graceful_max_turns(true);
        for tool in self.tools {
            builder = builder.tool(tool);
        }
        let mut agent = builder.build();
        agent
            .run(input.into())
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(agent.state().last_assistant_text().unwrap_or_default())
    }
}
