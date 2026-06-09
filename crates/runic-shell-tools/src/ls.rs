//! `ls` — list one storage directory level.
//!
//! Files and directories come back in a stable alphabetical order with
//! `[F]` / `[D]` prefixes so the model can tell them apart at a glance.
//! Default path is the backend root.

use std::sync::Arc;

use async_trait::async_trait;
use runic_storage_backend::{EntryKind, StorageBackend};
use runic_tool_core::{Tool, ToolContext, ToolResult};
use serde_json::Value;

use crate::error::ShellToolError;
use crate::paths::normalise;

pub const DEFAULT_MAX_ENTRIES: usize = 500;

const DESCRIPTION: &str = "List entries one level under a directory in storage.\n\
\n\
- `path` is optional — omit it to list the backend's root.\n\
- Output: one entry per line, `[D]` for directories and `[F]` for files, alphabetically sorted, with file sizes when known.\n\
- Caps the listing to keep responses bounded; truncation is reported in the trailing summary.";

pub struct LsTool {
    storage: Arc<dyn StorageBackend>,
    max_entries: usize,
}

impl LsTool {
    pub fn new(storage: Arc<dyn StorageBackend>) -> Self {
        Self {
            storage,
            max_entries: DEFAULT_MAX_ENTRIES,
        }
    }

    pub fn with_max_entries(mut self, n: usize) -> Self {
        self.max_entries = n;
        self
    }
}

#[async_trait]
impl Tool for LsTool {
    fn name(&self) -> &str {
        "ls"
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
                    "description": "Storage key of the directory to list. Omit or empty = root."
                }
            },
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

impl LsTool {
    async fn dispatch(&self, input: Value) -> Result<String, ShellToolError> {
        let path_raw = input.get("path").and_then(Value::as_str);
        let path = normalise(path_raw)?;
        let entries = self
            .storage
            .list(&path)
            .await
            .map_err(|e| ShellToolError::Storage(e.to_string()))?;

        // Backends differ. `LocalFsBackend` returns one Entry per direct
        // child with `Directory` / `File` kinds. `MemoryBackend` (flat
        // KV) returns Entries with full keys for every file under the
        // prefix. We normalise to one shape: "what does the agent see
        // at this directory level?" — synthesizing Directory rows on
        // flat-KV backends.
        let prefix = if path.is_empty() { String::new() } else { format!("{path}/") };
        let mut dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut files: std::collections::BTreeMap<String, Option<u64>> =
            std::collections::BTreeMap::new();

        for entry in entries {
            let relative = entry.key.strip_prefix(&prefix).unwrap_or(&entry.key);
            if relative.is_empty() {
                continue;
            }
            match entry.kind {
                EntryKind::Directory => {
                    let leaf = relative.split('/').next().unwrap_or(relative);
                    dirs.insert(leaf.to_string());
                }
                EntryKind::File => {
                    if let Some(slash) = relative.find('/') {
                        // Deeper than one level → its first segment is a synthesized dir.
                        dirs.insert(relative[..slash].to_string());
                    } else {
                        files.insert(relative.to_string(), entry.size);
                    }
                }
            }
        }

        let total = dirs.len() + files.len();
        if total == 0 {
            return Ok(format!(
                "(empty — no entries under '{}')",
                if path.is_empty() { "/" } else { &path }
            ));
        }

        // Build rows in alphabetical order across both kinds.
        #[derive(Eq, PartialEq)]
        struct Row {
            name: String,
            is_dir: bool,
            size: Option<u64>,
        }
        let mut rows: Vec<Row> = dirs
            .into_iter()
            .map(|name| Row { name, is_dir: true, size: None })
            .chain(
                files
                    .into_iter()
                    .map(|(name, size)| Row { name, is_dir: false, size }),
            )
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));

        let truncated = rows.len() > self.max_entries;
        if truncated {
            rows.truncate(self.max_entries);
        }

        let mut out = String::new();
        for row in &rows {
            let tag = if row.is_dir { "[D]" } else { "[F]" };
            let size = match row.size {
                Some(n) if !row.is_dir => format!(" ({n} B)"),
                _ => String::new(),
            };
            out.push_str(&format!("{tag} {}{}\n", row.name, size));
        }
        let trailer = if truncated {
            format!(
                "\n[{} of {} entries shown — listing capped at {}]",
                rows.len(),
                total,
                self.max_entries
            )
        } else {
            format!("\n[{} entries]", rows.len())
        };
        out.push_str(&trailer);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runic_storage_backend::MemoryBackend;

    fn ctx() -> ToolContext {
        ToolContext::new("s".into(), "r".into(), 0, Default::default())
    }

    async fn seeded() -> Arc<dyn StorageBackend> {
        let backend: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        backend.write("a.md", b"one").await.unwrap();
        backend.write("b.md", b"two").await.unwrap();
        backend.write("dir/c.md", b"three").await.unwrap();
        backend
    }

    #[tokio::test]
    async fn lists_root_when_path_omitted() {
        let tool = LsTool::new(seeded().await);
        let r = tool.execute(serde_json::json!({}), &ctx()).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("a.md"));
        assert!(r.content.contains("b.md"));
        assert!(r.content.contains("dir"));
    }

    #[tokio::test]
    async fn distinguishes_files_and_dirs() {
        let tool = LsTool::new(seeded().await);
        let r = tool.execute(serde_json::json!({}), &ctx()).await;
        assert!(r.content.contains("[F] a.md"));
        assert!(r.content.contains("[D] dir"));
    }

    #[tokio::test]
    async fn empty_dir_yields_friendly_message() {
        let backend: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let tool = LsTool::new(backend);
        let r = tool.execute(serde_json::json!({}), &ctx()).await;
        assert!(!r.is_error);
        assert!(r.content.contains("empty"));
    }

    #[tokio::test]
    async fn listing_is_capped() {
        let backend: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        for n in 0..10 {
            backend
                .write(&format!("f{n:02}.md"), b"x")
                .await
                .unwrap();
        }
        let tool = LsTool::new(backend).with_max_entries(3);
        let r = tool.execute(serde_json::json!({}), &ctx()).await;
        assert!(!r.is_error);
        assert!(r.content.contains("capped"));
        assert!(r.content.contains("3 of 10"));
    }
}
