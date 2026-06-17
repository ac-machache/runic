use std::collections::HashMap;

use async_trait::async_trait;
use runic_message_types::{ContentBlock, ToolCall};

use crate::state::{AgentState, SessionEvent};
use runic_tool_core::ToolResult;

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

/// Caps how many times a given tool may be invoked **within a single run**
/// (one user request — not one model turn). Configured by a
/// `HashMap<tool_name, max_calls>`; tools absent from the map are unlimited.
///
/// This stops a model from looping on the same tool (e.g. calling a search
/// tool 7 times across 7 turns because each result looked unsatisfying).
///
/// **No stored counter.** The count is derived from the in-flight run's own
/// history every time `before_tool` fires (`state.current_run()`), so it is
/// naturally per-run, never leaks across runs, and is safe under concurrency
/// — there is nothing to reset. This mirrors how step counters live in the
/// ephemeral execution context rather than the pooled/compiled object.
///
/// When the cap is exceeded the call is **not** dispatched; the model instead
/// receives an error [`ToolResult`] telling it to stop and answer with what it
/// already has (a soft cap the model can see and react to, not a hard abort).
///
/// To cover sub-agents, install the same hook on each child agent — every
/// agent then caps its own run independently (sub-agents run their own loop
/// over their own state).
#[derive(Debug, Clone, Default)]
pub struct CallLimitHook {
    limits: HashMap<String, usize>,
}

impl CallLimitHook {
    /// Build from a `tool_name -> max_calls_per_run` map.
    pub fn new(limits: HashMap<String, usize>) -> Self {
        Self { limits }
    }

    /// Builder-style: cap a single tool. Chainable.
    pub fn limit(mut self, tool: impl Into<String>, max_calls: usize) -> Self {
        self.limits.insert(tool.into(), max_calls);
        self
    }

    /// Count `tool_use` blocks for `tool` in the current run that appear
    /// strictly BEFORE `current_id`. The current call's own `tool_use` block
    /// is already in history when `before_tool` fires, so we stop at its id
    /// to count only prior invocations — this is precise even for several
    /// parallel calls emitted in one assistant turn.
    fn prior_uses(state: &AgentState, tool: &str, current_id: &str) -> usize {
        let Some(run) = state.current_run() else {
            return 0;
        };
        let mut prior = 0;
        for ev in run.events {
            if let SessionEvent::Message { msg, .. } = ev {
                for block in &msg.content {
                    if let ContentBlock::ToolUse { id, name, .. } = block {
                        if name != tool {
                            continue;
                        }
                        if id == current_id {
                            return prior; // reached the current call
                        }
                        prior += 1;
                    }
                }
            }
        }
        prior
    }
}

#[async_trait]
impl Hook for CallLimitHook {
    fn name(&self) -> &'static str {
        "call-limit"
    }

    async fn before_tool(&self, state: &mut AgentState, call: &mut ToolCall) -> HookOutcome {
        let Some(&max) = self.limits.get(&call.name) else {
            return HookOutcome::Continue;
        };
        let prior = Self::prior_uses(state, &call.name, &call.id);
        if prior >= max {
            return HookOutcome::SubstituteToolResult(ToolResult::error(format!(
                "Call limit reached: '{}' may be called at most {} time(s) per request \
                 and has already been called {} time(s). Do not call it again — answer with \
                 the information you already have, or take a different approach.",
                call.name, max, prior
            )));
        }
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

    // ─── CallLimitHook ──────────────────────────────────────────────────────

    use chrono::Utc;
    use runic_message_types::{Message, Role};

    fn start_run(s: &mut AgentState, run_id: &str) {
        s.push_event(SessionEvent::RunStart {
            run_id: run_id.into(),
            at: Utc::now(),
        });
    }

    /// Push an assistant message carrying one tool_use block — the same shape
    /// the loop records before dispatching the call.
    fn push_tool_use(s: &mut AgentState, run_id: &str, id: &str, name: &str) {
        s.push_event(SessionEvent::Message {
            run_id: run_id.into(),
            msg: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: id.into(),
                    name: name.into(),
                    input: serde_json::json!({}),
                }],
                timestamp: None,
                tool_duration_ms: None,
            },
            at: Utc::now(),
        });
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            input: serde_json::json!({}),
            intent: None,
        }
    }

    #[tokio::test]
    async fn untracked_tool_is_never_capped() {
        let hook = CallLimitHook::default().limit("search", 1);
        let mut s = AgentState::new("sess", "");
        start_run(&mut s, "r1");
        // Many prior calls to a DIFFERENT tool — irrelevant.
        for i in 0..5 {
            push_tool_use(&mut s, "r1", &format!("u{i}"), "other");
        }
        let mut c = call("u9", "other");
        assert!(matches!(
            hook.before_tool(&mut s, &mut c).await,
            HookOutcome::Continue
        ));
    }

    #[tokio::test]
    async fn allows_up_to_max_then_blocks() {
        let hook = CallLimitHook::new(HashMap::from([("search".to_string(), 3)]));
        let mut s = AgentState::new("sess", "");
        start_run(&mut s, "r1");

        // Simulate the loop: each turn pushes the tool_use, THEN before_tool
        // fires for that same call (its block already in history).
        for i in 1..=3 {
            let id = format!("u{i}");
            push_tool_use(&mut s, "r1", &id, "search");
            let mut c = call(&id, "search");
            assert!(
                matches!(hook.before_tool(&mut s, &mut c).await, HookOutcome::Continue),
                "call #{i} should be allowed (max 3)"
            );
        }

        // 4th call: 3 prior uses already in history → blocked.
        push_tool_use(&mut s, "r1", "u4", "search");
        let mut c = call("u4", "search");
        match hook.before_tool(&mut s, &mut c).await {
            HookOutcome::SubstituteToolResult(r) => {
                assert!(r.is_error, "substituted result should be an error");
            }
            other => panic!("expected SubstituteToolResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn counter_does_not_leak_across_runs() {
        let hook = CallLimitHook::default().limit("search", 1);
        let mut s = AgentState::new("sess", "");

        // Run 1: use the single allowance, then exhaust it.
        start_run(&mut s, "r1");
        push_tool_use(&mut s, "r1", "a1", "search");
        let mut c = call("a1", "search");
        assert!(matches!(
            hook.before_tool(&mut s, &mut c).await,
            HookOutcome::Continue
        ));
        push_tool_use(&mut s, "r1", "a2", "search");
        let mut c = call("a2", "search");
        assert!(matches!(
            hook.before_tool(&mut s, &mut c).await,
            HookOutcome::SubstituteToolResult(_)
        ));

        // Close run 1, open run 2 — the allowance resets (count is per-run).
        s.push_event(SessionEvent::RunEnd {
            run_id: "r1".into(),
            outcome: crate::agent::RunOutcome {
                total_turns: 1,
                stop_reason: None,
                usage: crate::event::TokenUsage::default(),
                structured_result: None,
                model: None,
                provider: None,
            },
            at: Utc::now(),
        });
        start_run(&mut s, "r2");
        push_tool_use(&mut s, "r2", "b1", "search");
        let mut c = call("b1", "search");
        assert!(
            matches!(hook.before_tool(&mut s, &mut c).await, HookOutcome::Continue),
            "fresh run must start with a fresh allowance"
        );
    }

    #[tokio::test]
    async fn parallel_calls_in_one_turn_allow_first_max() {
        // Two tool_use blocks emitted in ONE assistant turn; both already in
        // history. With max=1, the first (by id-position) is allowed, the
        // second blocked — counting is positional, not whole-message.
        let hook = CallLimitHook::default().limit("search", 1);
        let mut s = AgentState::new("sess", "");
        start_run(&mut s, "r1");
        s.push_event(SessionEvent::Message {
            run_id: "r1".into(),
            msg: Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::ToolUse {
                        id: "p1".into(),
                        name: "search".into(),
                        input: serde_json::json!({}),
                    },
                    ContentBlock::ToolUse {
                        id: "p2".into(),
                        name: "search".into(),
                        input: serde_json::json!({}),
                    },
                ],
                timestamp: None,
                tool_duration_ms: None,
            },
            at: Utc::now(),
        });
        let mut first = call("p1", "search");
        assert!(matches!(
            hook.before_tool(&mut s, &mut first).await,
            HookOutcome::Continue
        ));
        let mut second = call("p2", "search");
        assert!(matches!(
            hook.before_tool(&mut s, &mut second).await,
            HookOutcome::SubstituteToolResult(_)
        ));
    }
}
