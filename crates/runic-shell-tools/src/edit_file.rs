//! `edit_file` — string find/replace with a unique-match guard.
//!
//! Mirrors the Claude Code `Edit` tool semantics: `old_string` must
//! appear EXACTLY ONCE in the file unless `replace_all` is true. The
//! guard exists because the agent can't reliably tell which occurrence
//! of an ambiguous match it wants; we'd rather hard-fail than rewrite
//! the wrong one.

use std::sync::Arc;

use async_trait::async_trait;
use runic_storage_backend::{StorageBackend, StorageError};
use runic_tool_core::{Tool, ToolContext, ToolResult};
use serde_json::Value;

use crate::error::ShellToolError;
use crate::paths::normalise;

const DESCRIPTION: &str = "Edit a file by replacing one or more occurrences of `old_string` with `new_string`.\n\
\n\
- Default mode: `old_string` must match EXACTLY ONCE — error otherwise (no silent rewrite of the wrong occurrence).\n\
- Set `replace_all=true` to apply to every match.\n\
- The file must already exist; this tool does not create new files (use `write_file` for that).\n\
- `old_string` must be different from `new_string`.\n\
- Returns the number of replacements applied.";

pub struct EditFileTool {
    storage: Arc<dyn StorageBackend>,
}

impl EditFileTool {
    pub fn new(storage: Arc<dyn StorageBackend>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Storage key of the file to edit." },
                "old_string": { "type": "string", "description": "Exact substring to find. Must be unique unless replace_all is true." },
                "new_string": { "type": "string", "description": "Replacement text. Must differ from old_string." },
                "replace_all": { "type": "boolean", "description": "When true, replace every occurrence. Default false." }
            },
            "required": ["path", "old_string", "new_string"],
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

impl EditFileTool {
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
        let old = input
            .get("old_string")
            .and_then(Value::as_str)
            .ok_or(ShellToolError::MissingField { field: "old_string" })?;
        let new = input
            .get("new_string")
            .and_then(Value::as_str)
            .ok_or(ShellToolError::MissingField { field: "new_string" })?;
        if old.is_empty() {
            return Err(ShellToolError::InvalidValue {
                field: "old_string",
                reason: "must not be empty".to_string(),
            });
        }
        if old == new {
            return Err(ShellToolError::InvalidValue {
                field: "new_string",
                reason: "must differ from old_string".to_string(),
            });
        }
        let replace_all = input
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let raw = self
            .storage
            .read_to_string(&path)
            .await
            .map_err(|e| match e {
                StorageError::NotFound { .. } => ShellToolError::Storage(format!("not found: {path}")),
                other => ShellToolError::Storage(other.to_string()),
            })?;

        let match_count = raw.matches(old).count();
        match match_count {
            0 => return Err(ShellToolError::NoMatch),
            1 => {}
            n if !replace_all => return Err(ShellToolError::Ambiguous { count: n }),
            _ => {}
        }

        let updated = if replace_all {
            raw.replace(old, new)
        } else {
            raw.replacen(old, new, 1)
        };

        self.storage
            .write(&path, updated.as_bytes())
            .await
            .map_err(|e| ShellToolError::Storage(e.to_string()))?;
        Ok(format!(
            "ok — replaced {} occurrence(s) in {path}",
            if replace_all { match_count } else { 1 }
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runic_storage_backend::MemoryBackend;

    fn ctx() -> ToolContext {
        ToolContext::new("s".into(), "r".into(), 0, Default::default())
    }

    async fn seed(content: &str) -> (Arc<dyn StorageBackend>, EditFileTool) {
        let backend: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        backend.write("notes.md", content.as_bytes()).await.unwrap();
        let tool = EditFileTool::new(backend.clone());
        (backend, tool)
    }

    #[tokio::test]
    async fn unique_match_replaces_once() {
        let (backend, tool) = seed("hello there world").await;
        let r = tool
            .execute(
                serde_json::json!({
                    "path": "notes.md",
                    "old_string": "there",
                    "new_string": "BIG"
                }),
                &ctx(),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            backend.read_to_string("notes.md").await.unwrap(),
            "hello BIG world"
        );
    }

    #[tokio::test]
    async fn ambiguous_match_errors_unless_replace_all() {
        let (backend, tool) = seed("foo bar foo bar foo").await;
        let r = tool
            .execute(
                serde_json::json!({
                    "path": "notes.md",
                    "old_string": "foo",
                    "new_string": "X"
                }),
                &ctx(),
            )
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("3"));
        // File unchanged.
        assert_eq!(
            backend.read_to_string("notes.md").await.unwrap(),
            "foo bar foo bar foo"
        );
    }

    #[tokio::test]
    async fn replace_all_replaces_every_occurrence() {
        let (backend, tool) = seed("foo bar foo bar foo").await;
        let r = tool
            .execute(
                serde_json::json!({
                    "path": "notes.md",
                    "old_string": "foo",
                    "new_string": "X",
                    "replace_all": true
                }),
                &ctx(),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            backend.read_to_string("notes.md").await.unwrap(),
            "X bar X bar X"
        );
        assert!(r.content.contains("3 occurrence"));
    }

    #[tokio::test]
    async fn no_match_errors() {
        let (_, tool) = seed("hello").await;
        let r = tool
            .execute(
                serde_json::json!({
                    "path": "notes.md",
                    "old_string": "absent",
                    "new_string": "x"
                }),
                &ctx(),
            )
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("no entry") || r.content.contains("old_string"));
    }

    #[tokio::test]
    async fn missing_file_errors() {
        let backend: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let tool = EditFileTool::new(backend);
        let r = tool
            .execute(
                serde_json::json!({
                    "path": "nope.md",
                    "old_string": "x",
                    "new_string": "y"
                }),
                &ctx(),
            )
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("not found"));
    }

    #[tokio::test]
    async fn empty_old_string_rejected() {
        let (_, tool) = seed("hi").await;
        let r = tool
            .execute(
                serde_json::json!({
                    "path": "notes.md",
                    "old_string": "",
                    "new_string": "x"
                }),
                &ctx(),
            )
            .await;
        assert!(r.is_error);
    }

    #[tokio::test]
    async fn equal_old_and_new_rejected() {
        let (_, tool) = seed("hi").await;
        let r = tool
            .execute(
                serde_json::json!({
                    "path": "notes.md",
                    "old_string": "hi",
                    "new_string": "hi"
                }),
                &ctx(),
            )
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("differ"));
    }
}
