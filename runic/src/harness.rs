//! `Harness` — the shared agent wiring, built once and reused by every
//! surface (REPL, `serve`).
//!
//! [`Harness::load`] does the once-per-process work: select + blob-wrap the
//! provider, load plugins / skills / markdown-agents / commands, connect
//! MCP servers, and GC stale spillover. [`Harness::build_agent`] does the
//! per-session work: a fresh `BackgroundManager`, a fresh context-engine
//! stack (compactor + spillover caches must not be shared across sessions),
//! per-session markdown sub-agent tools, and the assembled `Agent`.
//!
//! The same harness implements [`runic_serve::AgentFactory`], so the server
//! builds the *same* fully-equipped agent the REPL gets — just keyed by
//! `(tenant, thread_id)` and resumed from persisted events.

use std::sync::Arc;

use anyhow::{Context, Result};
use runic_agent_core::{
    Agent, AgentConfig, AgentState, ApproverHandle, AsyncSubagentTool, BackgroundManager,
    SubagentTool,
};
use runic_agents::{AgentRegistry, DispatchMode, FilesystemMode};
use runic_blobs::{BlobMaterializingProvider, BlobStore, BlobStoreResolver, FileBlobStore};
use runic_commands::CommandRegistry;
use runic_context_engine::{
    BackgroundTaskReminder, BasePromptLayer, CompactorEngine, CompositeEngine, ContextEngine,
    MemoryLayer, PersonaLayer, ReminderEngine, SpilloverEngine, UserFactsLayer,
};
use runic_mcp::{McpConfig, McpManager};
use runic_memory::{BoundedMemoryStore, MemoryTool};
use runic_plugins::PluginManager;
use runic_provider_core::Provider;
use runic_sessions::{spawn_persister, replay_into_state, FileSessionStore, SessionStore};
use runic_shell_tools::{EditFileTool, GlobTool, GrepTool, LsTool, ReadFileTool, WriteFileTool};
use runic_skills::{SkillRegistry, SkillViewTool, SkillsIndexLayer};
use runic_storage_backend::{LocalFsBackend, RootedBackend, StorageBackend};

use crate::config::RunicConfig;
use crate::demo_tools::{EchoTool, EmailTool, SessionUuid, SlowCountTool};
use crate::hooks::{BindUserContextHook, LoggingHook};

pub const DEFAULT_SYSTEM_PROMPT: &str = "\
You are a focused test assistant for the runic agent harness.

Keep replies short. When asked to demonstrate a tool, use it; otherwise reply directly.
Available tools:
  - echo: returns the message you pass in (useful to confirm tool dispatch works).
";

/// Shared, session-independent agent wiring.
pub struct Harness {
    pub config: RunicConfig,
    provider: Arc<dyn Provider>,
    storage: Arc<dyn StorageBackend>,
    session_store: Arc<dyn SessionStore>,
    skill_registry: Arc<SkillRegistry>,
    agent_registry: Arc<AgentRegistry>,
    command_registry: Arc<CommandRegistry>,
    mcp_manager: McpManager,
}

impl Harness {
    /// Do all the once-per-process setup. Network I/O (MCP connect) and
    /// disk I/O (registry loads, spillover GC) happen here.
    pub async fn load(config: RunicConfig) -> Result<Self> {
        eprintln!("[runic-home] {}", config.runic_home.display());

        let storage: Arc<dyn StorageBackend> =
            Arc::new(LocalFsBackend::new(config.runic_home.clone()));

        // Blob-wrap the provider before anything clones it, so every clone
        // is automatically blob-aware.
        let raw_provider = config.build_raw_provider()?;
        let blob_store: Arc<dyn BlobStore> = Arc::new(FileBlobStore::new(storage.clone()));
        let provider: Arc<dyn Provider> = Arc::new(BlobMaterializingProvider::new(
            raw_provider,
            Arc::new(BlobStoreResolver::new(blob_store, config.tenant.clone())),
        ));
        eprintln!("[blobs] materializing blob references via tenant '{}'", config.tenant);

        // Plugins → merged into skills/agents below.
        let plugin_manager = PluginManager::load(storage.clone(), "plugins")
            .await
            .context("loading plugins from ~/.runic/plugins/")?;
        if !plugin_manager.is_empty() {
            eprintln!(
                "[plugins] loaded {} plugin(s): {:?} ({} skill(s), {} agent(s) total)",
                plugin_manager.len(),
                plugin_manager.names(),
                plugin_manager.total_skills(),
                plugin_manager.total_agents()
            );
        }

        let skill_registry = Arc::new({
            let mut reg = SkillRegistry::load(storage.clone(), "skills")
                .await
                .context("loading skills from ~/.runic/skills/")?;
            for s in plugin_manager.aggregate_skills().list() {
                reg.insert(s.clone());
            }
            reg
        });
        eprintln!(
            "[skills] {} total skill(s): {:?}",
            skill_registry.len(),
            skill_registry.list().iter().map(|s| s.meta.name.as_str()).collect::<Vec<_>>()
        );

        let command_registry = Arc::new(
            CommandRegistry::load(storage.clone(), "commands")
                .await
                .context("loading commands from ~/.runic/commands/")?,
        );
        if !command_registry.is_empty() {
            eprintln!(
                "[commands] {} command(s): {:?}",
                command_registry.len(),
                command_registry.list().iter().map(|c| c.meta.name.as_str()).collect::<Vec<_>>()
            );
        }

        let agent_registry = Arc::new({
            let mut reg = AgentRegistry::load(storage.clone(), "agents")
                .await
                .context("loading agents from ~/.runic/agents/")?;
            for a in plugin_manager.aggregate_agents().list() {
                reg.insert(a.clone());
            }
            reg
        });
        eprintln!(
            "[agents] {} markdown agent(s): {:?}",
            agent_registry.len(),
            agent_registry.list().iter().map(|a| a.def.name.as_str()).collect::<Vec<_>>()
        );

        // MCP: missing/empty config is fine.
        let mcp_config_path = config.runic_home.join("mcp.json");
        let mcp_manager = match McpConfig::try_load_from_path(&mcp_config_path).await {
            Ok(Some(cfg)) if !cfg.is_empty() => {
                eprintln!("[mcp] connecting to {} server(s) from {}", cfg.len(), mcp_config_path.display());
                let mgr = McpManager::connect_all(&cfg).await;
                eprintln!(
                    "[mcp] {} server(s) connected, {} tool(s): {:?}",
                    mgr.len(),
                    mgr.total_tool_count(),
                    mgr.server_names()
                );
                mgr
            }
            Ok(_) => {
                eprintln!("[mcp] no mcp.json (or empty) — skipping");
                McpManager::new()
            }
            Err(err) => {
                eprintln!("[mcp] could not load {}: {err}", mcp_config_path.display());
                McpManager::new()
            }
        };

        // Sweep stale spillover once at startup.
        if config.spillover_retention_days > 0 {
            let report = runic_context_engine::gc_spillover(
                storage.as_ref(),
                "spillover",
                chrono::Duration::days(config.spillover_retention_days),
            )
            .await;
            if report.deleted_files > 0 {
                eprintln!(
                    "[spillover] gc: deleted {} stale files ({} bytes), kept {}",
                    report.deleted_files, report.freed_bytes, report.kept_files
                );
            }
        }

        let session_store: Arc<dyn SessionStore> =
            Arc::new(FileSessionStore::new(storage.clone()));

        eprintln!("[context] compact_threshold={} tokens, spillover_threshold={} bytes",
            config.compact_threshold, config.spill_threshold);

        Ok(Self {
            config,
            provider,
            storage,
            session_store,
            skill_registry,
            agent_registry,
            command_registry,
            mcp_manager,
        })
    }

    pub fn command_registry(&self) -> &Arc<CommandRegistry> {
        &self.command_registry
    }

    pub fn session_store(&self) -> &Arc<dyn SessionStore> {
        &self.session_store
    }

    pub fn provider_model(&self) -> String {
        self.provider.model()
    }

    /// Build the per-session context-engine stack. A fresh instance per
    /// session: the compactor's summary cache and spillover's spilled-set
    /// are per-conversation state that must not bleed across sessions.
    fn build_context_engine(&self, background_manager: Arc<BackgroundManager>) -> Arc<dyn ContextEngine> {
        let composite = CompositeEngine::new()
            .with_layer(BasePromptLayer::new(DEFAULT_SYSTEM_PROMPT))
            .with_layer(PersonaLayer::new(self.storage.clone(), "SOUL.md"))
            .with_layer(UserFactsLayer::new(self.storage.clone(), "memory/USER.md").frozen())
            .with_layer(MemoryLayer::new(self.storage.clone(), "memory/MEMORY.md").frozen())
            .with_layer(SkillsIndexLayer::new(self.skill_registry.clone()));

        let inner: Arc<dyn ContextEngine> = Arc::new(composite);
        let compacted: Arc<dyn ContextEngine> = Arc::new(
            CompactorEngine::new(inner, self.provider.clone())
                .with_token_threshold(self.config.compact_threshold),
        );
        let spilled: Arc<dyn ContextEngine> = Arc::new(SpilloverEngine::with_settings(
            compacted,
            self.storage.clone(),
            "spillover",
            self.config.spill_threshold,
            runic_context_engine::DEFAULT_PREVIEW_CHARS,
        ));
        Arc::new(
            ReminderEngine::new(spilled)
                .with_reminder(BackgroundTaskReminder::new(background_manager)),
        )
    }

    /// Build the shared sub-agent pool (the tools markdown sub-agents can
    /// pull from via `allowed-tools`). These dispatchers capture only
    /// shared Arcs, so the pool is rebuilt cheaply per session.
    fn build_subagent_pool(
        &self,
        memory_tool: &Arc<MemoryTool>,
        shell: &ShellTools,
    ) -> Arc<runic_tool_core::ToolRegistry> {
        let mut pool = runic_tool_core::ToolRegistry::new();
        pool.register(Arc::new(EchoTool));
        pool.register(memory_tool.clone());
        shell.register_into(&mut pool);
        pool.register_hitl(Arc::new(EmailTool));
        pool.register_background(Arc::new(SlowCountTool));
        for tool in self.mcp_manager.all_tools() {
            pool.register(tool);
        }
        Arc::new(pool)
    }

    /// The per-session sub-agent persister: writes child events under
    /// `sessions/<tenant>/<parent_session>/subagents/<name>/<child>/...`.
    /// `None` when persistence is disabled.
    fn subagent_persister(&self, parent_session_id: &str) -> Option<runic_agents::SubagentPersisterFn> {
        if !self.config.persist {
            return None;
        }
        let store = self.session_store.clone();
        let tenant = self.config.tenant.clone();
        let parent_id = parent_session_id.to_string();
        Some(Arc::new(move |agent_name: &str, child_session_id: String, rx| {
            let composite_id = format!("{parent_id}/subagents/{agent_name}/{child_session_id}");
            let _ = spawn_persister(rx, store.clone(), tenant.clone(), composite_id);
        }))
    }

    /// Build a fully-wired agent for `session_id`. When `restore` is given
    /// (a replayed [`AgentState`]), the conversation resumes from it.
    /// `approver` supplies the HITL approval channel (stdin in the REPL;
    /// `None` on the server, where HITL tools simply have no approver).
    ///
    /// If persistence is enabled, a persister task is spawned here so the
    /// agent's events are durably written — which is what makes a later
    /// cold rebuild able to replay.
    pub fn build_agent(
        &self,
        session_id: Option<String>,
        restore: Option<AgentState>,
        approver: Option<ApproverHandle>,
    ) -> Agent {
        let background_manager = Arc::new(BackgroundManager::new());
        let context_engine = self.build_context_engine(background_manager.clone());

        let memory_store =
            Arc::new(BoundedMemoryStore::new(self.storage.clone()).with_lock_dir(self.config.runic_home.clone()));
        let memory_tool = Arc::new(MemoryTool::new(memory_store));
        let shell = ShellTools::new(self.storage.clone());
        let subagent_pool = self.build_subagent_pool(&memory_tool, &shell);

        // Synchronous + async generic research sub-agents (no persister —
        // they capture only shared Arcs).
        let make_factory = |provider: Arc<dyn Provider>, system_prompt: &'static str, max_turns: u32| {
            move || {
                Agent::builder(provider.clone())
                    .system_prompt(system_prompt)
                    .config(AgentConfig { max_turns, ..Default::default() })
                    .build()
            }
        };
        let research = SubagentTool::new(
            "research_assistant",
            "Spawn a focused synchronous subagent that investigates the prompt and returns a concise summary. The parent waits for the answer. The subagent has fresh context — be self-contained in the prompt.",
            make_factory(self.provider.clone(),
                "You are a focused research subagent. Investigate the user's prompt and return a concise summary in 3-6 lines. Do not ask clarifying questions — make reasonable assumptions and answer.",
                8),
        );
        let deep_research = AsyncSubagentTool::new(
            "deep_research",
            "Spawn an ASYNCHRONOUS subagent for longer investigations. Returns a task_id immediately so you can keep working; check progress with background_status(task_id) and read the result when status is 'done'.",
            make_factory(self.provider.clone(),
                "You are a deep research subagent. Take your time, explore the question thoroughly, and produce a thorough multi-paragraph answer.",
                16),
        );

        let skill_view = SkillViewTool::new(self.skill_registry.clone(), self.storage.clone(), "skills");

        let mut builder = Agent::builder(self.provider.clone())
            .system_prompt(DEFAULT_SYSTEM_PROMPT)
            .context_engine_arc(context_engine)
            .background_manager(background_manager)
            .tool(Arc::new(EchoTool))
            .tool(memory_tool.clone())
            .tool(Arc::new(skill_view))
            .tool(Arc::new(research))
            .hitl_tool(Arc::new(EmailTool))
            .background_tool(Arc::new(SlowCountTool))
            .background_tool(Arc::new(deep_research))
            .hook(Arc::new(LoggingHook))
            .hook(Arc::new(BindUserContextHook {
                user_id: self.config.user_id.clone(),
                org_id: self.config.org_id.clone(),
            }))
            .runtime(SessionUuid(uuid::Uuid::new_v4()));
        builder = shell.register_builder(builder);
        if let Some(approver) = approver {
            builder = builder.runtime(approver);
        }
        for tool in self.mcp_manager.all_tools() {
            builder = builder.tool(tool);
        }

        // We need the session id NOW to wire the sub-agent persister; if
        // the caller didn't pin one, mint it here so persistence paths
        // line up with the agent we build.
        let session_id = session_id.unwrap_or_else(|| {
            restore
                .as_ref()
                .map(|s| s.session_id.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
        });
        builder = builder.session_id(session_id.clone());
        if let Some(state) = restore {
            builder = builder.restore_state(state);
        }

        // Register each markdown sub-agent, shaping its pool by filesystem
        // mode, and wiring this session's sub-agent persister.
        let persister = self.subagent_persister(&session_id);
        for md_agent in self.agent_registry.list() {
            let pool_for_agent = match md_agent.def.filesystem.mode {
                FilesystemMode::Shared => subagent_pool.clone(),
                FilesystemMode::None => {
                    let mut scoped = (*subagent_pool).clone();
                    runic_shell_tools::deregister_all(&mut scoped);
                    Arc::new(scoped)
                }
                FilesystemMode::Isolated => {
                    let isolated: Arc<dyn StorageBackend> = match md_agent.def.filesystem.resolved_path() {
                        Some(path) => Arc::new(LocalFsBackend::new(&path)),
                        None => {
                            let root = md_agent.def.filesystem.root.as_deref()
                                .expect("registry::load validated this");
                            Arc::new(RootedBackend::new(self.storage.clone(), root))
                        }
                    };
                    let mut scoped = (*subagent_pool).clone();
                    runic_shell_tools::register_all(&mut scoped, isolated);
                    Arc::new(scoped)
                }
            };

            match md_agent.def.dispatch {
                DispatchMode::Sync => {
                    let tool = md_agent.make_subagent_tool_with_context(
                        self.provider.clone(), pool_for_agent, self.skill_registry.clone(),
                        self.storage.clone(), "skills", persister.clone(),
                    );
                    builder = builder.tool(Arc::new(tool));
                }
                DispatchMode::Async => {
                    let tool = md_agent.make_async_subagent_tool_with_context(
                        self.provider.clone(), pool_for_agent, self.skill_registry.clone(),
                        self.storage.clone(), "skills", persister.clone(),
                    );
                    builder = builder.background_tool(Arc::new(tool));
                }
            }
        }

        let agent = builder.build();

        // Spawn the persister so events are durably written. Dropping the
        // handle does NOT abort the task — it runs until the agent's
        // broadcast channel closes (i.e. the agent is dropped).
        if self.config.persist {
            let _ = spawn_persister(
                agent.subscribe_events(),
                self.session_store.clone(),
                self.config.tenant.clone(),
                session_id,
            );
        }

        agent
    }
}

#[async_trait::async_trait]
impl runic_serve::AgentFactory for Harness {
    async fn build(&self, tenant: &str, session_id: &str) -> Agent {
        // Resume from persisted events when any exist, so a thread that
        // went cold (server restart, pool eviction) comes back with its
        // full history instead of a blank slate.
        let restore = match replay_into_state(
            self.session_store.as_ref(),
            tenant,
            session_id,
            DEFAULT_SYSTEM_PROMPT,
        )
        .await
        {
            Ok(state) if !state.events.is_empty() => {
                eprintln!("[serve] thread {session_id}: replayed {} events", state.events.len());
                Some(state)
            }
            Ok(_) => None,
            Err(err) => {
                eprintln!("[serve] thread {session_id}: replay failed ({err}); starting fresh");
                None
            }
        };
        // Server agents get no stdin approver — HITL tools have no channel.
        self.build_agent(Some(session_id.to_string()), restore, None)
    }
}

/// Bundle of the six filesystem tools over one backend, so they can be
/// registered into both the parent builder and sub-agent pools without
/// re-listing them at every call site.
struct ShellTools {
    read: Arc<ReadFileTool>,
    write: Arc<WriteFileTool>,
    edit: Arc<EditFileTool>,
    ls: Arc<LsTool>,
    glob: Arc<GlobTool>,
    grep: Arc<GrepTool>,
}

impl ShellTools {
    fn new(storage: Arc<dyn StorageBackend>) -> Self {
        Self {
            read: Arc::new(ReadFileTool::new(storage.clone())),
            write: Arc::new(WriteFileTool::new(storage.clone())),
            edit: Arc::new(EditFileTool::new(storage.clone())),
            ls: Arc::new(LsTool::new(storage.clone())),
            glob: Arc::new(GlobTool::new(storage.clone())),
            grep: Arc::new(GrepTool::new(storage)),
        }
    }

    fn register_into(&self, pool: &mut runic_tool_core::ToolRegistry) {
        pool.register(self.read.clone());
        pool.register(self.write.clone());
        pool.register(self.edit.clone());
        pool.register(self.ls.clone());
        pool.register(self.glob.clone());
        pool.register(self.grep.clone());
    }

    fn register_builder(&self, builder: runic_agent_core::AgentBuilder) -> runic_agent_core::AgentBuilder {
        builder
            .tool(self.read.clone())
            .tool(self.write.clone())
            .tool(self.edit.clone())
            .tool(self.ls.clone())
            .tool(self.glob.clone())
            .tool(self.grep.clone())
    }
}
