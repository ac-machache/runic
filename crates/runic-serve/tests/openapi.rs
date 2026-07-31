mod common;

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};

struct PanicProvider;

#[async_trait]
impl Provider for PanicProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        panic!("openapi tests never drive the agent");
    }
}

async fn app() -> Option<Router> {
    let h = common::harness().await?;
    Some(h.single_router(common::agent(Arc::new(PanicProvider))))
}

async fn spec() -> Option<Value> {
    let app = app().await?;
    let resp = app
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
    Some(serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn openapi_json_serves_and_parses() {
    let Some(spec) = spec().await else { return };
    assert_eq!(spec["openapi"].as_str().unwrap().chars().next(), Some('3'));
    assert_eq!(spec["info"]["title"], "runic-serve");
}

#[tokio::test]
async fn every_route_and_method_is_documented() {
    let Some(spec) = spec().await else { return };
    let paths = &spec["paths"];
    let expect: &[(&str, &[&str])] = &[
        ("/healthz", &["get"]),
        ("/agents", &["get"]),
        ("/agents/{name}", &["get"]),
        ("/threads", &["get", "post"]),
        ("/threads/{thread_id}", &["get", "patch", "delete"]),
        ("/threads/{thread_id}/children", &["get"]),
        ("/threads/{thread_id}/events", &["get"]),
        ("/threads/{thread_id}/state", &["get"]),
        ("/threads/{thread_id}/artifacts", &["get", "post"]),
        ("/threads/{thread_id}/artifacts/{artifact_id}", &["get"]),
        ("/transcribe", &["post"]),
        ("/threads/{thread_id}/runs", &["get"]),
        ("/threads/{thread_id}/runs/{run_id}", &["get"]),
        ("/threads/{thread_id}/runs/{run_id}/timeline", &["get"]),
        ("/threads/{thread_id}/runs/wait", &["post"]),
        ("/threads/{thread_id}/asks/{ask_id}", &["post"]),
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
async fn deleted_endpoints_are_not_documented() {
    let Some(spec) = spec().await else { return };
    let paths = &spec["paths"];
    for path in [
        "/threads/{thread_id}/runs/stream",
        "/threads/{thread_id}/runs",
        "/threads/{thread_id}/runs/cancel",
        "/threads/{thread_id}/runs/steer",
        "/threads/{thread_id}/runs/{run_id}/stream",
        "/threads/{thread_id}/runs/{run_id}/asks/{ask_id}",
    ] {
        if let Some(item) = paths.get(path) {
            assert!(
                item.get("post").is_none(),
                "{path} POST should no longer be documented"
            );
        }
    }
}

#[tokio::test]
async fn important_schemas_and_error_body_exist() {
    let Some(spec) = spec().await else { return };
    let schemas = &spec["components"]["schemas"];
    for name in [
        "HealthResponse",
        "AgentInfo",
        "AgentList",
        "AgentOverview",
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
        "WaitRunResponse",
        "RunStatusResponse",
        "RunSummary",
        "RunListResponse",
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
async fn tenant_header_is_documented_on_list_threads() {
    let Some(spec) = spec().await else { return };
    let list_params = &spec["paths"]["/threads"]["get"]["parameters"];
    assert!(
        has_header(list_params, "X-Runic-Tenant"),
        "X-Runic-Tenant not documented on GET /threads"
    );
}

#[tokio::test]
async fn error_responses_reference_the_error_body_schema() {
    let Some(spec) = spec().await else { return };
    let schema = &spec["paths"]["/threads/{thread_id}"]["get"]["responses"]["404"]["content"]["application/json"]
        ["schema"]["$ref"];
    assert_eq!(schema, "#/components/schemas/ErrorBody");
}

#[cfg(feature = "docs-ui")]
#[tokio::test]
async fn swagger_ui_mounts_without_route_overlap() {
    let Some(app) = app().await else { return };
    let public = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(public.status(), StatusCode::OK);

    let internal = app
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
