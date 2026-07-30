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

impl DelegationMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            DelegationMode::Sync => "sync",
            DelegationMode::Parallel => "parallel",
            DelegationMode::Background => "background",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DelegationStatus {
    Ok,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PersistenceStatus {
    Flushed,
    FlushFailed(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditStamp {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    TextDelta(String),
    ThinkingDelta(String),
    RunStarted {
        run_id: String,
        agent: Option<String>,
        audit: Option<AuditStamp>,
        at: DateTime<Utc>,
    },
    Message {
        run_id: String,
        msg: Message,
        at: DateTime<Utc>,
    },
    ToolStarted {
        run_id: String,
        turn: u32,
        call_id: String,
        tool: String,
        input: serde_json::Value,
        at: DateTime<Utc>,
    },
    ToolFinished {
        run_id: String,
        turn: u32,
        call_id: String,
        tool: String,
        status: ToolStatus,
        result: serde_json::Value,
        provenance: Vec<runic_types::ProvenanceSource>,
        duration_ms: u64,
        at: DateTime<Utc>,
    },
    TurnEnd {
        run_id: String,
        turn: u32,
        model: String,
        usage: TokenUsage,
        model_ms: u64,
        stop_reason: String,
        at: DateTime<Utc>,
    },
    ToolDeferred {
        run_id: String,
        call_id: String,
        tool: String,
        payload: serde_json::Value,
        at: DateTime<Utc>,
    },
    HookFired {
        run_id: String,
        hook: String,
        hook_kind: String,
        lifecycle: HookLifecycle,
        outcome: String,
        note: Option<String>,
        at: DateTime<Utc>,
    },
    DelegationStarted {
        run_id: String,
        turn: u32,
        call_id: String,
        agent: String,
        mode: DelegationMode,
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
        model: Option<String>,
        duration_ms: u64,
        child_session: Option<String>,
        child_persistence: Option<PersistenceStatus>,
        at: DateTime<Utc>,
    },
    StateSnapshot {
        run_id: String,
        messages: Vec<Message>,
        system_prompt: String,
        reason: String,
        stats: Option<Box<crate::stats::ThreadStats>>,
        open_tasks: Option<Vec<crate::tasks::TaskRecord>>,
        data: Option<serde_json::Map<String, serde_json::Value>>,
        at: DateTime<Utc>,
    },
    StateUpdated {
        run_id: String,
        key: String,
        value: serde_json::Value,
        at: DateTime<Utc>,
    },
    TaskSpawned {
        run_id: String,
        task_id: String,
        agent: String,
        prompt: String,
        child_session: Option<String>,
        at: DateTime<Utc>,
    },
    TaskFinished {
        run_id: String,
        task_id: String,
        status: crate::tasks::TaskStatus,
        result: Option<String>,
        at: DateTime<Utc>,
    },
    RunEnd {
        run_id: String,
        status: RunEndStatus,
        outcome: RunOutcome,
        at: DateTime<Utc>,
    },
    Persisted {
        run_id: String,
        status: PersistenceStatus,
        at: DateTime<Utc>,
    },
}

impl AgentEvent {
    pub fn run_id(&self) -> Option<&str> {
        match self {
            AgentEvent::TextDelta(_) | AgentEvent::ThinkingDelta(_) => None,
            AgentEvent::RunStarted { run_id, .. }
            | AgentEvent::Message { run_id, .. }
            | AgentEvent::ToolStarted { run_id, .. }
            | AgentEvent::ToolFinished { run_id, .. }
            | AgentEvent::TurnEnd { run_id, .. }
            | AgentEvent::ToolDeferred { run_id, .. }
            | AgentEvent::HookFired { run_id, .. }
            | AgentEvent::DelegationStarted { run_id, .. }
            | AgentEvent::DelegationFinished { run_id, .. }
            | AgentEvent::StateSnapshot { run_id, .. }
            | AgentEvent::StateUpdated { run_id, .. }
            | AgentEvent::TaskSpawned { run_id, .. }
            | AgentEvent::TaskFinished { run_id, .. }
            | AgentEvent::RunEnd { run_id, .. }
            | AgentEvent::Persisted { run_id, .. } => Some(run_id),
        }
    }
}
