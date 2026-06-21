//! Route-level integration tests against the new stack.
//!
//! CRUD endpoints (health, threads, tenant isolation) run against a
//! `PanicFactory`; the agent-hot path (SSE streaming) runs against a
//! `ScriptedProvider` that returns one text turn with no network.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use runic_agent::Agent;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::{AgentFactory, HumanHub, ServeConfig, router};
use runic_substrate::MemorySessionStore;
use runic_types::{ContentBlock, StopReason, TokenUsage};
use serde_json::Value;

/// Build-on-demand fixture: panics if anyone tries to actually use the agent.
/// Fine for tests that only hit CRUD endpoints.
struct PanicFactory;

#[async_trait]
impl AgentFactory for PanicFactory {
    async fn build(&self, _: &str, _: &str) -> Agent {
        panic!("PanicFactory: tests must not invoke the agent path");
    }
}

fn make_router() -> axum::Router {
    router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        agent_factory: Arc::new(PanicFactory),
        human_hub: Arc::new(HumanHub::new()),
    })
}

/// Minimal provider: one text turn, then `EndTurn`. The trait's default
/// `stream` wraps `complete`, emitting a `TextDelta` + `ContentComplete`, so
/// the agent surfaces a `pong` token without a network or API key.
struct ScriptedProvider;

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: "pong".into(),
                provider_metadata: None,
            }],
            stop_reason: StopReason::EndTurn,
            tool_calls: vec![],
            usage: TokenUsage {
                input_tokens: 1,
                output_tokens: 2,
            },
        })
    }
}

/// Factory that builds a real Agent backed by the scripted provider, keyed to
/// the requested session id (so persistence/replay line up).
struct ScriptedFactory;

#[async_trait]
impl AgentFactory for ScriptedFactory {
    async fn build(&self, _tenant: &str, session_id: &str) -> Agent {
        Agent::builder(Arc::new(ScriptedProvider), "alice", session_id)
            .system_prompt("test")
            .build()
    }
}

fn scripted_router() -> axum::Router {
    router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        agent_factory: Arc::new(ScriptedFactory),
        human_hub: Arc::new(HumanHub::new()),
    })
}

fn run_request(thread_id: &str, message: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/threads/{thread_id}/runs/stream"))
        .header("content-type", "application/json")
        .header("x-runic-tenant", "alice")
        .body(Body::from(format!(r#"{{"message":"{message}"}}"#)))
        .unwrap()
}

async fn body_to_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 10_000_000)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

async fn body_to_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn healthz_returns_ok() {
    let app = make_router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["service"], "runic-serve");
}

#[tokio::test]
async fn create_thread_returns_201_with_generated_id() {
    let app = make_router();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/threads")
                .header("content-type", "application/json")
                .header("x-runic-tenant", "alice")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_to_json(resp).await;
    assert_eq!(body["tenant"], "alice");
    assert_eq!(body["event_count"], 0);
    assert!(body["thread_id"].as_str().is_some_and(|s| !s.is_empty()));
}

#[tokio::test]
async fn create_thread_honors_provided_id() {
    let app = make_router();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/threads")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"thread_id":"my-custom-id"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_to_json(resp).await;
    assert_eq!(body["thread_id"], "my-custom-id");
    assert_eq!(body["tenant"], "default");
}

#[tokio::test]
async fn list_threads_starts_empty() {
    let app = make_router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/threads")
                .header("x-runic-tenant", "alice")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_json(resp).await;
    assert!(body["threads"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn get_thread_returns_empty_event_count_for_new_thread() {
    let app = make_router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/threads/some-thread")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_json(resp).await;
    assert_eq!(body["thread_id"], "some-thread");
    assert_eq!(body["event_count"], 0);
}

#[tokio::test]
async fn delete_thread_returns_204() {
    let app = make_router();
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/threads/anything")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn tenant_header_isolates_thread_listings() {
    let app = router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        agent_factory: Arc::new(PanicFactory),
        human_hub: Arc::new(HumanHub::new()),
    });

    // Alice creates a thread via CRUD (no agent path → PanicFactory is safe).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/threads")
                .header("content-type", "application/json")
                .header("x-runic-tenant", "alice")
                .body(Body::from(r#"{"thread_id":"alice-thread"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/threads")
                .header("content-type", "application/json")
                .header("x-runic-tenant", "bob")
                .body(Body::from(r#"{"thread_id":"bob-thread"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Bob lists their own threads — must not see alice's.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/threads")
                .header("x-runic-tenant", "bob")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_json(resp).await;
    let ids: Vec<&str> = body["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["thread_id"].as_str().unwrap())
        .collect();
    assert!(
        !ids.contains(&"alice-thread"),
        "bob should not see alice's thread; got {ids:?}"
    );
}

#[tokio::test]
async fn run_streams_agent_events() {
    let app = scripted_router();
    let resp = app.oneshot(run_request("t1", "ping")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_string(resp).await;
    assert!(
        body.contains("pong"),
        "streamed text missing from body: {body}"
    );
}

#[tokio::test]
async fn sequential_runs_on_same_thread_both_succeed() {
    // The second run can only proceed once the first releases the thread's
    // slot mutex — proving the warm agent is reused across runs.
    let app = scripted_router();

    let r1 = app.clone().oneshot(run_request("t1", "one")).await.unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    assert!(body_to_string(r1).await.contains("pong"));

    let r2 = app.clone().oneshot(run_request("t1", "two")).await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    assert!(body_to_string(r2).await.contains("pong"));
}

#[tokio::test]
async fn abandoned_run_does_not_brick_the_thread() {
    // Drop the response without reading it (client disconnect); the detached
    // run task still drives the agent to completion, so a follow-up run on the
    // same thread succeeds.
    let app = scripted_router();

    let abandoned = app
        .clone()
        .oneshot(run_request("t1", "abandon"))
        .await
        .unwrap();
    assert_eq!(abandoned.status(), StatusCode::OK);
    drop(abandoned);

    let resp = app
        .clone()
        .oneshot(run_request("t1", "after"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_to_string(resp).await.contains("pong"));
}
