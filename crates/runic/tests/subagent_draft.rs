use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::Llm;
use runic::ability::{ability, subagent};
use runic::composer::Agent;
use runic_agent::RunContext;
use runic_hook::{HookLifecycle, HookOutcome, WriteHook};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_state::AgentState;
use runic_subagent::Subagent;
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

#[tokio::test]
async fn draft_owned_tools_are_isolated_and_models_resolve() {
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
        .with(runic::ability::Tools(vec![Arc::new(NamedTool(
            "main-tool",
        ))]))
        .with(
            subagent("sub-a", "a expert")
                .prompt("you are a")
                .provider(child_a.clone())
                .tool(NamedTool("only-a")),
        )
        .with(
            subagent("sub-b", "b expert")
                .prompt("you are b")
                .provider(child_b.clone())
                .model("custom-child")
                .tool(NamedTool("only-b")),
        )
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

#[tokio::test]
async fn a_consumer_hook_on_the_draft_reaches_the_childs_tool_calls() {
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
        .with(
            subagent("crm-expert", "crm digger")
                .prompt("dig")
                .provider(child.clone())
                .tool(RecordingTool {
                    name: "mcp__crm__lookup",
                    seen: seen.clone(),
                })
                .hook(InjectUserId),
        )
        .build("alice", "s1")
        .await
        .unwrap();

    let ctx = RunContext::new().config_value("user_id", serde_json::json!("u-42"));
    agent.run_with("start", ctx).await.unwrap();

    let args = seen.lock().unwrap().clone().unwrap();
    assert_eq!(args["user_id"], serde_json::json!("u-42"));
    assert_eq!(args["query"], serde_json::json!("dupont"));
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

#[tokio::test]
async fn child_abilities_see_the_child_model_not_the_parents() {
    let seen_model = Arc::new(Mutex::new(None));
    let child = ScriptedProvider::new(vec![]);
    let provider = ScriptedProvider::new(vec![]);

    Agent::new(Llm::new(provider, "main-model"))
        .with(
            subagent("expert", "digs")
                .prompt("dig")
                .provider(child)
                .model("child-override")
                .with(CtxProbe(seen_model.clone()))
                .tool(NamedTool("t")),
        )
        .build("alice", "s1")
        .await
        .unwrap();

    assert_eq!(
        seen_model.lock().unwrap().as_deref(),
        Some("child-override")
    );
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
        .with(
            subagent("crm-expert", "crm digger")
                .prompt("dig")
                .provider(child.clone())
                .tool(RecordingTool {
                    name: "mcp__crm__lookup",
                    seen: seen.clone(),
                })
                .hook(InjectUserId),
        )
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("start").await.unwrap();

    assert!(seen.lock().unwrap().is_none());
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
        .with(
            subagent("sub-a", "a expert")
                .prompt("you are a")
                .provider(child.clone())
                .tool(NamedTool("only-a")),
        )
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
        .with(
            subagent("sub-a", "a expert")
                .prompt("you are a")
                .provider(child_a.clone())
                .tool(NamedTool("only-a")),
        )
        .with(
            subagent("sub-b", "b expert")
                .prompt("you are b")
                .provider(child_b.clone())
                .tool(NamedTool("only-b")),
        )
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

#[tokio::test]
async fn nested_subagents_inside_a_draft_are_rejected() {
    let def = Subagent::new("inner", "inner").prompt("inner");
    let provider = ScriptedProvider::new(vec![]);
    let Err(err) = Agent::new(Llm::new(provider, "main-model"))
        .with(subagent("outer", "outer").with(ability("inner-owner").subagent_def(def)))
        .build("alice", "s1")
        .await
    else {
        panic!("nested subagents must not compose");
    };
    assert!(format!("{err:#}").contains("nested subagents"));
}

#[tokio::test]
async fn deferred_abilities_inside_a_draft_are_rejected() {
    let provider = ScriptedProvider::new(vec![]);
    let Err(err) = Agent::new(Llm::new(provider, "main-model"))
        .with(subagent("outer", "outer").with(ability("gated").describe("gated stuff").deferred()))
        .build("alice", "s1")
        .await
    else {
        panic!("deferred child abilities must not compose");
    };
    assert!(format!("{err:#}").contains("always eager"));
}

#[tokio::test]
async fn ability_prompts_reach_the_child_system_prompt() {
    let child = ScriptedProvider::new(vec![text("done")]);
    let main_provider = ScriptedProvider::new(vec![delegate_to("writer"), text("done")]);

    let mut agent = Agent::new(Llm::new(main_provider, "main-model"))
        .with(
            subagent("writer", "writes")
                .prompt("you are the writer")
                .provider(child.clone())
                .with(ability("style").prompt("EXTRA-SECTION"))
                .tool(NamedTool("pen")),
        )
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("start").await.unwrap();

    let system = child.last_request().system.unwrap_or_default();
    assert!(system.contains("you are the writer"));
    assert!(system.contains("EXTRA-SECTION"));
}

#[tokio::test]
async fn grouped_ability_can_own_a_draft() {
    let child = ScriptedProvider::new(vec![text("done")]);
    let main_provider = ScriptedProvider::new(vec![delegate_to("analyst"), text("done")]);

    let mut agent = Agent::new(Llm::new(main_provider.clone(), "main-model"))
        .with(
            ability("commerce").describe("commerce pack").subagent(
                subagent("analyst", "analyzes")
                    .prompt("analyze")
                    .provider(child.clone())
                    .tool(NamedTool("cube")),
            ),
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

#[tokio::test]
async fn grouped_deferred_draft_is_rejected() {
    let provider = ScriptedProvider::new(vec![]);
    let Err(err) = Agent::new(Llm::new(provider, "main-model"))
        .with(
            ability("commerce").describe("commerce pack").subagent(
                subagent("analyst", "analyzes")
                    .deferred()
                    .tool(NamedTool("cube")),
            ),
        )
        .build("alice", "s1")
        .await
    else {
        panic!("a deferred draft nested in an ability must not compose");
    };
    assert!(format!("{err:#}").contains("outer ability controls activation"));
}
