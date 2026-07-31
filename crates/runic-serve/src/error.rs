//! HTTP error type — converts to a JSON `{error: "kind", message: "..."}`
//! body with an appropriate status code.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

/// Unified error body returned by every endpoint on failure.
#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorBody {
    /// Machine-readable kind: one of `not_found`, `bad_request`, `store`,
    /// `agent`, `internal`, `upstream`, `not_configured`.
    #[schema(example = "bad_request")]
    pub error: String,
    /// Human-readable detail.
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("thread {id:?} not found")]
    ThreadNotFound { id: String },

    #[error("run {id:?} not found on thread {thread:?}")]
    RunNotFound { id: String, thread: String },

    #[error("no run in flight on thread {thread_id:?}")]
    NoRunInFlight { thread_id: String },

    #[error("a run is active on thread {thread_id:?}; cancel it before deleting")]
    ThreadBusy { thread_id: String },

    #[error("agent {name:?} not found")]
    AgentNotFound { name: String },

    #[error("artifact {id:?} not found on thread {thread:?}")]
    ArtifactNotFound { id: String, thread: String },

    #[error("persistence degraded on thread {thread:?} ({backlog} events unflushed)")]
    PersistenceDegraded { thread: String, backlog: u64 },

    #[error("this instance is at its concurrent run limit ({active} active)")]
    TooBusy { active: usize },

    #[error("run {run_id:?} did not finish within the wait window")]
    Timeout { run_id: String },

    #[error("session store error: {0}")]
    Store(String),

    #[error("agent error: {0}")]
    Runner(String),

    #[error("invalid request: {0}")]
    BadRequest(String),

    #[error("internal: {0}")]
    Internal(String),

    #[error("upstream error: {0}")]
    Upstream(String),

    #[error("not configured: {0}")]
    NotConfigured(String),
}

impl From<runic_substrate::Error> for ServeError {
    fn from(err: runic_substrate::Error) -> Self {
        Self::Store(err.to_string())
    }
}

impl IntoResponse for ServeError {
    fn into_response(self) -> Response {
        let (status, kind) = match &self {
            Self::ThreadNotFound { .. }
            | Self::RunNotFound { .. }
            | Self::AgentNotFound { .. }
            | Self::ArtifactNotFound { .. } => (StatusCode::NOT_FOUND, "not_found"),
            Self::NoRunInFlight { .. } | Self::ThreadBusy { .. } => {
                (StatusCode::CONFLICT, "conflict")
            }
            Self::PersistenceDegraded { .. } => (StatusCode::SERVICE_UNAVAILABLE, "degraded"),
            Self::TooBusy { .. } => (StatusCode::TOO_MANY_REQUESTS, "too_busy"),
            Self::Timeout { .. } => (StatusCode::GATEWAY_TIMEOUT, "timeout"),
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            Self::Store(_) => (StatusCode::INTERNAL_SERVER_ERROR, "store"),
            Self::Runner(_) => (StatusCode::INTERNAL_SERVER_ERROR, "agent"),
            Self::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
            Self::Upstream(_) => (StatusCode::BAD_GATEWAY, "upstream"),
            Self::NotConfigured(_) => (StatusCode::NOT_IMPLEMENTED, "not_configured"),
        };

        match &self {
            Self::Store(_) | Self::Runner(_) | Self::Internal(_) => {
                tracing::error!(kind, error = %self, "request failed")
            }
            Self::Upstream(_)
            | Self::NotConfigured(_)
            | Self::PersistenceDegraded { .. }
            | Self::Timeout { .. }
            | Self::TooBusy { .. } => {
                tracing::warn!(kind, error = %self, "request failed")
            }
            Self::ThreadNotFound { .. }
            | Self::RunNotFound { .. }
            | Self::AgentNotFound { .. }
            | Self::ArtifactNotFound { .. }
            | Self::NoRunInFlight { .. }
            | Self::ThreadBusy { .. }
            | Self::BadRequest(_) => {}
        }

        let body = Json(ErrorBody {
            error: kind.to_string(),
            message: self.to_string(),
        });
        (status, body).into_response()
    }
}
