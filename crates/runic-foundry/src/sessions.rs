use std::sync::Arc;

use runic_substrate::{MemorySessionStore, PostgresSessionStore, SearchChatsTool, SessionStore};
use runic_tool::Tool;

/// Connect a Postgres session store (runs migrations). Falls back to in-memory
/// (ephemeral, no persistence) if the connection fails — logged as an error.
pub async fn sessions_postgres(database_url: &str) -> Sessions {
    match PostgresSessionStore::connect(database_url).await {
        Ok(store) => {
            tracing::info!("connected to postgres session store");
            Sessions {
                store: Arc::new(store),
                search_tool: true,
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "postgres session store failed — falling back to in-memory (no persistence)");
            sessions_memory()
        }
    }
}

/// In-memory session store — ephemeral, for tests / single-run use.
pub fn sessions_memory() -> Sessions {
    tracing::info!("using in-memory session store (ephemeral)");
    Sessions {
        store: Arc::new(MemorySessionStore::new()),
        search_tool: true,
    }
}

pub struct Sessions {
    store: Arc<dyn SessionStore>,
    search_tool: bool,
}

impl Sessions {
    pub fn without_search(mut self) -> Self {
        self.search_tool = false;
        self
    }

    /// The store itself — for the server's persistence and the search tool.
    pub fn store(&self) -> Arc<dyn SessionStore> {
        self.store.clone()
    }

    pub fn tools(&self) -> Option<Arc<dyn Tool>> {
        if self.search_tool {
            tracing::debug!("search_chats tool enabled");
            Some(Arc::new(SearchChatsTool::new(self.store.clone())) as Arc<dyn Tool>)
        } else {
            None
        }
    }
}
