use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tower::ServiceExt;

use runic_agent::Agent;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::{AgentFactory, HumanHub, ServeConfig, router, single_agent};
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

struct GatedProvider {
    entered: Arc<Notify>,
    gate: Arc<Notify>,
}

#[async_trait]
impl Provider for GatedProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.entered.notify_one();
        self.gate.notified().await;
        Ok(CompletionResponse {
            content: vec![],
            stop_reason: StopReason::ToolUse,
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "noop".into(),
                input: json!({}),
            }],
            usage: TokenUsage::default(),
        })
    }
}

struct NoopTool;

#[async_trait]
impl Tool for NoopTool {
    fn name(&self) -> &str {
        "noop"
    }
    fn description(&self) -> &str {
        "does nothing"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok("ok"))
    }
}

struct GatedFactory {
    entered: Arc<Notify>,
    gate: Arc<Notify>,
}

#[async_trait]
impl AgentFactory for GatedFactory {
    async fn build(&self, tenant: &str, session_id: &str) -> Agent {
        Agent::builder(
            Arc::new(GatedProvider {
                entered: self.entered.clone(),
                gate: self.gate.clone(),
            }),
            tenant,
            session_id,
        )
        .system_prompt("test")
        .tool(Arc::new(NoopTool))
        .build()
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
        agents: single_agent("main", Arc::new(ScriptedFactory)),
        human_hub: Arc::new(HumanHub::new()),
    })
}

fn scripted_router_with_artifacts() -> (Router, Arc<dyn ArtifactStore>) {
    let artifacts: Arc<dyn ArtifactStore> = Arc::new(MemoryArtifactStore::new());
    let app = router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        artifact_store: artifacts.clone(),
        transcriber: None,
        agents: single_agent("main", Arc::new(ScriptedFactory)),
        human_hub: Arc::new(HumanHub::new()),
    });
    (app, artifacts)
}

fn failing_run_router() -> Router {
    router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agents: single_agent("main", Arc::new(FailingFactory)),
        human_hub: Arc::new(HumanHub::new()),
    })
}

fn asking_router() -> Router {
    router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agents: single_agent("main", Arc::new(AskingFactory)),
        human_hub: Arc::new(HumanHub::new()),
    })
}

fn gated_router() -> (Router, Arc<Notify>, Arc<Notify>) {
    let entered = Arc::new(Notify::new());
    let gate = Arc::new(Notify::new());
    let app = router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agents: single_agent(
            "main",
            Arc::new(GatedFactory {
                entered: entered.clone(),
                gate: gate.clone(),
            }),
        ),
        human_hub: Arc::new(HumanHub::new()),
    });
    (app, entered, gate)
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

fn wait_request(thread: &str, tenant: &str, message: &str) -> Request<Body> {
    post_json(
        &format!("/threads/{thread}/runs/wait"),
        tenant,
        json!({ "message": message }).to_string(),
    )
}

#[tokio::test]
async fn wait_run_returns_the_final_answer_as_json() {
    let app = scripted_router();
    let resp = app
        .oneshot(wait_request("t1", TENANT, "ping"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["text"], "pong");
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["total_turns"], 1);
    assert_eq!(body["input_tokens"], 1);
    assert_eq!(body["output_tokens"], 2);
    assert!(body["run_id"].as_str().unwrap().starts_with("r-"));
}

#[tokio::test]
async fn wait_run_provider_failure_is_500_agent_error() {
    let app = failing_run_router();
    let resp = app
        .oneshot(wait_request("t1", TENANT, "boom"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body_json(resp).await["error"], "agent");
}

#[tokio::test]
async fn wait_run_rejects_empty_body() {
    let app = scripted_router();
    let resp = app
        .oneshot(post_json("/threads/t1/runs/wait", TENANT, "{}".into()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn wait_run_without_human_channel_survives_an_ask() {
    let app = asking_router();
    let resp = app.oneshot(wait_request("t1", TENANT, "go")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["text"], "done");
    assert_eq!(body["total_turns"], 2);
}

#[tokio::test]
async fn wait_run_persists_the_same_lifecycle_as_streaming() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let app = scripted_router_with_store(store.clone());
    let resp = app
        .oneshot(wait_request("persist", TENANT, "hi"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    wait_for_stored_events(store.as_ref(), TENANT, "persist", 4).await;
}

#[tokio::test]
async fn cancel_with_no_run_in_flight_is_409() {
    let app = scripted_router();
    let resp = app
        .oneshot(post_json("/threads/t1/runs/cancel", TENANT, String::new()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn cancel_stops_the_run_gracefully_and_reports_it_over_sse() {
    let (app, entered, gate) = gated_router();
    let thread = "t1";

    let run_app = app.clone();
    let run_task = tokio::spawn(async move {
        let resp = run_app
            .oneshot(run_request(thread, TENANT, "go"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        body_string(resp).await
    });

    entered.notified().await;

    let cancel_resp = app
        .clone()
        .oneshot(post_json(
            &format!("/threads/{thread}/runs/cancel"),
            TENANT,
            String::new(),
        ))
        .await
        .unwrap();
    assert_eq!(cancel_resp.status(), StatusCode::ACCEPTED);

    gate.notify_one();

    let body = run_task.await.unwrap();
    let done = sse_data(&body)
        .into_iter()
        .find(|e| e["type"] == "done")
        .expect("a done event");
    assert_eq!(done["stop_reason"], "cancelled");

    for _ in 0..50 {
        let resp = app
            .clone()
            .oneshot(post_json(
                &format!("/threads/{thread}/runs/cancel"),
                TENANT,
                String::new(),
            ))
            .await
            .unwrap();
        if resp.status() == StatusCode::CONFLICT {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("cancel token was never cleared after the run finished");
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

struct SlowStore {
    inner: MemorySessionStore,
    delay: Duration,
}

#[async_trait]
impl SessionStore for SlowStore {
    async fn append(
        &self,
        tenant: &str,
        session_id: &str,
        event: &runic_state::SessionEvent,
    ) -> runic_substrate::Result<u64> {
        tokio::time::sleep(self.delay).await;
        self.inner.append(tenant, session_id, event).await
    }

    async fn append_batch(
        &self,
        tenant: &str,
        session_id: &str,
        events: &[runic_state::SessionEvent],
    ) -> runic_substrate::Result<()> {
        tokio::time::sleep(self.delay).await;
        self.inner.append_batch(tenant, session_id, events).await
    }

    async fn read(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> runic_substrate::Result<Vec<runic_substrate::StoredEvent>> {
        self.inner.read(tenant, session_id).await
    }

    async fn read_after(
        &self,
        tenant: &str,
        session_id: &str,
        after_seq: u64,
    ) -> runic_substrate::Result<Vec<runic_substrate::StoredEvent>> {
        self.inner.read_after(tenant, session_id, after_seq).await
    }

    async fn list_sessions(
        &self,
        tenant: &str,
    ) -> runic_substrate::Result<Vec<runic_substrate::SessionMeta>> {
        self.inner.list_sessions(tenant).await
    }

    async fn session_meta(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> runic_substrate::Result<Option<runic_substrate::SessionMeta>> {
        self.inner.session_meta(tenant, session_id).await
    }

    async fn set_label(
        &self,
        tenant: &str,
        session_id: &str,
        label: Option<&str>,
    ) -> runic_substrate::Result<()> {
        self.inner.set_label(tenant, session_id, label).await
    }

    async fn delete_session(&self, tenant: &str, session_id: &str) -> runic_substrate::Result<()> {
        self.inner.delete_session(tenant, session_id).await
    }

    async fn create_run(
        &self,
        tenant: &str,
        session_id: &str,
        run_id: &str,
        agent: &str,
    ) -> runic_substrate::Result<()> {
        self.inner
            .create_run(tenant, session_id, run_id, agent)
            .await
    }

    async fn set_run_status(
        &self,
        run_id: &str,
        status: runic_substrate::RunStatus,
        error: Option<&str>,
    ) -> runic_substrate::Result<()> {
        self.inner.set_run_status(run_id, status, error).await
    }

    async fn get_run(
        &self,
        tenant: &str,
        run_id: &str,
    ) -> runic_substrate::Result<Option<runic_substrate::RunRecord>> {
        self.inner.get_run(tenant, run_id).await
    }

    async fn latest_run(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> runic_substrate::Result<Option<runic_substrate::RunRecord>> {
        self.inner.latest_run(tenant, session_id).await
    }
}

#[tokio::test]
async fn wait_response_implies_the_run_is_durable() {
    let store = Arc::new(SlowStore {
        inner: MemorySessionStore::new(),
        delay: Duration::from_millis(200),
    });
    let app = router(ServeConfig {
        session_store: store.clone(),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agents: single_agent("main", Arc::new(ScriptedFactory)),
        human_hub: Arc::new(HumanHub::new()),
    });

    let resp = app
        .oneshot(wait_request("t1", TENANT, "ping"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let stored = store.read(TENANT, "t1").await.unwrap();
    assert!(
        stored
            .iter()
            .any(|s| matches!(s.event, runic_state::SessionEvent::RunEnd { .. })),
        "RunEnd must be durable before the wait response returns"
    );
}

#[tokio::test]
async fn stream_done_implies_the_run_is_durable() {
    let store = Arc::new(SlowStore {
        inner: MemorySessionStore::new(),
        delay: Duration::from_millis(200),
    });
    let app = router(ServeConfig {
        session_store: store.clone(),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agents: single_agent("main", Arc::new(ScriptedFactory)),
        human_hub: Arc::new(HumanHub::new()),
    });

    let resp = app
        .oneshot(run_body("t1", TENANT, json!({ "message": "ping" })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert_eq!(sse_kinds(&body).last().unwrap(), "done");

    let stored = store.read(TENANT, "t1").await.unwrap();
    assert!(
        stored
            .iter()
            .any(|s| matches!(s.event, runic_state::SessionEvent::RunEnd { .. })),
        "RunEnd must be durable before the stream's done event"
    );
}

struct SteerableProvider {
    entered: Arc<Notify>,
    gate: Arc<Notify>,
    first: AtomicBool,
    requests: std::sync::Mutex<Vec<CompletionRequest>>,
}

#[async_trait]
impl Provider for SteerableProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().unwrap().push(req);
        if !self.first.swap(true, Ordering::SeqCst) {
            self.entered.notify_one();
            self.gate.notified().await;
            return Ok(CompletionResponse {
                content: vec![],
                stop_reason: StopReason::ToolUse,
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "noop".into(),
                    input: json!({}),
                }],
                usage: TokenUsage::default(),
            });
        }
        Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: "steered done".into(),
                provider_metadata: None,
            }],
            stop_reason: StopReason::EndTurn,
            tool_calls: vec![],
            usage: TokenUsage::default(),
        })
    }
}

struct SteerableFactory {
    provider: Arc<SteerableProvider>,
}

#[async_trait]
impl AgentFactory for SteerableFactory {
    async fn build(&self, tenant: &str, session_id: &str) -> Agent {
        Agent::builder(self.provider.clone(), tenant, session_id)
            .system_prompt("test")
            .tool(Arc::new(NoopTool))
            .build()
    }
}

#[tokio::test]
async fn steer_with_no_run_in_flight_is_409() {
    let app = scripted_router();
    let resp = app
        .oneshot(post_json(
            "/threads/t1/runs/steer",
            TENANT,
            json!({ "text": "hey" }).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn steer_lands_at_the_next_turn_boundary() {
    let provider = Arc::new(SteerableProvider {
        entered: Arc::new(Notify::new()),
        gate: Arc::new(Notify::new()),
        first: AtomicBool::new(false),
        requests: std::sync::Mutex::new(Vec::new()),
    });
    let store = Arc::new(MemorySessionStore::new());
    let app = router(ServeConfig {
        session_store: store.clone(),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agents: single_agent(
            "main",
            Arc::new(SteerableFactory {
                provider: provider.clone(),
            }),
        ),
        human_hub: Arc::new(HumanHub::new()),
    });

    let run_app = app.clone();
    let run_task = tokio::spawn(async move {
        let resp = run_app
            .oneshot(run_request("t1", TENANT, "go"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        body_string(resp).await
    });

    provider.entered.notified().await;

    let steer_resp = app
        .clone()
        .oneshot(post_json(
            "/threads/t1/runs/steer",
            TENANT,
            json!({ "text": "check the db instead" }).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(steer_resp.status(), StatusCode::ACCEPTED);

    provider.gate.notify_one();

    let body = run_task.await.unwrap();
    assert_eq!(sse_kinds(&body).last().unwrap(), "done");

    let second_texts: String = {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        requests[1]
            .messages
            .iter()
            .map(|m| m.content.text_content())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(second_texts.contains("check the db instead"));

    let stored = store.read(TENANT, "t1").await.unwrap();
    assert!(stored.iter().any(|s| matches!(
        &s.event,
        runic_state::SessionEvent::Message { msg, .. }
            if msg.content.text_content().contains("check the db instead")
    )));
}

#[tokio::test]
async fn steer_with_empty_text_is_400() {
    let app = scripted_router();
    let resp = app
        .oneshot(post_json(
            "/threads/t1/runs/steer",
            TENANT,
            json!({ "text": "  " }).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn run_rows_track_the_lifecycle_over_http() {
    let store = Arc::new(MemorySessionStore::new());
    let app = router(ServeConfig {
        session_store: store.clone(),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agents: single_agent("main", Arc::new(ScriptedFactory)),
        human_hub: Arc::new(HumanHub::new()),
    });

    let resp = app
        .clone()
        .oneshot(wait_request("t1", TENANT, "ping"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let run_id = body_json(resp).await["run_id"]
        .as_str()
        .unwrap()
        .to_string();

    let rec = store.get_run(TENANT, &run_id).await.unwrap().unwrap();
    assert_eq!(rec.status, runic_substrate::RunStatus::Success);
    assert_eq!(rec.agent, "main");
    assert_eq!(rec.session_id, "t1");

    let failing = router(ServeConfig {
        session_store: store.clone(),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agents: single_agent("main", Arc::new(FailingFactory)),
        human_hub: Arc::new(HumanHub::new()),
    });
    let resp = failing
        .oneshot(wait_request("t2", TENANT, "boom"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let rec = store.latest_run(TENANT, "t2").await.unwrap().unwrap();
    assert_eq!(rec.status, runic_substrate::RunStatus::Error);
    assert!(rec.error.is_some());
}
