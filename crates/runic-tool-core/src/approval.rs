use crate::tool::ToolContext;
use crate::tool::ToolResult;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub type ApproverHandle = Arc<dyn Approver + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Draft {
    pub summary: String,
    pub current_input: serde_json::Value,
    pub input_schema: serde_json::Value,
    pub editable_fields: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApprovalRequest {
    pub tool_name: String,
    pub call_id: String,
    pub run_id: String,
    /// The session/thread this approval belongs to. Lets a remote approver
    /// (e.g. the HTTP server) route the request to the right client stream.
    pub session_id: String,
    pub draft: Draft,
}

/// The operator's verdict on an [`ApprovalRequest`]. Serde-tagged so it can
/// be carried in an HTTP request body: `{"decision":"submit","final_input":…}`
/// or `{"decision":"cancel","reason":"…"}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum UserDecision {
    Submit { final_input: serde_json::Value },
    Cancel { reason: String },
}

#[async_trait]
pub trait Approver: Send + Sync {
    async fn review(&self, reauest: ApprovalRequest) -> UserDecision;
}

#[async_trait]
pub trait HitlTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> serde_json::Value;
    fn draft(&self, input: &serde_json::Value) -> Draft;
    async fn execute(&self, final_output: serde_json::Value, ctx: &ToolContext) -> ToolResult;
}
