mod common;

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};

use common::Harness;

struct PanicProvider;

#[async_trait]
impl Provider for PanicProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        panic!("agent path must not run here");
    }
}

fn crud_router(h: &Harness) -> Router {
    h.single_router(common::agent(Arc::new(PanicProvider)))
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

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn upload_to_unknown_thread_is_404() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    let resp = app
        .oneshot(upload(
            "ghost",
            &h.tenant,
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
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    let resp = app
        .oneshot(common::get("/threads/ghost/artifacts", &h.tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn upload_without_content_type_defaults_to_octet_stream() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    common::create_thread(&app, &h.tenant, "t1").await;
    let resp = app
        .oneshot(upload("t1", &h.tenant, None, Some("blob.bin"), b"raw"))
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
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    common::create_thread(&app, &h.tenant, "t1").await;
    let resp = app
        .oneshot(upload(
            "t1",
            &h.tenant,
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
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    common::create_thread(&app, &h.tenant, "t1").await;
    let resp = app
        .oneshot(upload(
            "t1",
            &h.tenant,
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
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    common::create_thread(&app, &h.tenant, "t1").await;
    let a = body_json(
        app.clone()
            .oneshot(upload(
                "t1",
                &h.tenant,
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
                &h.tenant,
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
        app.oneshot(common::get("/threads/t1/artifacts", &h.tenant))
            .await
            .unwrap(),
    )
    .await;
    assert!(list.as_array().unwrap().len() >= 2);
}

#[tokio::test]
async fn wrong_tenant_cannot_upload_to_foreign_thread() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    common::create_thread(&app, &h.tenant, "shared-id").await;
    let resp = app
        .oneshot(upload(
            "shared-id",
            "someone-else",
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
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    common::create_thread(&app, &h.tenant, "shared-id").await;
    let resp = app
        .oneshot(common::get("/threads/shared-id/artifacts", "someone-else"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

async fn upload_ok(app: &Router, thread: &str, tenant: &str, bytes: &[u8]) -> String {
    let resp = app
        .clone()
        .oneshot(upload(thread, tenant, Some("image/png"), None, bytes))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    body_json(resp).await["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn download_returns_the_bytes_with_the_stored_type() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    common::create_thread(&app, &h.tenant, "t1").await;
    let id = upload_ok(&app, "t1", &h.tenant, b"\x89PNG fake image bytes").await;

    let resp = app
        .oneshot(common::get(
            &format!("/threads/t1/artifacts/{id}"),
            &h.tenant,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "image/png");
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"\x89PNG fake image bytes");
}

#[tokio::test]
async fn download_unknown_artifact_is_404() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    common::create_thread(&app, &h.tenant, "t1").await;
    let resp = app
        .oneshot(common::get("/threads/t1/artifacts/ghost", &h.tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["error"], "not_found");
}

#[tokio::test]
async fn download_from_another_thread_is_404() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    common::create_thread(&app, &h.tenant, "t1").await;
    common::create_thread(&app, &h.tenant, "t2").await;
    let id = upload_ok(&app, "t1", &h.tenant, b"secret").await;

    let resp = app
        .oneshot(common::get(
            &format!("/threads/t2/artifacts/{id}"),
            &h.tenant,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn wrong_tenant_cannot_download_foreign_artifact() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    common::create_thread(&app, &h.tenant, "t1").await;
    let id = upload_ok(&app, "t1", &h.tenant, b"secret").await;

    let resp = app
        .oneshot(common::get(
            &format!("/threads/t1/artifacts/{id}"),
            "someone-else",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
