use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use opendal::{ErrorKind, Operator};
use serde::{Deserialize, Serialize};

use super::def::{Artifact, ArtifactSource, ArtifactStore, new_artifact_id};
use crate::{Error, Result};

const PRESIGN_TTL: Duration = Duration::from_secs(3600);

pub fn memory() -> Result<ArtifactFiles> {
    Ok(from_operator(
        Operator::new(opendal::services::Memory::default()).map_err(dal)?,
    ))
}

pub fn local(root: impl AsRef<str>) -> Result<ArtifactFiles> {
    Ok(from_operator(
        Operator::new(opendal::services::Fs::default().root(root.as_ref())).map_err(dal)?,
    ))
}

#[cfg(feature = "s3")]
pub fn s3(bucket: &str, prefix: &str) -> Result<ArtifactFiles> {
    Ok(from_operator(
        Operator::new(opendal::services::S3::default().bucket(bucket).root(prefix)).map_err(dal)?,
    ))
}

#[cfg(feature = "gcs")]
pub fn gcs(bucket: &str, prefix: &str) -> Result<ArtifactFiles> {
    Ok(from_operator(
        Operator::new(
            opendal::services::Gcs::default()
                .bucket(bucket)
                .root(prefix),
        )
        .map_err(dal)?,
    ))
}

#[cfg(feature = "azblob")]
pub fn azblob(container: &str, prefix: &str) -> Result<ArtifactFiles> {
    Ok(from_operator(
        Operator::new(
            opendal::services::Azblob::default()
                .container(container)
                .root(prefix),
        )
        .map_err(dal)?,
    ))
}

pub fn from_operator(operator: Operator) -> ArtifactFiles {
    ArtifactFiles { operator }
}

pub struct ArtifactFiles {
    operator: Operator,
}

#[derive(Serialize, Deserialize)]
struct StoredMeta {
    #[serde(flatten)]
    artifact: Artifact,
    tenant: String,
    session_id: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    notes: BTreeMap<String, String>,
}

fn dal(error: opendal::Error) -> Error {
    match error.kind() {
        ErrorKind::NotFound => Error::NotFound(error.to_string()),
        ErrorKind::AlreadyExists => Error::AlreadyExists(error.to_string()),
        ErrorKind::Unsupported => Error::Unsupported(error.to_string()),
        ErrorKind::PermissionDenied | ErrorKind::IsADirectory | ErrorKind::NotADirectory => {
            Error::Io(error.to_string())
        }
        _ => Error::Backend(error.to_string()),
    }
}

fn serde(error: serde_json::Error) -> Error {
    Error::Serde(error.to_string())
}

fn blob_path(id: &str) -> Result<String> {
    Ok(format!("blobs/{}", safe_artifact_id(id)?))
}

fn meta_path(id: &str) -> Result<String> {
    Ok(format!("blobs/{}.json", safe_artifact_id(id)?))
}

fn session_prefix(tenant: &str, session_id: &str) -> String {
    format!(
        "index/{}/{}/",
        encode_segment(tenant),
        encode_segment(session_id)
    )
}

fn marker_path(tenant: &str, session_id: &str, id: &str) -> Result<String> {
    Ok(format!(
        "{}{}",
        session_prefix(tenant, session_id),
        safe_artifact_id(id)?
    ))
}

impl ArtifactFiles {
    async fn meta(&self, id: &str) -> Result<StoredMeta> {
        let raw = self.operator.read(&meta_path(id)?).await.map_err(dal)?;
        serde_json::from_slice(&raw.to_vec()).map_err(serde)
    }
}

#[async_trait]
impl ArtifactStore for ArtifactFiles {
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
        let stored = StoredMeta {
            artifact: artifact.clone(),
            tenant: tenant.to_string(),
            session_id: session_id.to_string(),
            notes: BTreeMap::new(),
        };

        self.operator
            .write(&blob_path(&artifact.id)?, bytes.to_vec())
            .await
            .map_err(dal)?;
        self.operator
            .write(
                &meta_path(&artifact.id)?,
                serde_json::to_vec(&stored).map_err(serde)?,
            )
            .await
            .map_err(dal)?;
        self.operator
            .write(
                &marker_path(tenant, session_id, &artifact.id)?,
                Vec::<u8>::new(),
            )
            .await
            .map_err(dal)?;

        Ok(artifact)
    }

    async fn get(&self, id: &str) -> Result<Vec<u8>> {
        let raw = self.operator.read(&blob_path(id)?).await.map_err(dal)?;
        Ok(raw.to_vec())
    }

    async fn head(&self, id: &str) -> Result<Artifact> {
        Ok(self.meta(id).await?.artifact)
    }

    async fn list(&self, tenant: &str, session_id: &str) -> Result<Vec<Artifact>> {
        let listed = match self
            .operator
            .list_with(&session_prefix(tenant, session_id))
            .recursive(false)
            .await
        {
            Ok(listed) => listed,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(dal(error)),
        };

        let mut artifacts = Vec::new();
        for entry in listed {
            if entry.metadata().is_dir() {
                continue;
            }
            match self.meta(entry.name()).await {
                Ok(stored) => artifacts.push(stored.artifact),
                Err(Error::NotFound(_)) => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(artifacts)
    }

    async fn delete(&self, id: &str) -> Result<()> {
        if safe_artifact_id(id).is_err() {
            return Ok(());
        }
        let stored = match self.meta(id).await {
            Ok(stored) => Some(stored),
            Err(Error::NotFound(_)) => None,
            Err(error) => return Err(error),
        };

        if let Some(stored) = stored {
            let marker = marker_path(&stored.tenant, &stored.session_id, id)?;
            self.operator.delete(&marker).await.map_err(dal)?;
            self.operator.delete(&meta_path(id)?).await.map_err(dal)?;
        }
        self.operator.delete(&blob_path(id)?).await.map_err(dal)?;
        Ok(())
    }

    async fn url(&self, id: &str) -> Result<Option<String>> {
        match self
            .operator
            .presign_read(&blob_path(id)?, PRESIGN_TTL)
            .await
        {
            Ok(signed) => Ok(Some(signed.uri().to_string())),
            Err(error) if error.kind() == ErrorKind::Unsupported => Ok(None),
            Err(error) => Err(dal(error)),
        }
    }
}

fn safe_artifact_id(id: &str) -> Result<&str> {
    let sane = !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    match sane {
        true => Ok(id),
        false => Err(Error::NotFound(id.to_string())),
    }
}

fn encode_segment(segment: &str) -> String {
    if segment.is_empty() {
        return "%EMPTY".to_string();
    }
    let mut encoded = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(hex_digit(byte >> 4));
            encoded.push(hex_digit(byte & 0x0f));
        }
    }
    encoded
}

fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => char::from(b'0' + nibble),
        10..=15 => char::from(b'A' + (nibble - 10)),
        _ => unreachable!("hex digit nibble"),
    }
}
