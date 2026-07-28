use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::ability::ability;
use runic::composer::Agent;
use runic::subagent::Subagent;
use runic::{Llm, agent};
use runic_agent::RunContext;
use runic_hook::{HookLifecycle, HookOutcome, WriteHook};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_state::AgentState;
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

    fn last_request(&self) -> CompletionRequest {
        self.requests.lock().unwrap().last().unwrap().clone()
    }

    fn request_tool_names(&self, index: usize) -> Vec<String> {
        self.requests.lock().unwrap()[index]
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

fn delegate_to(agent: &str) -> CompletionResponse {
    call(
        "c1",
        "delegate",
        serde_json::json!({ "agent": agent, "prompt": "go" }),
    )
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

struct RecordingTool {
    name: &'static str,
    seen: Arc<Mutex<Option<serde_json::Value>>>,
}

#[async_trait]
impl Tool for RecordingTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "records its args"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        *self.seen.lock().unwrap() = Some(args);
        Ok(ToolResult::ok("recorded"))
    }
}

#[agent(kind = subagent, name = "sub-a", description = "a expert")]
struct SubA(Arc<ScriptedProvider>);

impl SubA {
    async fn agent(&self, llm: Llm) -> anyhow::Result<Agent> {
        Ok(Agent::new(
            Llm::new(self.0.clone(), llm.config().model.clone())
                .instructions("you are a")
                .tool(NamedTool("only-a")),
        ))
    }
}

#[agent(kind = subagent, name = "sub-b", description = "b expert")]
struct SubB(Arc<ScriptedProvider>);

impl SubB {
    async fn agent(&self, _llm: Llm) -> anyhow::Result<Agent> {
        Ok(Agent::new(
            Llm::new(self.0.clone(), "custom-child")
                .instructions("you are b")
                .tool(NamedTool("only-b")),
        ))
    }
}

#[tokio::test]
async fn subagent_tools_are_isolated_and_models_resolve() {
    let child_a = ScriptedProvider::new(vec![text("a done")]);
    let child_b = ScriptedProvider::new(vec![text("b done")]);
    let main_provider = ScriptedProvider::new(vec![
        delegate_to("sub-a"),
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

    let request_a = child_a.last_request();
    assert_eq!(
        request_a
            .tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>(),
        vec!["only-a"]
    );
    assert_eq!(request_a.model, "main-model");

    let request_b = child_b.last_request();
    assert_eq!(request_b.model, "custom-child");
}

struct InjectUserId;

#[async_trait]
impl WriteHook for InjectUserId {
    fn name(&self) -> &str {
        "inject-user-id"
    }

    fn points(&self) -> &'static [HookLifecycle] {
        &[HookLifecycle::BeforeTool]
    }

    async fn before_tool(&self, state: &mut AgentState, call: &mut ToolCall) -> HookOutcome {
        if !call.name.starts_with("mcp__") {
            return HookOutcome::Noop;
        }
        let Some(value) = state.config.get("user_id").cloned() else {
            return HookOutcome::Cancel("user_id is not set for this run".into());
        };
        if let Some(input) = call.input.as_object_mut() {
            input.insert("user_id".into(), value);
        }
        HookOutcome::Continue
    }
}

#[agent(kind = subagent, name = "crm-expert", description = "crm digger")]
struct CrmExpert {
    provider: Arc<ScriptedProvider>,
    seen: Arc<Mutex<Option<serde_json::Value>>>,
}

impl CrmExpert {
    async fn agent(&self, _llm: Llm) -> anyhow::Result<Agent> {
        Ok(
            Agent::new(Llm::new(self.provider.clone(), "child-model").instructions("dig")).with(
                ability("crm")
                    .tool(RecordingTool {
                        name: "mcp__crm__lookup",
                        seen: self.seen.clone(),
                    })
                    .hook(InjectUserId),
            ),
        )
    }
}

#[tokio::test]
async fn a_consumer_hook_on_the_subagent_reaches_the_childs_tool_calls() {
    let seen = Arc::new(Mutex::new(None));
    let child = ScriptedProvider::new(vec![
        call(
            "t1",
            "mcp__crm__lookup",
            serde_json::json!({ "query": "dupont" }),
        ),
        text("child done"),
    ]);
    let main_provider = ScriptedProvider::new(vec![delegate_to("crm-expert"), text("done")]);

    let mut agent = Agent::new(Llm::new(main_provider, "main-model"))
        .with(CrmExpert {
            provider: child.clone(),
            seen: seen.clone(),
        })
        .build("alice", "s1")
        .await
        .unwrap();

    let ctx = RunContext::new().config_value("user_id", serde_json::json!("u-42"));
    agent.run_with("start", ctx).await.unwrap();

    let args = seen.lock().unwrap().clone().unwrap();
    assert_eq!(args["user_id"], serde_json::json!("u-42"));
    assert_eq!(args["query"], serde_json::json!("dupont"));
}

#[tokio::test]
async fn a_consumer_hook_can_block_the_childs_tool_calls() {
    let seen = Arc::new(Mutex::new(None));
    let child = ScriptedProvider::new(vec![
        call(
            "t1",
            "mcp__crm__lookup",
            serde_json::json!({ "query": "dupont" }),
        ),
        text("child done"),
    ]);
    let main_provider = ScriptedProvider::new(vec![delegate_to("crm-expert"), text("done")]);

    let mut agent = Agent::new(Llm::new(main_provider, "main-model"))
        .with(CrmExpert {
            provider: child.clone(),
            seen: seen.clone(),
        })
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("start").await.unwrap();

    assert!(seen.lock().unwrap().is_none());
}

struct CtxProbe(Arc<Mutex<Option<String>>>);

#[async_trait]
impl runic::ability::Ability for CtxProbe {
    async fn contribute(
        &self,
        _bundle: &mut runic::ability::AbilityBundle,
        ctx: &runic::ability::BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        *self.0.lock().unwrap() = Some(ctx.model.to_string());
        Ok(())
    }
}

#[agent(kind = subagent, name = "expert", description = "digs")]
struct Expert {
    provider: Arc<ScriptedProvider>,
    probe: Arc<Mutex<Option<String>>>,
}

impl Expert {
    async fn agent(&self, _llm: Llm) -> anyhow::Result<Agent> {
        Ok(
            Agent::new(Llm::new(self.provider.clone(), "child-override").instructions("dig"))
                .with(CtxProbe(self.probe.clone()))
                .with(ability("kit").tool(NamedTool("t"))),
        )
    }
}

#[tokio::test]
async fn child_abilities_see_the_child_model_not_the_parents() {
    let seen_model = Arc::new(Mutex::new(None));
    let child = ScriptedProvider::new(vec![text("child done")]);
    let provider = ScriptedProvider::new(vec![delegate_to("expert"), text("done")]);

    let mut agent = Agent::new(Llm::new(provider, "main-model"))
        .with(Expert {
            provider: child,
            probe: seen_model.clone(),
        })
        .build("alice", "s1")
        .await
        .unwrap();

    assert_eq!(
        *seen_model.lock().unwrap(),
        None,
        "a subagent's abilities compose when the child runs, not at parent build"
    );

    agent.run("start").await.unwrap();

    assert_eq!(
        seen_model.lock().unwrap().as_deref(),
        Some("child-override")
    );
}

#[tokio::test]
async fn delegation_edges_land_in_the_parent_log_with_child_usage() {
    let mut child_reply = text("child done");
    child_reply.usage = TokenUsage {
        input_tokens: 5,
        output_tokens: 7,
        ..Default::default()
    };
    let child = ScriptedProvider::new(vec![child_reply]);
    let main_provider = ScriptedProvider::new(vec![delegate_to("sub-a"), text("done")]);

    let mut agent = Agent::new(Llm::new(main_provider, "main-model"))
        .with(SubA(child.clone()))
        .build("alice", "s1")
        .await
        .unwrap();

    let (cap_tx, mut cap_rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .state_mut()
        .set_emitter(Some(Arc::new(runic_agent::ChannelEmitter(cap_tx))));

    let outcome = agent.run("start").await.unwrap();

    let mut events: Vec<runic_state::AgentEvent> = Vec::new();
    while let Ok(ev) = cap_rx.try_recv() {
        events.push(ev);
    }
    let started = events
        .iter()
        .find_map(|e| match e {
            runic_state::AgentEvent::DelegationStarted {
                agent, mode, turn, ..
            } => Some((agent.clone(), *mode, *turn)),
            _ => None,
        })
        .expect("delegation start edge");
    assert_eq!(started.0, "sub-a");
    assert_eq!(started.1, runic_state::DelegationMode::Sync);
    assert_eq!(started.2, 1);

    let finished = events
        .iter()
        .find_map(|e| match e {
            runic_state::AgentEvent::DelegationFinished {
                agent,
                status,
                usage,
                model,
                ..
            } => Some((agent.clone(), status.clone(), *usage, model.clone())),
            _ => None,
        })
        .expect("delegation finish edge");
    assert_eq!(finished.0, "sub-a");
    assert_eq!(finished.1, runic_state::DelegationStatus::Ok);
    assert_eq!(finished.2.input_tokens, 5);
    assert_eq!(finished.2.output_tokens, 7);
    assert_eq!(finished.3.as_deref(), Some("main-model"));

    assert_eq!(
        outcome.usage.input_tokens, 0,
        "parent tokens never include child tokens"
    );
}

#[tokio::test]
async fn parallel_delegation_emits_an_edge_per_child() {
    let child_a = ScriptedProvider::new(vec![text("a done")]);
    let child_b = ScriptedProvider::new(vec![text("b done")]);
    let main_provider = ScriptedProvider::new(vec![
        call(
            "c1",
            "delegate",
            serde_json::json!({ "parallel": ["sub-a", "sub-b"], "prompt": "go" }),
        ),
        text("done"),
    ]);

    let mut agent = Agent::new(Llm::new(main_provider, "main-model"))
        .with(SubA(child_a.clone()))
        .with(SubB(child_b.clone()))
        .build("alice", "s1")
        .await
        .unwrap();

    let (cap_tx, mut cap_rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .state_mut()
        .set_emitter(Some(Arc::new(runic_agent::ChannelEmitter(cap_tx))));

    agent.run("start").await.unwrap();

    let mut events: Vec<runic_state::AgentEvent> = Vec::new();
    while let Ok(ev) = cap_rx.try_recv() {
        events.push(ev);
    }
    let mut started: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            runic_state::AgentEvent::DelegationStarted {
                agent,
                mode,
                call_id,
                turn,
                ..
            } => Some((agent.clone(), *mode, call_id.clone(), *turn)),
            _ => None,
        })
        .collect();
    started.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        started,
        vec![
            (
                "sub-a".to_string(),
                runic_state::DelegationMode::Parallel,
                "c1".to_string(),
                1
            ),
            (
                "sub-b".to_string(),
                runic_state::DelegationMode::Parallel,
                "c1".to_string(),
                1
            ),
        ]
    );

    let mut finished: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            runic_state::AgentEvent::DelegationFinished { agent, status, .. } => {
                Some((agent.clone(), status.clone()))
            }
            _ => None,
        })
        .collect();
    finished.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        finished,
        vec![
            ("sub-a".to_string(), runic_state::DelegationStatus::Ok),
            ("sub-b".to_string(), runic_state::DelegationStatus::Ok),
        ]
    );
}

#[agent(kind = subagent, name = "outer", description = "outer")]
struct Outer(Arc<ScriptedProvider>);

impl Outer {
    async fn agent(&self, _llm: Llm) -> anyhow::Result<Agent> {
        let inner = Subagent::new(
            "inner",
            "inner expert",
            Agent::new(Llm::new(ScriptedProvider::new(vec![]), "inner-model")),
        );
        Ok(
            Agent::new(Llm::new(self.0.clone(), "outer-model").instructions("outer"))
                .with(ability("inner-owner").subagent(inner))
                .with(ability("gated").describe("gated stuff").deferred()),
        )
    }
}

#[tokio::test]
async fn a_subagent_composes_its_own_subagents_and_deferred_abilities() {
    let child = ScriptedProvider::new(vec![text("outer done")]);
    let main_provider = ScriptedProvider::new(vec![delegate_to("outer"), text("done")]);

    let mut agent = Agent::new(Llm::new(main_provider, "main-model"))
        .with(Outer(child.clone()))
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("start").await.unwrap();

    let request = child.last_request();
    let system = request.system.clone().unwrap_or_default();
    assert!(
        system.contains("- inner: inner expert"),
        "the child runs its own delegate roster: {system}"
    );
    assert!(
        system.contains("gated stuff"),
        "and its own deferred catalog: {system}"
    );

    let tools: Vec<String> = request.tools.iter().map(|tool| tool.name.clone()).collect();
    assert!(tools.iter().any(|name| name == "delegate"), "{tools:?}");
    assert!(tools.iter().any(|name| name == "load_ability"), "{tools:?}");
}

#[agent(kind = subagent, name = "writer", description = "writes")]
struct Writer(Arc<ScriptedProvider>);

impl Writer {
    async fn agent(&self, _llm: Llm) -> anyhow::Result<Agent> {
        Ok(Agent::new(
            Llm::new(self.0.clone(), "child-model")
                .instructions("you are the writer")
                .tool(NamedTool("pen")),
        )
        .with(ability("style").prompt("EXTRA-SECTION")))
    }
}

#[tokio::test]
async fn ability_prompts_reach_the_child_system_prompt() {
    let child = ScriptedProvider::new(vec![text("done")]);
    let main_provider = ScriptedProvider::new(vec![delegate_to("writer"), text("done")]);

    let mut agent = Agent::new(Llm::new(main_provider, "main-model"))
        .with(Writer(child.clone()))
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("start").await.unwrap();

    let system = child.last_request().system.unwrap_or_default();
    assert!(system.contains("you are the writer"));
    assert!(system.contains("EXTRA-SECTION"));
}

#[agent(kind = subagent, name = "analyst", description = "analyzes")]
struct Analyst(Arc<ScriptedProvider>);

impl Analyst {
    async fn agent(&self, _llm: Llm) -> anyhow::Result<Agent> {
        Ok(Agent::new(
            Llm::new(self.0.clone(), "child-model")
                .instructions("analyze")
                .tool(NamedTool("cube")),
        ))
    }
}

#[tokio::test]
async fn a_grouped_ability_can_own_a_subagent() {
    let child = ScriptedProvider::new(vec![text("done")]);
    let main_provider = ScriptedProvider::new(vec![delegate_to("analyst"), text("done")]);

    let mut agent = Agent::new(Llm::new(main_provider.clone(), "main-model"))
        .with(
            ability("commerce")
                .describe("commerce pack")
                .with(Analyst(child.clone())),
        )
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("start").await.unwrap();

    assert_eq!(
        child
            .last_request()
            .tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>(),
        vec!["cube"]
    );
}
