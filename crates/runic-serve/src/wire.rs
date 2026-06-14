//! Server-sent event payload types — the runic-native wire format.
//!
//! Each enum variant is one SSE event. The `type` discriminator goes
//! both in the JSON body and in the SSE `event:` field, so clients can
//! switch on either. Event ids on the wire are the [`runic_sessions`]
//! store-assigned seq numbers, used for `Last-Event-ID` resume.
//!
//! Live runs emit deltas (`assistant_text_delta`, …) that don't show
//! up in replay — the persisted log records full messages, not the
//! incremental tokens. Clients should handle both shapes.

use chrono::{DateTime, Utc};
use runic_agent_core::{AgentEvent, SessionEvent};
use runic_message_types::Message;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireEvent {
    /// Streaming token from the assistant — live runs only.
    AssistantTextDelta { text: String },

    /// Streaming thinking token (only fires when the provider exposes
    /// thinking).
    AssistantThinkingDelta { text: String },

    /// A tool call has been allocated and is about to be dispatched.
    ToolStart { id: String, name: String },

    /// A tool call has been dispatched (input shape is now known).
    ToolDispatching {
        id: String,
        name: String,
        input: serde_json::Value,
    },

    /// A tool call completed (success or error). `preview` is a trimmed
    /// head of the result for at-a-glance display; clients that need
    /// the full body can read it from the persisted message stream.
    /// `metadata` is the tool's client-facing structured payload (e.g.
    /// websearch source links for grounding chips) — passed through
    /// verbatim, never shown to the model.
    ToolFinish {
        id: String,
        name: String,
        is_error: bool,
        duration_ms: u64,
        preview: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },

    /// A complete message landed in agent state. Sent both during live
    /// runs (after deltas have flushed) and during replay.
    Message {
        run_id: String,
        msg: Message,
        at: DateTime<Utc>,
    },

    /// One model turn just finished.
    TurnComplete {
        stop_reason: Option<String>,
        tool_calls_this_turn: u32,
    },

    /// A run is starting.
    RunStart { run_id: String, at: DateTime<Utc> },

    /// A run finished — multiple turns may have happened.
    RunEnd {
        run_id: String,
        total_turns: u32,
        stop_reason: Option<String>,
        at: DateTime<Utc>,
    },

    /// Token usage update — emitted at run end.
    Usage {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cache_read_input_tokens: Option<u64>,
        cache_creation_input_tokens: Option<u64>,
    },

    /// Non-fatal warning from the agent. Doesn't end the run.
    Warning { message: String },

    /// A HITL tool is waiting for operator approval. The run is parked until
    /// a decision is POSTed to `.../approvals/{call_id}`. `draft` is the
    /// serialized [`runic_agent_core::Draft`] (summary, current input, input
    /// schema, editable fields) — everything a client needs to render a form.
    ApprovalRequired {
        call_id: String,
        tool_name: String,
        draft: serde_json::Value,
    },

    /// The model produced schema-valid structured output via the finish
    /// tool. The run ends right after this.
    StructuredOutput { result: serde_json::Value },

    /// Sent once after a stream finishes (live or replay). Clients use
    /// this to close their EventSource cleanly.
    Done { total_turns: u32 },
}

impl WireEvent {
    /// The discriminator string — used as the SSE `event:` field so
    /// clients can route on `event.type` without parsing the body.
    pub fn event_kind(&self) -> &'static str {
        match self {
            Self::AssistantTextDelta { .. } => "assistant_text_delta",
            Self::AssistantThinkingDelta { .. } => "assistant_thinking_delta",
            Self::ToolStart { .. } => "tool_start",
            Self::ToolDispatching { .. } => "tool_dispatching",
            Self::ToolFinish { .. } => "tool_finish",
            Self::Message { .. } => "message",
            Self::TurnComplete { .. } => "turn_complete",
            Self::RunStart { .. } => "run_start",
            Self::RunEnd { .. } => "run_end",
            Self::Usage { .. } => "usage",
            Self::Warning { .. } => "warning",
            Self::ApprovalRequired { .. } => "approval_required",
            Self::StructuredOutput { .. } => "structured_output",
            Self::Done { .. } => "done",
        }
    }
}

/// Convert a live [`AgentEvent`] (token-level granularity, from
/// `agent.run_streaming()`) into a wire event for SSE.
pub fn from_agent_event(event: AgentEvent) -> WireEvent {
    match event {
        AgentEvent::AssistantTextDelta(text) => WireEvent::AssistantTextDelta { text },
        AgentEvent::AssistantThinkingDelta(text) => WireEvent::AssistantThinkingDelta { text },
        AgentEvent::ToolUseStart { id, name } => WireEvent::ToolStart { id, name },
        AgentEvent::ToolDispatching(call) => WireEvent::ToolDispatching {
            id: call.id.clone(),
            name: call.name.clone(),
            input: call.input.clone(),
        },
        AgentEvent::ToolFinished {
            call,
            result,
            duration_ms,
        } => WireEvent::ToolFinish {
            id: call.id.clone(),
            name: call.name.clone(),
            is_error: result.is_error,
            duration_ms,
            preview: truncate(&result.content, 200),
            metadata: result.metadata.clone(),
        },
        AgentEvent::Usage(usage) => WireEvent::Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
        },
        AgentEvent::TurnComplete {
            stop_reason,
            tool_calls_this_turn,
        } => WireEvent::TurnComplete {
            stop_reason: stop_reason.map(|s| s.to_string()),
            tool_calls_this_turn,
        },
        AgentEvent::RunComplete { total_turns } => WireEvent::Done { total_turns },
        AgentEvent::StructuredOutput(result) => WireEvent::StructuredOutput { result },
        AgentEvent::Warning(msg) => WireEvent::Warning { message: msg },
    }
}

/// Convert a persisted [`SessionEvent`] (whole-message granularity,
/// from `SessionStore::read`) into a wire event for replay. Returns
/// `None` for event kinds that aren't user-visible (HookRan,
/// StateSnapshot, TurnBoundary — internal bookkeeping).
pub fn from_session_event(event: SessionEvent) -> Option<WireEvent> {
    match event {
        SessionEvent::RunStart { run_id, at } => Some(WireEvent::RunStart { run_id, at }),
        SessionEvent::RunEnd { run_id, outcome, at } => Some(WireEvent::RunEnd {
            run_id,
            total_turns: outcome.total_turns,
            stop_reason: outcome.stop_reason.map(|s| s.to_string()),
            at,
        }),
        SessionEvent::Message { run_id, msg, at } => {
            Some(WireEvent::Message { run_id, msg, at })
        }
        SessionEvent::TurnBoundary { .. }
        | SessionEvent::HookRan { .. }
        | SessionEvent::StateSnapshot { .. } => None,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut head: String = s.chars().take(max).collect();
        head.push('…');
        head
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_kind_strings_match_snake_case_tags() {
        // Sanity-check the discriminator and the serde rename agree —
        // clients filtering on the SSE `event:` field need them to match
        // the `type` they see in the JSON body.
        let event = WireEvent::AssistantTextDelta { text: "hi".into() };
        assert_eq!(event.event_kind(), "assistant_text_delta");
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "assistant_text_delta");
    }

    #[test]
    fn agent_event_text_delta_round_trips() {
        let wire = from_agent_event(AgentEvent::AssistantTextDelta("hello".into()));
        let WireEvent::AssistantTextDelta { text } = wire else {
            panic!()
        };
        assert_eq!(text, "hello");
    }

    #[test]
    fn agent_event_run_complete_becomes_done() {
        let wire = from_agent_event(AgentEvent::RunComplete { total_turns: 3 });
        let WireEvent::Done { total_turns } = wire else {
            panic!()
        };
        assert_eq!(total_turns, 3);
    }

    #[test]
    fn session_event_message_passes_through() {
        let msg = Message {
            role: runic_message_types::Role::User,
            content: vec![runic_message_types::ContentBlock::Text {
                text: "hi".into(),
                cache_control: None,
            }],
            timestamp: Some(Utc::now()),
            tool_duration_ms: None,
        };
        let evt = SessionEvent::Message {
            run_id: "r1".into(),
            msg: msg.clone(),
            at: Utc::now(),
        };
        let Some(WireEvent::Message { run_id, .. }) = from_session_event(evt) else {
            panic!()
        };
        assert_eq!(run_id, "r1");
    }

    #[test]
    fn hook_ran_is_filtered_out_of_replay() {
        let evt = SessionEvent::HookRan {
            run_id: "r1".into(),
            hook: "logging".into(),
            lifecycle: runic_agent_core::HookLifecycle::BeforeAgent,
            note: None,
            at: Utc::now(),
        };
        assert!(from_session_event(evt).is_none());
    }
}
