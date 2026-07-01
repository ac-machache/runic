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

    #[error("session store error: {0}")]
    Store(String),

    #[error("agent error: {0}")]
    Agent(String),

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
            Self::ThreadNotFound { .. } | Self::RunNotFound { .. } => {
                (StatusCode::NOT_FOUND, "not_found")
            }
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            Self::Store(_) => (StatusCode::INTERNAL_SERVER_ERROR, "store"),
            Self::Agent(_) => (StatusCode::INTERNAL_SERVER_ERROR, "agent"),
            Self::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
            Self::Upstream(_) => (StatusCode::BAD_GATEWAY, "upstream"),
            Self::NotConfigured(_) => (StatusCode::NOT_IMPLEMENTED, "not_configured"),
        };
        let body = Json(ErrorBody {
            error: kind.to_string(),
            message: self.to_string(),
        });
        (status, body).into_response()
    }
}
