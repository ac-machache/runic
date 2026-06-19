//! The six filesystem tools — thin wrappers over a
//! [`FilesystemBackend`](runic_filesystem::FilesystemBackend). They own the
//! LLM schema + output formatting; the backend owns the semantics. Swap the
//! backend (Local / Composite / S3) and these don't change.

use std::sync::Arc;

use async_trait::async_trait;

use runic_filesystem::{FilesystemBackend, FsError, ReadResult};
use runic_tool::{Tool, ToolContext, ToolResult};

type Fs = Arc<dyn FilesystemBackend>;

fn fs_err(e: FsError) -> ToolResult {
    ToolResult::error(e.to_string())
}

fn arg_str<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

/// `cat -n` style formatting starting at `start_line`.
fn format_read(r: &ReadResult) -> String {
    let mut out = String::new();
    for (i, line) in r.content.lines().enumerate() {
        out.push_str(&format!("{:>6}\t{}\n", r.start_line + i, line));
    }
    if r.truncated {
        out.push_str("       … (truncated; read more with offset/limit)\n");
    }
    out
}

// ── read_file ────────────────────────────────────────────────────────────────

pub struct ReadFileTool(pub Fs);

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a file with line numbers. Paginate large files with `offset` \
         (0-indexed line) and `limit` (lines)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "offset": { "type": "integer", "minimum": 0 },
                "limit": { "type": "integer", "minimum": 1 }
            },
            "required": ["path"]
        })
    }
    fn parallelizable(&self) -> bool {
        true
    }
    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        let Some(path) = arg_str(&args, "path") else {
            return Ok(ToolResult::error("read_file requires `path`"));
        };
        let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(2000) as usize;
        Ok(match self.0.read(path, offset, limit).await {
            Ok(r) => ToolResult::ok(format_read(&r)),
            Err(e) => fs_err(e),
        })
    }
}

// ── write_file ───────────────────────────────────────────────────────────────

pub struct WriteFileTool(pub Fs);

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Create a NEW file with the given content. Errors if the file already \
         exists — use edit_file to change an existing file."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        })
    }
    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        let (Some(path), Some(content)) = (arg_str(&args, "path"), arg_str(&args, "content")) else {
            return Ok(ToolResult::error("write_file requires `path` and `content`"));
        };
        Ok(match self.0.write(path, content).await {
            Ok(()) => ToolResult::ok(format!("wrote {path}")),
            Err(e) => fs_err(e),
        })
    }
}

// ── edit_file ────────────────────────────────────────────────────────────────

pub struct EditFileTool(pub Fs);

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "Replace an exact string in a file. `old_string` must be unique unless \
         `replace_all` is true. Include enough surrounding context to make it \
         unique."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_string": { "type": "string" },
                "new_string": { "type": "string" },
                "replace_all": { "type": "boolean", "default": false }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }
    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        let (Some(path), Some(old), Some(new)) = (
            arg_str(&args, "path"),
            arg_str(&args, "old_string"),
            arg_str(&args, "new_string"),
        ) else {
            return Ok(ToolResult::error(
                "edit_file requires `path`, `old_string`, `new_string`",
            ));
        };
        let replace_all = args.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
        Ok(match self.0.edit(path, old, new, replace_all).await {
            Ok(n) => ToolResult::ok(format!("edited {path} ({n} replacement(s))")),
            Err(e) => fs_err(e),
        })
    }
}

// ── ls ───────────────────────────────────────────────────────────────────────

pub struct LsTool(pub Fs);

#[async_trait]
impl Tool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }
    fn description(&self) -> &str {
        "List the entries of a directory (one level). Defaults to the root."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string", "description": "Directory (default '/')." } }
        })
    }
    fn parallelizable(&self) -> bool {
        true
    }
    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        let path = arg_str(&args, "path").unwrap_or("/");
        Ok(match self.0.ls(path).await {
            Ok(entries) => {
                let mut out = String::new();
                for e in entries {
                    out.push_str(&format!("{}{}\n", e.path, if e.is_dir { "/" } else { "" }));
                }
                ToolResult::ok(if out.is_empty() { "(empty)".into() } else { out })
            }
            Err(e) => fs_err(e),
        })
    }
}

// ── glob ─────────────────────────────────────────────────────────────────────

pub struct GlobTool(pub Fs);

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "Find files matching a glob pattern (e.g. `**/*.rs`), optionally under \
         `path`."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string" }
            },
            "required": ["pattern"]
        })
    }
    fn parallelizable(&self) -> bool {
        true
    }
    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        let Some(pattern) = arg_str(&args, "pattern") else {
            return Ok(ToolResult::error("glob requires `pattern`"));
        };
        let path = arg_str(&args, "path");
        Ok(match self.0.glob(pattern, path).await {
            Ok(found) if found.is_empty() => ToolResult::ok("(no matches)"),
            Ok(found) => ToolResult::ok(
                found.iter().map(|f| f.path.as_str()).collect::<Vec<_>>().join("\n"),
            ),
            Err(e) => fs_err(e),
        })
    }
}

// ── grep ─────────────────────────────────────────────────────────────────────

pub struct GrepTool(pub Fs);

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents for a literal string. `path` scopes the search; \
         `glob` filters which files. `output_mode`: content (default) | files | count."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string" },
                "glob": { "type": "string" },
                "output_mode": { "type": "string", "enum": ["content", "files", "count"] }
            },
            "required": ["pattern"]
        })
    }
    fn parallelizable(&self) -> bool {
        true
    }
    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        let Some(pattern) = arg_str(&args, "pattern") else {
            return Ok(ToolResult::error("grep requires `pattern`"));
        };
        let path = arg_str(&args, "path");
        let glob = arg_str(&args, "glob");
        let mode = arg_str(&args, "output_mode").unwrap_or("content");

        let matches = match self.0.grep(pattern, path, glob).await {
            Ok(m) => m,
            Err(e) => return Ok(fs_err(e)),
        };
        if matches.is_empty() {
            return Ok(ToolResult::ok("(no matches)"));
        }
        let out = match mode {
            "files" => {
                let mut files: Vec<&str> = matches.iter().map(|m| m.path.as_str()).collect();
                files.sort();
                files.dedup();
                files.join("\n")
            }
            "count" => {
                use std::collections::BTreeMap;
                let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
                for m in &matches {
                    *counts.entry(m.path.as_str()).or_default() += 1;
                }
                counts.iter().map(|(p, c)| format!("{p}: {c}")).collect::<Vec<_>>().join("\n")
            }
            _ => matches
                .iter()
                .map(|m| format!("{}:{}: {}", m.path, m.line, m.text))
                .collect::<Vec<_>>()
                .join("\n"),
        };
        Ok(ToolResult::ok(out))
    }
}

/// The six filesystem tools bound to `fs`, ready to register.
pub fn fs_tools(fs: Fs) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadFileTool(fs.clone())),
        Arc::new(WriteFileTool(fs.clone())),
        Arc::new(EditFileTool(fs.clone())),
        Arc::new(LsTool(fs.clone())),
        Arc::new(GlobTool(fs.clone())),
        Arc::new(GrepTool(fs)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use runic_filesystem::LocalFs;

    fn ctx() -> ToolContext {
        ToolContext::new("u", "s", "r")
    }

    #[tokio::test]
    async fn fs_tools_drive_a_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let fs: Fs = Arc::new(LocalFs::new(tmp.path()));

        WriteFileTool(fs.clone())
            .execute(serde_json::json!({ "path": "/a.txt", "content": "one\ntwo" }), &ctx())
            .await
            .unwrap();

        let r = ReadFileTool(fs.clone())
            .execute(serde_json::json!({ "path": "/a.txt" }), &ctx())
            .await
            .unwrap();
        assert!(r.success && r.output.contains("1\tone") && r.output.contains("2\ttwo"));

        let e = EditFileTool(fs.clone())
            .execute(serde_json::json!({ "path": "/a.txt", "old_string": "two", "new_string": "2" }), &ctx())
            .await
            .unwrap();
        assert!(e.success);

        let g = GrepTool(fs.clone())
            .execute(serde_json::json!({ "pattern": "one" }), &ctx())
            .await
            .unwrap();
        assert!(g.output.contains("/a.txt:1"));

        // ambiguous edit surfaces as an in-band error
        WriteFileTool(fs.clone())
            .execute(serde_json::json!({ "path": "/d.txt", "content": "x x x" }), &ctx())
            .await
            .unwrap();
        let bad = EditFileTool(fs)
            .execute(serde_json::json!({ "path": "/d.txt", "old_string": "x", "new_string": "y" }), &ctx())
            .await
            .unwrap();
        assert!(!bad.success);
    }
}
