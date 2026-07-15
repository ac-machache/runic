use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic_agent::{Agent, RunContext};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_state::SessionEvent;
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, Message, MessageContent, StopReason, TokenUsage, ToolCall};

struct ScriptedProvider {
    responses: Mutex<VecDeque<CompletionResponse>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<CompletionResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
        })
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::Parse("exhausted".into()))
    }
}

fn ask_call() -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::ToolUse {
            id: "c1".into(),
            name: "ask".into(),
            input: serde_json::json!({}),
            provider_metadata: None,
        }],
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "ask".into(),
            input: serde_json::json!({}),
        }],
        usage: TokenUsage::default(),
    }
}

fn text(t: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::Text {
            text: t.into(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::EndTurn,
        tool_calls: vec![],
        usage: TokenUsage::default(),
    }
}

struct AskTool;

#[async_trait]
impl Tool for AskTool {
    fn name(&self) -> &str {
        "ask"
    }
    fn description(&self) -> &str {
        "asks a human"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::defer(
            "human_ask",
            serde_json::json!({ "question": "proceed?" }),
        ))
    }
}

struct RecordingProvider {
    responses: Mutex<VecDeque<CompletionResponse>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl RecordingProvider {
    fn new(responses: Vec<CompletionResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl Provider for RecordingProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().unwrap().push(req);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::Parse("exhausted".into()))
    }
}

fn kinds(evs: &[SessionEvent]) -> Vec<&'static str> {
    evs.iter()
        .map(|e| match e {
            SessionEvent::RunStart { .. } => "RunStart",
            SessionEvent::RunEnd { .. } => "RunEnd",
            SessionEvent::Message { .. } => "Message",
            SessionEvent::TurnEnd { .. } => "TurnEnd",
            SessionEvent::ToolDeferred { .. } => "ToolDeferred",
            _ => "other",
        })
        .collect()
}

#[tokio::test]
async fn a_deferring_tool_suspends_the_run_and_resume_continues_it() {
    let provider = ScriptedProvider::new(vec![ask_call(), text("done")]);
    let mut agent = Agent::builder(provider, "alice", "s1")
        .model("m")
        .tool(Arc::new(AskTool))
        .build();

    let out = agent
        .run_with("go", RunContext::new().with_run_id("r1"))
        .await
        .unwrap();
    assert_eq!(out.stop_reason.as_deref(), Some("suspended"));

    let log = kinds(agent.state().events());
    assert!(log.contains(&"ToolDeferred"), "log = {log:?}");
    assert!(
        !log.contains(&"RunEnd"),
        "a suspended run has no RunEnd yet"
    );
    let tool_results = agent
        .state()
        .events()
        .iter()
        .filter(|e| {
            matches!(e, SessionEvent::Message { msg, .. }
            if matches!(&msg.content, MessageContent::Blocks(b)
                if b.iter().any(|blk| matches!(blk, ContentBlock::ToolResult { .. }))))
        })
        .count();
    assert_eq!(tool_results, 0, "the ask tool_use is left dangling");

    agent.state_mut().push_event(SessionEvent::Message {
        run_id: "r1".into(),
        msg: Message::user_with_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "c1".into(),
            tool_name: "ask".into(),
            content: "yes".into(),
            is_error: false,
        }]),
        at: chrono::Utc::now(),
    });

    let out2 = agent
        .resume(RunContext::new().with_run_id("r1"))
        .await
        .unwrap();
    assert_eq!(out2.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(agent.state().last_assistant_text().as_deref(), Some("done"));

    let terminals: Vec<_> = agent
        .state()
        .events()
        .iter()
        .filter_map(|e| match e {
            SessionEvent::RunEnd { status, .. } => Some(status.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        terminals.len(),
        1,
        "exactly one terminal across suspend+resume"
    );
    assert!(matches!(terminals[0], runic_state::RunEndStatus::Completed));
}

#[tokio::test]
async fn suspended_run_records_the_exact_deferral_payload() {
    let provider = ScriptedProvider::new(vec![ask_call()]);
    let mut agent = Agent::builder(provider, "alice", "s1")
        .model("m")
        .tool(Arc::new(AskTool))
        .build();

    let out = agent
        .run_with("go", RunContext::new().with_run_id("r1"))
        .await
        .unwrap();
    assert_eq!(out.stop_reason.as_deref(), Some("suspended"));

    let deferred = agent
        .state()
        .events()
        .iter()
        .find_map(|e| match e {
            SessionEvent::ToolDeferred {
                run_id,
                call_id,
                channel,
                payload,
                ..
            } => Some((run_id, call_id, channel, payload)),
            _ => None,
        })
        .expect("deferral is durable");
    assert_eq!(deferred.0, "r1");
    assert_eq!(deferred.1, "c1");
    assert_eq!(deferred.2, "human_ask");
    assert_eq!(deferred.3["question"], "proceed?");
}

#[tokio::test]
async fn resume_does_not_append_a_second_run_start_or_user_message() {
    let provider = ScriptedProvider::new(vec![ask_call(), text("done")]);
    let mut agent = Agent::builder(provider, "alice", "s1")
        .model("m")
        .tool(Arc::new(AskTool))
        .build();

    agent
        .run_with("go", RunContext::new().with_run_id("r1"))
        .await
        .unwrap();
    agent.state_mut().push_event(SessionEvent::Message {
        run_id: "r1".into(),
        msg: Message::user_with_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "c1".into(),
            tool_name: "ask".into(),
            content: "yes".into(),
            is_error: false,
        }]),
        at: chrono::Utc::now(),
    });
    agent
        .resume(RunContext::new().with_run_id("r1"))
        .await
        .unwrap();

    let events = agent.state().events();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, SessionEvent::RunStart { .. }))
            .count(),
        1,
        "resume must continue the existing run, not start another one"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, SessionEvent::Message { msg, .. }
                if matches!(msg.content, MessageContent::Text(ref text) if text == "go")))
            .count(),
        1,
        "resume must not append the original user input again"
    );
}

#[tokio::test]
async fn resume_sends_the_injected_tool_result_to_the_model() {
    let provider = RecordingProvider::new(vec![ask_call(), text("done")]);
    let mut agent = Agent::builder(provider.clone(), "alice", "s1")
        .model("m")
        .tool(Arc::new(AskTool))
        .build();

    agent
        .run_with("go", RunContext::new().with_run_id("r1"))
        .await
        .unwrap();
    agent.state_mut().push_event(SessionEvent::Message {
        run_id: "r1".into(),
        msg: Message::user_with_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "c1".into(),
            tool_name: "ask".into(),
            content: "human said yes".into(),
            is_error: false,
        }]),
        at: chrono::Utc::now(),
    });

    agent
        .resume(RunContext::new().with_run_id("r1"))
        .await
        .unwrap();

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let second = serde_json::to_value(&requests[1].messages).unwrap();
    assert!(
        second.to_string().contains("human said yes"),
        "resume request must include the injected tool result: {second}"
    );
}

#[tokio::test]
async fn a_fresh_run_after_suspension_does_not_re_emit_the_old_deferral() {
    let provider = ScriptedProvider::new(vec![ask_call(), text("fresh done")]);
    let mut agent = Agent::builder(provider, "alice", "s1")
        .model("m")
        .tool(Arc::new(AskTool))
        .build();

    agent
        .run_with("go", RunContext::new().with_run_id("r1"))
        .await
        .unwrap();
    let before = agent
        .state()
        .events()
        .iter()
        .filter(|e| matches!(e, SessionEvent::ToolDeferred { .. }))
        .count();

    let out = agent
        .run_with("new request", RunContext::new().with_run_id("r2"))
        .await
        .unwrap();
    assert_eq!(out.stop_reason.as_deref(), Some("end_turn"));
    let after = agent
        .state()
        .events()
        .iter()
        .filter(|e| matches!(e, SessionEvent::ToolDeferred { .. }))
        .count();
    assert_eq!(
        after, before,
        "stale pending deferral leaked into a new run"
    );
}
