//! Speech-to-text preprocessing — audio bytes in, text out.
//!
//! Deliberately separate from the LLM `Provider`: audio is transcribed *before*
//! it reaches a model, so only the resulting text ever enters a conversation.

mod mistral;

use async_trait::async_trait;

pub use mistral::MistralTranscriber;

#[derive(Debug, Clone)]
pub struct Transcript {
    pub text: String,
    pub language: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum TranscribeError {
    #[error("transcription request failed: {0}")]
    Http(String),
    #[error("invalid transcription response: {0}")]
    Parse(String),
}

#[async_trait]
pub trait SpeechToText: Send + Sync {
    /// Transcribe a complete audio file. `filename` carries the extension so the
    /// backend can infer the format.
    async fn transcribe(&self, audio: &[u8], filename: &str)
    -> Result<Transcript, TranscribeError>;
}
