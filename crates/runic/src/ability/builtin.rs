use std::sync::Arc;

use async_trait::async_trait;
use runic_hook::WriteHook;
use runic_skills::SkillSet;
use runic_subagent::{RosterVoice, Subagent};
use runic_substrate::{SearchChatsTool, SessionStore};
use runic_tool::Tool;

use super::{Ability, AbilityBundle, AbilityDraft, BuildCtx, ability};
use crate::tools::{
    AskUserTool, CalculatorTool, ComposioTool, SearchProvider, SystemTimeTool, WeatherHistoryTool,
    WeatherTool, WebFetchTool, WebSearchTool,
};

pub struct Skills(pub Arc<SkillSet>);

#[async_trait]
impl Ability for Skills {
    async fn contribute(
        &self,
        bundle: &mut AbilityBundle,
        _ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        if !self.0.is_empty() {
            bundle.skill_set(self.0.clone());
        }
        Ok(())
    }
}

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
impl Ability for Delegation {
    async fn contribute(
        &self,
        bundle: &mut AbilityBundle,
        _ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        for subagent in &self.subagents {
            bundle.subagent(subagent.clone());
        }
        bundle.delegation_voice.merge_first_wins(&self.voice);
        Ok(())
    }
}

pub fn search_chats(store: Arc<dyn SessionStore>) -> AbilityDraft {
    ability("search-chats")
        .describe("search this tenant's other conversations")
        .tool(SearchChatsTool::new(store))
}

pub fn basics() -> AbilityDraft {
    ability("basics").tool(CalculatorTool).tool(SystemTimeTool)
}

pub fn ask_user() -> AbilityDraft {
    ability("ask-user").tool(AskUserTool)
}

pub fn web_fetch() -> AbilityDraft {
    ability("web-fetch")
        .describe("fetch a URL (SSRF-guarded)")
        .tool(WebFetchTool::new())
}

pub fn web_search(provider: Arc<dyn SearchProvider>) -> AbilityDraft {
    ability("web-search")
        .describe("search the web")
        .tool(WebSearchTool::new(provider))
}

pub fn weather() -> AbilityDraft {
    ability("weather")
        .describe("current + historical weather, keyless")
        .tool(WeatherTool::new())
        .tool(WeatherHistoryTool::new())
}

pub fn composio(api_key: impl Into<String>, entity_id: Option<String>) -> AbilityDraft {
    ability("composio")
        .describe("1000+ external app actions via Composio")
        .tool(ComposioTool::new(api_key, entity_id))
}

pub struct Hooks(pub Vec<Arc<dyn WriteHook>>);

#[async_trait]
impl Ability for Hooks {
    async fn contribute(
        &self,
        bundle: &mut AbilityBundle,
        _ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        for hook in &self.0 {
            bundle.write_hook(hook.clone());
        }
        Ok(())
    }
}

pub struct Tools(pub Vec<Arc<dyn Tool>>);

#[async_trait]
impl Ability for Tools {
    async fn contribute(
        &self,
        bundle: &mut AbilityBundle,
        _ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        for tool in &self.0 {
            bundle.tool(tool.clone());
        }
        Ok(())
    }
}
