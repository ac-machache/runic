//! `AppState` and the top-level `router()` factory.

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use runic_sessions::SessionStore;

use crate::factory::BoxedAgentFactory;
use crate::pool::ThreadPool;
use crate::routes::{health, runs, threads};

/// Everything every handler needs. Cheap to clone (all internal data
/// is `Arc`-wrapped); axum requires `State<S>` to be `Clone`.
#[derive(Clone)]
pub struct AppState {
    pub session_store: Arc<dyn SessionStore>,
    pub pool: Arc<ThreadPool>,
}

/// Construction parameters — the binary fills these in and hands them
/// to [`router`].
pub struct ServeConfig {
    pub session_store: Arc<dyn SessionStore>,
    pub agent_factory: BoxedAgentFactory,
}

/// Build the axum `Router` with every Phase-1 endpoint mounted. The
/// binary owns the network layer (`axum::serve` / TLS / shutdown);
/// this crate just produces the route surface.
pub fn router(config: ServeConfig) -> Router {
    let state = AppState {
        session_store: config.session_store,
        pool: Arc::new(ThreadPool::new(config.agent_factory)),
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
        .route(
            "/threads/{thread_id}/runs/stream",
            post(runs::create_and_stream_run),
        )
        .route(
            "/threads/{thread_id}/runs/{run_id}/stream",
            get(runs::replay_run),
        )
        .with_state(state)
}

// Re-export to keep the public surface tidy.
pub use runic_sessions::FileSessionStore;
