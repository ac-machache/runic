//! Structured output via a synthesized "finish" tool.
//!
//! A dev provides a JSON schema; runic registers a tool whose `input_schema`
//! IS that schema. The model does whatever work it needs, then calls the
//! tool to deliver its final answer. The tool validates the arguments
//! against the schema and stashes them in a shared slot; the agent loop
//! sees the slot fill and ends the run, returning the validated value as
//! `RunOutcome::structured_result`.
//!
//! Why a tool instead of a provider-native response format: it's portable
//! across every provider (all support tool calls), it lets the model do
//! arbitrary agentic work first, and a validation failure comes back as an
//! ordinary tool error the model already knows how to recover from.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic_tool_core::{Tool, ToolContext, ToolResult};
use serde_json::Value;

/// Shared cell the tool writes the validated output into and the loop reads.
pub type OutputSlot = Arc<Mutex<Option<Value>>>;

/// Bookkeeping the agent keeps when structured output is active.
#[derive(Clone)]
pub struct StructuredHandle {
    pub tool_name: String,
    pub slot: OutputSlot,
}

/// The synthesized finish tool.
pub struct StructuredOutputTool {
    name: String,
    description: String,
    schema: Value,
    validator: Option<jsonschema::Validator>,
    slot: OutputSlot,
}

impl StructuredOutputTool {
    pub fn new(name: impl Into<String>, schema: Value, slot: OutputSlot) -> Self {
        let name = name.into();
        // Compile once. If the schema is itself invalid we keep the tool but
        // skip validation (better to pass the model's output through than to
        // hard-fail the run on a bad schema).
        let validator = jsonschema::validator_for(&schema).ok();
        Self {
            description: format!(
                "Call this tool with your final answer to finish the task. Its \
                 arguments MUST conform to the required schema. Do any needed \
                 work with other tools first, then call `{name}` exactly once."
            ),
            name,
            schema,
            validator,
            slot,
        }
    }
}

#[async_trait]
impl Tool for StructuredOutputTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.schema.clone()
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        if let Some(validator) = &self.validator {
            let errors: Vec<String> = validator
                .iter_errors(&input)
                .map(|e| {
                    let path = e.instance_path.to_string();
                    if path.is_empty() {
                        e.to_string()
                    } else {
                        format!("{path}: {e}")
                    }
                })
                .collect();
            if !errors.is_empty() {
                // Bounce back as a tool error so the model self-corrects and
                // calls again. The slot stays empty → the run continues.
                return ToolResult::error(format!(
                    "Your output does not match the required schema. Fix these and call `{}` again:\n- {}",
                    self.name,
                    errors.join("\n- ")
                ));
            }
        }
        *self.slot.lock().expect("output slot poisoned") = Some(input);
        ToolResult::ok("Final answer accepted.")
    }
}
