//! Per-user memory for Maia, rooted at `runic-data/{user_id}`.
//!
//! `user_id` is per-RUN (it rides in `RunContext.config`, not known at agent
//! build time), so both the memory tool and the memory context-layer resolve
//! it from the context at call time and route to a `RootedBackend` at
//! `runic-data/{user_id}`. Files land at
//! `runic-data/{user_id}/memory/MEMORY.md` and `…/memory/USER.md`.
//!
//! Reuses the framework's `runic_memory::{MemoryTool, BoundedMemoryStore}`
//! and `runic_context_engine::{MemoryLayer, UserFactsLayer}` — this module
//! only adds the per-user routing on top.

use std::sync::Arc;

use async_trait::async_trait;
use runic_context_engine::{ContextLayer, MemoryLayer, TurnContext, UserFactsLayer};
use runic_memory::store::{MEMORY_KEY, USER_KEY};
use runic_memory::{BoundedMemoryStore, MemoryTool};
use runic_storage_backend::{MemoryBackend, RootedBackend, StorageBackend};
use runic_tool_core::{Tool, ToolContext, ToolResult};

use super::context::KEY_USER_ID;

/// Resolve the per-run `user_id` from the tool/turn context.
fn user_id<'a>(config: impl Fn(&str) -> Option<&'a serde_json::Value>) -> Option<String> {
    config(KEY_USER_ID)
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

/// The `memory` tool, routed to the calling user's store. Same schema /
/// behaviour as `runic_memory::MemoryTool`, but the backing files are
/// per-user (`runic-data/{user_id}/memory/…`) resolved from `ctx.config`.
pub struct UserMemoryTool {
    base: Arc<dyn StorageBackend>,
    /// A throwaway `MemoryTool` over an in-memory store, used ONLY to expose
    /// the real tool's name / description / input schema (those are static).
    meta: MemoryTool,
}

impl UserMemoryTool {
    pub fn new(base: Arc<dyn StorageBackend>) -> Self {
        let dummy = Arc::new(BoundedMemoryStore::new(Arc::new(MemoryBackend::new())));
        Self {
            base,
            meta: MemoryTool::new(dummy),
        }
    }

    fn store_for(&self, uid: &str) -> Arc<BoundedMemoryStore> {
        let rooted: Arc<dyn StorageBackend> =
            Arc::new(RootedBackend::new(self.base.clone(), uid.to_string()));
        Arc::new(BoundedMemoryStore::new(rooted))
    }
}

#[async_trait]
impl Tool for UserMemoryTool {
    fn name(&self) -> &str {
        self.meta.name()
    }
    fn description(&self) -> &str {
        self.meta.description()
    }
    fn input_schema(&self) -> serde_json::Value {
        self.meta.input_schema()
    }
    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let Some(uid) = user_id(|k| ctx.config(k)) else {
            return ToolResult::error(
                "memory: no user_id in the run context — cannot persist memory for an \
                 anonymous request.",
            );
        };
        MemoryTool::new(self.store_for(&uid)).execute(input, ctx).await
    }
}

/// Context layer that injects the calling user's `MEMORY.md` + `USER.md`
/// into the prompt each turn, resolved from `ctx.config["user_id"]`. Renders
/// nothing when there's no user_id or the files are empty/missing.
pub struct UserMemoryLayer {
    base: Arc<dyn StorageBackend>,
}

impl UserMemoryLayer {
    pub fn new(base: Arc<dyn StorageBackend>) -> Self {
        Self { base }
    }
}

#[async_trait]
impl ContextLayer for UserMemoryLayer {
    fn name(&self) -> &str {
        "user_memory"
    }

    async fn render(&self, ctx: &TurnContext<'_>) -> Option<String> {
        let uid = user_id(|k| ctx.config.get(k))?;
        let rooted: Arc<dyn StorageBackend> =
            Arc::new(RootedBackend::new(self.base.clone(), uid));

        // Reuse the framework layers (hot-reload) over the per-user backend.
        let mem = MemoryLayer::new(rooted.clone(), MEMORY_KEY);
        let usr = UserFactsLayer::new(rooted, USER_KEY);

        let mut parts: Vec<String> = Vec::new();
        if let Some(m) = mem.render(ctx).await {
            parts.push(m);
        }
        if let Some(u) = usr.render(ctx).await {
            parts.push(u);
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_tool(uid: Option<&str>) -> ToolContext {
        let mut map = serde_json::Map::new();
        if let Some(u) = uid {
            map.insert("user_id".into(), serde_json::json!(u));
        }
        ToolContext::new("s".into(), "r".into(), 0, Default::default()).with_config(map)
    }

    #[tokio::test]
    async fn memory_tool_persists_per_user_and_layer_reads_it_back() {
        let base: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let tool = UserMemoryTool::new(base.clone());

        // Save a user fact for u1.
        let r = tool
            .execute(
                serde_json::json!({"action": "add", "target": "user", "content": "name: Manu"}),
                &ctx_tool(Some("u1")),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);

        // The layer renders it back for u1...
        let layer = UserMemoryLayer::new(base.clone());
        let cfg_u1: serde_json::Map<String, serde_json::Value> =
            [("user_id".to_string(), serde_json::json!("u1"))].into_iter().collect();
        let tc = TurnContext {
            base_system_prompt: "",
            messages: &[],
            run_id: "r1",
            turn: 0,
            config: &cfg_u1,
        };
        let out = layer.render(&tc).await.unwrap_or_default();
        assert!(out.contains("Manu"), "expected user fact in render: {out}");

        // ...but NOT for a different user.
        let cfg_u2: serde_json::Map<String, serde_json::Value> =
            [("user_id".to_string(), serde_json::json!("u2"))].into_iter().collect();
        let tc2 = TurnContext {
            base_system_prompt: "",
            messages: &[],
            run_id: "r1",
            turn: 0,
            config: &cfg_u2,
        };
        assert!(layer.render(&tc2).await.is_none(), "u2 must not see u1's memory");
    }

    #[tokio::test]
    async fn memory_tool_errors_without_user_id() {
        let tool = UserMemoryTool::new(Arc::new(MemoryBackend::new()));
        let r = tool
            .execute(
                serde_json::json!({"action": "add", "target": "user", "content": "x"}),
                &ctx_tool(None),
            )
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("no user_id"));
    }
}
