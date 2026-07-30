//! Step: interpret the provider response into the assistant [`Message`] + a
//! [`TurnRecord`]. The assistant message keeps **all** blocks verbatim
//! (`Thinking`/`RedactedThinking` included) so reasoning models retain state
//! and the `tool_use` blocks round-trip to match the next turn's results.

use runic_provider::CompletionResponse;
use runic_types::{ContentBlock, Message, ToolCall};

use crate::{Runner, TurnRecord};

impl Runner {
    pub(crate) fn tool_calls_of(content: &[ContentBlock]) -> Vec<ToolCall> {
        content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => Some(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn interpret_response(
        response: CompletionResponse,
        model: String,
        model_ms: u64,
    ) -> (Message, TurnRecord) {
        let assistant = Message::assistant_with_blocks(response.content);
        let turn = TurnRecord {
            tool_calls: response.tool_calls,
            stop_reason: response.stop_reason,
            usage: response.usage,
            model,
            model_ms,
        };
        (assistant, turn)
    }
}
