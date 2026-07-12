use std::fmt::Write;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use runic_skills::SkillSet;
use runic_state::{ExternalEvents, SessionEvent};
use runic_subagent::AgentRoster;
use runic_tool::{Tool, ToolContext, ToolResult, activated_key};

use super::gate::LoadedAbilities;
use super::registry::{AbilityRegistry, ability_activated_key};

pub(crate) const LOAD_ABILITY_TOOL_NAME: &str = "load_ability";

pub(crate) struct LoadAbilityTool {
    registry: Arc<AbilityRegistry>,
    loaded: LoadedAbilities,
}

impl LoadAbilityTool {
    pub(crate) fn new(registry: Arc<AbilityRegistry>, loaded: LoadedAbilities) -> Self {
        Self { registry, loaded }
    }
}

#[async_trait]
impl Tool for LoadAbilityTool {
    fn name(&self) -> &str {
        LOAD_ABILITY_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Load a deferred ability by id to unlock its instructions, tools, skills, and subagents."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The id of the ability to load, from the <deferred-abilities> catalog."
                }
            },
            "required": ["id"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let id = args
            .get("id")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .trim();
        if id.is_empty() {
            return Ok(ToolResult::error("id parameter is required"));
        }
        if self.loaded.contains(id) {
            return Ok(ToolResult::ok(format!(
                "Ability `{id}` is already available. Use its instructions and tools directly; do not load it again."
            )));
        }
        let Some(entry) = self.registry.get(id) else {
            return Ok(ToolResult::error(format!(
                "No ability with id `{id}`. Available: {}",
                self.registry.available_ids().join(", ")
            )));
        };

        let tool_names: Vec<&str> = entry.bundle.tools.iter().map(|tool| tool.name()).collect();
        match ctx.get::<ExternalEvents>() {
            Some(events) => {
                let emit = |key: String| {
                    events.emit(SessionEvent::StateUpdated {
                        run_id: ctx.run_id.clone(),
                        key,
                        value: serde_json::Value::Bool(true),
                        at: Utc::now(),
                    })
                };
                emit(ability_activated_key(id));
                for name in &tool_names {
                    emit(activated_key(name));
                }
            }
            None => {
                tracing::warn!(ability = %id, "no event rail in tool context — activation not recorded");
            }
        }
        self.loaded.mark(id);

        let mut output = format!("Ability `{id}` loaded.\n\n");
        for (_, text) in &entry.bundle.prompt {
            output.push_str(text);
            output.push_str("\n\n");
        }
        if !entry.bundle.skills.is_empty() {
            output.push_str("Unlocked skills (view with the `skill_view` tool):\n");
            output.push_str(&SkillSet::merge(entry.bundle.skills.iter().cloned()).prompt_section());
            output.push_str("\n\n");
        }
        if !entry.bundle.subagents.is_empty() {
            output.push_str("Unlocked subagents (dispatch with the `delegate` tool):\n");
            output.push_str(&AgentRoster::new(entry.bundle.subagents.clone()).roster_lines());
            output.push_str("\n\n");
        }
        if !entry.bundle.tools.is_empty() {
            output.push_str("Unlocked tools (callable from your next turn):\n<functions>\n");
            for tool in &entry.bundle.tools {
                let spec = tool.spec();
                let _ = writeln!(
                    output,
                    "<function>{{\"name\": \"{}\", \"description\": \"{}\", \"parameters\": {}}}</function>",
                    spec.name,
                    spec.description.replace('"', "\\\""),
                    spec.parameters
                );
            }
            output.push_str("</functions>");
        }
        Ok(ToolResult::ok(output.trim_end().to_string()))
    }
}
