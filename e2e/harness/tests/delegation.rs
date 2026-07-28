use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use proptest::prelude::*;
use runic_agent::Runner;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic::Llm;
use runic::ability::ability;
use runic::composer::Agent;
use runic::subagent::{DelegateTool, Subagent};
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, Message, MessageContent, Role, StopReason, TokenUsage, ToolCall};

fn text(t: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::Text {
            text: t.into(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::EndTurn,
        tool_calls: vec![],
        usage: TokenUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..TokenUsage::default()
        },
    }
}

fn tool_call(id: String, name: &str, input: serde_json::Value) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::ToolUse {
            id: id.clone(),
            name: name.into(),
            input: input.clone(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id,
            name: name.into(),
            input,
        }],
        usage: TokenUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..TokenUsage::default()
        },
    }
}

fn last_user_text(req: &CompletionRequest) -> String {
    req.messages
        .iter()
        .rev()
        .find_map(|m| match (&m.role, &m.content) {
            (Role::User, MessageContent::Text(t)) => Some(t.clone()),
            (Role::User, MessageContent::Blocks(b)) => b.iter().find_map(|x| match x {
                ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            }),
            _ => None,
        })
        .unwrap_or_default()
}

fn just_ran_tool(req: &CompletionRequest) -> bool {
    req.messages.last().is_some_and(|m| {
        matches!(&m.content, MessageContent::Blocks(b)
            if b.iter().any(|x| matches!(x, ContentBlock::ToolResult { .. })))
    })
}

struct ChildProvider {
    seq: AtomicU64,
}

#[async_trait]
impl Provider for ChildProvider {
    fn name(&self) -> &str {
        "child"
    }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        if just_ran_tool(&req) {
            return Ok(text("child done"));
        }
        let prompt = last_user_text(&req);
        let id = format!("k{}", self.seq.fetch_add(1, Ordering::SeqCst));
        if prompt.contains("boom") {
            Ok(tool_call(id, "cboom", serde_json::json!({})))
        } else if prompt.contains("err") {
            Ok(tool_call(id, "cerr", serde_json::json!({})))
        } else {
            Ok(text(&format!("child: {prompt}")))
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
                _a: serde_json::Value,
                _c: &ToolContext,
            ) -> anyhow::Result<ToolResult> {
                $body
            }
        }
    };
}

toy_tool!(COkTool, "cok", Ok(ToolResult::ok("ok")));
toy_tool!(CErrTool, "cerr", Ok(ToolResult::error("child tool error")));
toy_tool!(CBoomTool, "cboom", panic!("child tool panic"));


fn roster() -> Vec<Subagent> {
    vec![Subagent::new(
        "worker",
        "a worker subagent",
        Agent::new(
            Llm::new(
                Arc::new(ChildProvider {
                    seq: AtomicU64::new(0),
                }),
                "child-model",
            )
            .instructions("you are a worker")
            .max_turns(4),
        )
        .with(
            ability("child-tools")
                .tool(COkTool)
                .tool(CErrTool)
                .tool(CBoomTool),
        ),
    )]
}

#[derive(Debug, Clone)]
struct ParentStep {
    agent: String,
    prompt: String,
}

fn parent_step() -> impl Strategy<Value = ParentStep> {
    (
        prop_oneof![Just("worker"), Just("ghost")],
        prop_oneof![Just("say:hi"), Just("boom"), Just("err"), Just("recurse")],
    )
        .prop_map(|(a, p)| ParentStep {
            agent: a.to_string(),
            prompt: p.to_string(),
        })
}

struct ParentProvider {
    steps: Mutex<VecDeque<ParentStep>>,
    seq: AtomicU64,
}

#[async_trait]
impl Provider for ParentProvider {
    fn name(&self) -> &str {
        "parent"
    }
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let next = self.steps.lock().unwrap().pop_front();
        match next {
            Some(step) => {
                let id = format!("p{}", self.seq.fetch_add(1, Ordering::SeqCst));
                Ok(tool_call(
                    id,
                    "delegate",
                    serde_json::json!({
                        "action": "delegate",
                        "agent": step.agent,
                        "prompt": step.prompt,
                    }),
                ))
            }
            None => Ok(text("all delegated")),
        }
    }
}

async fn run_delegations(steps: Vec<ParentStep>) -> Result<(), TestCaseError> {
    let provider = Arc::new(ParentProvider {
        steps: Mutex::new(steps.into()),
        seq: AtomicU64::new(0),
    });
    let delegate = DelegateTool::new(roster());
    let mut agent = Runner::builder(provider, "tenant", "session")
        .model("parent-model")
        .system_prompt("parent")
        .max_turns(12)
        .tool(Arc::new(delegate))
        .build();

    let outcome = tokio::time::timeout(Duration::from_secs(10), agent.run("go")).await;
    prop_assert!(outcome.is_ok(), "delegation loop hung");
    if let Ok(o) = outcome.unwrap() {
        prop_assert!(
            o.total_turns <= 12,
            "parent exceeded turn cap: {}",
            o.total_turns
        );
    }

    let dangling = agent.state().messages_for_provider().iter().any(|msg| {
        matches!(&msg.content, MessageContent::Blocks(b)
            if b.iter().any(|x| matches!(x, ContentBlock::ToolUse { id, .. }
                if !has_result(agent.state().messages_for_provider(), id))))
    });
    prop_assert!(!dangling, "a delegate call was left without a result");
    Ok(())
}

fn has_result(messages: &[Message], call_id: &str) -> bool {
    messages.iter().any(|msg| {
        matches!(&msg.content, MessageContent::Blocks(b)
            if b.iter().any(|x| matches!(x, ContentBlock::ToolResult { tool_use_id, .. }
                if tool_use_id == call_id)))
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, max_shrink_iters: 3000, ..ProptestConfig::default() })]

    #[test]
    fn delegation_never_panics_respects_budget_and_terminates(
        steps in prop::collection::vec(parent_step(), 0..12)
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(run_delegations(steps))?;
    }
}
