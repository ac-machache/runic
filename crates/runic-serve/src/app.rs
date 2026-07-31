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

#[cfg(feature = "docs-ui")]
use utoipa::OpenApi;

use apalis_postgres::PgPool;

use crate::hosts::{AgentRegistry, HostedAgents};
use crate::queue::{RunSink, sink};
use crate::routes::{agents, artifacts, health, runs, threads, transcribe};

#[derive(Clone)]
pub struct AppState {
    pub sessions: Sessions,
    pub blobs: Blobs,
    pub pool: PgPool,
    pub transcriber: Option<Arc<dyn SpeechToText>>,
    pub agents: Arc<AgentRegistry>,
}

impl AppState {
    pub fn store(&self) -> Arc<dyn SessionStore> {
        self.sessions.store()
    }

    pub fn artifacts(&self) -> Arc<dyn ArtifactStore> {
        self.blobs.store()
    }

    pub fn queue(&self) -> RunSink {
        sink(&self.pool)
    }

    pub fn thread(&self, tenant: &str, thread_id: &str) -> runic::Session {
        runic::session((tenant, thread_id))
            .store(self.sessions.clone())
            .artifacts(self.blobs.clone())
    }
}

pub struct ServeConfig {
    pub sessions: Sessions,
    pub blobs: Blobs,
    pub pool: PgPool,
    pub transcriber: Option<Arc<dyn SpeechToText>>,
    pub agents: HashMap<String, HostedAgents>,
    pub identity: Option<Arc<dyn crate::auth::IdentityResolver>>,
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
        }
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
        pool: config.pool,
        transcriber: config.transcriber,
        agents: Arc::new(AgentRegistry::new(config.agents)),
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
            "/threads",
            post(threads::create_thread).get(threads::list_threads),
        )
        .route(
            "/threads/{thread_id}",
            get(threads::get_thread)
                .patch(threads::update_thread)
                .delete(threads::delete_thread),
        )
        .route(
            "/threads/{thread_id}/children",
            get(threads::list_thread_children),
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
        .route("/threads/{thread_id}/runs", get(runs::list_thread_runs))
        .route(
            "/threads/{thread_id}/runs/{run_id}/timeline",
            get(runs::run_timeline),
        )
        .route("/threads/{thread_id}/runs/wait", post(runs::wait::wait_run))
        .route("/threads/{thread_id}/runs/{run_id}", get(runs::run_status))
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
    let (state, identity) = app_state(config);
    if tokio::runtime::Handle::try_current().is_ok() {
        crate::worker::spawn_run_worker(state.clone(), &state.pool.clone());
    } else {
        tracing::warn!("router built outside a tokio runtime — the run worker is not started");
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
) -> anyhow::Result<()> {
    crate::queue::setup(&config.pool).await?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(addr = %listener.local_addr()?, "runic-serve listening");
    axum::serve(listener, router(config))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
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
