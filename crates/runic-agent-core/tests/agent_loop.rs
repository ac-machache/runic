//! End-to-end tests for the agent run loop using a scripted provider.
//!
//! The scripted provider lets us drive deterministic streams of `StreamEvent`s
//! turn-by-turn, so we can verify the loop's behaviour without hitting a real
//! Anthropic backend.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream;
use futures::StreamExt;
use runic_agent_core::{
    Agent, AgentError, AgentEvent, AgentState, Hook, HookOutcome, Tool, ToolContext, ToolResult,
};
use runic_message_types::{Message, StreamEvent, ToolCall, ToolDefinition};
use runic_provider_core::{EventStream, Provider, ProviderError};

// ─── Scripted provider ───────────────────────────────────────────────────────

struct ScriptedProvider {
    turns: Mutex<VecDeque<Vec<StreamEvent>>>,
}

impl ScriptedProvider {
    fn new(turns: Vec<Vec<StreamEvent>>) -> Arc<Self> {
        Arc::new(Self {
            turns: Mutex::new(turns.into()),
        })
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream, ProviderError> {
        let next = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::other("scripted provider exhausted"))?;
        let s = stream::iter(next.into_iter().map(Ok::<_, ProviderError>));
        Ok(Box::pin(s))
    }

    fn name(&self) -> &str {
        "scripted"
    }

    fn model(&self) -> String {
        "test-model".into()
    }

    fn fork(&self) -> Arc<dyn Provider> {
        unimplemented!("scripted provider does not support fork")
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn text_turn(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta(text.into()),
        StreamEvent::MessageEnd {
            stop_reason: Some("end_turn".into()),
        },
    ]
}

fn tool_call_turn(id: &str, name: &str, input_json: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolUseStart {
            id: id.into(),
            name: name.into(),
        },
        StreamEvent::ToolInputDelta(input_json.into()),
        StreamEvent::ToolUseEnd,
        StreamEvent::MessageEnd {
            stop_reason: Some("tool_use".into()),
        },
    ]
}

/// Build a single assistant turn that emits multiple tool_use blocks
/// (the Anthropic-style parallel-call pattern).
fn parallel_tool_calls_turn(calls: Vec<(&str, &str, &str)>) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    for (id, name, input) in calls {
        events.push(StreamEvent::ToolUseStart {
            id: id.into(),
            name: name.into(),
        });
        events.push(StreamEvent::ToolInputDelta(input.into()));
        events.push(StreamEvent::ToolUseEnd);
    }
    events.push(StreamEvent::MessageEnd {
        stop_reason: Some("tool_use".into()),
    });
    events
}

async fn collect_events(
    agent: Agent,
    input: &str,
) -> (Agent, Result<runic_agent_core::RunOutcome, AgentError>, Vec<AgentEvent>) {
    let (mut events, handle) = agent.run_streaming(input);
    let mut drained: Vec<AgentEvent> = Vec::new();
    while let Some(ev) = events.next().await {
        drained.push(ev);
    }
    let (agent, outcome) = handle.await.expect("agent task should not panic");
    (agent, outcome, drained)
}

// ─── Counting tool ──────────────────────────────────────────────────────────

struct CountingTool {
    counter: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        "noop"
    }
    fn description(&self) -> &str {
        "Records that it was dispatched."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": true })
    }
    async fn execute(&self, _input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        self.counter.fetch_add(1, Ordering::SeqCst);
        ToolResult::ok("ran")
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn text_only_response_completes_in_one_turn() {
    let provider = ScriptedProvider::new(vec![text_turn("hello world")]);
    let agent = Agent::builder(provider).system_prompt("test").build();

    let (agent, outcome, events) = collect_events(agent, "hi").await;
    let outcome = outcome.expect("run should succeed");

    assert_eq!(outcome.total_turns, 1);
    assert_eq!(outcome.stop_reason.as_deref(), Some("end_turn"));

    // RunComplete should be the last AgentEvent emitted.
    assert!(matches!(events.last(), Some(AgentEvent::RunComplete { total_turns: 1 })));

    // State must have RunStart, user Message, assistant Message, TurnBoundary, RunEnd.
    let s = agent.state();
    assert_eq!(s.runs().len(), 1);
    let messages = s.messages_for_provider();
    assert_eq!(messages.len(), 2); // user + assistant
}

#[tokio::test]
async fn tool_call_round_trip_dispatches_tool_then_replies() {
    let counter = Arc::new(AtomicUsize::new(0));
    let provider = ScriptedProvider::new(vec![
        tool_call_turn("toolu_1", "noop", r#"{}"#),
        text_turn("done"),
    ]);

    let agent = Agent::builder(provider)
        .system_prompt("test")
        .tool(Arc::new(CountingTool {
            counter: counter.clone(),
        }))
        .build();

    let (agent, outcome, events) = collect_events(agent, "use the noop tool").await;
    let outcome = outcome.expect("run should succeed");

    assert_eq!(counter.load(Ordering::SeqCst), 1, "tool dispatched exactly once");
    assert_eq!(outcome.total_turns, 2);
    assert_eq!(outcome.stop_reason.as_deref(), Some("end_turn"));

    // Verify the events stream surfaced a ToolFinished.
    let tool_finished = events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolFinished { .. }));
    assert!(tool_finished, "expected at least one ToolFinished event");

    // State: user msg, assistant tool_use msg, user tool_result msg, assistant text msg.
    let messages = agent.state().messages_for_provider();
    assert_eq!(messages.len(), 4);
}

struct GroundedSearchTool;

#[async_trait]
impl Tool for GroundedSearchTool {
    fn name(&self) -> &str {
        "websearch"
    }
    fn description(&self) -> &str {
        "Returns text for the model plus source links for the client."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": true })
    }
    async fn execute(&self, _input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        ToolResult::ok("1. Rust 1.79 released — see blog").with_metadata(serde_json::json!({
            "sources": [{ "title": "Rust blog", "url": "https://blog.rust-lang.org" }]
        }))
    }
}

#[tokio::test]
async fn tool_metadata_flows_into_events_and_state_messages() {
    let provider = ScriptedProvider::new(vec![
        tool_call_turn("toolu_1", "websearch", r#"{}"#),
        text_turn("done"),
    ]);
    let agent = Agent::builder(provider)
        .system_prompt("test")
        .tool(Arc::new(GroundedSearchTool))
        .build();

    let (agent, outcome, events) = collect_events(agent, "search rust news").await;
    outcome.expect("run should succeed");

    // Live event stream carries the metadata (this is what serve mode
    // forwards to clients for grounding chips).
    let live_metadata = events.iter().find_map(|e| match e {
        AgentEvent::ToolFinished { result, .. } => result.metadata.clone(),
        _ => None,
    });
    assert_eq!(
        live_metadata.expect("ToolFinished must carry metadata")["sources"][0]["url"],
        "https://blog.rust-lang.org"
    );

    // The persisted tool_result block carries it too (replayed threads
    // keep their grounding), while content stays the model-facing text.
    let stored_metadata = agent
        .state()
        .messages_for_provider()
        .iter()
        .flat_map(|m| m.content.clone())
        .find_map(|block| match block {
            runic_message_types::ContentBlock::ToolResult {
                content, metadata, ..
            } => {
                assert_eq!(content, "1. Rust 1.79 released — see blog");
                metadata
            }
            _ => None,
        });
    assert_eq!(
        stored_metadata.expect("stored block must carry metadata")["sources"][0]["title"],
        "Rust blog"
    );
}

// ─── Hook tests ─────────────────────────────────────────────────────────────

struct StopHook;

#[async_trait]
impl Hook for StopHook {
    async fn before_model(&self, _state: &mut AgentState) -> HookOutcome {
        HookOutcome::Stop
    }
}

#[tokio::test]
async fn hook_returning_stop_aborts_the_run() {
    let provider = ScriptedProvider::new(vec![text_turn("never reached")]);
    let agent = Agent::builder(provider)
        .system_prompt("test")
        .hook(Arc::new(StopHook))
        .build();

    let (_agent, outcome, _events) = collect_events(agent, "hi").await;
    assert!(matches!(outcome, Err(AgentError::HookStop)));
}

struct SubstituteHook {
    payload: &'static str,
}

#[async_trait]
impl Hook for SubstituteHook {
    async fn before_tool(&self, _state: &mut AgentState, _call: &mut ToolCall) -> HookOutcome {
        HookOutcome::SubstituteToolResult(ToolResult::ok(self.payload))
    }
}

#[tokio::test]
async fn substitute_tool_result_skips_actual_dispatch() {
    let counter = Arc::new(AtomicUsize::new(0));
    let provider = ScriptedProvider::new(vec![
        tool_call_turn("toolu_1", "noop", r#"{}"#),
        text_turn("done"),
    ]);

    let agent = Agent::builder(provider)
        .system_prompt("test")
        .tool(Arc::new(CountingTool {
            counter: counter.clone(),
        }))
        .hook(Arc::new(SubstituteHook {
            payload: "from hook",
        }))
        .build();

    let (agent, outcome, events) = collect_events(agent, "call it").await;
    outcome.expect("run should succeed");

    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "real tool MUST NOT be called when before_tool substitutes a result"
    );

    // The tool_result message in state should carry the hook's payload.
    let messages = agent.state().messages_for_provider();
    // Find the last user message that contains a tool_result block.
    let tool_result_text = messages.iter().rev().find_map(|m| {
        m.content.iter().find_map(|b| match b {
            runic_message_types::ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
    });
    assert_eq!(tool_result_text.as_deref(), Some("from hook"));

    // ToolFinished should still have been emitted with the synthetic result.
    let finished = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolFinished { result, .. } => Some(result.content.clone()),
            _ => None,
        });
    assert_eq!(finished.as_deref(), Some("from hook"));
}

// ─── Runtime context test ───────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
struct UserCtx {
    id: u64,
}

struct UserAwareTool {
    seen_id: Arc<Mutex<Option<u64>>>,
}

#[async_trait]
impl Tool for UserAwareTool {
    fn name(&self) -> &str {
        "noop"
    }
    fn description(&self) -> &str {
        "reads UserCtx from runtime"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        match ctx.get::<UserCtx>() {
            Some(u) => {
                *self.seen_id.lock().unwrap() = Some(u.id);
                ToolResult::ok(format!("user={}", u.id))
            }
            None => ToolResult::error("no UserCtx in runtime"),
        }
    }
}

#[tokio::test]
async fn runtime_handles_flow_through_to_tools() {
    let seen_id = Arc::new(Mutex::new(None));
    let provider = ScriptedProvider::new(vec![
        tool_call_turn("toolu_1", "noop", r#"{}"#),
        text_turn("done"),
    ]);

    let agent = Agent::builder(provider)
        .system_prompt("test")
        .tool(Arc::new(UserAwareTool {
            seen_id: seen_id.clone(),
        }))
        .runtime(UserCtx { id: 99 })
        .build();

    let (_agent, outcome, _) = collect_events(agent, "go").await;
    outcome.expect("run should succeed");

    assert_eq!(
        *seen_id.lock().unwrap(),
        Some(99),
        "tool should have observed the registered UserCtx"
    );
}

// ─── Max turns guard ────────────────────────────────────────────────────────

#[tokio::test]
async fn max_turns_exceeded_is_returned_as_error() {
    let provider = ScriptedProvider::new(vec![
        tool_call_turn("toolu_1", "noop", r#"{}"#),
        tool_call_turn("toolu_2", "noop", r#"{}"#),
        tool_call_turn("toolu_3", "noop", r#"{}"#),
    ]);

    let counter = Arc::new(AtomicUsize::new(0));
    let cfg = runic_agent_core::AgentConfig {
        max_turns: 2,
        ..Default::default()
    };

    let agent = Agent::builder(provider)
        .system_prompt("test")
        .tool(Arc::new(CountingTool {
            counter: counter.clone(),
        }))
        .config(cfg)
        .build();

    let (_agent, outcome, _events) = collect_events(agent, "loop").await;
    assert!(matches!(outcome, Err(AgentError::MaxTurnsExceeded(2))));
}

// ─── Subagent test ──────────────────────────────────────────────────────────

#[tokio::test]
async fn subagent_runs_to_completion_and_returns_final_text() {
    // Parent: emits one tool_use for our subagent, then a final text turn.
    let parent_provider = ScriptedProvider::new(vec![
        tool_call_turn("toolu_sub", "research", r#"{"prompt":"what is rust"}"#),
        text_turn("the research said it"),
    ]);

    // Child: ONE text turn that becomes the subagent's final answer.
    // The factory is called once per invocation, so each call gets a fresh script.
    let subagent = runic_agent_core::SubagentTool::new(
        "research",
        "Spawn a research subagent",
        move || {
            let child_provider =
                ScriptedProvider::new(vec![text_turn("rust is a systems programming language")]);
            Agent::builder(child_provider)
                .system_prompt("You are a research subagent.")
                .build()
        },
    );

    let agent = Agent::builder(parent_provider)
        .system_prompt("test")
        .tool(Arc::new(subagent))
        .build();

    let (agent, outcome, events) = collect_events(agent, "go").await;
    outcome.expect("run should succeed");

    // The ToolFinished event for the subagent must carry the child's final text.
    let sub_result = events.iter().find_map(|e| match e {
        AgentEvent::ToolFinished { call, result, .. } if call.name == "research" => {
            Some(result.content.clone())
        }
        _ => None,
    });
    assert_eq!(
        sub_result.as_deref(),
        Some("rust is a systems programming language"),
        "subagent's last assistant text must propagate as the tool result"
    );

    // The parent state should NOT contain any of the child's events — they're
    // ephemeral, in the child's AgentState which was dropped after dispatch.
    let parent_msgs = agent.state().messages_for_provider();
    let parent_text_contents: Vec<String> = parent_msgs
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            runic_message_types::ContentBlock::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    // The child's text shouldn't appear directly in parent message-text content
    // (it only appears as a tool_result block, not as plain text).
    assert!(
        !parent_text_contents
            .iter()
            .any(|t| t.contains("rust is a systems programming language")),
        "child's transcript must NOT leak into parent text messages"
    );
}

// ─── Parallel dispatch test ─────────────────────────────────────────────────

/// A plain `Tool` that sleeps for `sleep_ms` then returns "done-<name>".
/// Used to time concurrent vs sequential dispatch.
struct SlowTool {
    name_str: &'static str,
    sleep_ms: u64,
}

#[async_trait]
impl Tool for SlowTool {
    fn name(&self) -> &str {
        self.name_str
    }
    fn description(&self) -> &str {
        "sleeps then returns"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> runic_agent_core::ToolResult {
        tokio::time::sleep(std::time::Duration::from_millis(self.sleep_ms)).await;
        runic_agent_core::ToolResult::ok(format!("done-{}", self.name_str))
    }
}

#[tokio::test]
async fn plain_tools_in_one_turn_dispatch_in_parallel() {
    // The model emits 3 tool_use blocks in a single assistant turn.
    // Each tool sleeps 80ms. Sequential ≈ 240ms; parallel ≈ 80ms.
    let provider = ScriptedProvider::new(vec![
        parallel_tool_calls_turn(vec![
            ("toolu_a", "slow_a", r#"{}"#),
            ("toolu_b", "slow_b", r#"{}"#),
            ("toolu_c", "slow_c", r#"{}"#),
        ]),
        text_turn("all done"),
    ]);

    let agent = Agent::builder(provider)
        .system_prompt("test")
        .tool(Arc::new(SlowTool {
            name_str: "slow_a",
            sleep_ms: 80,
        }))
        .tool(Arc::new(SlowTool {
            name_str: "slow_b",
            sleep_ms: 80,
        }))
        .tool(Arc::new(SlowTool {
            name_str: "slow_c",
            sleep_ms: 80,
        }))
        .build();

    let start = std::time::Instant::now();
    let (_agent, outcome, events) = collect_events(agent, "go").await;
    let elapsed_ms = start.elapsed().as_millis() as u64;

    outcome.expect("run should succeed");

    // All three should have finished with their distinct results.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for ev in &events {
        if let AgentEvent::ToolFinished { result, .. } = ev {
            seen.insert(result.content.clone());
        }
    }
    assert_eq!(seen.len(), 3, "expected 3 distinct ToolFinished events");
    assert!(seen.contains("done-slow_a"));
    assert!(seen.contains("done-slow_b"));
    assert!(seen.contains("done-slow_c"));

    // Sequential would be ~240ms; allow generous slack for CI.
    // Parallel should complete in ~80ms + small overhead.
    assert!(
        elapsed_ms < 200,
        "expected parallel dispatch (~80ms total), got {elapsed_ms}ms — \
         likely sequential dispatch regression"
    );
}

// ─── Structured output ────────────────────────────────────────────────────────

fn answer_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": { "answer": { "type": "string" } },
        "required": ["answer"],
        "additionalProperties": false
    })
}

#[tokio::test]
async fn structured_output_ends_run_with_validated_result() {
    // Model calls the synthesized finish tool ("respond") with valid args.
    let provider = ScriptedProvider::new(vec![tool_call_turn(
        "t1",
        "respond",
        r#"{"answer":"42"}"#,
    )]);
    let agent = Agent::builder(provider)
        .system_prompt("test")
        .structured_output(answer_schema())
        .build();

    let (_agent, outcome, events) = collect_events(agent, "what is 6x7?").await;
    let outcome = outcome.expect("run should succeed");

    assert_eq!(outcome.total_turns, 1, "should end right after the finish call");
    assert_eq!(outcome.stop_reason.as_deref(), Some("structured_output"));
    assert_eq!(
        outcome.structured_result,
        Some(serde_json::json!({ "answer": "42" }))
    );
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::StructuredOutput(v) if v["answer"] == "42")),
        "a StructuredOutput event should be emitted"
    );
}

#[tokio::test]
async fn structured_output_invalid_args_retries_then_succeeds() {
    // Turn 1: missing the required "answer" → schema validation fails → the
    // tool returns an error → run keeps going. Turn 2: valid → ends.
    let provider = ScriptedProvider::new(vec![
        tool_call_turn("t1", "respond", r#"{}"#),
        tool_call_turn("t2", "respond", r#"{"answer":"fixed"}"#),
    ]);
    let agent = Agent::builder(provider)
        .system_prompt("test")
        .structured_output(answer_schema())
        .build();

    let (_agent, outcome, _events) = collect_events(agent, "go").await;
    let outcome = outcome.expect("run should succeed");

    assert_eq!(outcome.total_turns, 2, "first call invalid, retried on the second");
    assert_eq!(
        outcome.structured_result,
        Some(serde_json::json!({ "answer": "fixed" }))
    );
}
