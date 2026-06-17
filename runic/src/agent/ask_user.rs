//! `ask_user` — a human-in-the-loop questionnaire tool.
//!
//! coral's `ask_user` pauses the run, the frontend renders a QCM, and the
//! resume carries the chosen answer back to the model. In runic that maps
//! cleanly onto the [`HitlTool`] plumbing: the "approval" *is* the user
//! answering. The tool drafts the question (+ options) for the host to
//! present; the host returns the answer as the approval's `final_input`;
//! [`HitlTool::execute`] hands that answer back to the model as the tool
//! result.
//!
//! Register with `registry.register_hitl(Arc::new(AskUserTool))`. An
//! `Approver` must be installed in the runtime context (REPL prompt, HTTP
//! SSE, …) or the call errors — same contract as any HITL tool.

use async_trait::async_trait;
use runic_tool_core::approval::{Draft, HitlTool};
use runic_tool_core::{ToolContext, ToolResult};
use serde_json::{json, Value};

pub struct AskUserTool;

#[async_trait]
impl HitlTool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Pose une question au TC et attend sa réponse avant de continuer. \
         Utilise quand une décision ou une clarification est requise et que \
         tu ne peux pas la déduire du contexte (choix ambigu, confirmation, \
         information manquante). Fournis `question` ; ajoute `options` pour un \
         choix multiple. La réponse du TC t'est renvoyée comme résultat."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "La question posée au TC."
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Choix proposés (optionnel). Vide = réponse libre."
                }
            },
            "required": ["question"],
            "additionalProperties": false
        })
    }

    fn draft(&self, input: &Value) -> Draft {
        let question = input
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let options: Vec<String> = input
            .get("options")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        // Summary is the human-facing prompt: the question, plus the options
        // as a numbered list when present.
        let mut summary = question.clone();
        if !options.is_empty() {
            summary.push('\n');
            for (i, opt) in options.iter().enumerate() {
                summary.push_str(&format!("\n{}. {opt}", i + 1));
            }
        }

        Draft {
            summary,
            // What the host fills in: the answer. Carries the original
            // question/options so a remote approver can render the QCM.
            current_input: json!({
                "question": question,
                "options": options,
                "answer": Value::Null,
            }),
            input_schema: json!({
                "type": "object",
                "properties": { "answer": { "type": "string" } },
                "required": ["answer"]
            }),
            editable_fields: vec!["answer".to_string()],
        }
    }

    async fn execute(&self, final_output: Value, _ctx: &ToolContext) -> ToolResult {
        match final_output.get("answer").and_then(Value::as_str) {
            Some(answer) if !answer.trim().is_empty() => {
                ToolResult::ok(format!("Réponse du TC : {answer}"))
            }
            _ => ToolResult::error("ask_user: aucune réponse fournie par le TC."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn ctx() -> ToolContext {
        ToolContext::new("s".into(), "r".into(), 0, HashMap::new())
    }

    #[test]
    fn draft_renders_options_as_numbered_list() {
        let tool = AskUserTool;
        let draft = tool.draft(&json!({
            "question": "Quelle ferme ?",
            "options": ["Dupont", "Moreau"],
        }));
        assert!(draft.summary.contains("Quelle ferme ?"));
        assert!(draft.summary.contains("1. Dupont"));
        assert!(draft.summary.contains("2. Moreau"));
        assert_eq!(draft.editable_fields, vec!["answer".to_string()]);
        assert_eq!(draft.current_input["answer"], Value::Null);
    }

    #[test]
    fn draft_without_options_is_just_the_question() {
        let tool = AskUserTool;
        let draft = tool.draft(&json!({ "question": "Confirmez-vous ?" }));
        assert_eq!(draft.summary, "Confirmez-vous ?");
        assert!(draft.current_input["options"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn execute_returns_the_answer() {
        let tool = AskUserTool;
        let r = tool.execute(json!({ "answer": "Dupont" }), &ctx()).await;
        assert!(!r.is_error);
        assert!(r.content.contains("Dupont"));
    }

    #[tokio::test]
    async fn execute_errors_on_empty_answer() {
        let tool = AskUserTool;
        let r = tool.execute(json!({ "answer": "  " }), &ctx()).await;
        assert!(r.is_error);
    }
}
