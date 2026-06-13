//! `AppState` and the top-level `router()` factory.

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use runic_sessions::SessionStore;
use tower_http::cors::CorsLayer;

use crate::approval::ApprovalHub;
use crate::factory::BoxedAgentFactory;
use crate::pool::ThreadPool;
use crate::routes::{health, runs, threads};

/// Everything every handler needs. Cheap to clone (all internal data
/// is `Arc`-wrapped); axum requires `State<S>` to be `Clone`.
#[derive(Clone)]
pub struct AppState {
    pub session_store: Arc<dyn SessionStore>,
    pub pool: Arc<ThreadPool>,
    /// Bridges parked HITL approvals to the decision endpoint.
    pub approval_hub: Arc<ApprovalHub>,
}

/// Construction parameters — the binary fills these in and hands them
/// to [`router`].
pub struct ServeConfig {
    pub session_store: Arc<dyn SessionStore>,
    pub agent_factory: BoxedAgentFactory,
    /// Shared with the `ChannelApprover` installed in each server agent, so
    /// approvals raised inside a run can be resolved via the HTTP endpoint.
    pub approval_hub: Arc<ApprovalHub>,
}

/// Build the axum `Router` with every Phase-1 endpoint mounted. The
/// binary owns the network layer (`axum::serve` / TLS / shutdown);
/// this crate just produces the route surface.
pub fn router(config: ServeConfig) -> Router {
    let state = AppState {
        session_store: config.session_store,
        pool: Arc::new(ThreadPool::new(config.agent_factory)),
        approval_hub: config.approval_hub,
    };

    Router::new()
        .route("/healthz", get(health::healthz))
        .route(
            "/threads",
            post(threads::create_thread).get(threads::list_threads),
        )
        .route(
            "/threads/{thread_id}",
            get(threads::get_thread).delete(threads::delete_thread),
        )
        .route("/threads/{thread_id}/events", get(threads::thread_events))
        .route(
            "/threads/{thread_id}/runs/stream",
            post(runs::create_and_stream_run),
        )
        .route(
            "/threads/{thread_id}/runs/{run_id}/stream",
            get(runs::replay_run),
        )
        .route(
            "/threads/{thread_id}/runs/{run_id}/approvals/{call_id}",
            post(runs::submit_approval),
        )
        // Permissive CORS so a browser dev UI served from another origin
        // (e.g. trunk on :8080) can drive the server on :8920.
        .layer(CorsLayer::permissive())
        .with_state(state)
}

// Re-export to keep the public surface tidy.
pub use runic_sessions::FileSessionStore;
