use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::Result;

/// Where an artifact came from (for listing / GC policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactSource {
    UserUpload,
    ToolOutput,
    ModelOutput,
    Other,
}

impl ArtifactSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactSource::UserUpload => "user_upload",
            ArtifactSource::ToolOutput => "tool_output",
            ArtifactSource::ModelOutput => "model_output",
            ArtifactSource::Other => "other",
        }
    }

    pub fn parse(raw: &str) -> ArtifactSource {
        match raw {
            "user_upload" => ArtifactSource::UserUpload,
            "tool_output" => ArtifactSource::ToolOutput,
            "model_output" => ArtifactSource::ModelOutput,
            _ => ArtifactSource::Other,
        }
    }
}

/// A stored artifact's metadata — what a message references and what `list`
/// returns. Raw bytes are fetched separately via [`ArtifactStore::get`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    /// Opaque, backend-defined id; the stable handle a `MediaRef` carries.
    pub id: String,
    pub mime_type: String,
    pub size: u64,
    pub source: ArtifactSource,
    pub created_at: DateTime<Utc>,
}

/// The media storage interface. One trait; many backends (an OpenDAL operator
/// over memory/disk/S3/GCS/Azure, or a Postgres-indexed composition).
#[async_trait]
pub trait ArtifactStore: Send + Sync {
    /// Store `bytes` for `(tenant, session_id)`; return the artifact metadata.
    async fn put(
        &self,
        tenant: &str,
        session_id: &str,
        mime_type: &str,
        source: ArtifactSource,
        bytes: &[u8],
    ) -> Result<Artifact>;

    /// Fetch the raw bytes by id.
    async fn get(&self, id: &str) -> Result<Vec<u8>>;

    /// Fetch metadata only (cheap; no bytes).
    async fn head(&self, id: &str) -> Result<Artifact>;

    /// List artifacts belonging to one session.
    async fn list(&self, tenant: &str, session_id: &str) -> Result<Vec<Artifact>>;

    /// Delete an artifact's bytes (best-effort GC).
    async fn delete(&self, id: &str) -> Result<()>;

    /// A fetchable URL, if the backend can serve one (S3 presigned, web route).
    /// Default `None` — callers fall back to `get`.
    async fn url(&self, _id: &str) -> Result<Option<String>> {
        Ok(None)
    }

    fn reindex(&self, _bytes: Arc<dyn ArtifactStore>) -> Option<Arc<dyn ArtifactStore>> {
        None
    }

    /// Drop every artifact belonging to one session; returns how many were
    /// deleted. Default: `list` then `delete` one at a time. Backends that can
    /// batch the metadata cleanup (e.g. a single indexed `DELETE`) should
    /// override this.
    async fn delete_session_artifacts(&self, tenant: &str, session_id: &str) -> Result<usize> {
        let artifacts = self.list(tenant, session_id).await?;
        for artifact in &artifacts {
            self.delete(&artifact.id).await?;
        }
        Ok(artifacts.len())
    }

    /// Remove artifacts orphaned by a partial write (bytes stored but never
    /// indexed), skipping anything younger than `older_than` so an in-flight
    /// `put` is never raced. Idempotent; returns how many were swept. Default
    /// `0` — single-layer backends have no bytes/index split to orphan.
    async fn sweep_orphans(
        &self,
        _tenant: &str,
        _session_id: &str,
        _older_than: chrono::Duration,
    ) -> Result<usize> {
        Ok(0)
    }
}

pub(crate) fn new_artifact_id() -> String {
    format!("art-{}", uuid::Uuid::new_v4().simple())
}
