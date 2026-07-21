use std::sync::Arc;

use runic_provider::{Provider, ThinkingConfig};
use runic_tool::Tool;
use runic_types::TokenUsage;

use crate::{AgentConfig, AgentEvent, RunContext, RunOutcome, Runner};

pub fn schema_of<T: schemars::JsonSchema>() -> serde_json::Value {
    let mut schema = serde_json::to_value(schemars::schema_for!(T)).unwrap_or_default();
    if let Some(object) = schema.as_object_mut() {
        object.remove("$schema");
        object.remove("title");
    }
    schema
}

#[derive(Clone)]
pub struct Llm {
    provider: Arc<dyn Provider>,
    config: AgentConfig,
    instructions: String,
    tools: Vec<Arc<dyn Tool>>,
}

pub struct LlmOutput {
    pub text: String,
    pub usage: TokenUsage,
    pub stop_reason: Option<String>,
    pub structured: Option<serde_json::Value>,
}

impl LlmOutput {
    pub fn parse<T: serde::de::DeserializeOwned>(&self) -> anyhow::Result<T> {
        let Some(value) = &self.structured else {
            anyhow::bail!(
                "no structured output (stop_reason: {:?}) — was `.structured::<T>()` set?",
                self.stop_reason
            );
        };
        Ok(serde_json::from_value(value.clone())?)
    }
}

impl Llm {
    pub fn new(provider: Arc<dyn Provider>, model: impl Into<String>) -> Self {
        Self {
            provider,
            config: AgentConfig {
                model: model.into(),
                ..AgentConfig::default()
            },
            instructions: String::new(),
            tools: Vec::new(),
        }
    }

    pub fn provider(&self) -> Arc<dyn Provider> {
        self.provider.clone()
    }

    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    pub fn system_prompt(&self) -> &str {
        &self.instructions
    }

    pub fn tool_list(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    pub fn instructions(mut self, text: impl Into<String>) -> Self {
        self.instructions = text.into();
        self
    }

    pub fn temperature(mut self, temperature: f32) -> Self {
        self.config.temperature = temperature;
        self
    }

    pub fn max_tokens(mut self, tokens: u32) -> Self {
        self.config.max_tokens = tokens;
        self
    }

    pub fn max_turns(mut self, turns: u32) -> Self {
        self.config.max_turns = turns;
        self
    }

    pub fn thinking(mut self, enabled: bool) -> Self {
        self.config.thinking = Some(ThinkingConfig {
            enabled,
            budget_tokens: None,
        });
        self
    }

    pub fn tool(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.push(Arc::new(tool));
        self
    }

    pub fn tools(mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> Self {
        self.tools.extend(tools);
        self
    }

    pub fn structured<T: schemars::JsonSchema>(mut self) -> Self {
        self.config.output_schema = Some(schema_of::<T>());
        self
    }

    pub fn output_schema(mut self, schema: serde_json::Value) -> Self {
        self.config.output_schema = Some(schema);
        self
    }

    pub async fn run(&self, message: impl Into<String>) -> anyhow::Result<LlmOutput> {
        let mut agent = self.build_agent();
        let outcome = agent
            .run(message.into())
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Self::output(&agent, outcome))
    }

    pub async fn stream(
        &self,
        message: impl Into<String>,
        events: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
    ) -> anyhow::Result<LlmOutput> {
        let mut agent = self.build_agent();
        let ctx = RunContext::new().with_events(std::sync::Arc::new(crate::ChannelEmitter(events)));
        let outcome = agent
            .run_with(message.into(), ctx)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Self::output(&agent, outcome))
    }

    fn output(agent: &Runner, outcome: RunOutcome) -> LlmOutput {
        LlmOutput {
            text: agent.state().last_assistant_text().unwrap_or_default(),
            usage: outcome.usage,
            stop_reason: outcome.stop_reason,
            structured: outcome.structured,
        }
    }

    fn build_agent(&self) -> Runner {
        let mut builder = Runner::builder(self.provider.clone(), "llm", "llm")
            .system_prompt(self.instructions.clone())
            .config(self.config.clone());
        for tool in &self.tools {
            builder = builder.tool(tool.clone());
        }
        builder.build()
    }
}
