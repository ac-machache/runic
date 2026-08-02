use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use super::def::SkillSource;
use crate::security::{self, MAX_SKILLS};

pub fn local(dir: impl Into<PathBuf>) -> Arc<dyn SkillSource> {
    Arc::new(LocalSource { root: dir.into() })
}

struct LocalSource {
    root: PathBuf,
}

#[async_trait]
impl SkillSource for LocalSource {
    async fn entries(&self) -> anyhow::Result<Vec<String>> {
        let mut listing = match tokio::fs::read_dir(&self.root).await {
            Ok(listing) => listing,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(root = %self.root.display(), "skills dir does not exist — skipping");
                return Ok(Vec::new());
            }
            Err(error) => return Err(error.into()),
        };

        let mut entries = Vec::new();
        while let Some(entry) = listing.next_entry().await? {
            if entries.len() >= MAX_SKILLS {
                tracing::warn!(
                    root = %self.root.display(),
                    cap = MAX_SKILLS,
                    "skills cap reached — remaining entries skipped"
                );
                break;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let file_type = entry.file_type().await?;
            if file_type.is_symlink() || !file_type.is_dir() {
                continue;
            }
            entries.push(name);
        }
        Ok(entries)
    }

    async fn read(&self, rel: &str) -> anyhow::Result<String> {
        security::safe_rel(rel)?;
        let target = self.root.join(rel);
        let resolved = tokio::fs::canonicalize(&target)
            .await
            .map_err(|error| anyhow::anyhow!("cannot resolve '{rel}': {error}"))?;
        let root = tokio::fs::canonicalize(&self.root)
            .await
            .map_err(|error| anyhow::anyhow!("invalid skills root: {error}"))?;
        if !resolved.starts_with(&root) {
            anyhow::bail!("'{rel}' escapes the skills source");
        }
        tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|error| anyhow::anyhow!("cannot read '{rel}': {error}"))
    }
}
