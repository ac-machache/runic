//! Postgres-backed [`ArtifactStore`]: **metadata in the `runic.artifacts`
//! table** (FK → `runic.sessions`), **bytes delegated to an inner byte store**
//! (Local/S3).
//!
//! `put` writes bytes to the inner store, then records the metadata row; the
//! row is the index + session ownership, the bytes live wherever the inner
//! store puts them. Deleting a session cascades the *rows*; the *bytes* are
//! GC'd explicitly via [`PostgresArtifactStore::delete_session_artifacts`].

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::{PgPool, Row};

use super::{db, migrate};
use crate::artifacts::{Artifact, ArtifactSource, ArtifactStore};
use crate::{Error, Result};

/// Artifact store with metadata in Postgres + bytes in an inner store.
pub struct PostgresArtifactStore {
    pool: PgPool,
    bytes: Arc<dyn ArtifactStore>,
    /// Label recorded in `artifacts.storage` (e.g. `"local"`, `"s3"`).
    storage: String,
    /// Minimum age enforced on `sweep_orphans` cutoffs so a sweep can never
    /// race the bytes-then-row window of an in-flight `put`.
    sweep_margin: chrono::Duration,
}

impl PostgresArtifactStore {
    /// Connect, migrate, and wrap the inner byte store.
    pub async fn connect(
        database_url: &str,
        bytes: Arc<dyn ArtifactStore>,
        storage: impl Into<String>,
    ) -> Result<Self> {
        let pool = PgPool::connect(database_url).await.map_err(db)?;
        Self::from_pool(pool, bytes, storage).await
    }

    /// Build from an existing pool (shared with the session store).
    pub async fn from_pool(
        pool: PgPool,
        bytes: Arc<dyn ArtifactStore>,
        storage: impl Into<String>,
    ) -> Result<Self> {
        migrate(&pool).await?;
        Ok(Self {
            pool,
            bytes,
            storage: storage.into(),
            sweep_margin: chrono::Duration::minutes(5),
        })
    }

    /// Override the sweep-safety margin. Anything below the margin is treated
    /// as the margin; lowering it below put latency reintroduces the race.
    pub fn with_sweep_margin(mut self, margin: chrono::Duration) -> Self {
        self.sweep_margin = margin;
        self
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

fn row_to_artifact(row: &sqlx::postgres::PgRow) -> Result<Artifact> {
    Ok(Artifact {
        id: row.try_get("artifact_id").map_err(db)?,
        mime_type: row.try_get("mime_type").map_err(db)?,
        size: row.try_get::<i64, _>("size").map_err(db)? as u64,
        source: ArtifactSource::parse(&row.try_get::<String, _>("source").map_err(db)?),
        created_at: row.try_get("created_at").map_err(db)?,
    })
}

#[async_trait]
impl ArtifactStore for PostgresArtifactStore {
    async fn put(
        &self,
        tenant: &str,
        session_id: &str,
        mime_type: &str,
        source: ArtifactSource,
        bytes: &[u8],
    ) -> Result<Artifact> {
        // Ensure the sessions row exists so the FK holds (artifacts can arrive
        // before any conversation event).
        sqlx::query(
            "INSERT INTO runic.sessions (tenant, session_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(tenant)
        .bind(session_id)
        .execute(&self.pool)
        .await
        .map_err(db)?;

        // Bytes go to the inner store; it owns the id.
        let artifact = self
            .bytes
            .put(tenant, session_id, mime_type, source, bytes)
            .await?;

        sqlx::query(
            "INSERT INTO runic.artifacts
               (artifact_id, tenant, session_id, mime_type, size, source, storage, storage_key, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&artifact.id)
        .bind(tenant)
        .bind(session_id)
        .bind(&artifact.mime_type)
        .bind(artifact.size as i64)
        .bind(artifact.source.as_str())
        .bind(&self.storage)
        .bind(&artifact.id) // storage_key == inner id
        .bind(artifact.created_at)
        .execute(&self.pool)
        .await
        .map_err(db)?;

        Ok(artifact)
    }

    async fn get(&self, id: &str) -> Result<Vec<u8>> {
        // id is the inner store's key.
        self.bytes.get(id).await
    }

    async fn head(&self, id: &str) -> Result<Artifact> {
        let row = sqlx::query(
            "SELECT artifact_id, mime_type, size, source, created_at
             FROM runic.artifacts WHERE artifact_id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?
        .ok_or_else(|| Error::NotFound(id.to_string()))?;
        row_to_artifact(&row)
    }

    async fn list(&self, tenant: &str, session_id: &str) -> Result<Vec<Artifact>> {
        let rows = sqlx::query(
            "SELECT artifact_id, mime_type, size, source, created_at
             FROM runic.artifacts WHERE tenant = $1 AND session_id = $2
             ORDER BY created_at DESC",
        )
        .bind(tenant)
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.iter().map(row_to_artifact).collect()
    }

    async fn delete(&self, id: &str) -> Result<()> {
        match self.bytes.delete(id).await {
            Ok(()) | Err(Error::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
        sqlx::query("DELETE FROM runic.artifacts WHERE artifact_id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn url(&self, id: &str) -> Result<Option<String>> {
        self.bytes.url(id).await
    }

    fn reindex(&self, bytes: Arc<dyn ArtifactStore>) -> Option<Arc<dyn ArtifactStore>> {
        Some(Arc::new(Self {
            pool: self.pool.clone(),
            bytes,
            storage: self.storage.clone(),
            sweep_margin: self.sweep_margin,
        }))
    }

    async fn sweep_orphans(
        &self,
        tenant: &str,
        session_id: &str,
        older_than: chrono::Duration,
    ) -> Result<usize> {
        let stored = self.bytes.list(tenant, session_id).await?;
        let indexed: Vec<String> = sqlx::query_scalar(
            "SELECT artifact_id FROM runic.artifacts WHERE tenant = $1 AND session_id = $2",
        )
        .bind(tenant)
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        let indexed: std::collections::HashSet<String> = indexed.into_iter().collect();

        let cutoff = chrono::Utc::now() - older_than.max(self.sweep_margin);
        let mut swept = 0usize;
        for artifact in stored {
            if indexed.contains(&artifact.id) || artifact.created_at > cutoff {
                continue;
            }
            match self.bytes.delete(&artifact.id).await {
                Ok(()) | Err(Error::NotFound(_)) => {
                    swept += 1;
                    tracing::info!(%tenant, %session_id, artifact_id = %artifact.id, "orphaned artifact bytes swept");
                }
                Err(e) => return Err(e),
            }
        }
        Ok(swept)
    }

    /// Bytes are dropped one at a time via the inner store; the metadata rows
    /// are dropped in a single batched `DELETE` instead of one per artifact.
    async fn delete_session_artifacts(&self, tenant: &str, session_id: &str) -> Result<usize> {
        let artifacts = self.list(tenant, session_id).await?;
        let total = artifacts.len();
        for (deleted, artifact) in artifacts.iter().enumerate() {
            match self.bytes.delete(&artifact.id).await {
                Ok(()) | Err(Error::NotFound(_)) => {}
                Err(e) => {
                    tracing::warn!(
                        %tenant,
                        %session_id,
                        artifact_id = %artifact.id,
                        deleted,
                        total,
                        error = %e,
                        "artifact byte delete failed; session artifacts partially cleaned up"
                    );
                    return Err(e);
                }
            }
        }
        sqlx::query("DELETE FROM runic.artifacts WHERE tenant = $1 AND session_id = $2")
            .bind(tenant)
            .bind(session_id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        tracing::info!(%tenant, %session_id, artifact_count = total, "session artifacts deleted");
        Ok(total)
    }
}
