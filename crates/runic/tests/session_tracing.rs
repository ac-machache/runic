use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::{Agent, Llm};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_substrate::{MemorySessionStore, SessionStore};
use runic_types::{ContentBlock, StopReason, TokenUsage};
use tracing_subscriber::fmt::format::FmtSpan;

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
async fn session_run_carries_session_and_hydrate_spans() {
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_span_events(FmtSpan::CLOSE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();

    let provider = ScriptedProvider::new(vec![text("first"), text("second")]);
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let agent = Agent::new(Llm::new(provider, "test-model"));

    let _guard = tracing::subscriber::set_default(subscriber);

    runic::session(("tenant", "thread-1"))
        .store(store.clone())
        .run(&agent, "first message")
        .await
        .unwrap();
    runic::session(("tenant", "thread-1"))
        .store(store.clone())
        .run(&agent, "second message")
        .await
        .unwrap();

    let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();

    for expected in [
        "session_run{tenant=tenant thread=thread-1",
        "hydrate{tenant=tenant thread=thread-1",
        "persist_backlog_at_flush=",
        "flush_ms=",
        "run{run_id=",
    ] {
        assert!(
            output.contains(expected),
            "missing `{expected}` in trace output:\n{output}"
        );
    }

    let last_hydrate = output.rfind("hydrate{").expect("a hydrate span appears");
    assert!(
        !output[last_hydrate..].contains("events=0"),
        "the second run's hydrate span should report a non-zero folded-event count:\n{output}"
    );
}
