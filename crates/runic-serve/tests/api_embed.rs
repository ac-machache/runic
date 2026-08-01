mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::{ServeConfig, bare_router, router};
use runic_types::{ContentBlock, StopReason, TokenUsage};

use common::Harness;

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

fn config(h: &Harness) -> ServeConfig {
    h.config()
        .agent("main", common::agent(Arc::new(EchoProvider)))
}

#[tokio::test]
async fn bare_router_nests_under_a_prefix() {
    let Some(h) = common::harness().await else {
        return;
    };

    let bare = Router::new().nest("/api", bare_router(config(&h)));
    let health = bare
        .oneshot(
            Request::builder()
                .uri("/api/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);

    let full = Router::new().nest("/api", router(config(&h)));
    let run = full
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/t1/runs/wait")
                .header("content-type", "application/json")
                .header("x-runic-tenant", &h.tenant)
                .body(Body::from(json!({ "message": "hi" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(run.status(), StatusCode::OK);
    assert_eq!(common::body_json(run).await["text"], "echo");
}

#[tokio::test]
async fn openapi_reports_the_mount_prefix() {
    let Some(h) = common::harness().await else {
        return;
    };
    let nested = Router::new().nest("/api", bare_router(config(&h)));
    let spec = common::body_json(
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

    let root = router(config(&h));
    let spec = common::body_json(
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
    let Some(h) = common::harness().await else {
        return;
    };
    let response = bare_router(config(&h))
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
    let Some(h) = common::harness().await else {
        return;
    };
    let response = router(config(&h))
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
async fn openapi_reports_a_multi_segment_prefix() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = Router::new().nest(
        "/internal",
        Router::new().nest("/v1", bare_router(config(&h))),
    );
    let spec = common::body_json(
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
    let Some(h) = common::harness().await else {
        return;
    };
    let response = router(config(&h))
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/sessions/t1/runs/wait")
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
    let Some(h) = common::harness().await else {
        return;
    };
    let app = Router::new().nest("/api", bare_router(config(&h)));

    let missing_session = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/sessions/nope")
                .header("x-runic-tenant", &h.tenant)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_session.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        common::body_json(missing_session).await["error"],
        "not_found"
    );

    let unknown_agent = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/t1/runs/wait")
                .header("content-type", "application/json")
                .header("x-runic-tenant", &h.tenant)
                .body(Body::from(
                    json!({ "agent": "ghost", "message": "hi" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unknown_agent.status(), StatusCode::NOT_FOUND);
    assert_eq!(common::body_json(unknown_agent).await["error"], "not_found");
}

#[tokio::test]
async fn malformed_json_is_a_400_when_nested() {
    let Some(h) = common::harness().await else {
        return;
    };
    let response = Router::new()
        .nest("/api", bare_router(config(&h)))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/t1/runs/wait")
                .header("content-type", "application/json")
                .header("x-runic-tenant", &h.tenant)
                .body(Body::from("{not json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn wrong_content_type_is_rejected_when_nested() {
    let Some(h) = common::harness().await else {
        return;
    };
    let response = Router::new()
        .nest("/api", bare_router(config(&h)))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/t1/runs/wait")
                .header("content-type", "text/plain")
                .header("x-runic-tenant", &h.tenant)
                .body(Body::from(json!({ "message": "hi" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn serve_fails_loudly_on_a_taken_port() {
    let Some(h) = common::harness().await else {
        return;
    };
    let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = taken.local_addr().unwrap();

    let result = runic_serve::serve(config(&h), addr).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn serve_binds_and_answers_over_tcp() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let Some(h) = common::harness().await else {
        return;
    };
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);

    let server = tokio::spawn(runic_serve::serve(config(&h), addr));

    let mut stream = None;
    for _ in 0..50 {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
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
