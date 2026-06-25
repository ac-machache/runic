use std::path::PathBuf;
use std::sync::Arc;

use runic_substrate::{
    ArtifactStore, LocalArtifactStore, MemoryArtifactStore, PostgresArtifactStore,
};

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
pub async fn blobs_postgres(database_url: &str, bytes_root: impl Into<PathBuf>) -> Blobs {
    let bytes_root = bytes_root.into();
    let bytes: Arc<dyn ArtifactStore> = Arc::new(LocalArtifactStore::new(bytes_root.clone()));
    match PostgresArtifactStore::connect(database_url, bytes.clone(), "local").await {
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
