//! `grep` — regex content search across the storage tree.
//!
//! Three output modes match what deepagents exposes:
//!
//! - `files_with_matches` (default) — just file keys that had at least one hit
//! - `content` — `path:line:matching_line` for each hit
//! - `count` — `path: N` aggregated per file
//!
//! An optional `glob` narrows which files are scanned (`*.md` etc.).
//! Per-file byte cap prevents a single huge file from dominating runtime;
//! total match cap prevents a flood from drowning the response.

use std::sync::Arc;

use async_trait::async_trait;
use globset::Glob;
use regex::Regex;
use runic_storage_backend::StorageBackend;
use runic_tool_core::{Tool, ToolContext, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ShellToolError;
use crate::paths::normalise;
use crate::walk::walk_files;

pub const DEFAULT_MAX_MATCHES: usize = 200;
pub const DEFAULT_MAX_FILE_BYTES: usize = 200 * 1024;
pub const DEFAULT_MAX_WALK: usize = 5_000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GrepOutputMode {
    FilesWithMatches,
    Content,
    Count,
}

impl Default for GrepOutputMode {
    fn default() -> Self {
        Self::FilesWithMatches
    }
}

const DESCRIPTION: &str = "Search file contents under a storage subtree with a regex.\n\
\n\
- `pattern` is a Rust regex (https://docs.rs/regex). Case-sensitive by default; use `(?i)` for ignore-case.\n\
- `path` optional: where to start the walk. Omit for backend root.\n\
- `glob` optional: scan only files whose key matches this glob (e.g. `*.md`).\n\
- `output_mode`:\n\
  - `files_with_matches` (default) — list file keys that contained at least one match.\n\
  - `content` — `path:line:matching_line` for each match.\n\
  - `count` — `path: N` per file.\n\
- Each scanned file is capped at 200 KiB; total matches capped to keep responses bounded.";

pub struct GrepTool {
    storage: Arc<dyn StorageBackend>,
    max_matches: usize,
    max_file_bytes: usize,
    max_walk: usize,
}

impl GrepTool {
    pub fn new(storage: Arc<dyn StorageBackend>) -> Self {
        Self {
            storage,
            max_matches: DEFAULT_MAX_MATCHES,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_walk: DEFAULT_MAX_WALK,
        }
    }

    pub fn with_max_matches(mut self, n: usize) -> Self {
        self.max_matches = n;
        self
    }

    pub fn with_max_file_bytes(mut self, n: usize) -> Self {
        self.max_file_bytes = n;
        self
    }

    pub fn with_max_walk(mut self, n: usize) -> Self {
        self.max_walk = n;
        self
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Rust regex pattern (`(?i)` for case-insensitive)." },
                "path": { "type": "string", "description": "Subtree to scan. Omit or empty = root." },
                "glob": { "type": "string", "description": "Optional file-key glob filter (e.g. '*.md')." },
                "output_mode": {
                    "type": "string",
                    "enum": ["files_with_matches", "content", "count"],
                    "description": "Default 'files_with_matches'."
                }
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

impl GrepTool {
    async fn dispatch(&self, input: Value) -> Result<String, ShellToolError> {
        let pattern = input
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or(ShellToolError::MissingField { field: "pattern" })?;
        let path_raw = input.get("path").and_then(Value::as_str);
        let root = normalise(path_raw)?;
        let mode: GrepOutputMode = match input.get("output_mode").and_then(Value::as_str) {
            None => GrepOutputMode::default(),
            Some(s) => serde_json::from_value(Value::String(s.to_string()))
                .map_err(|_| ShellToolError::InvalidValue {
                    field: "output_mode",
                    reason: format!(
                        "must be one of files_with_matches | content | count (got {s:?})"
                    ),
                })?,
        };

        let regex = Regex::new(pattern).map_err(|source| ShellToolError::InvalidRegex {
            pattern: pattern.to_string(),
            source,
        })?;
        let file_glob = match input.get("glob").and_then(Value::as_str) {
            Some(g) => Some(
                Glob::new(g)
                    .map_err(|source| ShellToolError::InvalidGlob {
                        pattern: g.to_string(),
                        source,
                    })?
                    .compile_matcher(),
            ),
            None => None,
        };

        let mut files = walk_files(&self.storage, &root, self.max_walk).await?;
        if let Some(g) = &file_glob {
            files.retain(|k| g.is_match(k) || g.is_match(k.rsplit('/').next().unwrap_or(k)));
        }
        files.sort();

        let mut hits: Vec<(String, Vec<(usize, String)>)> = Vec::new();
        let mut total_matches = 0usize;
        let mut truncated = false;

        for key in &files {
            let bytes = self
                .storage
                .read(key)
                .await
                .map_err(|e| ShellToolError::Storage(e.to_string()))?;
            let slice: &[u8] = if bytes.len() > self.max_file_bytes {
                &bytes[..self.max_file_bytes]
            } else {
                &bytes
            };
            let text = match std::str::from_utf8(slice) {
                Ok(t) => t,
                Err(_) => continue, // skip binary
            };
            let mut per_file: Vec<(usize, String)> = Vec::new();
            for (idx, line) in text.lines().enumerate() {
                if regex.is_match(line) {
                    per_file.push((idx + 1, line.to_string()));
                    total_matches += 1;
                    if total_matches >= self.max_matches {
                        truncated = true;
                        break;
                    }
                }
            }
            if !per_file.is_empty() {
                hits.push((key.clone(), per_file));
            }
            if truncated {
                break;
            }
        }

        if hits.is_empty() {
            return Ok(format!(
                "(no matches for {:?} under '{}')",
                pattern,
                if root.is_empty() { "/" } else { &root }
            ));
        }

        let body = match mode {
            GrepOutputMode::FilesWithMatches => {
                let lines: Vec<String> = hits.iter().map(|(k, _)| k.clone()).collect();
                lines.join("\n")
            }
            GrepOutputMode::Count => hits
                .iter()
                .map(|(k, m)| format!("{k}: {}", m.len()))
                .collect::<Vec<_>>()
                .join("\n"),
            GrepOutputMode::Content => {
                let mut out: Vec<String> = Vec::new();
                for (key, matches) in &hits {
                    for (line_no, line) in matches {
                        out.push(format!("{key}:{line_no}:{line}"));
                    }
                }
                out.join("\n")
            }
        };

        let suffix = if truncated {
            format!(
                "\n\n[truncated at {} match(es); raise max_matches to see more]",
                self.max_matches
            )
        } else {
            format!(
                "\n\n[{} match(es) across {} file(s)]",
                total_matches,
                hits.len()
            )
        };
        Ok(format!("{body}{suffix}"))
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
        backend
            .write("a.md", b"alpha\nbeta foo\ngamma")
            .await
            .unwrap();
        backend
            .write("dir/b.md", b"hello\nfoo bar foo\nworld")
            .await
            .unwrap();
        backend.write("dir/c.txt", b"no match here").await.unwrap();
        backend
    }

    #[tokio::test]
    async fn files_with_matches_default() {
        let tool = GrepTool::new(seeded().await);
        let r = tool
            .execute(serde_json::json!({"pattern": "foo"}), &ctx())
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("a.md"));
        assert!(r.content.contains("dir/b.md"));
        assert!(!r.content.contains("c.txt"));
    }

    #[tokio::test]
    async fn content_mode_shows_line_and_text() {
        let tool = GrepTool::new(seeded().await);
        let r = tool
            .execute(
                serde_json::json!({"pattern": "foo", "output_mode": "content"}),
                &ctx(),
            )
            .await;
        assert!(!r.is_error);
        assert!(r.content.contains("a.md:2:beta foo"));
        assert!(r.content.contains("dir/b.md:2:foo bar foo"));
    }

    #[tokio::test]
    async fn count_mode_aggregates_per_file() {
        let tool = GrepTool::new(seeded().await);
        let r = tool
            .execute(
                serde_json::json!({"pattern": "foo", "output_mode": "count"}),
                &ctx(),
            )
            .await;
        assert!(!r.is_error);
        // a.md has 1 match line, dir/b.md has 1 match line (we count lines).
        assert!(r.content.contains("a.md: 1"));
        assert!(r.content.contains("dir/b.md: 1"));
    }

    #[tokio::test]
    async fn glob_filter_narrows_files() {
        let tool = GrepTool::new(seeded().await);
        let r = tool
            .execute(
                serde_json::json!({"pattern": "foo", "glob": "*.md"}),
                &ctx(),
            )
            .await;
        assert!(!r.is_error);
        assert!(r.content.contains("a.md"));
        assert!(r.content.contains("dir/b.md"));
    }

    #[tokio::test]
    async fn case_insensitive_via_inline_flag() {
        let backend: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        backend.write("a.md", b"FOO BAR").await.unwrap();
        let tool = GrepTool::new(backend);
        let r = tool
            .execute(serde_json::json!({"pattern": "(?i)foo"}), &ctx())
            .await;
        assert!(!r.is_error);
        assert!(r.content.contains("a.md"));
    }

    #[tokio::test]
    async fn invalid_regex_errors() {
        let tool = GrepTool::new(seeded().await);
        let r = tool
            .execute(serde_json::json!({"pattern": "[invalid"}), &ctx())
            .await;
        assert!(r.is_error);
    }

    #[tokio::test]
    async fn invalid_output_mode_errors() {
        let tool = GrepTool::new(seeded().await);
        let r = tool
            .execute(
                serde_json::json!({"pattern": "foo", "output_mode": "exotic"}),
                &ctx(),
            )
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("output_mode"));
    }

    #[tokio::test]
    async fn no_matches_returns_friendly_message() {
        let tool = GrepTool::new(seeded().await);
        let r = tool
            .execute(serde_json::json!({"pattern": "ZZZ"}), &ctx())
            .await;
        assert!(!r.is_error);
        assert!(r.content.contains("no matches"));
    }
}
