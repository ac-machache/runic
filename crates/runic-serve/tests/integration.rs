mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::{ServeConfig, router};
use runic_substrate::{ArtifactStore, LocalArtifactStore, SessionStore};
use runic_types::{ContentBlock, StopReason, TokenUsage};

use common::Harness;

struct PanicProvider;

#[async_trait]
impl Provider for PanicProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        panic!("PanicProvider: tests must not invoke the agent path");
    }
}

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
                ..Default::default()
            },
        })
    }
}

fn crud_router(h: &Harness) -> Router {
    h.single_router(common::agent(Arc::new(PanicProvider)))
}

fn scripted_router(h: &Harness) -> Router {
    h.single_router(common::agent(Arc::new(ScriptedProvider)))
}

fn upload_request(
    thread: &str,
    tenant: &str,
    mime: &str,
    filename: &str,
    bytes: &[u8],
) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/threads/{thread}/artifacts"))
        .header("content-type", mime)
        .header("x-runic-tenant", tenant)
        .header("x-runic-filename", filename)
        .body(Body::from(bytes.to_vec()))
        .unwrap()
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
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("stored event count did not reach {min_events}");
}

#[tokio::test]
async fn healthz_returns_ok() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
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
    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["service"], "runic-serve");
}

#[tokio::test]
async fn create_thread_returns_201_with_generated_id() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    let resp = app
        .oneshot(common::post_json("/threads", &h.tenant, "{}"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = common::body_json(resp).await;
    assert_eq!(body["tenant"], h.tenant.as_str());
    assert_eq!(body["event_count"], 0);
    assert!(body["thread_id"].as_str().is_some_and(|s| !s.is_empty()));
}

#[tokio::test]
async fn create_thread_honors_provided_id() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
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
    let body = common::body_json(resp).await;
    assert_eq!(body["thread_id"], "my-custom-id");
    assert_eq!(body["tenant"], "default");
}

#[tokio::test]
async fn list_threads_starts_empty() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    let resp = app
        .oneshot(common::get("/threads", &h.tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert!(body["threads"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn list_threads_rejects_invalid_cursor() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    let resp = app
        .oneshot(common::get("/threads?cursor=not-a-cursor", &h.tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_unknown_thread_returns_404() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    let resp = app
        .oneshot(common::get("/threads/never-created", &h.tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_then_get_thread_is_materialized() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    common::create_thread(&app, &h.tenant, "t1").await;

    let got = app
        .oneshot(common::get("/threads/t1", &h.tenant))
        .await
        .unwrap();
    assert_eq!(got.status(), StatusCode::OK);
    let body = common::body_json(got).await;
    assert_eq!(body["thread_id"], "t1");
    assert_eq!(body["event_count"], 0);
}

#[tokio::test]
async fn thread_events_unknown_thread_returns_404() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    let resp = app
        .oneshot(common::get("/threads/never-created/events", &h.tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn thread_state_unknown_thread_returns_404() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    let resp = app
        .oneshot(common::get("/threads/never-created/state", &h.tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_thread_returns_204() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    let resp = app
        .oneshot(common::delete("/threads/anything", &h.tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn delete_thread_removes_local_artifact_blobs() {
    let Some(h) = common::harness().await else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let artifact_store: Arc<dyn ArtifactStore> = Arc::new(LocalArtifactStore::new(root.path()));
    let app = router(
        ServeConfig::new(
            h.sessions.clone(),
            runic_substrate::Blobs::from(artifact_store.clone()),
            h.pool.clone(),
        )
        .agent("main", common::agent(Arc::new(PanicProvider))),
    );
    common::create_thread(&app, &h.tenant, "with-artifact").await;

    let resp = app
        .clone()
        .oneshot(upload_request(
            "with-artifact",
            &h.tenant,
            "text/plain",
            "note.txt",
            b"delete me",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let id = common::body_json(resp).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(root.path().join("blobs").join(&id).exists());

    let resp = app
        .oneshot(common::delete("/threads/with-artifact", &h.tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    assert!(!root.path().join("blobs").join(&id).exists());
    assert!(
        artifact_store
            .list(&h.tenant, "with-artifact")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn tenant_header_isolates_thread_listings() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    let alice = format!("{}-alice", h.tenant);
    let bob = format!("{}-bob", h.tenant);

    common::create_thread(&app, &alice, "alice-thread").await;
    common::create_thread(&app, &bob, "bob-thread").await;

    let resp = app.oneshot(common::get("/threads", &bob)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
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
async fn answering_missing_human_ask_returns_bad_request() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    let resp = app
        .oneshot(common::post_json(
            "/threads/t1/asks/missing-ask",
            &h.tenant,
            r#"{"answer":"yes"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(common::body_string(resp).await.contains("no deferred call"));
}

#[tokio::test]
async fn sequential_runs_on_same_thread_both_succeed() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = scripted_router(&h);

    let r1 = app
        .clone()
        .oneshot(common::wait_request("t1", &h.tenant, "one"))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    assert_eq!(common::body_json(r1).await["text"], "pong");

    let r2 = app
        .oneshot(common::wait_request("t1", &h.tenant, "two"))
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    assert_eq!(common::body_json(r2).await["text"], "pong");
}

#[tokio::test]
async fn run_persists_events_for_thread_history() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = scripted_router(&h);

    let resp = app
        .clone()
        .oneshot(common::wait_request(
            "persisted-thread",
            &h.tenant,
            "remember me",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(common::body_json(resp).await["text"], "pong");

    let events = wait_for_stored_events(h.store().as_ref(), &h.tenant, "persisted-thread", 4).await;
    assert!(events.iter().any(|stored| {
        matches!(&stored.event, runic_substrate::SessionEvent::Message { msg, .. }
            if msg.content.text_content().contains("remember me"))
    }));
    assert!(events.iter().any(|stored| {
        matches!(&stored.event, runic_substrate::SessionEvent::Message { msg, .. }
            if msg.content.text_content().contains("pong"))
    }));
    assert!(events.iter().any(|stored| {
        matches!(&stored.event, runic_substrate::SessionEvent::RunEnd { outcome, .. }
            if outcome.stop_reason.as_deref() == Some("end_turn"))
    }));

    let resp = app
        .oneshot(common::get("/threads/persisted-thread/events", &h.tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["events"].as_array().unwrap().len(), events.len());
}
