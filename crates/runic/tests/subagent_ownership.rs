use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::ability::ability;
use runic::composer::Agent;
use runic::{Llm, agent};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_tool::{Tool, ToolContext, ToolResult};
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

    fn request_tool_names(&self, index: usize) -> Vec<String> {
        self.requests.lock().unwrap()[index]
            .tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect()
    }

    fn last_request_tool_names(&self) -> Vec<String> {
        let requests = self.requests.lock().unwrap();
        requests
            .last()
            .unwrap()
            .tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect()
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().unwrap().push(request);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::Parse("scripted provider exhausted".into()))
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

struct NamedTool(&'static str);

#[async_trait]
impl Tool for NamedTool {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "test tool"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok("ok"))
    }
}

#[agent(kind = subagent, name = "sub-a", description = "sub-a subagent")]
struct SubA(Arc<ScriptedProvider>);

impl SubA {
    async fn agent(&self, _llm: Llm) -> anyhow::Result<Agent> {
        Ok(Agent::new(
            Llm::new(self.0.clone(), "child-model")
                .instructions("you are sub-a")
                .max_turns(3)
                .tool(NamedTool("only-a")),
        ))
    }
}

#[agent(kind = subagent, name = "sub-b", description = "sub-b subagent")]
struct SubB(Arc<ScriptedProvider>);

impl SubB {
    async fn agent(&self, _llm: Llm) -> anyhow::Result<Agent> {
        Ok(Agent::new(
            Llm::new(self.0.clone(), "child-model")
                .instructions("you are sub-b")
                .max_turns(3)
                .tool(NamedTool("only-b")),
        ))
    }
}

#[tokio::test]
async fn each_subagent_only_sees_its_own_tools() {
    let child_a = ScriptedProvider::new(vec![text("child-a done")]);
    let child_b = ScriptedProvider::new(vec![text("child-b done")]);

    let main_provider = ScriptedProvider::new(vec![
        call(
            "c1",
            "delegate",
            serde_json::json!({ "agent": "sub-a", "prompt": "go" }),
        ),
        call(
            "c2",
            "delegate",
            serde_json::json!({ "agent": "sub-b", "prompt": "go" }),
        ),
        text("done"),
    ]);

    let mut agent = Agent::new(Llm::new(main_provider.clone(), "main-model"))
        .with(ability("main-kit").tool(NamedTool("main-tool")))
        .with(SubA(child_a.clone()))
        .with(SubB(child_b.clone()))
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("start").await.unwrap();

    let main_tools = main_provider.request_tool_names(0);
    assert!(main_tools.iter().any(|name| name == "main-tool"));
    assert!(!main_tools.iter().any(|name| name == "only-a"));
    assert!(!main_tools.iter().any(|name| name == "only-b"));

    assert_eq!(child_a.last_request_tool_names(), vec!["only-a"]);
    assert_eq!(child_b.last_request_tool_names(), vec!["only-b"]);
}
