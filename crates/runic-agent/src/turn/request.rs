//! Step: build the [`CompletionRequest`] from state — system prompt, the
//! provider-facing message list (folded from the event log), and tool specs.
//! Per-provider schema normalization happens inside the driver, not here.

use runic_provider::CompletionRequest;
use runic_tool::ToolSpec;
use runic_types::ToolDefinition;

use crate::Agent;

/// Map a tool's LLM-facing spec to a provider tool definition.
fn spec_to_def(spec: ToolSpec) -> ToolDefinition {
    ToolDefinition {
        name: spec.name,
        description: spec.description,
        input_schema: spec.parameters,
    }
}

impl Agent {
    pub(crate) fn prepare_request(&self) -> CompletionRequest {
        let mut messages = self.state.messages_for_provider();

        // Swap summarized tool results for their full output, for this call
        // only; the overlay is consumed (cleared) here.
        let mut overlay = self
            .transient_tool_outputs
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if !overlay.is_empty() {
            for msg in &mut messages {
                let runic_types::MessageContent::Blocks(blocks) = &mut msg.content else {
                    continue;
                };
                for block in blocks {
                    if let runic_types::ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = block
                        && let Some(full) = overlay.get(tool_use_id)
                    {
                        *content = full.clone();
                    }
                }
            }
            overlay.clear();
        }
        drop(overlay);

        let mut tools: Vec<ToolDefinition> = self
            .tools
            .values()
            .map(|tool| spec_to_def(tool.spec()))
            .collect();

        // Rebuild on-demand-activated tool specs each turn so any tool the
        // model just activated (via `tool_search`) appears in this request.
        if let Some(activated) = &self.activated {
            let guard = activated
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            tools.extend(guard.specs().into_iter().map(spec_to_def));
        }

        if let Some(schema) = &self.config.output_schema {
            tools.push(ToolDefinition {
                name: crate::FINAL_ANSWER_TOOL.to_string(),
                description: "Call this with your final answer as JSON matching the schema, once the task is complete.".to_string(),
                input_schema: schema.clone(),
            });
        }

        let system = if self.state.system_prompt.is_empty() {
            None
        } else {
            Some(self.state.system_prompt.clone())
        };

        CompletionRequest {
            model: self.config.model.clone(),
            messages,
            tools,
            max_tokens: self.config.max_tokens,
            temperature: self.config.temperature,
            system,
            thinking: None,
        }
    }
}
