//! OpenAPI spec aggregation and the `GET /openapi.json` handler.

use axum::Json;
use axum::extract::OriginalUri;
use utoipa::OpenApi;
use utoipa::openapi::{OpenApi as OpenApiSpec, Server};

use crate::error::ErrorBody;
use crate::routes::{agents, artifacts, health, runs, sessions, transcribe};
use crate::wire::WireEvent;

#[derive(OpenApi)]
#[openapi(
    info(
        title = "runic-serve",
        description = "HTTP + SSE surface over a runic Runner. Threads are sessions; \
            runs are single agent invocations streamed as Server-Sent Events. \
            Every request carries an optional `X-Runic-Tenant` header (defaults to \
            `default`); every error shares the `ErrorBody` shape."
    ),
    paths(
        health::healthz,
        agents::list_agents,
        agents::agent_overview,
        sessions::create_session,
        sessions::list_sessions,
        sessions::list_session_children,
        sessions::get_session,
        sessions::update_session,
        sessions::delete_session,
        sessions::session_events,
        sessions::session_state,
        artifacts::upload_artifact,
        artifacts::list_artifacts,
        artifacts::download_artifact,
        transcribe::transcribe,
        runs::run_status,
        runs::list_session_runs,
        runs::run_timeline,
        runs::wait::wait_run,
        runs::loose::wait_run,
        runs::loose::forget_run,
        runs::loose::run_outcome,
        runs::stream::open_stream,
        runs::stream::resume_stream,
        runs::control::cancel_run,
        runs::control::steer_run,
        runs::control::resume_run,
    ),
    components(schemas(
        health::HealthResponse,
        agents::AgentInfo,
        agents::AgentList,
        agents::AgentOverview,
        agents::AbilityOverview,
        agents::ToolOverview,
        agents::SkillOverview,
        agents::SubagentOverview,
        sessions::SessionKey,
        sessions::SessionSummary,
        sessions::SessionList,
        sessions::CreateSessionRequest,
        sessions::UpdateSessionRequest,
        sessions::SessionEventsResponse,
        sessions::StoredEventEnvelope,
        sessions::SessionStateResponse,
        sessions::ThreadStatsView,
        artifacts::UploadedArtifact,
        artifacts::ArtifactMeta,
        transcribe::TranscriptResponse,
        runs::input::RunMessageRequest,
        runs::wait::WaitRunResponse,
        runs::wait::Awaiting,
        runs::loose::QueuedRun,
        runs::loose::LooseRunResponse,
        runs::RunStatusResponse,
        runs::RunSummary,
        runs::RunListResponse,
        runs::control::SteerRequest,
        runs::control::ResumeRequest,
        WireEvent,
        ErrorBody,
    )),
    tags(
        (name = "health", description = "Liveness"),
        (name = "agents", description = "The named agents this server hosts"),
        (name = "sessions", description = "SessionKey (session) lifecycle and history"),
        (name = "artifacts", description = "Per-session blob upload and listing"),
        (name = "runs", description = "Streaming agent runs, replay, and HITL answers"),
        (name = "transcription", description = "Audio-to-text preprocessing"),
    )
)]
pub struct ApiDoc;

pub async fn openapi_json(OriginalUri(uri): OriginalUri) -> Json<OpenApiSpec> {
    let mut spec = ApiDoc::openapi();
    if let Some(base) = uri.path().strip_suffix("/openapi.json")
        && !base.is_empty()
    {
        spec.servers = Some(vec![Server::new(base)]);
    }
    Json(spec)
}
