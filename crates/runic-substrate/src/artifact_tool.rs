//! `read_thread_artifact` — let the agent re-read a file the user already
//! uploaded **in this same thread**. Ownership is enforced from the
//! [`ToolContext`] (`user_id` is the tenant, `session_id` is the thread), never
//! the args, so the agent can't reach another thread's or tenant's artifacts.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;

use runic_tool::{Tool, ToolContext, ToolResult};

use crate::ArtifactStore;

/// Reads a thread's own artifacts via an [`ArtifactStore`].
pub struct ReadThreadArtifactTool {
    artifacts: Arc<dyn ArtifactStore>,
}

impl ReadThreadArtifactTool {
    pub fn new(artifacts: Arc<dyn ArtifactStore>) -> Self {
        Self { artifacts }
    }
}

/// MIME types we can hand back to the model as plain text.
fn is_textual(mime: &str) -> bool {
    mime.starts_with("text/")
        || matches!(
            mime,
            "application/json" | "application/xml" | "application/csv" | "application/yaml"
        )
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
}

#[async_trait]
impl Tool for ReadThreadArtifactTool {
    fn name(&self) -> &str {
        "read_thread_artifact"
    }

    fn description(&self) -> &str {
        "Read a file the user already uploaded in THIS thread. Use it only when \
         you need to inspect that file again. You cannot read arbitrary local \
         paths, URLs, or artifacts from other threads — the artifact_id must \
         come from a file reference in this thread."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "artifact_id": {
                    "type": "string",
                    "description": "The id (art-…) of a file uploaded in this thread."
                }
            },
            "required": ["artifact_id"]
        })
    }

    fn parallelizable(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let artifact_id = args
            .get("artifact_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if artifact_id.is_empty() {
            return Ok(ToolResult::error(
                "read_thread_artifact requires an artifact_id",
            ));
        }

        // Ownership: the id must belong to (tenant, thread) from the context.
        let tenant = &ctx.user_id;
        let owned = match self.artifacts.list(tenant, &ctx.session_id).await {
            Ok(list) => list,
            Err(e) => return Ok(ToolResult::error(format!("artifact lookup failed: {e}"))),
        };
        let Some(meta) = owned.into_iter().find(|a| a.id == artifact_id) else {
            return Ok(ToolResult::error(
                "unknown artifact id for this thread — it must reference a file uploaded here",
            ));
        };

        let bytes = match self.artifacts.get(artifact_id).await {
            Ok(b) => b,
            Err(e) => return Ok(ToolResult::error(format!("could not read artifact: {e}"))),
        };

        // Full content to the model; only a summary to the event log — re-reading
        // a file must never write its bytes back into history.
        let summary = format!(
            "read_thread_artifact returned {} ({}, {} bytes); content omitted from log.",
            meta.id, meta.mime_type, meta.size
        );
        let full = if is_textual(&meta.mime_type) {
            match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    return Ok(ToolResult::error(
                        "artifact is not valid UTF-8 text despite its media type",
                    ));
                }
            }
        } else {
            let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
            format!(
                "Artifact {} ({}, {} bytes), base64:\n{}",
                meta.id, meta.mime_type, meta.size, data
            )
        };
        Ok(ToolResult::ok(full).with_persisted_summary(summary))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ArtifactSource, MemoryArtifactStore};

    #[tokio::test]
    async fn reads_text_artifact_from_this_thread() {
        let store = Arc::new(MemoryArtifactStore::new());
        let a = store
            .put(
                "acme",
                "thread1",
                "text/plain",
                ArtifactSource::UserUpload,
                b"hello notes",
            )
            .await
            .unwrap();

        let tool = ReadThreadArtifactTool::new(store);
        let ctx = ToolContext::new("acme", "thread1", "run1");
        let r = tool
            .execute(serde_json::json!({ "artifact_id": a.id }), &ctx)
            .await
            .unwrap();
        assert!(r.success);
        assert_eq!(r.output, "hello notes");
    }

    #[tokio::test]
    async fn binary_artifact_returns_base64_to_model_but_summary_to_log() {
        let store = Arc::new(MemoryArtifactStore::new());
        let a = store
            .put(
                "acme",
                "thread1",
                "application/pdf",
                ArtifactSource::UserUpload,
                b"\x89\x01\x02PDFbytes",
            )
            .await
            .unwrap();

        let tool = ReadThreadArtifactTool::new(store);
        let ctx = ToolContext::new("acme", "thread1", "run1");
        let r = tool
            .execute(serde_json::json!({ "artifact_id": a.id }), &ctx)
            .await
            .unwrap();
        assert!(r.success);
        // Full bytes (base64) go to the model …
        assert!(r.output.contains("base64"));
        assert!(r.output.contains("application/pdf"));
        // … a summary (no bytes) is what gets persisted.
        let persisted = r.persisted_output.expect("a persisted summary");
        assert!(persisted.contains("omitted from log"));
        assert!(!persisted.contains("base64"));
    }

    #[tokio::test]
    async fn rejects_cross_thread_and_cross_tenant_ids() {
        let store = Arc::new(MemoryArtifactStore::new());
        let other = store
            .put(
                "acme",
                "thread2",
                "text/plain",
                ArtifactSource::UserUpload,
                b"secret",
            )
            .await
            .unwrap();
        let foreign = store
            .put(
                "evil",
                "thread1",
                "text/plain",
                ArtifactSource::UserUpload,
                b"theirs",
            )
            .await
            .unwrap();

        let tool = ReadThreadArtifactTool::new(store);
        let ctx = ToolContext::new("acme", "thread1", "run1");

        // belongs to another thread (same tenant) → rejected, bytes never read
        let r = tool
            .execute(serde_json::json!({ "artifact_id": other.id }), &ctx)
            .await
            .unwrap();
        assert!(!r.success);

        // belongs to another tenant → rejected
        let r = tool
            .execute(serde_json::json!({ "artifact_id": foreign.id }), &ctx)
            .await
            .unwrap();
        assert!(!r.success);

        // unknown id → rejected
        let r = tool
            .execute(serde_json::json!({ "artifact_id": "art-nope" }), &ctx)
            .await
            .unwrap();
        assert!(!r.success);
    }
}
