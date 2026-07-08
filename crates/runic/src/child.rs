use std::sync::Arc;

use async_trait::async_trait;
use runic_agent::Agent;
use runic_provider::Provider;
use runic_skills::SkillSet;
use runic_subagent::{AgentDef, DelegationCtx, SubagentBuilder};
use runic_tools::default_tools;

pub struct FoundrySubagentBuilder {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub skills: Option<Arc<SkillSet>>,
}

#[async_trait]
impl SubagentBuilder for FoundrySubagentBuilder {
    async fn build(&self, def: &AgentDef, _dctx: &DelegationCtx) -> anyhow::Result<Agent> {
        let model = def.model.clone().unwrap_or_else(|| self.model.clone());
        let scoped = self
            .skills
            .as_ref()
            .filter(|_| !def.skills.is_empty())
            .map(|catalog| Arc::new(catalog.scope_glob(&def.skills)))
            .filter(|set| !set.is_empty());

        let mut prompt = def.system_prompt.clone();
        if let Some(set) = &scoped {
            prompt = format!("{prompt}\n\n{}", set.prompt_section());
        }

        let mut b = Agent::builder(self.provider.clone(), "subagent", &def.name)
            .model(model)
            .system_prompt(prompt);
        for t in def.scope_tools(&default_tools()) {
            b = b.tool(t);
        }
        if let Some(set) = &scoped
            && let Some(tool) = set.view_tool()
        {
            b = b.tool(tool);
        }
        if let Some(max_turns) = def.max_turns {
            b = b.max_turns(max_turns);
        }
        Ok(b.build())
    }
}
