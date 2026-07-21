use chrono::Utc;

use runic_types::Message;

use crate::{AgentEvent, Session};

impl Session {
    pub(crate) fn push_assistant(&mut self, msg: Message, run_id: &str) {
        self.emit(AgentEvent::Message {
            run_id: run_id.to_string(),
            msg,
            at: Utc::now(),
        });
    }

    pub(crate) fn push_tool_results(&mut self, msg: Message, run_id: &str) {
        self.emit(AgentEvent::Message {
            run_id: run_id.to_string(),
            msg,
            at: Utc::now(),
        });
    }
}
