//! Ergonomic builders over the session + artifact stores.

use std::path::PathBuf;
use std::sync::Arc;

use runic_tool::Tool;

use crate::{
    ArtifactStore, LocalArtifactStore, MemoryArtifactStore, MemorySessionStore, SearchChatsTool,
    SessionStore,
};

// ── session store ────────────────────────────────────────────────────────────

/// In-memory session store — ephemeral, for tests / single-run use.
pub fn sessions_memory() -> Sessions {
    tracing::info!("using in-memory session store (ephemeral)");
    Sessions {
        store: Arc::new(MemorySessionStore::new()),
        search_tool: true,
    }
}

/// Connect a Postgres session store (runs migrations). Falls back to in-memory
/// (ephemeral, no persistence) if the connection fails — logged as an error.
#[cfg(feature = "postgres")]
pub async fn sessions_postgres(database_url: &str) -> Sessions {
    match crate::PostgresSessionStore::connect(database_url).await {
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

// ── artifact (blob) store ─────────────────────────────────────────────────────

/// In-memory artifact store — ephemeral, for tests.
pub fn blobs_memory() -> Blobs {
    tracing::info!("using in-memory artifact store (ephemeral)");
    Blobs {
        store: Arc::new(MemoryArtifactStore::new()),
    }
}

/// Filesystem artifact store rooted at `root` (bytes + per-session index).
pub fn blobs_local(root: impl Into<PathBuf>) -> Blobs {
    let root = root.into();
    tracing::info!(root = %root.display(), "using local artifact store");
    Blobs {
        store: Arc::new(LocalArtifactStore::new(root)),
    }
}

/// Postgres metadata index + bytes on the local filesystem under `bytes_root`.
/// Falls back to local-only if the Postgres connection fails — logged as error.
#[cfg(feature = "postgres")]
pub async fn blobs_postgres(database_url: &str, bytes_root: impl Into<PathBuf>) -> Blobs {
    let bytes_root = bytes_root.into();
    let bytes: Arc<dyn ArtifactStore> = Arc::new(LocalArtifactStore::new(bytes_root.clone()));
    match crate::PostgresArtifactStore::connect(database_url, bytes.clone(), "local").await {
        Ok(store) => {
            tracing::info!(bytes_root = %bytes_root.display(), "connected to postgres artifact store (bytes on local fs)");
            Blobs {
                store: Arc::new(store),
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "postgres artifact store failed — falling back to local-only");
            Blobs { store: bytes }
        }
    }
}

pub struct Blobs {
    store: Arc<dyn ArtifactStore>,
}

impl Blobs {
    /// The store itself — for the server's artifact endpoints.
    pub fn store(&self) -> Arc<dyn ArtifactStore> {
        self.store.clone()
    }
}
