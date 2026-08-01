mod common;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::StatusCode;
use tower::ServiceExt;

use runic::subagent::{DelegateTool, Subagent};
use runic::{Agent, Llm};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_substrate::SessionEvent;
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
    let input = serde_json::json!({
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

fn agent_with_delegate() -> Agent {
    let provider = Arc::new(ScriptedProvider {
        responses: Mutex::new(
            vec![delegate_background_response(), text_response("spawned")].into(),
        ),
    });
    let roster = vec![Subagent::new(
        "scout",
        "research",
        Agent::new(
            Llm::new(
                Arc::new(ScriptedProvider {
                    responses: Mutex::new(vec![text_response("dug it up")].into()),
                }),
                "child-model",
            )
            .instructions("dig"),
        ),
    )];
    Agent::new(Llm::new(provider, "test-model").instructions("sys")).tool(DelegateTool::new(roster))
}

#[tokio::test]
async fn a_background_task_is_durable_without_another_run() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(agent_with_delegate());
    let session = common::uid("t");

    let resp = app
        .oneshot(common::wait_request(&session, &h.tenant, "go"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let mut finished = None;
    for _ in 0..100 {
        let events = h.store().read(&h.tenant, &session).await.unwrap();
        finished = events.into_iter().find_map(|s| match s.event {
            SessionEvent::TaskFinished { status, result, .. } => Some((status, result)),
            _ => None,
        });
        if finished.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let (status, result) =
        finished.expect("TaskFinished must reach the store with no run in flight");
    assert_eq!(status, runic_state::TaskStatus::Completed);
    assert_eq!(result.as_deref(), Some("dug it up"));

    let events = h.store().read(&h.tenant, &session).await.unwrap();
    assert!(
        events
            .iter()
            .any(|s| matches!(s.event, SessionEvent::TaskSpawned { .. }))
    );
}
