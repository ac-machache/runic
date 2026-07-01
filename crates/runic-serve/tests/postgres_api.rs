#![cfg(feature = "postgres")]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use runic_agent::Agent;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::{AgentFactory, HumanHub, ServeConfig, router};
use runic_substrate::{
    ArtifactStore, LocalArtifactStore, PostgresArtifactStore, PostgresSessionStore, SessionStore,
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
            },
        })
    }
}

struct ScriptedFactory;

#[async_trait]
impl AgentFactory for ScriptedFactory {
    async fn build(&self, tenant: &str, session_id: &str) -> Agent {
        Agent::builder(Arc::new(ScriptedProvider), tenant, session_id)
            .system_prompt("test")
            .build()
    }
}

async fn pg_stores(root: &Path) -> Option<(Arc<dyn SessionStore>, Arc<dyn ArtifactStore>)> {
    let Ok(url) = std::env::var("RUNIC_TEST_DATABASE_URL") else {
        static NOTED: AtomicBool = AtomicBool::new(false);
        if !NOTED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "\n⚠  RUNIC_TEST_DATABASE_URL not set — postgres_api tests SKIPPED (NOT verified). \
                 Run scripts/test-postgres.sh to verify.\n"
            );
        }
        return None;
    };
    let sessions = PostgresSessionStore::connect(&url)
        .await
        .expect("connect session store");
    let bytes: Arc<dyn ArtifactStore> = Arc::new(LocalArtifactStore::new(root));
    let artifacts = PostgresArtifactStore::connect(&url, bytes, "local")
        .await
        .expect("connect artifact store");
    Some((Arc::new(sessions), Arc::new(artifacts)))
}

fn make_router(sessions: Arc<dyn SessionStore>, artifacts: Arc<dyn ArtifactStore>) -> Router {
    router(ServeConfig {
        session_store: sessions,
        artifact_store: artifacts,
        transcriber: None,
        agent_factory: Arc::new(ScriptedFactory),
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

fn upload(thread: &str, tenant: &str, bytes: &[u8]) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/threads/{thread}/artifacts"))
        .header("content-type", "text/plain")
        .header("x-runic-tenant", tenant)
        .header("x-runic-filename", "note.txt")
        .body(Body::from(bytes.to_vec()))
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 10_000_000)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn find_run_id(body: &str) -> Option<String> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|j| serde_json::from_str::<Value>(j.trim()).ok())
        .find_map(|e| (e["type"] == "run_start").then(|| e["run_id"].as_str().unwrap().to_string()))
}

async fn wait_for_events(store: &dyn SessionStore, tenant: &str, thread: &str, min: usize) {
    for _ in 0..100 {
        if store.read(tenant, thread).await.unwrap().len() >= min {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("stored event count did not reach {min}");
}

async fn create_thread(app: &Router, tenant: &str, thread: &str) {
    let resp = app
        .clone()
        .oneshot(post_json(
            "/threads",
            tenant,
            json!({ "thread_id": thread }).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn full_lifecycle_on_postgres() {
    let root = tempfile::tempdir().unwrap();
    let Some((sessions, artifacts)) = pg_stores(root.path()).await else {
        return;
    };
    let app = make_router(sessions.clone(), artifacts.clone());
    let tenant = uuid::Uuid::new_v4().to_string();
    let thread = uuid::Uuid::new_v4().to_string();

    create_thread(&app, &tenant, &thread).await;

    let resp = app
        .clone()
        .oneshot(upload(&thread, &tenant, b"blob bytes"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let art_id = body_json(resp).await["id"].as_str().unwrap().to_string();
    assert!(root.path().join("blobs").join(&art_id).exists());

    let resp = app
        .clone()
        .oneshot(post_json(
            &format!("/threads/{thread}/runs/stream"),
            &tenant,
            json!({ "message": "hello" }).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let run_body = body_string(resp).await;
    assert!(run_body.contains("pong"), "{run_body}");
    let run_id = find_run_id(&run_body).expect("run_start in stream");
    wait_for_events(sessions.as_ref(), &tenant, &thread, 4).await;

    let resp = app
        .clone()
        .oneshot(get(&format!("/threads/{thread}/events"), &tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        !body_json(resp).await["events"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let resp = app
        .clone()
        .oneshot(get(
            &format!("/threads/{thread}/runs/{run_id}/stream"),
            &tenant,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let replay = tokio::time::timeout(Duration::from_secs(5), body_string(resp))
        .await
        .expect("replay finishes for a closed run");
    assert!(replay.contains("event: run_end"), "{replay}");
    assert!(replay.contains("event: done"));

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/threads/{thread}"))
                .header("x-runic-tenant", &tenant)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    assert!(
        sessions
            .session_meta(&tenant, &thread)
            .await
            .unwrap()
            .is_none()
    );
    assert!(artifacts.list(&tenant, &thread).await.unwrap().is_empty());
    assert!(!root.path().join("blobs").join(&art_id).exists());
}

#[tokio::test]
async fn tenant_isolation_on_postgres() {
    let root = tempfile::tempdir().unwrap();
    let Some((sessions, artifacts)) = pg_stores(root.path()).await else {
        return;
    };
    let app = make_router(sessions, artifacts);
    let tenant_a = uuid::Uuid::new_v4().to_string();
    let tenant_b = uuid::Uuid::new_v4().to_string();
    let thread_a = uuid::Uuid::new_v4().to_string();
    let thread_b = uuid::Uuid::new_v4().to_string();

    create_thread(&app, &tenant_a, &thread_a).await;
    create_thread(&app, &tenant_b, &thread_b).await;

    let resp = app
        .clone()
        .oneshot(get("/threads", &tenant_a))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ids: Vec<String> = body_json(resp).await["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["thread_id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&thread_a));
    assert!(
        !ids.contains(&thread_b),
        "tenant A leaked tenant B's thread"
    );
}
