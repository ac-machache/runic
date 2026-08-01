use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::HeaderName;
use axum::routing::{get, post};
use runic_substrate::{ArtifactStore, Blobs, SessionStore, Sessions};
use runic_transcriber::SpeechToText;
use tower::ServiceBuilder;
use tower_http::cors::CorsLayer;
use tower_http::request_id::{
    MakeRequestUuid, PropagateRequestIdLayer, RequestId, SetRequestIdLayer,
};
use tower_http::trace::TraceLayer;

const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

#[cfg(feature = "docs-ui")]
use utoipa::OpenApi;

use sqlx::PgPool;

use crate::hosts::{AgentRegistry, HostedAgents};
use crate::routes::{agents, artifacts, health, runs, sessions, transcribe};
use crate::store::Runs;

#[derive(Clone)]
pub struct AppState {
    pub sessions: Sessions,
    pub blobs: Blobs,
    pub pool: PgPool,
    pub runs: Runs,
    pub transcriber: Option<Arc<dyn SpeechToText>>,
    pub agents: Arc<AgentRegistry>,
    pub completions: crate::completion::Completions,
    pub events: Arc<dyn crate::stream::RunEvents>,
    pub hooks: Arc<crate::hook::HookRegistry>,
}

impl AppState {
    pub fn store(&self) -> Arc<dyn SessionStore> {
        self.sessions.store()
    }

    pub fn runs(&self) -> &Runs {
        &self.runs
    }

    pub fn artifacts(&self) -> Arc<dyn ArtifactStore> {
        self.blobs.store()
    }

    pub fn session(&self, tenant: &str, session_id: &str) -> runic::Session {
        self.scratch(tenant, session_id)
            .store(self.sessions.clone())
    }

    pub fn scratch(&self, tenant: &str, session_id: &str) -> runic::Session {
        runic::session((tenant, session_id)).artifacts(self.blobs.clone())
    }
}

pub struct ServeConfig {
    pub sessions: Sessions,
    pub blobs: Blobs,
    pub pool: PgPool,
    pub transcriber: Option<Arc<dyn SpeechToText>>,
    pub agents: HashMap<String, HostedAgents>,
    pub identity: Option<Arc<dyn crate::auth::IdentityResolver>>,
    pub events: Option<Arc<dyn crate::stream::RunEvents>>,
    pub hooks: crate::hook::HookRegistry,
}

impl ServeConfig {
    pub fn new(sessions: Sessions, blobs: Blobs, pool: PgPool) -> Self {
        Self {
            sessions,
            blobs,
            pool,
            transcriber: None,
            agents: HashMap::new(),
            identity: None,
            events: None,
            hooks: crate::hook::HookRegistry::default(),
        }
    }

    pub fn events(mut self, events: Arc<dyn crate::stream::RunEvents>) -> Self {
        self.events = Some(events);
        self
    }

    pub fn hook(mut self, name: impl Into<String>, hook: impl crate::hook::RunHook) -> Self {
        self.hooks.insert(name, Arc::new(hook));
        self
    }

    pub fn agent(mut self, name: impl Into<String>, agent: impl Into<HostedAgents>) -> Self {
        self.agents.insert(name.into(), agent.into());
        self
    }

    pub async fn def(mut self, def: impl runic::AgentDef + 'static) -> anyhow::Result<Self> {
        let name = def.name().to_string();
        let description = def.description().map(str::to_string);
        let agent = def.build_agent().await?;
        self.agents
            .insert(name, HostedAgents { agent, description });
        Ok(self)
    }

    pub fn transcriber(mut self, transcriber: Option<Arc<dyn SpeechToText>>) -> Self {
        self.transcriber = transcriber;
        self
    }

    pub fn identity(mut self, identity: Arc<dyn crate::auth::IdentityResolver>) -> Self {
        self.identity = Some(identity);
        self
    }
}

pub fn single_agent(
    name: impl Into<String>,
    agent: impl Into<HostedAgents>,
) -> HashMap<String, HostedAgents> {
    HashMap::from([(name.into(), agent.into())])
}

fn app_state(config: ServeConfig) -> (AppState, Option<Arc<dyn crate::auth::IdentityResolver>>) {
    let state = AppState {
        sessions: config.sessions,
        blobs: config.blobs,
        runs: Runs::new(config.pool.clone()),
        pool: config.pool,
        transcriber: config.transcriber,
        agents: Arc::new(AgentRegistry::new(config.agents)),
        completions: crate::completion::Completions::new(),
        events: config
            .events
            .unwrap_or_else(|| crate::stream::LocalEvents::new()),
        hooks: Arc::new(config.hooks),
    };
    (state, config.identity)
}

pub fn bare_router(config: ServeConfig) -> Router {
    let (state, identity) = app_state(config);
    crate::auth::apply(routes(state), identity)
}

fn routes(state: AppState) -> Router {
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

pub fn router(config: ServeConfig) -> Router {
    let (state, identity) = app_state(config);
    if tokio::runtime::Handle::try_current().is_ok() {
        state.completions.watch(state.pool.clone());
        crate::worker::spawn(state.clone()).detach();
    } else {
        tracing::warn!("router built outside a tokio runtime — the run worker is not started");
    }
    with_layers(crate::auth::apply(routes(state), identity))
}

fn with_layers(router: Router) -> Router {
    router.layer(CorsLayer::permissive()).layer(
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
) -> anyhow::Result<()> {
    crate::store::migrate(&config.pool).await?;

    let (state, identity) = app_state(config);
    state.completions.watch(state.pool.clone());
    let worker = crate::worker::spawn(state.clone());
    let app = with_layers(crate::auth::apply(routes(state), identity));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(addr = %listener.local_addr()?, "runic-serve listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    worker.shutdown(SHUTDOWN_GRACE).await;
    Ok(())
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
