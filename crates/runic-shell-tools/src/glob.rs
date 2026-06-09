//! `glob` — recursive pattern match over the storage tree.
//!
//! Uses [`globset`] to compile patterns like `**/*.md` and matches them
//! against every file key under the optional `path` root. The walker
//! caps total files visited so a pathologically large tree can't lock
//! up the agent.

use std::sync::Arc;

use async_trait::async_trait;
use globset::Glob;
use runic_storage_backend::StorageBackend;
use runic_tool_core::{Tool, ToolContext, ToolResult};
use serde_json::Value;

use crate::error::ShellToolError;
use crate::paths::normalise;
use crate::walk::walk_files;

pub const DEFAULT_MAX_RESULTS: usize = 500;
pub const DEFAULT_MAX_WALK: usize = 5_000;

const DESCRIPTION: &str = "Find files matching a glob pattern under a storage subtree.\n\
\n\
- `pattern` examples: `**/*.md` (every markdown file), `wikis/*.json` (one level), `**/INDEX.md`.\n\
- `path` is optional — omit it to walk from the backend root.\n\
- Results are absolute storage keys, alphabetically sorted.\n\
- Recursion is capped to keep huge trees safe; truncation is reported in the response.";

pub struct GlobTool {
    storage: Arc<dyn StorageBackend>,
    max_results: usize,
    max_walk: usize,
}

impl GlobTool {
    pub fn new(storage: Arc<dyn StorageBackend>) -> Self {
        Self {
            storage,
            max_results: DEFAULT_MAX_RESULTS,
            max_walk: DEFAULT_MAX_WALK,
        }
    }

    pub fn with_max_results(mut self, n: usize) -> Self {
        self.max_results = n;
        self
    }

    pub fn with_max_walk(mut self, n: usize) -> Self {
        self.max_walk = n;
        self
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern (e.g. '**/*.md')." },
                "path": { "type": "string", "description": "Subtree to search. Omit or empty = root." }
            },
            "required": ["pattern"],
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

impl GlobTool {
    async fn dispatch(&self, input: Value) -> Result<String, ShellToolError> {
        let pattern = input
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or(ShellToolError::MissingField { field: "pattern" })?;
        let path_raw = input.get("path").and_then(Value::as_str);
        let root = normalise(path_raw)?;

        let matcher = Glob::new(pattern)
            .map_err(|source| ShellToolError::InvalidGlob {
                pattern: pattern.to_string(),
                source,
            })?
            .compile_matcher();

        let mut files = walk_files(&self.storage, &root, self.max_walk).await?;
        files.retain(|k| {
            // Match against the key as-is AND against the key relative to
            // the root — handles patterns expressed either way.
            let relative = k.strip_prefix(&root).unwrap_or(k);
            let relative = relative.strip_prefix('/').unwrap_or(relative);
            matcher.is_match(k) || matcher.is_match(relative)
        });
        files.sort();

        let total_matches = files.len();
        let truncated = total_matches > self.max_results;
        if truncated {
            files.truncate(self.max_results);
        }
        if files.is_empty() {
            return Ok(format!(
                "(no files matched '{pattern}' under '{}')",
                if root.is_empty() { "/" } else { &root }
            ));
        }
        let mut out = files.join("\n");
        let suffix = if truncated {
            format!(
                "\n\n[{} of {} matches shown — capped at {}]",
                files.len(),
                total_matches,
                self.max_results
            )
        } else {
            format!("\n\n[{} match(es)]", files.len())
        };
        out.push_str(&suffix);
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
        backend.write("README.md", b"a").await.unwrap();
        backend.write("docs/intro.md", b"b").await.unwrap();
        backend.write("docs/api/index.md", b"c").await.unwrap();
        backend.write("docs/api/users.json", b"d").await.unwrap();
        backend.write("src/main.rs", b"e").await.unwrap();
        backend
    }

    #[tokio::test]
    async fn matches_recursive_glob() {
        let tool = GlobTool::new(seeded().await);
        let r = tool
            .execute(serde_json::json!({"pattern": "**/*.md"}), &ctx())
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("README.md"));
        assert!(r.content.contains("docs/intro.md"));
        assert!(r.content.contains("docs/api/index.md"));
        assert!(!r.content.contains("users.json"));
        assert!(!r.content.contains("main.rs"));
    }

    #[tokio::test]
    async fn matches_with_path_root() {
        let tool = GlobTool::new(seeded().await);
        let r = tool
            .execute(
                serde_json::json!({"pattern": "**/*.json", "path": "docs"}),
                &ctx(),
            )
            .await;
        assert!(!r.is_error);
        assert!(r.content.contains("users.json"));
    }

    #[tokio::test]
    async fn no_match_returns_friendly_message() {
        let tool = GlobTool::new(seeded().await);
        let r = tool
            .execute(serde_json::json!({"pattern": "**/*.exe"}), &ctx())
            .await;
        assert!(!r.is_error);
        assert!(r.content.contains("no files matched"));
    }

    #[tokio::test]
    async fn invalid_pattern_errors() {
        let tool = GlobTool::new(seeded().await);
        let r = tool
            .execute(serde_json::json!({"pattern": "[invalid"}), &ctx())
            .await;
        assert!(r.is_error);
    }

    #[tokio::test]
    async fn result_cap_is_respected() {
        let backend: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        for n in 0..20 {
            backend
                .write(&format!("dir/f{n:02}.md"), b"x")
                .await
                .unwrap();
        }
        let tool = GlobTool::new(backend).with_max_results(5);
        let r = tool
            .execute(serde_json::json!({"pattern": "**/*.md"}), &ctx())
            .await;
        assert!(!r.is_error);
        assert!(r.content.contains("5 of 20"));
    }
}
