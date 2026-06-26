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
use runic_substrate::{MemorySessionStore, SessionStore};
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
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    scripted_router_with_store(store)
}

fn scripted_router_with_store(store: Arc<dyn SessionStore>) -> axum::Router {
    router(ServeConfig {
        session_store: store,
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

fn sse_data(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|json| serde_json::from_str(json.trim()).unwrap())
        .collect()
}

fn sse_ids(body: &str) -> Vec<u64> {
    body.lines()
        .filter_map(|line| line.strip_prefix("id:"))
        .map(|id| id.trim().parse().unwrap())
        .collect()
}

async fn wait_for_stored_events(
    store: &dyn SessionStore,
    tenant: &str,
    thread_id: &str,
    min_events: usize,
) -> Vec<runic_substrate::StoredEvent> {
    for _ in 0..50 {
        let events = store.read(tenant, thread_id).await.unwrap();
        if events.len() >= min_events {
            return events;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("stored event count did not reach {min_events}");
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
async fn run_persists_events_for_thread_history() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let app = scripted_router_with_store(store.clone());

    let resp = app
        .clone()
        .oneshot(run_request("persisted-thread", "remember me"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_to_string(resp).await.contains("pong"));

    let events = wait_for_stored_events(store.as_ref(), "alice", "persisted-thread", 5).await;
    assert!(events.iter().any(|stored| {
        matches!(&stored.event, runic_state::SessionEvent::Message { msg, .. }
            if msg.content.text_content().contains("remember me"))
    }));
    assert!(events.iter().any(|stored| {
        matches!(&stored.event, runic_state::SessionEvent::Message { msg, .. }
            if msg.content.text_content().contains("pong"))
    }));
    assert!(events.iter().any(|stored| {
        matches!(&stored.event, runic_state::SessionEvent::RunEnd { outcome, .. }
            if outcome.stop_reason.as_deref() == Some("end_turn"))
    }));

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/threads/persisted-thread/events")
                .header("x-runic-tenant", "alice")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_json(resp).await;
    assert_eq!(body["events"].as_array().unwrap().len(), events.len());
}

#[tokio::test]
async fn replay_run_respects_last_event_id_and_finishes_closed_run() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let app = scripted_router_with_store(store.clone());

    let resp = app
        .clone()
        .oneshot(run_request("replay-thread", "first"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let live_body = body_to_string(resp).await;
    let run_id = sse_data(&live_body)
        .into_iter()
        .find_map(|event| {
            (event["type"] == "run_start").then(|| event["run_id"].as_str().unwrap().to_string())
        })
        .expect("live stream contains run_start");

    let stored = wait_for_stored_events(store.as_ref(), "alice", "replay-thread", 5).await;
    let after_seq = stored
        .iter()
        .find(|stored| matches!(stored.event, runic_state::SessionEvent::RunStart { .. }))
        .expect("run start persisted")
        .seq;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/threads/replay-thread/runs/{run_id}/stream"))
                .header("x-runic-tenant", "alice")
                .header("last-event-id", after_seq.to_string())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let replay_body = tokio::time::timeout(std::time::Duration::from_secs(1), body_to_string(resp))
        .await
        .expect("replay stream should finish for a closed run");

    assert!(!replay_body.contains("event: run_start"));
    assert!(replay_body.contains("event: message"));
    assert!(replay_body.contains("event: run_end"));
    assert!(replay_body.contains("event: done"));
    assert!(sse_ids(&replay_body).iter().all(|seq| *seq > after_seq));
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

#[tokio::test]
async fn answering_missing_human_ask_returns_bad_request() {
    let app = make_router();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/threads/t1/runs/r1/asks/missing-ask")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"answer":"yes"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(body_to_string(resp).await.contains("no pending ask"));
}
