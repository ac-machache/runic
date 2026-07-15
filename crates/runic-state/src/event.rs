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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunEndStatus {
    Completed,
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolStatus {
    Ok,
    ToolError,
    ExecError,
    Panic,
    Timeout,
    Cancelled,
    UnknownTool,
    GuardBlocked,
    Substituted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DelegationMode {
    Sync,
    Parallel,
    Background,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DelegationStatus {
    Ok,
    Failed(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditStamp {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum SessionEvent {
    RunStart {
        run_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audit: Option<AuditStamp>,
        at: DateTime<Utc>,
    },

    RunEnd {
        run_id: String,
        status: RunEndStatus,
        outcome: RunOutcome,
        at: DateTime<Utc>,
    },

    Message {
        run_id: String,
        msg: Message,
        at: DateTime<Utc>,
    },

    TurnEnd {
        run_id: String,
        turn: u32,
        model: String,
        usage: TokenUsage,
        model_ms: u64,
        at: DateTime<Utc>,
    },

    ToolStarted {
        run_id: String,
        turn: u32,
        call_id: String,
        tool: String,
        at: DateTime<Utc>,
    },

    ToolFinished {
        run_id: String,
        turn: u32,
        call_id: String,
        tool: String,
        status: ToolStatus,
        duration_ms: u64,
        at: DateTime<Utc>,
    },

    DelegationStarted {
        run_id: String,
        turn: u32,
        call_id: String,
        agent: String,
        mode: DelegationMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child_session: Option<String>,
        at: DateTime<Utc>,
    },

    DelegationFinished {
        run_id: String,
        turn: u32,
        call_id: String,
        agent: String,
        status: DelegationStatus,
        usage: TokenUsage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        duration_ms: u64,
        at: DateTime<Utc>,
    },

    #[serde(alias = "HookRan")]
    HookFired {
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
        stats: Option<Box<crate::stats::ThreadStats>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        open_tasks: Option<Vec<crate::tasks::TaskRecord>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Map<String, serde_json::Value>>,
        at: DateTime<Utc>,
    },

    TaskSpawned {
        run_id: String,
        task_id: String,
        agent: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child_session: Option<String>,
        at: DateTime<Utc>,
    },

    TaskFinished {
        run_id: String,
        task_id: String,
        status: crate::tasks::TaskStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        at: DateTime<Utc>,
    },

    StateUpdated {
        run_id: String,
        key: String,
        value: serde_json::Value,
        at: DateTime<Utc>,
    },

    ToolDeferred {
        run_id: String,
        call_id: String,
        channel: String,
        payload: serde_json::Value,
        at: DateTime<Utc>,
    },
}

impl SessionEvent {
    pub fn run_id(&self) -> &str {
        match self {
            SessionEvent::RunStart { run_id, .. }
            | SessionEvent::RunEnd { run_id, .. }
            | SessionEvent::Message { run_id, .. }
            | SessionEvent::TurnEnd { run_id, .. }
            | SessionEvent::ToolStarted { run_id, .. }
            | SessionEvent::ToolFinished { run_id, .. }
            | SessionEvent::DelegationStarted { run_id, .. }
            | SessionEvent::DelegationFinished { run_id, .. }
            | SessionEvent::HookFired { run_id, .. }
            | SessionEvent::StateSnapshot { run_id, .. }
            | SessionEvent::TaskSpawned { run_id, .. }
            | SessionEvent::TaskFinished { run_id, .. }
            | SessionEvent::StateUpdated { run_id, .. }
            | SessionEvent::ToolDeferred { run_id, .. } => run_id,
        }
    }
}
