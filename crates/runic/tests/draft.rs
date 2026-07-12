use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::ability::ability;
use runic::composer::{Compose, ComposeError, Composer};
use runic::deferred::ability_activated_key;
use runic_agent::Agent;
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

fn call(call_id: &str, name: &str, input: serde_json::Value) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::ToolUse {
            id: call_id.into(),
            name: name.into(),
            input: input.clone(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: call_id.into(),
            name: name.into(),
            input,
        }],
        usage: TokenUsage::default(),
    }
}

struct CountingTool {
    name: &'static str,
    calls: Arc<Mutex<u32>>,
}

#[async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "counts invocations"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        *self.calls.lock().unwrap() += 1;
        Ok(ToolResult::ok("counted"))
    }
}

struct MarkerHook(Arc<Mutex<bool>>);

#[async_trait]
impl WriteHook for MarkerHook {
    fn name(&self) -> &str {
        "marker"
    }
    fn points(&self) -> &'static [HookLifecycle] {
        &[HookLifecycle::AfterAgent]
    }
    async fn after_agent(&self, _state: &mut AgentState) -> HookOutcome {
        *self.0.lock().unwrap() = true;
        HookOutcome::Continue
    }
}

fn state_flag(agent: &Agent, key: &str) -> bool {
    agent
        .state()
        .data()
        .get(key)
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

#[tokio::test]
async fn an_eager_draft_registers_prompt_tools_and_hooks() {
    let counted = Arc::new(Mutex::new(0));
    let fired = Arc::new(Mutex::new(false));
    let provider = ScriptedProvider::new(vec![
        call("c1", "refund", serde_json::json!({})),
        text("done"),
    ]);
    let mut agent = Composer::new(provider, "test-model")
        .instructions("core")
        .with(
            ability("core-pack")
                .prompt("house rules")
                .tool(CountingTool {
                    name: "refund",
                    calls: counted.clone(),
                })
                .hook(MarkerHook(fired.clone())),
        )
        .build("alice", "s1")
        .await
        .unwrap();

    assert!(agent.state().system_prompt.contains("house rules"));

    agent.run("go").await.unwrap();

    assert_eq!(*counted.lock().unwrap(), 1);
    assert!(*fired.lock().unwrap());
}

#[tokio::test]
async fn a_deferred_draft_loads_by_id() {
    let counted = Arc::new(Mutex::new(0));
    let provider = ScriptedProvider::new(vec![
        call("c1", "load_ability", serde_json::json!({ "id": "billing" })),
        call("c2", "refund", serde_json::json!({})),
        text("done"),
    ]);
    let mut agent = Composer::new(provider, "test-model")
        .with(
            ability("billing")
                .describe("invoices and refunds")
                .deferred()
                .prompt("billing rules")
                .tool(CountingTool {
                    name: "refund",
                    calls: counted.clone(),
                }),
        )
        .build("alice", "s1")
        .await
        .unwrap();

    let system = agent.state().system_prompt.clone();
    assert!(system.contains("- billing: invoices and refunds"));
    assert!(!system.contains("billing rules"));

    agent.run("go").await.unwrap();

    assert_eq!(*counted.lock().unwrap(), 1);
    assert!(state_flag(&agent, &ability_activated_key("billing")));
}

#[tokio::test]
async fn two_drafts_with_the_same_id_are_rejected() {
    let result = Composer::new(ScriptedProvider::new(vec![]), "test-model")
        .with(ability("dup"))
        .with(ability("dup"))
        .build("alice", "s1")
        .await;

    match result {
        Err(ComposeError::DuplicateAbilityId { id, .. }) => assert_eq!(id, "dup"),
        Err(other) => panic!("expected duplicate id error, got {other}"),
        Ok(_) => panic!("duplicate draft ids must fail"),
    }
}

#[test]
fn a_model_spec_without_a_provider_prefix_is_rejected() {
    match Agent::compose("mistral-medium-latest") {
        Err(ComposeError::InvalidModelSpec { spec }) => {
            assert_eq!(spec, "mistral-medium-latest");
        }
        Err(other) => panic!("expected invalid spec error, got {other}"),
        Ok(_) => panic!("spec without provider prefix must fail"),
    }
}

#[test]
fn an_unknown_provider_is_rejected_with_a_helpful_error() {
    match Agent::compose("hal9000:redundant-unit") {
        Err(ComposeError::UnknownProvider { name }) => assert_eq!(name, "hal9000"),
        Err(other) => panic!("expected unknown provider error, got {other}"),
        Ok(_) => panic!("unknown provider must fail"),
    }
}
