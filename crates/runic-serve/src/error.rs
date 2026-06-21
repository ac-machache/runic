//! HTTP error type — converts to a JSON `{error: "kind", message: "..."}`
//! body with an appropriate status code.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

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
        };
        let body = Json(serde_json::json!({
            "error": kind,
            "message": self.to_string(),
        }));
        (status, body).into_response()
    }
}
