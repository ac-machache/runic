use std::sync::Arc;

use runic_agent::Llm;
use runic_hook::WriteHook;
use runic_skills::SkillSet;
use runic_tool::Tool;

use crate::Input;
use crate::ability::{Ability, ToAbility};
use crate::subagent::Subagent;

pub(crate) const AGENT_BUNDLE_ID: &str = "agent";

#[derive(Clone)]
pub struct Agent {
    pub(crate) llm: Llm,
    pub(crate) base: Ability,
    pub(crate) abilities: Vec<Arc<dyn ToAbility>>,
    pub(crate) output_schema: Option<serde_json::Value>,
}

impl Agent {
    pub fn new(llm: Llm) -> Self {
        Self {
            llm,
            base: Ability::new(AGENT_BUNDLE_ID),
            abilities: Vec::new(),
            output_schema: None,
        }
    }

    pub fn model(&self) -> &str {
        &self.llm.config().model
    }

    pub fn tool(mut self, tool: impl Tool + 'static) -> Self {
        self.base = self.base.tool(tool);
        self
    }

    pub fn tools(mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> Self {
        self.base = self.base.tools(tools);
        self
    }

    pub fn hook(mut self, hook: impl WriteHook + 'static) -> Self {
        self.base = self.base.hook(hook);
        self
    }

    pub fn hooks(mut self, hooks: impl IntoIterator<Item = Arc<dyn WriteHook>>) -> Self {
        self.base = self.base.hooks(hooks);
        self
    }

    pub fn skills(mut self, set: Arc<SkillSet>) -> Self {
        self.base = self.base.skills(set);
        self
    }

    pub fn subagent(mut self, subagent: Subagent) -> Self {
        self.base = self.base.subagent(subagent);
        self
    }

    pub fn subagents(mut self, subagents: impl IntoIterator<Item = Subagent>) -> Self {
        self.base = self.base.subagents(subagents);
        self
    }

    pub fn with(mut self, ability: impl ToAbility + 'static) -> Self {
        self.abilities.push(Arc::new(ability));
        self
    }

    pub fn with_arc(mut self, ability: Arc<dyn ToAbility>) -> Self {
        self.abilities.push(ability);
        self
    }

    pub fn output<T: schemars::JsonSchema>(self) -> Self {
        let schema = runic_agent::schema_of::<T>();
        self.output_schema(schema)
    }

    pub fn output_schema(mut self, schema: serde_json::Value) -> Self {
        self.output_schema = Some(schema);
        self
    }

    pub async fn build(
        &self,
        tenant: &str,
        session: &str,
    ) -> Result<runic_agent::Runner, super::ComposeError> {
        super::Composer::new(self.clone())
            .build(tenant, session)
            .await
    }

    pub async fn run(&self, input: Input) -> anyhow::Result<AgentOutput> {
        let (message, ctx) = input.split();
        let mut runner = self.build("local", "local").await?;
        let outcome = runner
            .run_message_with(message, ctx)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(AgentOutput::from_run(&runner, outcome))
    }
}

pub struct AgentOutput {
    pub text: String,
    pub outcome: runic_agent::RunOutcome,
}

impl AgentOutput {
    pub(crate) fn from_run(runner: &runic_agent::Runner, outcome: runic_agent::RunOutcome) -> Self {
        Self {
            text: runner.state().last_assistant_text().unwrap_or_default(),
            outcome,
        }
    }

    pub fn parse<T: serde::de::DeserializeOwned>(&self) -> anyhow::Result<T> {
        let Some(value) = &self.outcome.structured else {
            anyhow::bail!(
                "no structured output (stop_reason: {:?})",
                self.outcome.stop_reason
            );
        };
        Ok(serde_json::from_value(value.clone())?)
    }
}
