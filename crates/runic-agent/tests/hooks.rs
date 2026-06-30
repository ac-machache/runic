//! Hook contract: firing order across all lifecycle points, priority ordering,
//! before-model mutation reaching the provider, before-tool substitution, and
//! the several ways a hook can halt the run.

mod harness;

use std::sync::{Arc, Mutex};

use harness::*;
use runic_agent::{Agent, AgentError};
use runic_types::MessageContent;

#[tokio::test]
async fn write_hook_lifecycle_points_fire_in_loop_order() {
    // One tool round-trip so every point is exercised.
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "rec", serde_json::json!({})),
        text_response("done"),
    ]));
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(RecordingTool::new("rec", "ok")))
        .write_hook(Arc::new(RecordWriteHook::new("h", log.clone())))
        .build();

    agent.run("go").await.unwrap();

    let got = log.lock().unwrap().clone();
    // before_agent once; before_model/after_model per turn (2 turns);
    // before_tool/after_tool around the dispatch; after_agent last.
    let expected = vec![
        "h:before_agent",
        "h:before_model", // turn 1
        "h:after_model",
        "h:before_tool",
        "h:after_tool",
        "h:before_model", // turn 2
        "h:after_model",
        "h:after_agent",
    ];
    assert_eq!(got, expected, "write-hook firing order");
}

#[tokio::test]
async fn write_hooks_run_in_priority_order() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("ok")]));
    let log = Arc::new(Mutex::new(Vec::new()));
    // `lo` has the lower priority value, so it must run before `hi`.
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .write_hook(Arc::new(
            RecordWriteHook::new("hi", log.clone()).priority(10),
        ))
        .write_hook(Arc::new(
            RecordWriteHook::new("lo", log.clone()).priority(-10),
        ))
        .build();

    agent.run("go").await.unwrap();

    let order: Vec<String> = log
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.ends_with(":before_model"))
        .cloned()
        .collect();
    assert_eq!(order, vec!["lo:before_model", "hi:before_model"]);
}

#[tokio::test]
async fn write_hook_runs_before_read_hook_at_the_tool_seam() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "rec", serde_json::json!({})),
        text_response("done"),
    ]));
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(RecordingTool::new("rec", "ok")))
        .write_hook(Arc::new(RecordWriteHook::new("w", log.clone())))
        .read_hook(Arc::new(RecordReadHook::new("r", log.clone())))
        .build();

    agent.run("go").await.unwrap();

    let got = log.lock().unwrap().clone();
    let w = got.iter().position(|e| e == "w:before_tool").unwrap();
    let r = got.iter().position(|e| e == "read:r:before_tool").unwrap();
    assert!(
        w < r,
        "write before_tool must precede read before_tool: {got:?}"
    );
}

#[tokio::test]
async fn before_model_mutation_is_visible_to_the_provider() {
    // A write hook injects a user message before the model call; the provider's
    // request must contain it.
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("ok")]));
    let log = Arc::new(Mutex::new(Vec::new()));
    let hook = RecordWriteHook::new("inj", log)
        .act("before_model", Act::Inject("injected-by-hook".into()));
    let mut agent = Agent::builder(provider.clone(), "u1", "s1")
        .model("test")
        .write_hook(Arc::new(hook))
        .build();

    agent.run("go").await.unwrap();

    let req = provider.last_request();
    assert!(
        req.messages
            .iter()
            .any(|m| matches!(&m.content, MessageContent::Text(t) if t == "injected-by-hook")),
        "the provider must see the hook's before_model mutation"
    );
}

#[tokio::test]
async fn before_tool_substitution_skips_real_execution() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "rec", serde_json::json!({})),
        text_response("done"),
    ]));
    let rec = Arc::new(RecordingTool::new("rec", "REAL"));
    let calls = rec.log();
    let log = Arc::new(Mutex::new(Vec::new()));
    let hook =
        RecordWriteHook::new("sub", log).act("before_tool", Act::Substitute("from-hook".into()));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(rec)
        .write_hook(Arc::new(hook))
        .build();

    agent.run("go").await.unwrap();

    assert!(
        calls.lock().unwrap().is_empty(),
        "substituted call must not execute the real tool"
    );
    let contents = tool_result_contents(&agent.state().messages_for_provider());
    assert!(
        contents.iter().any(|c| c == "from-hook"),
        "the substituted result is what reaches history: {contents:?}"
    );
    assert!(!contents.iter().any(|c| c.contains("REAL")));
}

#[tokio::test]
async fn write_hook_stop_before_model_halts_with_no_model_call() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("never")]));
    let log = Arc::new(Mutex::new(Vec::new()));
    let hook = RecordWriteHook::new("stopper", log).act("before_model", Act::Stop);
    let mut agent = Agent::builder(provider.clone(), "u1", "s1")
        .model("test")
        .write_hook(Arc::new(hook))
        .build();

    let err = agent.run("go").await.unwrap_err();
    assert!(matches!(err, AgentError::HookStop));
    assert_eq!(
        provider.call_count(),
        0,
        "stop before_model must prevent the call"
    );
    assert!(agent.state().current_run().is_none(), "run closed cleanly");
}

#[tokio::test]
async fn write_hook_cancel_at_before_tool_substitutes_an_error_and_continues() {
    // `Cancel` at the tool seam is in-band: the real tool is skipped, the model
    // gets an error result, and the run proceeds to the next turn.
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "rec", serde_json::json!({})),
        text_response("recovered"),
    ]));
    let rec = Arc::new(RecordingTool::new("rec", "REAL"));
    let calls = rec.log();
    let log = Arc::new(Mutex::new(Vec::new()));
    let hook = RecordWriteHook::new("canceller", log)
        .act("before_tool", Act::Cancel("blocked by policy".into()));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(rec)
        .write_hook(Arc::new(hook))
        .build();

    let outcome = agent.run("go").await.unwrap();

    assert_eq!(outcome.total_turns, 2, "the run continued past the cancel");
    assert!(calls.lock().unwrap().is_empty(), "cancelled tool never ran");
    let contents = tool_result_contents(&agent.state().messages_for_provider());
    assert!(
        contents.iter().any(|c| c.contains("blocked by policy")),
        "cancel reason becomes the model-facing error result: {contents:?}"
    );
}

#[tokio::test]
async fn read_hook_stop_at_before_model_halts_the_run() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("never")]));
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::builder(provider.clone(), "u1", "s1")
        .model("test")
        .read_hook(Arc::new(
            RecordReadHook::new("r", log).stop_at("before_model"),
        ))
        .build();

    let err = agent.run("go").await.unwrap_err();
    assert!(matches!(err, AgentError::HookStop));
    assert_eq!(provider.call_count(), 0);
}

#[tokio::test]
async fn write_hook_stop_at_after_model_halts_before_a_second_turn() {
    // The first turn requests a tool; an after_model Stop halts the run before
    // any dispatch or second model call.
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "rec", serde_json::json!({})),
        text_response("never"),
    ]));
    let rec = Arc::new(RecordingTool::new("rec", "REAL"));
    let calls = rec.log();
    let log = Arc::new(Mutex::new(Vec::new()));
    let hook = RecordWriteHook::new("stopper", log).act("after_model", Act::Stop);
    let mut agent = Agent::builder(provider.clone(), "u1", "s1")
        .model("test")
        .tool(rec)
        .write_hook(Arc::new(hook))
        .build();

    let err = agent.run("go").await.unwrap_err();
    assert!(matches!(err, AgentError::HookStop));
    assert_eq!(provider.call_count(), 1, "no second model call");
    assert!(
        calls.lock().unwrap().is_empty(),
        "no tool dispatched after stop"
    );
    assert!(agent.state().current_run().is_none());
}

#[tokio::test]
async fn write_hook_cancel_at_after_model_halts_the_run() {
    // `Cancel` at a non-tool lifecycle point maps to a hard stop.
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("only")]));
    let log = Arc::new(Mutex::new(Vec::new()));
    let hook = RecordWriteHook::new("c", log).act("after_model", Act::Cancel("stop now".into()));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .write_hook(Arc::new(hook))
        .build();

    let err = agent.run("go").await.unwrap_err();
    assert!(matches!(err, AgentError::HookStop));
}

#[tokio::test]
async fn write_hook_stop_at_after_tool_halts_after_the_result_is_recorded() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "rec", serde_json::json!({})),
        text_response("never"),
    ]));
    let rec = Arc::new(RecordingTool::new("rec", "ran"));
    let calls = rec.log();
    let log = Arc::new(Mutex::new(Vec::new()));
    let hook = RecordWriteHook::new("stopper", log).act("after_tool", Act::Stop);
    let mut agent = Agent::builder(provider.clone(), "u1", "s1")
        .model("test")
        .tool(rec)
        .write_hook(Arc::new(hook))
        .build();

    let err = agent.run("go").await.unwrap_err();
    assert!(matches!(err, AgentError::HookStop));
    // The tool *did* run (the stop is after it), but there's no second turn.
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert_eq!(
        provider.call_count(),
        1,
        "halted before the next model call"
    );
}

#[tokio::test]
async fn write_hook_cancel_at_after_tool_halts_the_run() {
    // The tool already ran, so `Cancel` here can't be in-band like at
    // `before_tool`; it halts the run, matching every other non-`before_tool`
    // seam. (No outcome is silently dropped.)
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "rec", serde_json::json!({})),
        text_response("should-not-run"),
    ]));
    let rec = Arc::new(RecordingTool::new("rec", "ran"));
    let calls = rec.log();
    let log = Arc::new(Mutex::new(Vec::new()));
    let hook =
        RecordWriteHook::new("c", log).act("after_tool", Act::Cancel("stop after tool".into()));
    let mut agent = Agent::builder(provider.clone(), "u1", "s1")
        .model("test")
        .tool(rec)
        .write_hook(Arc::new(hook))
        .build();

    let err = agent.run("go").await.unwrap_err();
    assert!(matches!(err, AgentError::HookStop));
    // The tool did run (the cancel is after it), but there's no second turn.
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert_eq!(
        provider.call_count(),
        1,
        "halted before the next model call"
    );
}

#[tokio::test]
async fn read_hook_stop_at_after_tool_halts_the_run() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "rec", serde_json::json!({})),
        text_response("never"),
    ]));
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::builder(provider.clone(), "u1", "s1")
        .model("test")
        .tool(Arc::new(RecordingTool::new("rec", "ran")))
        .read_hook(Arc::new(
            RecordReadHook::new("r", log).stop_at("after_tool"),
        ))
        .build();

    let err = agent.run("go").await.unwrap_err();
    assert!(matches!(err, AgentError::HookStop));
    assert_eq!(provider.call_count(), 1, "no second model call after stop");
}
