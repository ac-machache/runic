//! Mistral Voxtral via the dedicated `/v1/audio/transcriptions` endpoint: one
//! multipart request, no stored files, and the response carries the detected
//! language — cheaper and more accurate for pure transcription than
//! chat-with-audio.

use serde_json::Value;

use async_trait::async_trait;

use crate::{SpeechToText, TranscribeError, Transcript};

const DEFAULT_BASE_URL: &str = "https://api.mistral.ai";
const DEFAULT_MODEL: &str = "voxtral-mini-latest";
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
const MAX_ATTEMPTS: u32 = 3;

pub struct MistralTranscriber {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl MistralTranscriber {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(READ_TIMEOUT)
                .build()
                .unwrap_or_default(),
            api_key: api_key.into(),
            model: DEFAULT_MODEL.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    fn transcription_url(&self) -> String {
        format!("{}/v1/audio/transcriptions", self.base_url)
    }
}

fn should_retry(status: u16) -> bool {
    status == 429 || status >= 500
}

fn retry_delay_ms(headers: &reqwest::header::HeaderMap, attempt: u32) -> u64 {
    headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(|secs| secs.saturating_mul(1000))
        .unwrap_or(1000 * attempt as u64)
}

fn parse_transcription(value: &Value) -> Result<Transcript, TranscribeError> {
    let text = value
        .get("text")
        .and_then(|t| t.as_str())
        .ok_or_else(|| TranscribeError::Parse(format!("response missing `text`: {value}")))?
        .to_string();
    let language = value
        .get("language")
        .and_then(|l| l.as_str())
        .filter(|l| !l.is_empty())
        .map(str::to_string);
    Ok(Transcript { text, language })
}

#[async_trait]
impl SpeechToText for MistralTranscriber {
    async fn transcribe(
        &self,
        audio: &[u8],
        filename: &str,
    ) -> Result<Transcript, TranscribeError> {
        let mut last = String::new();
        for attempt in 1..=MAX_ATTEMPTS {
            let part =
                reqwest::multipart::Part::bytes(audio.to_vec()).file_name(filename.to_string());
            let form = reqwest::multipart::Form::new()
                .text("model", self.model.clone())
                .part("file", part);
            let resp = self
                .client
                .post(self.transcription_url())
                .bearer_auth(&self.api_key)
                .multipart(form)
                .send()
                .await
                .map_err(|e| TranscribeError::Http(e.to_string()))?;

            let status = resp.status().as_u16();
            if resp.status().is_success() {
                let value: Value = resp
                    .json()
                    .await
                    .map_err(|e| TranscribeError::Parse(e.to_string()))?;
                return parse_transcription(&value);
            }

            let delay = retry_delay_ms(resp.headers(), attempt);
            let body = resp.text().await.unwrap_or_default();
            last = format!("{status}: {body}");
            if should_retry(status) && attempt < MAX_ATTEMPTS {
                tracing::warn!(status, attempt, delay, "transcription failed, retrying");
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                continue;
            }
            break;
        }
        Err(TranscribeError::Http(last))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_and_language() {
        let value = serde_json::json!({
            "model": "voxtral-mini-latest",
            "text": "bonjour tout le monde",
            "language": "fr",
            "usage": {"prompt_tokens": 10}
        });
        let transcript = parse_transcription(&value).unwrap();
        assert_eq!(transcript.text, "bonjour tout le monde");
        assert_eq!(transcript.language.as_deref(), Some("fr"));
    }

    #[test]
    fn missing_language_is_none_and_missing_text_errors() {
        let value = serde_json::json!({ "text": "hi" });
        let transcript = parse_transcription(&value).unwrap();
        assert!(transcript.language.is_none());

        let value = serde_json::json!({ "language": "en" });
        assert!(matches!(
            parse_transcription(&value),
            Err(TranscribeError::Parse(_))
        ));

        let value = serde_json::json!({ "text": "hi", "language": "" });
        assert!(parse_transcription(&value).unwrap().language.is_none());
    }

    #[test]
    fn retry_policy_covers_rate_limits_and_server_errors() {
        assert!(should_retry(429));
        assert!(should_retry(500));
        assert!(should_retry(503));
        assert!(!should_retry(400));
        assert!(!should_retry(401));
        assert!(!should_retry(413));
    }

    #[test]
    fn retry_delay_honors_retry_after_and_backs_off() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "3".parse().unwrap());
        assert_eq!(retry_delay_ms(&headers, 1), 3000);
        assert_eq!(retry_delay_ms(&reqwest::header::HeaderMap::new(), 2), 2000);
    }

    #[test]
    fn url_building_tolerates_trailing_slash() {
        let t = MistralTranscriber::new("k").with_base_url("https://proxy/");
        assert_eq!(
            t.transcription_url(),
            "https://proxy/v1/audio/transcriptions"
        );
    }
}
