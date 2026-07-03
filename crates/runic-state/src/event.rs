//! `SessionEvent` — the unit of the event-sourced log.

use chrono::{DateTime, Utc};
use runic_types::{Message, TokenUsage};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookLifecycle {
    BeforeAgent,
    AfterAgent,
    BeforeModel,
    AfterModel,
    BeforeTool,
    AfterTool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunOutcome {
    pub total_turns: u32,

    pub stop_reason: Option<String>,

    pub usage: TokenUsage,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum SessionEvent {
    RunStart {
        run_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        at: DateTime<Utc>,
    },

    RunEnd {
        run_id: String,
        outcome: RunOutcome,
        at: DateTime<Utc>,
    },

    Message {
        run_id: String,
        msg: Message,
        at: DateTime<Utc>,
    },

    TurnBoundary {
        run_id: String,
        at: DateTime<Utc>,
    },

    HookRan {
        run_id: String,
        hook: String,
        lifecycle: HookLifecycle,
        #[serde(default)]
        hook_kind: String,
        #[serde(default)]
        outcome: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        at: DateTime<Utc>,
    },

    StateSnapshot {
        run_id: String,
        messages: Vec<Message>,
        system_prompt: String,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stats: Option<crate::stats::ThreadStats>,
        at: DateTime<Utc>,
    },
}

impl SessionEvent {
    pub fn run_id(&self) -> &str {
        match self {
            SessionEvent::RunStart { run_id, .. }
            | SessionEvent::RunEnd { run_id, .. }
            | SessionEvent::Message { run_id, .. }
            | SessionEvent::TurnBoundary { run_id, .. }
            | SessionEvent::HookRan { run_id, .. }
            | SessionEvent::StateSnapshot { run_id, .. } => run_id,
        }
    }
}
