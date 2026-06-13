use async_trait::async_trait;
use runic_message_types::{ToolCall, ToolDefinition};
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

/// Output of a tool execution.
///
/// A result has two consumers with different needs: `content` is what the
/// MODEL reads (plain text, costs tokens); `metadata` is what the CLIENT
/// renders (structured JSON — search-result links for grounding chips,
/// exec exit codes, thumbnail refs, …). Metadata never reaches the
/// provider: adapters build API payloads field-by-field and don't copy it.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
    /// Client-facing structured payload. Free-form by design — each tool
    /// ships whatever its UI needs, the core stays schema-agnostic.
    pub metadata: Option<serde_json::Value>,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            metadata: None,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            metadata: None,
        }
    }

    /// Attach client-facing metadata (builder-style).
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// Errors that can be surfaced by [`ToolRegistry::dispatch`]. Kept tiny and
/// crate-local so this crate does not depend on the agent-core error type.
#[derive(Debug, thiserror::Error)]
pub enum ToolDispatchError {
    #[error("tool '{tool}' not found")]
    UnknownTool { tool: String },
}

/// Read-only context passed into tool executions.
///
/// Carries identifying metadata plus a typed runtime bag for handles
/// registered via `AgentBuilder::runtime(...)` (DB pools, user info, etc).
/// Tools that don't need anything just take `_ctx: &ToolContext`.
#[derive(Clone)]
pub struct ToolContext {
    pub session_id: String,
    pub run_id: String,
    pub turn: u32,
    bag: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl ToolContext {
    pub fn new(
        session_id: String,
        run_id: String,
        turn: u32,
        bag: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
    ) -> Self {
        Self {
            session_id,
            run_id,
            turn,
            bag,
        }
    }

    /// Fetch a typed handle that was registered via `AgentBuilder::runtime(...)`.
    pub fn get<T: 'static + Send + Sync>(&self) -> Option<Arc<T>> {
        self.bag
            .get(&TypeId::of::<T>())
            .and_then(|v| v.clone().downcast::<T>().ok())
    }
}

// ─── Plain tool trait ───────────────────────────────────────────────────────

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema for the tool's input.
    fn input_schema(&self) -> serde_json::Value;

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult;
}

// ─── Universal dispatch interface ───────────────────────────────────────────

/// What the registry actually stores. Anything that can be invoked as a tool
/// implements this. Each tool *kind* (plain, HITL-gated, future streaming,
/// batched, …) gets a small adapter that converts its specific trait into this
/// universal one. Adding a new kind means writing a new adapter — the registry
/// and dispatch path are untouched.
#[async_trait]
pub trait ToolDispatch: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> serde_json::Value;
    async fn dispatch(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult;
    /// Whether this tool is safe to run concurrently with sibling tool calls
    /// in the same turn. Default `true` — Rust's ownership rules make it safe
    /// by construction for plain tools. `HitlAdapter` overrides to `false`
    /// because parallel HITL prompts would spam the user.
    fn parallelizable(&self) -> bool {
        true
    }
}

// ─── Adapter: plain Tool → ToolDispatch ─────────────────────────────────────

pub struct PlainAdapter<T: Tool>(pub Arc<T>);

#[async_trait]
impl<T: Tool + 'static> ToolDispatch for PlainAdapter<T> {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn description(&self) -> &str {
        self.0.description()
    }
    fn input_schema(&self) -> serde_json::Value {
        self.0.input_schema()
    }
    async fn dispatch(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult {
        self.0.execute(call.input.clone(), ctx).await
    }
}

// ─── Adapter: HitlTool → ToolDispatch (carries the approval flow) ───────────

pub struct HitlAdapter<T: crate::approval::HitlTool>(pub Arc<T>);

#[async_trait]
impl<T: crate::approval::HitlTool + 'static> ToolDispatch for HitlAdapter<T> {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn description(&self) -> &str {
        self.0.description()
    }
    fn input_schema(&self) -> serde_json::Value {
        self.0.input_schema()
    }
    fn parallelizable(&self) -> bool {
        // HITL prompts must serialize — never spam the user with 3 simultaneous approvals.
        false
    }
    async fn dispatch(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult {
        let approver = match ctx.get::<crate::approval::ApproverHandle>() {
            Some(a) => a,
            None => {
                return ToolResult::error(format!(
                    "{}: no Approver registered in runtime context",
                    call.name
                ))
            }
        };

        let draft = self.0.draft(&call.input);
        let request = crate::approval::ApprovalRequest {
            tool_name: call.name.clone(),
            call_id: call.id.clone(),
            run_id: ctx.run_id.clone(),
            session_id: ctx.session_id.clone(),
            draft,
        };

        match approver.review(request).await {
            crate::approval::UserDecision::Submit { final_input } => {
                self.0.execute(final_input, ctx).await
            }
            crate::approval::UserDecision::Cancel { reason } => {
                ToolResult::error(format!("{} cancelled: {}", call.name, reason))
            }
        }
    }
}

// ─── Registry ───────────────────────────────────────────────────────────────

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn ToolDispatch>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a plain Tool. Wraps it in `PlainAdapter` internally.
    pub fn register<T: Tool + 'static>(&mut self, tool: Arc<T>) {
        let adapter: Arc<dyn ToolDispatch> = Arc::new(PlainAdapter(tool));
        self.tools.insert(adapter.name().to_string(), adapter);
    }

    /// Register a BackgroundTool. Wraps it in `BackgroundAdapter` so the registry stays
    /// uniform (everything is `Arc<dyn ToolDispatch>`).
    pub fn register_background<T: crate::background::BackgroundTool + 'static>(
        &mut self,
        tool: Arc<T>,
    ) {
        let adapter: Arc<dyn ToolDispatch> = Arc::new(crate::background::BackgroundAdapter(tool));
        self.tools.insert(adapter.name().to_string(), adapter);
    }

    /// Register a HitlTool. Wraps it in `HitlAdapter` so the registry stays
    /// uniform (everything is `Arc<dyn ToolDispatch>`).
    pub fn register_hitl<T: crate::approval::HitlTool + 'static>(&mut self, tool: Arc<T>) {
        let adapter: Arc<dyn ToolDispatch> = Arc::new(HitlAdapter(tool));
        self.tools.insert(adapter.name().to_string(), adapter);
    }

    /// Insert an already-wrapped `Arc<dyn ToolDispatch>` directly. Used
    /// when assembling a filtered registry from a pre-existing pool —
    /// e.g. sub-agents that take a subset of the parent's tools without
    /// re-wrapping them in adapters.
    pub fn insert_dispatch(&mut self, dispatch: Arc<dyn ToolDispatch>) {
        self.tools.insert(dispatch.name().to_string(), dispatch);
    }

    /// Remove the dispatch registered under `name`. Returns the removed
    /// `Arc<dyn ToolDispatch>`, or `None` if no tool with that name
    /// existed. Used by sub-agent setup to strip a class of tools the
    /// child shouldn't have (e.g. shell tools when the AGENT.md
    /// declares `filesystem.mode: none`).
    pub fn remove(&mut self, name: &str) -> Option<Arc<dyn ToolDispatch>> {
        self.tools.remove(name)
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn ToolDispatch>> {
        self.tools.get(name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tools.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Build provider-facing tool definitions for every registered tool.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut defs: Vec<ToolDefinition> = self
            .tools
            .values()
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect();
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs
    }

    /// Dispatch a tool call. The registry doesn't know or care which kind
    /// of tool it's invoking — that's hidden behind `ToolDispatch`.
    pub async fn dispatch(
        &self,
        call: &ToolCall,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolDispatchError> {
        let tool = self
            .get(&call.name)
            .ok_or_else(|| ToolDispatchError::UnknownTool {
                tool: call.name.clone(),
            })?;
        Ok(tool.dispatch(call, ctx).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    #[derive(Debug, PartialEq)]
    struct DbHandle(&'static str);

    fn empty_ctx() -> ToolContext {
        ToolContext::new("sess".into(), "run".into(), 0, HashMap::new())
    }

    #[test]
    fn tool_result_constructors_carry_no_metadata() {
        assert!(ToolResult::ok("fine").metadata.is_none());
        assert!(ToolResult::error("boom").metadata.is_none());
    }

    #[test]
    fn tool_result_with_metadata_attaches_payload() {
        let result = ToolResult::ok("3 results").with_metadata(serde_json::json!({
            "sources": [{ "title": "Rust blog", "url": "https://blog.rust-lang.org" }]
        }));
        assert!(!result.is_error);
        assert_eq!(result.content, "3 results");
        assert_eq!(
            result.metadata.unwrap()["sources"][0]["url"],
            "https://blog.rust-lang.org"
        );
    }

    #[test]
    fn tool_context_get_returns_inserted_typed_value() {
        let mut bag: HashMap<TypeId, Arc<dyn Any + Send + Sync>> = HashMap::new();
        bag.insert(TypeId::of::<DbHandle>(), Arc::new(DbHandle("pg://prod")));

        let ctx = ToolContext::new("sess".into(), "run".into(), 1, bag);
        let db = ctx.get::<DbHandle>().expect("db present");
        assert_eq!(*db, DbHandle("pg://prod"));
    }

    #[test]
    fn tool_context_get_is_none_when_missing() {
        let ctx = empty_ctx();
        assert!(ctx.get::<DbHandle>().is_none());
    }

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echoes input.msg"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": { "msg": { "type": "string" } },
                "required": ["msg"]
            })
        }
        async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
            let msg = input["msg"].as_str().unwrap_or("(none)");
            ToolResult::ok(format!("echo: {msg}"))
        }
    }

    #[tokio::test]
    async fn registry_dispatches_to_registered_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        let ctx = empty_ctx();

        let call = ToolCall {
            id: "test-1".into(),
            name: "echo".into(),
            input: serde_json::json!({"msg": "hi"}),
            intent: None,
        };
        let result = reg.dispatch(&call, &ctx).await.expect("dispatch succeeds");

        assert!(!result.is_error);
        assert_eq!(result.content, "echo: hi");
    }

    #[tokio::test]
    async fn registry_dispatch_unknown_tool_errors() {
        let reg = ToolRegistry::new();
        let ctx = empty_ctx();

        let call = ToolCall {
            id: "test-2".into(),
            name: "missing".into(),
            input: serde_json::json!({}),
            intent: None,
        };
        let err = reg.dispatch(&call, &ctx).await.unwrap_err();

        match err {
            ToolDispatchError::UnknownTool { tool } => assert_eq!(tool, "missing"),
        }
    }

    #[test]
    fn registry_definitions_are_sorted_by_name() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        let defs = reg.definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "echo");
    }

    #[test]
    fn registry_names_are_sorted() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        assert_eq!(reg.names(), vec!["echo"]);
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_empty());
    }
}
