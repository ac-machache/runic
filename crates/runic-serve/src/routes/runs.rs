pub mod input;
pub mod wait;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use runic_substrate::SessionEvent;
use runic_types::{ContentBlock, Message, MessageContent};

use crate::app::AppState;
use crate::error::{ErrorBody, ServeError};
use crate::tenant::Tenant;

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct AnswerRequest {
    #[serde(deserialize_with = "answer_payload")]
    pub answer: serde_json::Value,
}

fn answer_payload<'de, D>(deserializer: D) -> Result<serde_json::Value, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(_)
        | serde_json::Value::Object(_)
        | serde_json::Value::Array(_) => Ok(value),
        other => Err(serde::de::Error::custom(format!(
            "answer must be text or a structured answer, got {other}"
        ))),
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RunStatusResponse {
    pub run_id: String,
    pub agent: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[utoipa::path(
    get,
    path = "/threads/{thread_id}/runs/{run_id}",
    tag = "runs",
    params(
        ("thread_id" = String, Path, description = "Thread id"),
        ("run_id" = String, Path, description = "Run id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "The run row", body = RunStatusResponse),
        (status = 404, description = "Unknown run", body = ErrorBody)
    )
)]
pub async fn run_status(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path((thread_id, run_id)): Path<(String, String)>,
) -> Result<Json<RunStatusResponse>, ServeError> {
    let record = state
        .store()
        .get_run(&tenant, &run_id)
        .await?
        .filter(|record| record.session_id == thread_id)
        .ok_or(ServeError::RunNotFound {
            id: run_id,
            thread: thread_id,
        })?;
    Ok(Json(RunStatusResponse {
        run_id: record.run_id,
        agent: record.agent,
        status: record.status.as_str().to_string(),
        error: record.error,
        created_at: record.created_at,
        updated_at: record.updated_at,
    }))
}

#[derive(Debug, serde::Deserialize, utoipa::IntoParams)]
pub struct RunListQuery {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

fn decode_run_cursor(cursor: &str) -> Option<(chrono::DateTime<chrono::Utc>, String)> {
    let (at, run_id) = cursor.split_once('|')?;
    let at = chrono::DateTime::parse_from_rfc3339(at).ok()?;
    Some((at.with_timezone(&chrono::Utc), run_id.to_string()))
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RunSummary {
    pub run_id: String,
    pub agent: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RunListResponse {
    pub runs: Vec<RunSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/threads/{thread_id}/runs",
    tag = "runs",
    params(
        ("thread_id" = String, Path, description = "Thread id"),
        RunListQuery,
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "Run summaries, newest first, keyset-paginated via `cursor`", body = RunListResponse)
    )
)]
pub async fn list_thread_runs(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(thread_id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<RunListQuery>,
) -> Result<Json<RunListResponse>, ServeError> {
    let limit = query.limit.unwrap_or(20).min(100);
    let before = query.cursor.as_deref().and_then(decode_run_cursor);
    let records = state
        .store()
        .list_runs(&tenant, &thread_id, limit, before)
        .await?;
    let next_cursor = (records.len() == limit)
        .then(|| {
            records
                .last()
                .map(|record| format!("{}|{}", record.created_at.to_rfc3339(), record.run_id))
        })
        .flatten();
    Ok(Json(RunListResponse {
        runs: records
            .into_iter()
            .map(|record| RunSummary {
                run_id: record.run_id,
                agent: record.agent,
                status: record.status.as_str().to_string(),
                error: record.error,
                created_at: record.created_at,
                updated_at: record.updated_at,
            })
            .collect(),
        next_cursor,
    }))
}

#[utoipa::path(
    get,
    path = "/threads/{thread_id}/runs/{run_id}/timeline",
    tag = "runs",
    params(
        ("thread_id" = String, Path, description = "Thread id"),
        ("run_id" = String, Path, description = "Run id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "The run's execution tree (turns → tools/delegations, with timing and usage)", body = serde_json::Value),
        (status = 404, description = "Unknown run", body = ErrorBody)
    )
)]
pub async fn run_timeline(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path((thread_id, run_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ServeError> {
    let events = state
        .store()
        .read_run_after(&tenant, &thread_id, &run_id, 0)
        .await?;
    let trace = runic_substrate::timeline::project(events.iter().map(|entry| &entry.event))
        .into_iter()
        .next()
        .ok_or(ServeError::RunNotFound {
            id: run_id,
            thread: thread_id,
        })?;
    Ok(Json(serde_json::to_value(trace).map_err(|error| {
        ServeError::Internal(format!("timeline serialization failed: {error}"))
    })?))
}

#[utoipa::path(
    post,
    path = "/threads/{thread_id}/asks/{ask_id}",
    tag = "runs",
    request_body = AnswerRequest,
    params(
        ("thread_id" = String, Path, description = "Thread id"),
        ("ask_id" = String, Path, description = "Ask id from the `ask_required` event"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 202, description = "Answer delivered; the parked run resumes"),
        (status = 400, description = "No pending ask for this (tenant, thread, ask_id)", body = ErrorBody)
    )
)]
pub async fn submit_answer(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path((thread_id, ask_id)): Path<(String, String)>,
    Json(body): Json<AnswerRequest>,
) -> Result<StatusCode, ServeError> {
    resolve_answer(state, tenant, thread_id, ask_id, body.answer).await
}

async fn resolve_answer(
    state: AppState,
    tenant: String,
    thread_id: String,
    ask_id: String,
    answer: serde_json::Value,
) -> Result<StatusCode, ServeError> {
    let events = state.store().read(&tenant, &thread_id).await?;

    let Some((run_id, tool_name)) = events.iter().rev().find_map(|entry| match &entry.event {
        SessionEvent::ToolDeferred {
            call_id,
            run_id,
            tool,
            ..
        } if *call_id == ask_id => Some((run_id.clone(), tool.clone())),
        _ => None,
    }) else {
        return Err(ServeError::BadRequest(format!(
            "no deferred call for ask_id '{ask_id}'"
        )));
    };

    let paired = events.iter().any(|entry| match &entry.event {
        SessionEvent::Message {
            run_id: msg_run,
            msg,
            ..
        } if *msg_run == run_id => match &msg.content {
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolUse { id, .. } if *id == ask_id)),
            _ => false,
        },
        _ => false,
    });
    if !paired {
        return Err(ServeError::BadRequest(format!(
            "deferred call '{ask_id}' has no matching tool_use in run '{run_id}'"
        )));
    }

    let event = SessionEvent::Message {
        run_id: run_id.clone(),
        msg: Message::user_with_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: ask_id,
            tool_name,
            content: answer.into(),
            is_error: false,
            provenance: Vec::new(),
        }]),
        at: chrono::Utc::now(),
    };
    if !state
        .store()
        .deliver_and_resume(&tenant, &run_id, &event)
        .await?
    {
        return Err(ServeError::BadRequest(format!(
            "run '{run_id}' is not awaiting an answer"
        )));
    }

    Ok(StatusCode::ACCEPTED)
}
