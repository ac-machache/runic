use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use runic_agent::Agent;
use runic_serve::{AgentFactory, HumanHub, ServeConfig, router};
use runic_substrate::{MemoryArtifactStore, MemorySessionStore};

const TENANT: &str = "alice";

struct PanicFactory;

#[async_trait]
impl AgentFactory for PanicFactory {
    async fn build(&self, _: &str, _: &str) -> Agent {
        panic!("agent path must not run here");
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

fn get(uri: &str, tenant: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("x-runic-tenant", tenant)
        .body(Body::empty())
        .unwrap()
}

fn upload(
    thread: &str,
    tenant: &str,
    content_type: Option<&str>,
    filename: Option<&str>,
    bytes: &[u8],
) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(format!("/threads/{thread}/artifacts"))
        .header("x-runic-tenant", tenant);
    if let Some(ct) = content_type {
        b = b.header("content-type", ct);
    }
    if let Some(fname) = filename {
        b = b.header("x-runic-filename", fname);
    }
    b.body(Body::from(bytes.to_vec())).unwrap()
}

async fn create_thread(app: &Router, tenant: &str, thread_id: &str) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/threads")
                .header("content-type", "application/json")
                .header("x-runic-tenant", tenant)
                .body(Body::from(json!({ "thread_id": thread_id }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn upload_to_unknown_thread_is_404() {
    let app = crud_router();
    let resp = app
        .oneshot(upload(
            "ghost",
            TENANT,
            Some("text/plain"),
            Some("a.txt"),
            b"hi",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_unknown_thread_is_404() {
    let app = crud_router();
    let resp = app
        .oneshot(get("/threads/ghost/artifacts", TENANT))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn upload_without_content_type_defaults_to_octet_stream() {
    let app = crud_router();
    create_thread(&app, TENANT, "t1").await;
    let resp = app
        .oneshot(upload("t1", TENANT, None, Some("blob.bin"), b"raw"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        body_json(resp).await["mime_type"],
        "application/octet-stream"
    );
}

#[tokio::test]
async fn upload_canonicalizes_content_type_with_charset() {
    let app = crud_router();
    create_thread(&app, TENANT, "t1").await;
    let resp = app
        .oneshot(upload(
            "t1",
            TENANT,
            Some("text/plain; charset=utf-8"),
            Some("a.txt"),
            b"hi",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(body_json(resp).await["mime_type"], "text/plain");
}

#[tokio::test]
async fn upload_trims_filename() {
    let app = crud_router();
    create_thread(&app, TENANT, "t1").await;
    let resp = app
        .oneshot(upload(
            "t1",
            TENANT,
            Some("text/plain"),
            Some("  note.txt  "),
            b"hi",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(body_json(resp).await["filename"], "note.txt");
}

#[tokio::test]
async fn filename_does_not_change_stored_size_or_type() {
    let app = crud_router();
    create_thread(&app, TENANT, "t1").await;
    let a = body_json(
        app.clone()
            .oneshot(upload(
                "t1",
                TENANT,
                Some("text/plain"),
                Some("one.txt"),
                b"same",
            ))
            .await
            .unwrap(),
    )
    .await;
    let b = body_json(
        app.clone()
            .oneshot(upload(
                "t1",
                TENANT,
                Some("text/plain"),
                Some("two.txt"),
                b"same",
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(a["size"], b["size"]);
    assert_eq!(a["mime_type"], b["mime_type"]);

    let list = body_json(
        app.oneshot(get("/threads/t1/artifacts", TENANT))
            .await
            .unwrap(),
    )
    .await;
    assert!(list.as_array().unwrap().len() >= 2);
}

#[tokio::test]
async fn wrong_tenant_cannot_upload_to_foreign_thread() {
    let app = crud_router();
    create_thread(&app, "alice", "shared-id").await;
    let resp = app
        .oneshot(upload(
            "shared-id",
            "bob",
            Some("text/plain"),
            Some("x.txt"),
            b"hi",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn wrong_tenant_cannot_list_foreign_thread() {
    let app = crud_router();
    create_thread(&app, "alice", "shared-id").await;
    let resp = app
        .oneshot(get("/threads/shared-id/artifacts", "bob"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
