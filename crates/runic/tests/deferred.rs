use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::Llm;
use runic::ability::{Ability, AbilityDescriptor, BuildCtx, ToAbility, ability};
use runic::composer::{Agent, ComposeError, Composer};
use runic::deferred::{ability_activated_key, activated_ability_ids};
use runic::subagent::Subagent;
use runic_hook::{HookLifecycle, HookOutcome, WriteHook};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_skills::SkillSet;
use runic_state::AgentState;
use runic_tool::{Tool, ToolContext, ToolResult, activated_key};
use runic_types::{ContentBlock, MessageContent, StopReason, TokenUsage, ToolCall};

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

struct CountingTool {
    name: &'static str,
    calls: Arc<Mutex<u32>>,
}

#[async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "counts invocations"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        *self.calls.lock().unwrap() += 1;
        Ok(ToolResult::ok("counted"))
    }
}

struct MarkerHook(Arc<Mutex<bool>>);

#[async_trait]
impl WriteHook for MarkerHook {
    fn name(&self) -> &str {
        "marker"
    }
    fn points(&self) -> &'static [HookLifecycle] {
        &[HookLifecycle::BeforeModel]
    }
    async fn before_model(&self, _state: &mut AgentState) -> HookOutcome {
        *self.0.lock().unwrap() = true;
        HookOutcome::Continue
    }
}

struct DeferredAbility {
    id: &'static str,
    description: &'static str,
    prompt: &'static str,
    tools: Vec<Arc<dyn Tool>>,
    hooks: Vec<Arc<dyn WriteHook>>,
    skills: Vec<Arc<SkillSet>>,
    subagents: Vec<Subagent>,
}

impl DeferredAbility {
    fn new(id: &'static str, description: &'static str) -> Self {
        Self {
            id,
            description,
            prompt: "",
            tools: Vec::new(),
            hooks: Vec::new(),
            skills: Vec::new(),
            subagents: Vec::new(),
        }
    }

    fn prompt(mut self, prompt: &'static str) -> Self {
        self.prompt = prompt;
        self
    }

    fn tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    fn hook(mut self, hook: Arc<dyn WriteHook>) -> Self {
        self.hooks.push(hook);
        self
    }

    fn skill(mut self, set: Arc<SkillSet>) -> Self {
        self.skills.push(set);
        self
    }

    fn subagent(mut self, def: Subagent) -> Self {
        self.subagents.push(def);
        self
    }
}

#[async_trait]
impl ToAbility for DeferredAbility {
    fn name(&self) -> &str {
        self.id
    }

    fn descriptor(&self) -> AbilityDescriptor {
        AbilityDescriptor::deferred(self.id, self.description)
    }

    async fn to_ability(&self, base: Ability, _ctx: &BuildCtx<'_>) -> anyhow::Result<Ability> {
        let mut base = base
            .tools(self.tools.iter().cloned())
            .hooks(self.hooks.iter().cloned())
            .subagents(self.subagents.iter().cloned());
        if !self.prompt.is_empty() {
            base = base.prompt(self.prompt);
        }
        for set in &self.skills {
            base = base.skills(set.clone());
        }
        Ok(base)
    }
}

async fn skill_catalog(namespace: &str, skill_name: &str, body: &str) -> Arc<SkillSet> {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join(skill_name);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {skill_name}\ndescription: test skill\n---\n{body}"),
    )
    .unwrap();
    Arc::new(SkillSet::load_dir(namespace, dir.path()).await)
}

fn worker_def() -> Subagent {
    Subagent::new(
        "worker",
        "a worker subagent",
        Agent::new(
            Llm::new(
                ScriptedProvider::new(vec![text("child done")]),
                "child-model",
            )
            .instructions("you are a worker")
            .max_turns(3),
        ),
    )
}

fn state_flag(agent: &runic_agent::Runner, key: &str) -> bool {
    agent
        .state()
        .data()
        .get(key)
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn tool_result_texts(agent: &runic_agent::Runner) -> Vec<String> {
    agent
        .state()
        .messages_for_provider()
        .iter()
        .filter_map(|msg| match &msg.content {
            MessageContent::Blocks(blocks) => Some(blocks),
            _ => None,
        })
        .flatten()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.text()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_deferred_ability_is_hidden_but_announced_in_the_catalog() {
    let provider = ScriptedProvider::new(vec![text("done")]);
    let agent = Agent::new(Llm::new(provider, "test-model").instructions("core"))
        .with(DeferredAbility::new("billing", "invoices and refunds").prompt("billing rules"))
        .build("alice", "s1")
        .await
        .unwrap();

    let system = &agent.state().system_prompt;
    assert!(system.contains("<deferred-abilities>"));
    assert!(system.contains("- billing: invoices and refunds"));
    assert!(!system.contains("billing rules"));
}

#[tokio::test]
async fn loading_an_ability_unlocks_its_tools_and_persists_activation() {
    let counted = Arc::new(Mutex::new(0));
    let provider = ScriptedProvider::new(vec![
        call("c1", "load_ability", serde_json::json!({ "id": "billing" })),
        call("c2", "refund", serde_json::json!({})),
        text("done"),
    ]);
    let mut agent = Agent::new(Llm::new(provider, "test-model").instructions("core"))
        .with(
            DeferredAbility::new("billing", "invoices and refunds")
                .prompt("billing rules")
                .tool(Arc::new(CountingTool {
                    name: "refund",
                    calls: counted.clone(),
                })),
        )
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("refund order 7").await.unwrap();

    assert_eq!(*counted.lock().unwrap(), 1);
    assert!(state_flag(&agent, &ability_activated_key("billing")));
    assert!(state_flag(&agent, &activated_key("refund")));
    let results = tool_result_texts(&agent);
    let load_result = results
        .iter()
        .find(|content| content.contains("Ability `billing` loaded"))
        .expect("load_ability result present");
    assert!(load_result.contains("billing rules"));
    assert!(load_result.contains("refund"));
    assert!(
        !load_result.contains("\"parameters\""),
        "an unlocked tool's schema reaches the model through the request's tools \
         array from the next turn; repeating it here pays for it twice, forever: {load_result}"
    );
    assert!(activated_ability_ids(agent.state().data()).contains(&"billing".to_string()));
}

#[tokio::test]
async fn loading_an_unknown_id_reports_the_available_ones() {
    let provider = ScriptedProvider::new(vec![
        call("c1", "load_ability", serde_json::json!({ "id": "ghost" })),
        text("done"),
    ]);
    let mut agent = Agent::new(Llm::new(provider, "test-model"))
        .with(DeferredAbility::new("billing", "invoices"))
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("go").await.unwrap();

    let results = tool_result_texts(&agent);
    let error = results
        .iter()
        .find(|content| content.contains("No ability with id `ghost`"))
        .expect("unknown-id error present");
    assert!(error.contains("billing"));
    assert!(!state_flag(&agent, &ability_activated_key("ghost")));
}

#[tokio::test]
async fn a_second_load_of_the_same_ability_bounces() {
    let counted = Arc::new(Mutex::new(0));
    let provider = ScriptedProvider::new(vec![
        call("c1", "load_ability", serde_json::json!({ "id": "billing" })),
        call("c2", "load_ability", serde_json::json!({ "id": "billing" })),
        text("done"),
    ]);
    let mut agent = Agent::new(Llm::new(provider, "test-model"))
        .with(
            DeferredAbility::new("billing", "invoices").tool(Arc::new(CountingTool {
                name: "refund",
                calls: counted.clone(),
            })),
        )
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("go").await.unwrap();

    let results = tool_result_texts(&agent);
    assert!(
        results
            .iter()
            .any(|content| content.contains("already available"))
    );
}

#[tokio::test]
async fn a_user_tool_named_load_ability_is_rejected_when_deferred_abilities_exist() {
    let calls = Arc::new(Mutex::new(0));
    let result = Agent::new(Llm::new(ScriptedProvider::new(vec![]), "test-model"))
        .with(ability("bad-kit").tool(CountingTool {
            name: "load_ability",
            calls,
        }))
        .with(DeferredAbility::new("billing", "invoices"))
        .build("alice", "s1")
        .await;

    match result {
        Err(ComposeError::ReservedToolName { ability }) => {
            assert_eq!(ability, "bad-kit")
        }
        Err(other) => panic!("expected reserved-name error, got {other}"),
        Ok(_) => panic!("a user load_ability tool must be rejected"),
    }
}

#[tokio::test]
async fn a_deferred_tool_colliding_with_an_eager_tool_is_rejected() {
    let eager_calls = Arc::new(Mutex::new(0));
    let deferred_calls = Arc::new(Mutex::new(0));
    let result = Agent::new(Llm::new(ScriptedProvider::new(vec![]), "test-model"))
        .with(ability("eager-kit").tool(CountingTool {
            name: "refund",
            calls: eager_calls,
        }))
        .with(
            DeferredAbility::new("billing", "invoices").tool(Arc::new(CountingTool {
                name: "refund",
                calls: deferred_calls,
            })),
        )
        .build("alice", "s1")
        .await;

    match result {
        Err(ComposeError::DuplicateToolName {
            first_ability,
            second_ability,
            tool,
        }) => {
            assert_eq!(first_ability, "eager-kit");
            assert_eq!(second_ability, "billing");
            assert_eq!(tool, "refund");
        }
        Err(other) => panic!("expected duplicate-tool error, got {other}"),
        Ok(_) => panic!("colliding deferred tool must be rejected"),
    }
}

#[tokio::test]
async fn a_deferred_abilitys_model_hook_stays_dormant_until_it_is_loaded() {
    let fired = Arc::new(Mutex::new(false));
    let provider = ScriptedProvider::new(vec![text("done")]);
    let mut agent = Agent::new(Llm::new(provider, "test-model"))
        .with(DeferredAbility::new("billing", "invoices").hook(Arc::new(MarkerHook(fired.clone()))))
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("go").await.unwrap();

    assert!(
        !*fired.lock().unwrap(),
        "the ability was never loaded, so nothing it carries is in play"
    );
}

#[tokio::test]
async fn loading_the_ability_brings_its_model_hook_into_play() {
    let fired = Arc::new(Mutex::new(false));
    let provider = ScriptedProvider::new(vec![
        call("c1", "load_ability", serde_json::json!({ "id": "billing" })),
        text("done"),
    ]);
    let mut agent = Agent::new(Llm::new(provider, "test-model"))
        .with(DeferredAbility::new("billing", "invoices").hook(Arc::new(MarkerHook(fired.clone()))))
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("go").await.unwrap();

    assert!(
        *fired.lock().unwrap(),
        "the turn after load_ability is inside the ability's lifetime"
    );
}

struct EchoInputTool;

#[async_trait]
impl Tool for EchoInputTool {
    fn name(&self) -> &str {
        "refund"
    }
    fn description(&self) -> &str {
        "echoes its input back"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok(args.to_string()))
    }
}

struct InjectUserIdHook;

#[async_trait]
impl WriteHook for InjectUserIdHook {
    fn name(&self) -> &str {
        "inject-user-id"
    }
    fn points(&self) -> &'static [HookLifecycle] {
        &[HookLifecycle::BeforeTool]
    }
    async fn before_tool(&self, _state: &mut AgentState, call: &mut ToolCall) -> HookOutcome {
        if call.name == "refund"
            && let serde_json::Value::Object(map) = &mut call.input
        {
            map.insert("user_id".into(), serde_json::json!("u-1"));
        }
        HookOutcome::Continue
    }
}

#[tokio::test]
async fn a_before_tool_hook_still_guards_the_deferred_ability_s_own_tool_once_loaded() {
    let provider = ScriptedProvider::new(vec![
        call("c1", "load_ability", serde_json::json!({ "id": "billing" })),
        call("c2", "refund", serde_json::json!({})),
        text("done"),
    ]);
    let mut agent = Agent::new(Llm::new(provider, "test-model").instructions("core"))
        .with(
            DeferredAbility::new("billing", "invoices and refunds")
                .tool(Arc::new(EchoInputTool))
                .hook(Arc::new(InjectUserIdHook)),
        )
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("refund order 7").await.unwrap();

    let results = tool_result_texts(&agent);
    let refund_result = results
        .iter()
        .find(|content| content.contains("user_id"))
        .expect("the before_tool hook injected user_id into the refund call");
    assert!(refund_result.contains("\"user_id\":\"u-1\""));
}

#[tokio::test]
async fn a_rebuild_with_the_activated_id_merges_the_full_bundle() {
    let counted = Arc::new(Mutex::new(0));
    let fired = Arc::new(Mutex::new(false));
    let provider = ScriptedProvider::new(vec![
        call("c1", "refund", serde_json::json!({})),
        text("done"),
    ]);
    let mut agent = Composer::new(
        Agent::new(Llm::new(provider, "test-model").instructions("core")).with(
            DeferredAbility::new("billing", "invoices and refunds")
                .prompt("billing rules")
                .tool(Arc::new(CountingTool {
                    name: "refund",
                    calls: counted.clone(),
                }))
                .hook(Arc::new(MarkerHook(fired.clone()))),
        ),
    )
    .activated(["billing"])
    .build("alice", "s1")
    .await
    .unwrap();

    let system = agent.state().system_prompt.clone();
    assert!(system.contains("billing rules"));
    assert!(!system.contains("<deferred-abilities>"));

    agent.run("refund order 7").await.unwrap();

    assert_eq!(*counted.lock().unwrap(), 1);
    assert!(*fired.lock().unwrap(), "activated ability's hook must fire");
}

#[tokio::test]
async fn the_catalog_lists_only_unloaded_abilities() {
    let provider = ScriptedProvider::new(vec![text("done")]);
    let agent = Composer::new(
        Agent::new(Llm::new(provider, "test-model"))
            .with(DeferredAbility::new("billing", "invoices"))
            .with(DeferredAbility::new("shipping", "labels and tracking")),
    )
    .activated(["billing"])
    .build("alice", "s1")
    .await
    .unwrap();

    let system = &agent.state().system_prompt;
    assert!(system.contains("- shipping: labels and tracking"));
    assert!(!system.contains("- billing"));
}

#[tokio::test]
async fn deferred_skills_are_gated_until_load_then_viewable_in_the_same_run() {
    let crm = skill_catalog("crm", "pipeline", "The pipeline body.").await;
    let docs = skill_catalog("docs", "guide", "The guide body.").await;
    let provider = ScriptedProvider::new(vec![
        call(
            "c1",
            "skill_view",
            serde_json::json!({ "name": "crm:pipeline" }),
        ),
        call(
            "c2",
            "load_ability",
            serde_json::json!({ "id": "crm-pack" }),
        ),
        call(
            "c3",
            "skill_view",
            serde_json::json!({ "name": "crm:pipeline" }),
        ),
        call(
            "c4",
            "skill_view",
            serde_json::json!({ "name": "docs:guide" }),
        ),
        text("done"),
    ]);
    let mut agent = Agent::new(Llm::new(provider, "test-model"))
        .with(ability("docs").skills(docs))
        .with(DeferredAbility::new("crm-pack", "crm workflows").skill(crm))
        .build("alice", "s1")
        .await
        .unwrap();

    let system = agent.state().system_prompt.clone();
    assert!(system.contains("docs:guide"));
    assert!(!system.contains("crm:pipeline"));

    agent.run("go").await.unwrap();

    let results = tool_result_texts(&agent);
    assert!(
        results
            .iter()
            .any(|content| content.contains("Skill `crm:pipeline` is not available yet"))
    );
    assert!(
        results
            .iter()
            .any(|content| content.contains("Unlocked skills") && content.contains("crm:pipeline"))
    );
    assert!(
        results
            .iter()
            .any(|content| content.contains("The pipeline body."))
    );
    assert!(
        results
            .iter()
            .any(|content| content.contains("The guide body."))
    );
}

#[tokio::test]
async fn deferred_subagents_are_gated_until_load_then_delegatable_in_the_same_run() {
    let provider = ScriptedProvider::new(vec![
        call(
            "c1",
            "delegate",
            serde_json::json!({ "agent": "worker", "prompt": "do it" }),
        ),
        call("c2", "load_ability", serde_json::json!({ "id": "ops" })),
        call(
            "c3",
            "delegate",
            serde_json::json!({ "agent": "worker", "prompt": "do it" }),
        ),
        text("done"),
    ]);
    let mut agent = Composer::new(
        Agent::new(Llm::new(provider, "test-model"))
            .with(DeferredAbility::new("ops", "operations crew").subagent(worker_def())),
    )
    .build("alice", "s1")
    .await
    .unwrap();

    assert!(!agent.state().system_prompt.contains("worker"));

    agent.run("go").await.unwrap();

    let results = tool_result_texts(&agent);
    assert!(
        results
            .iter()
            .any(|content| content.contains("Subagent `worker` is not available yet"))
    );
    assert!(
        results
            .iter()
            .any(|content| content.contains("Unlocked subagents") && content.contains("worker"))
    );
    assert!(results.iter().any(|content| content.contains("child done")));
}

#[tokio::test]
async fn a_rebuild_with_the_activated_id_ungates_skills_and_subagents() {
    let crm = skill_catalog("crm", "pipeline", "The pipeline body.").await;
    let provider = ScriptedProvider::new(vec![
        call(
            "c1",
            "skill_view",
            serde_json::json!({ "name": "crm:pipeline" }),
        ),
        call(
            "c2",
            "delegate",
            serde_json::json!({ "agent": "worker", "prompt": "do it" }),
        ),
        text("done"),
    ]);
    let mut agent = Composer::new(
        Agent::new(Llm::new(provider, "test-model")).with(
            DeferredAbility::new("crm-pack", "crm workflows")
                .skill(crm)
                .subagent(worker_def()),
        ),
    )
    .activated(["crm-pack"])
    .build("alice", "s1")
    .await
    .unwrap();

    let system = agent.state().system_prompt.clone();
    assert!(system.contains("crm:pipeline"));
    assert!(system.contains("worker"));
    assert!(!system.contains("<deferred-abilities>"));

    agent.run("go").await.unwrap();

    let results = tool_result_texts(&agent);
    assert!(
        results
            .iter()
            .any(|content| content.contains("The pipeline body."))
    );
    assert!(results.iter().any(|content| content.contains("child done")));
}

#[tokio::test]
async fn a_loaded_ability_bounces_after_a_rebuild_with_its_id() {
    let provider = ScriptedProvider::new(vec![
        call("c1", "load_ability", serde_json::json!({ "id": "billing" })),
        text("done"),
    ]);
    let mut agent = Composer::new(
        Agent::new(Llm::new(provider, "test-model"))
            .with(DeferredAbility::new("billing", "invoices"))
            .with(DeferredAbility::new("shipping", "labels")),
    )
    .activated(["billing"])
    .build("alice", "s1")
    .await
    .unwrap();

    agent.run("go").await.unwrap();

    let results = tool_result_texts(&agent);
    assert!(
        results
            .iter()
            .any(|content| content.contains("already available"))
    );
}
