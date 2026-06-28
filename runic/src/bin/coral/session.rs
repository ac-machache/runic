//! Coral's session store — conversation state persistence + the `search_chats`
//! tool. Postgres when `DATABASE_URL` is set; in-memory (ephemeral) otherwise.
//!
//! Options the `Sessions` builder offers:
//! - postgres vs in-memory store (chosen here by `DATABASE_URL`).
//! - `.without_search()` — drop the `search_chats` tool (kept on by default).

use runic_substrate::{Sessions, sessions_memory, sessions_postgres};

pub async fn coral_sessions() -> Sessions {
    match std::env::var("DATABASE_URL").ok().filter(|s| !s.is_empty()) {
        Some(url) => sessions_postgres(&url).await,
        None => {
            tracing::warn!("DATABASE_URL unset — using in-memory sessions (no persistence)");
            sessions_memory()
        }
    }
}
