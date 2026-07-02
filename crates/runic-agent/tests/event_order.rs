//! Exact `SessionEvent` ordering for the non-happy paths: substitution, the two
//! cancellation timings, and provider failure (with and without a prior tool
//! round-trip). The persisted event log is the audit trail, so its shape is a
//! contract worth pinning precisely.

mod harness;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use harness::*;
use runic_agent::{Agent, CancelToken, RunContext};
use runic_hook::{HookLifecycle, HookOutcome, HookSignal, ReadHook, WriteHook};
use runic_provider::ProviderError;
use runic_state::{AgentState, SessionEvent};
use runic_tool::ToolResult;
use runic_types::ToolCall;

#[tokio::test]
async fn every_hook_firing_leaves_a_hookran_entry() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "rec", serde_json::json!({})),
        text_response("done"),
    ]));
    let log = Arc::new(Mutex::new(Vec::new()));
    let hook =
        RecordWriteHook::new("sub", log).act("before_tool", Act::Substitute("hooked".into()));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(RecordingTool::new("rec", "REAL")))
        .write_hook(Arc::new(hook))
        .build();
    let mut events = capture_session_events(&mut agent);

    agent.run("go").await.unwrap();

    assert_eq!(
        session_kinds(&drain(&mut events)),
        vec![
            "RunStart",
            "Message",      // user
            "HookRan",      // before_agent (continue)
            "HookRan",      // before_model (continue)
            "Message",      // assistant (tool_use)
            "HookRan",      // after_model (continue)
            "TurnBoundary", // turn 1
            "HookRan",      // before_tool (substitute)
            "HookRan",      // after_tool (continue)
            "Message",      // substituted tool result
            "HookRan",      // before_model (continue)
            "Message",      // assistant (final text)
            "HookRan",      // after_model (continue)
            "TurnBoundary", // turn 2
            "HookRan",      // after_agent (continue)
            "RunEnd",
        ]
    );
}

struct BeforeToolOnly;

#[async_trait]
impl WriteHook for BeforeToolOnly {
    fn name(&self) -> &str {
        "before-tool-only"
    }
    fn points(&self) -> &'static [HookLifecycle] {
        &[HookLifecycle::BeforeTool]
    }
    async fn before_tool(&self, _state: &mut AgentState, _call: &mut ToolCall) -> HookOutcome {
        HookOutcome::SubstituteToolResult(ToolResult::ok("hooked"))
    }
}

#[tokio::test]
async fn scoped_hook_fires_only_at_its_declared_points() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "rec", serde_json::json!({})),
        text_response("done"),
    ]));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(RecordingTool::new("rec", "REAL")))
        .write_hook(Arc::new(BeforeToolOnly))
        .build();
    let mut events = capture_session_events(&mut agent);

    agent.run("go").await.unwrap();

    assert_eq!(
        session_kinds(&drain(&mut events)),
        vec![
            "RunStart",
            "Message",      // user
            "Message",      // assistant (tool_use)
            "TurnBoundary", // turn 1
            "HookRan",      // before_tool (substitute) — the only firing
            "Message",      // substituted tool result
            "Message",      // assistant (final text)
            "TurnBoundary", // turn 2
            "RunEnd",
        ]
    );
}

struct AfterToolWatcher;

#[async_trait]
impl ReadHook for AfterToolWatcher {
    fn name(&self) -> &str {
        "after-tool-watcher"
    }
    fn points(&self) -> &'static [HookLifecycle] {
        &[HookLifecycle::AfterTool]
    }
    async fn after_tool(
        &self,
        _state: &AgentState,
        _call: &ToolCall,
        _result: &ToolResult,
    ) -> HookSignal {
        HookSignal::Continue
    }
}

#[tokio::test]
async fn scoped_read_hook_records_one_entry_with_full_fields() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "rec", serde_json::json!({})),
        text_response("done"),
    ]));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(RecordingTool::new("rec", "ran")))
        .read_hook(Arc::new(AfterToolWatcher))
        .build();
    let mut events = capture_session_events(&mut agent);

    agent.run("go").await.unwrap();

    let hook_events: Vec<SessionEvent> = drain(&mut events)
        .into_iter()
        .filter(|e| matches!(e, SessionEvent::HookRan { .. }))
        .collect();
    assert_eq!(hook_events.len(), 1);
    let SessionEvent::HookRan {
        hook,
        lifecycle,
        hook_kind,
        outcome,
        note,
        ..
    } = &hook_events[0]
    else {
        unreachable!()
    };
    assert_eq!(hook, "after-tool-watcher");
    assert_eq!(*lifecycle, HookLifecycle::AfterTool);
    assert_eq!(hook_kind, "read");
    assert_eq!(outcome, "continue");
    assert!(note.is_none());
}

#[tokio::test]
async fn precancel_path_emits_only_runstart_message_runend() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("never")]));
    let mut agent = Agent::builder(provider, "u1", "s1").model("test").build();
    let mut events = capture_session_events(&mut agent);

    let cancel = CancelToken::new();
    cancel.cancel();
    agent
        .run_with("go", RunContext::new().with_cancel(cancel))
        .await
        .unwrap();

    assert_eq!(
        session_kinds(&drain(&mut events)),
        vec!["RunStart", "Message", "RunEnd"]
    );
}

#[tokio::test]
async fn cancel_after_tool_stops_before_the_next_assistant_message() {
    // Turn 1's tool flips the token; the log ends right after the tool result,
    // with no second assistant message and no second TurnBoundary.
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "cancel_tool", serde_json::json!({})),
        text_response("should-not-run"),
    ]));
    let cancel = CancelToken::new();
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(CancelTool {
            token: cancel.clone(),
        }))
        .build();
    let mut events = capture_session_events(&mut agent);

    agent
        .run_with("go", RunContext::new().with_cancel(cancel))
        .await
        .unwrap();

    assert_eq!(
        session_kinds(&drain(&mut events)),
        vec![
            "RunStart",
            "Message",      // user
            "Message",      // assistant (tool_use)
            "TurnBoundary", // turn 1
            "Message",      // tool result
            "RunEnd",       // cancelled at the next boundary
        ]
    );
}

#[tokio::test]
async fn first_call_failure_emits_runstart_message_runend() {
    let provider = Arc::new(ScriptedProvider::with_results(vec![Err(
        ProviderError::AuthenticationFailed("nope".into()),
    )]));
    let mut agent = Agent::builder(provider, "u1", "s1").model("test").build();
    let mut events = capture_session_events(&mut agent);

    agent.run("go").await.unwrap_err();

    // No assistant message was ever appended (the call failed before that).
    assert_eq!(
        session_kinds(&drain(&mut events)),
        vec!["RunStart", "Message", "RunEnd"]
    );
}

#[tokio::test]
async fn failure_after_a_tool_round_trip_keeps_the_partial_log() {
    let provider = Arc::new(ScriptedProvider::with_results(vec![
        Ok(tool_use_response("t1", "rec", serde_json::json!({}))),
        Err(ProviderError::Api {
            status: 500,
            message: "boom".into(),
        }),
    ]));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(RecordingTool::new("rec", "ran")))
        .build();
    let mut events = capture_session_events(&mut agent);

    agent.run("go").await.unwrap_err();

    assert_eq!(
        session_kinds(&drain(&mut events)),
        vec![
            "RunStart",
            "Message",      // user
            "Message",      // assistant (tool_use)
            "TurnBoundary", // turn 1
            "Message",      // tool result
            "RunEnd",       // turn 2's model call failed
        ]
    );
}
