use std::sync::Arc;

use runic_hook::HookOutcome;
use runic_macros::hook;
use runic_provider::CompletionRequest;
use runic_state::AgentState;
use runic_store::ArtifactStore;
use runic_types::{ContentBlock, MessageContent, Source};

#[hook(kind = write, name = "artifact-resolver", at = before_model)]
#[derive(Clone)]
pub struct ArtifactResolver {
    artifacts: Arc<dyn ArtifactStore>,
}

impl ArtifactResolver {
    pub fn new(artifacts: Arc<dyn ArtifactStore>) -> Self {
        Self { artifacts }
    }

    async fn hook(&self, _state: &mut AgentState, request: &mut CompletionRequest) -> HookOutcome {
        let mut touched = false;
        for message in request.messages.iter_mut() {
            let MessageContent::Blocks(blocks) = &mut message.content else {
                continue;
            };
            for block in blocks.iter_mut() {
                let Some(id) = stored_id(block) else {
                    continue;
                };
                *block = self.resolve(block.clone(), &id).await;
                touched = true;
            }
        }

        match touched {
            true => HookOutcome::Continue,
            false => HookOutcome::Noop,
        }
    }

    async fn resolve(&self, block: ContentBlock, id: &str) -> ContentBlock {
        match self.source(id).await {
            Ok(source) => with_source(block, source),
            Err(error) => {
                tracing::warn!(artifact = %id, %error, "could not resolve an artifact for the model");
                ContentBlock::Text {
                    text: format!("[{} is unavailable: {error}]", describe(&block, id)),
                    provider_metadata: None,
                }
            }
        }
    }

    async fn source(&self, id: &str) -> anyhow::Result<Source> {
        match self.artifacts.url(id).await? {
            Some(url) => Ok(Source::Url(url)),
            None => Ok(Source::Inline(self.artifacts.get(id).await?)),
        }
    }
}

fn stored_id(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Image { source, .. } | ContentBlock::File { source, .. } => {
            source.stored().map(str::to_string)
        }
        _ => None,
    }
}

fn with_source(block: ContentBlock, resolved: Source) -> ContentBlock {
    match block {
        ContentBlock::Image {
            media_type,
            filename,
            ..
        } => ContentBlock::Image {
            media_type,
            filename,
            source: resolved,
        },
        ContentBlock::File {
            media_type,
            filename,
            ..
        } => ContentBlock::File {
            media_type,
            filename,
            source: resolved,
        },
        other => other,
    }
}

fn describe(block: &ContentBlock, id: &str) -> String {
    let (media_type, filename) = match block {
        ContentBlock::Image {
            media_type,
            filename,
            ..
        }
        | ContentBlock::File {
            media_type,
            filename,
            ..
        } => (media_type.as_str(), filename.as_deref()),
        _ => ("", None),
    };
    match filename {
        Some(name) => format!("{name} ({media_type}, {id})"),
        None => format!("{media_type} {id}"),
    }
}
