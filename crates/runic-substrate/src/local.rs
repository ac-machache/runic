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

use crate::artifacts::{Artifact, ArtifactSource, ArtifactStore, new_artifact_id};
use crate::{Error, Result};

/// A filesystem-backed artifact store rooted at a directory.
pub struct LocalArtifactStore {
    root: PathBuf,
}

impl LocalArtifactStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn blob_path(&self, id: &str) -> Result<PathBuf> {
        safe_artifact_id(id)?;
        Ok(self.root.join("blobs").join(id))
    }
    fn meta_path(&self, id: &str) -> Result<PathBuf> {
        safe_artifact_id(id)?;
        Ok(self.root.join("blobs").join(format!("{id}.json")))
    }
    fn index_path(&self, tenant: &str, session_id: &str) -> PathBuf {
        self.root
            .join("index")
            .join(encode_segment(tenant))
            .join(format!("{}.jsonl", encode_segment(session_id)))
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

        let blob = self.blob_path(&artifact.id)?;
        ensure_parent(&blob).await?;
        tokio::fs::write(&blob, bytes).await.map_err(io)?;
        tokio::fs::write(
            self.meta_path(&artifact.id)?,
            serde_json::to_vec(&artifact).map_err(serde)?,
        )
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
        match tokio::fs::read(self.blob_path(id)?).await {
            Ok(b) => Ok(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::NotFound(id.into())),
            Err(e) => Err(io(e)),
        }
    }

    async fn head(&self, id: &str) -> Result<Artifact> {
        match tokio::fs::read(self.meta_path(id)?).await {
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
        let Ok(blob) = self.blob_path(id) else {
            return Ok(());
        };
        let Ok(meta) = self.meta_path(id) else {
            return Ok(());
        };
        let _ = tokio::fs::remove_file(blob).await;
        let _ = tokio::fs::remove_file(meta).await;
        Ok(())
    }
}

async fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(io)?;
    }
    Ok(())
}

fn safe_artifact_id(id: &str) -> Result<&str> {
    if !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        Ok(id)
    } else {
        Err(Error::NotFound(id.to_string()))
    }
}

fn encode_segment(s: &str) -> String {
    if s.is_empty() {
        return "%EMPTY".to_string();
    }
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0x0f));
        }
    }
    out
}

fn hex_digit(n: u8) -> char {
    match n {
        0..=9 => char::from(b'0' + n),
        10..=15 => char::from(b'A' + (n - 10)),
        _ => unreachable!("hex digit nibble"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_artifact_roundtrip_persists_index() {
        let tmp = tempfile::tempdir().unwrap();
        let s = LocalArtifactStore::new(tmp.path());
        let a = s
            .put(
                "org1",
                "sess",
                "image/png",
                ArtifactSource::ToolOutput,
                b"\x89PNG",
            )
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

    #[tokio::test]
    async fn local_artifact_index_paths_do_not_collapse_similar_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let s = LocalArtifactStore::new(tmp.path());
        let slash_tenant = s
            .put(
                "tenant/a",
                "session",
                "text/plain",
                ArtifactSource::UserUpload,
                b"slash tenant",
            )
            .await
            .unwrap();
        let underscore_tenant = s
            .put(
                "tenant_a",
                "session",
                "text/plain",
                ArtifactSource::UserUpload,
                b"underscore tenant",
            )
            .await
            .unwrap();
        let slash_session = s
            .put(
                "tenant",
                "session/1",
                "text/plain",
                ArtifactSource::UserUpload,
                b"slash session",
            )
            .await
            .unwrap();
        let underscore_session = s
            .put(
                "tenant",
                "session_1",
                "text/plain",
                ArtifactSource::UserUpload,
                b"underscore session",
            )
            .await
            .unwrap();

        let tenant_slash = s.list("tenant/a", "session").await.unwrap();
        assert_eq!(tenant_slash.len(), 1);
        assert_eq!(tenant_slash[0].id, slash_tenant.id);

        let tenant_underscore = s.list("tenant_a", "session").await.unwrap();
        assert_eq!(tenant_underscore.len(), 1);
        assert_eq!(tenant_underscore[0].id, underscore_tenant.id);

        let session_slash = s.list("tenant", "session/1").await.unwrap();
        assert_eq!(session_slash.len(), 1);
        assert_eq!(session_slash[0].id, slash_session.id);

        let session_underscore = s.list("tenant", "session_1").await.unwrap();
        assert_eq!(session_underscore.len(), 1);
        assert_eq!(session_underscore[0].id, underscore_session.id);
    }

    #[tokio::test]
    async fn local_artifact_ids_cannot_escape_blob_root() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        tokio::fs::write(&outside, b"do not touch").await.unwrap();
        let s = LocalArtifactStore::new(tmp.path().join("store"));

        assert!(matches!(s.get("../outside").await, Err(Error::NotFound(_))));
        assert!(matches!(
            s.head("../outside").await,
            Err(Error::NotFound(_))
        ));
        s.delete("../outside").await.unwrap();

        let still_there = tokio::fs::read(&outside).await.unwrap();
        assert_eq!(still_there, b"do not touch");
    }
}
