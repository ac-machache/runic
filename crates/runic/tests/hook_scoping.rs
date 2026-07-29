use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::Llm;
use runic::ability::ability;
use runic::composer::Agent;
use runic::subagent::Subagent;
use runic_hook::{HookLifecycle, HookOutcome, WriteHook};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_skills::SkillSet;
use runic_state::AgentState;
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, StopReason, TokenUsage, ToolCall};

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

struct Noop(&'static str);

#[async_trait]
impl Tool for Noop {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "does nothing"
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

/// Records the name of every call it is fired for.
struct Witness {
    label: &'static str,
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl WriteHook for Witness {
    fn name(&self) -> &str {
        self.label
    }
    fn points(&self) -> &'static [HookLifecycle] {
        &[HookLifecycle::BeforeTool]
    }
    async fn before_tool(&self, _state: &mut AgentState, call: &mut ToolCall) -> HookOutcome {
        self.seen
            .lock()
            .unwrap()
            .push(format!("{}:{}", self.label, call.name));
        HookOutcome::Continue
    }
}

#[tokio::test]
async fn a_tool_hook_fires_only_for_its_own_abilitys_tools() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = ScriptedProvider::new(vec![
        call("c1", "alpha", serde_json::json!({})),
        call("c2", "beta", serde_json::json!({})),
        text("done"),
    ]);

    let mut agent = Agent::new(Llm::new(provider, "test-model"))
        .with(ability("pack-a").tool(Noop("alpha")).hook(Witness {
            label: "a",
            seen: seen.clone(),
        }))
        .with(ability("pack-b").tool(Noop("beta")).hook(Witness {
            label: "b",
            seen: seen.clone(),
        }))
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("go").await.unwrap();

    assert_eq!(
        *seen.lock().unwrap(),
        vec!["a:alpha", "b:beta"],
        "each ability's hook sees only calls to the tools that ability owns"
    );
}

#[tokio::test]
async fn a_runtime_hook_sees_every_call() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = ScriptedProvider::new(vec![
        call("c1", "alpha", serde_json::json!({})),
        call("c2", "beta", serde_json::json!({})),
        text("done"),
    ]);

    let mut agent = Agent::new(Llm::new(provider, "test-model"))
        .with(ability("pack-a").tool(Noop("alpha")))
        .with(ability("pack-b").tool(Noop("beta")))
        .hook(Witness {
            label: "global",
            seen: seen.clone(),
        })
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("go").await.unwrap();

    assert_eq!(
        *seen.lock().unwrap(),
        vec!["global:alpha", "global:beta"],
        "agent-wide policy is what Runtime::hook is for"
    );
}

#[tokio::test]
async fn delegation_is_scoped_by_the_targeted_subagent_not_the_tool_name() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let child = || Agent::new(Llm::new(ScriptedProvider::new(vec![text("child")]), "m"));
    let provider = ScriptedProvider::new(vec![
        call(
            "c1",
            "delegate",
            serde_json::json!({ "agent": "mine", "prompt": "go" }),
        ),
        call(
            "c2",
            "delegate",
            serde_json::json!({ "agent": "theirs", "prompt": "go" }),
        ),
        text("done"),
    ]);

    let mut agent = Agent::new(Llm::new(provider, "test-model"))
        .with(
            ability("mine-pack")
                .subagent(Subagent::new("mine", "mine", child()))
                .hook(Witness {
                    label: "mine",
                    seen: seen.clone(),
                }),
        )
        .with(ability("theirs-pack").subagent(Subagent::new("theirs", "theirs", child())))
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("go").await.unwrap();

    assert_eq!(
        *seen.lock().unwrap(),
        vec!["mine:delegate"],
        "both calls are to `delegate`; only the one targeting the owned subagent fires the hook"
    );
}

async fn skill_set(namespace: &str, name: &str) -> Arc<SkillSet> {
    let dir = tempfile::tempdir().expect("tempdir");
    let skill_dir = dir.path().join(name);
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: a scoped skill\n---\nDo it."),
    )
    .expect("skill file");
    Arc::new(SkillSet::load_dir(namespace, dir.path()).await)
}

#[tokio::test]
async fn skill_views_are_scoped_by_the_targeted_skill() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mine = skill_set("", "mine").await;
    let theirs = skill_set("", "theirs").await;
    let provider = ScriptedProvider::new(vec![
        call("c1", "skill_view", serde_json::json!({ "name": "mine" })),
        call("c2", "skill_view", serde_json::json!({ "name": "theirs" })),
        text("done"),
    ]);

    let mut agent = Agent::new(Llm::new(provider, "test-model"))
        .with(ability("mine-pack").skills(mine).hook(Witness {
            label: "mine",
            seen: seen.clone(),
        }))
        .with(ability("theirs-pack").skills(theirs))
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("go").await.unwrap();

    assert_eq!(*seen.lock().unwrap(), vec!["mine:skill_view"]);
}

struct Order {
    label: &'static str,
    seen: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait]
impl WriteHook for Order {
    fn name(&self) -> &str {
        self.label
    }
    fn priority(&self) -> i32 {
        if self.label == "ability" { -100 } else { 100 }
    }
    fn points(&self) -> &'static [HookLifecycle] {
        &[HookLifecycle::BeforeTool]
    }
    async fn before_tool(&self, _state: &mut AgentState, _call: &mut ToolCall) -> HookOutcome {
        self.seen.lock().unwrap().push(self.label);
        HookOutcome::Continue
    }
}

#[tokio::test]
async fn an_agent_hook_runs_before_an_ability_hook_whatever_the_priorities() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = ScriptedProvider::new(vec![
        call("c1", "alpha", serde_json::json!({})),
        text("done"),
    ]);

    let mut agent = Agent::new(Llm::new(provider, "test-model"))
        .with(ability("pack-a").tool(Noop("alpha")).hook(Order {
            label: "ability",
            seen: seen.clone(),
        }))
        .hook(Order {
            label: "agent",
            seen: seen.clone(),
        })
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("go").await.unwrap();

    assert_eq!(
        *seen.lock().unwrap(),
        vec!["agent", "ability"],
        "the ability hook asked for priority -100 and still lost — global policy cannot be pre-empted"
    );
}
