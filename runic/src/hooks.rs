//! Hooks wired into the agent: context binding for MCP toolbox calls and
//! a lifecycle logger. Moved out of `main.rs` so both the REPL and the
//! server build the same agent.

use runic_agent_core::{AgentState, Hook, HookOutcome, ToolResult};
use runic_message_types::ToolCall;

fn compact_json(v: &serde_json::Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "<unprintable>".into())
}

/// Binds `user_id` (and `org_id` for create_* tools) into every MCP
/// toolbox call. Models the coral `bound_params` pattern: the agent never
/// sees these values — they're stamped into `call.input` right before
/// dispatch, so the LLM can't invent or leak them.
pub struct BindUserContextHook {
    pub user_id: String,
    pub org_id: String,
}

/// Tools from the `coral` toolset that take `org_id` in addition to
/// `user_id`.
const TOOLS_NEEDING_ORG_ID: &[&str] = &[
    "mcp__toolbox__create_farm",
    "mcp__toolbox__create_farmer",
];

#[async_trait::async_trait]
impl Hook for BindUserContextHook {
    fn name(&self) -> &'static str {
        "bind_user_context"
    }

    async fn before_tool(&self, _state: &mut AgentState, call: &mut ToolCall) -> HookOutcome {
        if !call.name.starts_with("mcp__toolbox__") {
            return HookOutcome::Continue;
        }
        let Some(obj) = call.input.as_object_mut() else {
            return HookOutcome::Continue;
        };
        if !self.user_id.is_empty() {
            obj.insert(
                "user_id".to_string(),
                serde_json::Value::String(self.user_id.clone()),
            );
        }
        if TOOLS_NEEDING_ORG_ID.contains(&call.name.as_str()) && !self.org_id.is_empty() {
            obj.insert(
                "org_id".to_string(),
                serde_json::Value::String(self.org_id.clone()),
            );
        }
        HookOutcome::Continue
    }
}

/// Demo hook — prints every lifecycle event to stderr. Useful for
/// visually confirming hook ordering in the REPL.
pub struct LoggingHook;

#[async_trait::async_trait]
impl Hook for LoggingHook {
    fn name(&self) -> &'static str {
        "logging"
    }

    async fn before_agent(&self, state: &mut AgentState) -> HookOutcome {
        eprintln!("  [hook] before_agent  | events_so_far={}", state.events.len());
        HookOutcome::Continue
    }

    async fn after_agent(&self, state: &mut AgentState) -> HookOutcome {
        eprintln!("  [hook] after_agent   | events_now={}", state.events.len());
        HookOutcome::Continue
    }

    async fn before_model(&self, _state: &mut AgentState) -> HookOutcome {
        eprintln!("  [hook] before_model");
        HookOutcome::Continue
    }

    async fn after_model(&self, _state: &mut AgentState, stop_reason: Option<&str>) -> HookOutcome {
        eprintln!("  [hook] after_model   | stop={:?}", stop_reason);
        HookOutcome::Continue
    }

    async fn before_tool(&self, _state: &mut AgentState, call: &mut ToolCall) -> HookOutcome {
        eprintln!(
            "  [hook] before_tool   | tool={} input={}",
            call.name,
            compact_json(&call.input)
        );
        HookOutcome::Continue
    }

    async fn after_tool(
        &self,
        _state: &mut AgentState,
        call: &ToolCall,
        result: &ToolResult,
    ) -> HookOutcome {
        eprintln!(
            "  [hook] after_tool    | tool={} is_error={} content_chars={}",
            call.name,
            result.is_error,
            result.content.chars().count()
        );
        HookOutcome::Continue
    }
}
