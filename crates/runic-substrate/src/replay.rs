//! Replay helpers — fold a stored event log back into an [`AgentState`] (or
//! just its message list) ready to resume.

use runic_state::{AgentState, SessionEvent};
use runic_types::Message;

use crate::{Error, SessionStore};

/// Rebuild a full [`AgentState`] for `(tenant, session_id)` from its log.
/// `tenant` is used as the state's `user_id` (the tenant axis).
pub async fn replay_into_state(
    store: &dyn SessionStore,
    tenant: &str,
    session_id: &str,
    system_prompt: impl Into<String>,
) -> Result<AgentState, Error> {
    let stored = store.read(tenant, session_id).await?;
    let mut state = AgentState::new(tenant, session_id, system_prompt);
    for entry in stored {
        state.push_event(entry.event);
    }
    Ok(state)
}

/// Like [`replay_into_state`] but returns just the provider-facing message
/// list (folding `Message` appends and `StateSnapshot` replacements).
pub async fn replay_messages(
    store: &dyn SessionStore,
    tenant: &str,
    session_id: &str,
) -> Result<Vec<Message>, Error> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemorySessionStore, SessionStore};
    use chrono::Utc;

    fn msg_event(role_user: bool, text: &str) -> SessionEvent {
        let msg = if role_user {
            Message::user(text)
        } else {
            Message::assistant(text)
        };
        SessionEvent::Message {
            run_id: "r".into(),
            msg,
            at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn replay_rebuilds_state_and_message_list() {
        let store = MemorySessionStore::new();
        store
            .append(
                "t",
                "s",
                &SessionEvent::RunStart {
                    run_id: "r".into(),
                    at: Utc::now(),
                },
            )
            .await
            .unwrap();
        store
            .append("t", "s", &msg_event(true, "hello"))
            .await
            .unwrap();
        store
            .append("t", "s", &msg_event(false, "hi there"))
            .await
            .unwrap();

        let state = replay_into_state(&store, "t", "s", "be nice")
            .await
            .unwrap();
        assert_eq!(state.user_id, "t");
        assert_eq!(state.session_id, "s");
        assert_eq!(state.last_assistant_text().as_deref(), Some("hi there"));

        // RunStart is not a message; only the two messages fold in.
        let msgs = replay_messages(&store, "t", "s").await.unwrap();
        assert_eq!(msgs.len(), 2);
    }

    #[tokio::test]
    async fn state_snapshot_replaces_messages_on_replay() {
        let store = MemorySessionStore::new();
        store
            .append("t", "s", &msg_event(true, "old one"))
            .await
            .unwrap();
        store
            .append(
                "t",
                "s",
                &SessionEvent::StateSnapshot {
                    run_id: "r".into(),
                    messages: vec![Message::user("compacted")],
                    system_prompt: "sp".into(),
                    reason: "compaction".into(),
                    at: Utc::now(),
                },
            )
            .await
            .unwrap();

        let msgs = replay_messages(&store, "t", "s").await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content.text_content(), "compacted");
    }
}
