//! OpenAPI spec aggregation and the `GET /openapi.json` handler.

use axum::Json;
use axum::extract::OriginalUri;
use utoipa::OpenApi;
use utoipa::openapi::{OpenApi as OpenApiSpec, Server};

use crate::error::ErrorBody;
use crate::routes::{agents, artifacts, health, runs, threads, transcribe};
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
        threads::create_thread,
        threads::list_threads,
        threads::list_thread_children,
        threads::get_thread,
        threads::update_thread,
        threads::delete_thread,
        threads::thread_events,
        threads::thread_state,
        artifacts::upload_artifact,
        artifacts::list_artifacts,
        artifacts::download_artifact,
        transcribe::transcribe,
        runs::run_status,
        runs::list_thread_runs,
        runs::run_timeline,
        runs::wait::wait_run,
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
        threads::Thread,
        threads::ThreadSummary,
        threads::ThreadList,
        threads::CreateThreadRequest,
        threads::UpdateThreadRequest,
        threads::ThreadEventsResponse,
        threads::StoredEventEnvelope,
        threads::ThreadStateResponse,
        threads::ThreadStatsView,
        artifacts::UploadedArtifact,
        artifacts::ArtifactMeta,
        transcribe::TranscriptResponse,
        runs::input::RunMessageRequest,
        runs::wait::WaitRunResponse,
        runs::wait::Awaiting,
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
        (name = "threads", description = "Thread (session) lifecycle and history"),
        (name = "artifacts", description = "Per-thread blob upload and listing"),
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
