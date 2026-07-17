use std::sync::Arc;

use async_trait::async_trait;
use runic_agent::AgentBuilder;
use runic_hook::WriteHook;
use runic_provider::Provider;
use runic_skills::SkillSet;
use runic_subagent::{Subagent, SubagentBuilder, SubagentReq};
use runic_tool::{Tool, ToolCatalog};

use super::builtin::{Hooks, Skills, Tools};
use super::{Ability, AbilityBundle, AbilityDescriptor, ActivationPolicy, BuildCtx};
use crate::models;

pub fn subagent(name: impl Into<String>, description: impl Into<String>) -> SubagentDraft {
    SubagentDraft {
        def: Subagent::new(name, description),
        activation: ActivationPolicy::Eager,
        abilities: Vec::new(),
        provider: None,
    }
}

pub struct SubagentDraft {
    def: Subagent,
    activation: ActivationPolicy,
    abilities: Vec<Arc<dyn Ability>>,
    provider: Option<Arc<dyn Provider>>,
}

impl SubagentDraft {
    pub fn prompt(mut self, text: impl Into<String>) -> Self {
        self.def = self.def.prompt(text);
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.def = self.def.model(model);
        self
    }

    pub fn provider(mut self, provider: Arc<dyn Provider>) -> Self {
        self.provider = Some(provider);
        self
    }

    pub fn max_turns(mut self, turns: u32) -> Self {
        self.def.max_turns = Some(turns);
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
}

#[async_trait]
impl Ability for SubagentDraft {
    fn name(&self) -> &str {
        &self.def.name
    }

    fn descriptor(&self) -> AbilityDescriptor {
        match self.activation {
            ActivationPolicy::Eager => AbilityDescriptor::eager(),
            ActivationPolicy::Deferred => {
                AbilityDescriptor::deferred(self.def.name.clone(), self.def.description.clone())
            }
        }
    }

    async fn contribute(
        &self,
        bundle: &mut AbilityBundle,
        ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        let provider = match (&self.provider, &self.def.provider) {
            (Some(explicit), _) => explicit.clone(),
            (None, Some(name)) => models::build_provider(name)?,
            (None, None) => ctx.provider.clone(),
        };
        let model = self
            .def
            .model
            .clone()
            .unwrap_or_else(|| ctx.model.to_string());
        let child_ctx = BuildCtx {
            tenant: ctx.tenant,
            session: ctx.session,
            provider: &provider,
            model: &model,
        };

        let mut child = AbilityBundle::default();
        for ability in &self.abilities {
            if ability.descriptor().activation == ActivationPolicy::Deferred {
                anyhow::bail!(
                    "subagent `{}`: ability `{}` is deferred; abilities inside a subagent are always eager",
                    self.def.name,
                    ability.name()
                );
            }
            ability
                .contribute(&mut child, &child_ctx)
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "subagent `{}`: ability `{}` failed: {e}",
                        self.def.name,
                        ability.name()
                    )
                })?;
        }
        if !child.subagents.is_empty() || !child.subagent_builders.is_empty() {
            anyhow::bail!(
                "subagent `{}`: nested subagents are not supported",
                self.def.name
            );
        }

        let owns_nothing = child.tools.is_empty()
            && child.write_hooks.is_empty()
            && child.skills.is_empty()
            && child.prompt.is_empty()
            && child.tool_catalog.is_none()
            && self.provider.is_none()
            && self.def.provider.is_none();
        if owns_nothing {
            bundle.subagent(self.def.clone());
            return Ok(());
        }

        let mut def = self.def.clone();
        if !child.tools.is_empty() {
            def.allowed_tools = vec!["*".to_string()];
        }

        let skills = (!child.skills.is_empty())
            .then(|| Arc::new(SkillSet::merge(child.skills.iter().cloned())));
        if skills.is_some() {
            def.skills = vec!["*".to_string()];
        }

        let fragments: Vec<String> = child.prompt.iter().map(|(_, text)| text.clone()).collect();
        if !fragments.is_empty() {
            let extra = fragments.join("\n\n");
            def.system_prompt = if def.system_prompt.is_empty() {
                extra
            } else {
                format!("{}\n\n{extra}", def.system_prompt)
            };
        }

        let builder = ComposedSubagentBuilder {
            provider,
            model,
            tools: child.tools,
            skills,
            hooks: child.write_hooks,
            tool_catalog: child.tool_catalog,
        };
        bundle.subagent_with(def, Arc::new(builder));
        Ok(())
    }
}

struct ComposedSubagentBuilder {
    provider: Arc<dyn Provider>,
    model: String,
    tools: Vec<Arc<dyn Tool>>,
    skills: Option<Arc<SkillSet>>,
    hooks: Vec<Arc<dyn WriteHook>>,
    tool_catalog: Option<Arc<dyn ToolCatalog>>,
}

#[async_trait]
impl SubagentBuilder for ComposedSubagentBuilder {
    async fn provider(&self, _req: &SubagentReq<'_>) -> Arc<dyn Provider> {
        self.provider.clone()
    }

    fn default_model(&self, _req: &SubagentReq<'_>) -> String {
        self.model.clone()
    }

    async fn tool_pool(&self, _req: &SubagentReq<'_>) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }

    fn skill_catalog(&self, _req: &SubagentReq<'_>) -> Option<Arc<SkillSet>> {
        self.skills.clone()
    }

    fn decorate(&self, mut b: AgentBuilder, _req: &SubagentReq<'_>) -> AgentBuilder {
        for hook in &self.hooks {
            b = b.write_hook(hook.clone());
        }
        if let Some(catalog) = &self.tool_catalog {
            b = b.tool_catalog(catalog.clone());
        }
        b
    }
}
