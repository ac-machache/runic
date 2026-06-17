//! runic — serves the coral agent ("Maia") over the `runic-serve` HTTP API.
//!
//! Reads `.env` (provider API keys), assembles [`agent::build::MaiaFactory`]
//! (the per-process shared state), and mounts the `runic-serve` router. Per
//! request, the tenant header + request-body `context` become the agent's
//! per-run context (identity, web opt-in, provider override) — there is no
//! build-time/env-baked context. The Leptos UI (`runic-web`) drives this
//! server over CORS.

mod agent;

use std::sync::Arc;

use anyhow::Result;
use runic_serve::{router, ApprovalHub, BoxedAgentFactory, ServeConfig};
use runic_sessions::{FileSessionStore, PostgresSessionStore, SessionStore};
use runic_storage_backend::{LocalFsBackend, StorageBackend};

use agent::build::MaiaFactory;

fn runic_home() -> std::path::PathBuf {
    std::env::var("RUNIC_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let mut p = std::env::var("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default();
            p.push(".runic");
            p
        })
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,runic=info,runic_serve=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let approval_hub = Arc::new(ApprovalHub::new());
    let factory: BoxedAgentFactory =
        Arc::new(MaiaFactory::new(approval_hub.clone()).await?);

    // Session store: Postgres when DATABASE_URL is set (multi-tenant,
    // production), else a file store under ~/.runic/sessions (zero infra for
    // local runs). Both implement SessionStore; the server is agnostic.
    let session_store: Arc<dyn SessionStore> = match std::env::var("DATABASE_URL") {
        Ok(url) => {
            tracing::info!("session store: Postgres");
            Arc::new(PostgresSessionStore::connect(&url).await?)
        }
        Err(_) => {
            tracing::info!("session store: file (~/.runic/sessions)");
            let sessions: Arc<dyn StorageBackend> =
                Arc::new(LocalFsBackend::new(runic_home().join("sessions")));
            Arc::new(FileSessionStore::new(sessions))
        }
    };

    let app = router(ServeConfig {
        session_store,
        agent_factory: factory,
        approval_hub,
    });

    let addr = std::env::var("RUNIC_ADDR").unwrap_or_else(|_| "127.0.0.1:8920".to_string());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    print_test_banner(&addr);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    eprintln!("\nrunic stopped.");
    Ok(())
}

/// Print a copy-paste recipe for driving the agent, so a fresh `cargo run`
/// is immediately testable from the terminal.
fn print_test_banner(addr: &str) {
    let base = format!("http://{addr}");
    eprintln!(
        "\n\
         ╭─ runic serving (Maia) on {base}\n\
         │  health:  curl -s {base}/healthz\n\
         │  chat (SSE stream):\n\
         │    curl -N -H 'Content-Type: application/json' -H 'X-Runic-Tenant: org1' \\\n\
         │      -d '{{\"message\":\"bonjour\",\"context\":{{\"user_id\":\"u1\",\"allow_web_search\":false}}}}' \\\n\
         │      {base}/threads/t1/runs/stream\n\
         │  per-run model override: add \"provider\":\"haiku\" (or \"gemini\"/\"mistral\") to context\n\
         │  NOTE: tool calls need the MCP toolbox at 127.0.0.1:5050; web search needs a real Tavily key.\n\
         ╰─ Ctrl-C to stop.\n"
    );
}

/// Resolve when the user presses Ctrl-C, so the server drains in-flight
/// runs and exits cleanly.
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
