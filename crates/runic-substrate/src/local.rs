//! `LocalArtifactStore` — a filesystem [`ArtifactStore`].
//!
//! `blobs/<id>` (bytes) + `blobs/<id>.json` (metadata) + per-session
//! `index/<tenant>/<session>.jsonl`. `get`/`head`/`delete` work by id; the
//! index is only for `list`. Maps cleanly onto S3 (a `blobs/` prefix + index
//! objects).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::Utc;
use tokio::io::AsyncWriteExt;

use crate::artifacts::{new_artifact_id, Artifact, ArtifactSource, ArtifactStore};
use crate::{Error, Result};

/// A filesystem-backed artifact store rooted at a directory.
pub struct LocalArtifactStore {
    root: PathBuf,
}

impl LocalArtifactStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn blob_path(&self, id: &str) -> PathBuf {
        self.root.join("blobs").join(id)
    }
    fn meta_path(&self, id: &str) -> PathBuf {
        self.root.join("blobs").join(format!("{id}.json"))
    }
    fn index_path(&self, tenant: &str, session_id: &str) -> PathBuf {
        self.root
            .join("index")
            .join(sanitize(tenant))
            .join(format!("{}.jsonl", sanitize(session_id)))
    }
}

fn io(e: std::io::Error) -> Error {
    Error::Io(e.to_string())
}
fn serde(e: serde_json::Error) -> Error {
    Error::Serde(e.to_string())
}

#[async_trait]
impl ArtifactStore for LocalArtifactStore {
    async fn put(
        &self,
        tenant: &str,
        session_id: &str,
        mime_type: &str,
        source: ArtifactSource,
        bytes: &[u8],
    ) -> Result<Artifact> {
        let artifact = Artifact {
            id: new_artifact_id(),
            mime_type: mime_type.to_string(),
            size: bytes.len() as u64,
            source,
            created_at: Utc::now(),
        };

        let blob = self.blob_path(&artifact.id);
        ensure_parent(&blob).await?;
        tokio::fs::write(&blob, bytes).await.map_err(io)?;
        tokio::fs::write(self.meta_path(&artifact.id), serde_json::to_vec(&artifact).map_err(serde)?)
            .await
            .map_err(io)?;

        let index = self.index_path(tenant, session_id);
        ensure_parent(&index).await?;
        let mut line = serde_json::to_string(&artifact).map_err(serde)?;
        line.push('\n');
        let mut f = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&index)
            .await
            .map_err(io)?;
        f.write_all(line.as_bytes()).await.map_err(io)?;

        Ok(artifact)
    }

    async fn get(&self, id: &str) -> Result<Vec<u8>> {
        match tokio::fs::read(self.blob_path(id)).await {
            Ok(b) => Ok(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::NotFound(id.into())),
            Err(e) => Err(io(e)),
        }
    }

    async fn head(&self, id: &str) -> Result<Artifact> {
        match tokio::fs::read(self.meta_path(id)).await {
            Ok(b) => serde_json::from_slice(&b).map_err(serde),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::NotFound(id.into())),
            Err(e) => Err(io(e)),
        }
    }

    async fn list(&self, tenant: &str, session_id: &str) -> Result<Vec<Artifact>> {
        let text = match tokio::fs::read_to_string(self.index_path(tenant, session_id)).await {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io(e)),
        };
        Ok(text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Artifact>(l).ok())
            .collect())
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let _ = tokio::fs::remove_file(self.blob_path(id)).await;
        let _ = tokio::fs::remove_file(self.meta_path(id)).await;
        Ok(())
    }
}

async fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(io)?;
    }
    Ok(())
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_artifact_roundtrip_persists_index() {
        let tmp = tempfile::tempdir().unwrap();
        let s = LocalArtifactStore::new(tmp.path());
        let a = s
            .put("org1", "sess", "image/png", ArtifactSource::ToolOutput, b"\x89PNG")
            .await
            .unwrap();
        assert_eq!(s.get(&a.id).await.unwrap(), b"\x89PNG");
        assert_eq!(s.list("org1", "sess").await.unwrap().len(), 1);

        // a fresh store over the same root sees the persisted index
        let reopened = LocalArtifactStore::new(tmp.path());
        assert_eq!(reopened.list("org1", "sess").await.unwrap().len(), 1);
        s.delete(&a.id).await.unwrap();
        assert!(matches!(s.get(&a.id).await, Err(Error::NotFound(_))));
    }
}
