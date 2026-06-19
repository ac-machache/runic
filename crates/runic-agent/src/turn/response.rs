//! Step: interpret the provider response into the assistant [`Message`] + a
//! [`TurnRecord`]. The assistant message keeps **all** blocks verbatim
//! (`Thinking`/`RedactedThinking` included) so reasoning models retain state
//! and the `tool_use` blocks round-trip to match the next turn's results.

use runic_provider::CompletionResponse;
use runic_types::Message;

use crate::{Agent, TurnRecord};

impl Agent {
    pub(crate) fn interpret_response(response: CompletionResponse) -> (Message, TurnRecord) {
        let assistant = Message::assistant_with_blocks(response.content);
        let turn = TurnRecord {
            tool_calls: response.tool_calls,
            stop_reason: response.stop_reason,
            usage: response.usage,
        };
        (assistant, turn)
    }
}
