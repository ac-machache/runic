use crate::subagent::{RosterVoice, Subagent};
use runic_hook::WriteHook;
use runic_skills::SkillSet;
use runic_tool::{Tool, ToolCatalog};
use std::sync::Arc;

use super::Layer;

#[derive(Default)]
pub struct AbilityBundle {
    pub prompt: Vec<(Layer, String)>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub write_hooks: Vec<Arc<dyn WriteHook>>,
    pub tool_catalog: Option<Arc<dyn ToolCatalog>>,
    pub skills: Vec<Arc<SkillSet>>,
    pub subagents: Vec<Subagent>,
    pub delegation_voice: RosterVoice,
}

impl AbilityBundle {
    pub fn prompt(&mut self, layer: Layer, text: impl Into<String>) {
        self.prompt.push((layer, text.into()));
    }

    pub fn tool(&mut self, tool: Arc<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn write_hook(&mut self, hook: Arc<dyn WriteHook>) {
        self.write_hooks.push(hook);
    }

    pub fn tool_catalog(&mut self, catalog: Arc<dyn ToolCatalog>) {
        self.tool_catalog = Some(catalog);
    }

    pub fn skill_set(&mut self, set: Arc<SkillSet>) {
        self.skills.push(set);
    }

    pub fn subagent(&mut self, def: Subagent) {
        self.subagents.push(def);
    }
}
