use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use runic::agent::Session;
use runic::subagent::{DelegateTool, Subagent, SubagentBuilder, SubagentReq};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::{AgentFactory, ServeConfig, router, single_agent};
use runic_state::SessionEvent;
use runic_substrate::{MemoryArtifactStore, MemorySessionStore, SessionStore};
use runic_types::{ContentBlock, StopReason, TokenUsage, ToolCall};

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

fn text_response(text: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::Text {
            text: text.into(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::EndTurn,
        tool_calls: vec![],
        usage: TokenUsage::default(),
    }
}

fn delegate_background_response() -> CompletionResponse {
    let input = json!({
        "action": "delegate",
        "agent": "scout",
        "prompt": "dig",
        "background": true,
    });
    CompletionResponse {
        content: vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: "delegate".into(),
            input: input.clone(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: "t1".into(),
            name: "delegate".into(),
            input,
        }],
        usage: TokenUsage::default(),
    }
}

struct StubBuilder;

#[async_trait]
impl SubagentBuilder for StubBuilder {
    async fn provider(&self, _req: &SubagentReq<'_>) -> Arc<dyn Provider> {
        Arc::new(ScriptedProvider {
            responses: Mutex::new(vec![text_response("dug it up")].into()),
        })
    }

    fn default_model(&self, _req: &SubagentReq<'_>) -> String {
        String::new()
    }
}

struct DelegatingFactory;

#[async_trait]
impl AgentFactory for DelegatingFactory {
    async fn build(&self, tenant: &str, session_id: &str) -> anyhow::Result<Session> {
        let provider = Arc::new(ScriptedProvider {
            responses: Mutex::new(
                vec![delegate_background_response(), text_response("spawned")].into(),
            ),
        });
        let roster = vec![Subagent::new("scout", "research").prompt("dig")];
        Ok(Session::builder(provider, tenant, session_id)
            .system_prompt("sys")
            .tool(Arc::new(DelegateTool::with_builder(
                roster,
                Arc::new(StubBuilder),
            )))
            .build())
    }
}

#[tokio::test]
async fn a_background_task_is_durable_without_another_run() {
    let store = Arc::new(MemorySessionStore::new());
    let app = router(ServeConfig {
        session_store: store.clone(),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agents: single_agent("main", Arc::new(DelegatingFactory)),
        limits: Default::default(),
        workers: None,
        broker: None,
        nudge: None,
        identity: None,
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/threads/t1/runs/wait")
                .header("content-type", "application/json")
                .body(Body::from(json!({ "message": "go" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let mut finished = None;
    for _ in 0..100 {
        let events = store.read("default", "t1").await.unwrap();
        finished = events.into_iter().find_map(|s| match s.event {
            SessionEvent::TaskFinished { status, result, .. } => Some((status, result)),
            _ => None,
        });
        if finished.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let (status, result) =
        finished.expect("TaskFinished must reach the store with no run in flight");
    assert_eq!(status, runic_state::TaskStatus::Completed);
    assert_eq!(result.as_deref(), Some("dug it up"));

    let events = store.read("default", "t1").await.unwrap();
    assert!(
        events
            .iter()
            .any(|s| matches!(s.event, SessionEvent::TaskSpawned { .. }))
    );
}
