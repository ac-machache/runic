//! End-to-end spine tests: a scripted provider drives the loop through a
//! text-only turn and a full tool round-trip.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use runic_agent::{Agent, AgentEvent, CancelToken, RunContext};
use runic_hook::{HookOutcome, HookSignal, ReadHook, WriteHook};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_state::AgentState;
use runic_tool::{ActivatedToolSet, Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, MessageContent, StopReason, TokenUsage, ToolCall};

/// A provider that returns a pre-scripted sequence of responses, one per call.
struct ScriptedProvider {
    responses: Mutex<std::collections::VecDeque<CompletionResponse>>,
    calls: Mutex<usize>,
    last_request: Mutex<Option<CompletionRequest>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<CompletionResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            calls: Mutex::new(0),
            last_request: Mutex::new(None),
        }
    }

    fn last_request(&self) -> CompletionRequest {
        self.last_request
            .lock()
            .unwrap()
            .clone()
            .expect("a request was sent")
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        *self.calls.lock().unwrap() += 1;
        *self.last_request.lock().unwrap() = Some(request);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::Parse("scripted provider exhausted".into()))
    }
}

fn text_response(text: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::Text {
            text: text.into(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::EndTurn,
        tool_calls: vec![],
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
        },
    }
}

fn tool_use_response(id: &str, name: &str, input: serde_json::Value) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input: input.clone(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            input,
        }],
        usage: TokenUsage {
            input_tokens: 20,
            output_tokens: 8,
        },
    }
}

/// A provider that always fails with `ModelNotFound` (to exercise fallback).
struct AlwaysModelNotFound;

#[async_trait]
impl Provider for AlwaysModelNotFound {
    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        Err(ProviderError::ModelNotFound("missing".into()))
    }
}

/// Emits the per-run `user_id` config value, to prove `RunContext` injection.
struct ConfigEcho;

#[async_trait]
impl Tool for ConfigEcho {
    fn name(&self) -> &str {
        "config_echo"
    }
    fn description(&self) -> &str {
        "Echoes the per-run user_id"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "additionalProperties": true })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let uid = ctx
            .config("user_id")
            .and_then(|v| v.as_str())
            .unwrap_or("none");
        Ok(ToolResult::ok(format!("user_id={uid}")))
    }
}

/// Echoes its `args` back; declares itself parallelizable.
struct Echo;

#[async_trait]
impl Tool for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes input"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "additionalProperties": true })
    }
    fn parallelizable(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok(format!("echo: {args}")))
    }
}

struct HookProbe {
    log: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl WriteHook for HookProbe {
    fn name(&self) -> &str {
        "hook-probe"
    }

    fn priority(&self) -> i32 {
        -10
    }

    async fn before_agent(&self, _state: &mut AgentState) -> HookOutcome {
        self.log.lock().unwrap().push("write:before_agent".into());
        HookOutcome::Continue
    }

    async fn before_tool(&self, _state: &mut AgentState, call: &mut ToolCall) -> HookOutcome {
        call.input = serde_json::json!({ "rewritten": true });
        self.log
            .lock()
            .unwrap()
            .push(format!("write:before_tool:{}", call.input));
        HookOutcome::SubstituteToolResult(ToolResult::ok("cached by hook"))
    }

    async fn after_tool(
        &self,
        _state: &mut AgentState,
        call: &ToolCall,
        result: &ToolResult,
    ) -> HookOutcome {
        self.log
            .lock()
            .unwrap()
            .push(format!("write:after_tool:{}:{}", call.input, result.output));
        HookOutcome::Continue
    }

    async fn after_agent(&self, _state: &mut AgentState) -> HookOutcome {
        self.log.lock().unwrap().push("write:after_agent".into());
        HookOutcome::Continue
    }
}

struct ReadProbe {
    log: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl ReadHook for ReadProbe {
    fn name(&self) -> &str {
        "read-probe"
    }

    async fn before_tool(&self, _state: &AgentState, call: &ToolCall) -> HookSignal {
        self.log
            .lock()
            .unwrap()
            .push(format!("read:before_tool:{}", call.input));
        HookSignal::Continue
    }

    async fn after_tool(
        &self,
        _state: &AgentState,
        call: &ToolCall,
        result: &ToolResult,
    ) -> HookSignal {
        self.log
            .lock()
            .unwrap()
            .push(format!("read:after_tool:{}:{}", call.input, result.output));
        HookSignal::Continue
    }
}

struct StopBeforeTool;

#[async_trait]
impl ReadHook for StopBeforeTool {
    fn name(&self) -> &str {
        "stop-before-tool"
    }

    async fn before_tool(&self, _state: &AgentState, _call: &ToolCall) -> HookSignal {
        HookSignal::Stop
    }
}

#[tokio::test]
async fn text_only_turn_ends_the_run() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("hello there")]));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .system_prompt("be brief")
        .build();

    let outcome = agent.run("hi").await.unwrap();

    assert_eq!(outcome.total_turns, 1);
    assert_eq!(outcome.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(outcome.usage.input_tokens, 10);
    assert_eq!(
        agent.state().last_assistant_text().as_deref(),
        Some("hello there")
    );
}

#[tokio::test]
async fn tool_call_round_trips_then_finishes() {
    // Turn 1: model calls echo. Turn 2: model answers and ends.
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "echo", serde_json::json!({ "x": 1 })),
        text_response("all done"),
    ]));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(Echo))
        .build();

    let outcome = agent.run("use the tool").await.unwrap();

    assert_eq!(outcome.total_turns, 2);
    assert_eq!(
        agent.state().last_assistant_text().as_deref(),
        Some("all done")
    );

    // The provider-facing history must contain the tool result the loop fed back.
    let msgs = agent.state().messages_for_provider();
    let has_tool_result = msgs.iter().any(|m| match &m.content {
        runic_types::MessageContent::Blocks(blocks) => blocks.iter().any(
            |b| matches!(b, ContentBlock::ToolResult { content, .. } if content.contains("echo:")),
        ),
        _ => false,
    });
    assert!(has_tool_result, "tool result should be in history");
}

#[tokio::test]
async fn hooks_rewrite_and_substitute_tool_results_in_the_loop() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "echo", serde_json::json!({ "original": true })),
        text_response("done"),
    ]));
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(Echo))
        .write_hook(Arc::new(HookProbe { log: log.clone() }))
        .read_hook(Arc::new(ReadProbe { log: log.clone() }))
        .build();

    let outcome = agent.run("use the tool").await.unwrap();

    assert_eq!(outcome.total_turns, 2);
    let log = log.lock().unwrap().clone();
    assert!(
        log.iter()
            .any(|entry| entry == "write:before_tool:{\"rewritten\":true}"),
        "write hook should rewrite the call before read hooks see it: {log:?}"
    );
    assert!(
        log.iter()
            .any(|entry| entry == "read:before_tool:{\"rewritten\":true}"),
        "read hook should observe the rewritten call: {log:?}"
    );
    assert!(
        log.iter()
            .any(|entry| entry.contains("write:after_tool:{\"rewritten\":true}:cached by hook")),
        "write after_tool should receive substituted result: {log:?}"
    );
    assert!(
        log.iter()
            .any(|entry| entry.contains("read:after_tool:{\"rewritten\":true}:cached by hook")),
        "read after_tool should receive substituted result: {log:?}"
    );

    let messages = agent.state().messages_for_provider();
    let cached = messages.iter().any(|m| {
        matches!(&m.content, MessageContent::Blocks(blocks)
            if blocks.iter().any(|b| matches!(b,
                ContentBlock::ToolResult { content, .. } if content == "cached by hook")))
    });
    let actual_echo_ran = messages.iter().any(|m| {
        matches!(&m.content, MessageContent::Blocks(blocks)
            if blocks.iter().any(|b| matches!(b,
                ContentBlock::ToolResult { content, .. } if content.contains("echo:"))))
    });
    assert!(cached, "substituted tool result should be persisted");
    assert!(
        !actual_echo_ran,
        "substitution must skip real tool execution"
    );
}

#[tokio::test]
async fn read_hook_stop_halts_before_tool_execution() {
    let provider = Arc::new(ScriptedProvider::new(vec![tool_use_response(
        "t1",
        "echo",
        serde_json::json!({}),
    )]));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(Echo))
        .read_hook(Arc::new(StopBeforeTool))
        .build();

    let err = agent.run("use the tool").await.unwrap_err();

    assert!(matches!(err, runic_agent::AgentError::HookStop));
    let tool_result_written = agent.state().messages_for_provider().iter().any(|m| {
        matches!(&m.content, MessageContent::Blocks(blocks)
            if blocks.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. })))
    });
    assert!(!tool_result_written, "stopped tools should not execute");
}

#[tokio::test]
async fn falls_back_to_secondary_provider_on_model_not_found() {
    let primary = Arc::new(AlwaysModelNotFound);
    let fallback = Arc::new(ScriptedProvider::new(vec![text_response(
        "fallback answer",
    )]));
    let mut agent = Agent::builder(primary, "u1", "s1")
        .model("does-not-exist")
        .fallback(fallback, "good-model")
        .build();

    let outcome = agent.run("hi").await.unwrap();
    assert_eq!(outcome.total_turns, 1);
    assert_eq!(
        agent.state().last_assistant_text().as_deref(),
        Some("fallback answer")
    );
}

#[tokio::test]
async fn unknown_tool_yields_error_result_not_crash() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "nonexistent", serde_json::json!({})),
        text_response("recovered"),
    ]));
    let mut agent = Agent::builder(provider, "u1", "s1").model("test").build();

    let outcome = agent.run("go").await.unwrap();
    assert_eq!(outcome.total_turns, 2);

    let msgs = agent.state().messages_for_provider();
    let has_unknown_err = msgs.iter().any(|m| match &m.content {
        runic_types::MessageContent::Blocks(blocks) => blocks.iter().any(|b| {
            matches!(b, ContentBlock::ToolResult { content, is_error, .. }
                if *is_error && content.contains("unknown tool"))
        }),
        _ => false,
    });
    assert!(
        has_unknown_err,
        "unknown tool should produce an in-band error result"
    );
}

// ─── Step 4: RunContext, cancellation, steering, graceful finish ─────────────

#[tokio::test]
async fn run_context_injects_per_run_config_into_tools() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "config_echo", serde_json::json!({})),
        text_response("done"),
        text_response("second run"),
    ]));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(ConfigEcho))
        .build();

    let ctx = RunContext::new().config_value("user_id", serde_json::json!("u-run"));
    agent.run_with("go", ctx).await.unwrap();

    let saw_injected = agent.state().messages_for_provider().iter().any(|m| {
        matches!(&m.content, MessageContent::Blocks(b)
            if b.iter().any(|blk| matches!(blk,
                ContentBlock::ToolResult { content, .. } if content.contains("user_id=u-run"))))
    });
    assert!(saw_injected, "tool should have seen the per-run user_id");
    assert_eq!(
        agent.state().config("user_id").and_then(|v| v.as_str()),
        Some("u-run"),
        "config persists within/after its run"
    );

    // No leak: a subsequent run with an empty context overwrites the map.
    agent.run_with("again", RunContext::new()).await.unwrap();
    assert!(
        agent.state().config("user_id").is_none(),
        "the next run's empty config overwrites the previous run's"
    );
}

#[tokio::test]
async fn provider_override_applies_then_restores() {
    // Primary always 404s; the per-run override answers; after the run the
    // build-time provider is restored, so a bare run fails again.
    let primary = Arc::new(AlwaysModelNotFound);
    let mut agent = Agent::builder(primary, "u1", "s1").model("primary").build();

    let override_provider = Arc::new(ScriptedProvider::new(vec![text_response("from override")]));
    let ctx = RunContext::new().with_provider(override_provider);
    let outcome = agent.run_with("hi", ctx).await.unwrap();
    assert_eq!(
        agent.state().last_assistant_text().as_deref(),
        Some("from override")
    );
    assert_eq!(outcome.total_turns, 1);

    // Provider restored → primary's ModelNotFound surfaces (no fallback set).
    assert!(agent.run("again").await.is_err());
}

#[tokio::test]
async fn cancellation_ends_the_run_before_any_turn() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("never reached")]));
    let mut agent = Agent::builder(provider, "u1", "s1").model("test").build();

    let cancel = CancelToken::new();
    cancel.cancel(); // pre-cancelled
    let outcome = agent
        .run_with("hi", RunContext::new().with_cancel(cancel))
        .await
        .unwrap();

    assert_eq!(outcome.total_turns, 0);
    assert_eq!(outcome.stop_reason.as_deref(), Some("cancelled"));
}

#[tokio::test]
async fn steering_messages_are_injected_into_history() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("ok")]));
    let mut agent = Agent::builder(provider, "u1", "s1").model("test").build();

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tx.send("steered nudge".to_string()).unwrap();
    drop(tx);

    agent
        .run_with("hi", RunContext::new().with_steering(rx))
        .await
        .unwrap();

    let saw_steer = agent
        .state()
        .messages_for_provider()
        .iter()
        .any(|m| matches!(&m.content, MessageContent::Text(t) if t == "steered nudge"));
    assert!(
        saw_steer,
        "steering text should be injected as a user message"
    );
}

#[tokio::test]
async fn graceful_max_turns_extracts_a_final_answer() {
    // Turn 1 wants a tool; the backstop trips at 1 turn; the graceful finish
    // makes a final tools-free call that answers.
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "echo", serde_json::json!({ "x": 1 })),
        text_response("wrapped up"),
    ]));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(Echo))
        .max_turns(1)
        .graceful_max_turns(true)
        .build();

    let outcome = agent.run("go").await.unwrap();
    assert_eq!(outcome.stop_reason.as_deref(), Some("max_turns"));
    assert_eq!(
        agent.state().last_assistant_text().as_deref(),
        Some("wrapped up")
    );
}

#[tokio::test]
async fn streaming_emits_lifecycle_and_token_events() {
    // Turn 1 calls echo, turn 2 answers. The default Provider::stream wraps
    // complete(), so text turns yield a TextDelta of the full text.
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "echo", serde_json::json!({ "x": 1 })),
        text_response("final answer"),
    ]));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(Echo))
        .build();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    agent
        .run_with("go", RunContext::new().with_events(tx))
        .await
        .unwrap();

    let mut started = false;
    let mut tool_started = false;
    let mut tool_finished = false;
    let mut texts = Vec::new();
    let mut turns = 0;
    let mut completed = false;
    while let Ok(ev) = rx.try_recv() {
        match ev {
            AgentEvent::RunStarted { .. } => started = true,
            AgentEvent::ToolStarted { name, .. } => tool_started |= name == "echo",
            AgentEvent::ToolFinished { name, is_error, .. } => {
                tool_finished |= name == "echo" && !is_error
            }
            AgentEvent::TextDelta(t) => texts.push(t),
            AgentEvent::TurnCompleted { .. } => turns += 1,
            AgentEvent::RunCompleted(_) => completed = true,
            AgentEvent::ThinkingDelta(_) => {}
            AgentEvent::HookFired { .. } => {}
        }
    }

    assert!(started, "RunStarted");
    assert!(tool_started, "ToolStarted for echo");
    assert!(tool_finished, "ToolFinished for echo");
    assert!(
        texts.iter().any(|t| t == "final answer"),
        "text delta streamed"
    );
    assert_eq!(turns, 2, "two TurnCompleted events");
    assert!(completed, "RunCompleted");
}

// A tool that, when run, activates a *new* tool into the shared set — stands
// in for an MCP `tool_search`, proving the deferred seam without depending on
// runic-mcp.
struct LateTool;

#[async_trait]
impl Tool for LateTool {
    fn name(&self) -> &str {
        "late_tool"
    }
    fn description(&self) -> &str {
        "Only available after activation"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok("late result"))
    }
}

struct Activator {
    set: std::sync::Arc<std::sync::Mutex<ActivatedToolSet>>,
}

#[async_trait]
impl Tool for Activator {
    fn name(&self) -> &str {
        "activate"
    }
    fn description(&self) -> &str {
        "Activates late_tool"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        self.set
            .lock()
            .unwrap()
            .activate("late_tool", Arc::new(LateTool));
        Ok(ToolResult::ok("activated late_tool"))
    }
}

#[tokio::test]
async fn deferred_tool_activates_then_becomes_callable() {
    // Turn 1: model calls `activate` (registered). Turn 2: model calls
    // `late_tool` (NOT registered — only resolvable via the activated set).
    // Turn 3: ends.
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("a1", "activate", serde_json::json!({})),
        tool_use_response("a2", "late_tool", serde_json::json!({})),
        text_response("done"),
    ]));
    let set = Arc::new(std::sync::Mutex::new(ActivatedToolSet::new()));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(Activator { set: set.clone() }))
        .activated_tools(set.clone())
        .build();

    let outcome = agent.run("go").await.unwrap();
    assert_eq!(outcome.total_turns, 3);
    assert!(set.lock().unwrap().is_activated("late_tool"));

    // The activated tool resolved and ran — its result is in history.
    let ran_late = agent.state().messages_for_provider().iter().any(|m| {
        matches!(&m.content, MessageContent::Blocks(b)
            if b.iter().any(|blk| matches!(blk,
                ContentBlock::ToolResult { content, .. } if content.contains("late result"))))
    });
    assert!(ran_late, "a deferred-activated tool must resolve and run");
}

#[tokio::test]
async fn hard_max_turns_errors_without_graceful() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "echo", serde_json::json!({})),
        tool_use_response("t2", "echo", serde_json::json!({})),
    ]));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(Echo))
        .max_turns(1)
        .build();

    assert!(matches!(
        agent.run("go").await,
        Err(runic_agent::AgentError::MaxTurnsExceeded(1))
    ));
}

#[tokio::test]
async fn output_schema_captures_final_answer_and_ends() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": { "answer": { "type": "string" } },
        "required": ["answer"]
    });
    let provider = Arc::new(ScriptedProvider::new(vec![tool_use_response(
        "f1",
        "final_answer",
        serde_json::json!({ "answer": "42" }),
    )]));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .output_schema(schema)
        .build();

    let outcome = agent.run("what is the answer").await.unwrap();

    assert_eq!(outcome.total_turns, 1);
    assert_eq!(outcome.stop_reason.as_deref(), Some("final_answer"));
    assert_eq!(
        outcome.structured,
        Some(serde_json::json!({ "answer": "42" }))
    );

    let msgs = agent.state().messages_for_provider();
    let acked = msgs.iter().any(|m| match &m.content {
        runic_types::MessageContent::Blocks(blocks) => blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { tool_name, .. } if tool_name == "final_answer")),
        _ => false,
    });
    assert!(acked, "final_answer call must get a matching tool_result");
}

#[tokio::test]
async fn prepare_request_carries_system_tools_and_user_message() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("ok")]));
    let mut agent = Agent::builder(provider.clone(), "u1", "s1")
        .model("test")
        .system_prompt("be terse")
        .tool(Arc::new(Echo))
        .build();

    agent.run("hello there").await.unwrap();

    let req = provider.last_request();
    assert_eq!(req.model, "test");
    assert_eq!(req.system.as_deref(), Some("be terse"));
    assert!(req.tools.iter().any(|t| t.name == "echo"));
    assert!(req.tools.iter().all(|t| t.name != "final_answer"));
    assert!(
        req.messages
            .iter()
            .any(|m| matches!(&m.content, MessageContent::Text(t) if t == "hello there"))
    );
}

#[tokio::test]
async fn prepare_request_injects_final_answer_tool_when_schema_set() {
    let schema =
        serde_json::json!({ "type": "object", "properties": { "x": { "type": "number" } } });
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("ok")]));
    let mut agent = Agent::builder(provider.clone(), "u1", "s1")
        .model("test")
        .output_schema(schema.clone())
        .build();

    agent.run("hi").await.unwrap();

    let req = provider.last_request();
    let fa = req
        .tools
        .iter()
        .find(|t| t.name == "final_answer")
        .expect("final_answer tool injected");
    assert_eq!(fa.input_schema, schema);
}
