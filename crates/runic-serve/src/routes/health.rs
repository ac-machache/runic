//! `GET /healthz` — liveness probe. Returns 200 with a short JSON body
//! so callers can confirm runic-serve is up.

use axum::Json;
use serde_json::{json, Value};

pub async fn healthz() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "runic-serve",
    }))
}
