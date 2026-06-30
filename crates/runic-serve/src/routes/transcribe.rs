//! `POST /transcribe` — audio bytes in, text out. A preprocessing step: the
//! audio never enters a thread or the event log; the client sends the returned
//! text as an ordinary message.

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, header};
use serde::Serialize;

use crate::app::AppState;
use crate::error::ServeError;
use crate::tenant::Tenant;

/// Upload ceiling for audio (also enforced as a `DefaultBodyLimit`).
pub const MAX_AUDIO_BYTES: usize = 100 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub struct TranscriptResponse {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

pub async fn transcribe(
    State(state): State<AppState>,
    Tenant(_tenant): Tenant,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<TranscriptResponse>, ServeError> {
    let Some(stt) = &state.transcriber else {
        return Err(ServeError::NotConfigured(
            "transcription is not enabled".into(),
        ));
    };
    if body.is_empty() {
        return Err(ServeError::BadRequest("empty audio body".into()));
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(str::trim)
        .unwrap_or_default();
    if !content_type
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("audio/"))
    {
        return Err(ServeError::BadRequest(format!(
            "expected audio content-type, got {content_type:?}"
        )));
    }
    let filename = clean_filename(
        headers
            .get("x-runic-filename")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("audio"),
    );

    let t = match stt.transcribe(&body, &filename).await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, filename = %filename, bytes = body.len(), "transcription failed");
            return Err(ServeError::Upstream(e.to_string()));
        }
    };
    tracing::info!(
        filename,
        bytes = body.len(),
        chars = t.text.len(),
        "transcribed audio"
    );

    Ok(Json(TranscriptResponse {
        text: t.text,
        language: t.language,
    }))
}

fn clean_filename(raw: &str) -> String {
    let name = raw
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "." && *s != "..")
        .unwrap_or("audio");
    let cleaned = name
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect::<String>();
    if cleaned.is_empty() {
        "audio".to_string()
    } else {
        cleaned
    }
}
