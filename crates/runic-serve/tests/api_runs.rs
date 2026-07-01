use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::StreamExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use runic_agent::Agent;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::{AgentFactory, HumanHub, ServeConfig, router};
use runic_substrate::{ArtifactStore, MemoryArtifactStore, MemorySessionStore, SessionStore};
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, StopReason, TokenUsage, ToolCall};

const TENANT: &str = "alice";

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

struct FailingProvider;

#[async_trait]
impl Provider for FailingProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Err(ProviderError::Http("scripted provider failure".into()))
    }
}

struct FailingFactory;

#[async_trait]
impl AgentFactory for FailingFactory {
    async fn build(&self, tenant: &str, session_id: &str) -> Agent {
        Agent::builder(Arc::new(FailingProvider), tenant, session_id)
            .system_prompt("test")
            .build()
    }
}

struct ParkTool;

#[async_trait]
impl Tool for ParkTool {
    fn name(&self) -> &str {
        "ask_user"
    }
    fn description(&self) -> &str {
        "ask the user"
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "question": { "type": "string" } },
            "required": ["question"]
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        let human = ctx.human().expect("serve wires a human channel");
        let question = args["question"].as_str().unwrap_or("proceed?");
        match human.ask(question, None).await {
            Ok(answer) => Ok(ToolResult::ok(answer)),
            Err(e) => Ok(ToolResult::error(e.to_string())),
        }
    }
}

struct AskingProvider {
    asked: AtomicBool,
}

#[async_trait]
impl Provider for AskingProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        if self.asked.swap(true, Ordering::SeqCst) {
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text {
                    text: "done".into(),
                    provider_metadata: None,
                }],
                stop_reason: StopReason::EndTurn,
                tool_calls: vec![],
                usage: TokenUsage::default(),
            })
        } else {
            Ok(CompletionResponse {
                content: vec![],
                stop_reason: StopReason::ToolUse,
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "ask_user".into(),
                    input: json!({ "question": "proceed?" }),
                }],
                usage: TokenUsage::default(),
            })
        }
    }
}

struct AskingFactory;

#[async_trait]
impl AgentFactory for AskingFactory {
    async fn build(&self, tenant: &str, session_id: &str) -> Agent {
        Agent::builder(
            Arc::new(AskingProvider {
                asked: AtomicBool::new(false),
            }),
            tenant,
            session_id,
        )
        .system_prompt("test")
        .tool(Arc::new(ParkTool))
        .build()
    }
}

fn scripted_router() -> Router {
    scripted_router_with_store(Arc::new(MemorySessionStore::new()))
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

fn scripted_router_with_artifacts() -> (Router, Arc<dyn ArtifactStore>) {
    let artifacts: Arc<dyn ArtifactStore> = Arc::new(MemoryArtifactStore::new());
    let app = router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        artifact_store: artifacts.clone(),
        transcriber: None,
        agent_factory: Arc::new(ScriptedFactory),
        human_hub: Arc::new(HumanHub::new()),
    });
    (app, artifacts)
}

fn failing_run_router() -> Router {
    router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agent_factory: Arc::new(FailingFactory),
        human_hub: Arc::new(HumanHub::new()),
    })
}

fn asking_router() -> Router {
    router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agent_factory: Arc::new(AskingFactory),
        human_hub: Arc::new(HumanHub::new()),
    })
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

fn get_with(uri: &str, tenant: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut b = Request::builder().uri(uri).header("x-runic-tenant", tenant);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Body::empty()).unwrap()
}

fn run_request(thread: &str, tenant: &str, message: &str) -> Request<Body> {
    post_json(
        &format!("/threads/{thread}/runs/stream"),
        tenant,
        json!({ "message": message }).to_string(),
    )
}

fn run_body(thread: &str, tenant: &str, body: Value) -> Request<Body> {
    post_json(
        &format!("/threads/{thread}/runs/stream"),
        tenant,
        body.to_string(),
    )
}

fn answer(uri: &str, tenant: &str, ans: &str) -> Request<Body> {
    post_json(uri, tenant, json!({ "answer": ans }).to_string())
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

fn sse_data(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|json| serde_json::from_str(json.trim()).unwrap())
        .collect()
}

fn sse_kinds(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|line| line.strip_prefix("event:"))
        .map(|k| k.trim().to_string())
        .collect()
}

fn find_run_id(body: &str) -> Option<String> {
    sse_data(body)
        .into_iter()
        .find_map(|e| (e["type"] == "run_start").then(|| e["run_id"].as_str().unwrap().to_string()))
}

fn find_ask_id(buf: &str) -> Option<String> {
    for line in buf.lines() {
        if let Some(j) = line.strip_prefix("data:")
            && let Ok(v) = serde_json::from_str::<Value>(j.trim())
            && v["type"] == "ask_required"
        {
            return v["ask_id"].as_str().map(str::to_string);
        }
    }
    None
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

async fn wait_for_stored_events(store: &dyn SessionStore, tenant: &str, thread: &str, min: usize) {
    for _ in 0..50 {
        if store.read(tenant, thread).await.unwrap().len() >= min {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("stored event count did not reach {min}");
}

async fn park_ask(
    app: &Router,
    thread: &str,
    tenant: &str,
) -> (String, tokio::task::JoinHandle<String>) {
    let resp = app
        .clone()
        .oneshot(run_request(thread, tenant, "go"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut stream = resp.into_body().into_data_stream();
    let mut buf = String::new();
    let ask_id = loop {
        let chunk = stream
            .next()
            .await
            .expect("stream ended before ask_required")
            .unwrap();
        buf.push_str(&String::from_utf8_lossy(&chunk));
        if let Some(id) = find_ask_id(&buf) {
            break id;
        }
    };
    let drain = tokio::spawn(async move {
        let mut rest = String::new();
        while let Some(Ok(c)) = stream.next().await {
            rest.push_str(&String::from_utf8_lossy(&c));
        }
        rest
    });
    (ask_id, drain)
}

#[tokio::test]
async fn malformed_json_body_is_400() {
    let app = scripted_router();
    let resp = app
        .oneshot(post_json(
            "/threads/t1/runs/stream",
            TENANT,
            "{ not json".into(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn missing_message_and_content_is_400() {
    let app = scripted_router();
    let resp = app
        .oneshot(post_json("/threads/t1/runs/stream", TENANT, "{}".into()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["error"], "bad_request");
}

#[tokio::test]
async fn empty_message_is_400() {
    let app = scripted_router();
    let resp = app.oneshot(run_request("t1", TENANT, "   ")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn empty_content_falls_back_to_message() {
    let app = scripted_router();
    let body = json!({ "message": "hello", "content": [] });
    let resp = app.oneshot(run_body("t1", TENANT, body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_string(resp).await.contains("pong"));
}

#[tokio::test]
async fn invalid_base64_inline_media_is_400_and_stores_nothing() {
    let (app, artifacts) = scripted_router_with_artifacts();
    create_thread(&app, TENANT, "b64").await;
    let body = json!({
        "content": [
            { "type": "image", "media_type": "image/png", "data": "!!!not-base64!!!" }
        ]
    });
    let resp = app
        .clone()
        .oneshot(run_body("b64", TENANT, body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(artifacts.list(TENANT, "b64").await.unwrap().is_empty());
}

#[tokio::test]
async fn oversize_run_body_is_rejected_by_body_limit() {
    let app = scripted_router();
    let big = "A".repeat(3 * 1024 * 1024);
    let body = json!({
        "content": [ { "type": "image", "media_type": "image/png", "data": big } ]
    });
    let resp = app.oneshot(run_body("big", TENANT, body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn live_stream_shape_is_stable_and_ends_with_done() {
    let app = scripted_router();
    let resp = app
        .oneshot(run_request("t1", TENANT, "ping"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;

    for e in sse_data(&body) {
        assert!(
            e["type"].is_string(),
            "every data payload carries a type: {e}"
        );
    }

    let kinds = sse_kinds(&body);
    assert!(kinds.contains(&"run_start".to_string()));
    assert!(kinds.contains(&"assistant_text_delta".to_string()));
    assert!(kinds.contains(&"usage".to_string()));
    assert_eq!(kinds.last().unwrap(), "done");
}

#[tokio::test]
async fn provider_failure_emits_run_error_then_done() {
    let app = failing_run_router();
    let resp = app
        .oneshot(run_request("t1", TENANT, "boom"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    let kinds = sse_kinds(&body);
    assert!(kinds.contains(&"run_error".to_string()), "{kinds:?}");
    assert_eq!(kinds.last().unwrap(), "done");
}

#[tokio::test]
async fn streamed_lifecycle_is_persisted_for_replay() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let app = scripted_router_with_store(store.clone());
    let resp = app
        .oneshot(run_request("persist", TENANT, "hi"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = body_string(resp).await;

    wait_for_stored_events(store.as_ref(), TENANT, "persist", 4).await;
    let events = store.read(TENANT, "persist").await.unwrap();
    let kinds: Vec<&str> = events
        .iter()
        .map(|s| match &s.event {
            runic_state::SessionEvent::RunStart { .. } => "run_start",
            runic_state::SessionEvent::RunEnd { .. } => "run_end",
            runic_state::SessionEvent::Message { .. } => "message",
            _ => "other",
        })
        .collect();
    assert!(kinds.contains(&"run_start"));
    assert!(kinds.contains(&"run_end"));
    assert!(kinds.iter().filter(|k| **k == "message").count() >= 2);
}

#[tokio::test]
async fn replay_unknown_thread_is_404() {
    let app = scripted_router();
    let resp = app
        .oneshot(get_with("/threads/ghost/runs/r1/stream", TENANT, &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn replay_unknown_run_on_known_thread_is_404() {
    let app = scripted_router();
    create_thread(&app, TENANT, "known").await;
    let resp = app
        .oneshot(get_with("/threads/known/runs/nope/stream", TENANT, &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn replay_bad_last_event_id_is_treated_as_zero() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let app = scripted_router_with_store(store.clone());
    let resp = app
        .clone()
        .oneshot(run_request("rp", TENANT, "hi"))
        .await
        .unwrap();
    let run_id = find_run_id(&body_string(resp).await).unwrap();
    wait_for_stored_events(store.as_ref(), TENANT, "rp", 4).await;

    let resp = app
        .oneshot(get_with(
            &format!("/threads/rp/runs/{run_id}/stream"),
            TENANT,
            &[("last-event-id", "not-a-number")],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = tokio::time::timeout(Duration::from_secs(2), body_string(resp))
        .await
        .expect("closed run replay should finish");
    let kinds = sse_kinds(&body);
    assert!(kinds.contains(&"message".to_string()));
    assert!(kinds.contains(&"run_end".to_string()));
    assert_eq!(kinds.last().unwrap(), "done");
}

#[tokio::test]
async fn replay_past_end_emits_only_done() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let app = scripted_router_with_store(store.clone());
    let resp = app
        .clone()
        .oneshot(run_request("pe", TENANT, "hi"))
        .await
        .unwrap();
    let run_id = find_run_id(&body_string(resp).await).unwrap();
    wait_for_stored_events(store.as_ref(), TENANT, "pe", 4).await;

    let resp = app
        .oneshot(get_with(
            &format!("/threads/pe/runs/{run_id}/stream"),
            TENANT,
            &[("last-event-id", "100000")],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = tokio::time::timeout(Duration::from_secs(2), body_string(resp))
        .await
        .expect("replay should finish");
    let kinds = sse_kinds(&body);
    assert_eq!(kinds, vec!["done".to_string()]);
}

#[tokio::test]
async fn ask_answered_through_legacy_route_resumes_run() {
    let app = asking_router();
    create_thread(&app, TENANT, "hitl").await;
    let (ask_id, drain) = park_ask(&app, "hitl", TENANT).await;

    let resp = app
        .clone()
        .oneshot(answer(
            &format!("/threads/hitl/runs/any/asks/{ask_id}"),
            TENANT,
            "yes",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let rest = tokio::time::timeout(Duration::from_secs(2), drain)
        .await
        .expect("run resumes after answer")
        .unwrap();
    assert!(sse_kinds(&rest).contains(&"done".to_string()));
}

#[tokio::test]
async fn ask_answer_wrong_scope_is_rejected_then_correct_scope_resumes() {
    let app = asking_router();
    create_thread(&app, TENANT, "scoped").await;
    let (ask_id, drain) = park_ask(&app, "scoped", TENANT).await;

    let wrong_tenant = app
        .clone()
        .oneshot(answer(
            &format!("/threads/scoped/asks/{ask_id}"),
            "mallory",
            "x",
        ))
        .await
        .unwrap();
    assert_eq!(wrong_tenant.status(), StatusCode::BAD_REQUEST);

    let wrong_thread = app
        .clone()
        .oneshot(answer(
            &format!("/threads/other/asks/{ask_id}"),
            TENANT,
            "x",
        ))
        .await
        .unwrap();
    assert_eq!(wrong_thread.status(), StatusCode::BAD_REQUEST);

    let correct = app
        .clone()
        .oneshot(answer(
            &format!("/threads/scoped/asks/{ask_id}"),
            TENANT,
            "yes",
        ))
        .await
        .unwrap();
    assert_eq!(correct.status(), StatusCode::ACCEPTED);

    let _ = tokio::time::timeout(Duration::from_secs(2), drain)
        .await
        .expect("run resumes");
}

#[tokio::test]
async fn answering_same_ask_twice_is_202_then_400() {
    let app = asking_router();
    create_thread(&app, TENANT, "twice").await;
    let (ask_id, drain) = park_ask(&app, "twice", TENANT).await;

    let first = app
        .clone()
        .oneshot(answer(
            &format!("/threads/twice/asks/{ask_id}"),
            TENANT,
            "yes",
        ))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::ACCEPTED);

    let _ = tokio::time::timeout(Duration::from_secs(2), drain)
        .await
        .expect("run resumes");

    let second = app
        .oneshot(answer(
            &format!("/threads/twice/asks/{ask_id}"),
            TENANT,
            "yes",
        ))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::BAD_REQUEST);
}
