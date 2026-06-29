//! Coral on the runic harness — Maia, a French TC assistant, orchestrating
//! three gated specialists (product / purchase / crm), each on its own model
//! with its own MCP toolset from the Google MCP Toolbox.
//!
//! Served over `runic-serve` so the web UI can chat with it: one warm agent per
//! thread, every event persisted to Postgres. Definitions live next to this
//! file as data — `coral.md` (Maia), `subagents/*.md`, `mcp.json`.

mod builder;
mod factory;
mod hooks;
mod memory;
mod providers;

use std::sync::Arc;

use anyhow::{Context, Result};
use runic_serve::{HumanHub, ServeConfig, router};
use runic_subagent::SubagentBuilder;
use runic_substrate::sessions_postgres;

use builder::CoralBuilder;
use factory::{CoralFactory, load_toolsets, parse_hook_provider_names};
use providers::Providers;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    let providers = Providers::from_env()?;
    let toolsets = load_toolsets().await;

    let database_url =
        std::env::var("DATABASE_URL").context("set DATABASE_URL for session persistence")?;
    let sessions = sessions_postgres(&database_url).await;
    let store = sessions.store();

    let builder: Arc<dyn SubagentBuilder> =
        Arc::new(CoralBuilder::new(providers.clone(), toolsets.clone()));

    let factory = Arc::new(CoralFactory {
        providers,
        provider_name: std::env::var("CORAL_PROVIDER").unwrap_or_else(|_| "sonnet".to_string()),
        hook_provider_names: parse_hook_provider_names(std::env::var("CORAL_HOOK_PROVIDERS").ok()),
        toolsets,
        builder,
        store: store.clone(),
        tavily_key: std::env::var("TAVILY_API_KEY")
            .ok()
            .filter(|s| !s.is_empty()),
        composio_key: std::env::var("COMPOSIO_API_KEY")
            .ok()
            .filter(|s| !s.is_empty()),
    });

    let app = router(ServeConfig {
        session_store: store,
        agent_factory: factory,
        human_hub: Arc::new(HumanHub::new()),
    });

    let addr = std::env::var("CORAL_ADDR").unwrap_or_else(|_| "127.0.0.1:8920".to_string());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "coral serve listening");
    axum::serve(listener, app).await?;
    Ok(())
}
