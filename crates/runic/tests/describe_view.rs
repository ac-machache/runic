use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use proptest::prelude::*;
use runic::Llm;
use runic::ability::ability;
use runic::composer::{Agent, Composer};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_tool::{Tool, ToolContext, ToolResult};

struct QueueProvider {
    responses: Mutex<VecDeque<CompletionResponse>>,
}

impl QueueProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(VecDeque::new()),
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

struct TrackedTool {
    name: String,
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
        Ok(ToolResult::ok("ok"))
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

fn build_composer(specs: &[AbilitySpec]) -> Composer {
    let mut agent = Agent::new(Llm::new(QueueProvider::new(), "test-model").instructions("root"));
    for spec in specs {
        let mut draft = ability(spec.id.clone()).prompt(format!("prompt-{}", spec.id));
        for index in 0..spec.tool_count {
            draft = draft.tool(TrackedTool {
                name: spec.tool_name(index),
            });
        }
        if spec.deferred {
            draft = draft.describe(format!("desc-{}", spec.id)).deferred();
        }
        agent = agent.with(draft);
    }
    let activated_ids: Vec<String> = specs
        .iter()
        .filter(|spec| spec.activated)
        .map(|spec| spec.id.clone())
        .collect();
    Composer::new(agent).activated(activated_ids)
}

async fn run_case(specs: Vec<AbilitySpec>) -> Result<(), TestCaseError> {
    let composer = build_composer(&specs);

    let views = composer.describe("tenant", "session").await.unwrap();
    prop_assert_eq!(views.len(), specs.len());

    for (view, spec) in views.iter().zip(specs.iter()) {
        prop_assert_eq!(&view.name, &spec.id);
        prop_assert_eq!(view.id.as_deref(), Some(spec.id.as_str()));
        prop_assert_eq!(view.deferred, spec.deferred);
        prop_assert_eq!(view.activated, spec.live());
        prop_assert_eq!(view.tools.len(), spec.tool_count);
        for (index, tool) in view.tools.iter().enumerate() {
            prop_assert_eq!(&tool.name, &spec.tool_name(index));
        }
        if spec.deferred {
            let expected = format!("desc-{}", spec.id);
            prop_assert_eq!(view.description.as_deref(), Some(expected.as_str()));
        }
    }

    let agent = composer.build("tenant", "session").await.unwrap();
    let live_tool_names: std::collections::HashSet<String> = agent
        .tool_specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect();

    for view in &views {
        for tool in &view.tools {
            if view.activated {
                prop_assert!(
                    live_tool_names.contains(&tool.name),
                    "describe() reports {} as activated but its tool {} is not resolvable on the built agent",
                    view.name,
                    tool.name
                );
            } else {
                prop_assert!(
                    !live_tool_names.contains(&tool.name),
                    "describe() reports {} as NOT activated but its tool {} IS resolvable on the built agent",
                    view.name,
                    tool.name
                );
            }
        }
    }
    Ok(())
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, max_shrink_iters: 2000, ..ProptestConfig::default() })]

    #[test]
    fn describe_matches_what_build_actually_produces(specs in ability_specs()) {
        rt().block_on(run_case(specs))?;
    }
}
