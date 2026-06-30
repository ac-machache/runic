//! Mistral Voxtral chat-with-audio: upload the file → get a signed URL → ask the
//! model to transcribe it. Uploading sidesteps base64 size limits, and the chat
//! endpoint supports Voxtral Small/Mini (and *understands* the audio, not just
//! transcribes).

use async_trait::async_trait;
use serde_json::Value;

use crate::{SpeechToText, TranscribeError, Transcript};

const DEFAULT_BASE_URL: &str = "https://api.mistral.ai";
const DEFAULT_MODEL: &str = "voxtral-mini-latest";
const DEFAULT_PROMPT: &str = "Transcribe this audio exactly as spoken. Do not add, \
     omit, summarize, or translate anything. Keep the original language.";

pub struct MistralTranscriber {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
    prompt: String,
}

impl MistralTranscriber {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            model: DEFAULT_MODEL.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            prompt: DEFAULT_PROMPT.to_string(),
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

    pub fn with_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = prompt.into();
        self
    }

    /// Upload the audio (`purpose=audio`) and return the file id.
    async fn upload(&self, audio: &[u8], filename: &str) -> Result<String, TranscribeError> {
        let part = reqwest::multipart::Part::bytes(audio.to_vec()).file_name(filename.to_string());
        let form = reqwest::multipart::Form::new()
            .text("purpose", "audio")
            .part("file", part);
        let v = self
            .send_json(
                self.client
                    .post(format!("{}/v1/files", self.base_url))
                    .multipart(form),
            )
            .await?;
        let id = v
            .get("id")
            .and_then(|i| i.as_str())
            .map(str::to_string)
            .ok_or_else(|| TranscribeError::Parse("upload response missing `id`".into()))?;
        tracing::info!(file_id = %id, bytes = audio.len(), "audio uploaded to Mistral");
        Ok(id)
    }

    /// Get a temporary signed URL for an uploaded file. Mistral can 404 the file
    /// briefly right after upload (read-after-write lag), so retry on 404.
    async fn signed_url(&self, file_id: &str) -> Result<String, TranscribeError> {
        let url = format!("{}/v1/files/{file_id}/url?expiry=24", self.base_url);
        let mut last = String::new();
        for attempt in 1..=5 {
            let resp = self
                .client
                .get(&url)
                .bearer_auth(&self.api_key)
                .send()
                .await
                .map_err(|e| TranscribeError::Http(format!("signed-url request: {e}")))?;
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            if status.is_success() {
                let v: Value = serde_json::from_str(&body).map_err(|e| {
                    TranscribeError::Parse(format!("signed-url body ({e}): {body}"))
                })?;
                return v
                    .get("url")
                    .and_then(|u| u.as_str())
                    .map(str::to_string)
                    .ok_or_else(|| {
                        TranscribeError::Parse(format!("signed-url missing `url`: {body}"))
                    });
            }
            last = format!("{status}: {body}");
            if status == reqwest::StatusCode::NOT_FOUND && attempt < 5 {
                tracing::warn!(
                    file_id,
                    attempt,
                    "signed-url 404 — file not queryable yet, retrying"
                );
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                continue;
            }
            break;
        }
        Err(TranscribeError::Http(format!(
            "signed-url (file {file_id}): {last}"
        )))
    }

    async fn send_json(&self, req: reqwest::RequestBuilder) -> Result<Value, TranscribeError> {
        let resp = req
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| TranscribeError::Http(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(TranscribeError::Http(format!("{status}: {body}")));
        }
        resp.json()
            .await
            .map_err(|e| TranscribeError::Parse(e.to_string()))
    }
}

#[async_trait]
impl SpeechToText for MistralTranscriber {
    async fn transcribe(
        &self,
        audio: &[u8],
        filename: &str,
    ) -> Result<Transcript, TranscribeError> {
        let file_id = self.upload(audio, filename).await?;
        let url = self.signed_url(&file_id).await?;

        let body = serde_json::json!({
            "model": self.model,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "input_audio", "input_audio": url },
                    { "type": "text", "text": self.prompt }
                ]
            }]
        });
        let v = self
            .send_json(
                self.client
                    .post(format!("{}/v1/chat/completions", self.base_url))
                    .json(&body),
            )
            .await?;

        let text = v
            .pointer("/choices/0/message/content")
            .and_then(|c| c.as_str())
            .ok_or_else(|| TranscribeError::Parse("chat response missing message content".into()))?
            .to_string();
        Ok(Transcript {
            text,
            language: None,
        })
    }
}
