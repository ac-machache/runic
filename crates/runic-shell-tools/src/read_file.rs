//! `read_file` — paginated line-based read.
//!
//! Default behaviour matches deepagents: read up to 200 lines starting
//! at line 0, return them with `cat -n` style line numbers. Pagination
//! via `offset` + `limit` lets the agent skim a large file without
//! drowning its context.
//!
//! A separate byte cap kills runaway reads even when `limit` is huge.

use std::sync::Arc;

use async_trait::async_trait;
use runic_storage_backend::{StorageBackend, StorageError};
use runic_tool_core::{Tool, ToolContext, ToolResult};
use serde_json::Value;

use crate::error::ShellToolError;
use crate::paths::normalise;

pub const DEFAULT_MAX_LINES: usize = 200;
pub const DEFAULT_MAX_BYTES: usize = 200 * 1024;

const DESCRIPTION: &str = "Read a file from storage with pagination.\n\
\n\
- Default reads the first 200 lines.\n\
- For large files, paginate with `offset` (0-indexed line number) and `limit` (lines per page).\n\
- Output uses `cat -n` style — each line prefixed with its 1-indexed line number.\n\
- Errors if the file doesn't exist or isn't valid UTF-8.\n\
- Hard byte cap stops runaway reads; the response notes when content was truncated.";

pub struct ReadFileTool {
    storage: Arc<dyn StorageBackend>,
    max_lines: usize,
    max_bytes: usize,
}

impl ReadFileTool {
    pub fn new(storage: Arc<dyn StorageBackend>) -> Self {
        Self {
            storage,
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    pub fn with_max_lines(mut self, n: usize) -> Self {
        self.max_lines = n;
        self
    }

    pub fn with_max_bytes(mut self, n: usize) -> Self {
        self.max_bytes = n;
        self
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Storage key of the file to read (e.g. 'wikis/notes/index.md')."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "0-indexed line number to start reading from (default 0)."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum lines to return (default 200, hard cap enforced by the tool)."
                }
            },
            "required": ["path"],
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

impl ReadFileTool {
    async fn dispatch(&self, input: Value) -> Result<String, ShellToolError> {
        let path_raw = input
            .get("path")
            .and_then(Value::as_str)
            .ok_or(ShellToolError::MissingField { field: "path" })?;
        let path = normalise(Some(path_raw))?;
        if path.is_empty() {
            return Err(ShellToolError::InvalidPath {
                path: path_raw.to_string(),
                reason: "path must point to a file, not the root",
            });
        }
        let offset = parse_int(&input, "offset", 0)? as usize;
        let raw_limit = parse_int(&input, "limit", self.max_lines as i64)? as usize;
        let limit = raw_limit.clamp(1, self.max_lines);

        let raw_bytes = self
            .storage
            .read(&path)
            .await
            .map_err(|e| match e {
                StorageError::NotFound { .. } => ShellToolError::Storage(format!("not found: {path}")),
                other => ShellToolError::Storage(other.to_string()),
            })?;

        let truncated_bytes = raw_bytes.len() > self.max_bytes;
        let slice: &[u8] = if truncated_bytes {
            &raw_bytes[..self.max_bytes]
        } else {
            &raw_bytes
        };
        let text = std::str::from_utf8(slice)
            .map_err(|e| ShellToolError::NotUtf8(e.to_string()))?;

        let total_lines = text.lines().count();
        let selected: Vec<(usize, &str)> = text
            .lines()
            .enumerate()
            .skip(offset)
            .take(limit)
            .collect();

        let mut out = String::new();
        for (idx, line) in &selected {
            // `cat -n` style: 1-indexed line numbers, right-justified.
            out.push_str(&format!("{:>6}\t{}\n", idx + 1, line));
        }
        let lines_returned = selected.len();
        let next_offset = offset + lines_returned;
        let mut suffix = format!(
            "\n[{lines_returned} line(s); total visible {total_lines}; next offset {next_offset}]"
        );
        if truncated_bytes {
            suffix.push_str(&format!(
                " (BYTE-CAPPED at {} bytes — file is {} bytes)",
                self.max_bytes,
                raw_bytes.len()
            ));
        }
        out.push_str(&suffix);
        Ok(out)
    }
}

fn parse_int(input: &Value, field: &'static str, default: i64) -> Result<i64, ShellToolError> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(n)) => n.as_i64().ok_or(ShellToolError::InvalidValue {
            field,
            reason: "must be an integer".to_string(),
        }),
        Some(_) => Err(ShellToolError::InvalidValue {
            field,
            reason: "must be an integer".to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runic_storage_backend::MemoryBackend;

    fn ctx() -> ToolContext {
        ToolContext::new("s".into(), "r".into(), 0, Default::default())
    }

    async fn store_with(key: &str, value: &str) -> ReadFileTool {
        let backend: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        backend.write(key, value.as_bytes()).await.unwrap();
        ReadFileTool::new(backend)
    }

    #[tokio::test]
    async fn reads_small_file_with_line_numbers() {
        let tool = store_with("notes.md", "first\nsecond\nthird").await;
        let r = tool
            .execute(serde_json::json!({"path": "notes.md"}), &ctx())
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("     1\tfirst"));
        assert!(r.content.contains("     2\tsecond"));
        assert!(r.content.contains("     3\tthird"));
    }

    #[tokio::test]
    async fn offset_and_limit_paginate() {
        let body = (1..=10).map(|n| format!("line{n}")).collect::<Vec<_>>().join("\n");
        let tool = store_with("big.md", &body).await;
        let r = tool
            .execute(
                serde_json::json!({"path": "big.md", "offset": 3, "limit": 2}),
                &ctx(),
            )
            .await;
        assert!(!r.is_error);
        assert!(r.content.contains("     4\tline4"));
        assert!(r.content.contains("     5\tline5"));
        assert!(!r.content.contains("line3"));
        assert!(!r.content.contains("line6"));
        assert!(r.content.contains("next offset 5"));
    }

    #[tokio::test]
    async fn limit_is_clamped_to_max() {
        let body = (1..=300).map(|n| format!("l{n}")).collect::<Vec<_>>().join("\n");
        let tool = store_with("huge.md", &body).await.with_max_lines(50);
        let r = tool
            .execute(
                serde_json::json!({"path": "huge.md", "limit": 9999}),
                &ctx(),
            )
            .await;
        assert!(!r.is_error);
        assert!(r.content.contains("50 line(s)"));
    }

    #[tokio::test]
    async fn byte_cap_truncates_and_notes() {
        let tool = store_with("big.md", "lots of bytes here").await.with_max_bytes(5);
        let r = tool
            .execute(serde_json::json!({"path": "big.md"}), &ctx())
            .await;
        assert!(!r.is_error);
        assert!(r.content.contains("BYTE-CAPPED"));
    }

    #[tokio::test]
    async fn missing_file_is_a_clear_error() {
        let tool = store_with("a.md", "x").await;
        let r = tool
            .execute(serde_json::json!({"path": "missing.md"}), &ctx())
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("not found"));
    }

    #[tokio::test]
    async fn parent_segment_rejected() {
        let tool = store_with("a.md", "x").await;
        let r = tool
            .execute(serde_json::json!({"path": "../etc/passwd"}), &ctx())
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("'..'"));
    }

    #[tokio::test]
    async fn empty_path_rejected() {
        let tool = store_with("a.md", "x").await;
        let r = tool
            .execute(serde_json::json!({"path": ""}), &ctx())
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("root"));
    }
}
