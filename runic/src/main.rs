//! `runic` — the binary. One file: wire the new stack into an HTTP server.
//!
//! Reads `.env` for the provider key, builds an [`AgentFactory`] that mints a
//! fresh [`Agent`] per thread (provider + filesystem tools + curated memory),
//! and mounts the [`runic_serve`] router. Per request, the `X-Runic-Tenant`
//! header + the request-body `context` become the agent's per-run context;
//! HITL `ask_user` prompts resolve over the serve crate's answer endpoint.
//!
//! Configuration (all via env, sensible defaults):
//!   - `ANTHROPIC_API_KEY`   — required.
//!   - `ANTHROPIC_BASE_URL`  — default `https://api.anthropic.com`.
//!   - `RUNIC_MODEL`         — default `claude-3-5-sonnet-latest`.
//!   - `RUNIC_WORKSPACE`     — tool filesystem root, default `./workspace`.
//!   - `RUNIC_BIND`          — listen addr, default `127.0.0.1:8920`.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;

use runic_agent::Agent;
use runic_filesystem::{FilesystemBackend, LocalFs, MemoryFs};
use runic_memory::{BoundedMemoryStore, MemoryTool};
use runic_provider::{AnthropicDriver, Provider};
use runic_serve::{router, AgentFactory, HumanHub, ServeConfig};
use runic_substrate::{MemorySessionStore, SessionStore};
use runic_tools::default_tools;

const SYSTEM_PROMPT: &str =
    "You are a helpful assistant. Use your tools to read and write files, do \
     math, check the time, and remember durable facts about the user and the \
     environment.";

/// Per-process shared state. `build` mints a fresh agent per thread, cloning
/// the shared provider / filesystem / memory backend in cheaply.
struct RunicFactory {
    provider: Arc<dyn Provider>,
    model: String,
    fs: Arc<dyn FilesystemBackend>,
    /// Curated-memory store — a *separate* backend from the tool `fs`, so the
    /// agent's own `write_file` can never clobber MEMORY.md / USER.md.
    memory_backend: Arc<dyn FilesystemBackend>,
}

#[async_trait]
impl AgentFactory for RunicFactory {
    async fn build(&self, _tenant: &str, session_id: &str) -> Agent {
        let mut builder = Agent::builder(self.provider.clone(), "default", session_id)
            .model(&self.model)
            .system_prompt(SYSTEM_PROMPT);

        // Built-in filesystem + utility tools, then curated memory.
        for tool in default_tools(self.fs.clone()) {
            builder = builder.tool(tool);
        }
        let store = Arc::new(BoundedMemoryStore::new(self.memory_backend.clone()));
        builder = builder.tool(Arc::new(MemoryTool::new(store)));

        builder.build()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt().with_target(false).init();

    let api_key = std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY not set")?;
    let base_url =
        std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| "https://api.anthropic.com".into());
    let model = std::env::var("RUNIC_MODEL").unwrap_or_else(|_| "claude-3-5-sonnet-latest".into());
    let provider: Arc<dyn Provider> = Arc::new(AnthropicDriver::new(api_key, base_url));

    // Tool filesystem (sandbox root) + an in-RAM memory backend.
    let workspace = std::env::var("RUNIC_WORKSPACE").unwrap_or_else(|_| "./workspace".into());
    std::fs::create_dir_all(&workspace).ok();
    let fs: Arc<dyn FilesystemBackend> = Arc::new(LocalFs::new(&workspace));
    let memory_backend: Arc<dyn FilesystemBackend> = Arc::new(MemoryFs::new());

    let factory = Arc::new(RunicFactory { provider, model, fs, memory_backend });

    // Event-sourced sessions (in-RAM; swap for PostgresSessionStore in prod).
    let session_store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let human_hub = Arc::new(HumanHub::new());

    let app = router(ServeConfig {
        session_store,
        agent_factory: factory,
        human_hub,
    });

    let addr = std::env::var("RUNIC_BIND").unwrap_or_else(|_| "127.0.0.1:8920".into());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!("runic serving on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
