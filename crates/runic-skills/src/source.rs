//! `SkillSource` — a read-only source of skill files. The whole abstraction is
//! two methods: list the skill folders, and read a file. That's all skill
//! loading needs, so any storage (local disk now, S3 later) is a thin impl —
//! not the heavy read-write filesystem an agent workspace would need.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::security::{self, MAX_SKILLS};

/// A read-only source of skills. Paths are relative to the source's own root.
#[async_trait]
pub trait SkillSource: Send + Sync {
    /// The skill folder names directly under this source (one level).
    async fn entries(&self) -> anyhow::Result<Vec<String>>;

    /// Read a UTF-8 file at `rel` within this source — a `<entry>/SKILL.md` or
    /// a sub-file. Implementations must keep `rel` inside the source root.
    async fn read(&self, rel: &str) -> anyhow::Result<String>;
}

/// A local-directory skill source over `tokio::fs`.
pub fn local(dir: impl Into<PathBuf>) -> Arc<dyn SkillSource> {
    Arc::new(LocalSource { root: dir.into() })
}

/// An S3-backed skill source. Requires the `s3` feature (pulls the AWS SDK).
#[cfg(feature = "s3")]
pub fn s3(_bucket: &str, _prefix: &str) -> Arc<dyn SkillSource> {
    // Extension point: implement `entries` over ListObjectsV2 (prefix +
    // delimiter) and `read` over GetObject. Left unimplemented so the shape is
    // proven without dragging `aws-sdk-s3` into the default build.
    unimplemented!("s3 skill source: wire aws-sdk-s3 here behind the `s3` feature")
}

struct LocalSource {
    root: PathBuf,
}

#[async_trait]
impl SkillSource for LocalSource {
    async fn entries(&self) -> anyhow::Result<Vec<String>> {
        let mut rd = match tokio::fs::read_dir(&self.root).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(root = %self.root.display(), "skills dir does not exist — skipping");
                return Ok(Vec::new());
            }
            Err(e) => return Err(e.into()),
        };

        let mut out = Vec::new();
        while let Some(entry) = rd.next_entry().await? {
            if out.len() >= MAX_SKILLS {
                tracing::warn!(
                    root = %self.root.display(),
                    cap = MAX_SKILLS,
                    "skills cap reached — remaining entries skipped"
                );
                break;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue; // skip dotfiles / hidden dirs
            }
            let file_type = entry.file_type().await?;
            // Don't follow symlinked directories out of the source root.
            if file_type.is_symlink() || !file_type.is_dir() {
                continue;
            }
            out.push(name);
        }
        Ok(out)
    }

    async fn read(&self, rel: &str) -> anyhow::Result<String> {
        security::safe_rel(rel)?;
        let target = self.root.join(rel);
        // Defense in depth: canonicalize and confirm we stayed under the root
        // (catches symlinks that escape).
        let canon = tokio::fs::canonicalize(&target)
            .await
            .map_err(|e| anyhow::anyhow!("cannot resolve '{rel}': {e}"))?;
        let root_canon = tokio::fs::canonicalize(&self.root)
            .await
            .map_err(|e| anyhow::anyhow!("invalid skills root: {e}"))?;
        if !canon.starts_with(&root_canon) {
            anyhow::bail!("'{rel}' escapes the skills source");
        }
        tokio::fs::read_to_string(&canon)
            .await
            .map_err(|e| anyhow::anyhow!("cannot read '{rel}': {e}"))
    }
}
