use runic_macros::tool;
use runic_tool::{ToolContext, ToolResult};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
pub struct Question {
    #[schemars(description = "The question to put to the user.")]
    question: String,
    #[schemars(
        description = "The choices to offer, e.g. [\"formal\", \"concise\", \"professional\"]."
    )]
    options: Vec<String>,
    #[serde(default)]
    #[schemars(description = "Let the user pick more than one option.")]
    multi_select: bool,
    #[serde(default)]
    #[schemars(description = "Accept an answer that is not one of the options.")]
    allow_other: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct QuestionnaireArgs {
    #[schemars(description = "One or more questions to put to the user in a single prompt.")]
    questions: Vec<Question>,
    #[serde(default)]
    #[schemars(
        with = "String",
        description = "Optional background to help them answer."
    )]
    context: Option<String>,
}

#[tool(
    name = "Questionnaire",
    args = QuestionnaireArgs,
    description = "Use this when you have more then when option for the user to choose from, \
                   instead of giving him a whole text you use this to ask the user about the \
                   options, only when the answer can be one of them of give free field"
)]
pub struct QuestionnaireTool;

impl QuestionnaireTool {
    async fn tool(
        &self,
        args: QuestionnaireArgs,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        if args.questions.is_empty() {
            return Ok(ToolResult::error(
                "Questionnaire needs at least one question; to ask for free text, answer directly instead",
            ));
        }
        if let Some(bare) = args.questions.iter().find(|q| q.options.is_empty()) {
            return Ok(ToolResult::error(format!(
                "question `{}` has no options; a Questionnaire always offers choices",
                bare.question
            )));
        }
        let mut payload = serde_json::json!({ "questions": args.questions });
        if let Some(context) = args.context {
            payload["context"] = context.into();
        }
        Ok(ToolResult::defer(payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runic_tool::Tool;
    use serde_json::json;

    #[tokio::test]
    async fn several_questions_each_carry_their_own_options() {
        let ctx = ToolContext::new("u", "s", "r");
        let r = QuestionnaireTool
            .execute(
                json!({
                    "questions": [
                        { "question": "Tone?", "options": ["formal", "casual"] },
                        {
                            "question": "Length?",
                            "options": ["concise", "detailed"],
                            "multi_select": true,
                            "allow_other": true
                        }
                    ],
                    "context": "drafting an email"
                }),
                &ctx,
            )
            .await
            .unwrap();

        let runic_tool::ToolResult::Deferred { payload } = r else {
            panic!("questionnaire defers, got {r:?}");
        };
        let questions = payload["questions"].as_array().unwrap();
        assert_eq!(questions.len(), 2);
        assert_eq!(questions[0]["question"], "Tone?");
        assert_eq!(questions[0]["options"][0], "formal");
        assert_eq!(questions[0]["multi_select"], false);
        assert_eq!(questions[1]["multi_select"], true);
        assert_eq!(questions[1]["allow_other"], true);
        assert_eq!(payload["context"], "drafting an email");
    }

    #[tokio::test]
    async fn a_question_without_options_errors_in_band() {
        let ctx = ToolContext::new("u", "s", "r");
        let r = QuestionnaireTool
            .execute(
                json!({ "questions": [{ "question": "anything?", "options": [] }] }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(r.is_error(), "a questionnaire always offers choices");
        assert!(!matches!(r, runic_tool::ToolResult::Deferred { .. }));
    }

    #[tokio::test]
    async fn no_questions_errors_in_band() {
        let ctx = ToolContext::new("u", "s", "r");
        let r = QuestionnaireTool
            .execute(json!({ "questions": [] }), &ctx)
            .await
            .unwrap();
        assert!(r.is_error());
        assert!(!matches!(r, runic_tool::ToolResult::Deferred { .. }));
    }

    #[test]
    fn the_nested_question_type_is_inlined_in_the_schema() {
        let schema = QuestionnaireTool.parameters_schema();
        assert!(
            schema.get("$defs").is_none(),
            "providers do not resolve $ref: {schema}"
        );
        assert_eq!(
            schema["properties"]["questions"]["items"]["properties"]["options"]["type"],
            "array"
        );
    }
}
