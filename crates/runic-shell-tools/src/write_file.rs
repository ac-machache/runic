//! `write_file` — full-file overwrite via `StorageBackend::write`.
//!
//! Always writes the full content (no append, no diff). Caller decides
//! whether to read first; the tool doesn't enforce a read-before-write
//! pattern (that's a hook concern).

use std::sync::Arc;

use async_trait::async_trait;
use runic_storage_backend::StorageBackend;
use runic_tool_core::{Tool, ToolContext, ToolResult};
use serde_json::Value;

use crate::error::ShellToolError;
use crate::paths::normalise;

pub const DEFAULT_MAX_BYTES: usize = 1024 * 1024;

const DESCRIPTION: &str = "Write a file end-to-end, replacing any existing content.\n\
\n\
- Creates parent directories as needed.\n\
- The full body lands on disk — there's no append, no diff merge.\n\
- Refuses writes larger than the per-tool byte cap (default 1 MiB).";

pub struct WriteFileTool {
    storage: Arc<dyn StorageBackend>,
    max_bytes: usize,
}

impl WriteFileTool {
    pub fn new(storage: Arc<dyn StorageBackend>) -> Self {
        Self {
            storage,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    pub fn with_max_bytes(mut self, n: usize) -> Self {
        self.max_bytes = n;
        self
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Storage key to write." },
                "content": { "type": "string", "description": "The full body of the file." }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        match self.dispatch(input).await {
            Ok(s) => ToolResult::ok(s),
            Err(e) => ToolResult::error(e.to_string()),
        }
    }
}

impl WriteFileTool {
    async fn dispatch(&self, input: Value) -> Result<String, ShellToolError> {
        let raw_path = input
            .get("path")
            .and_then(Value::as_str)
            .ok_or(ShellToolError::MissingField { field: "path" })?;
        let path = normalise(Some(raw_path))?;
        if path.is_empty() {
            return Err(ShellToolError::InvalidPath {
                path: raw_path.to_string(),
                reason: "path must point to a file, not the root",
            });
        }
        let content = input
            .get("content")
            .and_then(Value::as_str)
            .ok_or(ShellToolError::MissingField { field: "content" })?;
        let bytes = content.as_bytes();
        if bytes.len() > self.max_bytes {
            return Err(ShellToolError::OverWriteCap {
                actual: bytes.len(),
                limit: self.max_bytes,
            });
        }
        self.storage
            .write(&path, bytes)
            .await
            .map_err(|e| ShellToolError::Storage(e.to_string()))?;
        Ok(format!("ok — wrote {} byte(s) to {}", bytes.len(), path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runic_storage_backend::MemoryBackend;

    fn ctx() -> ToolContext {
        ToolContext::new("s".into(), "r".into(), 0, Default::default())
    }

    #[tokio::test]
    async fn writes_and_round_trips() {
        let backend: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let tool = WriteFileTool::new(backend.clone());
        let r = tool
            .execute(
                serde_json::json!({"path": "notes.md", "content": "hello"}),
                &ctx(),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        let raw = backend.read_to_string("notes.md").await.unwrap();
        assert_eq!(raw, "hello");
    }

    #[tokio::test]
    async fn overwrites_existing_file() {
        let backend: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        backend.write("notes.md", b"old content").await.unwrap();
        let tool = WriteFileTool::new(backend.clone());
        tool.execute(
            serde_json::json!({"path": "notes.md", "content": "new content"}),
            &ctx(),
        )
        .await;
        assert_eq!(backend.read_to_string("notes.md").await.unwrap(), "new content");
    }

    #[tokio::test]
    async fn refuses_oversized_write() {
        let backend: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let tool = WriteFileTool::new(backend).with_max_bytes(10);
        let r = tool
            .execute(
                serde_json::json!({"path": "notes.md", "content": "this is more than ten bytes"}),
                &ctx(),
            )
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("cap"));
    }

    #[tokio::test]
    async fn empty_path_rejected() {
        let backend: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let tool = WriteFileTool::new(backend);
        let r = tool
            .execute(serde_json::json!({"path": "", "content": "x"}), &ctx())
            .await;
        assert!(r.is_error);
    }
}
