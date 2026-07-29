use runic_macros::ability;

use crate::ability::{Ability, BuildCtx};
use crate::subagent::{DelegationLabels, Subagent};

#[ability]
pub struct Delegation {
    subagents: Vec<Subagent>,
    labels: DelegationLabels,
}

impl Delegation {
    pub fn new(subagents: impl IntoIterator<Item = Subagent>) -> Self {
        Self {
            subagents: subagents.into_iter().collect(),
            labels: DelegationLabels::default(),
        }
    }

    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.labels.tag = Some(tag.into());
        self
    }

    pub fn intro(mut self, text: impl Into<String>) -> Self {
        self.labels.intro = Some(text.into());
        self
    }

    pub fn tool_name(mut self, name: impl Into<String>) -> Self {
        self.labels.tool_name = Some(name.into());
        self
    }

    pub fn tool_description(mut self, text: impl Into<String>) -> Self {
        self.labels.tool_description = Some(text.into());
        self
    }

    async fn ability(&self, base: Ability, _ctx: &BuildCtx<'_>) -> anyhow::Result<Ability> {
        let mut base = base.subagents(self.subagents.iter().cloned());
        base.labels(&self.labels);
        Ok(base)
    }
}
