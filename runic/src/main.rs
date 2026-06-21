use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;

use runic_agent::Agent;
use runic_filesystem::{FilesystemBackend, LocalFs};
use runic_memory::{BoundedMemoryStore, MemoryTool};
use runic_provider::Provider;
use runic_provider::openai::OpenAIDriver;
use runic_serve::{AgentFactory, HumanHub, ServeConfig, router};
use runic_skills::{SkillRegistry, SkillViewTool, skills_prompt_section};
use runic_subagent::{
    AgentDef, AgentRoster, ChildBuilder, DelegateTool, DelegationCtx, roster_prompt_section,
};
use runic_substrate::{PostgresSessionStore, SearchChatsTool, SessionStore};
use runic_tools::{WeatherHistoryTool, WeatherTool, WebFetchTool, default_tools};

/// Default persona when no SOUL.md is found.
const DEFAULT_PERSONA: &str = "You are a helpful assistant with persistent memory and tools. \
Save durable facts about the user and the environment with the `memory` tool. Use \
`search_chats` to recall things from past conversations, `web_fetch` to read a URL, `skill_view` \
to load a skill's instructions, and `delegate` to hand a self-contained task to a subagent.";

/// Per-process shared state. `build` mints a fresh agent per thread, composing
/// the system prompt (persona + memory + skills + subagents) and registering
/// the full tool surface.
struct RunicFactory {
    provider: Arc<dyn Provider>,
    model: String,
    fs: Arc<dyn FilesystemBackend>,     // tool workspace
    mem_fs: Arc<dyn FilesystemBackend>, // curated memory (separate backend)
    sessions: Arc<dyn SessionStore>,    // for search_chats
    persona: String,                    // SOUL.md (or the default)
    skills: Arc<SkillRegistry>,
    roster: Arc<AgentRoster>,
}

#[async_trait]
impl AgentFactory for RunicFactory {
    async fn build(&self, tenant: &str, session_id: &str) -> Agent {
        // Curated memory: ONE store, shared between the snapshot read (below)
        // and the `memory` tool (writes), so reads and writes stay coherent.
        let store = Arc::new(BoundedMemoryStore::new(self.mem_fs.clone()));
        let memory_section = store
            .snapshot()
            .await
            .map(|s| s.section(true, true))
            .unwrap_or_default();

        // ── Context assembly: persona + memory + skills roster + subagent roster.
        let system = [
            self.persona.as_str(),
            memory_section.as_str(),
            &skills_prompt_section(&self.skills),
            &roster_prompt_section(&self.roster),
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

        // Key the agent to the tenant so search_chats scopes to the right
        // conversations (its tenant comes from ctx.user_id).
        let mut b = Agent::builder(self.provider.clone(), tenant, session_id)
            .model(&self.model)
            .system_prompt(system);

        // Built-in fs + utility tools.
        for t in default_tools(self.fs.clone()) {
            b = b.tool(t);
        }
        // Memory (same store the snapshot read from), chat search, web, skills.
        b = b.tool(Arc::new(MemoryTool::new(store)));
        b = b.tool(Arc::new(SearchChatsTool::new(self.sessions.clone())));
        b = b.tool(Arc::new(WebFetchTool::new()));
        b = b.tool(Arc::new(WeatherTool::new()));
        b = b.tool(Arc::new(WeatherHistoryTool::new()));
        b = b.tool(Arc::new(SkillViewTool::new(self.skills.clone())));

        // Subagent delegation.
        let child_builder: Arc<dyn ChildBuilder> = Arc::new(RunicChildBuilder {
            provider: self.provider.clone(),
            model: self.model.clone(),
            fs: self.fs.clone(),
        });
        b = b.tool(Arc::new(DelegateTool::new(
            self.roster.clone(),
            child_builder,
        )));

        b.build()
    }
}

/// Builds the child [`Agent`] for a subagent definition (the `delegate` tool
/// calls this). The child gets the fs/utility tools but NOT memory or delegate
/// (no escalation), and runs on the AGENT.md system prompt.
struct RunicChildBuilder {
    provider: Arc<dyn Provider>,
    model: String,
    fs: Arc<dyn FilesystemBackend>,
}

#[async_trait]
impl ChildBuilder for RunicChildBuilder {
    async fn build(&self, def: &AgentDef, _dctx: &DelegationCtx) -> Result<Agent> {
        let model = def.model.clone().unwrap_or_else(|| self.model.clone());
        let mut b = Agent::builder(self.provider.clone(), "subagent", &def.name)
            .model(model)
            .system_prompt(&def.system_prompt);
        for t in default_tools(self.fs.clone()) {
            b = b.tool(t);
        }
        if let Some(max_turns) = def.max_turns {
            b = b.max_turns(max_turns);
        }
        Ok(b.build())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt().with_target(false).init();

    let (provider, model) = select_provider()?;

    let root_dir = "/Users/machache/learner/runic/core/runic_fs";
    std::fs::create_dir_all(root_dir).ok();
    let fs: Arc<dyn FilesystemBackend> = Arc::new(LocalFs::new(root_dir));

    let mem_dir = format!("{root_dir}/memories");
    std::fs::create_dir_all(&mem_dir).ok();
    let mem_fs: Arc<dyn FilesystemBackend> = Arc::new(LocalFs::new(&mem_dir));

    // Persona: SOUL.md if present, else the default (hermes-style identity file).
    let persona = std::fs::read_to_string(format!("{root_dir}/SOUL.md"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_PERSONA.to_string());

    // Skills + subagents: load from dirs (empty registry/roster if absent).
    let skills_dir =
        std::env::var("RUNIC_SKILLS_DIR").unwrap_or_else(|_| format!("{root_dir}/skills"));
    let skills = Arc::new(
        SkillRegistry::from_dir(&skills_dir).unwrap_or_else(|_| SkillRegistry::new(vec![])),
    );
    let agents_dir =
        std::env::var("RUNIC_AGENTS_DIR").unwrap_or_else(|_| format!("{root_dir}/agents"));
    let roster =
        Arc::new(AgentRoster::from_dir(&agents_dir).unwrap_or_else(|_| AgentRoster::new(vec![])));
    tracing::info!(
        "loaded {} skill(s), {} subagent(s)",
        skills.len(),
        roster.len()
    );

    // Postgres session store — connect() also runs the migrations.
    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL not set")?;
    let session_store: Arc<dyn SessionStore> =
        Arc::new(PostgresSessionStore::connect(&database_url).await?);

    let factory = Arc::new(RunicFactory {
        provider,
        model,
        fs,
        mem_fs,
        sessions: session_store.clone(),
        persona,
        skills,
        roster,
    });

    let app = router(ServeConfig {
        session_store,
        agent_factory: factory,
        human_hub: Arc::new(HumanHub::new()),
    });

    let addr = "127.0.0.1:8920";
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("runic serving on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Mistral via the OpenAI-compatible driver. `RUNIC_MODEL` overrides the model.
fn select_provider() -> Result<(Arc<dyn Provider>, String)> {
    let api_key = std::env::var("MISTRAL_API_KEY").context("MISTRAL_API_KEY not set")?;
    let base_url =
        std::env::var("MISTRAL_BASE_URL").unwrap_or_else(|_| "https://api.mistral.ai/v1".into());
    let model = std::env::var("RUNIC_MODEL").unwrap_or_else(|_| "mistral-medium-latest".into());
    let provider: Arc<dyn Provider> = Arc::new(OpenAIDriver::new(api_key, base_url));
    tracing::info!("provider: mistral, model: {model}");
    Ok((provider, model))
}
