use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::ability::{Ability, AbilityBundle, BuildCtx};
use runic::composer::Composer;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_subagent::{Subagent, SubagentBuilder, SubagentReq};
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

fn subagent_def(name: &str) -> Subagent {
    Subagent::new(name, format!("{name} subagent"))
        .allowed_tools(["*"])
        .max_turns(3)
        .prompt(format!("you are {name}"))
}

struct SubagentOwner {
    def: Subagent,
    tool_name: &'static str,
    child_provider: Arc<ScriptedProvider>,
}

#[async_trait]
impl SubagentBuilder for SubagentOwner {
    async fn provider(&self, _req: &SubagentReq<'_>) -> Arc<dyn Provider> {
        self.child_provider.clone()
    }
    fn default_model(&self, _req: &SubagentReq<'_>) -> String {
        "child-model".into()
    }
    async fn tool_pool(&self, _req: &SubagentReq<'_>) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(NamedTool(self.tool_name))]
    }
}

struct OwnedSubagentAbility(Arc<SubagentOwner>);

#[async_trait]
impl Ability for OwnedSubagentAbility {
    async fn contribute(
        &self,
        bundle: &mut AbilityBundle,
        _ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        bundle.subagent_with(self.0.def.clone(), self.0.clone());
        Ok(())
    }
}

#[tokio::test]
async fn each_ability_owned_subagent_only_sees_its_own_tools() {
    let child_a = ScriptedProvider::new(vec![text("child-a done")]);
    let child_b = ScriptedProvider::new(vec![text("child-b done")]);

    let owner_a = Arc::new(SubagentOwner {
        def: subagent_def("sub-a"),
        tool_name: "only-a",
        child_provider: child_a.clone(),
    });
    let owner_b = Arc::new(SubagentOwner {
        def: subagent_def("sub-b"),
        tool_name: "only-b",
        child_provider: child_b.clone(),
    });

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

    let mut agent = Composer::new(main_provider.clone(), "main-model")
        .with(runic::ability::Tools(vec![Arc::new(NamedTool(
            "main-tool",
        ))]))
        .with(OwnedSubagentAbility(owner_a))
        .with(OwnedSubagentAbility(owner_b))
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("start").await.unwrap();

    let main_tools = main_provider.request_tool_names(0);
    assert!(main_tools.iter().any(|name| name == "main-tool"));
    assert!(!main_tools.iter().any(|name| name == "only-a"));
    assert!(!main_tools.iter().any(|name| name == "only-b"));

    let child_a_tools = child_a.last_request_tool_names();
    assert_eq!(child_a_tools, vec!["only-a"]);

    let child_b_tools = child_b.last_request_tool_names();
    assert_eq!(child_b_tools, vec!["only-b"]);
}
