use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use runic_agent::Agent;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::{AgentFactory, ServeConfig, bare_router, router};
use runic_substrate::{MemoryArtifactStore, MemorySessionStore};
use runic_types::{ContentBlock, StopReason, TokenUsage};

struct EchoProvider;

#[async_trait]
impl Provider for EchoProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: "echo".into(),
                provider_metadata: None,
            }],
            stop_reason: StopReason::EndTurn,
            tool_calls: vec![],
            usage: TokenUsage::default(),
        })
    }
}

struct EchoFactory;

#[async_trait]
impl AgentFactory for EchoFactory {
    async fn build(&self, tenant: &str, session_id: &str) -> anyhow::Result<Agent> {
        Ok(Agent::builder(Arc::new(EchoProvider), tenant, session_id)
            .system_prompt("test")
            .build())
    }
}

fn config() -> ServeConfig {
    ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agents: runic_serve::single_agent("main", Arc::new(EchoFactory)),
        limits: Default::default(),
        workers: None,
        broker: None,
        nudge: None,
        identity: None,
    }
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn body_string(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[tokio::test]
async fn bare_router_nests_under_a_prefix() {
    let app = Router::new().nest("/api", bare_router(config()));

    let health = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);

    let run = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/threads/t1/runs/wait")
                .header("content-type", "application/json")
                .body(Body::from(json!({ "message": "hi" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(run.status(), StatusCode::OK);
    assert_eq!(body_json(run).await["text"], "echo");
}

#[tokio::test]
async fn openapi_reports_the_mount_prefix() {
    let nested = Router::new().nest("/api", bare_router(config()));
    let spec = body_json(
        nested
            .oneshot(
                Request::builder()
                    .uri("/api/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(spec["servers"][0]["url"], "/api");

    let root = router(config());
    let spec = body_json(
        root.oneshot(
            Request::builder()
                .uri("/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap(),
    )
    .await;
    assert!(spec.get("servers").is_none() || spec["servers"].is_null());
}

#[tokio::test]
async fn bare_router_skips_cors_and_request_id() {
    let response = bare_router(config())
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .header("origin", "https://example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !response
            .headers()
            .contains_key("access-control-allow-origin")
    );
    assert!(!response.headers().contains_key("x-request-id"));
}

#[tokio::test]
async fn full_router_applies_cors_and_request_id() {
    let response = router(config())
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .header("origin", "https://example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
    assert!(response.headers().contains_key("x-request-id"));
}

#[tokio::test]
async fn streaming_works_through_a_nested_prefix() {
    let app = Router::new().nest("/api", bare_router(config()));

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/threads/t1/runs/stream")
                .header("content-type", "application/json")
                .body(Body::from(json!({ "message": "hi" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let body = body_string(response).await;
    let kinds: Vec<&str> = body
        .lines()
        .filter_map(|l| l.strip_prefix("event:"))
        .map(str::trim)
        .collect();
    assert_eq!(kinds.first(), Some(&"run_start"));
    assert_eq!(kinds.last(), Some(&"done"));

    let events = app
        .oneshot(
            Request::builder()
                .uri("/api/threads/t1/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(events.status(), StatusCode::OK);
    assert!(
        !body_json(events).await["events"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn openapi_reports_a_multi_segment_prefix() {
    let app = Router::new().nest(
        "/internal",
        Router::new().nest("/v1", bare_router(config())),
    );
    let spec = body_json(
        app.oneshot(
            Request::builder()
                .uri("/internal/v1/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(spec["servers"][0]["url"], "/internal/v1");
}

#[tokio::test]
async fn full_router_answers_preflight() {
    let response = router(config())
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/threads/t1/runs/wait")
                .header("origin", "https://example.com")
                .header("access-control-request-method", "POST")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
    assert_eq!(response.headers()["access-control-allow-methods"], "*");
}

#[tokio::test]
async fn errors_keep_their_shape_when_nested() {
    let app = Router::new().nest("/api", bare_router(config()));

    let missing_thread = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/threads/nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_thread.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(missing_thread).await["error"], "not_found");

    let unknown_agent = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/threads/t1/runs/wait")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "agent": "ghost", "message": "hi" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unknown_agent.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(unknown_agent).await["error"], "not_found");
}

#[tokio::test]
async fn malformed_json_is_a_400_when_nested() {
    let response = Router::new()
        .nest("/api", bare_router(config()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/threads/t1/runs/wait")
                .header("content-type", "application/json")
                .body(Body::from("{not json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn wrong_content_type_is_rejected_when_nested() {
    let response = Router::new()
        .nest("/api", bare_router(config()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/threads/t1/runs/wait")
                .header("content-type", "text/plain")
                .body(Body::from(json!({ "message": "hi" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn cancel_with_no_run_is_a_409_when_nested() {
    let response = Router::new()
        .nest("/api", bare_router(config()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/threads/t1/runs/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(body_json(response).await["error"], "conflict");
}

#[tokio::test]
async fn serve_fails_loudly_on_a_taken_port() {
    let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = taken.local_addr().unwrap();

    let result = runic_serve::serve(config(), addr).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn serve_binds_and_answers_over_tcp() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);

    let server = tokio::spawn(runic_serve::serve(config(), addr));

    let mut stream = None;
    for _ in 0..50 {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
        }
    }
    let mut stream = stream.expect("server never came up");

    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(response.contains(r#""status":"ok""#) || response.contains("ok"));

    server.abort();
}
