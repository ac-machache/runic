use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;

use runic_agent::AgentBuilder;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_state::{ChildPersistence, ChildPersistenceHandle, ChildSink, PersistSink, SessionEvent};
use runic_subagent::{AgentDef, AgentRoster, DelegateTool, SubagentBuilder, SubagentReq};
use runic_tool::{Tool, ToolContext};
use runic_types::{ContentBlock, StopReason, TokenUsage, ToolCall};
use tokio::sync::mpsc;

struct ScriptedProvider {
    responses: Mutex<VecDeque<CompletionResponse>>,
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

fn delegate_to(agent: &str) -> CompletionResponse {
    let input = serde_json::json!({ "agent": agent, "prompt": "go deeper" });
    CompletionResponse {
        content: vec![ContentBlock::ToolUse {
            id: "cc1".into(),
            name: "delegate".into(),
            input: input.clone(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: "cc1".into(),
            name: "delegate".into(),
            input,
        }],
        usage: TokenUsage::default(),
    }
}

struct NestedBuilder {
    roster: Arc<AgentRoster>,
    me: Weak<NestedBuilder>,
}

#[async_trait]
impl SubagentBuilder for NestedBuilder {
    async fn provider(&self, req: &SubagentReq<'_>) -> Arc<dyn Provider> {
        let responses = match req.def.name.as_str() {
            "mid" => vec![delegate_to("leaf"), text("mid done")],
            _ => vec![text("leaf done")],
        };
        Arc::new(ScriptedProvider {
            responses: Mutex::new(responses.into()),
        })
    }

    fn default_model(&self, _req: &SubagentReq<'_>) -> String {
        "test".to_string()
    }

    fn decorate(&self, b: AgentBuilder, req: &SubagentReq<'_>) -> AgentBuilder {
        match self.me.upgrade() {
            Some(me) => b.tool(Arc::new(
                DelegateTool::new(self.roster.clone(), me).with_depth(req.dctx.depth),
            )),
            None => b,
        }
    }
}

struct FakeInner {
    counter: AtomicUsize,
    sinks: Mutex<Vec<Arc<FakeSink>>>,
}

struct FakeHandle {
    inner: Arc<FakeInner>,
    parent: String,
}

struct FakeSink {
    inner: Arc<FakeInner>,
    session_id: String,
    agent: String,
    parent: String,
    sink: PersistSink,
    rx: Mutex<mpsc::UnboundedReceiver<Arc<SessionEvent>>>,
    events: Mutex<Vec<SessionEvent>>,
    flushed: AtomicBool,
}

#[async_trait]
impl ChildPersistence for FakeHandle {
    async fn begin(&self, agent: &str) -> anyhow::Result<Box<dyn ChildSink>> {
        let nth = self.inner.counter.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::unbounded_channel();
        let sink = Arc::new(FakeSink {
            inner: self.inner.clone(),
            session_id: format!("fake-{nth}"),
            agent: agent.to_string(),
            parent: self.parent.clone(),
            sink: PersistSink::new(tx),
            rx: Mutex::new(rx),
            events: Mutex::new(Vec::new()),
            flushed: AtomicBool::new(false),
        });
        self.inner.sinks.lock().unwrap().push(sink.clone());
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
        ChildPersistenceHandle(Arc::new(FakeHandle {
            inner: self.0.inner.clone(),
            parent: self.0.session_id.clone(),
        }))
    }

    async fn flush(&self) -> anyhow::Result<()> {
        let mut rx = self.0.rx.lock().unwrap();
        while let Ok(event) = rx.try_recv() {
            self.0.events.lock().unwrap().push((*event).clone());
        }
        self.0.flushed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn nested_delegation_produces_a_walkable_persisted_hierarchy() {
    let roster = Arc::new(AgentRoster::new(vec![
        AgentDef::parse_markdown("---\nname: mid\ndescription: middle\n---\nDelegate onward.")
            .unwrap(),
        AgentDef::parse_markdown("---\nname: leaf\ndescription: leaf\n---\nAnswer directly.")
            .unwrap(),
    ]));
    let builder = Arc::new_cyclic(|weak| NestedBuilder {
        roster: roster.clone(),
        me: weak.clone(),
    });
    let inner = Arc::new(FakeInner {
        counter: AtomicUsize::new(0),
        sinks: Mutex::new(Vec::new()),
    });

    let tool = DelegateTool::new(roster, builder);
    let mut ctx = ToolContext::new("alice", "root-thread", "r1");
    ctx.insert(ChildPersistenceHandle(Arc::new(FakeHandle {
        inner: inner.clone(),
        parent: "root-thread".into(),
    })));

    let result = tool
        .execute(
            serde_json::json!({ "agent": "mid", "prompt": "start" }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!result.is_error(), "{}", result.text());
    assert!(result.text().contains("mid done"));

    let sinks = inner.sinks.lock().unwrap();
    assert_eq!(sinks.len(), 2, "one transcript per level");

    let mid = &sinks[0];
    assert_eq!(mid.agent, "mid");
    assert_eq!(mid.parent, "root-thread");
    assert!(mid.flushed.load(Ordering::SeqCst));

    let leaf = &sinks[1];
    assert_eq!(leaf.agent, "leaf");
    assert_eq!(
        leaf.parent, mid.session_id,
        "the grandchild hangs off the child, not the root"
    );
    assert!(leaf.flushed.load(Ordering::SeqCst));

    let mid_events = mid.events.lock().unwrap();
    let leaf_edge = mid_events
        .iter()
        .find_map(|event| match event {
            SessionEvent::DelegationFinished {
                agent,
                child_session,
                ..
            } if agent == "leaf" => Some(child_session.clone()),
            _ => None,
        })
        .expect("the child's transcript records its own delegation edge");
    assert_eq!(
        leaf_edge.as_deref(),
        Some(leaf.session_id.as_str()),
        "the edge in the child's log points at the grandchild's session"
    );

    let leaf_events = leaf.events.lock().unwrap();
    assert!(
        leaf_events
            .iter()
            .any(|e| matches!(e, SessionEvent::RunEnd { .. })),
        "the grandchild transcript is complete"
    );
}
