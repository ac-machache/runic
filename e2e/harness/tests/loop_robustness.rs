use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use proptest::prelude::*;
use runic_agent::Agent;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, StopReason, TokenUsage, ToolCall};

const MAX_TURNS: u32 = 6;

#[derive(Debug, Clone)]
enum Step {
    Text,
    Call(String),
    ProviderError,
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => Just(Step::Text),
        7 => prop_oneof![
            Just("ok"),
            Just("boom"),
            Just("err"),
            Just("huge"),
            Just("ghost"),
            Just("slow"),
        ].prop_map(|s| Step::Call(s.to_string())),
        1 => Just(Step::ProviderError),
    ]
}

struct ScriptProvider {
    steps: Mutex<VecDeque<Step>>,
    call_seq: AtomicU64,
}

impl ScriptProvider {
    fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: Mutex::new(steps.into()),
            call_seq: AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl Provider for ScriptProvider {
    fn name(&self) -> &str {
        "script"
    }

    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let next = self.steps.lock().unwrap().pop_front();
        match next {
            Some(Step::ProviderError) => {
                Err(ProviderError::Parse("scripted provider error".into()))
            }
            Some(Step::Call(name)) => {
                let id = format!("c{}", self.call_seq.fetch_add(1, Ordering::SeqCst));
                Ok(CompletionResponse {
                    content: vec![ContentBlock::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: serde_json::json!({}),
                        provider_metadata: None,
                    }],
                    stop_reason: StopReason::ToolUse,
                    tool_calls: vec![ToolCall {
                        id,
                        name,
                        input: serde_json::json!({}),
                    }],
                    usage: TokenUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                })
            }
            _ => Ok(CompletionResponse {
                content: vec![ContentBlock::Text {
                    text: "done".into(),
                    provider_metadata: None,
                }],
                stop_reason: StopReason::EndTurn,
                tool_calls: vec![],
                usage: TokenUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            }),
        }
    }
}

macro_rules! toy_tool {
    ($ty:ident, $name:literal, $body:expr) => {
        struct $ty;
        #[async_trait]
        impl Tool for $ty {
            fn name(&self) -> &str {
                $name
            }
            fn description(&self) -> &str {
                $name
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({ "type": "object" })
            }
            async fn execute(
                &self,
                _args: serde_json::Value,
                _ctx: &ToolContext,
            ) -> anyhow::Result<ToolResult> {
                $body
            }
        }
    };
}

toy_tool!(OkTool, "ok", Ok(ToolResult::ok("fine")));
toy_tool!(
    ErrTool,
    "err",
    Ok(ToolResult::error("tool failed on purpose"))
);
toy_tool!(HugeTool, "huge", Ok(ToolResult::ok("x".repeat(1_000_000))));
toy_tool!(BoomTool, "boom", panic!("intentional tool panic"));

struct SlowTool;
#[async_trait]
impl Tool for SlowTool {
    fn name(&self) -> &str {
        "slow"
    }
    fn description(&self) -> &str {
        "slow"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        tokio::time::sleep(Duration::from_millis(5)).await;
        Ok(ToolResult::ok("slept"))
    }
}

async fn run_script(steps: Vec<Step>) -> Result<(), TestCaseError> {
    let provider = Arc::new(ScriptProvider::new(steps));
    let mut agent = Agent::builder(provider, "tenant", "session")
        .model("m")
        .system_prompt("robustness")
        .max_turns(MAX_TURNS)
        .tool(Arc::new(OkTool))
        .tool(Arc::new(ErrTool))
        .tool(Arc::new(HugeTool))
        .tool(Arc::new(BoomTool))
        .tool(Arc::new(SlowTool))
        .build();

    let outcome = tokio::time::timeout(Duration::from_secs(10), agent.run("go")).await;
    prop_assert!(outcome.is_ok(), "agent loop hung (never terminated)");

    if let Ok(o) = outcome.unwrap() {
        prop_assert!(
            o.total_turns <= MAX_TURNS,
            "loop ran {} turns, over the {MAX_TURNS} cap",
            o.total_turns
        );
    }

    let dangling = agent.state().events().iter().any(|e| {
        matches!(e, runic_state::SessionEvent::Message { msg, .. }
            if matches!(&msg.content, runic_types::MessageContent::Blocks(b)
                if b.iter().any(|x| matches!(x, ContentBlock::ToolUse { id, .. }
                    if !has_result(agent.state().events(), id)))))
    });
    prop_assert!(!dangling, "a tool call was left without a result");
    Ok(())
}

fn has_result(events: &[runic_state::SessionEvent], call_id: &str) -> bool {
    events.iter().any(|e| {
        matches!(e, runic_state::SessionEvent::Message { msg, .. }
            if matches!(&msg.content, runic_types::MessageContent::Blocks(b)
                if b.iter().any(|x| matches!(x, ContentBlock::ToolResult { tool_use_id, .. }
                    if tool_use_id == call_id))))
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, max_shrink_iters: 4000, ..ProptestConfig::default() })]

    #[test]
    fn the_loop_never_panics_and_always_terminates(steps in prop::collection::vec(step(), 0..14)) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(run_script(steps))?;
    }
}
