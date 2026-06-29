use std::sync::Arc;

use runic_agent::Agent;
use runic_hook::WriteHook;
use runic_mcp::McpConnection;
use runic_memory::Memory;
use runic_provider::Provider;
use runic_skills::SkillSet;
use runic_subagent::{SubagentBuilder, Subagents};
use runic_substrate::{ArtifactStore, Sessions};
use runic_tool::Tool;
use runic_tools::Tools;

use crate::artifact_resolver::ArtifactResolver;
use crate::child::FoundrySubagentBuilder;
use crate::context::Context;
use crate::memory_review::MemoryReviewHook;

/// The bundle of parts an agent is assembled from. Set the optional slices you
/// want; leave the rest `None`.
pub struct Assembly {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub instructions: String,
    pub memory: Option<Memory>,
    pub skills: Option<Arc<SkillSet>>,
    pub subagents: Option<Subagents>,
    pub subagent_builder: Option<Arc<dyn SubagentBuilder>>,
    pub mcp: Option<McpConnection>,
    pub sessions: Option<Sessions>,
    pub tools: Option<Tools>,
    /// App-specific tools to register as-is (e.g. domain tools the standard
    /// surfaces don't cover).
    pub custom_tools: Vec<Arc<dyn Tool>>,
    /// Force structured output via the synthetic `final_answer` tool.
    pub output_schema: Option<serde_json::Value>,
    /// Cap the agent's turns per run.
    pub max_turns: Option<u32>,
    /// App-specific read-edit hooks (e.g. tenant-id injection into tool calls).
    pub write_hooks: Vec<Arc<dyn WriteHook>>,
    /// When set, `ArtifactRef` blocks resolve to the stored bytes just before
    /// each model call (the event log keeps only the reference).
    pub artifact_store: Option<Arc<dyn ArtifactStore>>,
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
        ctx.skills(s);
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

    if let Some(store) = &a.artifact_store {
        b = b.media_resolver(Arc::new(ArtifactResolver::new(
            store.clone(),
            tenant,
            session,
        )));
    }

    // ── tools ──────────────────────────────────────────────────────────────
    if let Some(t) = &a.tools {
        for tool in t.collect() {
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
        && let Some(tool) = s.view_tool()
    {
        b = b.tool(tool);
    }
    if let Some(s) = &a.subagents {
        let builder: Arc<dyn SubagentBuilder> = a.subagent_builder.clone().unwrap_or_else(|| {
            Arc::new(FoundrySubagentBuilder {
                provider: a.provider.clone(),
                model: a.model.clone(),
            })
        });
        if let Some(tool) = s.tool(builder) {
            b = b.tool(tool);
        }
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
    for t in &a.custom_tools {
        b = b.tool(t.clone());
    }

    // ── agent config ────────────────────────────────────────────────────────
    if let Some(schema) = &a.output_schema {
        b = b.output_schema(schema.clone());
    }
    if let Some(n) = a.max_turns {
        b = b.max_turns(n);
    }

    // ── hooks ────────────────────────────────────────────────────────────────
    if let Some(m) = &a.memory
        && let Some(store) = &store
        && m.review_interval() > 0
    {
        let hook = MemoryReviewHook::new(
            m.review_interval(),
            a.provider.clone(),
            &a.model,
            store.clone(),
        );
        b = b.write_hook(Arc::new(hook));
    }
    for hook in &a.write_hooks {
        b = b.write_hook(hook.clone());
    }

    tracing::info!(tenant, session, "agent assembled");
    b.build()
}
