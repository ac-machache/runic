use std::convert::Infallible;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};

use super::input::RunMessageRequest;
use super::wait::collect;
use crate::app::AppState;
use crate::error::{ErrorBody, ServeError};
use crate::store::{RunSpec, RunStatus};
use crate::tenant::Tenant;
use crate::wire::WireEvent;

const KEEPALIVE_EVERY: Duration = Duration::from_secs(15);

struct Ended;

fn end_id(run_id: &str) -> String {
    format!("{run_id}:end")
}

fn resume_from(headers: &HeaderMap, run_id: &str) -> Result<u64, Ended> {
    let Some(last) = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
    else {
        return Ok(0);
    };
    if last == end_id(run_id) {
        return Err(Ended);
    }
    Ok(last
        .rsplit(':')
        .next()
        .and_then(|seq| seq.parse().ok())
        .unwrap_or(0))
}

#[utoipa::path(
    post,
    path = "/sessions/{session_id}/runs/stream",
    tag = "runs",
    request_body = RunMessageRequest,
    params(
        ("session_id" = String, Path, description = "Session id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "SSE stream; the first event carries the run id", body = String),
        (status = 400, description = "Invalid body or artifact reference", body = ErrorBody),
        (status = 404, description = "Unknown agent", body = ErrorBody)
    )
)]
pub async fn open_stream(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(session_id): Path<String>,
    Json(req): Json<RunMessageRequest>,
) -> Result<Response, ServeError> {
    let agent = state.agents.resolve_agent(req.agent.as_deref())?;
    let context = req.context.clone();
    let message = req.into_message()?;

    let run_id = runic::state::new_run_id();
    let payload = serde_json::to_value(&message)
        .map_err(|error| ServeError::Internal(format!("could not encode the turn: {error}")))?;
    let spec = RunSpec::new(&tenant, &run_id, &agent)
        .session(&session_id)
        .input(payload)
        .context(context);

    state
        .runs()
        .enqueue(&spec)
        .await
        .map_err(|error| ServeError::Internal(format!("could not queue the run: {error}")))?;

    tracing::info!(%tenant, %session_id, %agent, %run_id, "stream run queued");

    let opening = WireEvent::RunStart {
        run_id: run_id.clone(),
        agent: Some(agent),
        at: None,
    };
    Ok(follow(state, tenant, run_id, 0, Some(opening)).into_response())
}

#[utoipa::path(
    get,
    path = "/sessions/{session_id}/runs/{run_id}/stream",
    tag = "runs",
    params(
        ("session_id" = String, Path, description = "Session id"),
        ("run_id" = String, Path, description = "Run id"),
        ("Last-Event-ID" = Option<String>, Header, description = "Resume cursor from a dropped stream"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "SSE stream of run events", body = String),
        (status = 204, description = "The stream already ended for this client"),
        (status = 404, description = "Unknown run", body = ErrorBody)
    )
)]
pub async fn resume_stream(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path((session_id, run_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ServeError> {
    let Ok(after) = resume_from(&headers, &run_id) else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };

    let record = state
        .runs()
        .get(&tenant, &run_id)
        .await
        .map_err(|error| ServeError::Store(error.to_string()))?
        .filter(|record| record.session_id.as_deref() == Some(session_id.as_str()))
        .ok_or(ServeError::RunNotFound {
            id: run_id.clone(),
            session: session_id.clone(),
        })?;

    if record.status.is_terminal() || record.status == RunStatus::Waiting {
        let closing = finale(&state, &tenant, &run_id).await;
        return Ok(once(closing, &run_id).into_response());
    }

    Ok(follow(state, tenant, run_id, after, None).into_response())
}

fn follow(
    state: AppState,
    tenant: String,
    run_id: String,
    after: u64,
    opening: Option<WireEvent>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>> + use<>> {
    let stream = async_stream::stream! {
        if let Some(event) = opening {
            yield Ok(sse(&event, &format!("{run_id}:0")));
        }
        let mut cursor = after;
        loop {
            let replay = state.events.since(&run_id, cursor).await;
            if replay.gap {
                yield frame(finale(&state, &tenant, &run_id).await, &run_id);
                break;
            }
            for (seq, event) in replay.events {
                cursor = seq;
                yield Ok(sse(&event, &format!("{run_id}:{seq}")));
            }
            if replay.closed {
                yield frame(finale(&state, &tenant, &run_id).await, &run_id);
                break;
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::new().interval(KEEPALIVE_EVERY))
}

async fn finale(state: &AppState, tenant: &str, run_id: &str) -> Result<WireEvent, ServeError> {
    let answer = collect(state, tenant, run_id).await?;
    Ok(WireEvent::Done {
        total_turns: Some(answer.total_turns),
        stop_reason: answer.stop_reason,
    })
}

fn sse(event: &WireEvent, id: &str) -> Event {
    Event::default()
        .id(id)
        .json_data(event)
        .unwrap_or_else(|_| Event::default().id(id).data("{}"))
}

fn frame(closing: Result<WireEvent, ServeError>, run_id: &str) -> Result<Event, Infallible> {
    let event = closing.unwrap_or(WireEvent::Done {
        total_turns: None,
        stop_reason: Some("unavailable".into()),
    });
    Ok(sse(&event, &end_id(run_id)))
}

fn once(
    closing: Result<WireEvent, ServeError>,
    run_id: &str,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>> + use<>> {
    let frame = frame(closing, run_id);
    Sse::new(async_stream::stream! { yield frame; })
}
