use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::FoundrySubagentBuilder;
use runic_agent::{Agent, AgentError};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_skills::SkillSet;
use runic_subagent::{AgentDef, DelegationCtx, SubagentBuilder, SubagentReq, assemble_subagent};
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
        tenant: "alice".into(),
        session: "s1".into(),
    }
}

fn builder(provider: Arc<dyn Provider>) -> FoundrySubagentBuilder {
    FoundrySubagentBuilder {
        provider,
        model: "parent-model".into(),
        skills: None,
    }
}

async fn assemble(b: &dyn SubagentBuilder, def: &AgentDef) -> Agent {
    assemble_subagent(b, &SubagentReq { def, dctx: &ctx() }).await
}

#[tokio::test]
async fn children_inherit_the_parent_tenant() {
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let agent = assemble(
        &builder(provider),
        &def("---\nname: worker\ndescription: d\n---\nbody"),
    )
    .await;

    assert_eq!(agent.state().user_id, "alice");
    assert_eq!(agent.state().session_id, "s1:worker");
}

async fn crm_catalog() -> Arc<SkillSet> {
    let dir = tempfile::tempdir().unwrap();
    for (entry, desc) in [("pipeline", "crm pipeline"), ("followup", "crm followup")] {
        let skill = dir.path().join(entry);
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: {entry}\ndescription: {desc}\n---\nBody for {entry}."),
        )
        .unwrap();
    }
    Arc::new(SkillSet::load_dir("crm", dir.path()).await)
}

fn builder_with_skills(
    provider: Arc<dyn Provider>,
    skills: Arc<SkillSet>,
) -> FoundrySubagentBuilder {
    FoundrySubagentBuilder {
        provider,
        model: "parent-model".into(),
        skills: Some(skills),
    }
}

#[tokio::test]
async fn child_uses_def_prompt_and_model_override() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("done")]));
    let def = def(
        "---\nname: reviewer\ndescription: reviews\nmodel: child-model\n---\nChild instructions.",
    );
    let mut agent = assemble(&builder(provider.clone()), &def).await;

    agent.run("go").await.unwrap();

    let request = provider.last_request();
    assert_eq!(request.model, "child-model");
    assert_eq!(request.system.as_deref(), Some("Child instructions."));
}

#[tokio::test]
async fn child_falls_back_to_parent_model() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("done")]));
    let def = def("---\nname: reviewer\ndescription: reviews\n---\nChild instructions.");
    let mut agent = assemble(&builder(provider.clone()), &def).await;

    agent.run("go").await.unwrap();

    assert_eq!(provider.last_request().model, "parent-model");
}

#[tokio::test]
async fn child_without_allowed_tools_gets_no_tools_at_all() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("done")]));
    let def = def("---\nname: reviewer\ndescription: reviews\n---\nChild instructions.");
    let mut agent = assemble(&builder(provider.clone()), &def).await;

    agent.run("go").await.unwrap();

    assert!(provider.last_request().tools.is_empty());
}

#[tokio::test]
async fn child_wildcard_gets_the_base_pool_but_never_privileged_tools() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("done")]));
    let def =
        def("---\nname: reviewer\ndescription: reviews\ntools: [\"*\"]\n---\nChild instructions.");
    let mut agent = assemble(&builder(provider.clone()), &def).await;

    agent.run("go").await.unwrap();

    let names: Vec<String> = provider
        .last_request()
        .tools
        .into_iter()
        .map(|tool| tool.name)
        .collect();

    for expected in ["calculator", "system_time"] {
        assert!(
            names.iter().any(|name| name == expected),
            "missing {expected}"
        );
    }
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
    let mut agent = assemble(&builder(provider.clone()), &def).await;

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
async fn child_gets_only_the_skills_its_def_lists() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("done")]));
    let catalog = crm_catalog().await;
    let def =
        def("---\nname: crm\ndescription: crm\nskills: [crm:pipeline]\n---\nChild instructions.");
    let mut agent = assemble(&builder_with_skills(provider.clone(), catalog), &def).await;

    agent.run("go").await.unwrap();

    let request = provider.last_request();
    let system = request.system.as_deref().unwrap();
    assert!(system.contains("<available-skills>"));
    assert!(system.contains("crm:pipeline"));
    assert!(!system.contains("crm:followup"));

    let names: Vec<&str> = request.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["skill_view"]);
}

#[tokio::test]
async fn child_wildcard_gets_the_whole_catalog() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("done")]));
    let catalog = crm_catalog().await;
    let def = def("---\nname: crm\ndescription: crm\nskills: [\"*\"]\n---\nChild instructions.");
    let mut agent = assemble(&builder_with_skills(provider.clone(), catalog), &def).await;

    agent.run("go").await.unwrap();

    let system = provider.last_request().system.unwrap();
    assert!(system.contains("crm:pipeline"));
    assert!(system.contains("crm:followup"));
}

#[tokio::test]
async fn child_without_listed_skills_gets_none_even_with_a_catalog() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("done")]));
    let catalog = crm_catalog().await;
    let def = def("---\nname: crm\ndescription: crm\n---\nChild instructions.");
    let mut agent = assemble(&builder_with_skills(provider.clone(), catalog), &def).await;

    agent.run("go").await.unwrap();

    let request = provider.last_request();
    assert!(
        !request
            .system
            .as_deref()
            .unwrap()
            .contains("<available-skills>")
    );
    assert!(!request.tools.iter().any(|t| t.name == "skill_view"));
}

#[tokio::test]
async fn child_with_skills_listed_but_no_catalog_gets_none() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("done")]));
    let def = def("---\nname: crm\ndescription: crm\nskills: [crm:pipeline]\n---\nChild.");
    let mut agent = assemble(&builder(provider.clone()), &def).await;

    agent.run("go").await.unwrap();

    let request = provider.last_request();
    assert!(
        !request
            .system
            .as_deref()
            .unwrap()
            .contains("<available-skills>")
    );
    assert!(!request.tools.iter().any(|t| t.name == "skill_view"));
}

#[tokio::test]
async fn child_listing_unknown_skills_gets_no_empty_section_or_tool() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("done")]));
    let catalog = crm_catalog().await;
    let def = def("---\nname: crm\ndescription: crm\nskills: [ghost:*]\n---\nChild.");
    let mut agent = assemble(&builder_with_skills(provider.clone(), catalog), &def).await;

    agent.run("go").await.unwrap();

    let request = provider.last_request();
    assert!(
        !request
            .system
            .as_deref()
            .unwrap()
            .contains("<available-skills>")
    );
    assert!(!request.tools.iter().any(|t| t.name == "skill_view"));
}

#[tokio::test]
async fn skills_are_independent_of_the_tool_allow_list() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("done")]));
    let catalog = crm_catalog().await;
    let def = def(
        "---\nname: crm\ndescription: crm\nallowed-tools: [calculator]\nskills: [crm:pipeline]\n---\nChild.",
    );
    let mut agent = assemble(&builder_with_skills(provider.clone(), catalog), &def).await;

    agent.run("go").await.unwrap();

    let mut names: Vec<String> = provider
        .last_request()
        .tools
        .into_iter()
        .map(|t| t.name)
        .collect();
    names.sort();
    assert_eq!(names, vec!["calculator", "skill_view"]);
}

#[tokio::test]
async fn child_respects_max_turns() {
    let provider = Arc::new(ScriptedProvider::new(vec![tool_use_response("calculator")]));
    let def = def(
        "---\nname: reviewer\ndescription: reviews\nmax-turns: 1\ntools: [calculator]\n---\nChild instructions.",
    );
    let mut agent = assemble(&builder(provider), &def).await;

    let err = agent.run("go").await.unwrap_err();

    assert!(
        matches!(err, AgentError::MaxTurnsExceeded(1)),
        "got {err:?}"
    );
}
