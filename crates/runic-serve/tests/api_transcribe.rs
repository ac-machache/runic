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
use runic_transcriber::{SpeechToText, TranscribeError, Transcript};

const TENANT: &str = "alice";

struct PanicFactory;

#[async_trait]
impl AgentFactory for PanicFactory {
    async fn build(&self, _: &str, _: &str) -> Agent {
        panic!("agent path must not run here");
    }
}

struct EchoFilenameTranscriber;

#[async_trait]
impl SpeechToText for EchoFilenameTranscriber {
    async fn transcribe(
        &self,
        _audio: &[u8],
        filename: &str,
    ) -> Result<Transcript, TranscribeError> {
        Ok(Transcript {
            text: filename.to_string(),
            language: None,
        })
    }
}

fn transcribe_router() -> Router {
    router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: Some(Arc::new(EchoFilenameTranscriber)),
        agent_factory: Arc::new(PanicFactory),
        human_hub: Arc::new(HumanHub::new()),
    })
}

fn request(content_type: Option<&str>, filename: Option<&str>, bytes: &[u8]) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/transcribe")
        .header("x-runic-tenant", TENANT);
    if let Some(ct) = content_type {
        b = b.header("content-type", ct);
    }
    if let Some(fname) = filename {
        b = b.header("x-runic-filename", fname);
    }
    b.body(Body::from(bytes.to_vec())).unwrap()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn content_type_is_case_insensitive() {
    let app = transcribe_router();
    let resp = app
        .oneshot(request(Some("Audio/WAV"), Some("clip.wav"), b"bytes"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn content_type_with_charset_is_accepted() {
    let app = transcribe_router();
    let resp = app
        .oneshot(request(
            Some("audio/wav; charset=binary"),
            Some("clip.wav"),
            b"bytes",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn missing_content_type_is_rejected() {
    let app = transcribe_router();
    let resp = app
        .oneshot(request(None, Some("clip.wav"), b"bytes"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn non_audio_content_type_is_rejected() {
    let app = transcribe_router();
    let resp = app
        .oneshot(request(Some("text/plain"), Some("clip.wav"), b"bytes"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn filename_path_segments_are_stripped() {
    let app = transcribe_router();
    let resp = app
        .oneshot(request(
            Some("audio/wav"),
            Some("../clips\\voice.wav"),
            b"bytes",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["text"], "voice.wav");
}

#[tokio::test]
async fn empty_body_is_rejected() {
    let app = transcribe_router();
    let resp = app
        .oneshot(request(Some("audio/wav"), Some("clip.wav"), b""))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
