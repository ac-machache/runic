//! Step: append messages to the event log. (Session repair for strict
//! providers — OpenFang's `ensure_starts_with_user` / orphaned-`tool_use`
//! pruning — lands in a later step.)

use chrono::Utc;

use runic_state::SessionEvent;
use runic_types::Message;

use crate::Agent;

impl Agent {
    /// Record the assistant's reply for this run.
    pub(crate) fn push_assistant(&mut self, msg: Message, run_id: &str) {
        self.state.push_event(SessionEvent::Message {
            run_id: run_id.to_string(),
            msg,
            at: Utc::now(),
        });
    }

    /// Record the combined tool-result message (one user-role message holding
    /// every tool result this turn).
    pub(crate) fn push_tool_results(&mut self, msg: Message, run_id: &str) {
        self.state.push_event(SessionEvent::Message {
            run_id: run_id.to_string(),
            msg,
            at: Utc::now(),
        });
    }
}
