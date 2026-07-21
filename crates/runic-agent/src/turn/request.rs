//! Step: build the [`CompletionRequest`] from state — system prompt, the
//! provider-facing message list (folded from the event log), and tool specs.
//! Per-provider schema normalization happens inside the driver, not here.

use runic_provider::CompletionRequest;
use runic_tool::ToolSpec;
use runic_types::ToolDefinition;

use crate::Runner;

/// Map a tool's LLM-facing spec to a provider tool definition.
fn spec_to_def(spec: ToolSpec) -> ToolDefinition {
    ToolDefinition {
        name: spec.name,
        description: spec.description,
        input_schema: spec.parameters,
    }
}

impl Runner {
    pub(crate) fn prepare_request(&mut self) -> CompletionRequest {
        let mut messages = self.state.messages_for_provider().to_vec();

        // Swap summarized tool results for their full output, for this call
        // only; the overlay is consumed here.
        let mut overlay = std::mem::take(&mut self.transient_tool_outputs);
        if !overlay.is_empty() {
            for msg in messages.iter_mut().rev() {
                let runic_types::MessageContent::Blocks(blocks) = &mut msg.content else {
                    continue;
                };
                for block in blocks.iter_mut().rev() {
                    if let runic_types::ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = block
                        && let Some(queue) = overlay.get_mut(tool_use_id)
                        && let Some(full) = queue.pop()
                    {
                        *content = runic_types::ToolResultPayload::Inline(full);
                    }
                }
            }
        }

        let mut tools: Vec<ToolDefinition> = self
            .tools
            .values()
            .map(|tool| spec_to_def(tool.spec()))
            .collect();

        // On-demand activations (materialized from state at the turn top).
        tools.extend(self.activated.specs().into_iter().map(spec_to_def));
        tools.sort_by(|a, b| a.name.cmp(&b.name));

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
            thinking: self.config.thinking.clone(),
        }
    }
}
