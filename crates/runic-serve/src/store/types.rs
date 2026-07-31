use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Idle,
    Running,
    Waiting,
    Successful,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Idle => "idle",
            RunStatus::Running => "running",
            RunStatus::Waiting => "waiting",
            RunStatus::Successful => "successful",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(text: &str) -> Option<RunStatus> {
        match text {
            "idle" => Some(RunStatus::Idle),
            "running" => Some(RunStatus::Running),
            "waiting" => Some(RunStatus::Waiting),
            "successful" => Some(RunStatus::Successful),
            "failed" => Some(RunStatus::Failed),
            "cancelled" => Some(RunStatus::Cancelled),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RunStatus::Successful | RunStatus::Failed | RunStatus::Cancelled
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub tenant: String,
    pub session_id: String,
    pub agent: String,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub to_cancel: bool,
    pub attempt: i32,
    pub max_attempts: i32,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
pub struct RunSignals {
    pub to_cancel: bool,
    pub steering: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RunSpec {
    pub tenant: String,
    pub session_id: String,
    pub run_id: String,
    pub agent: String,
    pub input: Option<serde_json::Value>,
    pub context: Option<serde_json::Value>,
    pub execute_at: Option<DateTime<Utc>>,
}

impl RunSpec {
    pub fn new(
        tenant: impl Into<String>,
        session_id: impl Into<String>,
        run_id: impl Into<String>,
        agent: impl Into<String>,
    ) -> Self {
        Self {
            tenant: tenant.into(),
            session_id: session_id.into(),
            run_id: run_id.into(),
            agent: agent.into(),
            input: None,
            context: None,
            execute_at: None,
        }
    }

    pub fn input(mut self, input: serde_json::Value) -> Self {
        self.input = Some(input);
        self
    }

    pub fn context(mut self, context: Option<serde_json::Value>) -> Self {
        self.context = context;
        self
    }

    pub fn execute_at(mut self, at: DateTime<Utc>) -> Self {
        self.execute_at = Some(at);
        self
    }
}

#[derive(Debug, Clone)]
pub struct ClaimedRun {
    pub run_id: String,
    pub tenant: String,
    pub session_id: String,
    pub agent: String,
    pub input: Option<serde_json::Value>,
    pub context: Option<serde_json::Value>,
    pub attempt: i32,
    pub max_attempts: i32,
}

impl ClaimedRun {
    pub fn exhausted(&self) -> bool {
        self.attempt >= self.max_attempts
    }
}
