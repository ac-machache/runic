//! Replay helpers — turn a stored event log back into an [`AgentState`]
//! (or just the message list) ready for resume.

use runic_agent_core::{AgentState, SessionEvent};
use runic_message_types::Message;

use crate::error::StoreError;
use crate::store::SessionStore;

/// Read every event for a session and rebuild a full [`AgentState`].
///
/// The returned state has:
///   - the original `session_id`
///   - the provided `system_prompt` (replay doesn't store this — it's
///     the caller's responsibility to know what to resume with)
///   - the complete event history
///   - empty `runtime` (typed handles like DB pools don't survive
///     across process boundaries; the caller re-installs them via the
///     builder)
///   - NO `events_tx` (no broadcast channel — the caller hands this
///     state to `AgentBuilder` which installs a fresh one on `build`)
pub async fn replay_into_state(
    store: &dyn SessionStore,
    tenant: &str,
    session_id: &str,
    system_prompt: impl Into<String>,
) -> Result<AgentState, StoreError> {
    let stored = store.read(tenant, session_id).await?;
    let mut state = AgentState::new(session_id, system_prompt);
    for entry in stored {
        // Use the public push_event (broadcast is a no-op without events_tx).
        state.push_event(entry.event);
    }
    Ok(state)
}

/// Like [`replay_into_state`] but returns just the message list. Useful
/// when you want to inspect / display history without rebuilding the
/// full agent state.
pub async fn replay_messages(
    store: &dyn SessionStore,
    tenant: &str,
    session_id: &str,
) -> Result<Vec<Message>, StoreError> {
    let stored = store.read(tenant, session_id).await?;
    let mut msgs: Vec<Message> = Vec::new();
    for entry in stored {
        match entry.event {
            SessionEvent::Message { msg, .. } => msgs.push(msg),
            SessionEvent::StateSnapshot { messages, .. } => msgs = messages,
            _ => {}
        }
    }
    Ok(msgs)
}
