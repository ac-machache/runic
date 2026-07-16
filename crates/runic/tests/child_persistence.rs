use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::ability::subagent;
use runic::composer::Composer;
use runic_agent::RunContext;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_state::{
    ChildPersistence, ChildPersistenceHandle, ChildPersistenceStatus, ChildSink, PersistSink,
    SessionEvent,
};
use runic_types::{ContentBlock, StopReason, TokenUsage, ToolCall};
use tokio::sync::mpsc;

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
    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::Parse("scripted provider exhausted".into()))
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

fn delegate_to(agent: &str) -> CompletionResponse {
    let input = serde_json::json!({ "agent": agent, "prompt": "go" });
    CompletionResponse {
        content: vec![ContentBlock::ToolUse {
            id: "c1".into(),
            name: "delegate".into(),
            input: input.clone(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "delegate".into(),
            input,
        }],
        usage: TokenUsage::default(),
    }
}

struct FakeInner {
    fail_begin: bool,
    fail_flush: bool,
    counter: AtomicUsize,
    sinks: Mutex<Vec<Arc<FakeSink>>>,
}

struct FakeHandle(Arc<FakeInner>);

struct FakeSink {
    inner: Arc<FakeInner>,
    session_id: String,
    agent: String,
    sink: PersistSink,
    rx: Mutex<mpsc::UnboundedReceiver<Arc<SessionEvent>>>,
    events: Mutex<Vec<SessionEvent>>,
    flushed: AtomicBool,
}

impl FakeInner {
    fn handle(self: &Arc<Self>) -> ChildPersistenceHandle {
        ChildPersistenceHandle(Arc::new(FakeHandle(self.clone())))
    }
}

#[async_trait]
impl ChildPersistence for FakeHandle {
    async fn begin(&self, agent: &str) -> anyhow::Result<Box<dyn ChildSink>> {
        if self.0.fail_begin {
            anyhow::bail!("store down");
        }
        let nth = self.0.counter.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::unbounded_channel();
        let sink = Arc::new(FakeSink {
            inner: self.0.clone(),
            session_id: format!("fake-{nth}"),
            agent: agent.to_string(),
            sink: PersistSink::new(tx),
            rx: Mutex::new(rx),
            events: Mutex::new(Vec::new()),
            flushed: AtomicBool::new(false),
        });
        self.0.sinks.lock().unwrap().push(sink.clone());
        Ok(Box::new(SharedSink(sink)))
    }
}

struct SharedSink(Arc<FakeSink>);

#[async_trait]
impl ChildSink for SharedSink {
    fn session_id(&self) -> &str {
        &self.0.session_id
    }

    fn sink(&self) -> PersistSink {
        self.0.sink.clone()
    }

    fn nested(&self) -> ChildPersistenceHandle {
        self.0.inner.handle()
    }

    async fn flush(&self) -> anyhow::Result<()> {
        let mut rx = self.0.rx.lock().unwrap();
        while let Ok(event) = rx.try_recv() {
            self.0.events.lock().unwrap().push((*event).clone());
        }
        if self.0.inner.fail_flush {
            anyhow::bail!("flush blew up");
        }
        self.0.flushed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

fn persistence(fail_begin: bool, fail_flush: bool) -> Arc<FakeInner> {
    Arc::new(FakeInner {
        fail_begin,
        fail_flush,
        counter: AtomicUsize::new(0),
        sinks: Mutex::new(Vec::new()),
    })
}

async fn run_delegation(fake: &Arc<FakeInner>) -> runic_agent::Agent {
    let child = ScriptedProvider::new(vec![text("child done")]);
    let main_provider = ScriptedProvider::new(vec![delegate_to("sub-a"), text("done")]);
    let mut agent = Composer::new(main_provider, "main-model")
        .with(
            subagent("sub-a", "a expert")
                .prompt("you are a")
                .provider(child),
        )
        .build("alice", "s1")
        .await
        .unwrap();
    agent
        .run_with(
            "start",
            RunContext::new().with_child_persistence(fake.handle()),
        )
        .await
        .unwrap();
    agent
}

fn edges(
    agent: &runic_agent::Agent,
) -> (
    Option<String>,
    Option<String>,
    Option<ChildPersistenceStatus>,
) {
    let mut started_child = None;
    let mut finished_child = None;
    let mut finished_persistence = None;
    for event in agent.state().events() {
        match event {
            SessionEvent::DelegationStarted { child_session, .. } => {
                started_child = child_session.clone();
            }
            SessionEvent::DelegationFinished {
                child_session,
                child_persistence,
                ..
            } => {
                finished_child = child_session.clone();
                finished_persistence = child_persistence.clone();
            }
            _ => {}
        }
    }
    (started_child, finished_child, finished_persistence)
}

#[tokio::test]
async fn a_persisted_delegation_writes_the_child_transcript_under_an_opaque_id() {
    let fake = persistence(false, false);
    let agent = run_delegation(&fake).await;

    let sinks = fake.sinks.lock().unwrap();
    assert_eq!(sinks.len(), 1);
    let sink = &sinks[0];
    assert_eq!(sink.session_id, "fake-0");
    assert_eq!(sink.agent, "sub-a");
    assert!(sink.flushed.load(Ordering::SeqCst));

    let child_events = sink.events.lock().unwrap();
    assert!(
        child_events
            .iter()
            .any(|e| matches!(e, SessionEvent::RunStart { .. })),
        "the child transcript starts with its RunStart"
    );
    assert!(
        child_events
            .iter()
            .any(|e| matches!(e, SessionEvent::RunEnd { .. })),
        "the child transcript ends with its RunEnd"
    );

    let (started, finished, persistence_status) = edges(&agent);
    assert_eq!(started.as_deref(), Some("fake-0"));
    assert_eq!(finished.as_deref(), Some("fake-0"));
    assert_eq!(persistence_status, Some(ChildPersistenceStatus::Flushed));
}

#[tokio::test]
async fn begin_failure_degrades_to_an_ephemeral_child_with_honest_edges() {
    let fake = persistence(true, false);
    let agent = run_delegation(&fake).await;

    assert!(fake.sinks.lock().unwrap().is_empty());
    let (started, finished, persistence_status) = edges(&agent);
    assert_eq!(started, None, "no child_session claimed when begin failed");
    assert_eq!(finished, None);
    assert_eq!(persistence_status, None);
}

#[tokio::test]
async fn flush_failure_is_visible_on_the_finished_edge() {
    let fake = persistence(false, true);
    let agent = run_delegation(&fake).await;

    let (started, finished, persistence_status) = edges(&agent);
    assert_eq!(started.as_deref(), Some("fake-0"));
    assert_eq!(finished.as_deref(), Some("fake-0"));
    assert!(
        matches!(
            persistence_status,
            Some(ChildPersistenceStatus::FlushFailed(ref e)) if e.contains("flush blew up")
        ),
        "got {persistence_status:?}"
    );
}

#[tokio::test]
async fn every_delegation_attempt_gets_a_fresh_child_session() {
    let fake = persistence(false, false);
    run_delegation(&fake).await;
    run_delegation(&fake).await;

    let sinks = fake.sinks.lock().unwrap();
    assert_eq!(sinks.len(), 2);
    assert_ne!(
        sinks[0].session_id, sinks[1].session_id,
        "a retried delegation never appends to the previous child's transcript"
    );
}
