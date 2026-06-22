//! `SessionEvent` — the unit of the event-sourced log.
//!
//! The conversation is modeled as an append-only sequence of events (runic's
//! design — the strictly-better state model vs a mutable `Vec<Message>`):
//! it's replayable, auditable, groups naturally into runs, and compaction is
//! non-destructive (a `StateSnapshot` event, not an in-place trim). The
//! provider-facing `Vec<Message>` is *derived* from the log on demand.

use chrono::{DateTime, Utc};
use runic_types::{Message, TokenUsage};
use serde::{Deserialize, Serialize};

/// Where in the agent lifecycle a hook fired (recorded for the audit trail).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookLifecycle {
    BeforeAgent,
    AfterAgent,
    BeforeModel,
    AfterModel,
    BeforeTool,
    AfterTool,
}

/// Summary of a finished run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunOutcome {
    /// Number of model turns the run took.
    pub total_turns: u32,
    /// Why the run ended (provider stop reason, or `None`).
    pub stop_reason: Option<String>,
    /// Token usage accumulated across the run.
    pub usage: TokenUsage,
    /// Set when the run answered via the `final_answer` tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<serde_json::Value>,
}

/// One entry in the agent's event log. Tagged by `kind` on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum SessionEvent {
    /// A run (one user request → its answer) began.
    RunStart { run_id: String, at: DateTime<Utc> },
    /// A run finished.
    RunEnd {
        run_id: String,
        outcome: RunOutcome,
        at: DateTime<Utc>,
    },
    /// A complete message landed in state (user, assistant, or tool result).
    Message {
        run_id: String,
        msg: Message,
        at: DateTime<Utc>,
    },
    /// One model turn finished within a run.
    TurnBoundary { run_id: String, at: DateTime<Utc> },
    /// A hook fired — recorded for the audit trail.
    HookRan {
        run_id: String,
        hook: String,
        lifecycle: HookLifecycle,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        at: DateTime<Utc>,
    },
    /// Non-destructive compaction: replaces the accumulated history with a
    /// curated set (a summary + kept tail). Replay applies this in order, so
    /// no message is ever mutated or lost in place.
    StateSnapshot {
        run_id: String,
        messages: Vec<Message>,
        system_prompt: String,
        reason: String,
        at: DateTime<Utc>,
    },
}

impl SessionEvent {
    /// The `run_id` this event belongs to.
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
