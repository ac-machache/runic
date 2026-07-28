use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::ability::ability;
use runic::composer::Agent;
use runic::subagent::Invocation;
use runic::{AgentDef, Llm, agent, subagent};
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

fn delegate_to(name: &str) -> CompletionResponse {
    let input = serde_json::json!({ "agent": name, "prompt": "dig" });
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

struct SearchTool;

#[async_trait]
impl Tool for SearchTool {
    fn name(&self) -> &str {
        "search"
    }
    fn description(&self) -> &str {
        "searches"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok("found"))
    }
}

#[subagent(
    name = "researcher",
    description = "digs through docs",
    model = "ministral-3b-latest",
    invocation = background
)]
struct Researcher {
    provider: Arc<ScriptedProvider>,
}

impl Researcher {
    async fn agent(&self, _llm: Llm) -> anyhow::Result<Agent> {
        Ok(
            Agent::new(Llm::new(self.provider.clone(), "ministral-3b-latest").instructions("dig"))
                .with(ability("search").tool(SearchTool)),
        )
    }
}

#[subagent(name = "inheritor", description = "uses the parent model")]
struct Inheritor;

impl Inheritor {
    async fn agent(&self, llm: Llm) -> anyhow::Result<Agent> {
        Ok(Agent::new(llm.instructions("inherit")))
    }
}

#[agent(name = "root", description = "the agent a host serves")]
struct Root(Arc<ScriptedProvider>);

impl Root {
    async fn agent(&self) -> anyhow::Result<Agent> {
        Ok(Agent::new(
            Llm::new(self.0.clone(), "main-model").instructions("You are root."),
        ))
    }
}

#[tokio::test]
async fn agent_names_and_describes_itself_for_a_host() {
    let provider = ScriptedProvider::new(vec![text("root done")]);
    let root = Root(provider);

    assert_eq!(AgentDef::name(&root), "root");
    assert_eq!(root.description(), Some("the agent a host serves"));

    let mut runner = root
        .build_agent()
        .await
        .unwrap()
        .build("alice", "s1")
        .await
        .unwrap();

    runner.run("go").await.unwrap();
    assert_eq!(
        runner.state().last_assistant_text().as_deref(),
        Some("root done")
    );
}

#[tokio::test]
async fn a_host_can_hold_an_agent_def_as_a_trait_object() {
    let provider = ScriptedProvider::new(vec![text("root done")]);
    let defs: Vec<Arc<dyn AgentDef>> = vec![Arc::new(Root(provider))];

    let by_name: Vec<&str> = defs.iter().map(|def| def.name()).collect();
    assert_eq!(by_name, vec!["root"]);

    let mut runner = defs[0]
        .build_agent()
        .await
        .unwrap()
        .build("alice", "s1")
        .await
        .unwrap();
    runner.run("go").await.unwrap();
    assert_eq!(
        runner.state().last_assistant_text().as_deref(),
        Some("root done")
    );
}

#[tokio::test]
async fn kind_subagent_registers_in_the_roster_and_enforces_its_invocation() {
    let child = ScriptedProvider::new(vec![text("child found it")]);
    let parent = ScriptedProvider::new(vec![delegate_to("researcher"), text("parent done")]);

    let mut runner = Agent::new(Llm::new(parent, "main-model"))
        .with(Researcher {
            provider: child.clone(),
        })
        .build("alice", "s1")
        .await
        .unwrap();

    let system = runner.state().system_prompt.clone();
    assert!(
        system.contains("- researcher: digs through docs (runs in the background"),
        "the roster advertises the constraint: {system}"
    );

    runner.run("go").await.unwrap();

    let results: Vec<String> = runner
        .state()
        .messages_for_provider()
        .iter()
        .filter_map(|msg| match &msg.content {
            runic_types::MessageContent::Blocks(blocks) => Some(blocks),
            _ => None,
        })
        .flatten()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.text()),
            _ => None,
        })
        .collect();

    let delegation = results
        .first()
        .expect("the delegate call produced a result");
    assert!(
        delegation.contains("task_id="),
        "the model omitted `background`, but invocation=background forced it detached: {delegation}"
    );
    assert!(
        delegation.contains("invocation=background"),
        "and the model is told why: {delegation}"
    );
}

#[tokio::test]
async fn a_subagent_without_a_model_attribute_inherits_the_parents() {
    let parent = ScriptedProvider::new(vec![
        delegate_to("inheritor"),
        text("child reply"),
        text("parent done"),
    ]);

    let mut runner = Agent::new(Llm::new(parent.clone(), "parent-model"))
        .with(Inheritor)
        .build("alice", "s1")
        .await
        .unwrap();

    runner.run("go").await.unwrap();

    let models: Vec<String> = parent
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.model.clone())
        .collect();
    assert_eq!(
        models,
        vec!["parent-model", "parent-model", "parent-model"],
        "with no model attribute the child inherits the parent provider and model"
    );
}

#[test]
fn invocation_resolve_lets_the_declaration_win() {
    assert!(Invocation::Background.resolve(false));
    assert!(!Invocation::Sync.resolve(true));
    assert!(Invocation::Any.resolve(true));
    assert!(!Invocation::Any.resolve(false));
}
