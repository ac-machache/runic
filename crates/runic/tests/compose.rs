use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::compose::{Compose, Hooks, Skills, Tools};
use runic_agent::Agent;
use runic_hook::{HookLifecycle, HookOutcome, WriteHook};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_skills::SkillSet;
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

fn call(name: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::ToolUse {
            id: "c1".into(),
            name: name.into(),
            input: serde_json::json!({}),
            provider_metadata: None,
        }],
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: name.into(),
            input: serde_json::json!({}),
        }],
        usage: TokenUsage::default(),
    }
}

struct Ping(Arc<Mutex<u32>>);

#[async_trait]
impl Tool for Ping {
    fn name(&self) -> &str {
        "ping"
    }
    fn description(&self) -> &str {
        "pings"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        *self.0.lock().unwrap() += 1;
        Ok(ToolResult::ok("pong"))
    }
}

async fn crm_catalog() -> Arc<SkillSet> {
    let dir = tempfile::tempdir().unwrap();
    let skill = dir.path().join("pipeline");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: pipeline\ndescription: crm pipeline\n---\nBody.",
    )
    .unwrap();
    Arc::new(SkillSet::load_dir("crm", dir.path()).await)
}

#[tokio::test]
async fn composes_the_prompt_from_instructions_and_abilities() {
    let provider = ScriptedProvider::new(vec![text("done")]);
    let agent = Agent::compose(provider, "test-model")
        .instructions("core instructions")
        .with(Skills(crm_catalog().await))
        .build("alice", "s1")
        .await;

    let system = &agent.state().system_prompt;
    assert!(system.contains("core instructions"));
    assert!(system.contains("<available-skills>"));
    assert!(system.contains("crm:pipeline"));
}

struct Marker(Arc<Mutex<bool>>);

#[async_trait]
impl WriteHook for Marker {
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

#[tokio::test]
async fn a_hooks_ability_registers_custom_write_hooks() {
    let provider = ScriptedProvider::new(vec![text("done")]);
    let fired = Arc::new(Mutex::new(false));
    let mut agent = Agent::compose(provider, "test-model")
        .instructions("go")
        .with(Hooks(vec![Arc::new(Marker(fired.clone()))]))
        .build("alice", "s1")
        .await;

    agent.run("hi").await.unwrap();

    assert!(*fired.lock().unwrap(), "custom write hook must fire");
}

#[tokio::test]
async fn a_tools_ability_registers_runnable_tools() {
    let provider = ScriptedProvider::new(vec![call("ping"), text("done")]);
    let pings = Arc::new(Mutex::new(0));
    let mut agent = Agent::compose(provider, "test-model")
        .instructions("go")
        .with(Tools(vec![Arc::new(Ping(pings.clone()))]))
        .build("alice", "s1")
        .await;

    agent.run("hi").await.unwrap();

    assert_eq!(*pings.lock().unwrap(), 1);
}
