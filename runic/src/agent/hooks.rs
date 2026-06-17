//! Tool-level interceptors for the coral agent.
//!
//! These are [`ToolInterceptor`]s, not agent `Hook`s — they're attached to
//! the toolbox / Tavily tools themselves (see `MaiaFactory`), so they fire
//! for **whichever agent** dispatches the tool, parent or sub-agent. They
//! read the per-run context off the `ToolContext` (`ctx.config`), which the
//! agent loop populates from the request's `RunContext` and sub-agents
//! inherit from their parent. The attachment predicate decides which tools
//! get them, so the interceptors themselves are unconditional.

use super::context::{KEY_ALLOW_WEB_SEARCH, KEY_ORG_ID, KEY_USER_ID};
use runic_message_types::ToolCall;
use runic_tool_core::{ToolContext, ToolInterceptor, ToolResult};

/// Stamps the per-run `user_id` / `org_id` onto a toolbox tool call so the
/// toolbox scopes data to the right tenant. The model never sees or supplies
/// these — they're injected here, server-side, from the request context.
/// Attached only to `mcp__toolbox__*` tools by the factory.
pub struct BindToolContext;

#[async_trait::async_trait]
impl ToolInterceptor for BindToolContext {
    fn name(&self) -> &str {
        "bind_tool_context"
    }

    async fn before(&self, call: &mut ToolCall, ctx: &ToolContext) -> Option<ToolResult> {
        let user_id = ctx.config(KEY_USER_ID).and_then(|v| v.as_str()).map(str::to_string);
        let org_id = ctx.config(KEY_ORG_ID).and_then(|v| v.as_str()).map(str::to_string);

        if let Some(obj) = call.input.as_object_mut() {
            if let Some(u) = user_id {
                obj.insert(KEY_USER_ID.into(), serde_json::Value::String(u));
            }
            if let Some(o) = org_id {
                obj.insert(KEY_ORG_ID.into(), serde_json::Value::String(o));
            }
        }
        None // proceed to the tool
    }
}

/// Fail-closed gate on the Tavily web-search tools. The tools are always
/// registered (so the model knows web search exists), but a call only
/// proceeds when the per-run config has `allow_web_search = true`. Missing
/// key or `false` short-circuits with an error. Attached only to
/// `mcp__tavily__*` tools by the factory. Mirrors coral's
/// `WebSearchGuardMiddleware`.
pub struct WebSearchGuard;

#[async_trait::async_trait]
impl ToolInterceptor for WebSearchGuard {
    fn name(&self) -> &str {
        "web_search_guard"
    }

    async fn before(&self, _call: &mut ToolCall, ctx: &ToolContext) -> Option<ToolResult> {
        let allowed = ctx
            .config(KEY_ALLOW_WEB_SEARCH)
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if allowed {
            None
        } else {
            Some(ToolResult::error(
                "Recherche web non autorisée pour cette requête \
                 (le TC n'a pas activé l'accès web).",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            input: serde_json::json!({}),
            intent: None,
        }
    }

    fn ctx_with(config: serde_json::Value) -> ToolContext {
        let map = match config {
            serde_json::Value::Object(m) => m,
            _ => serde_json::Map::new(),
        };
        ToolContext::new("s".into(), "r".into(), 0, Default::default()).with_config(map)
    }

    #[tokio::test]
    async fn bind_stamps_user_and_org_from_config() {
        let mut c = call("mcp__toolbox__search_farms");
        let out = BindToolContext
            .before(&mut c, &ctx_with(serde_json::json!({"user_id": "u1", "org_id": "o1"})))
            .await;
        assert!(out.is_none());
        assert_eq!(c.input["user_id"], "u1");
        assert_eq!(c.input["org_id"], "o1");
    }

    #[tokio::test]
    async fn bind_is_noop_without_config() {
        let mut c = call("mcp__toolbox__search_farms");
        BindToolContext.before(&mut c, &ctx_with(serde_json::json!({}))).await;
        assert!(c.input.get("user_id").is_none());
        assert!(c.input.get("org_id").is_none());
    }

    #[tokio::test]
    async fn guard_blocks_when_no_config() {
        let mut c = call("mcp__tavily__search");
        let out = WebSearchGuard.before(&mut c, &ctx_with(serde_json::json!({}))).await;
        assert!(out.is_some_and(|r| r.is_error));
    }

    #[tokio::test]
    async fn guard_blocks_when_denied() {
        let mut c = call("mcp__tavily__search");
        let out = WebSearchGuard
            .before(&mut c, &ctx_with(serde_json::json!({"allow_web_search": false})))
            .await;
        assert!(out.is_some_and(|r| r.is_error));
    }

    #[tokio::test]
    async fn guard_allows_when_permitted() {
        let mut c = call("mcp__tavily__search");
        let out = WebSearchGuard
            .before(&mut c, &ctx_with(serde_json::json!({"allow_web_search": true})))
            .await;
        assert!(out.is_none());
    }
}
