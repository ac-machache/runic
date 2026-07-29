use std::sync::Arc;

use async_trait::async_trait;
use runic_hook::WriteHook;
use runic_skills::SkillSet;
use runic_tool::{Tool, ToolCatalog};

use super::{AbilityDescriptor, ActivationPolicy, BuildCtx, Layer, ToAbility};
use crate::subagent::{RosterVoice, Subagent};

pub fn ability(id: impl Into<String>) -> Ability {
    Ability::new(id)
}

#[derive(Default, Clone)]
pub struct Ability {
    pub(crate) id: String,
    pub(crate) description: Option<String>,
    pub(crate) activation: ActivationPolicy,
    pub(crate) prompt: Vec<(Layer, String)>,
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    pub(crate) hooks: Vec<Arc<dyn WriteHook>>,
    pub(crate) skills: Vec<Arc<SkillSet>>,
    pub(crate) subagents: Vec<Subagent>,
    pub(crate) tool_catalog: Option<Arc<dyn ToolCatalog>>,
    pub(crate) delegation_voice: RosterVoice,
    nested: Vec<Arc<dyn ToAbility>>,
}

impl Ability {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Self::default()
        }
    }

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

    pub fn subagent(mut self, subagent: Subagent) -> Self {
        self.subagents.push(subagent);
        self
    }

    pub fn subagents(mut self, subagents: impl IntoIterator<Item = Subagent>) -> Self {
        self.subagents.extend(subagents);
        self
    }

    pub fn with(mut self, ability: impl ToAbility + 'static) -> Self {
        self.nested.push(Arc::new(ability));
        self
    }

    pub fn tool_catalog(mut self, catalog: Arc<dyn ToolCatalog>) -> Self {
        self.tool_catalog = Some(catalog);
        self
    }

    pub(crate) fn voice(&mut self, voice: &RosterVoice) {
        self.delegation_voice.merge_first_wins(voice);
    }

    pub(crate) fn carries_nothing(&self) -> bool {
        self.prompt.is_empty()
            && self.tools.is_empty()
            && self.hooks.is_empty()
            && self.skills.is_empty()
            && self.subagents.is_empty()
            && self.nested.is_empty()
            && self.tool_catalog.is_none()
    }

    /// Fold in everything `with()` attached, however deep. A body that returns
    /// an ability carrying nested ones never has to replay them itself.
    pub(crate) async fn resolve(mut self, ctx: &BuildCtx<'_>) -> anyhow::Result<Ability> {
        while !self.nested.is_empty() {
            for nested in std::mem::take(&mut self.nested) {
                self = Box::pin(nested.to_ability(self, ctx)).await?;
            }
        }
        Ok(self)
    }

    /// Fold another ability's contents in, keeping this one's identity.
    fn absorb(&mut self, other: &Ability) {
        self.prompt.extend(other.prompt.iter().cloned());
        self.tools.extend(other.tools.iter().cloned());
        self.hooks.extend(other.hooks.iter().cloned());
        self.skills.extend(other.skills.iter().cloned());
        self.subagents.extend(other.subagents.iter().cloned());
        if let Some(catalog) = &other.tool_catalog {
            self.tool_catalog = Some(catalog.clone());
        }
        self.delegation_voice
            .merge_first_wins(&other.delegation_voice);
    }
}

#[async_trait]
impl ToAbility for Ability {
    fn name(&self) -> &str {
        &self.id
    }

    fn descriptor(&self) -> AbilityDescriptor {
        AbilityDescriptor {
            id: (!self.id.is_empty()).then(|| self.id.clone()),
            description: self.description.clone(),
            activation: self.activation,
        }
    }

    async fn to_ability(&self, mut base: Ability, ctx: &BuildCtx<'_>) -> anyhow::Result<Ability> {
        base.absorb(self);
        base.nested.extend(self.nested.iter().cloned());
        base.resolve(ctx).await
    }
}
