//! Resolves `ArtifactRef` pointers into bytes before the model call. The event
//! log keeps only the reference; bytes ride one transient request and are never
//! re-persisted. Runs in the agent loop ahead of primary/fallback dispatch.
//!
//! Latest-user refs must reach the model, so a missing/foreign/unreadable one
//! fails the run. Older refs degrade to a `read_thread_artifact` reminder.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use runic_agent::MediaResolver;
use runic_provider::CompletionRequest;
use runic_substrate::{Artifact, ArtifactStore};
use runic_types::{ContentBlock, MessageContent, Role};

pub struct ArtifactResolver {
    artifacts: Arc<dyn ArtifactStore>,
    tenant: String,
    session_id: String,
}

impl ArtifactResolver {
    pub fn new(
        artifacts: Arc<dyn ArtifactStore>,
        tenant: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            artifacts,
            tenant: tenant.into(),
            session_id: session_id.into(),
        }
    }

    async fn resolve_one(
        &self,
        id: String,
        filename: Option<String>,
        owned: &HashMap<String, Artifact>,
        is_latest_user: bool,
    ) -> Result<ContentBlock, String> {
        let meta = owned.get(&id);
        if is_latest_user {
            let meta = meta
                .ok_or_else(|| "a referenced file is not available in this thread".to_string())?;
            let bytes = self.artifacts.get(&id).await.map_err(|e| {
                tracing::warn!(artifact = %id, error = %e, "artifact fetch failed");
                "a referenced file could not be loaded".to_string()
            })?;
            let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let media_type = meta.mime_type.clone();
            return Ok(if media_type.starts_with("image/") {
                ContentBlock::Image { media_type, data }
            } else {
                ContentBlock::File { media_type, data }
            });
        }
        match meta {
            Some(meta) => {
                let name = filename.unwrap_or_else(|| id.clone());
                Ok(text(format!(
                    "File previously uploaded in this thread — id: {id}, filename: {name}, media type: {}. \
                     Call read_thread_artifact with this id if you need to inspect it again.",
                    meta.mime_type
                )))
            }
            None => Ok(text(
                "A file referenced here is not available in this thread.",
            )),
        }
    }
}

fn text(s: impl Into<String>) -> ContentBlock {
    ContentBlock::Text {
        text: s.into(),
        provider_metadata: None,
    }
}

fn has_ref(content: &MessageContent) -> bool {
    matches!(content, MessageContent::Blocks(b)
        if b.iter().any(|c| matches!(c, ContentBlock::ArtifactRef { .. })))
}

#[async_trait]
impl MediaResolver for ArtifactResolver {
    async fn resolve(&self, request: &mut CompletionRequest) -> Result<(), String> {
        if !request.messages.iter().any(|m| has_ref(&m.content)) {
            return Ok(());
        }

        let latest_user = request.messages.iter().rposition(|m| m.role == Role::User);

        // A list failure leaves `owned` empty → latest-user refs fail closed.
        let owned: HashMap<String, Artifact> =
            match self.artifacts.list(&self.tenant, &self.session_id).await {
                Ok(arts) => arts.into_iter().map(|a| (a.id.clone(), a)).collect(),
                Err(e) => {
                    tracing::warn!(error = %e, "artifact list failed");
                    HashMap::new()
                }
            };

        for (i, msg) in request.messages.iter_mut().enumerate() {
            let MessageContent::Blocks(blocks) = &mut msg.content else {
                continue;
            };
            if !blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ArtifactRef { .. }))
            {
                continue;
            }
            let is_latest_user = Some(i) == latest_user;
            let mut rebuilt = Vec::with_capacity(blocks.len());
            for block in std::mem::take(blocks) {
                match block {
                    ContentBlock::ArtifactRef { id, filename, .. } => {
                        rebuilt.push(
                            self.resolve_one(id, filename, &owned, is_latest_user)
                                .await?,
                        );
                    }
                    other => rebuilt.push(other),
                }
            }
            *blocks = rebuilt;
        }
        Ok(())
    }
}
