use async_trait::async_trait;
use runic_message_types::ToolCall;

use crate::state::AgentState;
use crate::tool::ToolResult;

/// What a hook tells the loop to do next.
#[derive(Debug, Clone)]
pub enum HookOutcome {
    /// Proceed normally.
    Continue,
    /// (`before_tool` only) Skip execution and use this synthetic result instead.
    /// Useful for human-in-the-loop approval / cached responses / blocking.
    SubstituteToolResult(ToolResult),
    /// Halt the loop immediately. Bubbles up as `AgentError::HookStop`.
    Stop,
}

/// Hook trait inspired by pi's function-fields-on-options-bag pattern, but
/// expressed as a single trait so users can group related callbacks in one
/// type. All methods have default no-op impls — implementors only override
/// the lifecycle points they care about.
#[async_trait]
pub trait Hook: Send + Sync {
    /// Identifier recorded on the `SessionEvent::HookRan` audit entry. Defaults
    /// to the concrete type's name; override for a friendlier label.
    fn name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    /// Fires once at the start of `run` before any model call.
    async fn before_agent(&self, _state: &mut AgentState) -> HookOutcome {
        HookOutcome::Continue
    }

    /// Fires once at the end of `run`, after the loop has terminated.
    async fn after_agent(&self, _state: &mut AgentState) -> HookOutcome {
        HookOutcome::Continue
    }

    /// Fires before each provider `complete` call. Hooks may mutate state
    /// (e.g. inject a system reminder, trim history) before the request goes out.
    async fn before_model(&self, _state: &mut AgentState) -> HookOutcome {
        HookOutcome::Continue
    }

    /// Fires after each provider stream completes (one assistant turn).
    async fn after_model(
        &self,
        _state: &mut AgentState,
        _stop_reason: Option<&str>,
    ) -> HookOutcome {
        HookOutcome::Continue
    }

    /// Fires before each tool dispatch. May mutate the call (rename, rewrite
    /// arguments) or substitute a synthetic result.
    async fn before_tool(&self, _state: &mut AgentState, _call: &mut ToolCall) -> HookOutcome {
        HookOutcome::Continue
    }

    /// Fires after each tool finishes.
    async fn after_tool(
        &self,
        _state: &mut AgentState,
        _call: &ToolCall,
        _result: &ToolResult,
    ) -> HookOutcome {
        HookOutcome::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct LoudHook;

    #[async_trait]
    impl Hook for LoudHook {}

    struct CustomNameHook;

    #[async_trait]
    impl Hook for CustomNameHook {
        fn name(&self) -> &'static str {
            "loud-and-clear"
        }
    }

    #[test]
    fn default_name_returns_type_name() {
        let h = LoudHook;
        // type_name returns the fully-qualified path; just assert it contains "LoudHook"
        assert!(
            h.name().contains("LoudHook"),
            "expected type name to contain LoudHook, got {}",
            h.name()
        );
    }

    #[test]
    fn custom_name_overrides_default() {
        let h = CustomNameHook;
        assert_eq!(h.name(), "loud-and-clear");
    }

    #[tokio::test]
    async fn default_before_model_returns_continue() {
        let h = LoudHook;
        let mut s = AgentState::new("sess", "");
        let outcome = h.before_model(&mut s).await;
        assert!(matches!(outcome, HookOutcome::Continue));
    }
}
