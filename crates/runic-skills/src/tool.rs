//! `SkillViewTool` — the `Tool` the agent calls to load a skill's content.
//!
//! Two modes:
//!   - `skill_view({ name })`                  → returns the skill's body
//!   - `skill_view({ name, path: "ref.md" })` → returns the sub-file content
//!
//! Sub-file paths are resolved relative to the skill's directory under the
//! configured `root`. `..` segments are rejected so the agent can never
//! escape the skill directory.
//!
//! The tool holds its own `Arc<dyn StorageBackend>` because the registry is
//! pure data (no I/O). This keeps the registry trivial to test and gives
//! the tool one obvious place to look when sub-file reads break.

use async_trait::async_trait;
use runic_storage_backend::StorageBackend;
use runic_tool_core::{Tool, ToolContext, ToolResult};
use std::sync::Arc;

use crate::SkillRegistry;

pub struct SkillViewTool {
    registry: Arc<SkillRegistry>,
    storage: Arc<dyn StorageBackend>,
    root: String,
}

impl SkillViewTool {
    pub fn new(
        registry: Arc<SkillRegistry>,
        storage: Arc<dyn StorageBackend>,
        root: impl Into<String>,
    ) -> Self {
        Self {
            registry,
            storage,
            root: root.into(),
        }
    }
}

#[async_trait]
impl Tool for SkillViewTool {
    fn name(&self) -> &str {
        "skill_view"
    }

    fn description(&self) -> &str {
        "Load the full instructions of a skill (no `path` arg) or read a supporting file within \
         a skill's directory (with `path` arg). Sub-file paths are relative to the skill — e.g. \
         `references/api.md` or `templates/draft.md`."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The skill's declared name (from its frontmatter `name:` field)."
                },
                "path": {
                    "type": "string",
                    "description": "Optional relative path to a sub-file inside the skill's directory \
                                    (e.g. 'references/api.md'). Omit to load the skill's main body."
                }
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let Some(name) = input.get("name").and_then(|v| v.as_str()) else {
            return ToolResult::error("missing required field 'name'");
        };

        let Some(skill) = self.registry.get(name) else {
            return ToolResult::error(format!("unknown skill: '{name}'"));
        };

        // Mode A: no `path` → return the SKILL.md body verbatim.
        let Some(rel_path) = input.get("path").and_then(|v| v.as_str()) else {
            return ToolResult::ok(skill.body.clone());
        };

        // Mode B: `path` given → resolve and read the sub-file.
        // Reject `..` segments so the agent can't traverse out of the skill
        // directory. Also reject absolute paths (leading '/') for the same
        // reason. These are belt-and-braces — the storage backend's own
        // `resolve` (for LocalFs) also rejects `..` — but a friendlier error
        // here saves a confusing storage-layer message.
        if rel_path.is_empty() {
            return ToolResult::error("path argument must not be empty");
        }
        if rel_path.starts_with('/') {
            return ToolResult::error(format!(
                "invalid path: '{rel_path}' must be relative (no leading '/')"
            ));
        }
        if rel_path.split('/').any(|seg| seg == ".." || seg.is_empty()) {
            return ToolResult::error(format!(
                "invalid path: '{rel_path}' must not contain '..' or empty segments"
            ));
        }

        let full_path = format!("{}/{}/{}", self.root, skill.dir, rel_path);
        match self.storage.read_to_string(&full_path).await {
            Ok(content) => ToolResult::ok(content),
            Err(err) => ToolResult::error(format!(
                "could not read '{rel_path}' for skill '{name}': {err}"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Skill, SkillMeta};
    use runic_storage_backend::MemoryBackend;
    use std::any::TypeId;
    use std::collections::HashMap;

    fn ctx() -> ToolContext {
        ToolContext::new(
            "sess".into(),
            "run".into(),
            0,
            HashMap::<TypeId, Arc<dyn std::any::Any + Send + Sync>>::new(),
        )
    }

    fn skill(name: &str, description: &str, body: &str, dir: &str) -> Skill {
        Skill {
            meta: SkillMeta {
                name: name.into(),
                description: description.into(),
            },
            body: body.into(),
            dir: dir.into(),
        }
    }

    fn registry_with(skills: Vec<Skill>) -> Arc<SkillRegistry> {
        let mut reg = SkillRegistry::new();
        for s in skills {
            reg.insert(s);
        }
        Arc::new(reg)
    }

    fn empty_storage() -> Arc<dyn StorageBackend> {
        Arc::new(MemoryBackend::new())
    }

    // ─── Mode A: body lookup ────────────────────────────────────────────────

    #[tokio::test]
    async fn returns_body_when_path_arg_is_absent() {
        let reg = registry_with(vec![skill("greeter", "hi", "Hello!", "greeter")]);
        let tool = SkillViewTool::new(reg, empty_storage(), "skills");

        let res = tool
            .execute(serde_json::json!({"name": "greeter"}), &ctx())
            .await;
        assert!(!res.is_error);
        assert_eq!(res.content, "Hello!");
    }

    #[tokio::test]
    async fn unknown_skill_is_an_error() {
        let reg = registry_with(vec![skill("known", "x", "body", "known")]);
        let tool = SkillViewTool::new(reg, empty_storage(), "skills");

        let res = tool
            .execute(serde_json::json!({"name": "missing"}), &ctx())
            .await;
        assert!(res.is_error);
        assert!(res.content.contains("unknown skill"));
    }

    #[tokio::test]
    async fn missing_name_arg_is_an_error() {
        let reg = registry_with(vec![skill("foo", "", "", "foo")]);
        let tool = SkillViewTool::new(reg, empty_storage(), "skills");

        let res = tool.execute(serde_json::json!({}), &ctx()).await;
        assert!(res.is_error);
        assert!(res.content.contains("name"));
    }

    // ─── Mode B: sub-file lookup ────────────────────────────────────────────

    #[tokio::test]
    async fn returns_sub_file_when_path_arg_is_present() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        storage
            .write(
                "skills/greeter/references/api.md",
                "# API\n\nThe API is great.".as_bytes(),
            )
            .await
            .unwrap();

        let reg = registry_with(vec![skill("greeter", "hi", "body", "greeter")]);
        let tool = SkillViewTool::new(reg, storage, "skills");

        let res = tool
            .execute(
                serde_json::json!({"name": "greeter", "path": "references/api.md"}),
                &ctx(),
            )
            .await;
        assert!(!res.is_error, "got error: {}", res.content);
        assert!(res.content.contains("The API is great."));
    }

    #[tokio::test]
    async fn sub_file_lookup_for_missing_file_is_an_error() {
        let reg = registry_with(vec![skill("greeter", "", "body", "greeter")]);
        let tool = SkillViewTool::new(reg, empty_storage(), "skills");

        let res = tool
            .execute(
                serde_json::json!({"name": "greeter", "path": "references/nope.md"}),
                &ctx(),
            )
            .await;
        assert!(res.is_error);
        assert!(res.content.contains("could not read"));
    }

    // ─── Path-traversal guard ───────────────────────────────────────────────

    #[tokio::test]
    async fn rejects_dotdot_in_path() {
        let reg = registry_with(vec![skill("greeter", "", "body", "greeter")]);
        let tool = SkillViewTool::new(reg, empty_storage(), "skills");

        let res = tool
            .execute(
                serde_json::json!({"name": "greeter", "path": "../escape.md"}),
                &ctx(),
            )
            .await;
        assert!(res.is_error);
        assert!(res.content.contains(".."));
    }

    #[tokio::test]
    async fn rejects_dotdot_nested_in_path() {
        let reg = registry_with(vec![skill("greeter", "", "body", "greeter")]);
        let tool = SkillViewTool::new(reg, empty_storage(), "skills");

        let res = tool
            .execute(
                serde_json::json!({"name": "greeter", "path": "references/../../etc/passwd"}),
                &ctx(),
            )
            .await;
        assert!(res.is_error);
        assert!(res.content.contains(".."));
    }

    #[tokio::test]
    async fn rejects_absolute_path() {
        let reg = registry_with(vec![skill("greeter", "", "body", "greeter")]);
        let tool = SkillViewTool::new(reg, empty_storage(), "skills");

        let res = tool
            .execute(
                serde_json::json!({"name": "greeter", "path": "/etc/passwd"}),
                &ctx(),
            )
            .await;
        assert!(res.is_error);
        assert!(res.content.contains("relative"));
    }

    #[tokio::test]
    async fn rejects_empty_path() {
        let reg = registry_with(vec![skill("greeter", "", "body", "greeter")]);
        let tool = SkillViewTool::new(reg, empty_storage(), "skills");

        let res = tool
            .execute(
                serde_json::json!({"name": "greeter", "path": ""}),
                &ctx(),
            )
            .await;
        assert!(res.is_error);
        assert!(res.content.contains("empty"));
    }

    // ─── Wiring smoke tests ─────────────────────────────────────────────────

    #[test]
    fn tool_has_a_stable_name_and_schema() {
        let tool = SkillViewTool::new(
            Arc::new(SkillRegistry::new()),
            empty_storage(),
            "skills",
        );
        assert_eq!(tool.name(), "skill_view");
        // Schema must be a JSON object with `name` required.
        let schema = tool.input_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"][0], "name");
    }

    #[test]
    fn tool_is_parallelizable_by_default() {
        // SkillViewTool inherits the default ToolDispatch::parallelizable() == true
        // via PlainAdapter — confirm we haven't accidentally serialized
        // sub-file reads.
        use runic_tool_core::{PlainAdapter, ToolDispatch};
        let tool = Arc::new(SkillViewTool::new(
            Arc::new(SkillRegistry::new()),
            empty_storage(),
            "skills",
        ));
        let adapter = PlainAdapter(tool);
        assert!(adapter.parallelizable());
    }
}
