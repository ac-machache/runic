use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::Llm;
use runic::ability::{Ability, AbilityBundle, AbilityDescriptor, BuildCtx, Layer, Tools};
use runic::composer::{Agent, ComposeError, Composer, Runtime};
use runic::deferred::{ability_activated_key, activated_ability_ids};
use runic_hook::{HookLifecycle, HookOutcome, WriteHook};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_skills::SkillSet;
use runic_state::AgentState;
use runic_subagent::{Subagent, SubagentBuilder, SubagentReq};
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
        &[HookLifecycle::AfterAgent]
    }
    async fn after_agent(&self, _state: &mut AgentState) -> HookOutcome {
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
impl Ability for DeferredAbility {
    fn name(&self) -> &str {
        self.id
    }

    fn descriptor(&self) -> AbilityDescriptor {
        AbilityDescriptor::deferred(self.id, self.description)
    }

    async fn contribute(
        &self,
        bundle: &mut AbilityBundle,
        _ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        if !self.prompt.is_empty() {
            bundle.prompt(Layer::Stable, self.prompt);
        }
        for tool in &self.tools {
            bundle.tool(tool.clone());
        }
        for hook in &self.hooks {
            bundle.write_hook(hook.clone());
        }
        for set in &self.skills {
            bundle.skill_set(set.clone());
        }
        for def in &self.subagents {
            bundle.subagent(def.clone());
        }
        Ok(())
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
    Subagent::new("worker", "a worker subagent")
        .max_turns(3)
        .prompt("you are a worker")
}

struct ChildBuilder;

#[async_trait]
impl SubagentBuilder for ChildBuilder {
    async fn provider(&self, _req: &SubagentReq<'_>) -> Arc<dyn Provider> {
        ScriptedProvider::new(vec![text("child done")])
    }
    fn default_model(&self, _req: &SubagentReq<'_>) -> String {
        "child-model".into()
    }
    async fn tool_pool(&self, _req: &SubagentReq<'_>) -> Vec<Arc<dyn Tool>> {
        vec![]
    }
}

fn state_flag(agent: &runic_agent::Agent, key: &str) -> bool {
    agent
        .state()
        .data()
        .get(key)
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn tool_result_texts(agent: &runic_agent::Agent) -> Vec<String> {
    agent
        .state()
        .events()
        .iter()
        .filter_map(|event| match event {
            runic_state::SessionEvent::Message { msg, .. } => Some(msg),
            _ => None,
        })
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
    assert!(load_result.contains("\"name\": \"refund\""));
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
        .with(Tools(vec![Arc::new(CountingTool {
            name: "load_ability",
            calls,
        })]))
        .with(DeferredAbility::new("billing", "invoices"))
        .build("alice", "s1")
        .await;

    match result {
        Err(ComposeError::ReservedToolName { ability }) => {
            assert!(ability.ends_with("::Tools"))
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
        .with(Tools(vec![Arc::new(CountingTool {
            name: "refund",
            calls: eager_calls,
        })]))
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
            assert!(first_ability.ends_with("::Tools"));
            assert_eq!(second_ability, "billing");
            assert_eq!(tool, "refund");
        }
        Err(other) => panic!("expected duplicate-tool error, got {other}"),
        Ok(_) => panic!("colliding deferred tool must be rejected"),
    }
}

#[tokio::test]
async fn deferred_hooks_compose_fine_and_stay_inert_before_load() {
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
        "deferred hook must not fire before load"
    );
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
        Runtime::new(),
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
        Runtime::new(),
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
        .with(runic::ability::Skills(docs))
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
        Runtime::new().subagent_builder(Arc::new(ChildBuilder)),
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
        Runtime::new().subagent_builder(Arc::new(ChildBuilder)),
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
        Runtime::new(),
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
