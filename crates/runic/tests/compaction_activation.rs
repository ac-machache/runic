use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use proptest::prelude::*;
use runic::ability::ability;
use runic::composer::{Agent, Composer, Runtime};
use runic::deferred::{ability_activated_key, activated_ability_ids};
use runic::{Compaction, Llm};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_state::SessionEvent;
use runic_tool::{Tool, ToolContext, ToolResult, activated_key};
use runic_types::{ContentBlock, Message, MessageContent, StopReason, TokenUsage, ToolCall};

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

fn state_flag(agent: &runic_agent::Session, key: &str) -> bool {
    agent
        .state()
        .data()
        .get(key)
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn tool_result_pairs(agent: &runic_agent::Session) -> Vec<(String, String, bool)> {
    agent
        .state()
        .events()
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Message { msg, .. } => Some(msg),
            _ => None,
        })
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

fn push_filler(agent: &mut runic_agent::Session, count: usize) {
    for _ in 0..count {
        agent.state_mut().push_event(SessionEvent::Message {
            run_id: "filler".into(),
            msg: Message::user("x".repeat(600)),
            at: chrono::Utc::now(),
        });
    }
}

async fn run_survival_case(ability_count: usize) -> Result<(), TestCaseError> {
    let ids: Vec<String> = (0..ability_count).map(|i| format!("ab{i}")).collect();
    let tool_names: Vec<String> = ids.iter().map(|id| format!("{id}_tool")).collect();

    let mut responses = Vec::new();
    let mut call_id = 0u32;
    let mut next_id = || {
        call_id += 1;
        format!("c{call_id}")
    };
    for id in &ids {
        responses.push(call(
            &next_id(),
            "load_ability",
            serde_json::json!({ "id": id }),
        ));
    }
    responses.push(text("all loaded"));
    responses.push(text("Summary: abilities loaded, work in progress."));
    responses.push(text("after compaction"));
    for name in &tool_names {
        responses.push(call(&next_id(), name, serde_json::json!({})));
    }
    responses.push(text("post-compaction tools done"));

    let provider = QueueProvider::new(responses);
    let mut def = Agent::new(
        Llm::new(provider.clone(), "test-model")
            .instructions("root")
            .max_turns(200),
    );
    for (id, tool_name) in ids.iter().zip(tool_names.iter()) {
        def = def.with(
            ability(id.clone())
                .describe(format!("desc-{id}"))
                .deferred()
                .prompt(format!("rules-{id}"))
                .tool(TrackedTool {
                    name: tool_name.clone(),
                    marker: format!("ran:{id}"),
                }),
        );
    }

    let composer = Composer::new(
        def,
        Runtime::new().hook(
            Compaction::new(Llm::new(provider, "test-model"))
                .max_context_tokens(2000)
                .keep_recent(3),
        ),
    );
    let mut agent = composer.build("tenant", "session").await.unwrap();

    agent.run("load everything").await.unwrap();
    for id in &ids {
        prop_assert!(
            state_flag(&agent, &ability_activated_key(id)),
            "ability {} not marked activated right after load",
            id
        );
    }
    for name in &tool_names {
        prop_assert!(
            state_flag(&agent, &activated_key(name)),
            "tool {} not marked activated right after load",
            name
        );
    }

    push_filler(&mut agent, 15);
    let messages_before = agent.state().events().len();

    agent.run("trigger compaction").await.unwrap();

    let compacted = agent.state().events().len() < messages_before;
    prop_assert!(compacted, "filler did not actually trigger compaction");

    for id in &ids {
        prop_assert!(
            state_flag(&agent, &ability_activated_key(id)),
            "ability {} lost its activation flag after compaction",
            id
        );
    }
    for name in &tool_names {
        prop_assert!(
            state_flag(&agent, &activated_key(name)),
            "tool {} lost its activation flag after compaction",
            name
        );
    }

    agent.run("use the tools").await.unwrap();
    let results = tool_result_pairs(&agent);
    for (id, name) in ids.iter().zip(tool_names.iter()) {
        let succeeded = results.iter().any(|(result_name, content, is_error)| {
            result_name == name && !is_error && content == &format!("ran:{id}")
        });
        prop_assert!(
            succeeded,
            "tool {} for ability {} did not execute after compaction",
            name,
            id
        );
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
    #![proptest_config(ProptestConfig { cases: 32, max_shrink_iters: 2000, ..ProptestConfig::default() })]

    #[test]
    fn activated_ability_and_tool_flags_survive_compaction(ability_count in 1usize..4) {
        rt().block_on(run_survival_case(ability_count))?;
    }
}

#[tokio::test]
async fn a_rebuild_after_compaction_restores_full_ability_fidelity() {
    let responses = vec![
        call("c1", "load_ability", serde_json::json!({ "id": "billing" })),
        text("loaded"),
        text("Summary: billing was loaded and is in effect."),
        text("after compaction"),
    ];
    let provider = QueueProvider::new(responses);
    let mut def = Agent::new(
        Llm::new(provider.clone(), "test-model")
            .instructions("root")
            .max_turns(200),
    );
    def = def.with(
        ability("billing")
            .describe("invoices and refunds")
            .deferred()
            .prompt("billing rules")
            .tool(TrackedTool {
                name: "refund".to_string(),
                marker: "ran:billing".to_string(),
            }),
    );

    let composer = Composer::new(
        def,
        Runtime::new().hook(
            Compaction::new(Llm::new(provider, "test-model"))
                .max_context_tokens(2000)
                .keep_recent(3),
        ),
    );
    let mut agent = composer.build("tenant", "session").await.unwrap();
    agent.run("load billing").await.unwrap();
    assert!(state_flag(&agent, &ability_activated_key("billing")));

    push_filler(&mut agent, 15);
    let before = agent.state().events().len();
    agent.run("trigger compaction").await.unwrap();
    assert!(
        agent.state().events().len() < before,
        "filler did not trigger compaction"
    );

    let data = agent.state().data().clone();
    let ids = activated_ability_ids(&data);
    assert_eq!(ids, vec!["billing".to_string()]);

    let rebuild_provider = QueueProvider::new(vec![
        call("c1", "refund", serde_json::json!({})),
        text("done"),
    ]);
    let mut rebuilt = Composer::new(
        Agent::new(Llm::new(rebuild_provider, "test-model").instructions("root")).with(
            ability("billing")
                .describe("invoices and refunds")
                .deferred()
                .prompt("billing rules")
                .tool(TrackedTool {
                    name: "refund".to_string(),
                    marker: "ran:billing".to_string(),
                }),
        ),
        Runtime::new(),
    )
    .activated(ids)
    .build("tenant", "session")
    .await
    .unwrap();

    let rebuilt_prompt = rebuilt.state().system_prompt.clone();
    assert!(
        rebuilt_prompt.contains("billing rules"),
        "a rebuild seeded from the post-compaction activation ids must re-merge the ability's \
         instructions fresh, even though the original 'Ability loaded' tool result may have \
         been folded into the compaction summary rather than kept verbatim"
    );
    assert!(!rebuilt_prompt.contains("<deferred-abilities>"));

    rebuilt.run("refund order 7").await.unwrap();
    let results = tool_result_pairs(&rebuilt);
    assert!(
        results
            .iter()
            .any(|(name, content, is_error)| name == "refund"
                && !is_error
                && content == "ran:billing"),
        "refund must be immediately callable on the rebuilt agent, no load_ability needed"
    );
}
