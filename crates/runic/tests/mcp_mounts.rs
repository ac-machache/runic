use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::Llm;
use runic::ability::subagent;
use runic::composer::Agent;
use runic::mcp::{self, McpClient, McpConnection, McpError, Transport};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_types::{ContentBlock, StopReason, TokenUsage, ToolCall};

#[derive(Debug)]
struct FakeTransport;

#[async_trait]
impl Transport for FakeTransport {
    fn server_name(&self) -> &str {
        "crm"
    }

    async fn request(
        &self,
        method: &str,
        _params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, McpError> {
        match method {
            "initialize" => Ok(serde_json::json!({
                "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "crm", "version": "1.0.0" },
            })),
            "tools/list" => Ok(serde_json::json!({
                "tools": [
                    { "name": "search", "description": "search crm", "inputSchema": { "type": "object" } },
                    { "name": "update", "description": "update crm", "inputSchema": { "type": "object" } },
                ]
            })),
            other => Err(McpError::protocol(format!("unexpected request: {other}"))),
        }
    }

    async fn notify(
        &self,
        _method: &str,
        _params: Option<serde_json::Value>,
    ) -> Result<(), McpError> {
        Ok(())
    }

    async fn close(&self) {}
}

async fn fake_connection() -> McpConnection {
    let client = McpClient::handshake(Arc::new(FakeTransport)).await.unwrap();
    McpConnection::from_clients(vec![client])
}

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

    fn last_request(&self) -> CompletionRequest {
        self.requests.lock().unwrap().last().unwrap().clone()
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

fn delegate_to(agent: &str) -> CompletionResponse {
    let input = serde_json::json!({ "agent": agent, "prompt": "go" });
    CompletionResponse {
        content: vec![ContentBlock::ToolUse {
            id: "c1".into(),
            name: "delegate".into(),
            input: input.clone(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "delegate".into(),
            input,
        }],
        usage: TokenUsage::default(),
    }
}

#[tokio::test]
async fn deferred_mount_gives_the_parent_search_not_tools() {
    let provider = ScriptedProvider::new(vec![text("done")]);
    let mut agent = Agent::new(Llm::new(provider.clone(), "main-model"))
        .with(mcp::deferred(fake_connection().await))
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("start").await.unwrap();

    let request = provider.last_request();
    let tools: Vec<String> = request.tools.iter().map(|tool| tool.name.clone()).collect();
    assert!(tools.iter().any(|name| name == "tool_search"));
    assert!(!tools.iter().any(|name| name == "mcp__crm__search"));
    assert!(
        request
            .system
            .unwrap_or_default()
            .contains("mcp__crm__search")
    );
}

#[tokio::test]
async fn direct_mount_gives_a_subagent_the_real_tools() {
    let child = ScriptedProvider::new(vec![text("child done")]);
    let main_provider = ScriptedProvider::new(vec![delegate_to("crm-expert"), text("done")]);

    let mut agent = Agent::new(Llm::new(main_provider.clone(), "main-model"))
        .with(
            subagent("crm-expert", "digs crm")
                .prompt("dig")
                .provider(child.clone())
                .with(mcp::direct(fake_connection().await)),
        )
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("start").await.unwrap();

    let mut child_tools: Vec<String> = child
        .last_request()
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    child_tools.sort();
    assert_eq!(child_tools, vec!["mcp__crm__search", "mcp__crm__update"]);

    let main_tools: Vec<String> = main_provider.requests.lock().unwrap()[0]
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    assert!(!main_tools.iter().any(|name| name.starts_with("mcp__")));
}
