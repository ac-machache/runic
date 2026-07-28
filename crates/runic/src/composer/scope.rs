use std::collections::HashSet;
use std::sync::Arc;

use runic_hook::{HookScope, WriteHook};
use runic_types::ToolCall;

use crate::deferred::LoadedAbilities;

pub(super) struct Routed {
    pub(super) tool: String,
    pub(super) extract: fn(&serde_json::Value) -> Vec<String>,
    pub(super) owned: HashSet<String>,
}

pub(super) struct AbilityScope {
    owner: String,
    deferred_id: Option<String>,
    loaded: LoadedAbilities,
    tools: HashSet<String>,
    routed: Vec<Routed>,
}

impl HookScope for AbilityScope {
    fn owner(&self) -> &str {
        &self.owner
    }

    fn active(&self) -> bool {
        match &self.deferred_id {
            Some(id) => self.loaded.contains(id),
            None => true,
        }
    }

    fn owns_call(&self, call: &ToolCall) -> bool {
        if self.tools.contains(&call.name) {
            return true;
        }
        self.routed.iter().any(|routed| {
            routed.tool == call.name
                && (routed.extract)(&call.input)
                    .iter()
                    .any(|subject| routed.owned.contains(subject))
        })
    }
}

pub(super) struct PendingHooks {
    pub(super) ability: String,
    pub(super) deferred_id: Option<String>,
    pub(super) hooks: Vec<Arc<dyn WriteHook>>,
    pub(super) tools: HashSet<String>,
    pub(super) subagents: HashSet<String>,
    pub(super) skills: HashSet<String>,
}

impl PendingHooks {
    pub(super) fn into_scope(
        self,
        loaded: &LoadedAbilities,
        delegate_tool: Option<&str>,
        skill_tool: Option<&str>,
    ) -> (Vec<Arc<dyn WriteHook>>, Arc<dyn HookScope>) {
        let mut routed = Vec::new();
        if let Some(tool) = delegate_tool
            && !self.subagents.is_empty()
        {
            routed.push(Routed {
                tool: tool.to_string(),
                extract: crate::deferred::delegate_subjects,
                owned: self.subagents,
            });
        }
        if let Some(tool) = skill_tool
            && !self.skills.is_empty()
        {
            routed.push(Routed {
                tool: tool.to_string(),
                extract: crate::deferred::skill_subjects,
                owned: self.skills,
            });
        }
        let scope = AbilityScope {
            owner: self.ability,
            deferred_id: self.deferred_id,
            loaded: loaded.clone(),
            tools: self.tools,
            routed,
        };
        (self.hooks, Arc::new(scope))
    }
}
