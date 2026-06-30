//! The streaming contract the agent *owns*: forwarding provider stream deltas
//! to the live event sink, falling back to a non-streamed call on a
//! fallback-worthy stream error, and surfacing a non-recoverable stream error.

mod harness;

use std::sync::Arc;

use harness::*;
use runic_agent::{Agent, AgentError, AgentEvent, RunContext};
use runic_provider::{CompletionResponse, ProviderError, StreamEvent};
use runic_types::{ContentBlock, StopReason, TokenUsage};

fn response(text: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::Text {
            text: text.into(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::EndTurn,
        tool_calls: vec![],
        usage: TokenUsage::default(),
    }
}

#[tokio::test]
async fn text_and_thinking_deltas_are_forwarded_in_order() {
    let provider = Arc::new(StreamProvider::new(
        vec![
            StreamEvent::ThinkingDelta {
                text: "thinking…".into(),
            },
            StreamEvent::TextDelta { text: "par".into() },
            StreamEvent::TextDelta {
                text: "tial".into(),
            },
            StreamEvent::ContentComplete {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            },
        ],
        Ok(response("partial")),
        Err(ProviderError::Parse("complete must not be called".into())),
    ));
    let mut agent = Agent::builder(provider, "u1", "s1").model("test").build();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    agent
        .run_with("go", RunContext::new().with_events(tx))
        .await
        .unwrap();

    let evs = drain(&mut rx);
    let texts: Vec<String> = evs
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["par", "tial"], "text deltas in order");
    assert!(
        evs.iter()
            .any(|e| matches!(e, AgentEvent::ThinkingDelta(t) if t == "thinking…")),
        "thinking delta forwarded"
    );
    assert_eq!(
        agent.state().last_assistant_text().as_deref(),
        Some("partial"),
        "the assembled response is what lands in history"
    );
}

#[tokio::test]
async fn fallback_worthy_stream_error_recovers_via_non_streaming_complete() {
    // stream() fails with an overloaded error (fallback-worthy); the agent
    // retries through complete(), which succeeds.
    let provider = Arc::new(StreamProvider::new(
        vec![],
        Err(ProviderError::Overloaded { retry_after_ms: 0 }),
        Ok(response("from complete")),
    ));
    let mut agent = Agent::builder(provider, "u1", "s1").model("test").build();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let outcome = agent
        .run_with("go", RunContext::new().with_events(tx))
        .await
        .unwrap();

    assert_eq!(outcome.total_turns, 1);
    assert_eq!(
        agent.state().last_assistant_text().as_deref(),
        Some("from complete"),
        "the recovered, non-streamed response is used"
    );
    // No text deltas: the streaming attempt failed before any landed.
    assert!(
        !drain(&mut rx)
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(_))),
        "a failed stream emits no text deltas"
    );
}

#[tokio::test]
async fn non_fallback_worthy_stream_error_fails_the_run() {
    // A Parse error is not fallback-worthy, so complete() is never tried and the
    // run surfaces the failure.
    let provider = Arc::new(StreamProvider::new(
        vec![],
        Err(ProviderError::Parse("garbled stream".into())),
        Ok(response("never used")),
    ));
    let mut agent = Agent::builder(provider, "u1", "s1").model("test").build();

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let err = agent
        .run_with("go", RunContext::new().with_events(tx))
        .await
        .unwrap_err();

    assert!(matches!(err, AgentError::Provider(_)), "got {err:?}");
    assert!(agent.state().current_run().is_none(), "run closed cleanly");
}
