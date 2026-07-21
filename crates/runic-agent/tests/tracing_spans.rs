use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic_agent::{RunContext, Runner};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, StopReason, TokenUsage, ToolCall};
use tracing_subscriber::fmt::format::FmtSpan;

struct ScriptedProvider {
    responses: Mutex<std::collections::VecDeque<CompletionResponse>>,
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Ok(self.responses.lock().unwrap().pop_front().unwrap())
    }
}

struct Echo;

#[async_trait]
impl Tool for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "echoes"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok("echoed"))
    }
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn the_span_tree_carries_the_agreed_fields() {
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_span_events(FmtSpan::CLOSE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();

    let provider = Arc::new(ScriptedProvider {
        responses: Mutex::new(
            vec![
                CompletionResponse {
                    content: vec![],
                    stop_reason: StopReason::ToolUse,
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "echo".into(),
                        input: serde_json::json!({}),
                    }],
                    usage: TokenUsage {
                        input_tokens: 100,
                        output_tokens: 20,
                        ..Default::default()
                    },
                },
                CompletionResponse {
                    content: vec![ContentBlock::Text {
                        text: "done".into(),
                        provider_metadata: None,
                    }],
                    stop_reason: StopReason::EndTurn,
                    tool_calls: vec![],
                    usage: TokenUsage {
                        input_tokens: 120,
                        output_tokens: 8,
                        ..Default::default()
                    },
                },
            ]
            .into(),
        ),
    });

    let _guard = tracing::subscriber::set_default(subscriber);
    let mut agent = Runner::builder(provider, "alice", "s1")
        .model("test-model")
        .tool(Arc::new(Echo))
        .build();
    agent
        .run_with("go", RunContext::new().with_run_id("r-traced"))
        .await
        .unwrap();

    let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();

    for expected in [
        "run{run_id=r-traced tenant=alice thread=s1 mode=\"direct\"",
        "total_turns=2",
        "stop_reason=\"end_turn\"",
        "turn{n=1}",
        "turn{n=2}",
        "provider_call{model=test-model",
        "input_tokens=100 output_tokens=20 stop_reason=\"tool_use\"",
        "dispatch{batch=1",
        "tool{name=echo call_id=c1 parallel=false",
        "outcome=\"ok\"",
        "is_error=false",
    ] {
        assert!(
            output.contains(expected),
            "missing `{expected}` in trace output:\n{output}"
        );
    }
}
