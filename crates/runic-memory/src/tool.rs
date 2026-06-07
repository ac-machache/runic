//! `MemoryTool` — the `memory` tool the agent calls to update its own
//! notes. Wraps [`BoundedMemoryStore`] in the [`Tool`] trait.
//!
//! Single tool, multi-action: the JSON schema dispatches on `action` +
//! `target`. Matches the hermes shape exactly so prompts/skills authored
//! against hermes work here too.

use std::sync::Arc;

use async_trait::async_trait;
use runic_tool_core::{Tool, ToolContext, ToolResult};
use serde_json::Value;

use crate::error::MemoryError;
use crate::store::{BoundedMemoryStore, Target};

const DESCRIPTION: &str = "Persistent curated memory you control. Two stores:\n\
- target='memory' (MEMORY.md): your own notes — environment facts, project conventions, tool quirks, things you learned. Cap 2200 chars.\n\
- target='user' (USER.md): facts about the user — preferences, communication style, expectations, workflow habits. Cap 1375 chars.\n\
\n\
Both files are auto-injected into your system prompt at the start of every turn. Mid-session writes are persisted to disk immediately; the next turn picks them up.\n\
\n\
Actions:\n\
- action='read', target=<store>: list current entries.\n\
- action='add', target=<store>, content='<single fact>': append a pithy single-fact entry. Idempotent (re-adding the same content does nothing). Errors if it would breach the cap — `remove` or `replace` first.\n\
- action='remove', target=<store>, search='<unique substring>': delete one entry. The substring must match exactly one entry — narrow it on ambiguity.\n\
- action='replace', target=<store>, search='<unique substring>', replacement='<new content>': swap one entry. Same uniqueness rule as remove.\n\
\n\
Style: each entry is one short self-contained line. Don't write paragraphs. Don't add timestamps or dates — they age badly. Don't duplicate things already in your persona (SOUL.md).";

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

    fn input_schema(&self) -> Value {
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

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        match self.dispatch(input).await {
            Ok(out) => ToolResult::ok(out),
            Err(err) => ToolResult::error(err.to_string()),
        }
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
                let replacement = input
                    .get("replacement")
                    .and_then(Value::as_str)
                    .ok_or(MemoryError::MissingField { field: "replacement" })?;
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
    use runic_storage_backend::{MemoryBackend, StorageBackend};
    use runic_tool_core::ToolContext;
    use std::sync::Arc;

    fn make() -> (MemoryTool, Arc<BoundedMemoryStore>) {
        let backend: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let store = Arc::new(BoundedMemoryStore::new(backend));
        (MemoryTool::new(store.clone()), store)
    }

    fn ctx() -> ToolContext {
        ToolContext::new("session-1".into(), "run-1".into(), 0, Default::default())
    }

    #[tokio::test]
    async fn add_then_read_via_tool() {
        let (tool, _) = make();
        let add = tool
            .execute(
                serde_json::json!({"action": "add", "target": "user", "content": "lives in Paris"}),
                &ctx(),
            )
            .await;
        assert!(!add.is_error, "{:?}", add.content);

        let read = tool
            .execute(
                serde_json::json!({"action": "read", "target": "user"}),
                &ctx(),
            )
            .await;
        assert!(!read.is_error);
        assert!(read.content.contains("lives in Paris"));
        assert!(read.content.contains("1 entries"));
    }

    #[tokio::test]
    async fn add_missing_content_errors() {
        let (tool, _) = make();
        let result = tool
            .execute(
                serde_json::json!({"action": "add", "target": "user"}),
                &ctx(),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("content"));
    }

    #[tokio::test]
    async fn invalid_action_errors() {
        let (tool, _) = make();
        let result = tool
            .execute(
                serde_json::json!({"action": "yeet", "target": "memory"}),
                &ctx(),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("yeet"));
    }

    #[tokio::test]
    async fn invalid_target_errors() {
        let (tool, _) = make();
        let result = tool
            .execute(
                serde_json::json!({"action": "read", "target": "nope"}),
                &ctx(),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("nope"));
    }

    #[tokio::test]
    async fn read_empty_reports_empty() {
        let (tool, _) = make();
        let result = tool
            .execute(
                serde_json::json!({"action": "read", "target": "memory"}),
                &ctx(),
            )
            .await;
        assert!(!result.is_error);
        assert!(result.content.contains("empty"));
    }

    #[tokio::test]
    async fn full_flow_add_replace_remove_read() {
        let (tool, _) = make();
        tool.execute(
            serde_json::json!({"action": "add", "target": "memory", "content": "uses fish shell"}),
            &ctx(),
        )
        .await;
        tool.execute(
            serde_json::json!({"action": "replace", "target": "memory", "search": "fish", "replacement": "uses zsh shell"}),
            &ctx(),
        )
        .await;
        let read = tool
            .execute(
                serde_json::json!({"action": "read", "target": "memory"}),
                &ctx(),
            )
            .await;
        assert!(read.content.contains("zsh"));
        assert!(!read.content.contains("fish"));

        let removed = tool
            .execute(
                serde_json::json!({"action": "remove", "target": "memory", "search": "zsh"}),
                &ctx(),
            )
            .await;
        assert!(!removed.is_error);
        let after = tool
            .execute(
                serde_json::json!({"action": "read", "target": "memory"}),
                &ctx(),
            )
            .await;
        assert!(after.content.contains("empty"));
    }

    #[tokio::test]
    async fn shared_store_is_visible_across_tool_invocations() {
        // Same store passed twice → entries written by one tool clone are
        // visible from the other (sanity check on Arc sharing).
        let (tool1, store) = make();
        let tool2 = MemoryTool::new(store);
        tool1
            .execute(
                serde_json::json!({"action": "add", "target": "user", "content": "shared fact"}),
                &ctx(),
            )
            .await;
        let read = tool2
            .execute(
                serde_json::json!({"action": "read", "target": "user"}),
                &ctx(),
            )
            .await;
        assert!(read.content.contains("shared fact"));
    }
}
