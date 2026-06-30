//! Property tests over the real loop: whatever the (bounded, random) shape of a
//! run, the structural invariants hold — bookended events, turn/usage counting,
//! terminal state, and the persistence-summary guarantee.

mod harness;

use std::sync::Arc;

use harness::*;
use proptest::prelude::*;
use runic_agent::{Agent, CancelToken, RunContext};
use runic_state::SessionEvent;

const FULL: &str = "FULL_SECRET_BYTES";
const SUMMARY: &str = "summary; content omitted from log";

/// Build a run script: `tool_turns` tool calls (distinct args, so the loop
/// guard never interferes) followed by a final text answer.
fn script(tool_turns: usize) -> Vec<runic_provider::CompletionResponse> {
    let mut responses = Vec::new();
    for i in 0..tool_turns {
        responses.push(tool_use_response(
            &format!("c{i}"),
            "summary_tool",
            serde_json::json!({ "i": i }),
        ));
    }
    responses.push(text_response("final"));
    responses
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// For any number of tool turns: one RunStart, one RunEnd, the run ends
    /// non-in-flight, turns/requests/turn-boundaries all agree, and the full
    /// tool bytes never land in the persisted message view.
    #[test]
    fn loop_structural_invariants(tool_turns in 0usize..8) {
        rt().block_on(async move {
            let provider = Arc::new(ScriptedProvider::new(script(tool_turns)));
            let mut agent = Agent::builder(provider.clone(), "u", "s")
                .model("test")
                .tool(Arc::new(SummaryTool::new(FULL, SUMMARY)))
                .build();
            let mut events = capture_session_events(&mut agent);

            let outcome = agent.run("go").await.unwrap();

            let expected_turns = (tool_turns + 1) as u32;
            prop_assert_eq!(outcome.total_turns, expected_turns);
            prop_assert_eq!(provider.call_count(), tool_turns + 1);

            let evs = drain(&mut events);
            let starts = evs.iter().filter(|e| matches!(e, SessionEvent::RunStart { .. })).count();
            let ends = evs.iter().filter(|e| matches!(e, SessionEvent::RunEnd { .. })).count();
            let boundaries = evs.iter().filter(|e| matches!(e, SessionEvent::TurnBoundary { .. })).count();
            prop_assert_eq!(starts, 1, "exactly one RunStart");
            prop_assert_eq!(ends, 1, "exactly one RunEnd");
            prop_assert_eq!(boundaries as u32, expected_turns, "a TurnBoundary per turn");
            prop_assert!(
                matches!(evs.first(), Some(SessionEvent::RunStart { .. })),
                "first event is RunStart"
            );
            prop_assert!(
                matches!(evs.last(), Some(SessionEvent::RunEnd { .. })),
                "last event is RunEnd"
            );

            // All events share the single minted run id.
            let run_id = evs.first().unwrap().run_id().to_string();
            prop_assert!(evs.iter().all(|e| e.run_id() == run_id));

            // Terminal: nothing left in flight.
            prop_assert!(agent.state().current_run().is_none());

            // Persistence-summary safety holds for every executed tool turn.
            let persisted = tool_result_contents(&agent.state().messages_for_provider());
            prop_assert!(!persisted.iter().any(|c| c.contains("SECRET")));
            if tool_turns > 0 {
                prop_assert!(persisted.iter().any(|c| c.contains("omitted from log")));
            }
            Ok(())
        })?;
    }

    /// A multi-run session over the same agent always lands every run in a
    /// terminal, grouped-in-order state, and each run mints a distinct id.
    #[test]
    fn repeated_runs_each_terminate(run_count in 1usize..5) {
        rt().block_on(async move {
            let responses: Vec<_> = (0..run_count).map(|i| text_response(&format!("a{i}"))).collect();
            let provider = Arc::new(ScriptedProvider::new(responses));
            let mut agent = Agent::builder(provider, "u", "s").model("test").build();

            for i in 0..run_count {
                agent.run(format!("msg {i}")).await.unwrap();
                prop_assert!(agent.state().current_run().is_none());
            }

            let runs = agent.state().runs();
            prop_assert_eq!(runs.len(), run_count);
            prop_assert!(runs.iter().all(|r| r.ended_at.is_some()));
            let mut ids: Vec<&str> = runs.iter().map(|r| r.id.as_str()).collect();
            let n = ids.len();
            ids.sort_unstable();
            ids.dedup();
            prop_assert_eq!(ids.len(), n, "run ids are distinct");
            Ok(())
        })?;
    }
}

/// Map a kind code to a tool name: 0 ⇒ ok, 1 ⇒ Err, 2 ⇒ panic.
fn kind_tool(kind: u8) -> &'static str {
    match kind % 3 {
        0 => "rec",
        1 => "err_tool",
        _ => "panic_tool",
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// A run made of arbitrarily-failing tool turns (success / Err / panic) still
    /// satisfies every structural invariant — failures are all in-band, so the
    /// loop never aborts mid-run.
    #[test]
    fn tool_failures_preserve_loop_invariants(kinds in prop::collection::vec(0u8..3, 0..7)) {
        rt().block_on(async move {
            let mut responses = Vec::new();
            for (i, k) in kinds.iter().enumerate() {
                responses.push(tool_use_response(&format!("c{i}"), kind_tool(*k), serde_json::json!({ "i": i })));
            }
            responses.push(text_response("final"));

            let provider = Arc::new(ScriptedProvider::new(responses));
            let mut agent = Agent::builder(provider.clone(), "u", "s")
                .model("test")
                .tool(Arc::new(RecordingTool::new("rec", "ran")))
                .tool(Arc::new(ErrTool))
                .tool(Arc::new(PanicTool))
                .build();
            let mut events = capture_session_events(&mut agent);

            let outcome = agent.run("go").await.unwrap();

            let expected_turns = (kinds.len() + 1) as u32;
            prop_assert_eq!(outcome.total_turns, expected_turns);
            prop_assert_eq!(provider.call_count(), kinds.len() + 1);

            let evs = drain(&mut events);
            prop_assert_eq!(evs.iter().filter(|e| matches!(e, SessionEvent::RunStart { .. })).count(), 1);
            prop_assert_eq!(evs.iter().filter(|e| matches!(e, SessionEvent::RunEnd { .. })).count(), 1);
            prop_assert!(
                matches!(evs.last(), Some(SessionEvent::RunEnd { .. })),
                "last event is RunEnd"
            );
            prop_assert!(agent.state().current_run().is_none());
            Ok(())
        })?;
    }

    /// Cancellation requested at any turn boundary always yields a terminal,
    /// `cancelled` run; otherwise the run completes normally. Either way nothing
    /// is left in flight.
    #[test]
    fn random_cancellation_is_always_terminal(
        n in 1usize..6,
        cancel in any::<bool>(),
        cancel_idx in 0usize..6,
    ) {
        rt().block_on(async move {
            let cancel_idx = cancel_idx % n;
            let token = CancelToken::new();

            let mut responses = Vec::new();
            for i in 0..n {
                let name = if cancel && i == cancel_idx { "cancel_tool" } else { "rec" };
                responses.push(tool_use_response(&format!("c{i}"), name, serde_json::json!({ "i": i })));
            }
            responses.push(text_response("final"));

            let provider = Arc::new(ScriptedProvider::new(responses));
            let mut agent = Agent::builder(provider.clone(), "u", "s")
                .model("test")
                .tool(Arc::new(RecordingTool::new("rec", "ran")))
                .tool(Arc::new(CancelTool { token: token.clone() }))
                .build();

            let outcome = agent
                .run_with("go", RunContext::new().with_cancel(token))
                .await
                .unwrap();

            if cancel {
                prop_assert_eq!(outcome.stop_reason.as_deref(), Some("cancelled"));
                prop_assert_eq!(outcome.total_turns, (cancel_idx + 1) as u32);
            } else {
                prop_assert_eq!(outcome.stop_reason.as_deref(), Some("end_turn"));
                prop_assert_eq!(outcome.total_turns, (n + 1) as u32);
            }
            prop_assert!(agent.state().current_run().is_none());
            Ok(())
        })?;
    }
}
