mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use runic::Agent;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::app::AppState;
use runic_serve::hosts::AgentRegistry;
use runic_serve::routes::runs::input::input_from_message;
use runic_serve::{ServeError, router, single_agent};
use runic_substrate::{MemorySessionStore, RunStatus, SessionStore};
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, Message, MessageContent, StopReason, TokenUsage, ToolCall};

use common::Harness;

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

struct FailingProvider;

#[async_trait]
impl Provider for FailingProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Err(ProviderError::Http("scripted provider failure".into()))
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
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        let question = args["question"].as_str().unwrap_or("proceed?");
        Ok(ToolResult::defer(json!({ "question": question })))
    }
}

struct AskingProvider;

#[async_trait]
impl Provider for AskingProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let answered = req.messages.iter().any(|m| match &m.content {
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. })),
            _ => false,
        });
        if answered {
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
                content: vec![ContentBlock::ToolUse {
                    id: "call-1".into(),
                    name: "ask_user".into(),
                    input: json!({ "question": "proceed?" }),
                    provider_metadata: None,
                }],
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

struct DeferringProvider;

#[async_trait]
impl Provider for DeferringProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Ok(CompletionResponse {
            content: vec![ContentBlock::ToolUse {
                id: "defer-1".into(),
                name: "defer_to_human".into(),
                input: json!({ "question": "continue?" }),
                provider_metadata: None,
            }],
            stop_reason: StopReason::ToolUse,
            tool_calls: vec![ToolCall {
                id: "defer-1".into(),
                name: "defer_to_human".into(),
                input: json!({ "question": "continue?" }),
            }],
            usage: TokenUsage::default(),
        })
    }
}

struct DeferTool;

#[async_trait]
impl Tool for DeferTool {
    fn name(&self) -> &str {
        "defer_to_human"
    }
    fn description(&self) -> &str {
        "defers the run"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::defer(args))
    }
}

fn scripted_agent() -> Agent {
    common::agent(Arc::new(ScriptedProvider))
}

fn failing_agent() -> Agent {
    common::agent(Arc::new(FailingProvider))
}

fn asking_agent() -> Agent {
    common::agent(Arc::new(AskingProvider)).tool(ParkTool)
}

fn deferring_agent() -> Agent {
    common::agent(Arc::new(DeferringProvider)).tool(DeferTool)
}

fn test_state(h: &Harness, agent: Agent) -> AppState {
    AppState {
        sessions: h.sessions.clone(),
        blobs: h.blobs.clone(),
        pool: h.pool.clone(),
        transcriber: None,
        agents: Arc::new(AgentRegistry::new(single_agent("main", agent))),
    }
}

fn wait_body(thread: &str, tenant: &str, body: Value) -> Request<Body> {
    common::post_json(
        &format!("/threads/{thread}/runs/wait"),
        tenant,
        body.to_string(),
    )
}

fn answer(uri: &str, tenant: &str, ans: &str) -> Request<Body> {
    common::post_json(uri, tenant, json!({ "answer": ans }).to_string())
}

async fn wait_for_run_status(
    store: &dyn SessionStore,
    tenant: &str,
    run_id: &str,
    want: RunStatus,
) {
    for _ in 0..200 {
        if let Some(rec) = store.get_run(tenant, run_id).await.unwrap()
            && rec.status == want
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let got = store
        .get_run(tenant, run_id)
        .await
        .unwrap()
        .map(|r| r.status);
    panic!("run {run_id} never reached {want:?} (last: {got:?})");
}

async fn suspend_a_run(app: &Router, h: &Harness, thread: &str) -> String {
    let resp = app
        .clone()
        .oneshot(common::wait_request(thread, &h.tenant, "go"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    let run_id = body["run_id"].as_str().unwrap().to_string();
    wait_for_run_status(h.store().as_ref(), &h.tenant, &run_id, RunStatus::Waiting).await;
    run_id
}

async fn seed_paused_deferred_run(
    store: &dyn SessionStore,
    tenant: &str,
    thread: &str,
    run_id: &str,
    call_id: &str,
    include_tool_use: bool,
) {
    store
        .create_run(tenant, thread, run_id, "main")
        .await
        .unwrap();
    store
        .set_run_status(run_id, RunStatus::Waiting, None)
        .await
        .unwrap();
    if include_tool_use {
        store
            .append(
                tenant,
                thread,
                &runic_substrate::SessionEvent::Message {
                    run_id: run_id.to_string(),
                    msg: Message::assistant_with_blocks(vec![ContentBlock::ToolUse {
                        id: call_id.to_string(),
                        name: "ask_user".to_string(),
                        input: json!({ "question": "proceed?" }),
                        provider_metadata: None,
                    }]),
                    at: chrono::Utc::now(),
                },
            )
            .await
            .unwrap();
    }
    store
        .append(
            tenant,
            thread,
            &runic_substrate::SessionEvent::ToolDeferred {
                run_id: run_id.to_string(),
                call_id: call_id.to_string(),
                tool: "ask_user".to_string(),
                payload: json!({ "question": "proceed?" }),
                at: chrono::Utc::now(),
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn malformed_json_body_is_400() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(scripted_agent());
    let resp = app
        .oneshot(common::post_json(
            "/threads/t1/runs/wait",
            &h.tenant,
            "{ not json",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn missing_message_and_content_is_400() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(scripted_agent());
    let resp = app
        .oneshot(common::post_json("/threads/t1/runs/wait", &h.tenant, "{}"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(common::body_json(resp).await["error"], "bad_request");
}

#[tokio::test]
async fn empty_message_is_400() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(scripted_agent());
    let resp = app
        .oneshot(common::wait_request("t1", &h.tenant, "   "))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn empty_content_falls_back_to_message() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(scripted_agent());
    let body = json!({ "message": "hello", "content": [] });
    let resp = app.oneshot(wait_body("t1", &h.tenant, body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(common::body_json(resp).await["text"], "pong");
}

#[tokio::test]
async fn oversize_run_body_is_rejected_by_body_limit() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(scripted_agent());
    let big = "A".repeat(3 * 1024 * 1024);
    let body = json!({
        "content": [ { "type": "image", "media_type": "image/png", "data": big } ]
    });
    let resp = app
        .oneshot(wait_body("big", &h.tenant, body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn invalid_base64_inline_media_is_rejected_and_stores_nothing() {
    let Some(h) = common::harness().await else {
        return;
    };
    common::create_thread(&h.single_router(scripted_agent()), &h.tenant, "b64").await;
    let state = test_state(&h, scripted_agent());
    let msg = Message::user_with_blocks(vec![ContentBlock::Image {
        media_type: "image/png".into(),
        data: "!!!not-base64!!!".into(),
    }]);
    let result = input_from_message(&state, &h.tenant, "b64", msg).await;
    assert!(matches!(result, Err(ServeError::BadRequest(_))));
    assert!(
        h.artifacts()
            .list(&h.tenant, "b64")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn wait_run_returns_the_final_answer_as_json() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(scripted_agent());
    let resp = app
        .oneshot(common::wait_request("t1", &h.tenant, "ping"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["text"], "pong");
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["total_turns"], 1);
    assert_eq!(body["input_tokens"], 1);
    assert_eq!(body["output_tokens"], 2);
    assert!(body["run_id"].as_str().unwrap().starts_with("r-"));
}

#[tokio::test]
async fn wait_run_with_deferred_tool_pauses_the_run_and_replays_the_deferral() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(deferring_agent());

    let resp = app
        .clone()
        .oneshot(common::wait_request("t1", &h.tenant, "need approval"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    let run_id = body["run_id"].as_str().unwrap().to_string();

    let rec = h
        .store()
        .get_run(&h.tenant, &run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        rec.status,
        RunStatus::Waiting,
        "a deferred tool suspends the HTTP run until an external resume"
    );

    let resp = app
        .oneshot(common::get("/threads/t1/events", &h.tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let events = common::body_json(resp).await;
    let deferred = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| &e["event"])
        .find(|e| e["kind"] == "ToolDeferred")
        .expect("events page exposes the deferred tool call");
    assert_eq!(deferred["call_id"], "defer-1");
    assert_eq!(
        deferred["tool"], "defer_to_human",
        "the frontend keys off the same tool name every other tool event carries"
    );
    assert_eq!(deferred["payload"]["question"], "continue?");
}

#[tokio::test]
async fn wait_run_provider_failure_is_500_agent_error() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(failing_agent());
    let resp = app
        .oneshot(common::wait_request("t1", &h.tenant, "boom"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(common::body_json(resp).await["error"], "agent");
}

#[tokio::test]
async fn wait_run_rejects_empty_body() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(scripted_agent());
    let resp = app
        .oneshot(common::post_json("/threads/t1/runs/wait", &h.tenant, "{}"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn wait_run_persists_events_for_thread_history() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(scripted_agent());
    let resp = app
        .oneshot(common::wait_request("persist", &h.tenant, "hi"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ok = common::poll_until(50, Duration::from_millis(10), || async {
        h.store().read(&h.tenant, "persist").await.unwrap().len() >= 4
    })
    .await;
    assert!(ok, "stored event count did not reach 4");
}

#[tokio::test]
async fn run_list_and_timeline_endpoints_serve_the_execution_tree() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(scripted_agent());

    let resp = app
        .clone()
        .oneshot(common::wait_request("t1", &h.tenant, "ping"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let run_id = common::body_json(resp).await["run_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = app
        .clone()
        .oneshot(common::get("/threads/t1/runs", &h.tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list = common::body_json(resp).await;
    assert_eq!(list["runs"].as_array().unwrap().len(), 1);
    assert_eq!(list["runs"][0]["run_id"], run_id.as_str());
    assert_eq!(list["runs"][0]["status"], "successful");

    let resp = app
        .oneshot(common::get(
            &format!("/threads/t1/runs/{run_id}/timeline"),
            &h.tenant,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let trace = common::body_json(resp).await;
    assert_eq!(trace["run_id"], run_id.as_str());
    assert_eq!(trace["status"], "Completed");
    let turns = trace["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0]["complete"], true);
    assert!(turns[0]["model"].is_string());
    assert!(turns[0]["model_ms"].is_u64());
}

#[tokio::test]
async fn run_status_for_an_unknown_or_foreign_run_is_404() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(scripted_agent());
    h.store()
        .create_run(&h.tenant, "t1", "r-real", "main")
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(common::get("/threads/t1/runs/r-missing", &h.tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = app
        .clone()
        .oneshot(common::get("/threads/other/runs/r-real", &h.tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = app
        .oneshot(common::get("/threads/t1/runs/r-real", "mallory"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn successful_run_rows_track_started_and_finished_timestamps() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(scripted_agent());

    let resp = app
        .oneshot(common::wait_request("t1", &h.tenant, "ping"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let run_id = common::body_json(resp).await["run_id"]
        .as_str()
        .unwrap()
        .to_string();

    let rec = h
        .store()
        .get_run(&h.tenant, &run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rec.status, RunStatus::Successful);
    assert_eq!(rec.agent, "main");
    assert_eq!(rec.session_id, "t1");
    assert!(rec.started_at.is_some());
    assert!(rec.finished_at.is_some());
}

#[tokio::test]
async fn failing_run_rows_land_as_failed_with_an_error() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(failing_agent());

    let resp = app
        .oneshot(common::wait_request("t2", &h.tenant, "boom"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let rec = h
        .store()
        .latest_run(&h.tenant, "t2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rec.status, RunStatus::Failed);
    assert!(rec.error.is_some());
}

#[tokio::test]
async fn ask_defers_then_answer_is_accepted_and_delivers_the_tool_result() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(asking_agent());
    common::create_thread(&app, &h.tenant, "hitl").await;
    let run_id = suspend_a_run(&app, &h, "hitl").await;

    let resp = app
        .clone()
        .oneshot(answer("/threads/hitl/asks/call-1", &h.tenant, "yes"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    wait_for_run_status(h.store().as_ref(), &h.tenant, &run_id, RunStatus::Idle).await;
    let delivered = h
        .store()
        .read(&h.tenant, "hitl")
        .await
        .unwrap()
        .into_iter()
        .any(|e| {
            matches!(&e.event,
            runic_substrate::SessionEvent::Message { msg, .. }
                if matches!(&msg.content, MessageContent::Blocks(b)
                    if b.iter().any(|blk| matches!(blk, ContentBlock::ToolResult { .. }))))
        });
    assert!(
        delivered,
        "the answer must land as a tool result on the paused call"
    );
}

#[tokio::test]
async fn answering_an_unknown_ask_is_400() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(asking_agent());
    let resp = app
        .oneshot(answer("/threads/nothread/asks/ghost", &h.tenant, "x"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn answering_from_a_wrong_tenant_is_rejected_and_leaves_the_run_paused() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(asking_agent());
    common::create_thread(&app, &h.tenant, "scoped").await;
    let run_id = suspend_a_run(&app, &h, "scoped").await;

    let wrong = app
        .clone()
        .oneshot(answer("/threads/scoped/asks/call-1", "mallory", "x"))
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        h.store()
            .get_run(&h.tenant, &run_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        RunStatus::Waiting,
        "a rejected answer must not disturb the paused run"
    );

    let correct = app
        .oneshot(answer("/threads/scoped/asks/call-1", &h.tenant, "yes"))
        .await
        .unwrap();
    assert_eq!(correct.status(), StatusCode::ACCEPTED);
    wait_for_run_status(h.store().as_ref(), &h.tenant, &run_id, RunStatus::Idle).await;
}

#[tokio::test]
async fn answering_from_a_wrong_thread_is_rejected_and_leaves_the_run_paused() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(asking_agent());
    common::create_thread(&app, &h.tenant, "origin").await;
    common::create_thread(&app, &h.tenant, "other").await;
    let run_id = suspend_a_run(&app, &h, "origin").await;

    let wrong = app
        .clone()
        .oneshot(answer("/threads/other/asks/call-1", &h.tenant, "x"))
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        h.store()
            .get_run(&h.tenant, &run_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        RunStatus::Waiting,
        "a rejected cross-thread answer must not disturb the paused run"
    );

    let correct = app
        .oneshot(answer("/threads/origin/asks/call-1", &h.tenant, "yes"))
        .await
        .unwrap();
    assert_eq!(correct.status(), StatusCode::ACCEPTED);
    wait_for_run_status(h.store().as_ref(), &h.tenant, &run_id, RunStatus::Idle).await;
}

#[tokio::test]
async fn answering_with_an_invalid_body_is_rejected_and_leaves_the_run_paused() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(asking_agent());
    common::create_thread(&app, &h.tenant, "bad-body").await;
    let run_id = suspend_a_run(&app, &h, "bad-body").await;

    let bad = app
        .clone()
        .oneshot(common::post_json(
            "/threads/bad-body/asks/call-1",
            &h.tenant,
            "{}",
        ))
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        h.store()
            .get_run(&h.tenant, &run_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        RunStatus::Waiting,
        "an invalid answer body must not resume the run"
    );

    let correct = app
        .oneshot(answer("/threads/bad-body/asks/call-1", &h.tenant, "yes"))
        .await
        .unwrap();
    assert_eq!(correct.status(), StatusCode::ACCEPTED);
    wait_for_run_status(h.store().as_ref(), &h.tenant, &run_id, RunStatus::Idle).await;
}

#[tokio::test]
async fn answering_with_a_wrong_json_type_is_rejected_and_leaves_the_run_paused() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(asking_agent());
    common::create_thread(&app, &h.tenant, "bad-type").await;
    let run_id = suspend_a_run(&app, &h, "bad-type").await;

    let bad = app
        .clone()
        .oneshot(common::post_json(
            "/threads/bad-type/asks/call-1",
            &h.tenant,
            json!({ "answer": 42 }).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        h.store()
            .get_run(&h.tenant, &run_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        RunStatus::Waiting,
        "a schema-invalid answer must not resume the run"
    );

    let correct = app
        .oneshot(answer("/threads/bad-type/asks/call-1", &h.tenant, "yes"))
        .await
        .unwrap();
    assert_eq!(correct.status(), StatusCode::ACCEPTED);
    wait_for_run_status(h.store().as_ref(), &h.tenant, &run_id, RunStatus::Idle).await;
}

#[tokio::test]
async fn answering_a_deferred_call_without_matching_tool_use_is_400_and_stays_paused() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(asking_agent());
    seed_paused_deferred_run(
        h.store().as_ref(),
        &h.tenant,
        "orphaned",
        "r-orphaned",
        "call-1",
        false,
    )
    .await;

    let resp = app
        .oneshot(answer("/threads/orphaned/asks/call-1", &h.tenant, "yes"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        h.store()
            .get_run(&h.tenant, "r-orphaned")
            .await
            .unwrap()
            .unwrap()
            .status,
        RunStatus::Waiting,
        "a malformed deferral must not be resumed"
    );
    let tool_results = h
        .store()
        .read(&h.tenant, "orphaned")
        .await
        .unwrap()
        .into_iter()
        .filter(|e| {
            matches!(&e.event,
            runic_substrate::SessionEvent::Message { msg, .. }
                if matches!(&msg.content, MessageContent::Blocks(blocks)
                    if blocks.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. }))))
        })
        .count();
    assert_eq!(tool_results, 0, "a rejected answer must not dirty the log");
}

#[tokio::test]
async fn answering_a_deferred_call_ignores_tool_use_from_another_run() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(asking_agent());
    seed_paused_deferred_run(
        h.store().as_ref(),
        &h.tenant,
        "same-thread",
        "r-other",
        "call-1",
        true,
    )
    .await;
    h.store()
        .set_run_status("r-other", RunStatus::Cancelled, None)
        .await
        .unwrap();
    seed_paused_deferred_run(
        h.store().as_ref(),
        &h.tenant,
        "same-thread",
        "r-paused",
        "call-1",
        false,
    )
    .await;

    let resp = app
        .oneshot(answer("/threads/same-thread/asks/call-1", &h.tenant, "yes"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        h.store()
            .get_run(&h.tenant, "r-paused")
            .await
            .unwrap()
            .unwrap()
            .status,
        RunStatus::Waiting,
        "answer resolution must not borrow a ToolUse from a different run"
    );
}

struct RacingResumeStore {
    inner: MemorySessionStore,
    resume_barrier: tokio::sync::Barrier,
}

impl RacingResumeStore {
    fn new() -> Self {
        Self {
            inner: MemorySessionStore::new(),
            resume_barrier: tokio::sync::Barrier::new(2),
        }
    }
}

#[async_trait]
impl SessionStore for RacingResumeStore {
    async fn append(
        &self,
        tenant: &str,
        session_id: &str,
        event: &runic_substrate::SessionEvent,
    ) -> runic_substrate::Result<u64> {
        self.inner.append(tenant, session_id, event).await
    }

    async fn append_batch(
        &self,
        tenant: &str,
        session_id: &str,
        events: &[runic_substrate::SessionEvent],
    ) -> runic_substrate::Result<()> {
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
        status: RunStatus,
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

    async fn resume_run(&self, tenant: &str, run_id: &str) -> runic_substrate::Result<bool> {
        self.inner.resume_run(tenant, run_id).await
    }

    async fn deliver_and_resume(
        &self,
        tenant: &str,
        run_id: &str,
        event: &runic_substrate::SessionEvent,
    ) -> runic_substrate::Result<bool> {
        self.resume_barrier.wait().await;
        self.inner.deliver_and_resume(tenant, run_id, event).await
    }
}

#[tokio::test]
async fn concurrent_answers_to_the_same_deferred_call_accept_only_one() {
    let Some(h) = common::harness().await else {
        return;
    };
    let store: Arc<dyn SessionStore> = Arc::new(RacingResumeStore::new());
    let app = router(
        runic_serve::ServeConfig::new(
            runic_substrate::Sessions::from(store.clone()),
            h.blobs.clone(),
            h.pool.clone(),
        )
        .agent("main", asking_agent()),
    );
    seed_paused_deferred_run(
        store.as_ref(),
        &h.tenant,
        "race-answer",
        "r-race",
        "call-1",
        true,
    )
    .await;

    let first = app
        .clone()
        .oneshot(answer("/threads/race-answer/asks/call-1", &h.tenant, "yes"));
    let second = app
        .clone()
        .oneshot(answer("/threads/race-answer/asks/call-1", &h.tenant, "yes"));
    let (first, second) = tokio::join!(first, second);
    let mut statuses = vec![first.unwrap().status(), second.unwrap().status()];
    statuses.sort();

    assert_eq!(
        statuses,
        vec![StatusCode::ACCEPTED, StatusCode::BAD_REQUEST],
        "only one answer should win the paused-run resume race"
    );

    let tool_results = store
        .read(&h.tenant, "race-answer")
        .await
        .unwrap()
        .into_iter()
        .filter(|e| {
            matches!(&e.event,
            runic_substrate::SessionEvent::Message { msg, .. }
                if matches!(&msg.content, MessageContent::Blocks(blocks)
                    if blocks.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. }))))
        })
        .count();
    assert_eq!(tool_results, 1, "only one tool result should be appended");
}

#[tokio::test]
async fn answering_a_second_time_is_400_once_the_run_left_paused() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = h.single_router(asking_agent());
    common::create_thread(&app, &h.tenant, "twice").await;
    let run_id = suspend_a_run(&app, &h, "twice").await;

    let first = app
        .clone()
        .oneshot(answer("/threads/twice/asks/call-1", &h.tenant, "yes"))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    wait_for_run_status(h.store().as_ref(), &h.tenant, &run_id, RunStatus::Idle).await;

    let second = app
        .oneshot(answer("/threads/twice/asks/call-1", &h.tenant, "yes"))
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
        event: &runic_substrate::SessionEvent,
    ) -> runic_substrate::Result<u64> {
        tokio::time::sleep(self.delay).await;
        self.inner.append(tenant, session_id, event).await
    }

    async fn append_batch(
        &self,
        tenant: &str,
        session_id: &str,
        events: &[runic_substrate::SessionEvent],
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
        status: RunStatus,
        error: Option<&str>,
    ) -> runic_substrate::Result<()> {
        self.inner.set_run_status(run_id, status, error).await
    }

    async fn try_start_run(&self, tenant: &str, run_id: &str) -> runic_substrate::Result<bool> {
        self.inner.try_start_run(tenant, run_id).await
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
    let Some(h) = common::harness().await else {
        return;
    };
    let store = Arc::new(SlowStore {
        inner: MemorySessionStore::new(),
        delay: Duration::from_millis(200),
    });
    let app = router(
        runic_serve::ServeConfig::new(
            runic_substrate::Sessions::from(store.clone() as Arc<dyn SessionStore>),
            h.blobs.clone(),
            h.pool.clone(),
        )
        .agent("main", scripted_agent()),
    );

    let resp = app
        .oneshot(common::wait_request("t1", &h.tenant, "ping"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let stored = store.read(&h.tenant, "t1").await.unwrap();
    assert!(
        stored
            .iter()
            .any(|s| matches!(s.event, runic_substrate::SessionEvent::RunEnd { .. })),
        "RunEnd must be durable before the wait response returns"
    );
}
