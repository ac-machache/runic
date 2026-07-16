use std::sync::Arc;

use async_trait::async_trait;
use runic_agent::{SpilledArtifact, ToolOutputSpill};
use runic_substrate::{ArtifactSource, ArtifactStore};

pub struct SpillToArtifacts {
    store: Arc<dyn ArtifactStore>,
}

impl SpillToArtifacts {
    pub fn new(store: Arc<dyn ArtifactStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl ToolOutputSpill for SpillToArtifacts {
    async fn store(
        &self,
        tenant: &str,
        session: &str,
        mime: &str,
        bytes: &[u8],
    ) -> anyhow::Result<SpilledArtifact> {
        let artifact = self
            .store
            .put(tenant, session, mime, ArtifactSource::ToolOutput, bytes)
            .await?;
        Ok(SpilledArtifact {
            id: artifact.id,
            mime: artifact.mime_type,
            size: artifact.size,
        })
    }
}
