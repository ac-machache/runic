//! OpenAPI spec aggregation and the `GET /openapi.json` handler.

use axum::Json;
use utoipa::OpenApi;
use utoipa::openapi::OpenApi as OpenApiSpec;

use crate::error::ErrorBody;
use crate::routes::{artifacts, health, runs, threads, transcribe};
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
        threads::create_thread,
        threads::list_threads,
        threads::get_thread,
        threads::update_thread,
        threads::delete_thread,
        threads::thread_events,
        threads::thread_state,
        artifacts::upload_artifact,
        artifacts::list_artifacts,
        transcribe::transcribe,
        runs::create_and_stream_run,
        runs::cancel_run,
        runs::replay_run,
        runs::submit_answer,
        runs::submit_answer_legacy,
    ),
    components(schemas(
        health::HealthResponse,
        threads::Thread,
        threads::ThreadSummary,
        threads::ThreadList,
        threads::CreateThreadRequest,
        threads::UpdateThreadRequest,
        threads::ThreadEventsResponse,
        threads::StoredEventEnvelope,
        threads::ThreadStateResponse,
        artifacts::UploadedArtifact,
        artifacts::ArtifactMeta,
        transcribe::TranscriptResponse,
        runs::RunMessageRequest,
        runs::AnswerRequest,
        WireEvent,
        ErrorBody,
    )),
    tags(
        (name = "health", description = "Liveness"),
        (name = "threads", description = "Thread (session) lifecycle and history"),
        (name = "artifacts", description = "Per-thread blob upload and listing"),
        (name = "runs", description = "Streaming agent runs, replay, and HITL answers"),
        (name = "transcription", description = "Audio-to-text preprocessing"),
    )
)]
pub struct ApiDoc;

pub async fn openapi_json() -> Json<OpenApiSpec> {
    Json(ApiDoc::openapi())
}
