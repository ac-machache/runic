use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic_agent::AgentError;
use runic_foundry::FoundrySubagentBuilder;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_subagent::{AgentDef, DelegationCtx, SubagentBuilder};
use runic_types::{ContentBlock, StopReason, TokenUsage, ToolCall};

struct ScriptedProvider {
    responses: Mutex<VecDeque<CompletionResponse>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<CompletionResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        }
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

fn text_response(text: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::Text {
            text: text.into(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::EndTurn,
        tool_calls: vec![],
        usage: TokenUsage::default(),
    }
}

fn tool_use_response(name: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: name.into(),
            input: serde_json::json!({ "expression": "1 + 1" }),
            provider_metadata: None,
        }],
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: "t1".into(),
            name: name.into(),
            input: serde_json::json!({ "expression": "1 + 1" }),
        }],
        usage: TokenUsage::default(),
    }
}

fn def(src: &str) -> AgentDef {
    AgentDef::parse_markdown(src).unwrap()
}

fn ctx() -> DelegationCtx {
    DelegationCtx {
        depth: 1,
        max_depth: 3,
        cancel: runic_agent::CancelToken::new(),
        config: serde_json::Map::new(),
    }
}

fn builder(provider: Arc<dyn Provider>) -> FoundrySubagentBuilder {
    FoundrySubagentBuilder {
        provider,
        model: "parent-model".into(),
    }
}

#[tokio::test]
async fn child_uses_def_prompt_and_model_override() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("done")]));
    let def = def(
        "---\nname: reviewer\ndescription: reviews\nmodel: child-model\n---\nChild instructions.",
    );
    let mut agent = builder(provider.clone()).build(&def, &ctx()).await.unwrap();

    agent.run("go").await.unwrap();

    let request = provider.last_request();
    assert_eq!(request.model, "child-model");
    assert_eq!(request.system.as_deref(), Some("Child instructions."));
}

#[tokio::test]
async fn child_falls_back_to_parent_model() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("done")]));
    let def = def("---\nname: reviewer\ndescription: reviews\n---\nChild instructions.");
    let mut agent = builder(provider.clone()).build(&def, &ctx()).await.unwrap();

    agent.run("go").await.unwrap();

    assert_eq!(provider.last_request().model, "parent-model");
}

#[tokio::test]
async fn child_gets_base_tools_only() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("done")]));
    let def = def("---\nname: reviewer\ndescription: reviews\n---\nChild instructions.");
    let mut agent = builder(provider.clone()).build(&def, &ctx()).await.unwrap();

    agent.run("go").await.unwrap();

    let mut names: Vec<String> = provider
        .last_request()
        .tools
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    names.sort();

    // the fs-free base only
    for expected in ["calculator", "system_time"] {
        assert!(
            names.iter().any(|name| name == expected),
            "missing {expected}"
        );
    }

    // file tools are gone; higher-tier tools never escalate to a child
    for forbidden in [
        "read_file",
        "write_file",
        "edit_file",
        "ls",
        "glob",
        "grep",
        "apply_patch",
        "memory",
        "delegate",
        "tool_search",
        "search_chats",
        "skill_view",
    ] {
        assert!(
            !names.iter().any(|name| name == forbidden),
            "unexpected {forbidden}"
        );
    }
}

#[tokio::test]
async fn child_allowed_tools_narrows_even_base_tools() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("done")]));
    let def = def(
        "---\nname: reviewer\ndescription: reviews\nallowed-tools: [calculator]\n---\nChild instructions.",
    );
    let mut agent = builder(provider.clone()).build(&def, &ctx()).await.unwrap();

    agent.run("go").await.unwrap();

    let names: Vec<String> = provider
        .last_request()
        .tools
        .into_iter()
        .map(|tool| tool.name)
        .collect();

    assert_eq!(names, vec!["calculator"]);
}

#[tokio::test]
async fn child_respects_max_turns() {
    let provider = Arc::new(ScriptedProvider::new(vec![tool_use_response("calculator")]));
    let def =
        def("---\nname: reviewer\ndescription: reviews\nmax-turns: 1\n---\nChild instructions.");
    let mut agent = builder(provider).build(&def, &ctx()).await.unwrap();

    let err = agent.run("go").await.unwrap_err();

    assert!(
        matches!(err, AgentError::MaxTurnsExceeded(1)),
        "got {err:?}"
    );
}
