use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use runic_agent::Agent;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::{
    AgentFactory, Identity, IdentityError, IdentityResolver, ServeConfig, bare_router, router,
    single_agent,
};
use runic_substrate::{MemoryArtifactStore, MemorySessionStore, SessionStore};
use runic_types::{ContentBlock, StopReason, TokenUsage};

struct EchoProvider;

#[async_trait]
impl Provider for EchoProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: "ok".into(),
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

struct BearerResolver;

#[async_trait]
impl IdentityResolver for BearerResolver {
    async fn resolve(&self, parts: &Parts) -> Result<Identity, IdentityError> {
        let header = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or(IdentityError::MissingCredentials)?;
        match header.strip_prefix("Bearer ") {
            Some("token-alice") => Ok(Identity {
                tenant: "alice".into(),
            }),
            Some("token-boom") => Err(IdentityError::Internal("resolver exploded".into())),
            Some(_) => Err(IdentityError::InvalidCredentials("unknown token".into())),
            None => Err(IdentityError::InvalidCredentials(
                "expected bearer scheme".into(),
            )),
        }
    }
}

fn config(store: Arc<dyn SessionStore>) -> ServeConfig {
    ServeConfig {
        session_store: store,
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agents: single_agent("main", Arc::new(EchoFactory)),
        limits: Default::default(),
        workers: None,
        broker: None,
        nudge: None,
        identity: Some(Arc::new(BearerResolver)),
    }
}

fn app() -> (Router, Arc<dyn SessionStore>) {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    (router(config(store.clone())), store)
}

fn wait_request(auth: Option<&str>, spoofed_tenant: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/threads/t1/runs/wait")
        .header("content-type", "application/json");
    if let Some(token) = auth {
        builder = builder.header("authorization", token);
    }
    if let Some(tenant) = spoofed_tenant {
        builder = builder.header("x-runic-tenant", tenant);
    }
    builder
        .body(Body::from(json!({ "message": "hi" }).to_string()))
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn missing_credentials_is_401() {
    let (app, _) = app();
    let resp = app.oneshot(wait_request(None, None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["error"], "unauthorized");
    assert_eq!(body["message"], "missing credentials");
}

#[tokio::test]
async fn invalid_credentials_is_401() {
    let (app, _) = app();
    let resp = app
        .oneshot(wait_request(Some("Bearer token-mallory"), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["error"], "unauthorized");
    assert!(body["message"].as_str().unwrap().contains("unknown token"));
}

#[tokio::test]
async fn non_bearer_scheme_is_401() {
    let (app, _) = app();
    let resp = app
        .oneshot(wait_request(Some("Basic dXNlcjpwdw=="), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn resolver_failure_is_500() {
    let (app, _) = app();
    let resp = app
        .oneshot(wait_request(Some("Bearer token-boom"), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = body_json(resp).await;
    assert_eq!(body["error"], "internal");
}

#[tokio::test]
async fn the_verified_tenant_wins_over_a_spoofed_header() {
    let (app, store) = app();
    let resp = app
        .oneshot(wait_request(Some("Bearer token-alice"), Some("mallory")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let alice_events = store.read("alice", "t1").await.unwrap();
    assert!(!alice_events.is_empty());
    let mallory_events = store.read("mallory", "t1").await.unwrap();
    assert!(mallory_events.is_empty());
}

#[tokio::test]
async fn healthz_stays_open_without_credentials() {
    let (app, _) = app();
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
}

#[tokio::test]
async fn every_other_route_requires_credentials() {
    let (app, _) = app();
    for path in [
        "/agents",
        "/threads",
        "/threads/t1",
        "/threads/t1/events",
        "/threads/t1/state",
        "/threads/t1/artifacts",
        "/threads/t1/runs/r1",
        "/openapi.json",
    ] {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "expected 401 for {path}"
        );
    }
}

#[tokio::test]
async fn bare_router_applies_the_resolver_too() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let app = bare_router(config(store));
    let resp = app.oneshot(wait_request(None, None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn without_a_resolver_the_header_is_trusted_as_before() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let mut config = config(store.clone());
    config.identity = None;
    let app = router(config);
    let resp = app.oneshot(wait_request(None, Some("bob"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!store.read("bob", "t1").await.unwrap().is_empty());
}
