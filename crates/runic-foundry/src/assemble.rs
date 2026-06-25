use std::sync::Arc;

use runic_agent::Agent;
use runic_filesystem::FilesystemBackend;
use runic_provider::Provider;

use crate::context::Context;
use crate::{McpConnection, Memory, Sessions, Skills, Subagents, Tools};

/// The bundle of parts an agent is assembled from. Set the optional slices you
/// want; leave the rest `None`.
pub struct Assembly {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub instructions: String,
    pub workspace: Arc<dyn FilesystemBackend>,
    pub memory: Option<Memory>,
    pub skills: Option<Skills>,
    pub subagents: Option<Subagents>,
    pub mcp: Option<McpConnection>,
    pub sessions: Option<Sessions>,
    pub tools: Option<Tools>,
}

/// Wire every configured part into a fresh agent for `(tenant, session)`:
/// compose the system prompt, register tools, install the MCP activated set,
/// and the memory-review hook.
pub async fn assemble(a: &Assembly, tenant: &str, session: &str) -> Agent {
    // ── system prompt ──────────────────────────────────────────────────────
    let store = a.memory.as_ref().map(|m| m.store(tenant));

    let mut ctx = Context::new();
    ctx.instructions(&a.instructions);
    if let Some(store) = &store {
        ctx.memory(store, true, true).await;
    }
    if let Some(s) = &a.skills {
        ctx.skills(&s.registry());
    }
    if let Some(s) = &a.subagents {
        ctx.subagents(&s.roster());
    }
    if let Some(m) = &a.mcp {
        ctx.mcp(m.section());
    }

    let mut b = Agent::builder(a.provider.clone(), tenant, session)
        .model(&a.model)
        .system_prompt(ctx.render());

    // ── tools ──────────────────────────────────────────────────────────────
    if let Some(t) = &a.tools {
        for tool in t.collect(a.workspace.clone()) {
            b = b.tool(tool);
        }
    }
    if let Some(m) = &a.memory
        && let Some(store) = &store
        && let Some(tool) = m.tools(store.clone())
    {
        b = b.tool(tool);
    }
    if let Some(s) = &a.skills
        && let Some(tool) = s.tools()
    {
        b = b.tool(tool);
    }
    if let Some(s) = &a.subagents
        && let Some(tool) = s.tools(a.provider.clone(), &a.model, a.workspace.clone())
    {
        b = b.tool(tool);
    }
    if let Some(s) = &a.sessions
        && let Some(tool) = s.tools()
    {
        b = b.tool(tool);
    }
    if let Some(m) = &a.mcp {
        if let Some(tool) = m.tools() {
            b = b.tool(tool);
        }
        b = b.activated_tools(m.activated());
    }

    // ── hooks ────────────────────────────────────────────────────────────────
    if let Some(m) = &a.memory
        && let Some(store) = &store
        && let Some(hook) = m.review_hook(a.provider.clone(), &a.model, store.clone())
    {
        b = b.write_hook(hook);
    }

    tracing::info!(tenant, session, "agent assembled");
    b.build()
}
