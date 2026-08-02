use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};

#[cfg(feature = "docs-ui")]
use utoipa::OpenApi;

use super::config::{ServeConfig, app_state};
use super::layers::with_layers;
use super::state::AppState;
use crate::routes::{agents, artifacts, health, runs, sessions, transcribe};

pub fn bare_router(config: ServeConfig) -> Router {
    let (state, identity) = app_state(config);
    crate::auth::apply(routes(state), identity)
}

pub fn router(config: ServeConfig) -> Router {
    let declared = config.declared.clone();
    let (state, identity) = app_state(config);
    if tokio::runtime::Handle::try_current().is_ok() {
        state.completions.watch(state.pool.clone());
        crate::worker::spawn(state.clone()).detach();
        crate::routines::spawn(state.clone());
        let schedules = state.schedules().clone();
        tokio::spawn(async move {
            if let Err(error) = super::config::reconcile(&schedules, &declared).await {
                tracing::error!(%error, "could not reconcile declared routines");
            }
        });
    } else {
        tracing::warn!("router built outside a tokio runtime — the run worker is not started");
    }
    with_layers(crate::auth::apply(routes(state), identity))
}

pub(crate) fn routes(state: AppState) -> Router {
    let router = Router::new()
        .route("/healthz", get(health::healthz))
        .route("/openapi.json", get(crate::openapi::openapi_json))
        .route("/agents", get(agents::list_agents))
        .route("/agents/{name}", get(agents::agent_overview))
        .route(
            "/sessions",
            post(sessions::create_session).get(sessions::list_sessions),
        )
        .route(
            "/sessions/{session_id}",
            get(sessions::get_session)
                .patch(sessions::update_session)
                .delete(sessions::delete_session),
        )
        .route(
            "/sessions/{session_id}/children",
            get(sessions::list_session_children),
        )
        .route(
            "/sessions/{session_id}/events",
            get(sessions::session_events),
        )
        .route("/sessions/{session_id}/state", get(sessions::session_state))
        .route(
            "/sessions/{session_id}/artifacts",
            post(artifacts::upload_artifact)
                .get(artifacts::list_artifacts)
                .layer(DefaultBodyLimit::max(artifacts::MAX_ARTIFACT_BYTES)),
        )
        .route(
            "/sessions/{session_id}/artifacts/{artifact_id}",
            get(artifacts::download_artifact),
        )
        .route(
            "/transcribe",
            post(transcribe::transcribe).layer(DefaultBodyLimit::max(transcribe::MAX_AUDIO_BYTES)),
        )
        .route("/runs/wait", post(runs::loose::wait_run))
        .route("/runs/forget", post(runs::loose::forget_run))
        .route("/runs/{run_id}", get(runs::loose::run_outcome))
        .route("/sessions/{session_id}/runs", get(runs::list_session_runs))
        .route(
            "/sessions/{session_id}/runs/{run_id}/timeline",
            get(runs::run_timeline),
        )
        .route(
            "/sessions/{session_id}/runs/wait",
            post(runs::wait::wait_run),
        )
        .route(
            "/sessions/{session_id}/runs/stream",
            post(runs::stream::open_stream),
        )
        .route(
            "/sessions/{session_id}/runs/{run_id}/stream",
            get(runs::stream::resume_stream),
        )
        .route(
            "/sessions/{session_id}/runs/{run_id}",
            get(runs::run_status),
        )
        .route(
            "/sessions/{session_id}/runs/{run_id}/cancel",
            post(runs::control::cancel_run),
        )
        .route(
            "/sessions/{session_id}/runs/{run_id}/steer",
            post(runs::control::steer_run),
        )
        .route(
            "/sessions/{session_id}/runs/{run_id}/resume",
            post(runs::control::resume_run),
        )
        .with_state(state);

    // Swagger UI reads the spec from an internal path so it doesn't collide with
    // the public `GET /openapi.json` route mounted above.
    #[cfg(feature = "docs-ui")]
    let router = router.merge(
        utoipa_swagger_ui::SwaggerUi::new("/docs")
            .url("/docs/openapi.json", crate::openapi::ApiDoc::openapi()),
    );

    router
}
