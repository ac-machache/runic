mod common;

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use axum::http::StatusCode;
use runic::types::{ContentBlock, StopReason, TokenUsage};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::hook::{FinishedRun, RunHook};
use runic_serve::router;
use tower::ServiceExt;

struct ScriptedProvider;

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: "the answer".into(),
                provider_metadata: None,
            }],
            stop_reason: StopReason::EndTurn,
            tool_calls: vec![],
            usage: TokenUsage::default(),
        })
    }
}

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<FinishedRun>>>);

#[async_trait]
impl RunHook for Recorder {
    async fn finished(&self, run: &FinishedRun) -> anyhow::Result<()> {
        self.0.lock().expect("recorder poisoned").push(run.clone());
        Ok(())
    }
}

async fn settled(recorder: &Recorder) -> Option<FinishedRun> {
    for _ in 0..200 {
        if let Some(run) = recorder.0.lock().expect("recorder poisoned").first() {
            return Some(run.clone());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    None
}

#[tokio::test]
async fn a_named_hook_runs_with_the_answer_once_the_run_settles() {
    let Some(h) = common::harness().await else {
        return;
    };
    let recorder = Recorder::default();
    let app = router(
        h.config()
            .agent("main", common::agent(Arc::new(ScriptedProvider)))
            .hook("record", recorder.clone()),
    );

    let resp = app
        .clone()
        .oneshot(common::post_json(
            "/runs/forget",
            &h.tenant,
            r#"{"message":"hello","hook":"record"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let run_id = common::body_json(resp).await["run_id"]
        .as_str()
        .unwrap()
        .to_string();

    let Some(fired) = settled(&recorder).await else {
        let resp = app
            .oneshot(common::get(&format!("/runs/{run_id}"), &h.tenant))
            .await
            .unwrap();
        panic!(
            "the hook never fired; run row says {}",
            common::body_string(resp).await
        );
    };
    assert_eq!(fired.output.text, "the answer");
    assert_eq!(fired.status.as_str(), "successful");
    assert!(fired.stateless(), "a forget run carries no session");
    assert!(fired.session_id.is_none());
}

#[tokio::test]
async fn a_session_run_fires_its_hook_and_names_the_session() {
    let Some(harness) = common::harness().await else {
        return;
    };
    let recorder = Recorder::default();
    let app = router(
        harness
            .config()
            .agent("main", common::agent(Arc::new(ScriptedProvider)))
            .hook("record", recorder.clone()),
    );

    let session_id = common::uid("session");
    let resp = app
        .oneshot(common::post_json(
            &format!("/sessions/{session_id}/runs/wait"),
            &harness.tenant,
            r#"{"message":"hello","hook":"record"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let fired = settled(&recorder).await.expect("the hook never fired");
    assert_eq!(fired.output.text, "the answer");
    assert_eq!(fired.status.as_str(), "successful");
    assert_eq!(fired.session_id.as_deref(), Some(session_id.as_str()));
    assert!(!fired.stateless());
}

#[tokio::test]
async fn a_run_without_a_hook_fires_nothing() {
    let Some(h) = common::harness().await else {
        return;
    };
    let recorder = Recorder::default();
    let app = router(
        h.config()
            .agent("main", common::agent(Arc::new(ScriptedProvider)))
            .hook("record", recorder.clone()),
    );

    let resp = app
        .oneshot(common::post_json(
            "/runs/forget",
            &h.tenant,
            r#"{"message":"hello"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert!(
        recorder.0.lock().expect("recorder poisoned").is_empty(),
        "naming no hook must run no hook"
    );
}

#[tokio::test]
async fn an_unregistered_hook_name_is_rejected_before_the_run_is_queued() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = router(
        h.config()
            .agent("main", common::agent(Arc::new(ScriptedProvider)))
            .hook("record", Recorder::default()),
    );

    let resp = app
        .oneshot(common::post_json(
            "/runs/forget",
            &h.tenant,
            r#"{"message":"hello","hook":"typo"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = common::body_string(resp).await;
    assert!(
        body.contains("typo"),
        "the error names the bad hook: {body}"
    );
    assert!(body.contains("record"), "and lists what is registered");
}
