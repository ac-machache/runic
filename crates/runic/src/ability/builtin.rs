use std::sync::Arc;

use crate::subagent::{RosterVoice, Subagent};
use async_trait::async_trait;
use runic_substrate::{SearchChatsTool, SessionStore};

use super::{Ability, BuildCtx, ToAbility, ability};
use crate::tools::{
    AskUserTool, CalculatorTool, ComposioTool, SearchProvider, SystemTimeTool, WeatherHistoryTool,
    WeatherTool, WebFetchTool, WebSearchTool,
};

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

pub fn search_chats(store: Arc<dyn SessionStore>) -> Ability {
    ability("search-chats")
        .describe("search this tenant's other conversations")
        .tool(SearchChatsTool::new(store))
}

pub fn basics() -> Ability {
    ability("basics").tool(CalculatorTool).tool(SystemTimeTool)
}

pub fn ask_user() -> Ability {
    ability("ask-user").tool(AskUserTool)
}

pub fn web_fetch() -> Ability {
    ability("web-fetch")
        .describe("fetch a URL (SSRF-guarded)")
        .tool(WebFetchTool::new())
}

pub fn web_search(provider: Arc<dyn SearchProvider>) -> Ability {
    ability("web-search")
        .describe("search the web")
        .tool(WebSearchTool::new(provider))
}

pub fn weather() -> Ability {
    ability("weather")
        .describe("current + historical weather, keyless")
        .tool(WeatherTool::new())
        .tool(WeatherHistoryTool::new())
}

pub fn composio(api_key: impl Into<String>, entity_id: Option<String>) -> Ability {
    ability("composio")
        .describe("1000+ external app actions via Composio")
        .tool(ComposioTool::new(api_key, entity_id))
}
