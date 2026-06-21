//! `MemoryTool` — the `memory` tool the agent calls to update its own
//! notes. Wraps [`BoundedMemoryStore`] in the [`Tool`] trait.
//!
//! Single tool, multi-action: the JSON schema dispatches on `action` +
//! `target`. Matches the hermes shape exactly so prompts/skills authored
//! against hermes work here too.

use std::sync::Arc;

use async_trait::async_trait;
use runic_tool::{Tool, ToolContext, ToolResult};
use serde_json::Value;

use crate::error::MemoryError;
use crate::store::{BoundedMemoryStore, Target};

/// Tool description — folds in hermes's MEMORY_GUIDANCE so an LLM authored
/// against hermes uses this identically: *when* to save, declarative-not-
/// imperative phrasing, and what NOT to store.
const DESCRIPTION: &str = "Persistent curated memory you control, injected into your prompt every session. Two stores:\n\
- target='memory' (MEMORY.md): your own notes — environment facts, project conventions, tool quirks, lessons learned. Cap 2200 chars.\n\
- target='user' (USER.md): who the user is — preferences, role, communication style, pet peeves, workflow habits. Cap 1375 chars.\n\
\n\
WHEN TO SAVE (proactively, don't wait to be asked): the user corrects you or says 'remember this'; shares a preference or personal detail; you discover an environment fact (OS, tooling, project layout); you learn a convention or API quirk. Priority: user preferences/corrections > environment facts > procedural knowledge — the best memory stops the user repeating themselves.\n\
\n\
DO NOT SAVE task progress, session outcomes, PR/issue numbers, commit SHAs, 'Phase N done', or anything stale within a week. If it will be stale in a week, it does not belong here. Procedures/workflows belong in skills, not memory.\n\
\n\
Write DECLARATIVE FACTS, not instructions: 'User prefers concise responses' OK — 'Always respond concisely' WRONG. Imperative phrasing gets re-read as a directive in later sessions and causes repeated work.\n\
\n\
Actions:\n\
- action='read', target=<store>: list current entries.\n\
- action='add', target=<store>, content='<single fact>': append a pithy single-fact line. Idempotent. Errors if it would breach the cap — `remove` or `replace` first.\n\
- action='remove', target=<store>, search='<unique substring>': delete one entry (substring must match exactly one).\n\
- action='replace', target=<store>, search='<unique substring>', replacement='<new content>': swap one entry (same uniqueness rule).\n\
\n\
Style: one short self-contained line per entry. No paragraphs, no timestamps/dates.";

pub struct MemoryTool {
    store: Arc<BoundedMemoryStore>,
}

impl MemoryTool {
    pub fn new(store: Arc<BoundedMemoryStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "replace", "remove", "read"],
                    "description": "What to do with the store."
                },
                "target": {
                    "type": "string",
                    "enum": ["memory", "user"],
                    "description": "Which store: 'memory' (MEMORY.md) or 'user' (USER.md)."
                },
                "content": {
                    "type": "string",
                    "description": "Entry content. Required for action=add."
                },
                "search": {
                    "type": "string",
                    "description": "Unique substring of an existing entry. Required for action=remove and action=replace."
                },
                "replacement": {
                    "type": "string",
                    "description": "New content for the matched entry. Required for action=replace."
                }
            },
            "required": ["action", "target"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        Ok(match self.dispatch(input).await {
            Ok(out) => ToolResult::ok(out),
            Err(err) => ToolResult::error(err.to_string()),
        })
    }
}

impl MemoryTool {
    async fn dispatch(&self, input: Value) -> Result<String, MemoryError> {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .ok_or(MemoryError::MissingField { field: "action" })?;
        let target = Target::parse(
            input
                .get("target")
                .and_then(Value::as_str)
                .ok_or(MemoryError::MissingField { field: "target" })?,
        )?;
        let limit = self.store.limit_for(target);

        match action {
            "read" => {
                let entries = self.store.read(target).await?;
                if entries.is_empty() {
                    return Ok(format!("(empty — no entries in {})", target.label()));
                }
                let body = entries
                    .iter()
                    .enumerate()
                    .map(|(i, e)| format!("{}. {}", i + 1, e))
                    .collect::<Vec<_>>()
                    .join("\n");
                let total = self.store.char_count(target).await?;
                Ok(format!(
                    "{body}\n\n[{n} entries, {total}/{limit} chars]",
                    n = entries.len()
                ))
            }
            "add" => {
                let content = input
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or(MemoryError::MissingField { field: "content" })?;
                let count = self.store.add(target, content).await?;
                let total = self.store.char_count(target).await?;
                Ok(format!(
                    "ok — added to {} ({} entries, {}/{} chars)",
                    target.label(),
                    count,
                    total,
                    limit
                ))
            }
            "remove" => {
                let search = input
                    .get("search")
                    .and_then(Value::as_str)
                    .ok_or(MemoryError::MissingField { field: "search" })?;
                let removed = self.store.remove(target, search).await?;
                Ok(format!(
                    "ok — removed from {}: {}",
                    target.label(),
                    truncate(&removed, 120)
                ))
            }
            "replace" => {
                let search = input
                    .get("search")
                    .and_then(Value::as_str)
                    .ok_or(MemoryError::MissingField { field: "search" })?;
                let replacement = input.get("replacement").and_then(Value::as_str).ok_or(
                    MemoryError::MissingField {
                        field: "replacement",
                    },
                )?;
                self.store.replace(target, search, replacement).await?;
                Ok(format!("ok — replaced in {}", target.label()))
            }
            other => Err(MemoryError::InvalidAction {
                action: other.to_string(),
            }),
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut head: String = s.chars().take(n).collect();
        head.push('…');
        head
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runic_filesystem::{FilesystemBackend, MemoryFs};
    use runic_tool::ToolContext;
    use std::sync::Arc;

    fn make() -> (MemoryTool, Arc<BoundedMemoryStore>) {
        let backend: Arc<dyn FilesystemBackend> = Arc::new(MemoryFs::new());
        let store = Arc::new(BoundedMemoryStore::new(backend));
        (MemoryTool::new(store.clone()), store)
    }

    fn ctx() -> ToolContext {
        ToolContext::new("user-1", "session-1", "run-1")
    }

    async fn run(tool: &MemoryTool, v: Value) -> ToolResult {
        tool.execute(v, &ctx()).await.unwrap()
    }

    #[tokio::test]
    async fn add_then_read_via_tool() {
        let (tool, _) = make();
        let add = run(
            &tool,
            serde_json::json!({"action": "add", "target": "user", "content": "lives in Paris"}),
        )
        .await;
        assert!(add.success, "{:?}", add.output);

        let read = run(
            &tool,
            serde_json::json!({"action": "read", "target": "user"}),
        )
        .await;
        assert!(read.success);
        assert!(read.output.contains("lives in Paris"));
        assert!(read.output.contains("1 entries"));
    }

    #[tokio::test]
    async fn add_missing_content_errors() {
        let (tool, _) = make();
        let result = run(
            &tool,
            serde_json::json!({"action": "add", "target": "user"}),
        )
        .await;
        assert!(!result.success);
        assert!(result.output.contains("content"));
    }

    #[tokio::test]
    async fn invalid_action_errors() {
        let (tool, _) = make();
        let result = run(
            &tool,
            serde_json::json!({"action": "yeet", "target": "memory"}),
        )
        .await;
        assert!(!result.success);
        assert!(result.output.contains("yeet"));
    }

    #[tokio::test]
    async fn invalid_target_errors() {
        let (tool, _) = make();
        let result = run(
            &tool,
            serde_json::json!({"action": "read", "target": "nope"}),
        )
        .await;
        assert!(!result.success);
        assert!(result.output.contains("nope"));
    }

    #[tokio::test]
    async fn read_empty_reports_empty() {
        let (tool, _) = make();
        let result = run(
            &tool,
            serde_json::json!({"action": "read", "target": "memory"}),
        )
        .await;
        assert!(result.success);
        assert!(result.output.contains("empty"));
    }

    #[tokio::test]
    async fn full_flow_add_replace_remove_read() {
        let (tool, _) = make();
        run(
            &tool,
            serde_json::json!({"action": "add", "target": "memory", "content": "uses fish shell"}),
        )
        .await;
        run(&tool, serde_json::json!({"action": "replace", "target": "memory", "search": "fish", "replacement": "uses zsh shell"})).await;
        let read = run(
            &tool,
            serde_json::json!({"action": "read", "target": "memory"}),
        )
        .await;
        assert!(read.output.contains("zsh"));
        assert!(!read.output.contains("fish"));

        let removed = run(
            &tool,
            serde_json::json!({"action": "remove", "target": "memory", "search": "zsh"}),
        )
        .await;
        assert!(removed.success);
        let after = run(
            &tool,
            serde_json::json!({"action": "read", "target": "memory"}),
        )
        .await;
        assert!(after.output.contains("empty"));
    }

    #[tokio::test]
    async fn shared_store_is_visible_across_tool_invocations() {
        // Same store passed twice -> entries written by one tool clone are
        // visible from the other (sanity check on Arc sharing).
        let (tool1, store) = make();
        let tool2 = MemoryTool::new(store);
        run(
            &tool1,
            serde_json::json!({"action": "add", "target": "user", "content": "shared fact"}),
        )
        .await;
        let read = run(
            &tool2,
            serde_json::json!({"action": "read", "target": "user"}),
        )
        .await;
        assert!(read.output.contains("shared fact"));
    }
}
