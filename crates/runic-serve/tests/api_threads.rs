use std::sync::Arc;
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
use runic_substrate::{MemoryArtifactStore, MemorySessionStore, SessionStore};
use runic_types::{ContentBlock, StopReason, TokenUsage};

const TENANT: &str = "alice";

struct PanicFactory;

#[async_trait]
impl AgentFactory for PanicFactory {
    async fn build(&self, _: &str, _: &str) -> Agent {
        panic!("agent path must not run in CRUD tests");
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

fn crud_router() -> Router {
    router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agent_factory: Arc::new(PanicFactory),
        human_hub: Arc::new(HumanHub::new()),
    })
}

fn scripted_router_with_store(store: Arc<dyn SessionStore>) -> Router {
    router(ServeConfig {
        session_store: store,
        artifact_store: Arc::new(MemoryArtifactStore::new()),
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

fn patch_json(uri: &str, tenant: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-runic-tenant", tenant)
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn run_request(thread: &str, tenant: &str, message: &str) -> Request<Body> {
    post_json(
        &format!("/threads/{thread}/runs/stream"),
        tenant,
        json!({ "message": message }).to_string(),
    )
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

async fn create_thread(app: &Router, tenant: &str, thread_id: &str) {
    let resp = app
        .clone()
        .oneshot(post_json(
            "/threads",
            tenant,
            json!({ "thread_id": thread_id }).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

async fn create_labeled(app: &Router, id: &str, label: Value) -> Value {
    let body = json!({ "thread_id": id, "label": label }).to_string();
    let resp = app
        .clone()
        .oneshot(post_json("/threads", TENANT, body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    body_json(resp).await
}

async fn wait_for_stored_events(
    store: &dyn SessionStore,
    tenant: &str,
    thread_id: &str,
    min_events: usize,
) {
    for _ in 0..50 {
        if store.read(tenant, thread_id).await.unwrap().len() >= min_events {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("stored event count did not reach {min_events}");
}

async fn list(app: &Router, tenant: &str, query: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(get(&format!("/threads{query}"), tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

fn page_ids(page: &Value) -> Vec<String> {
    page["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["thread_id"].as_str().unwrap().to_string())
        .collect()
}

fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            other => format!("%{:02X}", other as u32),
        })
        .collect()
}

#[tokio::test]
async fn list_limit_clamps_to_upper_bound_of_200() {
    let app = crud_router();
    for i in 0..205 {
        create_thread(&app, TENANT, &format!("t{i:03}")).await;
    }
    let page = list(&app, TENANT, "?limit=1000").await;
    assert_eq!(page["threads"].as_array().unwrap().len(), 200);
    assert!(page["next_cursor"].is_string());
}

#[tokio::test]
async fn list_limit_clamps_to_lower_bound_of_1() {
    let app = crud_router();
    create_thread(&app, TENANT, "a").await;
    create_thread(&app, TENANT, "b").await;
    let page = list(&app, TENANT, "?limit=0").await;
    assert_eq!(page["threads"].as_array().unwrap().len(), 1);
    assert!(page["next_cursor"].is_string());
}

#[tokio::test]
async fn next_cursor_absent_when_page_exhausts_results() {
    let app = crud_router();
    create_thread(&app, TENANT, "only-a").await;
    create_thread(&app, TENANT, "only-b").await;
    let page = list(&app, TENANT, "?limit=50").await;
    assert_eq!(page["threads"].as_array().unwrap().len(), 2);
    assert!(page["next_cursor"].is_null());
}

#[tokio::test]
async fn walking_cursor_covers_every_thread_once() {
    let app = crud_router();
    let mut created: Vec<String> = (0..25).map(|i| format!("t{i:02}")).collect();
    for id in &created {
        create_thread(&app, TENANT, id).await;
    }

    let mut seen: Vec<String> = Vec::new();
    let mut query = "?limit=7".to_string();
    loop {
        let page = list(&app, TENANT, &query).await;
        seen.extend(page_ids(&page));
        match page["next_cursor"].as_str() {
            Some(cursor) => query = format!("?limit=7&cursor={}", urlencode(cursor)),
            None => break,
        }
    }

    seen.sort();
    created.sort();
    assert_eq!(seen, created);
}

#[tokio::test]
async fn listing_is_newest_active_first() {
    let app = crud_router();
    for id in ["oldest", "middle", "newest"] {
        create_thread(&app, TENANT, id).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let page = list(&app, TENANT, "?limit=50").await;
    assert_eq!(page_ids(&page), vec!["newest", "middle", "oldest"]);
}

#[tokio::test]
async fn cursor_is_tenant_scoped_and_cannot_leak_foreign_threads() {
    let app = crud_router();
    for i in 0..5 {
        create_thread(&app, "alice", &format!("alice-{i}")).await;
        create_thread(&app, "bob", &format!("bob-{i}")).await;
    }

    let alice_page = list(&app, "alice", "?limit=2").await;
    let cursor = alice_page["next_cursor"].as_str().unwrap();

    let bob_page = list(
        &app,
        "bob",
        &format!("?limit=50&cursor={}", urlencode(cursor)),
    )
    .await;
    let bob_ids = page_ids(&bob_page);
    assert!(
        bob_ids.iter().all(|id| id.starts_with("bob-")),
        "{bob_ids:?}"
    );
}

#[tokio::test]
async fn create_trims_label_whitespace() {
    let app = crud_router();
    let body = create_labeled(&app, "trim", json!("  spaced  ")).await;
    assert_eq!(body["label"], "spaced");
}

#[tokio::test]
async fn create_whitespace_label_becomes_null() {
    let app = crud_router();
    let body = create_labeled(&app, "blank", json!("   ")).await;
    assert!(body["label"].is_null());
}

#[tokio::test]
async fn recreate_without_label_preserves_existing() {
    let app = crud_router();
    create_labeled(&app, "keepme", json!("original")).await;
    let resp = app
        .clone()
        .oneshot(post_json(
            "/threads",
            TENANT,
            json!({ "thread_id": "keepme" }).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(body_json(resp).await["label"], "original");
}

#[tokio::test]
async fn recreate_with_label_updates_it() {
    let app = crud_router();
    create_labeled(&app, "reup", json!("first")).await;
    let body = create_labeled(&app, "reup", json!("second")).await;
    assert_eq!(body["label"], "second");
}

#[tokio::test]
async fn patch_sets_clears_and_leaves_label_unchanged() {
    let app = crud_router();
    create_labeled(&app, "patchme", json!("start")).await;

    let resp = app
        .clone()
        .oneshot(patch_json(
            "/threads/patchme",
            TENANT,
            r#"{"label":"renamed"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["label"], "renamed");

    let resp = app
        .clone()
        .oneshot(patch_json("/threads/patchme", TENANT, "{}"))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["label"], "renamed");

    let resp = app
        .clone()
        .oneshot(patch_json("/threads/patchme", TENANT, r#"{"label":null}"#))
        .await
        .unwrap();
    assert!(body_json(resp).await["label"].is_null());
}

#[tokio::test]
async fn patch_unknown_thread_is_404() {
    let app = crud_router();
    let resp = app
        .oneshot(patch_json("/threads/ghost", TENANT, r#"{"label":"x"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

async fn seed_run_events(store: Arc<dyn SessionStore>) -> Router {
    let app = scripted_router_with_store(store.clone());
    let resp = app
        .clone()
        .oneshot(run_request("evthread", TENANT, "hello"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = body_string(resp).await;
    wait_for_stored_events(store.as_ref(), TENANT, "evthread", 5).await;
    app
}

async fn events_page(app: &Router, query: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(get(&format!("/threads/evthread/events{query}"), TENANT))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

#[tokio::test]
async fn events_pagination_walks_all_seqs_without_gaps() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let app = seed_run_events(store.clone()).await;
    let total = store.read(TENANT, "evthread").await.unwrap().len();

    let mut seqs: Vec<u64> = Vec::new();
    let mut after = 0u64;
    loop {
        let page = events_page(&app, &format!("?after_seq={after}&limit=2")).await;
        for e in page["events"].as_array().unwrap() {
            seqs.push(e["seq"].as_u64().unwrap());
        }
        if page["has_more"].as_bool().unwrap() {
            after = page["next_after_seq"].as_u64().unwrap();
        } else {
            break;
        }
    }

    assert_eq!(seqs.len(), total);
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
}

#[tokio::test]
async fn events_has_more_and_next_after_seq_are_consistent() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let app = seed_run_events(store.clone()).await;
    let total = store.read(TENANT, "evthread").await.unwrap().len();

    let first = events_page(&app, "?limit=2").await;
    assert_eq!(first["events"].as_array().unwrap().len(), 2);
    assert_eq!(first["has_more"], true);
    assert!(first["next_after_seq"].is_u64());

    let all = events_page(&app, &format!("?limit={}", total + 10)).await;
    assert_eq!(all["events"].as_array().unwrap().len(), total);
    assert_eq!(all["has_more"], false);
}

#[tokio::test]
async fn events_limit_clamps_to_lower_bound_of_1() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let app = seed_run_events(store.clone()).await;
    let page = events_page(&app, "?limit=0").await;
    assert_eq!(page["events"].as_array().unwrap().len(), 1);
    assert_eq!(page["has_more"], true);
}

#[tokio::test]
async fn events_for_wrong_tenant_is_404_not_foreign_events() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let _ = seed_run_events(store.clone()).await;
    let app = scripted_router_with_store(store);
    let resp = app
        .oneshot(get("/threads/evthread/events", "mallory"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
