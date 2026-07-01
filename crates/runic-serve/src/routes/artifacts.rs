//! Artifact upload/list — bytes live in the [`runic_substrate::ArtifactStore`],
//! keyed by `(tenant, thread)`. A message references one by id (an
//! `artifact_ref` content block) instead of carrying inline base64, so the
//! event log stays lean.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use chrono::{DateTime, Utc};
use runic_substrate::{Artifact, ArtifactSource};
use serde::Serialize;
use utoipa::ToSchema;

use crate::app::AppState;
use crate::error::{ErrorBody, ServeError};
use crate::tenant::Tenant;

/// Upload ceiling — also enforced as a `DefaultBodyLimit` on the route so the
/// body is rejected before it's fully buffered.
pub const MAX_ARTIFACT_BYTES: usize = 25 * 1024 * 1024;

async fn require_thread(state: &AppState, tenant: &str, thread_id: &str) -> Result<(), ServeError> {
    if state
        .session_store
        .session_meta(tenant, thread_id)
        .await?
        .is_none()
    {
        return Err(ServeError::ThreadNotFound {
            id: thread_id.to_string(),
        });
    }
    Ok(())
}

/// Result of a successful upload.
#[derive(Debug, Serialize, ToSchema)]
pub struct UploadedArtifact {
    pub id: String,
    pub mime_type: String,
    pub size: u64,
    /// Echo of the `X-Runic-Filename` header, if one was sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

/// Stored-artifact metadata as returned by the list endpoint.
#[derive(Debug, Serialize, ToSchema)]
pub struct ArtifactMeta {
    pub id: String,
    pub mime_type: String,
    pub size: u64,
    /// Provenance: `user_upload`, `tool_output`, `model_output`, or `other`.
    pub source: String,
    pub created_at: DateTime<Utc>,
}

impl From<Artifact> for ArtifactMeta {
    fn from(a: Artifact) -> Self {
        Self {
            id: a.id,
            mime_type: a.mime_type,
            size: a.size,
            source: a.source.as_str().to_string(),
            created_at: a.created_at,
        }
    }
}

/// `POST /threads/:id/artifacts` — store the raw request body. `Content-Type`
/// gives the media type; `x-runic-filename` (optional) is echoed back.
#[utoipa::path(
    post,
    path = "/threads/{thread_id}/artifacts",
    tag = "artifacts",
    request_body(content = String, description = "Raw artifact bytes (max 25 MiB)", content_type = "application/octet-stream"),
    params(
        ("thread_id" = String, Path, description = "Thread id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`"),
        ("Content-Type" = Option<String>, Header, description = "Media type; defaults to application/octet-stream"),
        ("X-Runic-Filename" = Option<String>, Header, description = "Optional filename, echoed back")
    ),
    responses(
        (status = 201, description = "Stored", body = UploadedArtifact),
        (status = 400, description = "Empty or oversized body", body = ErrorBody),
        (status = 404, description = "Unknown thread", body = ErrorBody)
    )
)]
pub async fn upload_artifact(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(thread_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<UploadedArtifact>), ServeError> {
    if body.is_empty() {
        return Err(ServeError::BadRequest("empty upload body".into()));
    }
    if body.len() > MAX_ARTIFACT_BYTES {
        return Err(ServeError::BadRequest("upload exceeds size limit".into()));
    }
    require_thread(&state, &tenant, &thread_id).await?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("application/octet-stream");
    let filename = headers
        .get("x-runic-filename")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let art = state
        .artifact_store
        .put(
            &tenant,
            &thread_id,
            content_type,
            ArtifactSource::UserUpload,
            &body,
        )
        .await?;

    tracing::info!(
        %tenant,
        %thread_id,
        artifact_id = %art.id,
        mime_type = %art.mime_type,
        bytes = art.size,
        "artifact uploaded"
    );

    Ok((
        StatusCode::CREATED,
        Json(UploadedArtifact {
            id: art.id,
            mime_type: art.mime_type,
            size: art.size,
            filename,
        }),
    ))
}

/// `GET /threads/:id/artifacts` — metadata for every artifact in the thread.
#[utoipa::path(
    get,
    path = "/threads/{thread_id}/artifacts",
    tag = "artifacts",
    params(
        ("thread_id" = String, Path, description = "Thread id"),
        ("X-Runic-Tenant" = Option<String>, Header, description = "Tenant; defaults to `default`")
    ),
    responses(
        (status = 200, description = "Artifact metadata", body = [ArtifactMeta]),
        (status = 404, description = "Unknown thread", body = ErrorBody)
    )
)]
pub async fn list_artifacts(
    State(state): State<AppState>,
    Tenant(tenant): Tenant,
    Path(thread_id): Path<String>,
) -> Result<Json<Vec<ArtifactMeta>>, ServeError> {
    require_thread(&state, &tenant, &thread_id).await?;
    let arts = state.artifact_store.list(&tenant, &thread_id).await?;
    Ok(Json(arts.into_iter().map(ArtifactMeta::from).collect()))
}
