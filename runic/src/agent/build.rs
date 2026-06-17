//! `MaiaFactory` — assembles the coral agent ("Maia") for `runic-serve`.
//!
//! Shared, per-process state (provider registry, MCP tool pool, parsed
//! sub-agents, commands, title provider, approval hub) is built ONCE in
//! [`MaiaFactory::new`]. Per **thread**, [`MaiaFactory::build`] assembles a
//! fresh `Agent` shell (prompt + tools + sub-agents + hooks + context
//! engine + HITL approver). Per **request**,
//! [`MaiaFactory::build_run_context`] turns the request's open `context`
//! JSON (+ tenant) into a [`RunContext`] — identity keys, web opt-in, and a
//! resolved per-run provider override. Request-varying values live there,
//! never baked into the thread shell.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use runic_agent_core::{Agent, AgentConfig, CallLimitHook, Hook, RunContext};
use runic_commands::CommandRegistry;
use runic_context_engine::{
    BackgroundTaskReminder, BasePromptLayer, CompositeEngine, ContextEngine, ReminderEngine,
};
use runic_provider_core::Provider;
use runic_serve::{AgentFactory, ApprovalHub, ChannelApprover};
use runic_storage_backend::{MemoryBackend, StorageBackend};
use runic_tool_core::approval::ApproverHandle;
use runic_tool_core::{BackgroundManager, ToolInterceptor, ToolRegistry};
use serde_json::Value;

use super::ask_user::AskUserTool;
use super::commands::CommandExpansionEngine;
use super::context::{KEY_ORG_ID, KEY_PROVIDER};
use super::context_layers::DateLayer;
use super::hooks::{BindToolContext, WebSearchGuard};
use super::prompt::CORAL_PROMPT;
use super::providers::{Providers, DEFAULT_PROVIDER};
use super::subagent_loader::{load_subagents, register_subagents};
use super::titles::{LoggingTitleSink, TitleReflector};

/// Maia's own toolset (coral `coral` toolset): reads + task lifecycle +
/// reports. The write tools are deliberately absent — they live only in
/// `crm_expert`, so a mutation can't happen without an explicit delegation.
const CORAL_TOOLSET: &[&str] = &[
    "search_farms",
    "get_farm",
    "list_farmer_farms",
    "list_tasks",
    "get_tasks",
    "create_task",
    "update_task",
    "set_task_items_done",
    "add_task_items",
    "delete_task",
    "list_reports",
    "get_report",
    "create_report",
    "summary",
    "search_portfolio",
];

const TOOLBOX_PREFIX: &str = "mcp__toolbox__";
const TAVILY_PREFIX: &str = "mcp__tavily__";

fn runic_home() -> PathBuf {
    std::env::var("RUNIC_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let mut p = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
            p.push(".runic");
            p
        })
}

/// Sub-agent definitions ship in the repo; resolve their dir at compile
/// time so the path is stable regardless of CWD.
fn subagents_dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/src/agent/subagents"))
}

/// Per-process shared state for the coral agent. Cheap to clone the Arcs
/// per thread build.
pub struct MaiaFactory {
    providers: Providers,
    main_provider: Arc<dyn Provider>,
    title_provider: Arc<dyn Provider>,
    mcp_pool: ToolRegistry,
    agents: Vec<runic_agents::MdAgent>,
    commands: Arc<CommandRegistry>,
    parent_storage: Arc<dyn StorageBackend>,
    /// Persistent root for per-user memory: `runic-data/{user_id}/memory/…`.
    memory_base: Arc<dyn StorageBackend>,
    approval_hub: Arc<ApprovalHub>,
    /// Per-run call caps (`tool_name -> max calls per request`), enforced by a
    /// [`CallLimitHook`] on the main agent AND every sub-agent. Stops the
    /// model from looping on one tool (e.g. re-running a web search 7×).
    call_limits: HashMap<String, usize>,
}

impl MaiaFactory {
    /// One-time setup: provider registry, MCP toolbox + Tavily tools, parsed
    /// sub-agents, slash commands.
    pub async fn new(approval_hub: Arc<ApprovalHub>) -> Result<Self> {
        let providers = Providers::from_env().context("building provider registry")?;
        let main_provider = providers
            .get(DEFAULT_PROVIDER)
            .context("default provider 'sonnet' unavailable — is ANTHROPIC_API_KEY set?")?;
        let title_provider = providers
            .get("haiku")
            .unwrap_or_else(|| main_provider.clone());

        let mcp_path = runic_home().join("mcp.json");
        let manager = super::mcp::load(&mcp_path).await;
        tracing::info!(
            servers = manager.len(),
            tools = manager.total_tool_count(),
            "MCP connected"
        );
        let mut mcp_pool = ToolRegistry::new();
        for tool in manager.all_tools() {
            mcp_pool.register(tool);
        }

        // Attach binding/guard as TOOL-LEVEL interceptors on the shared pool.
        // Because the wrapped dispatch is what later gets cloned into every
        // sub-agent pool, the binding fires for whichever agent invokes the
        // tool — parent or sub-agent — reading identity off the per-run
        // context, which sub-agents inherit from their parent.
        let bind: Arc<dyn ToolInterceptor> = Arc::new(BindToolContext);
        mcp_pool.intercept(|n| n.starts_with(TOOLBOX_PREFIX), vec![bind]);
        let guard: Arc<dyn ToolInterceptor> = Arc::new(WebSearchGuard);
        mcp_pool.intercept(|n| n.starts_with(TAVILY_PREFIX), vec![guard]);

        let agents = load_subagents(&[subagents_dir()]);
        tracing::info!(count = agents.len(), "sub-agents loaded");
        let commands = Arc::new(load_commands().await);
        let parent_storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        // Per-user memory persists under an ABSOLUTE root (CWD-independent):
        // `$RUNIC_DATA_DIR`, else `<workspace>/runic-data` (the workspace root
        // is the parent of this binary crate's manifest dir). Then
        // `/{user_id}/memory/…`.
        let memory_root = std::env::var("RUNIC_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .map(|p| p.join("runic-data"))
                    .unwrap_or_else(|| PathBuf::from("runic-data"))
            });
        tracing::info!(path = %memory_root.display(), "per-user memory root");
        let memory_base: Arc<dyn StorageBackend> =
            Arc::new(runic_storage_backend::LocalFsBackend::new(memory_root));

        // Per-run call caps. Tavily was observed looping (7 search calls in
        // one request, re-phrasing because results carried no synthesized
        // answer); cap EVERY tavily tool (search/extract/crawl/map/…) at 3
        // per request so the model stops and answers with what it has.
        // Built from the live pool so it tracks whatever the server exposes.
        const TAVILY_MAX_CALLS: usize = 3;
        let call_limits: HashMap<String, usize> = mcp_pool
            .names()
            .into_iter()
            .filter(|n| n.starts_with(TAVILY_PREFIX))
            .map(|n| (n, TAVILY_MAX_CALLS))
            .collect();
        tracing::info!(
            tools = call_limits.len(),
            max = TAVILY_MAX_CALLS,
            "tavily per-run call cap"
        );

        Ok(Self {
            providers,
            main_provider,
            title_provider,
            mcp_pool,
            agents,
            commands,
            parent_storage,
            memory_base,
            approval_hub,
            call_limits,
        })
    }
}

#[async_trait]
impl AgentFactory for MaiaFactory {
    async fn build(&self, _tenant: &str, session_id: &str) -> Agent {
        // Maia's pool: coral toolset (reads/tasks/reports) + Tavily +
        // ask_user + the sub-agents. Writes stay in crm_expert.
        let mut pool = ToolRegistry::new();
        for name in CORAL_TOOLSET {
            let key = format!("{TOOLBOX_PREFIX}{name}");
            match self.mcp_pool.get(&key) {
                Some(d) => pool.insert_dispatch(d),
                None => tracing::warn!(tool = key, "coral toolset tool missing on MCP server"),
            }
        }
        for name in self.mcp_pool.names() {
            if name.starts_with(TAVILY_PREFIX)
                && let Some(d) = self.mcp_pool.get(&name)
            {
                pool.insert_dispatch(d);
            }
        }
        pool.register_hitl(Arc::new(AskUserTool));

        // One call-limit hook, shared by reference onto the main agent and
        // every sub-agent. Each agent counts only its OWN run's history, so
        // the cap is per-agent-per-run — no shared mutable counter, no leak.
        let call_limit: Arc<dyn Hook> = Arc::new(CallLimitHook::new(self.call_limits.clone()));

        register_subagents(
            &self.agents,
            &self.providers,
            &self.main_provider,
            &self.parent_storage,
            &self.mcp_pool,
            &mut pool,
            std::slice::from_ref(&call_limit),
        );

        // Per-user memory tool (`memory`) — writes to runic-data/{user_id}.
        pool.register(Arc::new(super::memory::UserMemoryTool::new(
            self.memory_base.clone(),
        )));

        // One BackgroundManager shared between the async tools (which start
        // tasks) and the reminder (which surfaces their completions).
        let bg_manager = Arc::new(BackgroundManager::new());

        // Context engine, inner → outer:
        //   CompositeEngine (base prompt + date + per-user memory)
        //   → CommandExpansionEngine (slash-command expansion)
        //   → ReminderEngine (injects finished background-task results as
        //     ambient notes, so async sub-agents like wikis_expert surface
        //     their answers WITHOUT the model polling background_status).
        let composite = CompositeEngine::new()
            .with_layer(BasePromptLayer::new(CORAL_PROMPT))
            .with_layer(DateLayer::new())
            .with_layer(super::memory::UserMemoryLayer::new(self.memory_base.clone()));
        let with_commands: Arc<dyn ContextEngine> = Arc::new(CommandExpansionEngine::new(
            Arc::new(composite),
            self.commands.clone(),
        ));
        let engine: Arc<dyn ContextEngine> = Arc::new(
            ReminderEngine::new(with_commands)
                .with_reminder(BackgroundTaskReminder::new(bg_manager.clone())),
        );

        // HITL approver routes `ask_user` over the server's SSE stream.
        let approver: ApproverHandle = Arc::new(ChannelApprover::new(self.approval_hub.clone()));

        Agent::builder(self.main_provider.clone())
            .system_prompt(CORAL_PROMPT)
            .tools(pool)
            // wikis_expert is `dispatch: async` → a background tool. The
            // BackgroundManager must be in runtime for it to register tasks
            // and hand back a task_id (must come AFTER .tools, which replaces
            // the registry; this also registers background_status/cancel).
            // Same instance the ReminderEngine watches, so completions surface.
            .background_manager(bg_manager.clone())
            // BindToolContext / WebSearchGuard are now TOOL-level interceptors
            // (wired in `new`), so they reach sub-agents too. Only the
            // run-level title reflector remains an agent hook.
            .hook(call_limit.clone())
            .hook(Arc::new(TitleReflector::new(
                self.title_provider.clone(),
                Arc::new(LoggingTitleSink),
            )))
            .runtime(approver)
            .context_engine_arc(engine)
            .config(AgentConfig {
                max_turns: 64,
                ..Default::default()
            })
            .session_id(session_id)
            .build()
    }

    async fn build_run_context(
        &self,
        tenant: &str,
        session_id: &str,
        context: &Value,
    ) -> RunContext {
        // Open map straight from the request; the dev can carry any keys.
        let mut config = match context {
            Value::Object(m) => m.clone(),
            _ => serde_json::Map::new(),
        };
        // Conventional identity defaults: org_id from the tenant, session_id
        // for traceability. The thread links to user_id when the client sent
        // one (left as-is).
        config
            .entry(KEY_ORG_ID)
            .or_insert_with(|| Value::String(tenant.to_string()));
        config
            .entry("session_id")
            .or_insert_with(|| Value::String(session_id.to_string()));

        // Per-run main-model override: resolve the `provider` key against the
        // registry. Unknown key → no override (keeps the build-time provider).
        let provider = config
            .get(KEY_PROVIDER)
            .and_then(|v| v.as_str())
            .and_then(|key| {
                let p = self.providers.get(key);
                if p.is_none() {
                    tracing::warn!(provider = key, "unknown per-run provider key — ignoring");
                }
                p
            });

        let mut rc = RunContext::from_json(Value::Object(config));
        rc.provider = provider;
        rc
    }
}

/// Load slash commands from `~/.runic/commands`, tolerating a missing
/// directory (empty registry).
async fn load_commands() -> CommandRegistry {
    use runic_storage_backend::LocalFsBackend;
    let storage: Arc<dyn StorageBackend> = Arc::new(LocalFsBackend::new(runic_home()));
    CommandRegistry::load(storage, "commands")
        .await
        .unwrap_or_default()
}
