mod common;

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use runic::transcriber::{SpeechToText, TranscribeError, Transcript};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};

struct PanicProvider;

#[async_trait]
impl Provider for PanicProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
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

async fn transcribe_router() -> Option<Router> {
    let h = common::harness().await?;
    let config = h
        .config()
        .agent("main", common::agent(Arc::new(PanicProvider)))
        .transcriber(Some(Arc::new(EchoFilenameTranscriber)));
    Some(runic_serve::router(config))
}

fn request(content_type: Option<&str>, filename: Option<&str>, bytes: &[u8]) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/transcribe")
        .header("x-runic-tenant", "alice");
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
    let Some(app) = transcribe_router().await else {
        return;
    };
    let resp = app
        .oneshot(request(Some("Audio/WAV"), Some("clip.wav"), b"bytes"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn content_type_with_charset_is_accepted() {
    let Some(app) = transcribe_router().await else {
        return;
    };
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
    let Some(app) = transcribe_router().await else {
        return;
    };
    let resp = app
        .oneshot(request(None, Some("clip.wav"), b"bytes"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn non_audio_content_type_is_rejected() {
    let Some(app) = transcribe_router().await else {
        return;
    };
    let resp = app
        .oneshot(request(Some("text/plain"), Some("clip.wav"), b"bytes"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn filename_path_segments_are_stripped() {
    let Some(app) = transcribe_router().await else {
        return;
    };
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
    let Some(app) = transcribe_router().await else {
        return;
    };
    let resp = app
        .oneshot(request(Some("audio/wav"), Some("clip.wav"), b""))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
