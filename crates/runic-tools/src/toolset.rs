//! A configurable builder over the native tools: the always-on base set
//! ([`default_tools`](crate::default_tools)) plus opt-in web / weather /
//! composio / HITL tools.

use std::sync::Arc;

use runic_filesystem::FilesystemBackend;
use runic_tool::Tool;

use crate::{
    AskUserTool, ComposioTool, EscalateToHumanTool, WeatherHistoryTool, WeatherTool, WebFetchTool,
    default_tools,
};

/// Start configuring a native tool set. The base set (read/write/edit/ls/glob/
/// grep + apply_patch + calculator + system_time) is always included by
/// [`Tools::collect`]; the rest are opt-in.
pub fn tools() -> Tools {
    Tools {
        web: false,
        weather: false,
        composio: None,
        hitl: false,
    }
}

pub struct Tools {
    web: bool,
    weather: bool,
    composio: Option<(String, Option<String>)>,
    hitl: bool,
}

impl Tools {
    pub fn web(mut self) -> Self {
        self.web = true;
        self
    }
    pub fn weather(mut self) -> Self {
        self.weather = true;
        self
    }
    pub fn composio(mut self, api_key: impl Into<String>, entity_id: Option<String>) -> Self {
        self.composio = Some((api_key.into(), entity_id));
        self
    }
    pub fn hitl(mut self) -> Self {
        self.hitl = true;
        self
    }

    /// Build the tool set over `workspace` (the agent's filesystem).
    pub fn collect(&self, workspace: Arc<dyn FilesystemBackend>) -> Vec<Arc<dyn Tool>> {
        let mut out = default_tools(workspace);
        if self.web {
            out.push(Arc::new(WebFetchTool::new()));
        }
        if self.weather {
            out.push(Arc::new(WeatherTool::new()));
            out.push(Arc::new(WeatherHistoryTool::new()));
        }
        if let Some((key, entity)) = &self.composio {
            out.push(Arc::new(ComposioTool::new(key.clone(), entity.clone())));
        }
        if self.hitl {
            out.push(Arc::new(AskUserTool));
            out.push(Arc::new(EscalateToHumanTool));
        }
        tracing::info!(
            count = out.len(),
            web = self.web,
            weather = self.weather,
            composio = self.composio.is_some(),
            hitl = self.hitl,
            "internal tools collected"
        );
        out
    }
}
