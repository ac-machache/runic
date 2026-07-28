use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use proptest::prelude::*;
use runic::Llm;
use runic::ability::ability;
use runic::composer::{Agent, Composer, Runtime};
use runic::subagent::Subagent;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_skills::SkillSet;
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, MessageContent, StopReason, TokenUsage, ToolCall};

struct QueueProvider {
    responses: Mutex<VecDeque<CompletionResponse>>,
}

impl QueueProvider {
    fn new(responses: Vec<CompletionResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
        })
    }
}

#[async_trait]
impl Provider for QueueProvider {
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

fn tool_result_pairs(agent: &runic_agent::Runner) -> Vec<(String, String, bool)> {
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
            ContentBlock::ToolResult {
                tool_name,
                content,
                is_error,
                ..
            } => Some((tool_name.clone(), content.text(), *is_error)),
            _ => None,
        })
        .collect()
}

struct TrackedTool {
    name: String,
    marker: String,
}

#[async_trait]
impl Tool for TrackedTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "tracked"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok(self.marker.clone()))
    }
}

#[derive(Debug, Clone)]
struct AbilitySpec {
    id: String,
    deferred: bool,
    activated: bool,
    tool_count: usize,
}

impl AbilitySpec {
    fn live(&self) -> bool {
        !self.deferred || self.activated
    }
    fn tool_name(&self, index: usize) -> String {
        format!("{}_tool{index}", self.id)
    }
}

fn ability_specs() -> impl Strategy<Value = Vec<AbilitySpec>> {
    prop::collection::vec((any::<bool>(), any::<bool>(), 0usize..3), 1..6).prop_map(|raw| {
        raw.into_iter()
            .enumerate()
            .map(|(index, (deferred, activated, tool_count))| AbilitySpec {
                id: format!("ab{index}"),
                deferred,
                activated: deferred && activated,
                tool_count,
            })
            .collect()
    })
}

fn build_gating_script(specs: &[AbilitySpec]) -> Vec<CompletionResponse> {
    let mut responses = Vec::new();
    let mut call_id = 0u32;
    let mut next_id = || {
        call_id += 1;
        format!("c{call_id}")
    };

    for spec in specs {
        for index in 0..spec.tool_count {
            responses.push(call(
                &next_id(),
                &spec.tool_name(index),
                serde_json::json!({}),
            ));
        }
    }
    for spec in specs.iter().filter(|spec| spec.deferred) {
        responses.push(call(
            &next_id(),
            "load_ability",
            serde_json::json!({ "id": spec.id }),
        ));
    }
    for spec in specs.iter().filter(|spec| spec.deferred && !spec.activated) {
        for index in 0..spec.tool_count {
            responses.push(call(
                &next_id(),
                &spec.tool_name(index),
                serde_json::json!({}),
            ));
        }
    }
    responses.push(text("done"));
    responses
}

async fn run_gating_case(specs: Vec<AbilitySpec>) -> Result<(), TestCaseError> {
    let provider = QueueProvider::new(build_gating_script(&specs));
    let mut def = Agent::new(
        Llm::new(provider, "test-model")
            .instructions("root")
            .max_turns(200),
    );
    for spec in &specs {
        let mut draft = ability(spec.id.clone()).prompt(format!("prompt-{}", spec.id));
        for index in 0..spec.tool_count {
            draft = draft.tool(TrackedTool {
                name: spec.tool_name(index),
                marker: format!("ran:{}:{index}", spec.id),
            });
        }
        if spec.deferred {
            draft = draft.describe(format!("desc-{}", spec.id)).deferred();
        }
        def = def.with(draft);
    }
    let activated_ids: Vec<String> = specs
        .iter()
        .filter(|spec| spec.activated)
        .map(|spec| spec.id.clone())
        .collect();
    let composer = Composer::new(def, Runtime::new()).activated(activated_ids);

    let mut agent = composer.build("tenant", "session").await.unwrap();
    let system_prompt = agent.state().system_prompt.clone();

    for spec in &specs {
        let prompt_fragment = format!("prompt-{}", spec.id);
        let catalog_entry = format!("- {}: desc-{}", spec.id, spec.id);
        if spec.live() {
            prop_assert!(
                system_prompt.contains(&prompt_fragment),
                "live ability {} prompt missing from system prompt",
                spec.id
            );
            prop_assert!(
                !system_prompt.contains(&catalog_entry),
                "live ability {} must not be catalogued as deferred",
                spec.id
            );
        } else {
            prop_assert!(
                !system_prompt.contains(&prompt_fragment),
                "deferred ability {} leaked its prompt before load",
                spec.id
            );
            prop_assert!(
                system_prompt.contains(&catalog_entry),
                "deferred ability {} missing from the catalog",
                spec.id
            );
        }
    }

    agent.run("go").await.unwrap();
    let results = tool_result_pairs(&agent);
    let mut index = 0;

    for spec in &specs {
        for tool_index in 0..spec.tool_count {
            let (name, content, is_error) = &results[index];
            prop_assert_eq!(name, &spec.tool_name(tool_index));
            if spec.live() {
                prop_assert!(!is_error, "expected {} to succeed pre-load", name);
                prop_assert_eq!(content, &format!("ran:{}:{tool_index}", spec.id));
            } else {
                prop_assert!(*is_error, "expected {} to be gated pre-load", name);
                prop_assert!(
                    content.contains("unknown tool"),
                    "unexpected gate message for {}: {}",
                    name,
                    content
                );
            }
            index += 1;
        }
    }
    for spec in specs.iter().filter(|spec| spec.deferred) {
        let (name, content, is_error) = &results[index];
        prop_assert_eq!(name, "load_ability");
        prop_assert!(!is_error);
        if spec.activated {
            prop_assert!(
                content.contains("already available"),
                "pre-activated {} should bounce on load, got: {}",
                spec.id,
                content
            );
        } else {
            prop_assert!(
                content.contains(&format!("Ability `{}` loaded", spec.id)),
                "load result missing the 'loaded' banner for {}: {}",
                spec.id,
                content
            );
            prop_assert!(
                content.contains(&format!("prompt-{}", spec.id)),
                "load result missing instructions for {}",
                spec.id
            );
        }
        index += 1;
    }
    for spec in specs.iter().filter(|spec| spec.deferred && !spec.activated) {
        for tool_index in 0..spec.tool_count {
            let (name, content, is_error) = &results[index];
            prop_assert_eq!(name, &spec.tool_name(tool_index));
            prop_assert!(!is_error, "expected {} to succeed post-load", name);
            prop_assert_eq!(content, &format!("ran:{}:{tool_index}", spec.id));
            index += 1;
        }
    }
    prop_assert_eq!(
        index,
        results.len(),
        "unexpected extra or missing tool results"
    );
    Ok(())
}

#[derive(Debug, Clone)]
struct GatedSpec {
    id: String,
    activated: bool,
}

fn gated_specs() -> impl Strategy<Value = Vec<GatedSpec>> {
    prop::collection::vec(any::<bool>(), 1..4).prop_map(|raw| {
        raw.into_iter()
            .enumerate()
            .map(|(index, activated)| GatedSpec {
                id: format!("gs{index}"),
                activated,
            })
            .collect()
    })
}

struct ChildProvider;

#[async_trait]
impl Provider for ChildProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Ok(text("child done"))
    }
}

async fn skill_for(id: &str) -> Arc<SkillSet> {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: skill\ndescription: a gated skill\n---\nthe-content-of-{id}"),
    )
    .unwrap();
    Arc::new(SkillSet::load_dir(id, dir.path()).await)
}

fn worker_def(id: &str) -> Subagent {
    Subagent::new(
        format!("{id}-worker"),
        format!("worker for {id}"),
        Agent::new(
            Llm::new(Arc::new(ChildProvider), "child-model")
                .instructions("you are a worker")
                .max_turns(3),
        ),
    )
}

fn build_live_gate_script(specs: &[GatedSpec]) -> Vec<CompletionResponse> {
    let mut responses = Vec::new();
    let mut call_id = 0u32;
    let mut next_id = || {
        call_id += 1;
        format!("c{call_id}")
    };

    for spec in specs {
        responses.push(call(
            &next_id(),
            "skill_view",
            serde_json::json!({ "name": format!("{}:skill", spec.id) }),
        ));
        responses.push(call(
            &next_id(),
            "delegate",
            serde_json::json!({ "agent": format!("{}-worker", spec.id), "prompt": "go" }),
        ));
    }
    for spec in specs {
        responses.push(call(
            &next_id(),
            "load_ability",
            serde_json::json!({ "id": spec.id }),
        ));
    }
    for spec in specs.iter().filter(|spec| !spec.activated) {
        responses.push(call(
            &next_id(),
            "skill_view",
            serde_json::json!({ "name": format!("{}:skill", spec.id) }),
        ));
        responses.push(call(
            &next_id(),
            "delegate",
            serde_json::json!({ "agent": format!("{}-worker", spec.id), "prompt": "go" }),
        ));
    }
    responses.push(text("done"));
    responses
}

async fn run_live_gate_case(specs: Vec<GatedSpec>) -> Result<(), TestCaseError> {
    let provider = QueueProvider::new(build_live_gate_script(&specs));
    let mut def = Agent::new(
        Llm::new(provider, "test-model")
            .instructions("root")
            .max_turns(200),
    );
    for spec in &specs {
        def = def.with(
            ability(spec.id.clone())
                .describe(format!("desc-{}", spec.id))
                .deferred()
                .skills(skill_for(&spec.id).await)
                .subagent_def(worker_def(&spec.id)),
        );
    }
    let activated_ids: Vec<String> = specs
        .iter()
        .filter(|spec| spec.activated)
        .map(|spec| spec.id.clone())
        .collect();
    let composer = Composer::new(def, Runtime::new()).activated(activated_ids);

    let mut agent = composer.build("tenant", "session").await.unwrap();
    agent.run("go").await.unwrap();
    let results = tool_result_pairs(&agent);
    let mut index = 0;

    for spec in &specs {
        let (skill_name, skill_content, skill_is_error) = &results[index];
        prop_assert_eq!(skill_name, "skill_view");
        index += 1;
        let (delegate_name, delegate_content, delegate_is_error) = &results[index];
        prop_assert_eq!(delegate_name, "delegate");
        index += 1;

        if spec.activated {
            prop_assert!(!skill_is_error, "activated skill blocked for {}", spec.id);
            prop_assert!(
                skill_content.contains(&format!("the-content-of-{}", spec.id)),
                "skill content missing for activated {}: {}",
                spec.id,
                skill_content
            );
            prop_assert!(
                !delegate_is_error,
                "activated delegate blocked for {}",
                spec.id
            );
        } else {
            prop_assert!(*skill_is_error, "unloaded skill not gated for {}", spec.id);
            prop_assert!(
                skill_content.contains("is not available yet"),
                "unexpected skill gate message for {}: {}",
                spec.id,
                skill_content
            );
            prop_assert!(
                *delegate_is_error,
                "unloaded subagent not gated for {}",
                spec.id
            );
            prop_assert!(
                delegate_content.contains("is not available yet"),
                "unexpected delegate gate message for {}: {}",
                spec.id,
                delegate_content
            );
        }
    }

    for spec in &specs {
        let (name, content, is_error) = &results[index];
        prop_assert_eq!(name, "load_ability");
        prop_assert!(!is_error);
        if spec.activated {
            prop_assert!(
                content.contains("already available"),
                "pre-activated {} should bounce on load, got: {}",
                spec.id,
                content
            );
        } else {
            prop_assert!(
                content.contains(&format!("Ability `{}` loaded", spec.id)),
                "load result missing the 'loaded' banner for {}: {}",
                spec.id,
                content
            );
        }
        index += 1;
    }

    for spec in specs.iter().filter(|spec| !spec.activated) {
        let (skill_name, skill_content, skill_is_error) = &results[index];
        prop_assert_eq!(skill_name, "skill_view");
        prop_assert!(
            !skill_is_error,
            "skill still gated after load for {}",
            spec.id
        );
        prop_assert!(
            skill_content.contains(&format!("the-content-of-{}", spec.id)),
            "skill content missing after load for {}: {}",
            spec.id,
            skill_content
        );
        index += 1;

        let (delegate_name, _delegate_content, delegate_is_error) = &results[index];
        prop_assert_eq!(delegate_name, "delegate");
        prop_assert!(
            !delegate_is_error,
            "delegate still gated after load for {}",
            spec.id
        );
        index += 1;
    }

    prop_assert_eq!(
        index,
        results.len(),
        "unexpected extra or missing tool results"
    );
    Ok(())
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, max_shrink_iters: 4000, ..ProptestConfig::default() })]

    #[test]
    fn tool_gating_catalog_and_prompt_match_the_model(specs in ability_specs()) {
        rt().block_on(run_gating_case(specs))?;
    }

    #[test]
    fn skill_and_subagent_gating_is_live_across_random_abilities(specs in gated_specs()) {
        rt().block_on(run_live_gate_case(specs))?;
    }
}
