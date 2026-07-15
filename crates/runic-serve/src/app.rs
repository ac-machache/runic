//! `AppState` and the top-level `router()` factory.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::HeaderName;
use axum::routing::{get, post};
use runic_substrate::{ArtifactStore, SessionStore};
use runic_transcriber::SpeechToText;
use tower::ServiceBuilder;
use tower_http::cors::CorsLayer;
use tower_http::request_id::{
    MakeRequestUuid, PropagateRequestIdLayer, RequestId, SetRequestIdLayer,
};
use tower_http::trace::TraceLayer;

const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

#[cfg(feature = "docs-ui")]
use utoipa::OpenApi;

use crate::executor::{WorkerConfig, spawn_run_workers};
use crate::factory::BoxedAgentFactory;
use crate::registry::{AgentRegistry, RunLimits, RunRegistry, spawn_lease_reaper};
use crate::routes::{agents, artifacts, health, runs, threads, transcribe};

/// Everything every handler needs. Cheap to clone (all internal data is
/// `Arc`-wrapped); axum requires `State<S>` to be `Clone`.
#[derive(Clone)]
pub struct AppState {
    pub session_store: Arc<dyn SessionStore>,
    pub artifact_store: Arc<dyn ArtifactStore>,
    /// Optional speech-to-text backend powering `POST /transcribe`.
    pub transcriber: Option<Arc<dyn SpeechToText>>,
    pub agents: Arc<AgentRegistry>,
    pub runs: Arc<RunRegistry>,
    pub queue_runs: bool,
    pub nudge: Option<Arc<dyn crate::broker::QueueNudge>>,
}

/// Construction parameters — the binary fills these in and hands them to
/// [`router`].
pub struct ServeConfig {
    pub session_store: Arc<dyn SessionStore>,
    pub artifact_store: Arc<dyn ArtifactStore>,
    /// Optional speech-to-text backend; `None` disables `POST /transcribe`.
    pub transcriber: Option<Arc<dyn SpeechToText>>,
    /// Named agents; run requests pick one via `"agent"` (default: `default`).
    pub agents: HashMap<String, BoxedAgentFactory>,
    pub limits: RunLimits,
    /// `Some` switches background runs to queued execution: `POST .../runs`
    /// only records the run; polling workers (this instance's and any other
    /// instance's) claim and execute. `None` (default) executes in-process.
    pub workers: Option<WorkerConfig>,
    /// Cross-instance live event fan-out (e.g. [`crate::RedisBroker`]). `None`
    /// (default) keeps live SSE attach instance-local; replay always works.
    pub broker: Option<Arc<dyn crate::broker::EventBroker>>,
    pub nudge: Option<Arc<dyn crate::broker::QueueNudge>>,
    pub identity: Option<Arc<dyn crate::auth::IdentityResolver>>,
}

impl ServeConfig {
    pub fn new(
        session_store: Arc<dyn SessionStore>,
        artifact_store: Arc<dyn ArtifactStore>,
        agents: HashMap<String, BoxedAgentFactory>,
    ) -> Self {
        Self {
            session_store,
            artifact_store,
            transcriber: None,
            agents,
            limits: RunLimits::default(),
            workers: None,
            broker: None,
            nudge: None,
            identity: None,
        }
    }

    pub fn transcriber(mut self, transcriber: Option<Arc<dyn SpeechToText>>) -> Self {
        self.transcriber = transcriber;
        self
    }

    pub fn limits(mut self, limits: RunLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn workers(mut self, workers: WorkerConfig) -> Self {
        self.workers = Some(workers);
        self
    }

    pub fn broker(mut self, broker: Arc<dyn crate::broker::EventBroker>) -> Self {
        self.broker = Some(broker);
        self
    }

    pub fn nudge(mut self, nudge: Arc<dyn crate::broker::QueueNudge>) -> Self {
        self.nudge = Some(nudge);
        self
    }

    pub fn identity(mut self, identity: Arc<dyn crate::auth::IdentityResolver>) -> Self {
        self.identity = Some(identity);
        self
    }
}

pub fn single_agent(
    name: impl Into<String>,
    factory: BoxedAgentFactory,
) -> HashMap<String, BoxedAgentFactory> {
    HashMap::from([(name.into(), factory)])
}

fn app_state(
    config: ServeConfig,
) -> (
    AppState,
    Option<WorkerConfig>,
    Option<Arc<dyn crate::auth::IdentityResolver>>,
) {
    let mut registry = RunRegistry::with_limits(config.limits);
    if let Some(broker) = config.broker {
        registry = registry.with_broker(broker);
    }
    let state = AppState {
        session_store: config.session_store,
        artifact_store: config.artifact_store,
        transcriber: config.transcriber,
        agents: Arc::new(AgentRegistry::new(config.agents)),
        runs: Arc::new(registry),
        queue_runs: config.workers.is_some(),
        nudge: config.nudge,
    };
    (state, config.workers, config.identity)
}

pub fn bare_router(config: ServeConfig) -> Router {
    let (state, _, identity) = app_state(config);
    crate::auth::apply(routes(state), identity)
}

fn routes(state: AppState) -> Router {
    let router = Router::new()
        .route("/healthz", get(health::healthz))
        .route("/openapi.json", get(crate::openapi::openapi_json))
        .route("/agents", get(agents::list_agents))
        .route("/agents/{name}", get(agents::agent_overview))
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
            "/threads/{thread_id}/artifacts/{artifact_id}",
            get(artifacts::download_artifact),
        )
        .route(
            "/transcribe",
            post(transcribe::transcribe).layer(DefaultBodyLimit::max(transcribe::MAX_AUDIO_BYTES)),
        )
        .route(
            "/threads/{thread_id}/runs",
            post(runs::background_run).get(runs::list_thread_runs),
        )
        .route(
            "/threads/{thread_id}/runs/{run_id}/timeline",
            get(runs::run_timeline),
        )
        .route(
            "/threads/{thread_id}/runs/stream",
            post(runs::create_and_stream_run),
        )
        .route("/threads/{thread_id}/runs/wait", post(runs::wait_run))
        .route("/threads/{thread_id}/runs/cancel", post(runs::cancel_run))
        .route("/threads/{thread_id}/runs/steer", post(runs::steer_run))
        .route("/threads/{thread_id}/runs/{run_id}", get(runs::run_status))
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

pub fn router(config: ServeConfig) -> Router {
    let reap_every = config.limits.reap_every;
    let (state, workers, identity) = app_state(config);
    if tokio::runtime::Handle::try_current().is_ok() {
        spawn_lease_reaper(state.session_store.clone(), reap_every);
        if let Some(worker_config) = workers {
            spawn_run_workers(
                state.session_store.clone(),
                state.agents.clone(),
                state.runs.clone(),
                worker_config,
                state.nudge.clone(),
            );
        }
    } else {
        tracing::warn!("router built outside a tokio runtime — background loops not started");
    }
    crate::auth::apply(routes(state), identity)
        .layer(CorsLayer::permissive())
        .layer(
            ServiceBuilder::new()
                .layer(SetRequestIdLayer::new(REQUEST_ID_HEADER, MakeRequestUuid))
                .layer(
                    TraceLayer::new_for_http()
                        .make_span_with(|request: &axum::http::Request<axum::body::Body>| {
                            let request_id = request
                                .extensions()
                                .get::<RequestId>()
                                .and_then(|id| id.header_value().to_str().ok())
                                .unwrap_or("-")
                                .to_string();
                            tracing::info_span!(
                                "http_request",
                                method = %request.method(),
                                path = %request.uri().path(),
                                request_id = %request_id,
                            )
                        })
                        .on_response(
                            |response: &axum::response::Response,
                             latency: Duration,
                             _span: &tracing::Span| {
                                tracing::info!(
                                    status = %response.status().as_u16(),
                                    latency_ms = %latency.as_millis(),
                                    "request completed"
                                );
                            },
                        ),
                )
                .layer(PropagateRequestIdLayer::new(REQUEST_ID_HEADER)),
        )
}

pub async fn serve(
    config: ServeConfig,
    addr: impl tokio::net::ToSocketAddrs,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(addr = %listener.local_addr()?, "runic-serve listening");
    axum::serve(listener, router(config))
        .with_graceful_shutdown(shutdown_signal())
        .await
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install ctrl-c handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
