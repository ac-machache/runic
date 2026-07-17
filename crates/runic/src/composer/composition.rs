use std::sync::Arc;

use runic_hook::WriteHook;
use runic_skills::SkillSet;
use runic_subagent::Subagent;
use runic_tool::{Tool, ToolCatalog};

use crate::ability::AbilityBundle;

#[derive(Default)]
pub struct Composition {
    pub(super) prompt: crate::context::Context,
    pub(super) tools: Vec<Arc<dyn Tool>>,
    pub(super) write_hooks: Vec<Arc<dyn WriteHook>>,
    pub(super) tool_catalogs: Vec<Arc<dyn ToolCatalog>>,
    pub(super) skills: Vec<Arc<SkillSet>>,
    pub(super) subagents: Vec<Subagent>,
}

impl Composition {
    pub(super) fn merge(&mut self, bundle: AbilityBundle) {
        for (layer, text) in bundle.prompt {
            self.prompt.fragment(layer, text);
        }
        self.tools.extend(bundle.tools);
        self.write_hooks.extend(bundle.write_hooks);
        if let Some(catalog) = bundle.tool_catalog {
            self.tool_catalogs.push(catalog);
        }
        self.skills.extend(bundle.skills);
        self.subagents.extend(bundle.subagents);
    }
}
