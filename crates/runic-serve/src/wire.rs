//! Server-sent event payload types — the runic-native wire format.
//!
//! Each enum variant is one SSE event. The `type` discriminator goes both in
//! the JSON body and in the SSE `event:` field, so clients can switch on
//! either. Event ids on the wire are the [`runic_substrate`] store-assigned
//! seq numbers, used for `Last-Event-ID` resume.
//!
//! Live runs emit deltas (`assistant_text_delta`, …) that don't show up in
//! replay — the persisted log records full messages, not incremental tokens.
//! Clients should handle both shapes.

use chrono::{DateTime, Utc};
use runic_agent::AgentEvent;
use runic_state::SessionEvent;
use runic_types::Message;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireEvent {
    /// A run is starting. `at` is present only on replay (the persisted
    /// `RunStart` carries a timestamp; the live event does not).
    RunStart {
        run_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        at: Option<DateTime<Utc>>,
    },

    /// Streaming token from the assistant — live runs only.
    AssistantTextDelta { text: String },

    /// Streaming thinking token (only when the provider exposes thinking).
    AssistantThinkingDelta { text: String },

    /// A tool call is about to run, with its input args.
    ToolStart {
        id: String,
        name: String,
        input: serde_json::Value,
    },

    /// A tool call finished (success or error). `preview` is a trimmed head of
    /// the output for at-a-glance display; the full result is in the persisted
    /// message log.
    ToolFinish {
        id: String,
        name: String,
        is_error: bool,
        preview: String,
    },

    /// One model turn just finished — live runs only.
    TurnComplete { turn: u32, stop_reason: String },

    /// A complete message landed in agent state. Replay only (live runs
    /// surface the same content as deltas + the persisted log).
    Message {
        run_id: String,
        msg: Message,
        at: DateTime<Utc>,
    },

    /// A run finished — replay only.
    RunEnd {
        run_id: String,
        total_turns: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        at: DateTime<Utc>,
    },

    /// Token usage — emitted at run end.
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },

    /// A HITL `ask_user` is waiting for an operator answer. The run is parked
    /// until an answer is POSTed to `.../asks/{ask_id}`.
    AskRequired {
        ask_id: String,
        question: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        context: Option<String>,
    },

    /// A HITL `escalate_to_human` fired — fire-and-forget, the run continues.
    Escalated {
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },

    /// Non-fatal server-side warning (e.g. a run task that failed to join).
    Warning { message: String },

    /// Sent once after a stream finishes (live or replay). Clients use this to
    /// close their EventSource cleanly.
    Done { total_turns: u32 },
}

impl WireEvent {
    /// The discriminator string — used as the SSE `event:` field so clients
    /// can route without parsing the body.
    pub fn event_kind(&self) -> &'static str {
        match self {
            Self::RunStart { .. } => "run_start",
            Self::AssistantTextDelta { .. } => "assistant_text_delta",
            Self::AssistantThinkingDelta { .. } => "assistant_thinking_delta",
            Self::ToolStart { .. } => "tool_start",
            Self::ToolFinish { .. } => "tool_finish",
            Self::TurnComplete { .. } => "turn_complete",
            Self::Message { .. } => "message",
            Self::RunEnd { .. } => "run_end",
            Self::Usage { .. } => "usage",
            Self::AskRequired { .. } => "ask_required",
            Self::Escalated { .. } => "escalated",
            Self::Warning { .. } => "warning",
            Self::Done { .. } => "done",
        }
    }
}

/// Convert a live [`AgentEvent`] (token-level, from `RunContext::with_events`)
/// into wire events. One agent event can fan into several wire events — a
/// completed run yields both `usage` and `done`.
pub fn from_agent_event(event: AgentEvent) -> Vec<WireEvent> {
    match event {
        AgentEvent::RunStarted { run_id } => vec![WireEvent::RunStart { run_id, at: None }],
        AgentEvent::TextDelta(text) => vec![WireEvent::AssistantTextDelta { text }],
        AgentEvent::ThinkingDelta(text) => vec![WireEvent::AssistantThinkingDelta { text }],
        AgentEvent::ToolStarted { id, name, input } => {
            vec![WireEvent::ToolStart { id, name, input }]
        }
        AgentEvent::ToolFinished {
            id,
            name,
            is_error,
            result,
        } => {
            vec![WireEvent::ToolFinish {
                id,
                name,
                is_error,
                preview: truncate(&result, 4000),
            }]
        }
        AgentEvent::TurnCompleted { turn, stop_reason } => {
            vec![WireEvent::TurnComplete { turn, stop_reason }]
        }
        AgentEvent::RunCompleted(outcome) => vec![
            WireEvent::Usage {
                input_tokens: outcome.usage.input_tokens,
                output_tokens: outcome.usage.output_tokens,
            },
            WireEvent::Done {
                total_turns: outcome.total_turns,
            },
        ],
    }
}

/// Convert a persisted [`SessionEvent`] (whole-message granularity, from
/// `SessionStore::read`) into a wire event for replay. Returns `None` for
/// internal bookkeeping events (`TurnBoundary`, `HookRan`, `StateSnapshot`).
pub fn from_session_event(event: SessionEvent) -> Option<WireEvent> {
    match event {
        SessionEvent::RunStart { run_id, at } => Some(WireEvent::RunStart {
            run_id,
            at: Some(at),
        }),
        SessionEvent::RunEnd {
            run_id,
            outcome,
            at,
        } => Some(WireEvent::RunEnd {
            run_id,
            total_turns: outcome.total_turns,
            stop_reason: outcome.stop_reason,
            at,
        }),
        SessionEvent::Message { run_id, msg, at } => Some(WireEvent::Message { run_id, msg, at }),
        SessionEvent::TurnBoundary { .. }
        | SessionEvent::HookRan { .. }
        | SessionEvent::StateSnapshot { .. } => None,
    }
}

/// Trim a string to `max` chars (char-boundary safe), marking truncation.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_kind_matches_serde_tag() {
        let event = WireEvent::AssistantTextDelta { text: "hi".into() };
        assert_eq!(event.event_kind(), "assistant_text_delta");
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "assistant_text_delta");
    }

    #[test]
    fn text_delta_maps_one_to_one() {
        let wires = from_agent_event(AgentEvent::TextDelta("hello".into()));
        assert_eq!(wires.len(), 1);
        let WireEvent::AssistantTextDelta { text } = &wires[0] else {
            panic!()
        };
        assert_eq!(text, "hello");
    }

    #[test]
    fn run_completed_fans_into_usage_then_done() {
        let outcome = runic_state::RunOutcome {
            total_turns: 3,
            stop_reason: Some("end_turn".into()),
            usage: runic_types::TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
            },
        };
        let wires = from_agent_event(AgentEvent::RunCompleted(outcome));
        assert!(matches!(
            wires[0],
            WireEvent::Usage {
                input_tokens: 10,
                output_tokens: 20
            }
        ));
        assert!(matches!(wires[1], WireEvent::Done { total_turns: 3 }));
    }

    #[test]
    fn session_message_passes_through() {
        let evt = SessionEvent::Message {
            run_id: "r1".into(),
            msg: Message::user("hi"),
            at: Utc::now(),
        };
        let Some(WireEvent::Message { run_id, .. }) = from_session_event(evt) else {
            panic!()
        };
        assert_eq!(run_id, "r1");
    }

    #[test]
    fn turn_boundary_is_filtered_from_replay() {
        let evt = SessionEvent::TurnBoundary {
            run_id: "r1".into(),
            at: Utc::now(),
        };
        assert!(from_session_event(evt).is_none());
    }
}
