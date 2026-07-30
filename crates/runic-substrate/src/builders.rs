//! Ergonomic builders over the session + artifact stores.

use std::path::PathBuf;
use std::sync::Arc;

use runic_hook::WriteHook;
use runic_tool::Tool;

use crate::{
    ArtifactStore, LocalArtifactStore, MemoryArtifactStore, MemorySessionStore, SessionStore,
};

// ── session store ────────────────────────────────────────────────────────────

/// In-memory session store — ephemeral, for tests / single-run use.
pub fn sessions_memory() -> Sessions {
    tracing::info!("using in-memory session store (ephemeral)");
    Sessions::from(Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>)
}

/// Connect a Postgres session store (runs migrations). Fails closed: returns the
/// connect/migration error rather than silently degrading to a different backend.
#[cfg(feature = "postgres")]
pub async fn sessions_postgres(database_url: &str) -> crate::Result<Sessions> {
    let store = crate::PostgresSessionStore::connect(database_url).await?;
    tracing::info!("connected to postgres session store");
    Ok(Sessions::from(Arc::new(store) as Arc<dyn SessionStore>))
}

/// Dev/demo only: Postgres if it connects, else an ephemeral in-memory store.
/// Do NOT use in production — on a connect failure data silently stops persisting.
#[cfg(feature = "postgres")]
pub async fn sessions_postgres_or_memory(database_url: &str) -> Sessions {
    match sessions_postgres(database_url).await {
        Ok(sessions) => sessions,
        Err(e) => {
            tracing::error!(error = %e, "postgres session store failed — falling back to in-memory (DEV ONLY, no persistence)");
            sessions_memory()
        }
    }
}

#[derive(Clone)]
pub struct Sessions {
    store: Arc<dyn SessionStore>,
    tools: Vec<Arc<dyn Tool>>,
    hooks: Vec<Arc<dyn WriteHook>>,
}

impl Sessions {
    pub fn store(&self) -> Arc<dyn SessionStore> {
        self.store.clone()
    }

    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    pub fn tool(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.push(Arc::new(tool));
        self
    }

    pub fn hooks(&self) -> &[Arc<dyn WriteHook>] {
        &self.hooks
    }

    pub fn hook(mut self, hook: impl WriteHook + 'static) -> Self {
        self.hooks.push(Arc::new(hook));
        self
    }
}

impl From<Arc<dyn SessionStore>> for Sessions {
    fn from(store: Arc<dyn SessionStore>) -> Self {
        Self {
            store,
            tools: Vec::new(),
            hooks: Vec::new(),
        }
    }
}

// ── artifact (blob) store ─────────────────────────────────────────────────────

/// In-memory artifact store — ephemeral, for tests.
pub fn blobs_memory() -> Blobs {
    tracing::info!("using in-memory artifact store (ephemeral)");
    Blobs::from(Arc::new(MemoryArtifactStore::new()) as Arc<dyn ArtifactStore>)
}

/// Filesystem artifact store rooted at `root` (bytes + per-session index).
pub fn blobs_local(root: impl Into<PathBuf>) -> Blobs {
    let root = root.into();
    tracing::info!(root = %root.display(), "using local artifact store");
    Blobs::from(Arc::new(LocalArtifactStore::new(root)) as Arc<dyn ArtifactStore>)
}

/// Postgres metadata index + bytes on the local filesystem under `bytes_root`.
/// Fails closed: returns the connect/migration error rather than degrading.
#[cfg(feature = "postgres")]
pub async fn blobs_postgres(
    database_url: &str,
    bytes_root: impl Into<PathBuf>,
) -> crate::Result<Blobs> {
    let bytes_root = bytes_root.into();
    let bytes: Arc<dyn ArtifactStore> = Arc::new(LocalArtifactStore::new(bytes_root.clone()));
    let store = crate::PostgresArtifactStore::connect(database_url, bytes, "local").await?;
    tracing::info!(bytes_root = %bytes_root.display(), "connected to postgres artifact store (bytes on local fs)");
    Ok(Blobs::from(Arc::new(store) as Arc<dyn ArtifactStore>))
}

/// Dev/demo only: Postgres-indexed if it connects, else local-only bytes. Do NOT
/// use in production — on a connect failure artifacts silently lose their index.
#[cfg(feature = "postgres")]
pub async fn blobs_postgres_or_local(database_url: &str, bytes_root: impl Into<PathBuf>) -> Blobs {
    let bytes_root = bytes_root.into();
    match blobs_postgres(database_url, bytes_root.clone()).await {
        Ok(blobs) => blobs,
        Err(e) => {
            tracing::error!(error = %e, "postgres artifact store failed — falling back to local-only (DEV ONLY)");
            blobs_local(bytes_root)
        }
    }
}

#[derive(Clone)]
pub struct Blobs {
    store: Arc<dyn ArtifactStore>,
    tools: Vec<Arc<dyn Tool>>,
    hooks: Vec<Arc<dyn WriteHook>>,
}

impl Blobs {
    /// The store itself — for the server's artifact endpoints.
    pub fn store(&self) -> Arc<dyn ArtifactStore> {
        self.store.clone()
    }

    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    pub fn tool(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.push(Arc::new(tool));
        self
    }

    pub fn hooks(&self) -> &[Arc<dyn WriteHook>] {
        &self.hooks
    }

    pub fn hook(mut self, hook: impl WriteHook + 'static) -> Self {
        self.hooks.push(Arc::new(hook));
        self
    }
}

impl From<Arc<dyn ArtifactStore>> for Blobs {
    fn from(store: Arc<dyn ArtifactStore>) -> Self {
        Self {
            store,
            tools: Vec::new(),
            hooks: Vec::new(),
        }
    }
}
