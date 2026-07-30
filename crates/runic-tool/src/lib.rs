//! Tool contracts and per-run execution context.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

pub use runic_types::ProvenanceSource;

/// Result of executing a tool. Tool-level failures are reported in-band via
/// `Failed`; the `Result` wrapper is for unexpected execution errors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResult {
    Done {
        output: serde_json::Value,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        provenance: Vec<ProvenanceSource>,
    },
    Failed {
        message: String,
    },
    Deferred {
        payload: serde_json::Value,
    },
}

impl ToolResult {
    pub fn ok(output: impl Into<serde_json::Value>) -> Self {
        Self::Done {
            output: output.into(),
            provenance: Vec::new(),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::Failed {
            message: message.into(),
        }
    }

    pub fn defer(payload: serde_json::Value) -> Self {
        Self::Deferred { payload }
    }

    pub fn with_provenance(mut self, sources: Vec<ProvenanceSource>) -> Self {
        if let Self::Done { provenance, .. } = &mut self {
            *provenance = sources;
        }
        self
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }

    pub fn output(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Done { output, .. } => Some(output),
            _ => None,
        }
    }

    pub fn text(&self) -> String {
        match self {
            Self::Done {
                output: serde_json::Value::String(text),
                ..
            } => text.clone(),
            Self::Done { output, .. } => output.to_string(),
            Self::Failed { message } => message.clone(),
            Self::Deferred { .. } => String::new(),
        }
    }

    pub fn push_notes(&mut self, notes: &[String]) {
        if notes.is_empty() {
            return;
        }
        match self {
            Self::Done {
                output: serde_json::Value::String(text),
                ..
            } => {
                for note in notes {
                    text.push_str("\n\n");
                    text.push_str(note);
                }
            }
            Self::Done { output, .. } => {
                let original = std::mem::take(output);
                *output = serde_json::json!({ "output": original, "notes": notes });
            }
            Self::Failed { message } => {
                for note in notes {
                    message.push_str("\n\n");
                    message.push_str(note);
                }
            }
            Self::Deferred { .. } => {}
        }
    }
}

/// The LLM-facing spec for a tool (function-calling registration).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Runtime context handed to a tool at execution.
#[derive(Default)]
pub struct ToolContext {
    pub user_id: String,
    pub session_id: String,
    pub run_id: String,
    bag: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
    config: serde_json::Map<String, serde_json::Value>,
    emitter: Option<Arc<dyn runic_state::Emitter>>,
    sub_session: Option<Arc<dyn runic_state::SubSession>>,
}

impl ToolContext {
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
            emitter: None,
            sub_session: None,
        }
    }

    pub fn with_emitter(mut self, emitter: Option<Arc<dyn runic_state::Emitter>>) -> Self {
        self.emitter = emitter;
        self
    }

    pub fn with_sub_session(
        mut self,
        sub_session: Option<Arc<dyn runic_state::SubSession>>,
    ) -> Self {
        self.sub_session = sub_session;
        self
    }

    pub fn sub_session(&self) -> Option<Arc<dyn runic_state::SubSession>> {
        self.sub_session.clone()
    }

    pub fn emit(&self, event: runic_state::AgentEvent) {
        if let Some(emitter) = &self.emitter {
            emitter.emit(event);
        }
    }

    pub fn emitter(&self) -> Option<Arc<dyn runic_state::Emitter>> {
        self.emitter.clone()
    }

    pub fn update(&self, key: impl Into<String>, value: serde_json::Value) {
        let key = key.into();
        if runic_state::validate_state_key(&key).is_err() {
            return;
        }
        let run_id = if self.run_id.is_empty() {
            "update".to_string()
        } else {
            self.run_id.clone()
        };
        self.emit(runic_state::AgentEvent::StateUpdated {
            run_id,
            key,
            value,
            at: chrono::Utc::now(),
        });
    }

    pub fn insert<T: 'static + Send + Sync>(&mut self, value: T) {
        self.bag.insert(TypeId::of::<T>(), Arc::new(value));
    }

    pub fn insert_arc<T: 'static + Send + Sync>(&mut self, value: Arc<T>) {
        self.bag.insert(TypeId::of::<T>(), value);
    }

    pub fn get<T: 'static + Send + Sync>(&self) -> Option<Arc<T>> {
        self.bag
            .get(&TypeId::of::<T>())
            .and_then(|v| v.clone().downcast::<T>().ok())
    }

    pub fn config(&self, key: &str) -> Option<&serde_json::Value> {
        self.config.get(key)
    }

    pub fn config_as<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.config
            .get(key)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    pub fn config_map(&self) -> &serde_json::Map<String, serde_json::Value> {
        &self.config
    }

    pub fn with_config(mut self, config: serde_json::Map<String, serde_json::Value>) -> Self {
        self.config = config;
        self
    }
}

/// Implement this to give the model a callable capability.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    fn description(&self) -> &str;

    fn parameters_schema(&self) -> serde_json::Value;

    /// Whether this tool is safe to run concurrently with other tools in the
    /// same turn. Read-only tools (search, fetch, file reads) return `true`;
    /// tools with side effects or that gate on approval return `false`
    /// (the default). The loop batches `parallelizable` calls via `join_all`
    /// and runs the rest serially.
    fn parallelizable(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult>;

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters_schema(),
        }
    }
}

pub const ACTIVATED_KEY_PREFIX: &str = "tool-search/activated/";

pub fn activated_key(name: &str) -> String {
    format!("{ACTIVATED_KEY_PREFIX}{name}")
}

/// Resolves an on-demand tool by prefixed name (e.g. the deferred MCP
/// catalog). Boot-scoped and shared; which tools are switched on for a
/// conversation lives in that conversation's state, not here.
pub trait ToolCatalog: Send + Sync {
    fn resolve(&self, name: &str) -> Option<Arc<dyn Tool>>;
}

/// Snapshot of the calling agent's activated tool names, inserted into each
/// [`ToolContext`] so an activating tool can skip re-emitting for names that
/// are already live.
#[derive(Clone, Default)]
pub struct ActivatedToolNames(pub Arc<std::collections::HashSet<String>>);

#[derive(Clone)]
pub struct CallId(pub String);

#[derive(Clone, Copy)]
pub struct CurrentTurn(pub u32);

impl ActivatedToolNames {
    pub fn contains(&self, name: &str) -> bool {
        self.0.contains(name)
    }
}

/// Tools activated on demand during a conversation.
#[derive(Default)]
pub struct ActivatedToolSet {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ActivatedToolSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn activate(&mut self, name: impl Into<String>, tool: Arc<dyn Tool>) {
        self.tools.insert(name.into(), tool);
    }

    pub fn is_activated(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

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

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    pub fn names(&self) -> std::collections::HashSet<String> {
        self.tools.keys().cloned().collect()
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
        assert!(!r.is_error());
        assert!(r.text().starts_with("u1:"));
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
        let done = ToolResult::ok("hi");
        assert!(!done.is_error());
        assert_eq!(done.text(), "hi");
        assert_eq!(done.output(), Some(&serde_json::json!("hi")));

        let structured = ToolResult::ok(serde_json::json!({ "count": 3 }));
        assert_eq!(structured.text(), r#"{"count":3}"#);

        let failed = ToolResult::error("boom");
        assert!(failed.is_error());
        assert_eq!(failed.text(), "boom");
        assert!(failed.output().is_none());

        let deferred = ToolResult::defer(serde_json::json!({ "question": "ok?" }));
        assert!(!deferred.is_error());
        let json = serde_json::to_value(&deferred).unwrap();
        assert_eq!(json["deferred"]["payload"]["question"], "ok?");
    }

    #[test]
    fn the_provenance_builder_only_touches_done() {
        let sourced = ToolResult::ok("answer")
            .with_provenance(vec![ProvenanceSource::new("s1", "https://e.com")]);
        assert!(matches!(
            sourced,
            ToolResult::Done { ref provenance, .. } if provenance.len() == 1
        ));

        let failed = ToolResult::error("x")
            .with_provenance(vec![ProvenanceSource::new("s1", "https://e.com")]);
        assert_eq!(failed, ToolResult::error("x"));
    }

    #[test]
    fn notes_append_to_strings_and_wrap_structured_output_once() {
        let mut text_result = ToolResult::ok("body");
        text_result.push_notes(&["[loop guard] stop".to_string()]);
        assert_eq!(text_result.text(), "body\n\n[loop guard] stop");

        let mut structured = ToolResult::ok(serde_json::json!({ "a": 1 }));
        structured.push_notes(&["n1".to_string(), "n2".to_string()]);
        let output = structured.output().unwrap();
        assert_eq!(output["output"]["a"], 1);
        assert_eq!(output["notes"][1], "n2");

        let mut failed = ToolResult::error("bad");
        failed.push_notes(&["note".to_string()]);
        assert_eq!(failed.text(), "bad\n\nnote");

        let mut untouched = ToolResult::ok("x");
        untouched.push_notes(&[]);
        assert_eq!(untouched.text(), "x");
    }

    #[test]
    fn activated_set_resolves_exact_and_unique_suffix() {
        let mut set = ActivatedToolSet::new();
        set.activate("fs__read_file", Arc::new(Echo));
        assert!(set.is_activated("fs__read_file"));
        assert!(set.get("fs__read_file").is_some());
        assert!(set.get_resolved("read_file").is_some());
        set.activate("net__read_file", Arc::new(Echo));
        assert!(set.get_resolved("read_file").is_none());
        assert_eq!(set.specs().len(), 2);
    }
}
