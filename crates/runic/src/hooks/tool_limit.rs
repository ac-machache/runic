use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use runic_hook::{HookLifecycle, HookOutcome, WriteHook};
use runic_state::AgentState;
use runic_tool::ToolResult;
use runic_types::ToolCall;

#[derive(Default)]
struct Counts {
    run_tool: HashMap<String, u32>,
    run_total: u32,
    thread_tool: HashMap<String, u64>,
    thread_total: u64,
}

pub struct ToolCallLimit {
    per_run: HashMap<String, u32>,
    per_thread: HashMap<String, u32>,
    total_per_run: Option<u32>,
    total_per_thread: Option<u32>,
    messages: HashMap<String, String>,
    counts: Mutex<Counts>,
}

impl Default for ToolCallLimit {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolCallLimit {
    pub fn new() -> Self {
        Self {
            per_run: HashMap::new(),
            per_thread: HashMap::new(),
            total_per_run: None,
            total_per_thread: None,
            messages: HashMap::new(),
            counts: Mutex::new(Counts::default()),
        }
    }

    pub fn message(mut self, tool: impl Into<String>, text: impl Into<String>) -> Self {
        self.messages.insert(tool.into(), text.into());
        self
    }

    fn block(&self, tool: &str, default: String) -> HookOutcome {
        let text = self.messages.get(tool).cloned().unwrap_or(default);
        HookOutcome::SubstituteToolResult(ToolResult::error(text))
    }

    pub fn per_run(mut self, tool: impl Into<String>, max_calls: u32) -> Self {
        self.per_run.insert(tool.into(), max_calls);
        self
    }

    pub fn per_thread(mut self, tool: impl Into<String>, max_calls: u32) -> Self {
        self.per_thread.insert(tool.into(), max_calls);
        self
    }

    pub fn total_per_run(mut self, max_calls: u32) -> Self {
        self.total_per_run = Some(max_calls);
        self
    }

    pub fn total_per_thread(mut self, max_calls: u32) -> Self {
        self.total_per_thread = Some(max_calls);
        self
    }
}

#[async_trait]
impl WriteHook for ToolCallLimit {
    fn name(&self) -> &str {
        "tool-call-limit"
    }

    fn points(&self) -> &'static [HookLifecycle] {
        &[HookLifecycle::BeforeAgent, HookLifecycle::BeforeTool]
    }

    async fn before_agent(&self, state: &mut AgentState) -> HookOutcome {
        *self.counts.lock().unwrap() = Counts {
            run_tool: HashMap::new(),
            run_total: 0,
            thread_tool: state.stats().tool_calls.clone(),
            thread_total: state.stats().total_tool_calls,
        };
        HookOutcome::Noop
    }

    async fn before_tool(&self, _state: &mut AgentState, call: &mut ToolCall) -> HookOutcome {
        let mut counts = self.counts.lock().unwrap();

        if let Some(&max) = self.per_thread.get(&call.name) {
            let max = max as u64;
            let used = counts.thread_tool.get(&call.name).copied().unwrap_or(0);
            if used >= max {
                return self.block(
                    &call.name,
                    format!(
                        "tool call limit reached for {} ({used}/{max} this thread)",
                        call.name
                    ),
                );
            }
        }
        if let Some(&max) = self.per_run.get(&call.name) {
            let used = counts.run_tool.get(&call.name).copied().unwrap_or(0);
            if used >= max {
                return self.block(
                    &call.name,
                    format!(
                        "tool call limit reached for {} ({used}/{max} this run)",
                        call.name
                    ),
                );
            }
        }
        if let Some(max) = self.total_per_thread
            && counts.thread_total >= max as u64
        {
            return HookOutcome::SubstituteToolResult(ToolResult::error(format!(
                "total tool call limit reached ({}/{max} this thread)",
                counts.thread_total
            )));
        }
        if let Some(max) = self.total_per_run
            && counts.run_total >= max
        {
            return HookOutcome::SubstituteToolResult(ToolResult::error(format!(
                "total tool call limit reached ({}/{max} this run)",
                counts.run_total
            )));
        }

        *counts.run_tool.entry(call.name.clone()).or_insert(0) += 1;
        counts.run_total += 1;
        *counts.thread_tool.entry(call.name.clone()).or_insert(0) += 1;
        counts.thread_total += 1;
        HookOutcome::Noop
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use runic_state::SessionEvent;
    use runic_types::{ContentBlock, Message};

    fn state() -> AgentState {
        AgentState::new("u1", "s1", "sys")
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: name.into(),
            input: serde_json::json!({}),
        }
    }

    fn tool_result_event(run: &str, tool: &str) -> SessionEvent {
        SessionEvent::Message {
            run_id: run.into(),
            msg: Message::user_with_blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "tu".into(),
                tool_name: tool.into(),
                content: "ok".into(),
                is_error: false,
            }]),
            at: Utc::now(),
        }
    }

    fn is_blocked(outcome: &HookOutcome, scope: &str) -> bool {
        matches!(outcome, HookOutcome::SubstituteToolResult(r) if !r.success && r.output.contains(scope))
    }

    async fn allowed(hook: &ToolCallLimit, s: &mut AgentState, name: &str) -> bool {
        matches!(
            hook.before_tool(s, &mut call(name)).await,
            HookOutcome::Noop
        )
    }

    #[tokio::test]
    async fn per_run_blocks_past_the_cap_within_a_run() {
        let hook = ToolCallLimit::new().per_run("search", 2);
        let mut s = state();
        hook.before_agent(&mut s).await;

        assert!(allowed(&hook, &mut s, "search").await);
        assert!(allowed(&hook, &mut s, "search").await);
        let third = hook.before_tool(&mut s, &mut call("search")).await;
        assert!(is_blocked(&third, "this run"));
    }

    #[tokio::test]
    async fn per_run_resets_on_a_new_run() {
        let hook = ToolCallLimit::new().per_run("search", 1);
        let mut s = state();

        hook.before_agent(&mut s).await;
        assert!(allowed(&hook, &mut s, "search").await);
        assert!(!allowed(&hook, &mut s, "search").await);

        hook.before_agent(&mut s).await;
        assert!(allowed(&hook, &mut s, "search").await);
    }

    #[tokio::test]
    async fn per_thread_persists_across_runs() {
        let hook = ToolCallLimit::new().per_thread("payment", 2);
        let mut s = state();

        hook.before_agent(&mut s).await;
        assert!(allowed(&hook, &mut s, "payment").await);
        assert!(allowed(&hook, &mut s, "payment").await);
        s.push_event(tool_result_event("r1", "payment"));
        s.push_event(tool_result_event("r1", "payment"));

        hook.before_agent(&mut s).await;
        let blocked = hook.before_tool(&mut s, &mut call("payment")).await;
        assert!(is_blocked(&blocked, "this thread"));
        assert!(allowed(&hook, &mut s, "search").await);
    }

    #[tokio::test]
    async fn custom_message_replaces_the_default() {
        let hook = ToolCallLimit::new()
            .per_thread("payment", 1)
            .message("payment", "No more charges — ask the user to confirm.");
        let mut s = state();
        s.push_event(tool_result_event("r1", "payment"));

        hook.before_agent(&mut s).await;
        match hook.before_tool(&mut s, &mut call("payment")).await {
            HookOutcome::SubstituteToolResult(r) => {
                assert!(!r.success);
                assert_eq!(r.output, "No more charges — ask the user to confirm.");
            }
            other => panic!("expected block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn per_thread_survives_compaction() {
        let hook = ToolCallLimit::new().per_thread("payment", 2);
        let mut s = state();
        s.push_event(tool_result_event("r1", "payment"));
        s.push_event(tool_result_event("r1", "payment"));
        s.push_event(SessionEvent::StateSnapshot {
            run_id: "r1".into(),
            messages: vec![Message::assistant("summary of earlier context")],
            system_prompt: "sys".into(),
            reason: "test compaction".into(),
            stats: None,
            open_tasks: None,
            data: None,
            at: Utc::now(),
        });
        assert_eq!(s.messages_for_provider().len(), 1);

        hook.before_agent(&mut s).await;
        let blocked = hook.before_tool(&mut s, &mut call("payment")).await;
        assert!(is_blocked(&blocked, "this thread"));
    }

    #[tokio::test]
    async fn per_thread_survives_an_agent_rebuild() {
        let hook = ToolCallLimit::new().per_thread("payment", 2);
        let mut replayed = state();
        replayed.push_event(tool_result_event("r1", "payment"));
        replayed.push_event(tool_result_event("r2", "payment"));

        hook.before_agent(&mut replayed).await;
        let blocked = hook.before_tool(&mut replayed, &mut call("payment")).await;
        assert!(is_blocked(&blocked, "this thread"));
    }

    #[tokio::test]
    async fn same_turn_calls_count_even_before_results_land() {
        let hook = ToolCallLimit::new().per_run("payment", 2);
        let mut s = state();
        hook.before_agent(&mut s).await;

        assert!(allowed(&hook, &mut s, "payment").await);
        assert!(allowed(&hook, &mut s, "payment").await);
        assert!(!allowed(&hook, &mut s, "payment").await);
    }

    #[tokio::test]
    async fn totals_cap_across_tools_per_run_and_per_thread() {
        let hook = ToolCallLimit::new().total_per_run(2).total_per_thread(3);
        let mut s = state();

        hook.before_agent(&mut s).await;
        assert!(allowed(&hook, &mut s, "a").await);
        assert!(allowed(&hook, &mut s, "b").await);
        let blocked = hook.before_tool(&mut s, &mut call("c")).await;
        assert!(is_blocked(&blocked, "this run"));
        s.push_event(tool_result_event("r1", "a"));
        s.push_event(tool_result_event("r1", "b"));

        hook.before_agent(&mut s).await;
        assert!(allowed(&hook, &mut s, "c").await);
        s.push_event(tool_result_event("r2", "c"));

        hook.before_agent(&mut s).await;
        let blocked = hook.before_tool(&mut s, &mut call("d")).await;
        assert!(is_blocked(&blocked, "this thread"));
    }

    #[tokio::test]
    async fn blocked_calls_do_not_consume_budgets() {
        let hook = ToolCallLimit::new().per_run("search", 1).total_per_run(5);
        let mut s = state();
        hook.before_agent(&mut s).await;

        assert!(allowed(&hook, &mut s, "search").await);
        for _ in 0..3 {
            assert!(!allowed(&hook, &mut s, "search").await);
        }
        assert!(allowed(&hook, &mut s, "a").await);
        assert!(allowed(&hook, &mut s, "b").await);
        assert!(allowed(&hook, &mut s, "c").await);
        assert!(allowed(&hook, &mut s, "d").await);
        assert!(!allowed(&hook, &mut s, "e").await);
    }
}
