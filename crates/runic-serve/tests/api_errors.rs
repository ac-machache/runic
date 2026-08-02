mod common;

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use runic::substrate::{SessionEvent, SessionMeta, SessionStore, StoredEvent};
use runic::transcriber::{SpeechToText, TranscribeError, Transcript};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::router;

use common::Harness;

const TENANT: &str = "alice";

struct PanicProvider;

#[async_trait]
impl Provider for PanicProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        panic!("agent path must not run here");
    }
}

struct FailingProvider;

#[async_trait]
impl Provider for FailingProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Err(ProviderError::Http("upstream model down".into()))
    }
}

fn boom() -> runic::substrate::Error {
    runic::substrate::Error::Database("injected store failure".into())
}

struct FailingSessionStore;

#[async_trait]
impl SessionStore for FailingSessionStore {
    async fn append(
        &self,
        _tenant: &str,
        _session_id: &str,
        _event: &SessionEvent,
    ) -> runic::substrate::Result<u64> {
        Err(boom())
    }
    async fn append_batch(
        &self,
        _tenant: &str,
        _session_id: &str,
        _events: &[SessionEvent],
    ) -> runic::substrate::Result<()> {
        Err(boom())
    }
    async fn read(
        &self,
        _tenant: &str,
        _session_id: &str,
    ) -> runic::substrate::Result<Vec<StoredEvent>> {
        Err(boom())
    }
    async fn read_after(
        &self,
        _tenant: &str,
        _session_id: &str,
        _after_seq: u64,
    ) -> runic::substrate::Result<Vec<StoredEvent>> {
        Err(boom())
    }
    async fn list_sessions(&self, _tenant: &str) -> runic::substrate::Result<Vec<SessionMeta>> {
        Err(boom())
    }
    async fn session_meta(
        &self,
        _tenant: &str,
        _session_id: &str,
    ) -> runic::substrate::Result<Option<SessionMeta>> {
        Err(boom())
    }
    async fn set_label(
        &self,
        _tenant: &str,
        _session_id: &str,
        _label: Option<&str>,
    ) -> runic::substrate::Result<()> {
        Err(boom())
    }
    async fn delete_session(
        &self,
        _tenant: &str,
        _session_id: &str,
    ) -> runic::substrate::Result<()> {
        Err(boom())
    }
}

struct FailingTranscriber;

#[async_trait]
impl SpeechToText for FailingTranscriber {
    async fn transcribe(&self, _a: &[u8], _f: &str) -> Result<Transcript, TranscribeError> {
        Err(TranscribeError::Http("provider unavailable".into()))
    }
}

fn crud_router(h: &Harness) -> Router {
    h.single_router(common::agent(Arc::new(PanicProvider)))
}

fn failing_store_router(h: &Harness) -> Router {
    let sessions =
        runic::substrate::Sessions::from(Arc::new(FailingSessionStore) as Arc<dyn SessionStore>);
    router(
        runic_serve::ServeConfig::new(sessions, h.blobs.clone(), h.pool.clone())
            .agent("main", common::agent(Arc::new(PanicProvider))),
    )
}

fn failing_agent_router(h: &Harness) -> Router {
    h.single_router(common::agent(Arc::new(FailingProvider)))
}

fn get(uri: &str, tenant: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("x-runic-tenant", tenant)
        .body(Body::empty())
        .unwrap()
}

fn transcribe(mime: &str, bytes: &[u8]) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/transcribe")
        .header("content-type", mime)
        .header("x-runic-tenant", TENANT)
        .body(Body::from(bytes.to_vec()))
        .unwrap()
}

async fn status_json(resp: axum::response::Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn assert_error_shape(body: &Value, kind: &str) {
    assert_eq!(body["error"], kind, "unexpected error kind: {body}");
    assert!(
        body["message"].is_string(),
        "error body needs a message: {body}"
    );
}

#[tokio::test]
async fn not_found_shape() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    let (status, body) =
        status_json(app.oneshot(get("/sessions/ghost", TENANT)).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_error_shape(&body, "not_found");
}

#[tokio::test]
async fn bad_request_shape() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    let resp = app
        .oneshot(common::post_json("/sessions/t1/runs/wait", TENANT, "{}"))
        .await
        .unwrap();
    let (status, body) = status_json(resp).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_error_shape(&body, "bad_request");
}

#[tokio::test]
async fn store_error_shape() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = failing_store_router(&h);
    let (status, body) = status_json(
        app.oneshot(get("/sessions/anything", TENANT))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_error_shape(&body, "store");
}

#[tokio::test]
async fn upstream_error_shape() {
    let Some(h) = common::harness().await else {
        return;
    };
    let config = h
        .config()
        .agent("main", common::agent(Arc::new(PanicProvider)))
        .transcriber(Some(Arc::new(FailingTranscriber)));
    let app = router(config);
    let (status, body) =
        status_json(app.oneshot(transcribe("audio/wav", b"x")).await.unwrap()).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_error_shape(&body, "upstream");
}

#[tokio::test]
async fn not_configured_error_shape() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    let (status, body) =
        status_json(app.oneshot(transcribe("audio/wav", b"x")).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_error_shape(&body, "not_configured");
}

#[tokio::test]
async fn agent_failure_surfaces_as_a_500_agent_error() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = failing_agent_router(&h);
    let resp = app
        .oneshot(common::post_json(
            "/sessions/t1/runs/wait",
            TENANT,
            json!({ "message": "hi" }).to_string(),
        ))
        .await
        .unwrap();
    let (status, body) = status_json(resp).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_error_shape(&body, "agent");
}
