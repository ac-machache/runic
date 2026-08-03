use std::sync::Arc;

use runic_macros::tool;
use runic_store::SessionStore;
use runic_tool::{ToolContext, ToolResult};

const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 25;

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SearchChatsArgs {
    #[schemars(description = "Words to look for in earlier conversations.")]
    query: String,
    #[serde(default)]
    #[schemars(description = "How many matches to return.")]
    limit: Option<usize>,
}

#[tool(
    name = "search_chats",
    args = SearchChatsArgs,
    description = "Search this user's earlier conversations for a phrase and get back the \
                   matching sessions with a snippet around each hit. The current conversation \
                   is excluded — you can already see it."
)]
pub struct SearchChats {
    sessions: Arc<dyn SessionStore>,
    default_limit: usize,
    max_limit: usize,
}

impl SearchChats {
    pub fn new(sessions: Arc<dyn SessionStore>) -> Self {
        Self {
            sessions,
            default_limit: DEFAULT_LIMIT,
            max_limit: MAX_LIMIT,
        }
    }

    pub fn default_limit(mut self, limit: usize) -> Self {
        self.default_limit = limit;
        self
    }

    pub fn max_limit(mut self, limit: usize) -> Self {
        self.max_limit = limit;
        self
    }

    async fn tool(&self, args: SearchChatsArgs, ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        let query = args.query.trim();
        if query.is_empty() {
            return Ok(ToolResult::error("query must not be empty"));
        }
        let limit = args.limit.unwrap_or(self.default_limit).min(self.max_limit);

        let hits = self
            .sessions
            .search(&ctx.user_id, query, limit, Some(&ctx.session_id))
            .await;

        let hits = match hits {
            Ok(hits) => hits,
            Err(runic_store::Error::Unsupported(_)) => {
                return Ok(ToolResult::error(
                    "this deployment's store cannot search earlier conversations",
                ));
            }
            Err(error) => return Ok(ToolResult::error(error.to_string())),
        };

        if hits.is_empty() {
            return Ok(ToolResult::ok(format!(
                "no earlier chat mentions {query:?}"
            )));
        }

        let mut rendered = String::new();
        for hit in &hits {
            rendered.push_str(&format!(
                "- session {} ({}, {}): {}\n",
                hit.session_id,
                hit.role,
                hit.at.format("%Y-%m-%d"),
                hit.snippet.trim()
            ));
        }
        Ok(ToolResult::ok(rendered))
    }
}
