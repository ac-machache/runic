//! `apply_patch` — apply a multi-file diff (OpenFang's format) via the
//! filesystem backend. An alternative to many `edit_file`/`write_file` calls:
//! one patch can add, update (hunks), and delete files at once.
//!
//! ```text
//! *** Begin Patch
//! *** Add File: src/new.rs
//! +fn x() {}
//! *** Update File: src/lib.rs
//! @@
//!  unchanged context
//! -old line
//! +new line
//! *** Delete File: src/old.rs
//! *** End Patch
//! ```
//! Add → `write`, Delete → `delete`, Update → `edit` per hunk (each hunk's
//! context+removed block must match uniquely).

use std::sync::Arc;

use async_trait::async_trait;

use runic_filesystem::FilesystemBackend;
use runic_tool::{Tool, ToolContext, ToolResult};

type Fs = Arc<dyn FilesystemBackend>;

pub struct ApplyPatchTool(pub Fs);

enum Op {
    Add {
        path: String,
        content: String,
    },
    Update {
        path: String,
        hunks: Vec<(String, String)>,
    },
    Delete {
        path: String,
    },
}

fn flush_hunk(old: &mut Vec<String>, new: &mut Vec<String>, hunks: &mut Vec<(String, String)>) {
    if !old.is_empty() || !new.is_empty() {
        hunks.push((old.join("\n"), new.join("\n")));
        old.clear();
        new.clear();
    }
}

fn parse_patch(text: &str) -> Result<Vec<Op>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut ops = Vec::new();
    let mut i = 0;
    while i < lines.len() && !lines[i].starts_with("*** ") {
        i += 1;
    }
    if i < lines.len() && lines[i].trim() == "*** Begin Patch" {
        i += 1;
    }

    while i < lines.len() {
        let line = lines[i];
        if line.trim() == "*** End Patch" {
            break;
        }
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            i += 1;
            let mut content = Vec::new();
            while i < lines.len() && !lines[i].starts_with("*** ") {
                let l = lines[i];
                content.push(l.strip_prefix('+').unwrap_or(l).to_string());
                i += 1;
            }
            ops.push(Op::Add {
                path: path.trim().to_string(),
                content: content.join("\n"),
            });
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            i += 1;
            ops.push(Op::Delete {
                path: path.trim().to_string(),
            });
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            i += 1;
            let mut old = Vec::new();
            let mut new = Vec::new();
            let mut hunks = Vec::new();
            while i < lines.len() && !lines[i].starts_with("*** ") {
                let l = lines[i];
                if l.starts_with("@@") {
                    flush_hunk(&mut old, &mut new, &mut hunks);
                } else if let Some(rest) = l.strip_prefix(' ') {
                    old.push(rest.to_string());
                    new.push(rest.to_string());
                } else if let Some(rest) = l.strip_prefix('-') {
                    old.push(rest.to_string());
                } else if let Some(rest) = l.strip_prefix('+') {
                    new.push(rest.to_string());
                } else {
                    // blank / unprefixed line → context in both
                    old.push(l.to_string());
                    new.push(l.to_string());
                }
                i += 1;
            }
            flush_hunk(&mut old, &mut new, &mut hunks);
            ops.push(Op::Update {
                path: path.trim().to_string(),
                hunks,
            });
        } else {
            i += 1;
        }
    }

    if ops.is_empty() {
        return Err("no file operations found in patch".into());
    }
    Ok(ops)
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }
    fn description(&self) -> &str {
        "Apply a multi-file patch (Add/Update/Delete with hunks) in one call. \
         Update hunks use ' ' context, '-' removed, '+' added lines; the \
         context+removed block must match the file uniquely."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "patch": { "type": "string", "description": "The patch text." } },
            "required": ["patch"]
        })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let Some(patch) = args.get("patch").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("apply_patch requires `patch`"));
        };
        let ops = match parse_patch(patch) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::error(e)),
        };

        let mut report = Vec::new();
        for op in ops {
            let result = match op {
                Op::Add { path, content } => self
                    .0
                    .write(&path, &content)
                    .await
                    .map(|()| format!("added {path}"))
                    .map_err(|e| format!("add {path}: {e}")),
                Op::Delete { path } => self
                    .0
                    .delete(&path)
                    .await
                    .map(|()| format!("deleted {path}"))
                    .map_err(|e| format!("delete {path}: {e}")),
                Op::Update { path, hunks } => {
                    let mut applied = 0;
                    let mut err = None;
                    for (old, new) in hunks {
                        if old.is_empty() {
                            err = Some(format!("update {path}: a hunk has no lines to match"));
                            break;
                        }
                        if let Err(e) = self.0.edit(&path, &old, &new, false).await {
                            err = Some(format!("update {path}: {e}"));
                            break;
                        }
                        applied += 1;
                    }
                    match err {
                        Some(e) => Err(e),
                        None => Ok(format!("updated {path} ({applied} hunk(s))")),
                    }
                }
            };
            match result {
                Ok(line) => report.push(line),
                // Patches aren't transactional here — report what failed so the
                // model can recover.
                Err(e) => {
                    report.push(format!("FAILED: {e}"));
                    return Ok(ToolResult::error(report.join("\n")));
                }
            }
        }
        Ok(ToolResult::ok(report.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runic_filesystem::LocalFs;

    #[tokio::test]
    async fn add_update_delete_in_one_patch() {
        let tmp = tempfile::tempdir().unwrap();
        let fs: Fs = Arc::new(LocalFs::new(tmp.path()));
        fs.write("/keep.rs", "fn a() {}\nfn old() {}\n")
            .await
            .unwrap();
        fs.write("/gone.rs", "delete me").await.unwrap();

        let patch = "*** Begin Patch\n\
            *** Add File: /new.rs\n\
            +fn fresh() {}\n\
            *** Update File: /keep.rs\n\
            @@\n\
            -fn old() {}\n\
            +fn renamed() {}\n\
            *** Delete File: /gone.rs\n\
            *** End Patch";

        let tool = ApplyPatchTool(fs.clone());
        let r = tool
            .execute(
                serde_json::json!({ "patch": patch }),
                &ToolContext::new("u", "s", "r"),
            )
            .await
            .unwrap();
        assert!(r.success, "{}", r.output);

        assert_eq!(
            fs.read("/new.rs", 0, 9).await.unwrap().content,
            "fn fresh() {}"
        );
        assert!(
            fs.read("/keep.rs", 0, 9)
                .await
                .unwrap()
                .content
                .contains("renamed")
        );
        assert!(matches!(
            fs.read("/gone.rs", 0, 1).await,
            Err(runic_filesystem::FsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn empty_patch_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let fs: Fs = Arc::new(LocalFs::new(tmp.path()));
        let r = ApplyPatchTool(fs)
            .execute(
                serde_json::json!({ "patch": "nothing here" }),
                &ToolContext::new("u", "s", "r"),
            )
            .await
            .unwrap();
        assert!(!r.success);
    }
}
