//! `ask_user` + `escalate_to_human` — human-in-the-loop tools. Both defer:
//! the run suspends durably and resumes when a human delivers the answer.

use async_trait::async_trait;

use runic_tool::{Tool, ToolContext, ToolResult};

pub struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }
    fn description(&self) -> &str {
        "Ask the human a question and wait for their answer. Use when you need \
         a decision, clarification, or information only the user can provide. \
         The run pauses until the answer arrives, then continues with it as the \
         tool result."
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
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let Some(question) = args.get("question").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("ask_user requires `question`"));
        };
        let mut payload = serde_json::json!({ "question": question });
        if let Some(context) = args.get("context").and_then(|v| v.as_str()) {
            payload["context"] = context.into();
        }
        Ok(ToolResult::defer("human_ask", payload))
    }
}

pub struct EscalateToHumanTool;

#[async_trait]
impl Tool for EscalateToHumanTool {
    fn name(&self) -> &str {
        "escalate_to_human"
    }
    fn description(&self) -> &str {
        "Escalate to a human operator when the task is beyond your authority or \
         you are stuck. The run pauses until a human takes the hand-off; their \
         response comes back as the tool result."
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
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let Some(reason) = args.get("reason").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("escalate_to_human requires `reason`"));
        };
        let mut payload = serde_json::json!({ "reason": reason });
        if let Some(detail) = args.get("detail").and_then(|v| v.as_str()) {
            payload["detail"] = detail.into();
        }
        Ok(ToolResult::defer("human_escalation", payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn ask_user_defers_with_the_question() {
        let ctx = ToolContext::new("u", "s", "r");
        let r = AskUserTool
            .execute(json!({ "question": "proceed?", "context": "bg" }), &ctx)
            .await
            .unwrap();
        let runic_tool::ToolResult::Deferred { channel, payload } = r else {
            panic!("ask_user defers, got {r:?}");
        };
        assert_eq!(channel, "human_ask");
        assert_eq!(payload["question"], "proceed?");
        assert_eq!(payload["context"], "bg");
    }

    #[tokio::test]
    async fn ask_user_without_question_errors_in_band() {
        let ctx = ToolContext::new("u", "s", "r");
        let r = AskUserTool.execute(json!({}), &ctx).await.unwrap();
        assert!(r.is_error());
        assert!(!matches!(r, runic_tool::ToolResult::Deferred { .. }));
    }

    #[tokio::test]
    async fn escalate_defers_with_reason() {
        let ctx = ToolContext::new("u", "s", "r");
        let r = EscalateToHumanTool
            .execute(json!({ "reason": "need approval", "detail": "d" }), &ctx)
            .await
            .unwrap();
        let runic_tool::ToolResult::Deferred { channel, payload } = r else {
            panic!("escalate defers, got {r:?}");
        };
        assert_eq!(channel, "human_escalation");
        assert_eq!(payload["reason"], "need approval");
        assert_eq!(payload["detail"], "d");
    }

    #[tokio::test]
    async fn escalate_without_reason_errors_in_band() {
        let ctx = ToolContext::new("u", "s", "r");
        let r = EscalateToHumanTool.execute(json!({}), &ctx).await.unwrap();
        assert!(r.is_error());
        assert!(!matches!(r, runic_tool::ToolResult::Deferred { .. }));
    }
}
