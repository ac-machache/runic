//! `runic serve` — boot the HTTP server.
//!
//! Owns the network layer (binds the socket, `axum::serve`, graceful
//! shutdown). The route surface and agent lifecycle come from
//! `runic-serve`; the agent wiring comes from [`Harness`], which doubles
//! as the [`runic_serve::AgentFactory`].

use std::sync::Arc;

use anyhow::{Context, Result};
use runic_serve::{router, ServeConfig};

use crate::harness::Harness;

/// Default bind address; override with `RUNIC_SERVE_ADDR`.
const DEFAULT_ADDR: &str = "127.0.0.1:8080";

pub async fn run(harness: Harness) -> Result<()> {
    let addr = std::env::var("RUNIC_SERVE_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.into());

    let session_store = harness.session_store().clone();
    let model = harness.provider_model();
    // The harness IS the factory. Arc it so the pool can build agents on
    // demand, keyed by (tenant, thread_id).
    let factory: Arc<dyn runic_serve::AgentFactory> = Arc::new(harness);

    let app = router(ServeConfig { session_store, agent_factory: factory });

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;

    eprintln!("[serve] runic listening on http://{addr} (model={model})");
    eprintln!("[serve] tenant via X-Runic-Tenant header (defaults to 'default')");
    eprintln!("[serve] POST /threads · POST /threads/:id/runs/stream · GET .../runs/:run_id/stream");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("\n[serve] shutting down");
        })
        .await
        .context("axum serve")?;

    Ok(())
}
