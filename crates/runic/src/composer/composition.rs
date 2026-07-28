use std::sync::Arc;

use crate::subagent::{RosterVoice, Subagent};
use runic_skills::SkillSet;
use runic_tool::{Tool, ToolCatalog};

use crate::ability::AbilityBundle;

#[derive(Default)]
pub struct Composition {
    pub(super) prompt: crate::context::Context,
    pub(super) tools: Vec<Arc<dyn Tool>>,
    pub(super) tool_catalogs: Vec<Arc<dyn ToolCatalog>>,
    pub(super) skills: Vec<Arc<SkillSet>>,
    pub(super) subagents: Vec<Subagent>,
    pub(super) delegation_voice: RosterVoice,
}

impl Composition {
    pub(super) fn merge(&mut self, bundle: AbilityBundle) {
        for (layer, text) in bundle.prompt {
            self.prompt.fragment(layer, text);
        }
        self.tools.extend(bundle.tools);
        if let Some(catalog) = bundle.tool_catalog {
            self.tool_catalogs.push(catalog);
        }
        self.skills.extend(bundle.skills);
        self.subagents.extend(bundle.subagents);
        self.delegation_voice
            .merge_first_wins(&bundle.delegation_voice);
    }
}
