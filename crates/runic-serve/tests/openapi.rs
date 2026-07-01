use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use runic_agent::Agent;
use runic_serve::{AgentFactory, HumanHub, ServeConfig, router};
use runic_substrate::{MemoryArtifactStore, MemorySessionStore};

struct PanicFactory;

#[async_trait]
impl AgentFactory for PanicFactory {
    async fn build(&self, _: &str, _: &str) -> Agent {
        panic!("openapi tests never drive the agent");
    }
}

fn app() -> Router {
    router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agent_factory: Arc::new(PanicFactory),
        human_hub: Arc::new(HumanHub::new()),
    })
}

async fn spec() -> Value {
    let resp = app()
        .oneshot(
            Request::builder()
                .uri("/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 4_000_000)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn openapi_json_serves_and_parses() {
    let spec = spec().await;
    assert_eq!(spec["openapi"].as_str().unwrap().chars().next(), Some('3'));
    assert_eq!(spec["info"]["title"], "runic-serve");
}

#[tokio::test]
async fn every_route_and_method_is_documented() {
    let spec = spec().await;
    let paths = &spec["paths"];
    let expect: &[(&str, &[&str])] = &[
        ("/healthz", &["get"]),
        ("/threads", &["get", "post"]),
        ("/threads/{thread_id}", &["get", "patch", "delete"]),
        ("/threads/{thread_id}/events", &["get"]),
        ("/threads/{thread_id}/state", &["get"]),
        ("/threads/{thread_id}/artifacts", &["get", "post"]),
        ("/transcribe", &["post"]),
        ("/threads/{thread_id}/runs/stream", &["post"]),
        ("/threads/{thread_id}/runs/{run_id}/stream", &["get"]),
        ("/threads/{thread_id}/asks/{ask_id}", &["post"]),
        (
            "/threads/{thread_id}/runs/{run_id}/asks/{ask_id}",
            &["post"],
        ),
    ];
    for (path, methods) in expect {
        let item = &paths[path];
        assert!(item.is_object(), "missing path {path}");
        for method in *methods {
            assert!(
                item.get(method).is_some(),
                "path {path} missing method {method}"
            );
        }
    }
}

#[tokio::test]
async fn important_schemas_and_error_body_exist() {
    let spec = spec().await;
    let schemas = &spec["components"]["schemas"];
    for name in [
        "HealthResponse",
        "Thread",
        "ThreadSummary",
        "ThreadList",
        "CreateThreadRequest",
        "UpdateThreadRequest",
        "ThreadEventsResponse",
        "StoredEventEnvelope",
        "ThreadStateResponse",
        "UploadedArtifact",
        "ArtifactMeta",
        "TranscriptResponse",
        "RunMessageRequest",
        "AnswerRequest",
        "WireEvent",
        "ErrorBody",
    ] {
        assert!(schemas.get(name).is_some(), "missing schema {name}");
    }
    let error_props = &spec["components"]["schemas"]["ErrorBody"]["properties"];
    assert!(error_props.get("error").is_some());
    assert!(error_props.get("message").is_some());
}

#[tokio::test]
async fn sse_endpoints_are_marked_event_stream() {
    let spec = spec().await;
    for (path, method) in [
        ("/threads/{thread_id}/runs/stream", "post"),
        ("/threads/{thread_id}/runs/{run_id}/stream", "get"),
    ] {
        let content = &spec["paths"][path][method]["responses"]["200"]["content"];
        assert!(
            content.get("text/event-stream").is_some(),
            "{path} {method} 200 is not text/event-stream: {content}"
        );
    }
}

#[tokio::test]
async fn tenant_and_resume_headers_are_documented() {
    let spec = spec().await;
    let list_params = &spec["paths"]["/threads"]["get"]["parameters"];
    assert!(
        has_header(list_params, "X-Runic-Tenant"),
        "X-Runic-Tenant not documented on GET /threads"
    );

    let replay_params =
        &spec["paths"]["/threads/{thread_id}/runs/{run_id}/stream"]["get"]["parameters"];
    assert!(
        has_header(replay_params, "Last-Event-ID"),
        "Last-Event-ID not documented on replay"
    );
}

#[tokio::test]
async fn error_responses_reference_the_error_body_schema() {
    let spec = spec().await;
    let schema = &spec["paths"]["/threads/{thread_id}"]["get"]["responses"]["404"]["content"]["application/json"]
        ["schema"]["$ref"];
    assert_eq!(schema, "#/components/schemas/ErrorBody");
}

#[cfg(feature = "docs-ui")]
#[tokio::test]
async fn swagger_ui_mounts_without_route_overlap() {
    let public = app()
        .oneshot(
            Request::builder()
                .uri("/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(public.status(), StatusCode::OK);

    let internal = app()
        .oneshot(
            Request::builder()
                .uri("/docs/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(internal.status(), StatusCode::OK);
}

fn has_header(params: &Value, name: &str) -> bool {
    params
        .as_array()
        .map(|arr| arr.iter().any(|p| p["in"] == "header" && p["name"] == name))
        .unwrap_or(false)
}
