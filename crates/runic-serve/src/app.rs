//! `AppState` and the top-level `router()` factory.

use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use runic_substrate::{ArtifactStore, SessionStore};
use runic_transcriber::SpeechToText;
use tower_http::cors::CorsLayer;

use crate::factory::BoxedAgentFactory;
use crate::human::HumanHub;
use crate::pool::ThreadPool;
use crate::routes::{artifacts, health, runs, threads, transcribe};

/// Everything every handler needs. Cheap to clone (all internal data is
/// `Arc`-wrapped); axum requires `State<S>` to be `Clone`.
#[derive(Clone)]
pub struct AppState {
    pub session_store: Arc<dyn SessionStore>,
    pub artifact_store: Arc<dyn ArtifactStore>,
    /// Optional speech-to-text backend powering `POST /transcribe`.
    pub transcriber: Option<Arc<dyn SpeechToText>>,
    pub pool: Arc<ThreadPool>,
    /// Bridges parked HITL asks (`ask_user`) to the answer endpoint.
    pub human_hub: Arc<HumanHub>,
}

/// Construction parameters — the binary fills these in and hands them to
/// [`router`].
pub struct ServeConfig {
    pub session_store: Arc<dyn SessionStore>,
    pub artifact_store: Arc<dyn ArtifactStore>,
    /// Optional speech-to-text backend; `None` disables `POST /transcribe`.
    pub transcriber: Option<Arc<dyn SpeechToText>>,
    pub agent_factory: BoxedAgentFactory,
    /// Shared HITL hub; the serve crate builds a per-run `HumanChannel` over it
    /// and installs it on each run's context, so an `ask_user` raised mid-run
    /// resolves via the HTTP answer endpoint.
    pub human_hub: Arc<HumanHub>,
}

/// Build the axum `Router` with every endpoint mounted. The binary owns the
/// network layer (`axum::serve` / TLS / shutdown); this crate just produces the
/// route surface.
pub fn router(config: ServeConfig) -> Router {
    let state = AppState {
        session_store: config.session_store.clone(),
        artifact_store: config.artifact_store,
        transcriber: config.transcriber,
        pool: Arc::new(ThreadPool::new(config.agent_factory, config.session_store)),
        human_hub: config.human_hub,
    };

    Router::new()
        .route("/healthz", get(health::healthz))
        .route(
            "/threads",
            post(threads::create_thread).get(threads::list_threads),
        )
        .route(
            "/threads/{thread_id}",
            get(threads::get_thread)
                .patch(threads::update_thread)
                .delete(threads::delete_thread),
        )
        .route("/threads/{thread_id}/events", get(threads::thread_events))
        .route("/threads/{thread_id}/state", get(threads::thread_state))
        .route(
            "/threads/{thread_id}/artifacts",
            post(artifacts::upload_artifact)
                .get(artifacts::list_artifacts)
                .layer(DefaultBodyLimit::max(artifacts::MAX_ARTIFACT_BYTES)),
        )
        .route(
            "/transcribe",
            post(transcribe::transcribe).layer(DefaultBodyLimit::max(transcribe::MAX_AUDIO_BYTES)),
        )
        .route(
            "/threads/{thread_id}/runs/stream",
            post(runs::create_and_stream_run),
        )
        .route(
            "/threads/{thread_id}/runs/{run_id}/stream",
            get(runs::replay_run),
        )
        .route(
            "/threads/{thread_id}/runs/{run_id}/asks/{ask_id}",
            post(runs::submit_answer_legacy),
        )
        .route(
            "/threads/{thread_id}/asks/{ask_id}",
            post(runs::submit_answer),
        )
        // Permissive CORS so a browser dev UI served from another origin can
        // drive the server.
        .layer(CorsLayer::permissive())
        .with_state(state)
}
