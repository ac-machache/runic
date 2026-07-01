use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use runic_agent::Agent;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::{AgentFactory, HumanHub, ServeConfig, router};
use runic_state::SessionEvent;
use runic_substrate::{
    MemoryArtifactStore, MemorySessionStore, SessionMeta, SessionStore, StoredEvent,
};
use runic_transcriber::{SpeechToText, TranscribeError, Transcript};

const TENANT: &str = "alice";

struct PanicFactory;

#[async_trait]
impl AgentFactory for PanicFactory {
    async fn build(&self, _: &str, _: &str) -> Agent {
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

struct FailingAgentFactory;

#[async_trait]
impl AgentFactory for FailingAgentFactory {
    async fn build(&self, tenant: &str, session_id: &str) -> Agent {
        Agent::builder(Arc::new(FailingProvider), tenant, session_id)
            .system_prompt("test")
            .build()
    }
}

fn boom() -> runic_substrate::Error {
    runic_substrate::Error::Database("injected store failure".into())
}

struct FailingSessionStore;

#[async_trait]
impl SessionStore for FailingSessionStore {
    async fn append(
        &self,
        _tenant: &str,
        _session_id: &str,
        _event: &SessionEvent,
    ) -> runic_substrate::Result<u64> {
        Err(boom())
    }
    async fn read(
        &self,
        _tenant: &str,
        _session_id: &str,
    ) -> runic_substrate::Result<Vec<StoredEvent>> {
        Err(boom())
    }
    async fn read_after(
        &self,
        _tenant: &str,
        _session_id: &str,
        _after_seq: u64,
    ) -> runic_substrate::Result<Vec<StoredEvent>> {
        Err(boom())
    }
    async fn list_sessions(&self, _tenant: &str) -> runic_substrate::Result<Vec<SessionMeta>> {
        Err(boom())
    }
    async fn session_meta(
        &self,
        _tenant: &str,
        _session_id: &str,
    ) -> runic_substrate::Result<Option<SessionMeta>> {
        Err(boom())
    }
    async fn set_label(
        &self,
        _tenant: &str,
        _session_id: &str,
        _label: Option<&str>,
    ) -> runic_substrate::Result<()> {
        Err(boom())
    }
    async fn delete_session(
        &self,
        _tenant: &str,
        _session_id: &str,
    ) -> runic_substrate::Result<()> {
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

fn crud_router() -> Router {
    router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agent_factory: Arc::new(PanicFactory),
        human_hub: Arc::new(HumanHub::new()),
    })
}

fn failing_store_router() -> Router {
    router(ServeConfig {
        session_store: Arc::new(FailingSessionStore),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agent_factory: Arc::new(PanicFactory),
        human_hub: Arc::new(HumanHub::new()),
    })
}

fn transcribe_router(transcriber: Option<Arc<dyn SpeechToText>>) -> Router {
    router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber,
        agent_factory: Arc::new(PanicFactory),
        human_hub: Arc::new(HumanHub::new()),
    })
}

fn failing_agent_router() -> Router {
    router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agent_factory: Arc::new(FailingAgentFactory),
        human_hub: Arc::new(HumanHub::new()),
    })
}

fn get(uri: &str, tenant: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("x-runic-tenant", tenant)
        .body(Body::empty())
        .unwrap()
}

fn post_json(uri: &str, tenant: &str, body: String) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-runic-tenant", tenant)
        .body(Body::from(body))
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

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 10_000_000)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
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
    let app = crud_router();
    let (status, body) =
        status_json(app.oneshot(get("/threads/ghost", TENANT)).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_error_shape(&body, "not_found");
}

#[tokio::test]
async fn bad_request_shape() {
    let app = crud_router();
    let resp = app
        .oneshot(post_json("/threads/t1/runs/stream", TENANT, "{}".into()))
        .await
        .unwrap();
    let (status, body) = status_json(resp).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_error_shape(&body, "bad_request");
}

#[tokio::test]
async fn store_error_shape() {
    let app = failing_store_router();
    let (status, body) =
        status_json(app.oneshot(get("/threads/anything", TENANT)).await.unwrap()).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_error_shape(&body, "store");
}

#[tokio::test]
async fn upstream_error_shape() {
    let app = transcribe_router(Some(Arc::new(FailingTranscriber)));
    let (status, body) =
        status_json(app.oneshot(transcribe("audio/wav", b"x")).await.unwrap()).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_error_shape(&body, "upstream");
}

#[tokio::test]
async fn not_configured_error_shape() {
    let app = transcribe_router(None);
    let (status, body) =
        status_json(app.oneshot(transcribe("audio/wav", b"x")).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_error_shape(&body, "not_configured");
}

#[tokio::test]
async fn agent_failure_surfaces_as_run_error_not_an_http_error_body() {
    let app = failing_agent_router();
    let resp = app
        .oneshot(post_json(
            "/threads/t1/runs/stream",
            TENANT,
            json!({ "message": "hi" }).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(body.contains("event: run_error"), "{body}");
}
