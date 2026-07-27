use std::sync::Arc;

use crate::subagent::Subagent;
use async_trait::async_trait;
use runic_hook::WriteHook;
use runic_skills::SkillSet;
use runic_tool::{Tool, ToolCatalog};

use super::subagent::SubagentDraft;
use super::{Ability, AbilityBundle, AbilityDescriptor, ActivationPolicy, BuildCtx, Layer};

pub fn ability(id: impl Into<String>) -> AbilityDraft {
    AbilityDraft {
        id: id.into(),
        description: None,
        activation: ActivationPolicy::Eager,
        prompt: Vec::new(),
        tools: Vec::new(),
        hooks: Vec::new(),
        skills: Vec::new(),
        subagents: Vec::new(),
        nested: Vec::new(),
        tool_catalog: None,
    }
}

pub struct AbilityDraft {
    id: String,
    description: Option<String>,
    activation: ActivationPolicy,
    prompt: Vec<(Layer, String)>,
    tools: Vec<Arc<dyn Tool>>,
    hooks: Vec<Arc<dyn WriteHook>>,
    skills: Vec<Arc<SkillSet>>,
    subagents: Vec<Subagent>,
    nested: Vec<Arc<dyn Ability>>,
    tool_catalog: Option<Arc<dyn ToolCatalog>>,
}

impl AbilityDraft {
    pub fn describe(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }

    pub fn deferred(mut self) -> Self {
        self.activation = ActivationPolicy::Deferred;
        self
    }

    pub fn prompt(mut self, text: impl Into<String>) -> Self {
        self.prompt.push((Layer::Stable, text.into()));
        self
    }

    pub fn volatile_prompt(mut self, text: impl Into<String>) -> Self {
        self.prompt.push((Layer::Volatile, text.into()));
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

    pub fn hook(mut self, hook: impl WriteHook + 'static) -> Self {
        self.hooks.push(Arc::new(hook));
        self
    }

    pub fn hooks(mut self, hooks: impl IntoIterator<Item = Arc<dyn WriteHook>>) -> Self {
        self.hooks.extend(hooks);
        self
    }

    pub fn skills(mut self, set: Arc<SkillSet>) -> Self {
        self.skills.push(set);
        self
    }

    pub fn subagent(mut self, draft: SubagentDraft) -> Self {
        self.nested.push(Arc::new(draft));
        self
    }

    pub fn subagent_def(mut self, subagent: Subagent) -> Self {
        self.subagents.push(subagent);
        self
    }

    pub fn subagents(mut self, subagents: impl IntoIterator<Item = Subagent>) -> Self {
        self.subagents.extend(subagents);
        self
    }

    pub fn tool_catalog(mut self, catalog: Arc<dyn ToolCatalog>) -> Self {
        self.tool_catalog = Some(catalog);
        self
    }
}

#[async_trait]
impl Ability for AbilityDraft {
    fn name(&self) -> &str {
        &self.id
    }

    fn descriptor(&self) -> AbilityDescriptor {
        AbilityDescriptor {
            id: Some(self.id.clone()),
            description: self.description.clone(),
            activation: self.activation,
        }
    }

    async fn contribute(
        &self,
        bundle: &mut AbilityBundle,
        ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        for (layer, text) in &self.prompt {
            bundle.prompt(*layer, text.clone());
        }
        for tool in &self.tools {
            bundle.tool(tool.clone());
        }
        for hook in &self.hooks {
            bundle.write_hook(hook.clone());
        }
        for set in &self.skills {
            bundle.skill_set(set.clone());
        }
        for def in &self.subagents {
            bundle.subagent(def.clone());
        }
        for nested in &self.nested {
            if nested.descriptor().activation == ActivationPolicy::Deferred {
                anyhow::bail!(
                    "ability `{}`: nested subagent `{}` must not be deferred — the outer ability controls activation",
                    self.id,
                    nested.name()
                );
            }
            nested.contribute(bundle, ctx).await?;
        }
        if let Some(catalog) = &self.tool_catalog {
            bundle.tool_catalog(catalog.clone());
        }
        Ok(())
    }
}
