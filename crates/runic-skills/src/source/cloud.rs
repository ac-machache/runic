use std::sync::Arc;

use async_trait::async_trait;
use opendal::Operator;

use super::def::SkillSource;
use crate::security::{self, MAX_SKILLS};

pub struct CloudSource {
    operator: Operator,
}

pub fn from_operator(operator: Operator) -> Arc<dyn SkillSource> {
    Arc::new(CloudSource { operator })
}

#[cfg(feature = "s3")]
pub fn s3(bucket: &str, prefix: &str) -> anyhow::Result<Arc<dyn SkillSource>> {
    let builder = opendal::services::S3::default().bucket(bucket).root(prefix);
    Ok(from_operator(Operator::new(builder)?))
}

#[cfg(feature = "gcs")]
pub fn gcs(bucket: &str, prefix: &str) -> anyhow::Result<Arc<dyn SkillSource>> {
    let builder = opendal::services::Gcs::default()
        .bucket(bucket)
        .root(prefix);
    Ok(from_operator(Operator::new(builder)?))
}

#[cfg(feature = "azblob")]
pub fn azblob(container: &str, prefix: &str) -> anyhow::Result<Arc<dyn SkillSource>> {
    let builder = opendal::services::Azblob::default()
        .container(container)
        .root(prefix);
    Ok(from_operator(Operator::new(builder)?))
}

#[async_trait]
impl SkillSource for CloudSource {
    async fn entries(&self) -> anyhow::Result<Vec<String>> {
        let listed = self.operator.list_with("").recursive(false).await?;

        let mut entries = Vec::new();
        for entry in listed {
            if entries.len() >= MAX_SKILLS {
                tracing::warn!(
                    cap = MAX_SKILLS,
                    "skills cap reached — remaining entries skipped"
                );
                break;
            }
            if !entry.metadata().is_dir() {
                continue;
            }
            let name = entry.name().trim_end_matches('/').to_string();
            if name.is_empty() || name.starts_with('.') {
                continue;
            }
            entries.push(name);
        }
        Ok(entries)
    }

    async fn read(&self, rel: &str) -> anyhow::Result<String> {
        security::safe_rel(rel)?;
        let bytes = self.operator.read(rel).await?;
        Ok(String::from_utf8(bytes.to_vec())?)
    }
}
