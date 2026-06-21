//! `search_chats` — let the agent textually search the **same tenant's** other
//! conversations. Full-text (NOT semantic). Tenant scoping comes from the
//! [`ToolContext`], never the args, so an agent can't reach another tenant's
//! chats.

use std::sync::Arc;

use async_trait::async_trait;

use runic_tool::{Tool, ToolContext, ToolResult};

use crate::{Error, SessionStore};

const DEFAULT_LIMIT: usize = 10;

/// Searches conversations via a [`SessionStore`].
pub struct SearchChatsTool {
    store: Arc<dyn SessionStore>,
}

impl SearchChatsTool {
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for SearchChatsTool {
    fn name(&self) -> &str {
        "search_chats"
    }

    fn description(&self) -> &str {
        "Search your OTHER past conversations (same tenant) by keywords — a \
         textual full-text search, not semantic. Returns matching snippets and \
         their session ids so you can open one. Query syntax supports quoted \
         \"exact phrases\", OR, and -exclusion."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Keywords / phrase to search for." },
                "limit": { "type": "number", "description": "Max results (default 10)." }
            },
            "required": ["query"]
        })
    }

    // Read-only: safe to fan out alongside other tool calls.
    fn parallelizable(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if query.is_empty() {
            return Ok(ToolResult::error("search_chats requires a non-empty query"));
        }
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| usize::try_from(n).unwrap_or(DEFAULT_LIMIT))
            .unwrap_or(DEFAULT_LIMIT);

        // Tenant + current session come from the run context, not the model.
        let tenant = &ctx.user_id;
        let exclude = Some(ctx.session_id.as_str());

        match self.store.search(tenant, query, limit, exclude).await {
            Ok(hits) if hits.is_empty() => Ok(ToolResult::ok(
                "No matching messages in your other conversations.",
            )),
            Ok(hits) => {
                let mut out = format!("{} match(es):\n", hits.len());
                for h in hits {
                    out.push_str(&format!(
                        "- [session {}] {} @ {}: {}\n",
                        h.session_id,
                        h.role,
                        h.at.format("%Y-%m-%d %H:%M"),
                        h.snippet
                    ));
                }
                Ok(ToolResult::ok(out))
            }
            Err(Error::Unsupported(_)) => Ok(ToolResult::error(
                "chat search is not available with this store",
            )),
            Err(e) => Ok(ToolResult::error(format!("chat search failed: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemorySessionStore, SessionStore};
    use chrono::Utc;
    use runic_state::SessionEvent;
    use runic_types::Message;

    async fn seed(store: &MemorySessionStore, tenant: &str, session: &str, text: &str) {
        store
            .append(
                tenant,
                session,
                &SessionEvent::Message {
                    run_id: "r".into(),
                    msg: Message::user(text),
                    at: Utc::now(),
                },
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn searches_other_sessions_scoped_to_the_context_tenant() {
        let store = Arc::new(MemorySessionStore::new());
        seed(
            &store,
            "acme",
            "past",
            "the postgres migration failed badly",
        )
        .await;
        seed(&store, "acme", "current", "postgres status now").await;
        seed(&store, "other", "x", "postgres migration in another tenant").await;

        let tool = SearchChatsTool::new(store);
        // tenant + current session come from the context, not the args
        let ctx = ToolContext::new("acme", "current", "run1");

        let r = tool
            .execute(serde_json::json!({ "query": "postgres migration" }), &ctx)
            .await
            .unwrap();
        assert!(r.success);
        assert!(r.output.contains("past")); // matched the other acme session
        assert!(!r.output.contains("current")); // excluded the current one
        assert!(!r.output.contains("another tenant")); // never crossed tenants

        // empty query is rejected
        let r = tool
            .execute(serde_json::json!({ "query": "" }), &ctx)
            .await
            .unwrap();
        assert!(!r.success);
    }
}
