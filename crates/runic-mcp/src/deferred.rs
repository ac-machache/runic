//! Deferred (lazy) MCP tool activation — ZeroClaw's answer to context bloat.
//!
//! With several MCP servers you can have *hundreds* of tools; putting every
//! schema in every prompt is ruinous. Instead we register only a built-in
//! [`ToolSearchTool`](crate::tool_search::ToolSearchTool): the system prompt
//! lists tool *names*, and the model calls `tool_search` to fetch + activate
//! the few it actually needs. Activated tools land in a shared
//! [`ActivatedToolSet`], which the agent loop reads when assembling each
//! request and resolving calls.

use runic_tool::ToolSpec;

use crate::client::{McpClient, McpHandle};
use crate::protocol::McpToolDef;
use crate::tool::{prefixed_name, McpTool};

/// A not-yet-activated MCP tool: just enough to list and search by, plus the
/// live handle needed to materialize a real [`McpTool`] on activation.
#[derive(Clone)]
pub struct DeferredMcpToolStub {
    handle: McpHandle,
    def: McpToolDef,
    prefixed: String,
}

impl DeferredMcpToolStub {
    pub fn new(handle: McpHandle, def: McpToolDef) -> Self {
        let prefixed = prefixed_name(handle.server_name(), &def.name);
        Self {
            handle,
            def,
            prefixed,
        }
    }

    /// The registry name (`mcp__server__tool`).
    pub fn prefixed_name(&self) -> &str {
        &self.prefixed
    }

    pub fn description(&self) -> &str {
        self.def.description.as_deref().unwrap_or("")
    }

    /// The LLM-facing spec — built without activating (cheap).
    pub fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.prefixed.clone(),
            description: self.description().to_string(),
            parameters: self.def.input_schema.clone(),
        }
    }

    /// Materialize the live tool.
    pub fn activate(&self) -> McpTool {
        McpTool::new(self.handle.clone(), self.def.clone())
    }
}

/// All deferred MCP tool stubs across every connected server. Searchable by
/// keyword; the source of truth `tool_search` activates from.
#[derive(Clone, Default)]
pub struct DeferredMcpToolSet {
    stubs: Vec<DeferredMcpToolStub>,
}

impl DeferredMcpToolSet {
    pub fn new(stubs: Vec<DeferredMcpToolStub>) -> Self {
        Self { stubs }
    }

    /// Build from connected clients — every tool of every client is deferred.
    pub fn from_clients(clients: &[McpClient]) -> Self {
        let mut stubs = Vec::new();
        for client in clients {
            for def in client.tools() {
                stubs.push(DeferredMcpToolStub::new(client.handle().clone(), def.clone()));
            }
        }
        Self { stubs }
    }

    pub fn is_empty(&self) -> bool {
        self.stubs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.stubs.len()
    }

    /// Prefixed names (what the system prompt lists).
    pub fn names(&self) -> Vec<&str> {
        self.stubs.iter().map(|s| s.prefixed_name()).collect()
    }

    pub fn find(&self, prefixed: &str) -> Option<&DeferredMcpToolStub> {
        self.stubs.iter().find(|s| s.prefixed_name() == prefixed)
    }

    pub fn spec(&self, prefixed: &str) -> Option<ToolSpec> {
        self.find(prefixed).map(|s| s.spec())
    }

    pub fn activate(&self, prefixed: &str) -> Option<McpTool> {
        self.find(prefixed).map(|s| s.activate())
    }

    /// Keyword search: rank stubs by how many query terms hit the name or
    /// description, return the top `max`.
    pub fn search(&self, query: &str, max: usize) -> Vec<&DeferredMcpToolStub> {
        let terms: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if terms.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<(usize, &DeferredMcpToolStub)> = self
            .stubs
            .iter()
            .filter_map(|stub| {
                let hay = format!(
                    "{} {}",
                    stub.prefixed_name().to_lowercase(),
                    stub.description().to_lowercase()
                );
                let hits = terms.iter().filter(|t| hay.contains(t.as_str())).count();
                (hits > 0).then_some((hits, stub))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored.into_iter().take(max).map(|(_, s)| s).collect()
    }
}

/// Allow/deny filtering applied at discovery time — denied tools are never
/// surfaced to the model and never activated, so they never enter context.
#[derive(Clone, Default)]
pub struct ToolAccessPolicy {
    pub allowed: Option<Vec<String>>,
    pub denied: Option<Vec<String>>,
}

impl ToolAccessPolicy {
    pub fn is_tool_allowed(&self, name: &str) -> bool {
        let in_allow = self
            .allowed
            .as_ref()
            .is_none_or(|list| list.iter().any(|t| t == name));
        let in_deny = self
            .denied
            .as_ref()
            .is_some_and(|list| list.iter().any(|t| t == name));
        in_allow && !in_deny
    }
}

/// Render the system-prompt section that lists deferred tool names. The app
/// concatenates this into the agent's system prompt; the loop stays
/// MCP-agnostic.
pub fn deferred_tools_prompt_section(names: &[&str]) -> String {
    if names.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "<available-deferred-tools>\n\
         The following tools exist but their full schemas are not loaded. To use \
         one, call `tool_search` with `select:<name>` (exact) or keywords to \
         search, then call the tool by name.\n",
    );
    for name in names {
        out.push_str("- ");
        out.push_str(name);
        out.push('\n');
    }
    out.push_str("</available-deferred-tools>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stub(handle: &McpHandle, name: &str, desc: &str) -> DeferredMcpToolStub {
        DeferredMcpToolStub::new(
            handle.clone(),
            McpToolDef {
                name: name.to_string(),
                description: Some(desc.to_string()),
                input_schema: serde_json::json!({ "type": "object" }),
            },
        )
    }

    // A dead transport is fine — these tests only touch stub metadata/search.
    fn handle(server: &str) -> McpHandle {
        use crate::transport::Transport;
        use std::sync::Arc;

        #[derive(Debug)]
        struct Dead(String);
        #[async_trait::async_trait]
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

    #[test]
    fn stub_prefixes_and_specs() {
        let h = handle("fs");
        let s = stub(&h, "read_file", "Read a file from disk");
        assert_eq!(s.prefixed_name(), "mcp__fs__read_file");
        assert_eq!(s.spec().name, "mcp__fs__read_file");
        assert_eq!(s.spec().parameters["type"], "object");
    }

    #[test]
    fn search_ranks_by_term_hits() {
        let h = handle("fs");
        let set = DeferredMcpToolSet::new(vec![
            stub(&h, "read_file", "Read a file from disk"),
            stub(&h, "write_file", "Write a file to disk"),
            stub(&h, "query_db", "Query the database"),
        ]);
        let hits = set.search("read file", 5);
        assert_eq!(hits[0].prefixed_name(), "mcp__fs__read_file");
        assert!(set.search("database", 5)[0]
            .prefixed_name()
            .ends_with("query_db"));
        assert!(set.search("nonexistent", 5).is_empty());
    }

    #[test]
    fn search_respects_max() {
        let h = handle("fs");
        let set = DeferredMcpToolSet::new(vec![
            stub(&h, "read_a", "read file"),
            stub(&h, "read_b", "read file"),
            stub(&h, "read_c", "read file"),
        ]);
        assert_eq!(set.search("read file", 2).len(), 2);
    }

    #[test]
    fn access_policy_allow_deny() {
        let p = ToolAccessPolicy {
            allowed: Some(vec!["a".into(), "b".into()]),
            denied: Some(vec!["b".into()]),
        };
        assert!(p.is_tool_allowed("a"));
        assert!(!p.is_tool_allowed("b")); // deny overrides allow
        assert!(!p.is_tool_allowed("c")); // not in allow
        assert!(ToolAccessPolicy::default().is_tool_allowed("anything"));
    }

    #[test]
    fn prompt_section_lists_names() {
        let section = deferred_tools_prompt_section(&["mcp__fs__read", "mcp__fs__write"]);
        assert!(section.contains("tool_search"));
        assert!(section.contains("mcp__fs__read"));
        assert!(deferred_tools_prompt_section(&[]).is_empty());
    }
}
