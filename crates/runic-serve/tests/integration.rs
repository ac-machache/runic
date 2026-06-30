//! Route-level integration tests against the new stack.
//!
//! CRUD endpoints (health, threads, tenant isolation) run against a
//! `PanicFactory`; the agent-hot path (SSE streaming) runs against a
//! `ScriptedProvider` that returns one text turn with no network.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use runic_agent::Agent;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::{AgentFactory, HumanHub, ServeConfig, router};
use runic_substrate::{
    ArtifactStore, LocalArtifactStore, MemoryArtifactStore, MemorySessionStore, SessionStore,
};
use runic_transcriber::{SpeechToText, TranscribeError, Transcript};
use runic_types::{ContentBlock, MessageContent, Role, StopReason, TokenUsage};
use serde_json::Value;

/// Build-on-demand fixture: panics if anyone tries to actually use the agent.
/// Fine for tests that only hit CRUD endpoints.
struct PanicFactory;

#[async_trait]
impl AgentFactory for PanicFactory {
    async fn build(&self, _: &str, _: &str) -> Agent {
        panic!("PanicFactory: tests must not invoke the agent path");
    }
}

fn make_router() -> axum::Router {
    router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agent_factory: Arc::new(PanicFactory),
        human_hub: Arc::new(HumanHub::new()),
    })
}

/// Minimal provider: one text turn, then `EndTurn`. The trait's default
/// `stream` wraps `complete`, emitting a `TextDelta` + `ContentComplete`, so
/// the agent surfaces a `pong` token without a network or API key.
struct ScriptedProvider;

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: "pong".into(),
                provider_metadata: None,
            }],
            stop_reason: StopReason::EndTurn,
            tool_calls: vec![],
            usage: TokenUsage {
                input_tokens: 1,
                output_tokens: 2,
            },
        })
    }
}

/// Factory that builds a real Agent backed by the scripted provider, keyed to
/// the requested session id (so persistence/replay line up).
struct ScriptedFactory;

#[async_trait]
impl AgentFactory for ScriptedFactory {
    async fn build(&self, _tenant: &str, session_id: &str) -> Agent {
        Agent::builder(Arc::new(ScriptedProvider), "alice", session_id)
            .system_prompt("test")
            .build()
    }
}

fn scripted_router() -> axum::Router {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    scripted_router_with_store(store)
}

fn scripted_router_with_store(store: Arc<dyn SessionStore>) -> axum::Router {
    router(ServeConfig {
        session_store: store,
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agent_factory: Arc::new(ScriptedFactory),
        human_hub: Arc::new(HumanHub::new()),
    })
}

struct FakeTranscriber {
    expected_filename: &'static str,
}

#[async_trait]
impl SpeechToText for FakeTranscriber {
    async fn transcribe(
        &self,
        audio: &[u8],
        filename: &str,
    ) -> Result<Transcript, TranscribeError> {
        assert_eq!(audio, b"audio-bytes");
        assert_eq!(filename, self.expected_filename);
        Ok(Transcript {
            text: "bonjour".into(),
            language: Some("fr".into()),
        })
    }
}

struct FailingTranscriber;

#[async_trait]
impl SpeechToText for FailingTranscriber {
    async fn transcribe(
        &self,
        _audio: &[u8],
        _filename: &str,
    ) -> Result<Transcript, TranscribeError> {
        Err(TranscribeError::Http("provider unavailable".into()))
    }
}

fn transcribe_router(transcriber: Option<Arc<dyn SpeechToText>>) -> axum::Router {
    router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber,
        agent_factory: Arc::new(PanicFactory),
        human_hub: Arc::new(HumanHub::new()),
    })
}

fn run_request(thread_id: &str, message: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/threads/{thread_id}/runs/stream"))
        .header("content-type", "application/json")
        .header("x-runic-tenant", "alice")
        .body(Body::from(format!(r#"{{"message":"{message}"}}"#)))
        .unwrap()
}

async fn body_to_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 10_000_000)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

async fn body_to_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn sse_data(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|json| serde_json::from_str(json.trim()).unwrap())
        .collect()
}

fn sse_ids(body: &str) -> Vec<u64> {
    body.lines()
        .filter_map(|line| line.strip_prefix("id:"))
        .map(|id| id.trim().parse().unwrap())
        .collect()
}

async fn wait_for_stored_events(
    store: &dyn SessionStore,
    tenant: &str,
    thread_id: &str,
    min_events: usize,
) -> Vec<runic_substrate::StoredEvent> {
    for _ in 0..50 {
        let events = store.read(tenant, thread_id).await.unwrap();
        if events.len() >= min_events {
            return events;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("stored event count did not reach {min_events}");
}

#[tokio::test]
async fn transcribe_requires_configured_backend() {
    let app = transcribe_router(None);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/transcribe")
                .header("content-type", "audio/wav")
                .header("x-runic-tenant", "alice")
                .body(Body::from("audio-bytes"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    let body = body_to_json(resp).await;
    assert_eq!(body["error"], "not_configured");
}

#[tokio::test]
async fn transcribe_rejects_empty_body() {
    let app = transcribe_router(Some(Arc::new(FakeTranscriber {
        expected_filename: "audio",
    })));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/transcribe")
                .header("content-type", "audio/wav")
                .header("x-runic-tenant", "alice")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_to_json(resp).await;
    assert_eq!(body["error"], "bad_request");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("empty audio body")
    );
}

#[tokio::test]
async fn transcribe_rejects_non_audio_content_type() {
    let app = transcribe_router(Some(Arc::new(FakeTranscriber {
        expected_filename: "audio",
    })));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/transcribe")
                .header("content-type", "text/plain")
                .header("x-runic-tenant", "alice")
                .body(Body::from("audio-bytes"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_to_json(resp).await;
    assert_eq!(body["error"], "bad_request");
}

#[tokio::test]
async fn transcribe_returns_text_and_cleans_filename() {
    let app = transcribe_router(Some(Arc::new(FakeTranscriber {
        expected_filename: "voice.wav",
    })));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/transcribe")
                .header("content-type", "audio/wav; charset=binary")
                .header("x-runic-tenant", "alice")
                .header("x-runic-filename", "../clips\\voice.wav")
                .body(Body::from("audio-bytes"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_json(resp).await;
    assert_eq!(body["text"], "bonjour");
    assert_eq!(body["language"], "fr");
}

#[tokio::test]
async fn transcribe_upstream_errors_are_bad_gateway() {
    let app = transcribe_router(Some(Arc::new(FailingTranscriber)));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/transcribe")
                .header("content-type", "audio/wav")
                .header("x-runic-tenant", "alice")
                .body(Body::from("audio-bytes"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let body = body_to_json(resp).await;
    assert_eq!(body["error"], "upstream");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("provider unavailable")
    );
}

#[tokio::test]
async fn healthz_returns_ok() {
    let app = make_router();
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
    let body = body_to_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["service"], "runic-serve");
}

#[tokio::test]
async fn create_thread_returns_201_with_generated_id() {
    let app = make_router();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/threads")
                .header("content-type", "application/json")
                .header("x-runic-tenant", "alice")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_to_json(resp).await;
    assert_eq!(body["tenant"], "alice");
    assert_eq!(body["event_count"], 0);
    assert!(body["thread_id"].as_str().is_some_and(|s| !s.is_empty()));
}

#[tokio::test]
async fn create_thread_honors_provided_id() {
    let app = make_router();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/threads")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"thread_id":"my-custom-id"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_to_json(resp).await;
    assert_eq!(body["thread_id"], "my-custom-id");
    assert_eq!(body["tenant"], "default");
}

#[tokio::test]
async fn list_threads_starts_empty() {
    let app = make_router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/threads")
                .header("x-runic-tenant", "alice")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_json(resp).await;
    assert!(body["threads"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn list_threads_rejects_invalid_cursor() {
    let app = make_router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/threads?cursor=not-a-cursor")
                .header("x-runic-tenant", "alice")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_unknown_thread_returns_404() {
    let app = make_router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/threads/never-created")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_then_get_thread_is_materialized() {
    let app = make_router();
    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/threads")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"thread_id":"t1"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);

    let got = app
        .oneshot(
            Request::builder()
                .uri("/threads/t1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(got.status(), StatusCode::OK);
    let body = body_to_json(got).await;
    assert_eq!(body["thread_id"], "t1");
    assert_eq!(body["event_count"], 0);
}

#[tokio::test]
async fn thread_events_unknown_thread_returns_404() {
    let app = make_router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/threads/never-created/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn thread_state_unknown_thread_returns_404() {
    let app = make_router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/threads/never-created/state")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_thread_returns_204() {
    let app = make_router();
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/threads/anything")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn delete_thread_removes_local_artifact_blobs() {
    let root = tempfile::tempdir().unwrap();
    let artifact_store: Arc<dyn ArtifactStore> = Arc::new(LocalArtifactStore::new(root.path()));
    let app = router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        artifact_store: artifact_store.clone(),
        transcriber: None,
        agent_factory: Arc::new(PanicFactory),
        human_hub: Arc::new(HumanHub::new()),
    });
    create_thread(&app, "with-artifact").await;

    let resp = app
        .clone()
        .oneshot(upload_request(
            "with-artifact",
            "text/plain",
            "note.txt",
            b"delete me",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let id = body_to_json(resp).await["id"].as_str().unwrap().to_string();
    assert!(root.path().join("blobs").join(&id).exists());

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/threads/with-artifact")
                .header("x-runic-tenant", "alice")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    assert!(!root.path().join("blobs").join(&id).exists());
    assert!(
        artifact_store
            .list("alice", "with-artifact")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn tenant_header_isolates_thread_listings() {
    let app = router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agent_factory: Arc::new(PanicFactory),
        human_hub: Arc::new(HumanHub::new()),
    });

    // Alice creates a thread via CRUD (no agent path → PanicFactory is safe).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/threads")
                .header("content-type", "application/json")
                .header("x-runic-tenant", "alice")
                .body(Body::from(r#"{"thread_id":"alice-thread"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/threads")
                .header("content-type", "application/json")
                .header("x-runic-tenant", "bob")
                .body(Body::from(r#"{"thread_id":"bob-thread"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Bob lists their own threads — must not see alice's.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/threads")
                .header("x-runic-tenant", "bob")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_json(resp).await;
    let ids: Vec<&str> = body["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["thread_id"].as_str().unwrap())
        .collect();
    assert!(
        !ids.contains(&"alice-thread"),
        "bob should not see alice's thread; got {ids:?}"
    );
}

#[tokio::test]
async fn run_streams_agent_events() {
    let app = scripted_router();
    let resp = app.oneshot(run_request("t1", "ping")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_string(resp).await;
    assert!(
        body.contains("pong"),
        "streamed text missing from body: {body}"
    );
}

#[tokio::test]
async fn run_persists_events_for_thread_history() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let app = scripted_router_with_store(store.clone());

    let resp = app
        .clone()
        .oneshot(run_request("persisted-thread", "remember me"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_to_string(resp).await.contains("pong"));

    let events = wait_for_stored_events(store.as_ref(), "alice", "persisted-thread", 5).await;
    assert!(events.iter().any(|stored| {
        matches!(&stored.event, runic_state::SessionEvent::Message { msg, .. }
            if msg.content.text_content().contains("remember me"))
    }));
    assert!(events.iter().any(|stored| {
        matches!(&stored.event, runic_state::SessionEvent::Message { msg, .. }
            if msg.content.text_content().contains("pong"))
    }));
    assert!(events.iter().any(|stored| {
        matches!(&stored.event, runic_state::SessionEvent::RunEnd { outcome, .. }
            if outcome.stop_reason.as_deref() == Some("end_turn"))
    }));

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/threads/persisted-thread/events")
                .header("x-runic-tenant", "alice")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_json(resp).await;
    assert_eq!(body["events"].as_array().unwrap().len(), events.len());
}

#[tokio::test]
async fn replay_run_respects_last_event_id_and_finishes_closed_run() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let app = scripted_router_with_store(store.clone());

    let resp = app
        .clone()
        .oneshot(run_request("replay-thread", "first"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let live_body = body_to_string(resp).await;
    let run_id = sse_data(&live_body)
        .into_iter()
        .find_map(|event| {
            (event["type"] == "run_start").then(|| event["run_id"].as_str().unwrap().to_string())
        })
        .expect("live stream contains run_start");

    let stored = wait_for_stored_events(store.as_ref(), "alice", "replay-thread", 5).await;
    let after_seq = stored
        .iter()
        .find(|stored| matches!(stored.event, runic_state::SessionEvent::RunStart { .. }))
        .expect("run start persisted")
        .seq;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/threads/replay-thread/runs/{run_id}/stream"))
                .header("x-runic-tenant", "alice")
                .header("last-event-id", after_seq.to_string())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let replay_body = tokio::time::timeout(std::time::Duration::from_secs(1), body_to_string(resp))
        .await
        .expect("replay stream should finish for a closed run");

    assert!(!replay_body.contains("event: run_start"));
    assert!(replay_body.contains("event: message"));
    assert!(replay_body.contains("event: run_end"));
    assert!(replay_body.contains("event: done"));
    assert!(sse_ids(&replay_body).iter().all(|seq| *seq > after_seq));
}

#[tokio::test]
async fn sequential_runs_on_same_thread_both_succeed() {
    // The second run can only proceed once the first releases the thread's
    // slot mutex — proving the warm agent is reused across runs.
    let app = scripted_router();

    let r1 = app.clone().oneshot(run_request("t1", "one")).await.unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    assert!(body_to_string(r1).await.contains("pong"));

    let r2 = app.clone().oneshot(run_request("t1", "two")).await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    assert!(body_to_string(r2).await.contains("pong"));
}

#[tokio::test]
async fn abandoned_run_does_not_brick_the_thread() {
    // Drop the response without reading it (client disconnect); the detached
    // run task still drives the agent to completion, so a follow-up run on the
    // same thread succeeds.
    let app = scripted_router();

    let abandoned = app
        .clone()
        .oneshot(run_request("t1", "abandon"))
        .await
        .unwrap();
    assert_eq!(abandoned.status(), StatusCode::OK);
    drop(abandoned);

    let resp = app
        .clone()
        .oneshot(run_request("t1", "after"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_to_string(resp).await.contains("pong"));
}

#[tokio::test]
async fn answering_missing_human_ask_returns_bad_request() {
    let app = make_router();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/threads/t1/asks/missing-ask")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"answer":"yes"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(body_to_string(resp).await.contains("no pending ask"));
}

// ── artifacts ────────────────────────────────────────────────────────────

fn upload_request(thread: &str, mime: &str, filename: &str, bytes: &[u8]) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/threads/{thread}/artifacts"))
        .header("content-type", mime)
        .header("x-runic-tenant", "alice")
        .header("x-runic-filename", filename)
        .body(Body::from(bytes.to_vec()))
        .unwrap()
}

async fn create_thread(app: &axum::Router, thread_id: &str) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/threads")
                .header("content-type", "application/json")
                .header("x-runic-tenant", "alice")
                .body(Body::from(format!(r#"{{"thread_id":"{thread_id}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn upload_artifact_stores_and_lists() {
    let app = scripted_router();
    create_thread(&app, "t1").await;
    let bytes = b"%PDF-1.7 hello world";

    let resp = app
        .clone()
        .oneshot(upload_request("t1", "application/pdf", "doc.pdf", bytes))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = body_to_json(resp).await;
    let id = v["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("art-"));
    assert_eq!(v["size"].as_u64().unwrap(), bytes.len() as u64);
    assert_eq!(v["mime_type"], "application/pdf");
    assert_eq!(v["filename"], "doc.pdf");

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/threads/t1/artifacts")
                .header("x-runic-tenant", "alice")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_to_json(resp).await;
    assert!(list.as_array().unwrap().iter().any(|a| a["id"] == id));
}

#[tokio::test]
async fn empty_upload_is_rejected() {
    let app = scripted_router();
    let resp = app
        .oneshot(upload_request("t1", "application/pdf", "x.pdf", b""))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn run_with_artifact_ref_persists_only_the_reference() {
    let (app, store, _artifacts, _last) = resolving_setup();
    create_thread(&app, "reflike").await;

    // Upload first, then reference the returned id from the run.
    let resp = app
        .clone()
        .oneshot(upload_request(
            "reflike",
            "application/pdf",
            "r.pdf",
            b"%PDF some bytes",
        ))
        .await
        .unwrap();
    let id = body_to_json(resp).await["id"].as_str().unwrap().to_string();

    let body = serde_json::json!({
        "content": [
            { "type": "text", "text": "summarize the file" },
            { "type": "artifact_ref", "id": id, "media_type": "application/pdf", "filename": "r.pdf" }
        ]
    })
    .to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/threads/reflike/runs/stream")
        .header("content-type", "application/json")
        .header("x-runic-tenant", "alice")
        .body(Body::from(body))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_to_string(resp).await.contains("pong"));

    let events = wait_for_stored_events(store.as_ref(), "alice", "reflike", 3).await;
    // The event log keeps the lean pointer …
    let kept_ref = events.iter().any(|stored| {
        matches!(&stored.event, runic_state::SessionEvent::Message { msg, .. }
            if matches!(&msg.content, MessageContent::Blocks(b)
                if b.iter().any(|c| matches!(c, ContentBlock::ArtifactRef { id: rid, .. } if rid == &id))))
    });
    assert!(kept_ref, "the artifact_ref pointer should be persisted");
    // … and never the inline bytes.
    let inlined = events.iter().any(|stored| {
        matches!(&stored.event, runic_state::SessionEvent::Message { msg, .. }
            if matches!(&msg.content, MessageContent::Blocks(b)
                if b.iter().any(|c| matches!(c, ContentBlock::Image { .. } | ContentBlock::File { .. }))))
    });
    assert!(
        !inlined,
        "no inline image/file bytes should reach the event log"
    );
}

#[tokio::test]
async fn inline_media_in_run_body_is_stored_as_a_ref() {
    let (app, store, _artifacts, _last) = resolving_setup();
    create_thread(&app, "inline").await;

    // Client posts inline base64 media directly to /runs/stream (bypassing
    // /artifacts) — the server must store it and persist only a ref.
    let body = serde_json::json!({
        "content": [
            { "type": "text", "text": "look" },
            { "type": "image", "media_type": "image/png", "data": "aGVsbG8gcG5n" }
        ]
    })
    .to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/threads/inline/runs/stream")
        .header("content-type", "application/json")
        .header("x-runic-tenant", "alice")
        .body(Body::from(body))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_to_string(resp).await.contains("pong"));

    let events = wait_for_stored_events(store.as_ref(), "alice", "inline", 3).await;
    let kept_ref = events.iter().any(|stored| {
        matches!(&stored.event, runic_state::SessionEvent::Message { msg, .. }
            if matches!(&msg.content, MessageContent::Blocks(b)
                if b.iter().any(|c| matches!(c, ContentBlock::ArtifactRef { .. }))))
    });
    assert!(kept_ref, "inline media should be persisted as a ref");
    let inlined = events.iter().any(|stored| {
        matches!(&stored.event, runic_state::SessionEvent::Message { msg, .. }
            if matches!(&msg.content, MessageContent::Blocks(b)
                if b.iter().any(|c| matches!(c, ContentBlock::Image { .. } | ContentBlock::File { .. }))))
    });
    assert!(!inlined, "no inline bytes should reach the event log");
}

#[tokio::test]
async fn run_body_ref_persists_canonical_mime_not_the_clients_claim() {
    let (app, session, _artifacts, _last) = resolving_setup();
    create_thread(&app, "mimethread").await;

    // Stored as a PDF.
    let resp = app
        .clone()
        .oneshot(upload_request(
            "mimethread",
            "application/pdf",
            "r.pdf",
            b"%PDF bytes",
        ))
        .await
        .unwrap();
    let id = body_to_json(resp).await["id"].as_str().unwrap().to_string();

    // The run references it but LIES that it's an image.
    let body = serde_json::json!({
        "content": [
            { "type": "text", "text": "hi" },
            { "type": "artifact_ref", "id": id, "media_type": "image/png" }
        ]
    })
    .to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/threads/mimethread/runs/stream")
        .header("content-type", "application/json")
        .header("x-runic-tenant", "alice")
        .body(Body::from(body))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_to_string(resp).await.contains("pong"));

    let events = wait_for_stored_events(session.as_ref(), "alice", "mimethread", 3).await;
    let canonical = events.iter().any(|stored| {
        matches!(&stored.event, runic_state::SessionEvent::Message { msg, .. }
            if matches!(&msg.content, MessageContent::Blocks(b)
                if b.iter().any(|c| matches!(c,
                    ContentBlock::ArtifactRef { media_type, .. } if media_type == "application/pdf"))))
    });
    assert!(canonical, "persisted ref should carry the stored mime");
    let lied = events.iter().any(|stored| {
        matches!(&stored.event, runic_state::SessionEvent::Message { msg, .. }
            if matches!(&msg.content, MessageContent::Blocks(b)
                if b.iter().any(|c| matches!(c,
                    ContentBlock::ArtifactRef { media_type, .. } if media_type == "image/png"))))
    });
    assert!(!lied, "the client's fake mime must not be persisted");
}

#[tokio::test]
async fn foreign_artifact_ref_in_run_body_is_rejected() {
    let app = scripted_router();
    let body = serde_json::json!({
        "content": [
            { "type": "artifact_ref", "id": "art-not-mine", "media_type": "image/png" }
        ]
    })
    .to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/threads/whatever/runs/stream")
        .header("content-type", "application/json")
        .header("x-runic-tenant", "alice")
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rejected_run_stores_no_orphan_artifacts() {
    let (app, _session, artifacts, _last) = resolving_setup();
    create_thread(&app, "orphan").await;

    // A valid inline image followed by a foreign ref → the whole request is
    // rejected, and the inline block before it must not have been stored.
    let body = serde_json::json!({
        "content": [
            { "type": "image", "media_type": "image/png", "data": "aGVsbG8=" },
            { "type": "artifact_ref", "id": "art-not-mine", "media_type": "image/png" }
        ]
    })
    .to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/threads/orphan/runs/stream")
        .header("content-type", "application/json")
        .header("x-runic-tenant", "alice")
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let stored = artifacts.list("alice", "orphan").await.unwrap();
    assert!(
        stored.is_empty(),
        "a rejected request must store no artifacts"
    );
}

/// Records the (post-resolution) request the model layer receives.
struct RecordingProvider {
    last: Arc<std::sync::Mutex<Option<CompletionRequest>>>,
}

#[async_trait]
impl Provider for RecordingProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        *self.last.lock().unwrap() = Some(req);
        Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: "pong".into(),
                provider_metadata: None,
            }],
            stop_reason: StopReason::EndTurn,
            tool_calls: vec![],
            usage: TokenUsage::default(),
        })
    }
}

/// Builds a real agent on the foundry `ArtifactResolver`, so a run through serve
/// exercises the actual resolution path.
struct ResolvingFactory {
    store: Arc<dyn ArtifactStore>,
    last: Arc<std::sync::Mutex<Option<CompletionRequest>>>,
}

#[async_trait]
impl AgentFactory for ResolvingFactory {
    async fn build(&self, tenant: &str, session_id: &str) -> Agent {
        Agent::builder(
            Arc::new(RecordingProvider {
                last: self.last.clone(),
            }),
            tenant,
            session_id,
        )
        .system_prompt("test")
        .media_resolver(Arc::new(runic_foundry::ArtifactResolver::new(
            self.store.clone(),
            tenant,
            session_id,
        )))
        .build()
    }
}

type Recorder = Arc<std::sync::Mutex<Option<CompletionRequest>>>;

fn resolving_setup() -> (
    axum::Router,
    Arc<dyn SessionStore>,
    Arc<dyn ArtifactStore>,
    Recorder,
) {
    let session: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let artifacts: Arc<dyn ArtifactStore> = Arc::new(MemoryArtifactStore::new());
    let last: Recorder = Arc::new(std::sync::Mutex::new(None));
    let app = router(ServeConfig {
        session_store: session.clone(),
        artifact_store: artifacts.clone(),
        transcriber: None,
        agent_factory: Arc::new(ResolvingFactory {
            store: artifacts.clone(),
            last: last.clone(),
        }),
        human_hub: Arc::new(HumanHub::new()),
    });
    (app, session, artifacts, last)
}

#[tokio::test]
async fn run_resolves_latest_artifact_ref_through_serve() {
    let (app, _session, _artifacts, last) = resolving_setup();

    create_thread(&app, "img-thread").await;
    let png = b"\x89PNG fake png bytes";
    let resp = app
        .clone()
        .oneshot(upload_request("img-thread", "image/png", "p.png", png))
        .await
        .unwrap();
    let id = body_to_json(resp).await["id"].as_str().unwrap().to_string();

    let body = serde_json::json!({
        "content": [
            { "type": "text", "text": "what is in this image" },
            { "type": "artifact_ref", "id": id, "media_type": "image/png", "filename": "p.png" }
        ]
    })
    .to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/threads/img-thread/runs/stream")
        .header("content-type", "application/json")
        .header("x-runic-tenant", "alice")
        .body(Body::from(body))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = body_to_string(resp).await; // drive the stream to completion

    // The model layer received the RESOLVED request: an inline image, no ref.
    let captured = last.lock().unwrap().clone().expect("a model request");
    let user = captured
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .expect("a user message");
    let blocks = match &user.content {
        MessageContent::Blocks(b) => b.clone(),
        MessageContent::Text(_) => vec![],
    };
    assert!(
        blocks
            .iter()
            .any(|c| matches!(c, ContentBlock::Image { media_type, data }
        if media_type == "image/png" && !data.is_empty()))
    );
    assert!(
        !blocks
            .iter()
            .any(|c| matches!(c, ContentBlock::ArtifactRef { .. }))
    );
}
