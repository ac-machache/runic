//! `runic-tool` — Layer 2 tool contract.
//!
//! The base `Tool` trait + `ToolResult` + `ToolSpec` are copied from **ZeroClaw**
//! (`zeroclaw-api/src/tool.rs`) — the only reference with a clean, *pluggable*
//! tool trait (OpenFang has none; its tools are a hardcoded `match`). Two
//! deliberate changes:
//!
//! 1. **`Attributable` supertrait dropped** — provenance is deferred.
//! 2. **`execute` extended to accept a [`ToolContext`]** — the runtime context,
//!    modeled on OpenFang's `KernelHandle` idea (a tool reaches identity,
//!    per-run config, and runtime handles when it needs them), generalized to a
//!    typed handle bag so it isn't welded to a fixed capability set.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

/// Result of executing a tool. Tool-level failures are reported in-band via
/// `success`/`error`; the `Result` wrapper is for unexpected execution errors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// Whether the tool succeeded.
    pub success: bool,
    /// The output content (shown to the model).
    pub output: String,
    /// Error detail when `success` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When set, this (a summary) is persisted to the event log instead of
    /// `output`; the full `output` reaches the model only for the immediate
    /// next call. Lets a tool feed the model a large payload (e.g. a re-read
    /// file) without writing the bytes into history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persisted_output: Option<String>,
}

impl ToolResult {
    /// A successful result.
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            success: true,
            output: output.into(),
            error: None,
            persisted_output: None,
        }
    }
    /// A failed result (the message is both the output and the error).
    pub fn error(message: impl Into<String>) -> Self {
        let m = message.into();
        Self {
            success: false,
            output: m.clone(),
            error: Some(m),
            persisted_output: None,
        }
    }
    /// Persist `summary` to the log instead of the full `output`.
    pub fn with_persisted_summary(mut self, summary: impl Into<String>) -> Self {
        self.persisted_output = Some(summary.into());
        self
    }
}

/// The LLM-facing spec for a tool (function-calling registration).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// A request→reply channel to the human operating the agent. The surface
/// provides this **per run** (the agent is pooled across requests / users), so
/// it flows through the run context, not the build-time agent. `ask_user` and
/// `escalate_to_human` reach it via [`ToolContext::human`].
#[async_trait]
pub trait HumanInterface: Send + Sync {
    /// Ask the user a question and wait for their answer.
    async fn ask(&self, question: &str, context: Option<&str>) -> anyhow::Result<String>;
    /// Escalate to a human operator — notify, no reply expected.
    async fn escalate(&self, reason: &str, detail: Option<&str>) -> anyhow::Result<()>;
}

/// Runtime context handed to a tool at execution — the extension over
/// ZeroClaw's stateless base. A tool reaches identity, per-run config, and
/// runtime handles only when it needs them.
///
/// `bag` is the generalized form of OpenFang's `KernelHandle`: stash any typed
/// handle (a DB pool, an approver, and — once it exists — a kernel/runtime
/// handle) and fetch it by type.
#[derive(Default)]
pub struct ToolContext {
    /// Owning user (the tenant axis).
    pub user_id: String,
    /// The conversation id.
    pub session_id: String,
    /// The current run id.
    pub run_id: String,
    /// Typed runtime handles, keyed by type.
    bag: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
    /// Open per-run config map (request-scoped values the app sets).
    config: serde_json::Map<String, serde_json::Value>,
    /// The human channel for this run, if the surface wired one.
    human: Option<Arc<dyn HumanInterface>>,
}

impl ToolContext {
    /// A context keyed by `(user_id, session_id)` for a given run.
    pub fn new(
        user_id: impl Into<String>,
        session_id: impl Into<String>,
        run_id: impl Into<String>,
    ) -> Self {
        Self {
            user_id: user_id.into(),
            session_id: session_id.into(),
            run_id: run_id.into(),
            bag: HashMap::new(),
            config: serde_json::Map::new(),
            human: None,
        }
    }

    /// Attach the per-run human channel (builder-style).
    pub fn with_human(mut self, human: Option<Arc<dyn HumanInterface>>) -> Self {
        self.human = human;
        self
    }

    /// The human channel for this run, if the surface wired one.
    pub fn human(&self) -> Option<Arc<dyn HumanInterface>> {
        self.human.clone()
    }

    /// Stash a typed runtime handle.
    pub fn insert<T: 'static + Send + Sync>(&mut self, value: T) {
        self.bag.insert(TypeId::of::<T>(), Arc::new(value));
    }

    /// Stash an already-shared typed handle.
    pub fn insert_arc<T: 'static + Send + Sync>(&mut self, value: Arc<T>) {
        self.bag.insert(TypeId::of::<T>(), value);
    }

    /// Fetch a typed runtime handle.
    pub fn get<T: 'static + Send + Sync>(&self) -> Option<Arc<T>> {
        self.bag
            .get(&TypeId::of::<T>())
            .and_then(|v| v.clone().downcast::<T>().ok())
    }

    /// Read a per-run config value.
    pub fn config(&self, key: &str) -> Option<&serde_json::Value> {
        self.config.get(key)
    }

    /// Read + deserialize a per-run config value.
    pub fn config_as<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.config
            .get(key)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// The full per-run config map (e.g. to propagate to a delegated child).
    pub fn config_map(&self) -> &serde_json::Map<String, serde_json::Value> {
        &self.config
    }

    /// Set the per-run config map (builder-style).
    pub fn with_config(mut self, config: serde_json::Map<String, serde_json::Value>) -> Self {
        self.config = config;
        self
    }
}

/// The base tool contract — implement this to give the model a capability.
/// Pluggable (a trait, unlike OpenFang) and state-aware (gets a `ctx`, unlike
/// ZeroClaw's base).
#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool name (used in LLM function calling).
    fn name(&self) -> &str;

    /// Human-readable description for the model.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's parameters.
    fn parameters_schema(&self) -> serde_json::Value;

    /// Whether this tool is safe to run concurrently with other tools in the
    /// same turn. Read-only tools (search, fetch, file reads) return `true`;
    /// tools with side effects or that gate on approval return `false`
    /// (the default). The loop batches `parallelizable` calls via `join_all`
    /// and runs the rest serially.
    fn parallelizable(&self) -> bool {
        false
    }

    /// Execute the tool with `args`, reaching the runtime via `ctx`.
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult>;

    /// Full spec for LLM registration.
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters_schema(),
        }
    }
}

/// A set of tools activated on demand during a conversation — e.g. an MCP
/// `tool_search` that fetches schemas lazily so hundreds of remote tools don't
/// bloat every prompt. Generic over any [`Tool`]; the loop reads [`specs`] when
/// assembling each request and resolves calls via [`get_resolved`]. Wrap in
/// `Arc<Mutex<…>>` to share between the activating tool and the loop.
///
/// [`specs`]: ActivatedToolSet::specs
/// [`get_resolved`]: ActivatedToolSet::get_resolved
#[derive(Default)]
pub struct ActivatedToolSet {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ActivatedToolSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark a tool active under `name` (idempotent — re-activating replaces).
    pub fn activate(&mut self, name: impl Into<String>, tool: Arc<dyn Tool>) {
        self.tools.insert(name.into(), tool);
    }

    /// Whether `name` is already active.
    pub fn is_activated(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Exact lookup.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Resolve a call name, with a unique-suffix fallback: some providers strip
    /// a `server__` prefix after a tool search, so `read_file` can resolve to
    /// the single activated `fs__read_file`. Ambiguous suffixes resolve to None.
    pub fn get_resolved(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if let Some(tool) = self.tools.get(name) {
            return Some(tool.clone());
        }
        let suffix = format!("__{name}");
        let mut hit = None;
        for (key, tool) in &self.tools {
            if key.ends_with(&suffix) {
                if hit.is_some() {
                    return None; // ambiguous
                }
                hit = Some(tool.clone());
            }
        }
        hit
    }

    /// LLM-facing specs for every activated tool (rebuilt into each request).
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    #[async_trait]
    impl Tool for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes its input back."
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object", "additionalProperties": true })
        }
        async fn execute(
            &self,
            args: serde_json::Value,
            ctx: &ToolContext,
        ) -> anyhow::Result<ToolResult> {
            // Demonstrates reaching the runtime context (per-run config).
            let user = ctx
                .config("user_id")
                .and_then(|v| v.as_str())
                .unwrap_or(&ctx.user_id);
            Ok(ToolResult::ok(format!("{user}: {args}")))
        }
    }

    #[tokio::test]
    async fn tool_executes_with_ctx_and_spec() {
        let t = Echo;
        let ctx = ToolContext::new("u1", "s1", "r1");
        let r = t
            .execute(serde_json::json!({ "x": 1 }), &ctx)
            .await
            .unwrap();
        assert!(r.success);
        assert!(r.output.starts_with("u1:"));
        assert_eq!(t.spec().name, "echo");
        assert_eq!(t.spec().parameters["type"], "object");
    }

    #[test]
    fn ctx_bag_round_trips_typed_handles() {
        #[derive(Debug, PartialEq)]
        struct DbPool(u32);
        let mut ctx = ToolContext::new("u", "s", "r");
        ctx.insert(DbPool(5));
        assert_eq!(*ctx.get::<DbPool>().unwrap(), DbPool(5));
        assert!(ctx.get::<String>().is_none());
    }

    #[test]
    fn tool_result_helpers() {
        assert!(ToolResult::ok("hi").success);
        let e = ToolResult::error("boom");
        assert!(!e.success);
        assert_eq!(e.error.as_deref(), Some("boom"));
    }

    #[test]
    fn activated_set_resolves_exact_and_unique_suffix() {
        let mut set = ActivatedToolSet::new();
        set.activate("fs__read_file", Arc::new(Echo));
        assert!(set.is_activated("fs__read_file"));
        // exact
        assert!(set.get("fs__read_file").is_some());
        // unique-suffix fallback (provider dropped the `fs__` prefix)
        assert!(set.get_resolved("read_file").is_some());
        // ambiguous suffix → None
        set.activate("net__read_file", Arc::new(Echo));
        assert!(set.get_resolved("read_file").is_none());
        assert_eq!(set.specs().len(), 2);
    }
}
