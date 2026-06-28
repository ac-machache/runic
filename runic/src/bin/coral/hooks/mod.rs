//! Tenant-id injection — the runic equivalent of coral's client-side
//! `bound_params`. A `WriteHook` that, before an MCP toolbox tool runs,
//! overwrites the scoped ids in the call from the run's config, so the model
//! never supplies them (and the value never leaks into the prompt).
//!
//! Values are read from `state.config` (the per-run open map), populated per
//! request — the multi-user seam. Scope each agent's hook to its toolset:
//!   Maia → `mcp__coral__` + ["user_id"]; crm-expert → `mcp__crm-expert__` +
//!   ["user_id", "org_id"]; product-expert (ephy) → none.

use async_trait::async_trait;
use runic_hook::{HookOutcome, WriteHook};
use runic_state::AgentState;
use runic_types::ToolCall;

pub struct InjectIds {
    prefix: String,
    ids: Vec<String>,
}

impl InjectIds {
    pub fn new(
        prefix: impl Into<String>,
        ids: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            prefix: prefix.into(),
            ids: ids.into_iter().map(Into::into).collect(),
        }
    }
}

#[async_trait]
impl WriteHook for InjectIds {
    fn name(&self) -> &str {
        "inject-ids"
    }

    async fn before_tool(&self, state: &mut AgentState, call: &mut ToolCall) -> HookOutcome {
        if !call.name.starts_with(&self.prefix) {
            return HookOutcome::Continue;
        }
        if !call.input.is_object() {
            call.input = serde_json::Value::Object(serde_json::Map::new());
        }
        let obj = call.input.as_object_mut().expect("input is an object");
        for id in &self.ids {
            if let Some(value) = state.config.get(id).cloned() {
                obj.insert(id.clone(), value);
            } else {
                tracing::warn!(id = %id, tool = %call.name, "id not in run config — tool call will be unscoped");
            }
        }
        HookOutcome::Continue
    }
}

/// Forces composio's `entity_id` to the run's `user_id` (from `state.config`),
/// so the model acts only as its own user's connected accounts — never one it
/// names itself. Composio is the orchestrator's tool, so this rides on Maia.
pub struct ComposioEntity;

#[async_trait]
impl WriteHook for ComposioEntity {
    fn name(&self) -> &str {
        "composio-entity"
    }

    async fn before_tool(&self, state: &mut AgentState, call: &mut ToolCall) -> HookOutcome {
        if call.name != "composio" {
            return HookOutcome::Continue;
        }
        let Some(user_id) = state.config.get("user_id").cloned() else {
            tracing::warn!("user_id not in run config — composio call will use the default entity");
            return HookOutcome::Continue;
        };
        if !call.input.is_object() {
            call.input = serde_json::Value::Object(serde_json::Map::new());
        }
        call.input
            .as_object_mut()
            .expect("input is an object")
            .insert("entity_id".into(), user_id);
        HookOutcome::Continue
    }
}
