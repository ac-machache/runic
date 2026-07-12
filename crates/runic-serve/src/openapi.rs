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
        description = "HTTP + SSE surface over a runic Agent. Threads are sessions; \
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
        threads::get_thread,
        threads::update_thread,
        threads::delete_thread,
        threads::thread_events,
        threads::thread_state,
        artifacts::upload_artifact,
        artifacts::list_artifacts,
        artifacts::download_artifact,
        transcribe::transcribe,
        runs::background_run,
        runs::run_status,
        runs::create_and_stream_run,
        runs::wait_run,
        runs::cancel_run,
        runs::steer_run,
        runs::replay_run,
        runs::submit_answer,
        runs::submit_answer_legacy,
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
        runs::RunMessageRequest,
        runs::WaitRunResponse,
        runs::BackgroundRunResponse,
        runs::RunStatusResponse,
        runs::AnswerRequest,
        runs::SteerRequest,
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
