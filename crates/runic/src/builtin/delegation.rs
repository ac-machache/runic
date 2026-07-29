use async_trait::async_trait;

use crate::ability::{Ability, BuildCtx, ToAbility};
use crate::subagent::{RosterVoice, Subagent};

pub struct Delegation {
    subagents: Vec<Subagent>,
    voice: RosterVoice,
}

impl Delegation {
    pub fn new(subagents: impl IntoIterator<Item = Subagent>) -> Self {
        Self {
            subagents: subagents.into_iter().collect(),
            voice: RosterVoice::default(),
        }
    }

    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.voice.tag = Some(tag.into());
        self
    }

    pub fn intro(mut self, text: impl Into<String>) -> Self {
        self.voice.intro = Some(text.into());
        self
    }

    pub fn tool_name(mut self, name: impl Into<String>) -> Self {
        self.voice.tool_name = Some(name.into());
        self
    }

    pub fn tool_description(mut self, text: impl Into<String>) -> Self {
        self.voice.tool_description = Some(text.into());
        self
    }
}

#[async_trait]
impl ToAbility for Delegation {
    async fn to_ability(&self, base: Ability, _ctx: &BuildCtx<'_>) -> anyhow::Result<Ability> {
        let mut base = base.subagents(self.subagents.iter().cloned());
        base.voice(&self.voice);
        Ok(base)
    }
}
