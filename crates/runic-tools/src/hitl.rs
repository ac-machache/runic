//! `ask_user` + `escalate_to_human` — human-in-the-loop tools.
//!
//! Both reach the human through the per-run [`HumanInterface`] the surface
//! wires onto the [`ToolContext`]. This mirrors openfang's approval design (a
//! request→reply channel that *blocks the agent task until a human answers*),
//! but generalized: `ask_user` returns the human's free-text reply into the
//! conversation, and `escalate_to_human` is a fire-and-forget hand-off. If no
//! human channel is wired for the run, the tools fail in-band so the model can
//! adapt rather than hang.

use async_trait::async_trait;

use runic_tool::{Tool, ToolContext, ToolResult};

/// Ask the human operator a question mid-run and wait for their reply.
pub struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }
    fn description(&self) -> &str {
        "Ask the human a question and wait for their answer. Use when you need \
         a decision, clarification, or information only the user can provide. \
         The reply comes back as the tool result."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "The question to put to the user." },
                "context": { "type": "string", "description": "Optional background to help them answer." }
            },
            "required": ["question"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        let Some(question) = args.get("question").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("ask_user requires `question`"));
        };
        let Some(human) = ctx.human() else {
            return Ok(ToolResult::error(
                "no human channel is available for this run; cannot ask the user",
            ));
        };
        let context = args.get("context").and_then(|v| v.as_str());
        match human.ask(question, context).await {
            Ok(answer) => Ok(ToolResult::ok(answer)),
            Err(e) => Ok(ToolResult::error(format!("ask_user failed: {e}"))),
        }
    }
}

/// Hand off to a human operator — stop and flag that this needs a person.
pub struct EscalateToHumanTool;

#[async_trait]
impl Tool for EscalateToHumanTool {
    fn name(&self) -> &str {
        "escalate_to_human"
    }
    fn description(&self) -> &str {
        "Escalate to a human operator when the task is beyond your authority or \
         you are stuck. Notifies a person with your reason; does not wait for a \
         reply."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "reason": { "type": "string", "description": "Why this needs a human." },
                "detail": { "type": "string", "description": "Optional supporting detail." }
            },
            "required": ["reason"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        let Some(reason) = args.get("reason").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("escalate_to_human requires `reason`"));
        };
        let Some(human) = ctx.human() else {
            return Ok(ToolResult::error(
                "no human channel is available for this run; cannot escalate",
            ));
        };
        let detail = args.get("detail").and_then(|v| v.as_str());
        match human.escalate(reason, detail).await {
            Ok(()) => Ok(ToolResult::ok("Escalated to a human operator.")),
            Err(e) => Ok(ToolResult::error(format!("escalate_to_human failed: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use runic_tool::HumanInterface;

    struct StubHuman;

    #[async_trait]
    impl HumanInterface for StubHuman {
        async fn ask(&self, question: &str, _context: Option<&str>) -> anyhow::Result<String> {
            Ok(format!("answer to: {question}"))
        }
        async fn escalate(&self, _reason: &str, _detail: Option<&str>) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn ask_user_returns_human_reply() {
        let ctx = ToolContext::new("u", "s", "r").with_human(Some(Arc::new(StubHuman)));
        let r = AskUserTool
            .execute(serde_json::json!({ "question": "proceed?" }), &ctx)
            .await
            .unwrap();
        assert!(r.success);
        assert_eq!(r.output, "answer to: proceed?");
    }

    #[tokio::test]
    async fn tools_fail_in_band_without_a_human_channel() {
        let ctx = ToolContext::new("u", "s", "r");
        let ask = AskUserTool
            .execute(serde_json::json!({ "question": "x" }), &ctx)
            .await
            .unwrap();
        assert!(!ask.success);
        let esc = EscalateToHumanTool
            .execute(serde_json::json!({ "reason": "stuck" }), &ctx)
            .await
            .unwrap();
        assert!(!esc.success);
    }

    #[tokio::test]
    async fn escalate_notifies() {
        let ctx = ToolContext::new("u", "s", "r").with_human(Some(Arc::new(StubHuman)));
        let r = EscalateToHumanTool
            .execute(serde_json::json!({ "reason": "need approval" }), &ctx)
            .await
            .unwrap();
        assert!(r.success);
    }
}
