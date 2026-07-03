//! Hook contracts for observing, mutating, or steering the agent loop.

use async_trait::async_trait;
use runic_state::AgentState;
use runic_tool::ToolResult;
use runic_types::ToolCall;

pub use runic_state::HookLifecycle;

pub const ALL_POINTS: &[HookLifecycle] = &[
    HookLifecycle::BeforeAgent,
    HookLifecycle::BeforeModel,
    HookLifecycle::BeforeTool,
    HookLifecycle::AfterTool,
    HookLifecycle::AfterModel,
    HookLifecycle::AfterAgent,
];

/// What a read-only [`ReadHook`] may ask the loop to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookSignal {
    Continue,
    Stop,
}

/// What a read-edit [`WriteHook`] may ask the loop to do.
#[derive(Debug, Clone)]
pub enum HookOutcome {
    Continue,
    /// Skip the tool entirely and use this result instead. Honored only when
    /// returned from [`WriteHook::before_tool`]; ignored elsewhere.
    SubstituteToolResult(ToolResult),
    /// Abort this step with a reason.
    Cancel(String),
    Stop,
}

/// The lifecycle points at which hooks fire. Both hook types expose the same
/// six points; the default implementations are no-ops so an impl overrides
/// only what it cares about.
///
/// Read-only hooks: observe state, run in parallel, may only `Continue`/`Stop`.
#[async_trait]
pub trait ReadHook: Send + Sync {
    fn name(&self) -> &str;

    fn priority(&self) -> i32 {
        0
    }

    fn points(&self) -> &'static [HookLifecycle] {
        ALL_POINTS
    }

    async fn before_agent(&self, _state: &AgentState) -> HookSignal {
        HookSignal::Continue
    }

    async fn before_model(&self, _state: &AgentState) -> HookSignal {
        HookSignal::Continue
    }

    async fn before_tool(&self, _state: &AgentState, _call: &ToolCall) -> HookSignal {
        HookSignal::Continue
    }

    async fn after_model(&self, _state: &AgentState) -> HookSignal {
        HookSignal::Continue
    }

    async fn after_tool(
        &self,
        _state: &AgentState,
        _call: &ToolCall,
        _result: &ToolResult,
    ) -> HookSignal {
        HookSignal::Continue
    }

    async fn after_agent(&self, _state: &AgentState) -> HookSignal {
        HookSignal::Continue
    }
}

/// Read-edit hooks: mutate state and steer the loop, run sequentially by
/// `priority()`, may return the full [`HookOutcome`].
#[async_trait]
pub trait WriteHook: Send + Sync {
    fn name(&self) -> &str;

    fn priority(&self) -> i32 {
        0
    }

    fn points(&self) -> &'static [HookLifecycle] {
        ALL_POINTS
    }

    async fn before_agent(&self, _state: &mut AgentState) -> HookOutcome {
        HookOutcome::Continue
    }

    async fn before_model(&self, _state: &mut AgentState) -> HookOutcome {
        HookOutcome::Continue
    }

    /// Before a tool runs: rewrite the call in place via `&mut ToolCall`, or
    /// short-circuit with `SubstituteToolResult` / `Cancel` / `Stop`.
    async fn before_tool(&self, _state: &mut AgentState, _call: &mut ToolCall) -> HookOutcome {
        HookOutcome::Continue
    }

    async fn after_model(&self, _state: &mut AgentState) -> HookOutcome {
        HookOutcome::Continue
    }

    async fn after_tool(
        &self,
        _state: &mut AgentState,
        _call: &ToolCall,
        _result: &ToolResult,
    ) -> HookOutcome {
        HookOutcome::Continue
    }

    async fn after_agent(&self, _state: &mut AgentState) -> HookOutcome {
        HookOutcome::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AgentState {
        AgentState::new("u1", "s1", "be helpful")
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: name.into(),
            input: serde_json::json!({ "q": 1 }),
        }
    }

    struct StopOnTool(&'static str);

    #[async_trait]
    impl ReadHook for StopOnTool {
        fn name(&self) -> &str {
            "stop-on-tool"
        }
        async fn before_tool(&self, _s: &AgentState, c: &ToolCall) -> HookSignal {
            if c.name == self.0 {
                HookSignal::Stop
            } else {
                HookSignal::Continue
            }
        }
    }

    struct CacheAndRewrite;

    #[async_trait]
    impl WriteHook for CacheAndRewrite {
        fn name(&self) -> &str {
            "cache-and-rewrite"
        }
        fn priority(&self) -> i32 {
            -10
        }
        async fn before_tool(&self, _s: &mut AgentState, c: &mut ToolCall) -> HookOutcome {
            if c.name == "search" {
                return HookOutcome::SubstituteToolResult(ToolResult::ok("cached: 42"));
            }
            c.input = serde_json::json!({ "q": 2 });
            HookOutcome::Continue
        }
    }

    #[tokio::test]
    async fn read_hook_observes_and_stops() {
        let s = state();
        let h = StopOnTool("danger");
        assert_eq!(h.before_tool(&s, &call("safe")).await, HookSignal::Continue);
        assert_eq!(h.before_tool(&s, &call("danger")).await, HookSignal::Stop);
        assert_eq!(h.priority(), 0);
    }

    #[tokio::test]
    async fn write_hook_substitutes_and_rewrites() {
        let mut s = state();
        let h = CacheAndRewrite;
        assert_eq!(h.priority(), -10);

        match h.before_tool(&mut s, &mut call("search")).await {
            HookOutcome::SubstituteToolResult(r) => {
                assert!(r.success);
                assert_eq!(r.output, "cached: 42");
            }
            other => panic!("expected substitution, got {other:?}"),
        }

        let mut c = call("other");
        match h.before_tool(&mut s, &mut c).await {
            HookOutcome::Continue => assert_eq!(c.input, serde_json::json!({ "q": 2 })),
            other => panic!("expected continue, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn defaults_are_noops() {
        struct Noop;
        #[async_trait]
        impl WriteHook for Noop {
            fn name(&self) -> &str {
                "noop"
            }
        }
        let mut s = state();
        assert!(matches!(
            Noop.before_model(&mut s).await,
            HookOutcome::Continue
        ));
    }
}
