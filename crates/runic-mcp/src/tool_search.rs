//! The built-in `tool_search` tool — on-demand activation of deferred MCP
//! tools (ported from ZeroClaw's `tool_search`).
//!
//! Two query modes:
//! - `select:name1,name2` — activate exact tools by prefixed name.
//! - free-text — keyword-search the deferred set and activate the matches.
//!
//! It activates into a shared [`ActivatedToolSet`]; the agent loop reads that
//! set when assembling each request and resolving calls, so `tool_search`
//! needs no loop special-casing — it's just a normal tool with a side effect.

use std::fmt::Write;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use runic_tool::{ActivatedToolSet, Tool, ToolContext, ToolResult, ToolSpec};

use crate::deferred::{DeferredMcpToolSet, ToolAccessPolicy};

const DEFAULT_MAX_RESULTS: usize = 5;

/// The `tool_search` tool. Holds the full deferred set + the shared activated
/// set it writes into.
pub struct ToolSearchTool {
    deferred: DeferredMcpToolSet,
    activated: Arc<Mutex<ActivatedToolSet>>,
    policy: Option<ToolAccessPolicy>,
}

impl ToolSearchTool {
    pub fn new(deferred: DeferredMcpToolSet, activated: Arc<Mutex<ActivatedToolSet>>) -> Self {
        Self {
            deferred,
            activated,
            policy: None,
        }
    }

    pub fn with_access_policy(mut self, policy: ToolAccessPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    fn is_allowed(&self, name: &str) -> bool {
        self.policy.as_ref().is_none_or(|p| p.is_tool_allowed(name))
    }

    /// Recover a poisoned lock rather than panicking — a crashed activation
    /// shouldn't wedge the whole conversation.
    fn lock_activated(&self) -> std::sync::MutexGuard<'_, ActivatedToolSet> {
        self.activated
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Append one tool's `<function>` line and activate it (idempotently).
    fn emit_and_activate(&self, out: &mut String, spec: &ToolSpec, prefixed: &str) {
        {
            let mut guard = self.lock_activated();
            if !guard.is_activated(prefixed)
                && let Some(tool) = self.deferred.activate(prefixed) {
                    guard.activate(prefixed.to_string(), Arc::new(tool));
                }
        }
        let _ = writeln!(
            out,
            "<function>{{\"name\": \"{}\", \"description\": \"{}\", \"parameters\": {}}}</function>",
            spec.name,
            spec.description.replace('"', "\\\""),
            spec.parameters
        );
    }

    fn select(&self, names: &[&str]) -> ToolResult {
        let mut out = String::from("<functions>\n");
        let mut not_found = Vec::new();
        for name in names {
            if name.is_empty() {
                continue;
            }
            if !self.is_allowed(name) {
                not_found.push(*name);
                continue;
            }
            match self.deferred.spec(name) {
                Some(spec) => self.emit_and_activate(&mut out, &spec, name),
                None => not_found.push(*name),
            }
        }
        out.push_str("</functions>\n");
        if !not_found.is_empty() {
            let _ = write!(out, "\nNot found: {}", not_found.join(", "));
        }
        ToolResult::ok(out)
    }

    fn keyword(&self, query: &str, max_results: usize) -> ToolResult {
        // With a policy active, fetch all matches so denied tools don't consume
        // result slots; apply the cap after filtering.
        let search_limit = if self.policy.is_some() {
            usize::MAX
        } else {
            max_results
        };
        let results = self.deferred.search(query, search_limit);
        if results.is_empty() {
            return ToolResult::ok("No matching deferred tools found.");
        }

        let mut out = String::from("<functions>\n");
        let mut returned = 0;
        for stub in results {
            if returned >= max_results {
                break;
            }
            if !self.is_allowed(stub.prefixed_name()) {
                continue;
            }
            self.emit_and_activate(&mut out, &stub.spec(), stub.prefixed_name());
            returned += 1;
        }
        out.push_str("</functions>\n");
        ToolResult::ok(out)
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        "tool_search"
    }

    fn description(&self) -> &str {
        "Fetch full schema definitions for deferred tools so they can be called. \
         Use \"select:name1,name2\" for exact tools, or keywords to search."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "\"select:<name>[,<name>...]\" for exact tools, or keywords to search."
                },
                "max_results": {
                    "type": "number",
                    "description": "Maximum number of results (default 5)."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim();
        if query.is_empty() {
            return Ok(ToolResult::error("query parameter is required"));
        }
        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .map(|v| usize::try_from(v).unwrap_or(DEFAULT_MAX_RESULTS))
            .unwrap_or(DEFAULT_MAX_RESULTS);

        let result = match query.strip_prefix("select:") {
            Some(names) => {
                let names: Vec<&str> = names.split(',').map(str::trim).collect();
                self.select(&names)
            }
            None => self.keyword(query, max_results),
        };
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::client::McpHandle;
    use crate::deferred::DeferredMcpToolStub;
    use crate::protocol::McpToolDef;
    use crate::transport::Transport;

    fn handle(server: &str) -> McpHandle {
        #[derive(Debug)]
        struct Dead(String);
        #[async_trait]
        impl Transport for Dead {
            fn server_name(&self) -> &str {
                &self.0
            }
            async fn request(
                &self,
                _m: &str,
                _p: Option<serde_json::Value>,
            ) -> Result<serde_json::Value, crate::error::McpError> {
                Err(crate::error::McpError::Disconnected(self.0.clone()))
            }
            async fn notify(
                &self,
                _m: &str,
                _p: Option<serde_json::Value>,
            ) -> Result<(), crate::error::McpError> {
                Ok(())
            }
            async fn close(&self) {}
        }
        McpHandle::from_transport(Arc::new(Dead(server.to_string())))
    }

    fn deferred(stubs: Vec<(&str, &str)>) -> DeferredMcpToolSet {
        let h = handle("fs");
        DeferredMcpToolSet::new(
            stubs
                .into_iter()
                .map(|(name, desc)| {
                    DeferredMcpToolStub::new(
                        h.clone(),
                        McpToolDef {
                            name: name.to_string(),
                            description: Some(desc.to_string()),
                            input_schema: serde_json::json!({ "type": "object" }),
                        },
                    )
                })
                .collect(),
        )
    }

    fn ctx() -> ToolContext {
        ToolContext::new("u", "s", "r")
    }

    #[tokio::test]
    async fn empty_query_errors() {
        let t = ToolSearchTool::new(deferred(vec![]), Arc::new(Mutex::new(ActivatedToolSet::new())));
        let r = t.execute(serde_json::json!({ "query": "" }), &ctx()).await.unwrap();
        assert!(!r.success);
    }

    #[tokio::test]
    async fn keyword_search_finds_and_activates() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let t = ToolSearchTool::new(
            deferred(vec![("read_file", "Read a file from disk")]),
            Arc::clone(&activated),
        );
        let r = t
            .execute(serde_json::json!({ "query": "read file" }), &ctx())
            .await
            .unwrap();
        assert!(r.success);
        assert!(r.output.contains("<function>"));
        assert!(r.output.contains("mcp__fs__read_file"));
        assert!(activated.lock().unwrap().is_activated("mcp__fs__read_file"));
    }

    #[tokio::test]
    async fn select_activates_exact_and_reports_not_found() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let t = ToolSearchTool::new(
            deferred(vec![("tool_a", "A"), ("tool_b", "B")]),
            Arc::clone(&activated),
        );
        let r = t
            .execute(
                serde_json::json!({ "query": "select:mcp__fs__tool_a,mcp__fs__missing" }),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(r.success);
        assert!(r.output.contains("mcp__fs__tool_a"));
        assert!(r.output.contains("Not found"));
        assert!(activated.lock().unwrap().is_activated("mcp__fs__tool_a"));
        assert!(!activated.lock().unwrap().is_activated("mcp__fs__missing"));
    }

    #[tokio::test]
    async fn reactivation_is_idempotent() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let t = ToolSearchTool::new(deferred(vec![("t", "a tool")]), Arc::clone(&activated));
        t.execute(serde_json::json!({ "query": "select:mcp__fs__t" }), &ctx()).await.unwrap();
        t.execute(serde_json::json!({ "query": "select:mcp__fs__t" }), &ctx()).await.unwrap();
        assert_eq!(activated.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn policy_filters_denied_tools() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let t = ToolSearchTool::new(
            deferred(vec![("allowed", "a tool"), ("blocked", "a tool")]),
            Arc::clone(&activated),
        )
        .with_access_policy(ToolAccessPolicy {
            allowed: None,
            denied: Some(vec!["mcp__fs__blocked".into()]),
        });
        let r = t.execute(serde_json::json!({ "query": "tool" }), &ctx()).await.unwrap();
        assert!(r.output.contains("mcp__fs__allowed"));
        assert!(!r.output.contains("mcp__fs__blocked"));
        assert!(!activated.lock().unwrap().is_activated("mcp__fs__blocked"));
    }
}
