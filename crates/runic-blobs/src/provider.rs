//! `BlobMaterializingProvider` — a decorator that resolves
//! [`ContentBlock::Blob`] references to inline [`ContentBlock::Image`]
//! blocks before forwarding to an underlying [`Provider`].
//!
//! Wraps any provider; resolver fetches bytes from a [`crate::BlobStore`]
//! via [`crate::BlobResolver`] and base64-encodes them in place.
//! Provider-agnostic: works with Anthropic, Gemini, anything else
//! implementing `Provider`.
//!
//! Failures during resolution are logged and the block is DROPPED from
//! the materialized message. The original messages remain intact for
//! state-keeping; only the per-turn provider input is rewritten.

use async_trait::async_trait;
use base64::Engine;
use runic_message_types::{ContentBlock, Message, ToolDefinition};
use runic_provider_core::{EventStream, Provider, ProviderError};
use std::sync::Arc;
use tracing::warn;

use crate::resolver::BlobResolver;

/// Decorator provider that materializes blob references inline before
/// forwarding to the wrapped provider.
pub struct BlobMaterializingProvider {
    inner: Arc<dyn Provider>,
    resolver: Arc<dyn BlobResolver>,
}

impl BlobMaterializingProvider {
    pub fn new(inner: Arc<dyn Provider>, resolver: Arc<dyn BlobResolver>) -> Self {
        Self { inner, resolver }
    }

    /// Walk every message; replace each `ContentBlock::Blob` with an
    /// equivalent `ContentBlock::Image` containing the base64-encoded
    /// bytes. Other content blocks pass through untouched. Resolution
    /// failures drop the blob and log a warning.
    async fn materialize(&self, messages: &[Message]) -> Vec<Message> {
        let mut out = Vec::with_capacity(messages.len());
        for msg in messages {
            let mut new_content = Vec::with_capacity(msg.content.len());
            for block in &msg.content {
                match block {
                    ContentBlock::Blob(b) => {
                        match self.resolver.resolve(&b.id).await {
                            Ok(bytes) => {
                                let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                                new_content.push(ContentBlock::Image {
                                    media_type: b.mime.clone(),
                                    data,
                                });
                            }
                            Err(err) => {
                                warn!(
                                    blob_id = %b.id,
                                    error = %err,
                                    "blob materialization failed; dropping block"
                                );
                            }
                        }
                    }
                    other => new_content.push(other.clone()),
                }
            }
            out.push(Message {
                role: msg.role.clone(),
                content: new_content,
                timestamp: msg.timestamp,
                tool_duration_ms: msg.tool_duration_ms,
            });
        }
        out
    }
}

#[async_trait]
impl Provider for BlobMaterializingProvider {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        system: &str,
        resume_session_id: Option<&str>,
    ) -> Result<EventStream, ProviderError> {
        let materialized = self.materialize(messages).await;
        self.inner
            .complete(&materialized, tools, system, resume_session_id)
            .await
    }

    async fn complete_split(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        system_static: &str,
        system_dynamic: &str,
        resume_session_id: Option<&str>,
    ) -> Result<EventStream, ProviderError> {
        let materialized = self.materialize(messages).await;
        self.inner
            .complete_split(
                &materialized,
                tools,
                system_static,
                system_dynamic,
                resume_session_id,
            )
            .await
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn model(&self) -> String {
        self.inner.model()
    }

    fn supports_image_input(&self) -> bool {
        // We turn blobs into images — so this is effectively true for
        // any inner provider regardless of what it claims.
        true
    }

    fn available_models(&self) -> Vec<&'static str> {
        self.inner.available_models()
    }

    fn set_model(&self, model: &str) -> Result<(), ProviderError> {
        self.inner.set_model(model)
    }

    fn context_window(&self) -> usize {
        self.inner.context_window()
    }

    fn fork(&self) -> Arc<dyn Provider> {
        // Forking a materializing provider produces another materializing
        // provider over a forked inner — same resolver (it's read-only).
        Arc::new(BlobMaterializingProvider {
            inner: self.inner.fork(),
            resolver: self.resolver.clone(),
        })
    }
}

impl std::fmt::Debug for BlobMaterializingProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobMaterializingProvider")
            .field("inner_model", &self.inner.model())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlobInput, BlobStore, BlobStoreResolver, FileBlobStore};
    use futures::stream;
    use runic_message_types::{BlobRef, Role, StreamEvent};
    use runic_provider_core::{EventStream, ProviderError};
    use runic_storage_backend::{MemoryBackend, StorageBackend};
    use std::sync::Mutex;

    /// Provider stub that records exactly what messages it received,
    /// so we can assert on the post-materialization shape.
    struct CapturingProvider {
        captured: Mutex<Vec<Message>>,
    }

    #[async_trait]
    impl Provider for CapturingProvider {
        async fn complete(
            &self,
            messages: &[Message],
            _tools: &[ToolDefinition],
            _system: &str,
            _resume: Option<&str>,
        ) -> Result<EventStream, ProviderError> {
            *self.captured.lock().unwrap() = messages.to_vec();
            let events = vec![Ok(StreamEvent::MessageEnd {
                stop_reason: Some("end_turn".into()),
            })];
            Ok(Box::pin(stream::iter(events)))
        }
        fn name(&self) -> &str {
            "capturing"
        }
        fn model(&self) -> String {
            "capturing-1".into()
        }
        fn fork(&self) -> Arc<dyn Provider> {
            Arc::new(CapturingProvider {
                captured: Mutex::new(Vec::new()),
            })
        }
    }

    async fn fresh_store() -> (Arc<dyn BlobStore>, Arc<dyn BlobResolver>) {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let store: Arc<dyn BlobStore> = Arc::new(FileBlobStore::new(storage));
        let resolver: Arc<dyn BlobResolver> =
            Arc::new(BlobStoreResolver::new(store.clone(), "alice"));
        (store, resolver)
    }

    #[tokio::test]
    async fn messages_without_blobs_pass_through_unchanged() {
        let (_store, resolver) = fresh_store().await;
        let inner = Arc::new(CapturingProvider {
            captured: Mutex::new(Vec::new()),
        });
        let p = BlobMaterializingProvider::new(inner.clone(), resolver);

        let messages = vec![Message::user("hello")];
        let _ = p.complete(&messages, &[], "system", None).await.unwrap();
        let captured = inner.captured.lock().unwrap().clone();
        assert_eq!(captured.len(), 1);
        match &captured[0].content[0] {
            ContentBlock::Text { text, .. } => assert_eq!(text, "hello"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn blob_blocks_get_materialized_to_image_blocks() {
        let (store, resolver) = fresh_store().await;
        let payload = b"\x89PNG\x0d\x0a\x1a\x0a".to_vec(); // fake PNG header
        let r = store
            .put("alice", BlobInput::new(payload.clone(), "image/png"))
            .await
            .unwrap();

        let inner = Arc::new(CapturingProvider {
            captured: Mutex::new(Vec::new()),
        });
        let p = BlobMaterializingProvider::new(inner.clone(), resolver);

        let messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Blob(r.clone()),
                ContentBlock::Text {
                    text: "what is this?".into(),
                    cache_control: None,
                },
            ],
            timestamp: None,
            tool_duration_ms: None,
        }];

        let _ = p.complete(&messages, &[], "", None).await.unwrap();
        let captured = inner.captured.lock().unwrap().clone();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].content.len(), 2);
        match &captured[0].content[0] {
            ContentBlock::Image { media_type, data } => {
                assert_eq!(media_type, "image/png");
                let expected = base64::engine::general_purpose::STANDARD.encode(&payload);
                assert_eq!(data, &expected);
            }
            other => panic!("expected Image, got {other:?}"),
        }
        // Text block should be unchanged.
        match &captured[0].content[1] {
            ContentBlock::Text { text, .. } => assert_eq!(text, "what is this?"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unresolvable_blob_is_silently_dropped() {
        let (_store, resolver) = fresh_store().await;
        let inner = Arc::new(CapturingProvider {
            captured: Mutex::new(Vec::new()),
        });
        let p = BlobMaterializingProvider::new(inner.clone(), resolver);

        let messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Blob(BlobRef {
                    id: "nonexistent".into(),
                    mime: "image/png".into(),
                    size: 100,
                    name: None,
                }),
                ContentBlock::Text {
                    text: "fallback text".into(),
                    cache_control: None,
                },
            ],
            timestamp: None,
            tool_duration_ms: None,
        }];

        let _ = p.complete(&messages, &[], "", None).await.unwrap();
        let captured = inner.captured.lock().unwrap().clone();
        // Blob dropped; Text preserved.
        assert_eq!(captured[0].content.len(), 1);
        match &captured[0].content[0] {
            ContentBlock::Text { text, .. } => assert_eq!(text, "fallback text"),
            other => panic!("expected Text, got {other:?}"),
        }
    }
}
