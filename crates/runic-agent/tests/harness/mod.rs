//! Shared test fixtures for the `runic-agent` loop tests.
//!
//! Everything here is fake/in-memory — no real provider, DB, or HTTP. The
//! pieces compose: a [`ScriptedProvider`] doubles as a request spy, the
//! recording tools log their inputs, and the recording hooks log their firing
//! order so a test can assert the exact loop contract.

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use runic_agent::Agent;
use runic_hook::{HookOutcome, HookSignal, ReadHook, WriteHook};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_state::{AgentState, SessionEvent};
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, MessageContent, StopReason, TokenUsage, ToolCall};
use tokio::sync::mpsc;

// ─── Response builders ───────────────────────────────────────────────────────

/// A plain text answer that ends the turn.
pub fn text_response(text: &str) -> CompletionResponse {
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

/// A single tool call.
pub fn tool_use_response(id: &str, name: &str, input: serde_json::Value) -> CompletionResponse {
    multi_tool_response(vec![(id, name, input)])
}

/// Several tool calls in one turn (order preserved).
pub fn multi_tool_response(calls: Vec<(&str, &str, serde_json::Value)>) -> CompletionResponse {
    let content = calls
        .iter()
        .map(|(id, name, input)| ContentBlock::ToolUse {
            id: (*id).into(),
            name: (*name).into(),
            input: input.clone(),
            provider_metadata: None,
        })
        .collect();
    let tool_calls = calls
        .into_iter()
        .map(|(id, name, input)| ToolCall {
            id: id.into(),
            name: name.into(),
            input,
        })
        .collect();
    CompletionResponse {
        content,
        stop_reason: StopReason::ToolUse,
        tool_calls,
        usage: TokenUsage {
            input_tokens: 20,
            output_tokens: 8,
        },
    }
}

// ─── Scripted / spying provider ──────────────────────────────────────────────

/// Returns a predefined sequence of results, one per `complete` call, and
/// records every request it received — so it serves as both the
/// `ScriptedProvider` and the `SpyProvider` from the plan.
pub struct ScriptedProvider {
    results: Mutex<VecDeque<Result<CompletionResponse, ProviderError>>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl ScriptedProvider {
    /// Script a sequence of successful responses.
    pub fn new(responses: Vec<CompletionResponse>) -> Self {
        Self::with_results(responses.into_iter().map(Ok).collect())
    }

    /// Script a sequence that may include provider errors mid-run.
    pub fn with_results(results: Vec<Result<CompletionResponse, ProviderError>>) -> Self {
        Self {
            results: Mutex::new(results.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// How many model calls were made.
    pub fn call_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    /// Every request the loop sent, in order.
    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// The most recent request the loop sent.
    pub fn last_request(&self) -> CompletionRequest {
        self.requests
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("a request was sent")
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().unwrap().push(request);
        self.results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(ProviderError::Parse("scripted provider exhausted".into())))
    }
}

/// Always fails with `ModelNotFound` (exercises the fallback chain).
pub struct AlwaysModelNotFound;

#[async_trait]
impl Provider for AlwaysModelNotFound {
    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        Err(ProviderError::ModelNotFound("missing".into()))
    }
}

/// A provider whose Nth `complete` call blocks until a gate is opened, so a
/// test can make cancellation arrive *while a model call is in flight*. Other
/// calls return immediately. `entered()` fires once the gated call is pending.
pub struct GatedProvider {
    results: Mutex<VecDeque<Result<CompletionResponse, ProviderError>>>,
    requests: Mutex<Vec<CompletionRequest>>,
    calls: Mutex<usize>,
    gate_on_call: usize,
    entered: Arc<tokio::sync::Notify>,
    gate: Arc<tokio::sync::Notify>,
}

impl GatedProvider {
    /// `gate_on_call` is 1-based: the call index that blocks on the gate.
    pub fn new(
        results: Vec<Result<CompletionResponse, ProviderError>>,
        gate_on_call: usize,
    ) -> Self {
        Self {
            results: Mutex::new(results.into()),
            requests: Mutex::new(Vec::new()),
            calls: Mutex::new(0),
            gate_on_call,
            entered: Arc::new(tokio::sync::Notify::new()),
            gate: Arc::new(tokio::sync::Notify::new()),
        }
    }
    /// Resolves once the gated call is parked.
    pub fn entered(&self) -> Arc<tokio::sync::Notify> {
        self.entered.clone()
    }
    /// Release the parked call.
    pub fn open_gate(&self) {
        self.gate.notify_one();
    }
    pub fn call_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

#[async_trait]
impl Provider for GatedProvider {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().unwrap().push(request);
        let this_call = {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            *c
        };
        if this_call == self.gate_on_call {
            self.entered.notify_one();
            self.gate.notified().await;
        }
        self.results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(ProviderError::Parse("gated provider exhausted".into())))
    }
}

/// A provider with fully scripted streaming behavior: it emits a fixed list of
/// [`StreamEvent`]s on `stream`, then returns a configured stream result;
/// `complete` returns a separate configured result (used by the agent's
/// fallback-to-non-streaming path).
pub struct StreamProvider {
    events: Mutex<Vec<runic_provider::StreamEvent>>,
    stream_result: Mutex<Option<Result<CompletionResponse, ProviderError>>>,
    complete_result: Mutex<Option<Result<CompletionResponse, ProviderError>>>,
}

impl StreamProvider {
    pub fn new(
        events: Vec<runic_provider::StreamEvent>,
        stream_result: Result<CompletionResponse, ProviderError>,
        complete_result: Result<CompletionResponse, ProviderError>,
    ) -> Self {
        Self {
            events: Mutex::new(events),
            stream_result: Mutex::new(Some(stream_result)),
            complete_result: Mutex::new(Some(complete_result)),
        }
    }
}

#[async_trait]
impl Provider for StreamProvider {
    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.complete_result
            .lock()
            .unwrap()
            .take()
            .expect("complete_result consumed twice")
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
        tx: tokio::sync::mpsc::Sender<runic_provider::StreamEvent>,
    ) -> Result<CompletionResponse, ProviderError> {
        let events = std::mem::take(&mut *self.events.lock().unwrap());
        for ev in events {
            let _ = tx.send(ev).await;
        }
        self.stream_result
            .lock()
            .unwrap()
            .take()
            .expect("stream_result consumed twice")
    }
}

// ─── Tools ───────────────────────────────────────────────────────────────────

/// Records every `(name, input)` it's executed with and returns a fixed output.
pub struct RecordingTool {
    name: String,
    output: String,
    parallel: bool,
    pub calls: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
}

impl RecordingTool {
    pub fn new(name: &str, output: &str) -> Self {
        Self {
            name: name.into(),
            output: output.into(),
            parallel: false,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn parallel(mut self) -> Self {
        self.parallel = true;
        self
    }

    /// A shared handle to the recorded call log (clone before `Arc::new`-ing).
    pub fn log(&self) -> Arc<Mutex<Vec<(String, serde_json::Value)>>> {
        self.calls.clone()
    }
}

#[async_trait]
impl Tool for RecordingTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "records its inputs"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "additionalProperties": true })
    }
    fn parallelizable(&self) -> bool {
        self.parallel
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        self.calls
            .lock()
            .unwrap()
            .push((self.name.clone(), args.clone()));
        Ok(ToolResult::ok(format!("{}: {args}", self.output)))
    }
}

/// Fails by returning an `Err` from `execute`.
pub struct ErrTool;

#[async_trait]
impl Tool for ErrTool {
    fn name(&self) -> &str {
        "err_tool"
    }
    fn description(&self) -> &str {
        "always returns Err"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Err(anyhow::anyhow!("boom from inside the tool"))
    }
}

/// Fails by returning `ToolResult::error` (an in-band, model-facing error).
pub struct ErrResultTool;

#[async_trait]
impl Tool for ErrResultTool {
    fn name(&self) -> &str {
        "err_result_tool"
    }
    fn description(&self) -> &str {
        "returns an in-band error result"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::error("could not do the thing"))
    }
}

/// Panics during execution (must be caught and turned into an error result).
pub struct PanicTool;

#[async_trait]
impl Tool for PanicTool {
    fn name(&self) -> &str {
        "panic_tool"
    }
    fn description(&self) -> &str {
        "panics"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        panic!("tool blew up")
    }
}

/// Returns a big full output plus a short persisted summary — the artifact /
/// transient-output path.
pub struct SummaryTool {
    full: String,
    summary: String,
}

impl SummaryTool {
    pub fn new(full: &str, summary: &str) -> Self {
        Self {
            full: full.into(),
            summary: summary.into(),
        }
    }
}

#[async_trait]
impl Tool for SummaryTool {
    fn name(&self) -> &str {
        "summary_tool"
    }
    fn description(&self) -> &str {
        "full output to the model, summary to the log"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok(self.full.clone()).with_persisted_summary(self.summary.clone()))
    }
}

/// Flips a shared [`CancelToken`] when run — to exercise mid-run cancellation
/// triggered from inside a tool.
pub struct CancelTool {
    pub token: runic_agent::CancelToken,
}

#[async_trait]
impl Tool for CancelTool {
    fn name(&self) -> &str {
        "cancel_tool"
    }
    fn description(&self) -> &str {
        "cancels the run"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        self.token.cancel();
        Ok(ToolResult::ok("cancelled the run"))
    }
}

/// A parallelizable tool that forces a *deterministic* completion order via
/// gates (no timing). Each call carries `args["tag"]`; the tool releases tags in
/// the configured `completion_order`, so completion order is fixed regardless of
/// issue order — and crucially, a serial (issue-order) executor would deadlock,
/// so a pass genuinely proves the calls ran concurrently. It records each tag as
/// it completes.
pub struct OrderedGateTool {
    name: String,
    completion_order: Vec<String>,
    gates: Vec<Arc<tokio::sync::Notify>>,
    pub completions: Arc<Mutex<Vec<String>>>,
}

impl OrderedGateTool {
    /// `completion_order` lists tags in the order they must finish.
    pub fn new(name: &str, completion_order: Vec<&str>) -> Self {
        let gates = completion_order
            .iter()
            .map(|_| Arc::new(tokio::sync::Notify::new()))
            .collect();
        Self {
            name: name.into(),
            completion_order: completion_order.into_iter().map(String::from).collect(),
            gates,
            completions: Arc::new(Mutex::new(Vec::new())),
        }
    }
    pub fn completions(&self) -> Arc<Mutex<Vec<String>>> {
        self.completions.clone()
    }
}

#[async_trait]
impl Tool for OrderedGateTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "completes in a fixed, gated order"
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
        let tag = args
            .get("tag")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let pos = self
            .completion_order
            .iter()
            .position(|t| *t == tag)
            .expect("tag is in the completion order");
        // Wait my turn (the first in the order proceeds immediately).
        if pos > 0 {
            self.gates[pos].notified().await;
        }
        self.completions.lock().unwrap().push(tag.clone());
        // Release the next in line.
        if let Some(next) = self.gates.get(pos + 1) {
            next.notify_one();
        }
        Ok(ToolResult::ok(format!("done: {tag}")))
    }
}

// ─── Hooks ───────────────────────────────────────────────────────────────────

/// What a recording hook does at a given lifecycle point.
#[derive(Clone)]
pub enum Act {
    Continue,
    Stop,
    Cancel(String),
    /// Only meaningful at `before_tool` for write hooks.
    Substitute(String),
    /// Push a user message into state (a `before_model` mutation, write hooks).
    Inject(String),
}

/// A [`WriteHook`] that logs `"{name}:{point}"` in firing order, and applies a
/// configured [`Act`] at chosen points.
pub struct RecordWriteHook {
    name: String,
    priority: i32,
    pub log: Arc<Mutex<Vec<String>>>,
    actions: HashMap<&'static str, Act>,
}

impl RecordWriteHook {
    pub fn new(name: &str, log: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            name: name.into(),
            priority: 0,
            log,
            actions: HashMap::new(),
        }
    }
    pub fn priority(mut self, p: i32) -> Self {
        self.priority = p;
        self
    }
    pub fn act(mut self, point: &'static str, act: Act) -> Self {
        self.actions.insert(point, act);
        self
    }

    async fn run(&self, point: &'static str, state: &mut AgentState) -> HookOutcome {
        self.log
            .lock()
            .unwrap()
            .push(format!("{}:{point}", self.name));
        match self.actions.get(point) {
            None | Some(Act::Continue) => HookOutcome::Continue,
            Some(Act::Stop) => HookOutcome::Stop,
            Some(Act::Cancel(r)) => HookOutcome::Cancel(r.clone()),
            Some(Act::Substitute(s)) => {
                HookOutcome::SubstituteToolResult(ToolResult::ok(s.clone()))
            }
            Some(Act::Inject(text)) => {
                state.push_event(SessionEvent::Message {
                    run_id: state
                        .current_run()
                        .map(|r| r.id)
                        .unwrap_or_else(|| "r".into()),
                    msg: runic_types::Message::user(text.clone()),
                    at: chrono::Utc::now(),
                });
                HookOutcome::Continue
            }
        }
    }
}

#[async_trait]
impl WriteHook for RecordWriteHook {
    fn name(&self) -> &str {
        &self.name
    }
    fn priority(&self) -> i32 {
        self.priority
    }
    async fn before_agent(&self, state: &mut AgentState) -> HookOutcome {
        self.run("before_agent", state).await
    }
    async fn before_model(&self, state: &mut AgentState) -> HookOutcome {
        self.run("before_model", state).await
    }
    async fn before_tool(&self, state: &mut AgentState, call: &mut ToolCall) -> HookOutcome {
        self.log
            .lock()
            .unwrap()
            .push(format!("{}:before_tool", self.name));
        match self.actions.get("before_tool") {
            None | Some(Act::Continue) => HookOutcome::Continue,
            Some(Act::Stop) => HookOutcome::Stop,
            Some(Act::Cancel(r)) => HookOutcome::Cancel(r.clone()),
            Some(Act::Substitute(s)) => {
                HookOutcome::SubstituteToolResult(ToolResult::ok(s.clone()))
            }
            Some(Act::Inject(_)) => {
                let _ = (state, call);
                HookOutcome::Continue
            }
        }
    }
    async fn after_model(&self, state: &mut AgentState) -> HookOutcome {
        self.run("after_model", state).await
    }
    async fn after_tool(
        &self,
        _state: &mut AgentState,
        _call: &ToolCall,
        _result: &ToolResult,
    ) -> HookOutcome {
        self.log
            .lock()
            .unwrap()
            .push(format!("{}:after_tool", self.name));
        match self.actions.get("after_tool") {
            Some(Act::Stop) => HookOutcome::Stop,
            Some(Act::Cancel(r)) => HookOutcome::Cancel(r.clone()),
            _ => HookOutcome::Continue,
        }
    }
    async fn after_agent(&self, state: &mut AgentState) -> HookOutcome {
        self.run("after_agent", state).await
    }
}

/// A [`ReadHook`] that logs its firing order and can `Stop`.
pub struct RecordReadHook {
    name: String,
    priority: i32,
    pub log: Arc<Mutex<Vec<String>>>,
    stop_at: Option<&'static str>,
}

impl RecordReadHook {
    pub fn new(name: &str, log: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            name: name.into(),
            priority: 0,
            log,
            stop_at: None,
        }
    }
    pub fn stop_at(mut self, point: &'static str) -> Self {
        self.stop_at = Some(point);
        self
    }

    fn signal(&self, point: &'static str) -> HookSignal {
        self.log
            .lock()
            .unwrap()
            .push(format!("read:{}:{point}", self.name));
        if self.stop_at == Some(point) {
            HookSignal::Stop
        } else {
            HookSignal::Continue
        }
    }
}

#[async_trait]
impl ReadHook for RecordReadHook {
    fn name(&self) -> &str {
        &self.name
    }
    fn priority(&self) -> i32 {
        self.priority
    }
    async fn before_agent(&self, _state: &AgentState) -> HookSignal {
        self.signal("before_agent")
    }
    async fn before_model(&self, _state: &AgentState) -> HookSignal {
        self.signal("before_model")
    }
    async fn before_tool(&self, _state: &AgentState, _call: &ToolCall) -> HookSignal {
        self.signal("before_tool")
    }
    async fn after_model(&self, _state: &AgentState) -> HookSignal {
        self.signal("after_model")
    }
    async fn after_tool(
        &self,
        _state: &AgentState,
        _call: &ToolCall,
        _result: &ToolResult,
    ) -> HookSignal {
        self.signal("after_tool")
    }
    async fn after_agent(&self, _state: &AgentState) -> HookSignal {
        self.signal("after_agent")
    }
}

// ─── Event capture ───────────────────────────────────────────────────────────

/// Install a lossless persist sink on the agent and return its receiver. Every
/// [`SessionEvent`] the run pushes lands here in order.
pub fn capture_session_events(agent: &mut Agent) -> mpsc::UnboundedReceiver<SessionEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    agent.state_mut().set_persist_tx(tx);
    rx
}

/// Drain everything currently queued on an unbounded receiver.
pub fn drain<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> Vec<T> {
    let mut out = Vec::new();
    while let Ok(v) = rx.try_recv() {
        out.push(v);
    }
    out
}

/// The `(tool_use_id, content, is_error)` of every `tool_result` block across a
/// message list, in order.
pub fn tool_results(messages: &[runic_types::Message]) -> Vec<(String, String, bool)> {
    messages
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::Blocks(b) => Some(b),
            _ => None,
        })
        .flatten()
        .filter_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => Some((tool_use_id.clone(), content.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

/// A short label per [`SessionEvent`] for exact-ordering assertions.
pub fn session_kinds(evs: &[SessionEvent]) -> Vec<&'static str> {
    evs.iter()
        .map(|e| match e {
            SessionEvent::RunStart { .. } => "RunStart",
            SessionEvent::RunEnd { .. } => "RunEnd",
            SessionEvent::Message { .. } => "Message",
            SessionEvent::TurnBoundary { .. } => "TurnBoundary",
            SessionEvent::HookRan { .. } => "HookRan",
            SessionEvent::StateSnapshot { .. } => "StateSnapshot",
        })
        .collect()
}

/// A raw response whose `content` blocks and `tool_calls` can be set
/// independently — to exercise provider responses where the two disagree.
pub fn mismatched_response(
    content: Vec<ContentBlock>,
    tool_calls: Vec<ToolCall>,
    stop_reason: StopReason,
) -> CompletionResponse {
    CompletionResponse {
        content,
        stop_reason,
        tool_calls,
        usage: TokenUsage {
            input_tokens: 1,
            output_tokens: 1,
        },
    }
}

/// Collect every `tool_result` content string across a message list (provider
/// view or a captured request).
pub fn tool_result_contents(messages: &[runic_types::Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::Blocks(b) => Some(b),
            _ => None,
        })
        .flatten()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect()
}
