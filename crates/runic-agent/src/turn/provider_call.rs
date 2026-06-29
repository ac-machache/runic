//! Step: call the model.
//!
//! - **Non-streaming** (no event sink): [`crate::retry::call_with_retry`]
//!   (backoff) + the fallback-model chain.
//! - **Streaming** (event sink attached): `Provider::stream` on the primary,
//!   forwarding token/thinking deltas to the [`crate::AgentEvent`] sink. On a
//!   fallback-worthy stream failure, falls back to the non-streaming path
//!   (resilience over token-streaming for the recovery call).

use runic_provider::{CompletionRequest, CompletionResponse, ProviderError, StreamEvent};
use runic_types::{ContentBlock, MessageContent};
use tokio::sync::mpsc;

use crate::{Agent, AgentError, AgentEvent, retry};

impl Agent {
    pub(crate) async fn call_model(
        &self,
        mut request: CompletionRequest,
    ) -> Result<CompletionResponse, AgentError> {
        if let Some(resolver) = &self.media_resolver {
            resolver
                .resolve(&mut request)
                .await
                .map_err(AgentError::Media)?;
        }
        // No artifact pointer may reach a provider — fail loud, never silently
        // drop a file the model was meant to see.
        if let Some(id) = first_artifact_ref(&request) {
            return Err(AgentError::Media(format!(
                "unresolved artifact reference {id} reached the model call"
            )));
        }
        if self.events.is_some() {
            self.call_model_streaming(request).await
        } else {
            self.call_model_complete(request).await
        }
    }

    /// Stream the primary provider, forwarding deltas to the event sink.
    async fn call_model_streaming(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, AgentError> {
        let (se_tx, mut se_rx) = mpsc::channel::<StreamEvent>(64);
        let provider = self.provider.clone();
        let sink = self.events.clone();

        // Forward provider stream events → AgentEvents until the channel closes.
        let forward = async move {
            while let Some(ev) = se_rx.recv().await {
                let Some(sink) = &sink else { continue };
                let _ = match ev {
                    StreamEvent::TextDelta { text } => sink.send(AgentEvent::TextDelta(text)),
                    StreamEvent::ThinkingDelta { text } => {
                        sink.send(AgentEvent::ThinkingDelta(text))
                    }
                    _ => Ok(()),
                };
            }
        };

        let (stream_result, _) = tokio::join!(provider.stream(request.clone(), se_tx), forward);

        match stream_result {
            Ok(response) => Ok(response),
            Err(e) if is_fallback_worthy(&e) => {
                tracing::warn!(error = %e, "streaming call failed; retrying non-streamed");
                self.call_model_complete(request).await
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Non-streaming call with backoff + fallback-model chain.
    async fn call_model_complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, AgentError> {
        let primary_err =
            match retry::call_with_retry(self.provider.as_ref(), request.clone()).await {
                Ok(response) => return Ok(response),
                Err(e) => e,
            };

        if self.fallbacks.is_empty() || !is_fallback_worthy(&primary_err) {
            return Err(primary_err.into());
        }

        for fb in &self.fallbacks {
            let mut req = request.clone();
            req.model = fb.model.clone();
            match retry::call_with_retry(fb.provider.as_ref(), req).await {
                Ok(response) => {
                    tracing::warn!(
                        fallback_model = %fb.model,
                        primary_error = %primary_err,
                        "primary model call failed; served from fallback"
                    );
                    return Ok(response);
                }
                Err(e) => {
                    tracing::warn!(fallback_model = %fb.model, error = %e, "fallback failed");
                }
            }
        }

        Err(primary_err.into())
    }
}

fn first_artifact_ref(request: &CompletionRequest) -> Option<&str> {
    request.messages.iter().find_map(|m| {
        let MessageContent::Blocks(blocks) = &m.content else {
            return None;
        };
        blocks.iter().find_map(|b| match b {
            ContentBlock::ArtifactRef { id, .. } => Some(id.as_str()),
            _ => None,
        })
    })
}

/// Whether an error is worth retrying on a different model/provider.
fn is_fallback_worthy(err: &ProviderError) -> bool {
    matches!(
        err,
        ProviderError::ModelNotFound(_)
            | ProviderError::Overloaded { .. }
            | ProviderError::RateLimited { .. }
            | ProviderError::Http(_)
            | ProviderError::Api { .. }
    )
}
