//! `GET /healthz` — liveness probe.

use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

/// Liveness response.
#[derive(Debug, Serialize, ToSchema)]
pub struct HealthResponse {
    #[schema(example = "ok")]
    pub status: String,
    #[schema(example = "runic-serve")]
    pub service: String,
}

#[utoipa::path(
    get,
    path = "/healthz",
    tag = "health",
    responses((status = 200, description = "Service is up", body = HealthResponse))
)]
pub async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".into(),
        service: "runic-serve".into(),
    })
}
