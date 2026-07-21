//! Exact `SessionEvent` ordering for the non-happy paths: substitution, the two
//! cancellation timings, and provider failure (with and without a prior tool
//! round-trip). The persisted event log is the audit trail, so its shape is a
//! contract worth pinning precisely.

mod harness;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use harness::*;
use runic_agent::{AgentEvent, CancelToken, RunContext, Session};
use runic_hook::{HookLifecycle, HookOutcome, HookSignal, ReadHook, WriteHook};
use runic_provider::ProviderError;
use runic_state::AgentState;
use runic_tool::ToolResult;
use runic_types::ToolCall;

#[tokio::test]
async fn every_hook_execution_leaves_a_hookfired_entry() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "rec", serde_json::json!({})),
        text_response("done"),
    ]));
    let log = Arc::new(Mutex::new(Vec::new()));
    let hook =
        RecordWriteHook::new("sub", log).act("before_tool", Act::Substitute("hooked".into()));
    let mut agent = Session::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(RecordingTool::new("rec", "REAL")))
        .write_hook(Arc::new(hook))
        .build();
    let mut events = capture_session_events(&mut agent);

    agent.run("go").await.unwrap();

    assert_eq!(
        durable_kinds(&drain_session(&mut events)),
        vec![
            "RunStart",
            "Message",      // user
            "HookFired",    // before_agent (continue)
            "HookFired",    // before_model (continue)
            "Message",      // assistant (tool_use)
            "TurnEnd",      // turn 1 — durable before after_model can fail
            "HookFired",    // after_model (continue)
            "HookFired",    // before_tool (substitute)
            "ToolFinished", // substituted disposition (no ToolStarted — never ran)
            "HookFired",    // after_tool (continue)
            "Message",      // substituted tool result
            "HookFired",    // before_model (continue)
            "Message",      // assistant (final text)
            "TurnEnd",      // turn 2
            "HookFired",    // after_model (continue)
            "HookFired",    // after_agent (continue)
            "RunEnd",
        ]
    );
}

struct UnscopedOneMethod;

#[async_trait]
impl WriteHook for UnscopedOneMethod {
    fn name(&self) -> &str {
        "unscoped-one-method"
    }
    async fn before_tool(&self, _state: &mut AgentState, _call: &mut ToolCall) -> HookOutcome {
        HookOutcome::SubstituteToolResult(ToolResult::ok("hooked"))
    }
}

#[tokio::test]
async fn default_bodies_fire_without_leaving_audit_entries() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "rec", serde_json::json!({})),
        text_response("done"),
    ]));
    let mut agent = Session::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(RecordingTool::new("rec", "REAL")))
        .write_hook(Arc::new(UnscopedOneMethod))
        .build();
    let mut events = capture_session_events(&mut agent);

    agent.run("go").await.unwrap();

    let hook_events: Vec<AgentEvent> = drain_session(&mut events)
        .into_iter()
        .filter(|e| matches!(e, AgentEvent::HookFired { .. }))
        .collect();
    assert_eq!(
        hook_events.len(),
        1,
        "an unscoped hook fires at all six points but only its overridden method leaves an entry"
    );
    let AgentEvent::HookFired {
        lifecycle, outcome, ..
    } = &hook_events[0]
    else {
        unreachable!()
    };
    assert_eq!(*lifecycle, HookLifecycle::BeforeTool);
    assert_eq!(outcome, "substitute");
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
    let mut agent = Session::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(RecordingTool::new("rec", "REAL")))
        .write_hook(Arc::new(BeforeToolOnly))
        .build();
    let mut events = capture_session_events(&mut agent);

    agent.run("go").await.unwrap();

    assert_eq!(
        durable_kinds(&drain_session(&mut events)),
        vec![
            "RunStart",
            "Message",      // user
            "Message",      // assistant (tool_use)
            "TurnEnd",      // turn 1
            "HookFired",    // before_tool (substitute) — the only firing
            "ToolFinished", // substituted disposition
            "Message",      // substituted tool result
            "Message",      // assistant (final text)
            "TurnEnd",      // turn 2
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
    let mut agent = Session::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(RecordingTool::new("rec", "ran")))
        .read_hook(Arc::new(AfterToolWatcher))
        .build();
    let mut events = capture_session_events(&mut agent);

    agent.run("go").await.unwrap();

    let hook_events: Vec<AgentEvent> = drain_session(&mut events)
        .into_iter()
        .filter(|e| matches!(e, AgentEvent::HookFired { .. }))
        .collect();
    assert_eq!(hook_events.len(), 1);
    let AgentEvent::HookFired {
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

fn durable_kinds(evs: &[AgentEvent]) -> Vec<&'static str> {
    session_kinds(evs)
        .into_iter()
        .filter(|kind| *kind != "TextDelta" && *kind != "ThinkingDelta")
        .collect()
}

fn terminal_shape(evs: &[AgentEvent]) -> (usize, usize, Option<runic_state::RunEndStatus>) {
    let starts = evs
        .iter()
        .filter(|e| matches!(e, AgentEvent::RunStarted { .. }))
        .count();
    let ends: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            AgentEvent::RunEnd { status, .. } => Some(status.clone()),
            _ => None,
        })
        .collect();
    (starts, ends.len(), ends.first().cloned())
}

#[tokio::test]
async fn every_hook_failure_point_still_yields_exactly_one_terminal_event() {
    for point in ["before_agent", "before_model", "after_model", "after_agent"] {
        let provider = Arc::new(ScriptedProvider::new(vec![
            text_response("a"),
            text_response("b"),
        ]));
        let log = Arc::new(Mutex::new(Vec::new()));
        let hook = RecordWriteHook::new("bomb", log).act(point, Act::Stop);
        let mut agent = Session::builder(provider, "u1", "s1")
            .model("test")
            .write_hook(Arc::new(hook))
            .build();
        let mut events = capture_session_events(&mut agent);

        let result = agent.run("go").await;
        assert!(result.is_err(), "hook Stop at {point} must fail the run");

        let (starts, ends, status) = terminal_shape(&drain_session(&mut events));
        assert_eq!(starts, 1, "{point}: exactly one RunStart");
        assert_eq!(ends, 1, "{point}: exactly one terminal RunEnd");
        assert!(
            matches!(status, Some(runic_state::RunEndStatus::Failed(_))),
            "{point}: terminal status is Failed"
        );
    }
}

#[tokio::test]
async fn a_failing_after_model_hook_cannot_erase_the_turns_accounting() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("paid for")]));
    let log = Arc::new(Mutex::new(Vec::new()));
    let hook = RecordWriteHook::new("bomb", log).act("after_model", Act::Stop);
    let mut agent = Session::builder(provider, "u1", "s1")
        .model("test")
        .write_hook(Arc::new(hook))
        .build();
    let mut events = capture_session_events(&mut agent);

    assert!(agent.run("go").await.is_err());

    let evs = drain_session(&mut events);
    let turn_ends = evs
        .iter()
        .filter(|e| matches!(e, AgentEvent::TurnEnd { .. }))
        .count();
    assert_eq!(
        turn_ends, 1,
        "the model call happened and cost money — its TurnEnd must survive the hook failure"
    );
    let (starts, ends, _) = terminal_shape(&evs);
    assert_eq!((starts, ends), (1, 1));
}

#[tokio::test]
async fn provider_failure_yields_exactly_one_failed_terminal_event() {
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let mut agent = Session::builder(provider, "u1", "s1").model("test").build();
    let mut events = capture_session_events(&mut agent);

    assert!(agent.run("go").await.is_err());

    let (starts, ends, status) = terminal_shape(&drain_session(&mut events));
    assert_eq!((starts, ends), (1, 1));
    assert!(matches!(status, Some(runic_state::RunEndStatus::Failed(_))));
}

#[tokio::test]
async fn the_audit_stamp_carries_actor_and_model_but_never_the_config_map() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("ok")]));
    let mut agent = Session::builder(provider, "u1", "s1").model("test").build();
    let mut events = capture_session_events(&mut agent);

    let ctx = RunContext::new()
        .config_value("api_key", serde_json::json!("sk-supersecret"))
        .config_value("db_password", serde_json::json!("hunter2"))
        .with_actor("user-42");
    agent.run_with("go", ctx).await.unwrap();

    let evs = drain_session(&mut events);
    let audit = evs
        .iter()
        .find_map(|e| match e {
            AgentEvent::RunStarted { audit, .. } => audit.clone(),
            _ => None,
        })
        .expect("run start carries an audit stamp");
    assert_eq!(audit.actor.as_deref(), Some("user-42"));
    assert_eq!(audit.model.as_deref(), Some("test"));
    let json = serde_json::to_string(&audit).unwrap();
    assert!(!json.contains("supersecret"), "{json}");
    assert!(!json.contains("hunter2"), "{json}");
    assert!(!json.contains("api_key"), "{json}");

    let all = format!("{evs:?}");
    assert!(!all.contains("supersecret"), "no event may carry config");
}

#[tokio::test]
async fn precancel_path_emits_only_runstart_message_runend() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("never")]));
    let mut agent = Session::builder(provider, "u1", "s1").model("test").build();
    let mut events = capture_session_events(&mut agent);

    let cancel = CancelToken::new();
    cancel.cancel();
    agent
        .run_with("go", RunContext::new().with_cancel(cancel))
        .await
        .unwrap();

    assert_eq!(
        durable_kinds(&drain_session(&mut events)),
        vec!["RunStart", "Message", "RunEnd"]
    );
}

#[tokio::test]
async fn cancel_after_tool_stops_before_the_next_assistant_message() {
    // Turn 1's tool flips the token; the log ends right after the tool result,
    // with no second assistant message and no second TurnEnd.
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "cancel_tool", serde_json::json!({})),
        text_response("should-not-run"),
    ]));
    let cancel = CancelToken::new();
    let mut agent = Session::builder(provider, "u1", "s1")
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
        durable_kinds(&drain_session(&mut events)),
        vec![
            "RunStart",
            "Message",      // user
            "Message",      // assistant (tool_use)
            "TurnEnd",      // turn 1
            "ToolStarted",  // real dispatch
            "ToolFinished", // ok, timed
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
    let mut agent = Session::builder(provider, "u1", "s1").model("test").build();
    let mut events = capture_session_events(&mut agent);

    agent.run("go").await.unwrap_err();

    // No assistant message was ever appended (the call failed before that).
    assert_eq!(
        durable_kinds(&drain_session(&mut events)),
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
    let mut agent = Session::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(RecordingTool::new("rec", "ran")))
        .build();
    let mut events = capture_session_events(&mut agent);

    agent.run("go").await.unwrap_err();

    assert_eq!(
        durable_kinds(&drain_session(&mut events)),
        vec![
            "RunStart",
            "Message",      // user
            "Message",      // assistant (tool_use)
            "TurnEnd",      // turn 1
            "ToolStarted",  // real dispatch
            "ToolFinished", // ok, timed
            "Message",      // tool result
            "RunEnd",       // turn 2's model call failed
        ]
    );
}
