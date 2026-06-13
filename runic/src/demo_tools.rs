//! Demonstration tools that ship with the reference binary: a connectivity
//! echo, a human-in-the-loop email stub, and a background counter. They
//! exercise each tool kind (plain / HITL / background) end to end. Kept in
//! one module so the harness can register them and they don't clutter the
//! entrypoint.

use async_trait::async_trait;
use runic_agent_core::{
    ApprovalRequest, Approver, BackgroundTool, Draft, HitlTool, Tool, ToolContext, ToolResult,
    UserDecision,
};
use uuid::Uuid;

/// Typed wrapper so the runtime bag can be keyed by exactly this type
/// (rather than colliding with any other `Uuid` someone might stash).
#[derive(Debug, Clone)]
pub struct SessionUuid(pub Uuid);

/// Trivial echo tool — proves the full tool-dispatch round-trip works
/// without external side effects, and surfaces the runtime-context plumbing.
pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echo the given message back to the assistant. Use this to verify tool dispatch."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": { "type": "string", "description": "The text to echo back" }
            },
            "required": ["message"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let Some(msg) = input.get("message").and_then(|v| v.as_str()) else {
            return ToolResult::error("missing required field 'message'");
        };
        match ctx.get::<SessionUuid>() {
            Some(uuid) => ToolResult::ok(format!("echo: {msg} | session_uuid={}", uuid.0)),
            None => ToolResult::error("SessionUuid not found in runtime context"),
        }
    }
}

/// Human-in-the-loop email stub. Drafts an email, asks for approval, and
/// (for the demo) echoes what would have been sent rather than sending.
pub struct EmailTool;

#[async_trait]
impl HitlTool for EmailTool {
    fn name(&self) -> &str {
        "send_email"
    }
    fn description(&self) -> &str {
        "Send an email. The user is asked to fill in the recipient and may edit the subject/body before sending."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "recipient": { "type": "string", "description": "Email address (often left blank — the user will provide it)" },
                "subject":   { "type": "string" },
                "body":      { "type": "string" }
            },
            "required": ["subject", "body"]
        })
    }

    fn draft(&self, input: &serde_json::Value) -> Draft {
        let recipient = input.get("recipient").and_then(|v| v.as_str()).unwrap_or("");
        let subject = input.get("subject").and_then(|v| v.as_str()).unwrap_or("(no subject)");
        let summary = format!(
            "Send email\n  to:      {}\n  subject: {}",
            if recipient.is_empty() { "(blank — please fill)" } else { recipient },
            subject
        );
        Draft {
            summary,
            current_input: input.clone(),
            input_schema: self.input_schema(),
            editable_fields: vec!["recipient".into(), "subject".into(), "body".into()],
        }
    }

    async fn execute(&self, final_input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let recipient = final_input.get("recipient").and_then(|v| v.as_str()).unwrap_or("?");
        let subject = final_input.get("subject").and_then(|v| v.as_str()).unwrap_or("?");
        let body = final_input.get("body").and_then(|v| v.as_str()).unwrap_or("");
        ToolResult::ok(format!(
            "(simulated) email sent\n  to: {recipient}\n  subject: {subject}\n  body: {body}"
        ))
    }
}

/// Demo `BackgroundTool` — sleeps `seconds` then reports back. Returns a
/// task id immediately; poll via the auto-registered `background_status`.
pub struct SlowCountTool;

#[async_trait]
impl BackgroundTool for SlowCountTool {
    fn name(&self) -> &str {
        "slow_count"
    }
    fn description(&self) -> &str {
        "Start a background counter that finishes after `seconds` seconds. Returns a task_id immediately; use `background_status` with that id to check on it later."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "seconds": { "type": "integer", "minimum": 1, "maximum": 120, "description": "How many seconds to count" }
            },
            "required": ["seconds"],
            "additionalProperties": false
        })
    }

    async fn run(&self, input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let seconds = input.get("seconds").and_then(|v| v.as_u64()).unwrap_or(1).min(120);
        tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
        ToolResult::ok(format!("counted to {seconds}"))
    }
}

/// Stdin-driven HITL approver for the REPL. Prompts the operator to edit
/// fields and confirm before a HITL tool executes.
pub struct StdinApprover;

#[async_trait]
impl Approver for StdinApprover {
    async fn review(&self, req: ApprovalRequest) -> UserDecision {
        use std::io::Write as _;
        use tokio::io::{AsyncBufReadExt, BufReader};

        let mut reader = BufReader::new(tokio::io::stdin()).lines();

        eprintln!("\n--- HITL APPROVAL ---");
        eprintln!("Tool: {}", req.tool_name);
        eprintln!("{}", req.draft.summary);
        eprintln!("---");

        let mut current = req.draft.current_input.clone();

        for field in &req.draft.editable_fields {
            let cur_str = current.get(field).and_then(|v| v.as_str()).unwrap_or("");
            let display = if cur_str.is_empty() { "blank" } else { cur_str };
            eprint!("  {field} [{display}] (enter to keep): ");
            std::io::stderr().flush().ok();

            match reader.next_line().await {
                Ok(Some(line)) => {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        current[field] = serde_json::Value::String(trimmed.to_string());
                    }
                }
                _ => return UserDecision::Cancel { reason: "stdin closed".into() },
            }
        }

        eprint!("Send? [y/N]: ");
        std::io::stderr().flush().ok();
        match reader.next_line().await {
            Ok(Some(line)) if matches!(line.trim().to_lowercase().as_str(), "y" | "yes") => {
                UserDecision::Submit { final_input: current }
            }
            _ => UserDecision::Cancel { reason: "user declined".into() },
        }
    }
}
