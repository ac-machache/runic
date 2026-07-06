use std::sync::Arc;

use async_trait::async_trait;
use axum::Json;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::ErrorBody;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub tenant: String,
}

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("missing credentials")]
    MissingCredentials,

    #[error("invalid credentials: {0}")]
    InvalidCredentials(String),

    #[error("identity resolution failed: {0}")]
    Internal(String),
}

#[async_trait]
pub trait IdentityResolver: Send + Sync {
    async fn resolve(&self, parts: &Parts) -> Result<Identity, IdentityError>;
}

impl IntoResponse for IdentityError {
    fn into_response(self) -> Response {
        let (status, kind) = match &self {
            Self::MissingCredentials | Self::InvalidCredentials(_) => {
                (StatusCode::UNAUTHORIZED, "unauthorized")
            }
            Self::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        match &self {
            Self::Internal(_) => tracing::error!(error = %self, "identity resolution failed"),
            _ => tracing::debug!(error = %self, "request rejected"),
        }
        let body = Json(ErrorBody {
            error: kind.to_string(),
            message: self.to_string(),
        });
        (status, body).into_response()
    }
}

pub(crate) fn apply(
    router: axum::Router,
    resolver: Option<Arc<dyn IdentityResolver>>,
) -> axum::Router {
    match resolver {
        Some(resolver) => router.layer(axum::middleware::from_fn(
            move |request: Request, next: Next| {
                let resolver = resolver.clone();
                async move { authenticate(resolver, request, next).await }
            },
        )),
        None => router,
    }
}

async fn authenticate(
    resolver: Arc<dyn IdentityResolver>,
    request: Request,
    next: Next,
) -> Response {
    if request.uri().path() == "/healthz" {
        return next.run(request).await;
    }
    let (mut parts, body) = request.into_parts();
    match resolver.resolve(&parts).await {
        Ok(identity) => {
            parts.extensions.insert(identity);
            next.run(Request::from_parts(parts, body)).await
        }
        Err(err) => err.into_response(),
    }
}
