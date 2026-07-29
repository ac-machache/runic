use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::Llm;
use runic::ability::ability;
use runic::composer::{Agent, ComposeError};
use runic_hook::{HookLifecycle, HookOutcome, WriteHook};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_state::AgentState;
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, StopReason, TokenUsage, ToolCall};

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

fn text(content: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::Text {
            text: content.into(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::EndTurn,
        tool_calls: vec![],
        usage: TokenUsage::default(),
    }
}

fn call(id: &str, name: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input: serde_json::json!({}),
            provider_metadata: None,
        }],
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            input: serde_json::json!({}),
        }],
        usage: TokenUsage::default(),
    }
}

struct Noop(&'static str);

#[async_trait]
impl Tool for Noop {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "does nothing"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok(format!("ran {}", self.0)))
    }
}

#[tokio::test]
async fn a_tool_attached_to_the_agent_runs_without_an_ability() {
    let provider = ScriptedProvider::new(vec![call("c1", "alpha"), text("done")]);
    let mut agent = Agent::new(Llm::new(provider, "test-model"))
        .tool(Noop("alpha"))
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("go").await.unwrap();

    let ran = agent.state().messages_for_provider().iter().any(|msg| {
        matches!(&msg.content, runic_types::MessageContent::Blocks(blocks)
            if blocks.iter().any(|block| matches!(block,
                ContentBlock::ToolResult { content, .. } if content.text() == "ran alpha")))
    });
    assert!(ran, "a bare tool must reach the model without a wrapper");
}

struct Counter {
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl WriteHook for Counter {
    fn name(&self) -> &str {
        "counter"
    }
    fn points(&self) -> &'static [HookLifecycle] {
        &[HookLifecycle::BeforeTool]
    }
    async fn before_tool(&self, _state: &mut AgentState, call: &mut ToolCall) -> HookOutcome {
        self.seen.lock().unwrap().push(call.name.clone());
        HookOutcome::Continue
    }
}

#[tokio::test]
async fn an_agent_hook_sees_calls_it_does_not_own() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = ScriptedProvider::new(vec![call("c1", "alpha"), call("c2", "beta"), text("ok")]);

    let mut agent = Agent::new(Llm::new(provider, "test-model"))
        .with(ability("pack").tool(Noop("alpha")))
        .tool(Noop("beta"))
        .hook(Counter { seen: seen.clone() })
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("go").await.unwrap();

    assert_eq!(
        *seen.lock().unwrap(),
        vec!["alpha", "beta"],
        "an agent hook is agent-wide, so it must fire for an ability's tool too"
    );
}

struct AfterRun;

#[async_trait]
impl WriteHook for AfterRun {
    fn name(&self) -> &str {
        "after-run"
    }
    fn points(&self) -> &'static [HookLifecycle] {
        &[HookLifecycle::AfterAgent]
    }
}

#[tokio::test]
async fn a_run_boundary_hook_is_legal_on_the_agent_but_not_on_an_ability() {
    let provider = ScriptedProvider::new(vec![text("done")]);
    Agent::new(Llm::new(provider, "test-model"))
        .hook(AfterRun)
        .build("alice", "s1")
        .await
        .expect("the agent owns the run boundary");

    let provider = ScriptedProvider::new(vec![text("done")]);
    let rejected = Agent::new(Llm::new(provider, "test-model"))
        .with(ability("pack").hook(AfterRun))
        .build("alice", "s1")
        .await;

    assert!(matches!(
        rejected,
        Err(ComposeError::AbilityLifecycleHook { .. })
    ));
}

#[tokio::test]
async fn an_ability_cannot_claim_the_reserved_agent_id() {
    let provider = ScriptedProvider::new(vec![text("done")]);
    let result = Agent::new(Llm::new(provider, "test-model"))
        .with(ability("agent").tool(Noop("alpha")))
        .build("alice", "s1")
        .await;

    match result {
        Err(ComposeError::DuplicateAbilityId { id, .. }) => assert_eq!(id, "agent"),
        Err(other) => panic!("expected the reserved id to be refused, got {other}"),
        Ok(_) => panic!("an ability must not claim the reserved `agent` id"),
    }
}
