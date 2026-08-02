mod common;

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use runic::types::{ContentBlock, StopReason, TokenUsage};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::{Identity, IdentityError, IdentityResolver, bare_router, router};

use common::Harness;

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

struct BearerResolver {
    alice_tenant: String,
}

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
                tenant: self.alice_tenant.clone(),
            }),
            Some("token-boom") => Err(IdentityError::Internal("resolver exploded".into())),
            Some(_) => Err(IdentityError::InvalidCredentials("unknown token".into())),
            None => Err(IdentityError::InvalidCredentials(
                "expected bearer scheme".into(),
            )),
        }
    }
}

fn wait_request(auth: Option<&str>, spoofed_tenant: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/sessions/t1/runs/wait")
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

fn authed_config(h: &Harness) -> runic_serve::ServeConfig {
    h.config()
        .agent("main", common::agent(Arc::new(EchoProvider)))
        .identity(Arc::new(BearerResolver {
            alice_tenant: h.tenant.clone(),
        }))
}

#[tokio::test]
async fn missing_credentials_is_401() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = router(authed_config(&h));
    let resp = app.oneshot(wait_request(None, None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = common::body_json(resp).await;
    assert_eq!(body["error"], "unauthorized");
    assert_eq!(body["message"], "missing credentials");
}

#[tokio::test]
async fn invalid_credentials_is_401() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = router(authed_config(&h));
    let resp = app
        .oneshot(wait_request(Some("Bearer token-mallory"), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = common::body_json(resp).await;
    assert_eq!(body["error"], "unauthorized");
    assert!(body["message"].as_str().unwrap().contains("unknown token"));
}

#[tokio::test]
async fn non_bearer_scheme_is_401() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = router(authed_config(&h));
    let resp = app
        .oneshot(wait_request(Some("Basic dXNlcjpwdw=="), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn resolver_failure_is_500() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = router(authed_config(&h));
    let resp = app
        .oneshot(wait_request(Some("Bearer token-boom"), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = common::body_json(resp).await;
    assert_eq!(body["error"], "internal");
}

#[tokio::test]
async fn the_verified_tenant_wins_over_a_spoofed_header() {
    let Some(h) = common::harness().await else {
        return;
    };
    let store = h.store();
    let tenant = h.tenant.clone();
    let app = router(authed_config(&h));
    let resp = app
        .oneshot(wait_request(Some("Bearer token-alice"), Some("mallory")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let alice_events = store.read(&tenant, "t1").await.unwrap();
    assert!(!alice_events.is_empty());
    let mallory_events = store.read("mallory", "t1").await.unwrap();
    assert!(mallory_events.is_empty());
}

#[tokio::test]
async fn healthz_stays_open_without_credentials() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = router(authed_config(&h));
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
    let Some(h) = common::harness().await else {
        return;
    };
    let app = router(authed_config(&h));
    for path in [
        "/agents",
        "/sessions",
        "/sessions/t1",
        "/sessions/t1/events",
        "/sessions/t1/state",
        "/sessions/t1/artifacts",
        "/sessions/t1/runs/r1",
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
    let Some(h) = common::harness().await else {
        return;
    };
    let app: Router = bare_router(authed_config(&h));
    let resp = app.oneshot(wait_request(None, None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn without_a_resolver_the_header_is_trusted_as_before() {
    let Some(h) = common::harness().await else {
        return;
    };
    let store = h.store();
    let app = h.single_router(common::agent(Arc::new(EchoProvider)));
    let resp = app.oneshot(wait_request(None, Some("bob"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!store.read("bob", "t1").await.unwrap().is_empty());
}
