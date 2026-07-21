//! The built-in `tool_search` tool — on-demand activation of deferred MCP
//! tools (ported from ZeroClaw's `tool_search`).
//!
//! Two query modes:
//! - `select:name1,name2` — activate exact tools by prefixed name.
//! - free-text — keyword-search the deferred set and activate the matches.
//!
//! Activation is a state write: each match emits a
//! `tool-search/activated/<name>` key through the run's [`ExternalEvents`],
//! so it lands in the event log and the agent loop materializes the tool from
//! its catalog at the next turn — per conversation, surviving rebuilds.

use std::fmt::Write;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;

use runic_state::{AgentEvent, Emitter};
use runic_tool::{ActivatedToolNames, Tool, ToolContext, ToolResult, ToolSpec, activated_key};

use crate::deferred::{DeferredMcpToolSet, ToolAccessPolicy};

const DEFAULT_MAX_RESULTS: usize = 5;

/// The `tool_search` tool. Holds the full deferred set it searches and
/// activates from.
pub struct ToolSearchTool {
    deferred: Arc<DeferredMcpToolSet>,
    policy: Option<ToolAccessPolicy>,
}

struct Activation<'a> {
    events: Option<Arc<dyn Emitter>>,
    already_active: Option<Arc<ActivatedToolNames>>,
    run_id: &'a str,
}

impl Activation<'_> {
    fn activate(&self, name: &str) {
        if self
            .already_active
            .as_ref()
            .is_some_and(|names| names.contains(name))
        {
            return;
        }
        match &self.events {
            Some(events) => events.emit(AgentEvent::StateUpdated {
                run_id: self.run_id.to_string(),
                key: activated_key(name),
                value: serde_json::Value::Bool(true),
                at: Utc::now(),
            }),
            None => {
                tracing::warn!(tool = %name, "no event rail in tool context — activation not recorded");
            }
        }
    }
}

impl ToolSearchTool {
    pub fn new(deferred: Arc<DeferredMcpToolSet>) -> Self {
        Self {
            deferred,
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

    /// Append one tool's `<function>` line and record its activation.
    fn emit_and_activate(
        &self,
        out: &mut String,
        spec: &ToolSpec,
        prefixed: &str,
        activation: &Activation<'_>,
    ) {
        activation.activate(prefixed);
        let _ = writeln!(
            out,
            "<function>{{\"name\": \"{}\", \"description\": \"{}\", \"parameters\": {}}}</function>",
            spec.name,
            spec.description.replace('"', "\\\""),
            spec.parameters
        );
    }

    fn select(&self, names: &[&str], activation: &Activation<'_>) -> ToolResult {
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
                Some(spec) => self.emit_and_activate(&mut out, &spec, name, activation),
                None => not_found.push(*name),
            }
        }
        out.push_str("</functions>\n");
        if !not_found.is_empty() {
            let _ = write!(out, "\nNot found: {}", not_found.join(", "));
        }
        ToolResult::ok(out)
    }

    fn keyword(&self, query: &str, max_results: usize, activation: &Activation<'_>) -> ToolResult {
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
            self.emit_and_activate(&mut out, &stub.spec(), stub.prefixed_name(), activation);
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
        ctx: &ToolContext,
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

        let activation = Activation {
            events: ctx.emitter(),
            already_active: ctx.get::<ActivatedToolNames>(),
            run_id: &ctx.run_id,
        };
        let result = match query.strip_prefix("select:") {
            Some(names) => {
                let names: Vec<&str> = names.split(',').map(str::trim).collect();
                self.select(&names, &activation)
            }
            None => self.keyword(query, max_results, &activation),
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

    #[derive(Debug)]
    struct ChanEmitter(tokio::sync::mpsc::UnboundedSender<AgentEvent>);
    impl Emitter for ChanEmitter {
        fn emit(&self, event: AgentEvent) {
            let _ = self.0.send(event);
        }
    }

    fn ctx_with_rail() -> (
        ToolContext,
        tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolContext::new("u", "s", "r")
            .with_emitter(Some(std::sync::Arc::new(ChanEmitter(tx))));
        (ctx, rx)
    }

    fn activated_keys(
        pending: &mut tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
    ) -> Vec<String> {
        let mut events = Vec::new();
        while let Ok(e) = pending.try_recv() {
            events.push(e);
        }
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::StateUpdated { key, value, .. } if value.as_bool() == Some(true) => {
                    Some(
                        key.strip_prefix(runic_tool::ACTIVATED_KEY_PREFIX)?
                            .to_string(),
                    )
                }
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn empty_query_errors() {
        let t = ToolSearchTool::new(Arc::new(deferred(vec![])));
        let (ctx, _) = ctx_with_rail();
        let r = t
            .execute(serde_json::json!({ "query": "" }), &ctx)
            .await
            .unwrap();
        assert!(r.is_error());
    }

    #[tokio::test]
    async fn keyword_search_finds_and_activates() {
        let t = ToolSearchTool::new(Arc::new(deferred(vec![(
            "read_file",
            "Read a file from disk",
        )])));
        let (ctx, mut pending) = ctx_with_rail();
        let r = t
            .execute(serde_json::json!({ "query": "read file" }), &ctx)
            .await
            .unwrap();
        assert!(!r.is_error());
        assert!(r.text().contains("<function>"));
        assert!(r.text().contains("mcp__fs__read_file"));
        assert_eq!(activated_keys(&mut pending), ["mcp__fs__read_file"]);
    }

    #[tokio::test]
    async fn select_activates_exact_and_reports_not_found() {
        let t = ToolSearchTool::new(Arc::new(deferred(vec![("tool_a", "A"), ("tool_b", "B")])));
        let (ctx, mut pending) = ctx_with_rail();
        let r = t
            .execute(
                serde_json::json!({ "query": "select:mcp__fs__tool_a,mcp__fs__missing" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!r.is_error());
        assert!(r.text().contains("mcp__fs__tool_a"));
        assert!(r.text().contains("Not found"));
        assert_eq!(activated_keys(&mut pending), ["mcp__fs__tool_a"]);
    }

    #[tokio::test]
    async fn already_active_names_are_not_re_emitted() {
        let t = ToolSearchTool::new(Arc::new(deferred(vec![("t", "a tool")])));
        let (mut ctx, mut pending) = ctx_with_rail();
        ctx.insert(ActivatedToolNames(Arc::new(
            [String::from("mcp__fs__t")].into(),
        )));
        let r = t
            .execute(serde_json::json!({ "query": "select:mcp__fs__t" }), &ctx)
            .await
            .unwrap();
        assert!(!r.is_error());
        assert!(r.text().contains("mcp__fs__t"), "schema is still returned");
        assert!(activated_keys(&mut pending).is_empty());
    }

    #[tokio::test]
    async fn a_context_without_the_event_rail_still_returns_schemas() {
        let t = ToolSearchTool::new(Arc::new(deferred(vec![("t", "a tool")])));
        let r = t
            .execute(
                serde_json::json!({ "query": "select:mcp__fs__t" }),
                &ToolContext::new("u", "s", "r"),
            )
            .await
            .unwrap();
        assert!(!r.is_error());
        assert!(r.text().contains("mcp__fs__t"));
    }

    #[tokio::test]
    async fn policy_filters_denied_tools() {
        let t = ToolSearchTool::new(Arc::new(deferred(vec![
            ("allowed", "a tool"),
            ("blocked", "a tool"),
        ])))
        .with_access_policy(ToolAccessPolicy {
            allowed: None,
            denied: Some(vec!["mcp__fs__blocked".into()]),
        });
        let (ctx, mut pending) = ctx_with_rail();
        let r = t
            .execute(serde_json::json!({ "query": "tool" }), &ctx)
            .await
            .unwrap();
        assert!(r.text().contains("mcp__fs__allowed"));
        assert!(!r.text().contains("mcp__fs__blocked"));
        assert_eq!(activated_keys(&mut pending), ["mcp__fs__allowed"]);
    }
}
