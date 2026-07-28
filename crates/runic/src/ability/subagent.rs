use std::sync::Arc;

use async_trait::async_trait;
use runic_agent::Llm;
use runic_hook::WriteHook;
use runic_provider::Provider;
use runic_skills::SkillSet;
use runic_tool::Tool;

use super::builtin::{Hooks, Skills, Tools};
use super::{Ability, AbilityBundle, AbilityDescriptor, ActivationPolicy, BuildCtx};
use crate::composer::Agent;
use crate::models;
use crate::subagent::Subagent;

pub fn subagent(name: impl Into<String>, description: impl Into<String>) -> SubagentDraft {
    SubagentDraft {
        name: name.into(),
        description: description.into(),
        activation: ActivationPolicy::Eager,
        agent: None,
        abilities: Vec::new(),
        provider: None,
        provider_name: None,
        model: None,
        prompt: String::new(),
        max_turns: None,
    }
}

pub struct SubagentDraft {
    name: String,
    description: String,
    activation: ActivationPolicy,
    agent: Option<Agent>,
    abilities: Vec<Arc<dyn Ability>>,
    provider: Option<Arc<dyn Provider>>,
    provider_name: Option<String>,
    model: Option<String>,
    prompt: String,
    max_turns: Option<u32>,
}

impl SubagentDraft {
    pub fn agent(mut self, agent: Agent) -> Self {
        self.agent = Some(agent);
        self
    }

    pub fn prompt(mut self, text: impl Into<String>) -> Self {
        let text = text.into();
        if self.prompt.is_empty() {
            self.prompt = text;
        } else {
            self.prompt = format!("{}\n\n{text}", self.prompt);
        }
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn provider(mut self, provider: Arc<dyn Provider>) -> Self {
        self.provider = Some(provider);
        self
    }

    pub fn provider_named(mut self, name: impl Into<String>) -> Self {
        self.provider_name = Some(name.into());
        self
    }

    pub fn max_turns(mut self, turns: u32) -> Self {
        self.max_turns = Some(turns);
        self
    }

    pub fn deferred(mut self) -> Self {
        self.activation = ActivationPolicy::Deferred;
        self
    }

    pub fn with(mut self, ability: impl Ability + 'static) -> Self {
        self.abilities.push(Arc::new(ability));
        self
    }

    pub fn tool(self, tool: impl Tool + 'static) -> Self {
        self.with(Tools(vec![Arc::new(tool)]))
    }

    pub fn hook(self, hook: impl WriteHook + 'static) -> Self {
        self.with(Hooks(vec![Arc::new(hook)]))
    }

    pub fn skills(self, set: Arc<SkillSet>) -> Self {
        self.with(Skills(set))
    }

    fn build_agent(&self, ctx: &BuildCtx<'_>) -> anyhow::Result<Agent> {
        if let Some(agent) = &self.agent {
            return Ok(agent.clone());
        }
        let provider = match (&self.provider, &self.provider_name) {
            (Some(explicit), _) => explicit.clone(),
            (None, Some(name)) => models::build_provider(name)?,
            (None, None) => ctx.provider.clone(),
        };
        let model = self.model.clone().unwrap_or_else(|| ctx.model.to_string());
        let mut llm = Llm::new(provider, model);
        if !self.prompt.is_empty() {
            llm = llm.instructions(&self.prompt);
        }
        if let Some(turns) = self.max_turns {
            llm = llm.max_turns(turns);
        }
        let mut agent = Agent::new(llm);
        for ability in &self.abilities {
            agent = agent.with_arc(ability.clone());
        }
        Ok(agent)
    }
}

#[async_trait]
impl Ability for SubagentDraft {
    fn name(&self) -> &str {
        &self.name
    }

    fn descriptor(&self) -> AbilityDescriptor {
        match self.activation {
            ActivationPolicy::Eager => AbilityDescriptor::eager(),
            ActivationPolicy::Deferred => {
                AbilityDescriptor::deferred(self.name.clone(), self.description.clone())
            }
        }
    }

    async fn contribute(
        &self,
        bundle: &mut AbilityBundle,
        ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        let agent = self.build_agent(ctx)?;
        bundle.subagent(Subagent::new(
            self.name.clone(),
            self.description.clone(),
            agent,
        ));
        Ok(())
    }
}
