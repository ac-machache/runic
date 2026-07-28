use std::sync::Arc;

use runic_tool::{Tool, ToolCatalog};

use crate::ability::Ability;

pub const ABILITY_ACTIVATED_PREFIX: &str = "abilities/activated/";

pub fn ability_activated_key(id: &str) -> String {
    format!("{ABILITY_ACTIVATED_PREFIX}{id}")
}

pub fn activated_ability_ids(data: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    data.iter()
        .filter(|(_, value)| value.as_bool().unwrap_or(false))
        .filter_map(|(key, _)| key.strip_prefix(ABILITY_ACTIVATED_PREFIX))
        .map(str::to_string)
        .collect()
}

pub(crate) struct DeferredEntry {
    pub(crate) id: String,
    pub(crate) description: String,
    pub(crate) parts: Ability,
}

#[derive(Default)]
pub(crate) struct AbilityRegistry {
    pub(crate) entries: Vec<DeferredEntry>,
}

impl AbilityRegistry {
    pub(crate) fn get(&self, id: &str) -> Option<&DeferredEntry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    pub(crate) fn available_ids(&self) -> Vec<&str> {
        self.entries.iter().map(|entry| entry.id.as_str()).collect()
    }

    pub(crate) fn catalog_section(&self) -> String {
        let mut section = String::from(
            "<deferred-abilities>\nThese abilities are available but not loaded. \
             Load one with the `load_ability` tool to unlock its instructions and tools:\n",
        );
        for entry in &self.entries {
            if entry.description.is_empty() {
                section.push_str(&format!("- {}\n", entry.id));
            } else {
                section.push_str(&format!("- {}: {}\n", entry.id, entry.description));
            }
        }
        section.push_str("</deferred-abilities>");
        section
    }
}

impl ToolCatalog for AbilityRegistry {
    fn resolve(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.entries
            .iter()
            .flat_map(|entry| entry.parts.tools.iter())
            .find(|tool| tool.name() == name)
            .cloned()
    }
}
