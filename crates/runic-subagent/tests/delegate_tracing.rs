//! Asserts the `delegate` span shows up around a delegated call, carrying the
//! child session id and outcome fields.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_state::{AgentEvent, Emitter, SubRun, SubSession};
use runic_subagent::{DelegateTool, Subagent, SubagentBuilder, SubagentReq};
use runic_tool::{Tool, ToolContext};
use runic_types::{ContentBlock, StopReason, TokenUsage};
use tracing_subscriber::fmt::format::FmtSpan;

struct OneShot(String);

#[async_trait]
impl Provider for OneShot {
    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: self.0.clone(),
                provider_metadata: None,
            }],
            stop_reason: StopReason::EndTurn,
            tool_calls: vec![],
            usage: TokenUsage::default(),
        })
    }
}

struct FakeBuilder;

#[async_trait]
impl SubagentBuilder for FakeBuilder {
    async fn provider(&self, req: &SubagentReq<'_>) -> Arc<dyn Provider> {
        Arc::new(OneShot(format!("done: {}", req.subagent.name)))
    }

    fn default_model(&self, _req: &SubagentReq<'_>) -> String {
        "test".to_string()
    }
}

#[derive(Debug)]
struct NoopEmitter;

impl Emitter for NoopEmitter {
    fn emit(&self, _event: AgentEvent) {}
}

struct FakeSubSession;

#[async_trait]
impl SubSession for FakeSubSession {
    async fn begin(&self, agent: &str) -> anyhow::Result<Box<dyn SubRun>> {
        Ok(Box::new(FakeSubRun(format!("child-of-{agent}"))))
    }
}

struct FakeSubRun(String);

#[async_trait]
impl SubRun for FakeSubRun {
    fn session_id(&self) -> &str {
        &self.0
    }

    fn emitter(&self) -> Arc<dyn Emitter> {
        Arc::new(NoopEmitter)
    }

    fn nested(&self) -> Arc<dyn SubSession> {
        Arc::new(FakeSubSession)
    }

    async fn flush(&self) -> anyhow::Result<()> {
        Ok(())
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
async fn delegate_span_carries_the_child_session_and_outcome() {
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_span_events(FmtSpan::CLOSE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();

    let delegate = DelegateTool::with_builder(
        vec![Subagent::new("reviewer", "reviews").prompt("Review things.")],
        Arc::new(FakeBuilder),
    );
    let ctx = ToolContext::new("u", "s", "r").with_sub_session(Some(Arc::new(FakeSubSession)));

    let _guard = tracing::subscriber::set_default(subscriber);

    let result = delegate
        .execute(
            serde_json::json!({ "action": "delegate", "agent": "reviewer", "prompt": "go" }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!result.is_error());

    let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    for expected in [
        "delegate{agent=reviewer mode=sync depth=",
        "child_session=\"child-of-reviewer\"",
        "status=\"ok\"",
    ] {
        assert!(
            output.contains(expected),
            "missing `{expected}` in trace output:\n{output}"
        );
    }
    assert!(
        !output.contains("otel.status_code=\"ERROR\""),
        "a successful delegation should not mark otel.status_code=ERROR:\n{output}"
    );
}
