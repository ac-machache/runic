#![cfg(feature = "postgres")]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::{ServeConfig, router};
use runic_substrate::{
    ArtifactStore, Blobs, LocalArtifactStore, PostgresArtifactStore, PostgresSessionStore,
    SessionStore, Sessions,
};
use runic_types::{ContentBlock, StopReason, TokenUsage};

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

async fn pg_router(root: &Path) -> Option<(Router, Arc<dyn SessionStore>, Arc<dyn ArtifactStore>)> {
    let pool = common::test_pool().await?;
    let sessions = PostgresSessionStore::from_pool(pool.clone())
        .await
        .expect("connect session store");
    runic_serve::store::migrate(&pool)
        .await
        .expect("serve run schema setup");
    let bytes: Arc<dyn ArtifactStore> = Arc::new(LocalArtifactStore::new(root));
    let artifacts = PostgresArtifactStore::from_pool(pool.clone(), bytes, "local")
        .await
        .expect("connect artifact store");

    let sessions: Arc<dyn SessionStore> = Arc::new(sessions);
    let artifacts: Arc<dyn ArtifactStore> = Arc::new(artifacts);
    let app = router(
        ServeConfig::new(
            Sessions::from(sessions.clone()),
            Blobs::from(artifacts.clone()),
            pool,
        )
        .agent("main", common::agent(Arc::new(ScriptedProvider))),
    );
    Some((app, sessions, artifacts))
}

async fn wait_for_events(store: &dyn SessionStore, tenant: &str, session: &str, min: usize) {
    for _ in 0..100 {
        if store.read(tenant, session).await.unwrap().len() >= min {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("stored event count did not reach {min}");
}

#[tokio::test]
async fn full_lifecycle_on_postgres() {
    let root = tempfile::tempdir().unwrap();
    let Some((app, sessions, artifacts)) = pg_router(root.path()).await else {
        return;
    };
    let tenant = common::uid("tenant");
    let session = common::uid("session");

    common::create_session(&app, &tenant, &session).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/sessions/{session}/artifacts"))
                .header("content-type", "text/plain")
                .header("x-runic-tenant", &tenant)
                .header("x-runic-filename", "note.txt")
                .body(Body::from("blob bytes"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let art_id = common::body_json(resp).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(root.path().join("blobs").join(&art_id).exists());

    let resp = app
        .clone()
        .oneshot(common::wait_request(&session, &tenant, "hello"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = common::body_json(resp).await;
    assert_eq!(body["text"], "pong");
    let run_id = body["run_id"].as_str().unwrap().to_string();

    wait_for_events(sessions.as_ref(), &tenant, &session, 4).await;

    let resp = app
        .clone()
        .oneshot(common::get(&format!("/sessions/{session}/events"), &tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        !common::body_json(resp).await["events"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let resp = app
        .clone()
        .oneshot(common::get(
            &format!("/sessions/{session}/runs/{run_id}/timeline"),
            &tenant,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let trace = common::body_json(resp).await;
    assert_eq!(trace["run_id"], run_id.as_str());

    let resp = app
        .clone()
        .oneshot(common::delete(&format!("/sessions/{session}"), &tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    assert!(
        sessions
            .session_meta(&tenant, &session)
            .await
            .unwrap()
            .is_none()
    );
    assert!(artifacts.list(&tenant, &session).await.unwrap().is_empty());
    assert!(!root.path().join("blobs").join(&art_id).exists());
}

#[tokio::test]
async fn tenant_isolation_on_postgres() {
    let root = tempfile::tempdir().unwrap();
    let Some((app, _sessions, _artifacts)) = pg_router(root.path()).await else {
        return;
    };
    let tenant_a = common::uid("tenant");
    let tenant_b = common::uid("tenant");
    let session_a = common::uid("session");
    let session_b = common::uid("session");

    common::create_session(&app, &tenant_a, &session_a).await;
    common::create_session(&app, &tenant_b, &session_b).await;

    let resp = app
        .clone()
        .oneshot(common::get("/sessions", &tenant_a))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ids: Vec<String> = common::body_json(resp).await["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["session_id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&session_a));
    assert!(
        !ids.contains(&session_b),
        "tenant A leaked tenant B's session"
    );
}

#[tokio::test]
async fn wait_run_persists_the_full_lifecycle_on_postgres() {
    let root = tempfile::tempdir().unwrap();
    let Some((app, sessions, _artifacts)) = pg_router(root.path()).await else {
        return;
    };
    let tenant = common::uid("tenant");
    let session = common::uid("session");

    let resp = app
        .oneshot(common::wait_request(&session, &tenant, "hi"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    wait_for_events(sessions.as_ref(), &tenant, &session, 4).await;
    let events = sessions.read(&tenant, &session).await.unwrap();
    assert!(events.iter().any(|stored| {
        matches!(&stored.event, runic_substrate::SessionEvent::RunEnd { outcome, .. }
            if outcome.stop_reason.as_deref() == Some("end_turn"))
    }));
}
