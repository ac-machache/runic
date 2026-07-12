use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::ability::{
    Ability, AbilityBundle, AbilityDescriptor, ActivationPolicy, BuildCtx, Hooks, Skills, Tools,
};
use runic::composer::{ComposeError, Composer};
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

fn text(t: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::Text {
            text: t.into(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::EndTurn,
        tool_calls: vec![],
        usage: TokenUsage::default(),
    }
}

fn call(name: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::ToolUse {
            id: "c1".into(),
            name: name.into(),
            input: serde_json::json!({}),
            provider_metadata: None,
        }],
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: name.into(),
            input: serde_json::json!({}),
        }],
        usage: TokenUsage::default(),
    }
}

struct Ping(Arc<Mutex<u32>>);

#[async_trait]
impl Tool for Ping {
    fn name(&self) -> &str {
        "ping"
    }
    fn description(&self) -> &str {
        "pings"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        *self.0.lock().unwrap() += 1;
        Ok(ToolResult::ok("pong"))
    }
}

async fn crm_catalog() -> Arc<SkillSet> {
    let dir = tempfile::tempdir().unwrap();
    let skill = dir.path().join("pipeline");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: pipeline\ndescription: crm pipeline\n---\nBody.",
    )
    .unwrap();
    Arc::new(SkillSet::load_dir("crm", dir.path()).await)
}

#[tokio::test]
async fn composes_the_prompt_from_instructions_and_abilities() {
    let provider = ScriptedProvider::new(vec![text("done")]);
    let agent = Composer::new(provider, "test-model")
        .instructions("core instructions")
        .with(Skills(crm_catalog().await))
        .build("alice", "s1")
        .await
        .unwrap();

    let system = &agent.state().system_prompt;
    assert!(system.contains("core instructions"));
    assert!(system.contains("<available-skills>"));
    assert!(system.contains("crm:pipeline"));
}

struct Marker(Arc<Mutex<bool>>);

#[async_trait]
impl WriteHook for Marker {
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

#[tokio::test]
async fn a_hooks_ability_registers_custom_write_hooks() {
    let provider = ScriptedProvider::new(vec![text("done")]);
    let fired = Arc::new(Mutex::new(false));
    let mut agent = Composer::new(provider, "test-model")
        .instructions("go")
        .with(Hooks(vec![Arc::new(Marker(fired.clone()))]))
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("hi").await.unwrap();

    assert!(*fired.lock().unwrap(), "custom write hook must fire");
}

#[tokio::test]
async fn a_tools_ability_registers_runnable_tools() {
    let provider = ScriptedProvider::new(vec![call("ping"), text("done")]);
    let pings = Arc::new(Mutex::new(0));
    let mut agent = Composer::new(provider, "test-model")
        .instructions("go")
        .with(Tools(vec![Arc::new(Ping(pings.clone()))]))
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("hi").await.unwrap();

    assert_eq!(*pings.lock().unwrap(), 1);
}

struct FailingAbility;

#[async_trait]
impl Ability for FailingAbility {
    fn name(&self) -> &str {
        "failing-test-ability"
    }

    async fn contribute(
        &self,
        bundle: &mut AbilityBundle,
        _ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        bundle.prompt(runic::ability::Layer::Stable, "partial contribution");
        anyhow::bail!("setup exploded")
    }
}

struct CountingAbility(Arc<Mutex<u32>>);

#[async_trait]
impl Ability for CountingAbility {
    async fn contribute(
        &self,
        _bundle: &mut AbilityBundle,
        _ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        *self.0.lock().unwrap() += 1;
        Ok(())
    }
}

#[tokio::test]
async fn failing_ability_reports_its_name_and_source() {
    let provider = ScriptedProvider::new(vec![]);
    let error = match Composer::new(provider, "test-model")
        .with(FailingAbility)
        .build("alice", "s1")
        .await
    {
        Ok(_) => panic!("a failing ability must abort the build"),
        Err(error) => error,
    };

    match error {
        ComposeError::Ability { ability, source } => {
            assert_eq!(ability, "failing-test-ability");
            assert_eq!(source.to_string(), "setup exploded");
        }
        other => panic!("expected ability failure, got {other}"),
    }
}

#[tokio::test]
async fn abilities_after_a_failure_are_not_executed() {
    let provider = ScriptedProvider::new(vec![]);
    let calls = Arc::new(Mutex::new(0));
    let result = Composer::new(provider, "test-model")
        .with(FailingAbility)
        .with(CountingAbility(calls.clone()))
        .build("alice", "s1")
        .await;

    assert!(result.is_err());
    assert_eq!(*calls.lock().unwrap(), 0);
}

struct DescribedAbility {
    name: &'static str,
    descriptor: AbilityDescriptor,
    calls: Arc<Mutex<u32>>,
}

#[async_trait]
impl Ability for DescribedAbility {
    fn name(&self) -> &str {
        self.name
    }

    fn descriptor(&self) -> AbilityDescriptor {
        self.descriptor.clone()
    }

    async fn contribute(
        &self,
        _bundle: &mut AbilityBundle,
        _ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        *self.calls.lock().unwrap() += 1;
        Ok(())
    }
}

fn described(
    name: &'static str,
    descriptor: AbilityDescriptor,
    calls: Arc<Mutex<u32>>,
) -> DescribedAbility {
    DescribedAbility {
        name,
        descriptor,
        calls,
    }
}

#[tokio::test]
async fn deferred_ability_requires_an_explicit_id_before_contribution() {
    let calls = Arc::new(Mutex::new(0));
    let descriptor = AbilityDescriptor {
        id: None,
        description: Some("missing an id".into()),
        activation: ActivationPolicy::Deferred,
    };
    let result = Composer::new(ScriptedProvider::new(vec![]), "test-model")
        .with(described("missing-id", descriptor, calls.clone()))
        .build("alice", "s1")
        .await;

    match result {
        Err(ComposeError::DeferredAbilityMissingId { ability }) => {
            assert_eq!(ability, "missing-id");
        }
        Err(other) => panic!("expected missing ID error, got {other}"),
        Ok(_) => panic!("a deferred ability without an ID must fail"),
    }
    assert_eq!(*calls.lock().unwrap(), 0);
}

#[tokio::test]
async fn ability_ids_enforce_the_full_syntax_contract() {
    let mut invalid = vec![
        "".to_string(),
        "Uppercase".to_string(),
        "-leading".to_string(),
        "trailing.".to_string(),
        "has space".to_string(),
        "has/slash".to_string(),
        "unicode-e".replace('e', "é"),
    ];
    invalid.push("a".repeat(65));

    for id in invalid {
        let descriptor = AbilityDescriptor::deferred(id.clone(), "invalid test ability");
        let result = Composer::new(ScriptedProvider::new(vec![]), "test-model")
            .with(described("invalid-id", descriptor, Arc::new(Mutex::new(0))))
            .build("alice", "s1")
            .await;

        match result {
            Err(ComposeError::InvalidAbilityId {
                ability,
                id: actual,
            }) => {
                assert_eq!(ability, "invalid-id");
                assert_eq!(actual, id);
            }
            Err(other) => panic!("expected invalid ID error for {id:?}, got {other}"),
            Ok(_) => panic!("invalid ability ID {id:?} must fail"),
        }
    }
}

#[tokio::test]
async fn ability_ids_accept_allowed_punctuation_and_the_64_byte_boundary() {
    for id in ["a-b_c.d9".to_string(), "a".repeat(64)] {
        let descriptor = AbilityDescriptor::deferred(id, "valid test ability");
        let result = Composer::new(ScriptedProvider::new(vec![]), "test-model")
            .with(described("valid-id", descriptor, Arc::new(Mutex::new(0))))
            .build("alice", "s1")
            .await;

        assert!(result.is_ok());
    }
}

#[tokio::test]
async fn duplicate_explicit_ids_fail_before_any_ability_contributes() {
    let calls = Arc::new(Mutex::new(0));
    let eager = AbilityDescriptor {
        id: Some("shared-id".into()),
        description: None,
        activation: ActivationPolicy::Eager,
    };
    let deferred = AbilityDescriptor::deferred("shared-id", "duplicate test ability");
    let result = Composer::new(ScriptedProvider::new(vec![]), "test-model")
        .with(described("first", eager, calls.clone()))
        .with(described("second", deferred, calls.clone()))
        .build("alice", "s1")
        .await;

    match result {
        Err(ComposeError::DuplicateAbilityId {
            id,
            first_ability,
            second_ability,
        }) => {
            assert_eq!(id, "shared-id");
            assert_eq!(first_ability, "first");
            assert_eq!(second_ability, "second");
        }
        Err(other) => panic!("expected duplicate ID error, got {other}"),
        Ok(_) => panic!("duplicate explicit ability IDs must fail"),
    }
    assert_eq!(*calls.lock().unwrap(), 0);
}
