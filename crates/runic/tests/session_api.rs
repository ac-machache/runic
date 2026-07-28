use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::ability::ability;
use runic::tool::{Tool, ToolContext, ToolResult};
use runic::{Agent, Llm, subagent};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_substrate::{
    ArtifactStore, MemoryArtifactStore, MemorySessionStore, SessionEvent, SessionStore,
};
use runic_types::{ContentBlock, StopReason, TokenUsage, ToolCall};

struct ScriptedProvider {
    responses: Mutex<VecDeque<CompletionResponse>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<CompletionResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().unwrap().push(req);
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

#[tokio::test]
async fn agent_runs_standalone_and_stateless() {
    let provider = ScriptedProvider::new(vec![text("hello there")]);
    let agent = Agent::new(Llm::new(provider, "test-model"));

    let out = agent.run("hi").await.unwrap();
    assert_eq!(out.text, "hello there");
    assert_eq!(out.outcome.total_turns, 1);
}

#[tokio::test]
async fn session_persists_and_hydrates_across_runs() {
    let provider = ScriptedProvider::new(vec![text("my name is Ada"), text("you are Ada")]);
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let agent = Agent::new(Llm::new(provider.clone(), "test-model"));

    let first = agent
        .session(store.clone(), "tenant", "thread-1")
        .run("remember my name")
        .await
        .unwrap();
    assert_eq!(first.text, "my name is Ada");

    let stored = store.read("tenant", "thread-1").await.unwrap();
    assert!(
        !stored.is_empty(),
        "the run's events are appended to the store"
    );

    let second = agent
        .session(store.clone(), "tenant", "thread-1")
        .run("what is my name")
        .await
        .unwrap();
    assert_eq!(second.text, "you are Ada");

    let second_request = &provider.requests()[1];
    let texts: Vec<String> = second_request
        .messages
        .iter()
        .map(|m| m.content.text_content())
        .collect();
    assert!(
        texts.iter().any(|t| t == "remember my name"),
        "run 2 hydrated run 1's user message: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t == "my name is Ada"),
        "run 2 hydrated run 1's assistant reply: {texts:?}"
    );
}

struct SpillingTool;

#[async_trait]
impl Tool for SpillingTool {
    fn name(&self) -> &str {
        "dump"
    }

    fn description(&self) -> &str {
        "returns a large blob that spills to the artifact store"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok("x".repeat(4096)).spill())
    }
}

#[tokio::test]
async fn agent_with_an_artifact_store_spills_a_tool_output_to_it() {
    let provider = ScriptedProvider::new(vec![
        call("c1", "dump", serde_json::json!({})),
        text("done"),
    ]);
    let artifacts: Arc<MemoryArtifactStore> = Arc::new(MemoryArtifactStore::new());
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());

    let agent = Agent::new(Llm::new(provider, "test-model"))
        .with(ability("dumper").tool(SpillingTool))
        .artifacts(artifacts.clone() as Arc<dyn ArtifactStore>);

    let out = agent
        .session(store.clone(), "tenant", "t1")
        .run("go")
        .await
        .unwrap();
    assert_eq!(out.text, "done");

    let listed = artifacts.list("tenant", "t1").await.unwrap();
    assert_eq!(
        listed.len(),
        1,
        "the spilled tool output landed in the artifact store"
    );
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

fn delegate_to(agent: &str) -> CompletionResponse {
    call(
        "c1",
        "delegate",
        serde_json::json!({ "agent": agent, "prompt": "dig" }),
    )
}

#[subagent(name = "researcher", description = "digs")]
struct Researcher(Arc<ScriptedProvider>);

impl Researcher {
    async fn agent(&self, _llm: Llm) -> anyhow::Result<Agent> {
        Ok(Agent::new(Llm::new(self.0.clone(), "child-model")))
    }
}

#[tokio::test]
async fn session_persists_a_delegated_run_to_its_own_child_session() {
    let parent = ScriptedProvider::new(vec![delegate_to("researcher"), text("parent done")]);
    let child = ScriptedProvider::new(vec![text("child found it")]);
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());

    let agent = Agent::new(Llm::new(parent, "main-model")).with(Researcher(child));

    let out = agent
        .session(store.clone(), "tenant", "t1")
        .run("go")
        .await
        .unwrap();
    assert_eq!(out.text, "parent done");

    let parent_log = store.read("tenant", "t1").await.unwrap();
    let child_session = parent_log
        .iter()
        .find_map(|e| match &e.event {
            SessionEvent::DelegationFinished { child_session, .. } => child_session.clone(),
            _ => None,
        })
        .expect("a DelegationFinished carrying the child session id");

    let child_log = store.read("tenant", &child_session).await.unwrap();
    let child_texts: Vec<String> = child_log
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::Message { msg, .. } => Some(msg.content.text_content()),
            _ => None,
        })
        .collect();
    assert!(
        child_texts.iter().any(|t| t.contains("child found it")),
        "the child's transcript persisted to its own session: {child_texts:?}"
    );
}
