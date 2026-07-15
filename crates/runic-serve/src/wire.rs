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
use runic_state::{HookLifecycle, SessionEvent};
use runic_types::Message;
use serde::Serialize;
use utoipa::ToSchema;

/// One Server-Sent Event on a run stream. The `type` field is the discriminator
/// and also the SSE `event:` name.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireEvent {
    /// A run is starting. `at` is present only on replay (the persisted
    /// `RunStart` carries a timestamp; the live event does not).
    RunStart {
        run_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
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
        #[schema(value_type = Object)]
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

    /// One model turn just finished. Live runs carry `stop_reason`; replayed
    /// turns carry the durable usage/model/latency instead.
    TurnComplete {
        turn: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        input_tokens: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        output_tokens: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model_ms: Option<u64>,
    },

    /// A delegation edge opened (the parent handed work to a subagent).
    DelegationStart {
        run_id: String,
        call_id: String,
        agent: String,
        mode: String,
    },

    /// A delegation edge closed, carrying the child's cost.
    DelegationFinish {
        run_id: String,
        call_id: String,
        agent: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        duration_ms: u64,
        input_tokens: u64,
        output_tokens: u64,
    },

    /// A complete message landed in agent state. Replay only (live runs
    /// surface the same content as deltas + the persisted log).
    Message {
        run_id: String,
        #[schema(value_type = Object)]
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

    TaskSpawned {
        run_id: String,
        task_id: String,
        agent: String,
    },

    TaskFinished {
        run_id: String,
        task_id: String,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },

    /// A durable key was written via `state.update`.
    StateUpdated { run_id: String, key: String },

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

    ToolDeferred {
        run_id: String,
        call_id: String,
        channel: String,
        payload: serde_json::Value,
    },

    /// Non-fatal server-side warning (e.g. a run task that failed to join).
    Warning { message: String },

    /// The run failed server-side (provider error, max turns, …). Terminal — a
    /// `Done` follows so EventSource clients close cleanly.
    RunError {
        #[serde(skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        message: String,
    },

    /// Sent once after a stream finishes (live or replay). Clients use this to
    /// close their EventSource cleanly. `total_turns` is present only when the
    /// run actually completed (a real `RunEnd`); never invented.
    Done {
        #[serde(skip_serializing_if = "Option::is_none")]
        total_turns: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
    },

    /// A hook did something other than `continue` — live and replay.
    HookFired {
        hook_name: String,
        hook_kind: String,
        lifecycle: String,
        outcome: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
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
            Self::DelegationStart { .. } => "delegation_start",
            Self::DelegationFinish { .. } => "delegation_finish",
            Self::Message { .. } => "message",
            Self::RunEnd { .. } => "run_end",
            Self::Usage { .. } => "usage",
            Self::AskRequired { .. } => "ask_required",
            Self::Escalated { .. } => "escalated",
            Self::ToolDeferred { .. } => "tool_deferred",
            Self::Warning { .. } => "warning",
            Self::RunError { .. } => "run_error",
            Self::Done { .. } => "done",
            Self::HookFired { .. } => "hook_fired",
            Self::TaskSpawned { .. } => "task_spawned",
            Self::TaskFinished { .. } => "task_finished",
            Self::StateUpdated { .. } => "state_updated",
        }
    }
}

fn lifecycle_str(lifecycle: HookLifecycle) -> &'static str {
    match lifecycle {
        HookLifecycle::BeforeAgent => "before_agent",
        HookLifecycle::AfterAgent => "after_agent",
        HookLifecycle::BeforeModel => "before_model",
        HookLifecycle::AfterModel => "after_model",
        HookLifecycle::BeforeTool => "before_tool",
        HookLifecycle::AfterTool => "after_tool",
    }
}

/// Convert a live [`AgentEvent`] (token-level, from `RunContext::with_events`)
/// into wire events. One agent event can fan into several wire events — a
/// completed run yields both `usage` and `done`.
pub fn from_agent_event(event: AgentEvent) -> Vec<WireEvent> {
    match event {
        AgentEvent::RunStarted { run_id } => vec![WireEvent::RunStart {
            run_id,
            agent: None,
            at: None,
        }],
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
            vec![WireEvent::TurnComplete {
                turn,
                stop_reason: Some(stop_reason),
                model: None,
                input_tokens: None,
                output_tokens: None,
                model_ms: None,
            }]
        }
        AgentEvent::ToolDeferred {
            run_id,
            call_id,
            channel,
            payload,
        } => vec![
            WireEvent::ToolDeferred {
                run_id,
                call_id,
                channel,
                payload,
            },
            WireEvent::Done {
                total_turns: None,
                stop_reason: Some("suspended".to_string()),
            },
        ],
        AgentEvent::RunCompleted(outcome) => vec![
            WireEvent::Usage {
                input_tokens: outcome.usage.input_tokens,
                output_tokens: outcome.usage.output_tokens,
            },
            WireEvent::Done {
                total_turns: Some(outcome.total_turns),
                stop_reason: outcome.stop_reason,
            },
        ],
        AgentEvent::HookFired {
            hook_name,
            hook_kind,
            lifecycle,
            outcome,
            note,
        } => vec![WireEvent::HookFired {
            hook_name,
            hook_kind: hook_kind.to_string(),
            lifecycle: lifecycle_str(lifecycle).to_string(),
            outcome: outcome.to_string(),
            note,
        }],
    }
}

/// Convert a persisted [`SessionEvent`] (whole-message granularity, from
/// `SessionStore::read`) into a wire event for replay. Returns `None` for
/// internal bookkeeping events (`StateSnapshot` and, until 1.9 maps them,
/// the execution-fact events).
pub fn from_session_event(event: SessionEvent) -> Option<WireEvent> {
    match event {
        SessionEvent::RunStart {
            run_id, agent, at, ..
        } => Some(WireEvent::RunStart {
            run_id,
            agent,
            at: Some(at),
        }),
        SessionEvent::RunEnd {
            run_id,
            outcome,
            at,
            ..
        } => Some(WireEvent::RunEnd {
            run_id,
            total_turns: outcome.total_turns,
            stop_reason: outcome.stop_reason,
            at,
        }),
        SessionEvent::Message { run_id, msg, at } => Some(WireEvent::Message { run_id, msg, at }),
        SessionEvent::HookFired {
            hook,
            lifecycle,
            hook_kind,
            outcome,
            note,
            ..
        } => Some(WireEvent::HookFired {
            hook_name: hook,
            hook_kind,
            lifecycle: lifecycle_str(lifecycle).to_string(),
            outcome,
            note,
        }),
        SessionEvent::TaskSpawned {
            run_id,
            task_id,
            agent,
            ..
        } => Some(WireEvent::TaskSpawned {
            run_id,
            task_id,
            agent,
        }),
        SessionEvent::TaskFinished {
            run_id,
            task_id,
            status,
            result,
            ..
        } => Some(WireEvent::TaskFinished {
            run_id,
            task_id,
            status: match status {
                runic_state::TaskStatus::Running => "running".to_string(),
                runic_state::TaskStatus::Completed => "completed".to_string(),
                runic_state::TaskStatus::Failed => "failed".to_string(),
                runic_state::TaskStatus::Cancelled => "cancelled".to_string(),
            },
            preview: result.map(|r| truncate(&r, 300)),
        }),
        SessionEvent::StateUpdated { run_id, key, .. } => {
            Some(WireEvent::StateUpdated { run_id, key })
        }
        SessionEvent::ToolDeferred {
            run_id,
            call_id,
            channel,
            payload,
            ..
        } => Some(WireEvent::ToolDeferred {
            run_id,
            call_id,
            channel,
            payload,
        }),
        SessionEvent::TurnEnd {
            turn,
            model,
            usage,
            model_ms,
            ..
        } => Some(WireEvent::TurnComplete {
            turn,
            stop_reason: None,
            model: Some(model),
            input_tokens: Some(usage.input_tokens),
            output_tokens: Some(usage.output_tokens),
            model_ms: Some(model_ms),
        }),
        SessionEvent::ToolFinished {
            call_id,
            tool,
            status,
            ..
        } => Some(WireEvent::ToolFinish {
            id: call_id,
            name: tool,
            is_error: !matches!(
                status,
                runic_state::ToolStatus::Ok | runic_state::ToolStatus::Substituted
            ),
            preview: String::new(),
        }),
        SessionEvent::DelegationStarted {
            run_id,
            call_id,
            agent,
            mode,
            ..
        } => Some(WireEvent::DelegationStart {
            run_id,
            call_id,
            agent,
            mode: match mode {
                runic_state::DelegationMode::Sync => "sync".to_string(),
                runic_state::DelegationMode::Parallel => "parallel".to_string(),
                runic_state::DelegationMode::Background => "background".to_string(),
            },
        }),
        SessionEvent::DelegationFinished {
            run_id,
            call_id,
            agent,
            status,
            usage,
            model,
            duration_ms,
            ..
        } => Some(WireEvent::DelegationFinish {
            run_id,
            call_id,
            agent,
            ok: matches!(status, runic_state::DelegationStatus::Ok),
            model,
            duration_ms,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
        }),
        SessionEvent::ToolStarted { .. } | SessionEvent::StateSnapshot { .. } => None,
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
                ..Default::default()
            },
            structured: None,
        };
        let wires = from_agent_event(AgentEvent::RunCompleted(outcome));
        assert!(matches!(
            wires[0],
            WireEvent::Usage {
                input_tokens: 10,
                output_tokens: 20
            }
        ));
        let WireEvent::Done {
            total_turns,
            stop_reason,
        } = &wires[1]
        else {
            panic!()
        };
        assert_eq!(*total_turns, Some(3));
        assert_eq!(stop_reason.as_deref(), Some("end_turn"));
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
    fn turn_end_replays_with_durable_usage() {
        let evt = SessionEvent::TurnEnd {
            run_id: "r1".into(),
            turn: 3,
            model: "m".into(),
            usage: runic_types::TokenUsage {
                input_tokens: 12,
                output_tokens: 5,
                ..Default::default()
            },
            model_ms: 40,
            at: Utc::now(),
        };
        let Some(WireEvent::TurnComplete {
            turn,
            stop_reason,
            model,
            input_tokens,
            model_ms,
            ..
        }) = from_session_event(evt)
        else {
            panic!("turn end must replay");
        };
        assert_eq!(turn, 3);
        assert!(stop_reason.is_none());
        assert_eq!(model.as_deref(), Some("m"));
        assert_eq!(input_tokens, Some(12));
        assert_eq!(model_ms, Some(40));
    }

    #[test]
    fn hook_fired_maps_one_to_one_live() {
        let wires = from_agent_event(AgentEvent::HookFired {
            hook_name: "guard".into(),
            hook_kind: "write",
            lifecycle: HookLifecycle::BeforeTool,
            outcome: "cancel",
            note: Some("blocked".into()),
        });
        assert_eq!(wires.len(), 1);
        let WireEvent::HookFired {
            hook_name,
            hook_kind,
            lifecycle,
            outcome,
            note,
        } = &wires[0]
        else {
            panic!()
        };
        assert_eq!(hook_name, "guard");
        assert_eq!(hook_kind, "write");
        assert_eq!(lifecycle, "before_tool");
        assert_eq!(outcome, "cancel");
        assert_eq!(note.as_deref(), Some("blocked"));
    }

    #[test]
    fn hook_ran_is_visible_on_replay() {
        let evt = SessionEvent::HookFired {
            run_id: "r1".into(),
            hook: "guard".into(),
            lifecycle: HookLifecycle::AfterTool,
            hook_kind: "write".into(),
            outcome: "substitute".into(),
            note: None,
            at: Utc::now(),
        };
        let Some(WireEvent::HookFired {
            hook_name,
            lifecycle,
            outcome,
            ..
        }) = from_session_event(evt)
        else {
            panic!()
        };
        assert_eq!(hook_name, "guard");
        assert_eq!(lifecycle, "after_tool");
        assert_eq!(outcome, "substitute");
    }

    #[test]
    fn tool_deferred_is_visible_on_replay() {
        let evt = SessionEvent::ToolDeferred {
            run_id: "r1".into(),
            call_id: "call-1".into(),
            channel: "human_ask".into(),
            payload: serde_json::json!({ "question": "continue?" }),
            at: Utc::now(),
        };
        let Some(WireEvent::ToolDeferred {
            run_id,
            call_id,
            channel,
            payload,
        }) = from_session_event(evt)
        else {
            panic!()
        };
        assert_eq!(run_id, "r1");
        assert_eq!(call_id, "call-1");
        assert_eq!(channel, "human_ask");
        assert_eq!(payload["question"], "continue?");
    }
}
