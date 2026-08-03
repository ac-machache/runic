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
use tracing::Instrument;

use crate::{AgentError, AgentEvent, Runner, retry};

impl Runner {
    pub(crate) async fn call_model(
        &self,
        request: CompletionRequest,
    ) -> Result<(CompletionResponse, String), AgentError> {
        let span = tracing::info_span!(
            "provider_call",
            gen_ai.request.model = %request.model,
            streaming = self.state.observed(),
            messages = request.messages.len(),
            tools = request.tools.len(),
            gen_ai.usage.input_tokens = tracing::field::Empty,
            gen_ai.usage.output_tokens = tracing::field::Empty,
            gen_ai.response.finish_reasons = tracing::field::Empty,
            gen_ai.response.model = tracing::field::Empty,
            gen_ai.system = self.provider.name(),
            gen_ai.operation.name = "chat",
            otel.name = tracing::field::Empty,
            otel.kind = "client",
            otel.status_code = tracing::field::Empty,
        );
        span.record("otel.name", format!("chat {}", request.model));
        let result = self
            .call_model_inner(request)
            .instrument(span.clone())
            .await;
        match &result {
            Ok((response, model)) => {
                span.record("gen_ai.usage.input_tokens", response.usage.input_tokens);
                span.record("gen_ai.usage.output_tokens", response.usage.output_tokens);
                span.record(
                    "gen_ai.response.finish_reasons",
                    crate::run::stop_reason_str(response.stop_reason),
                );
                span.record("gen_ai.response.model", model.as_str());
            }
            Err(_) => {
                span.record("otel.status_code", "ERROR");
            }
        }
        result
    }

    async fn call_model_inner(
        &self,
        request: CompletionRequest,
    ) -> Result<(CompletionResponse, String), AgentError> {
        // No artifact pointer may reach a provider — fail loud, never silently
        // drop a file the model was meant to see. Resolving them into bytes is a
        // `before_model` hook's job; this is the backstop when none did.
        if let Some(id) = first_stored_artifact(&request) {
            return Err(AgentError::Media(format!(
                "unresolved artifact reference {id} reached the model call"
            )));
        }
        if self.state.observed() {
            self.call_model_streaming(request).await
        } else {
            self.call_model_complete(request).await
        }
    }

    /// Stream the primary provider, forwarding deltas to the event sink.
    async fn call_model_streaming(
        &self,
        request: CompletionRequest,
    ) -> Result<(CompletionResponse, String), AgentError> {
        let (se_tx, mut se_rx) = mpsc::channel::<StreamEvent>(64);
        let provider = self.provider.clone();
        let sinks = self.state.emitters().to_vec();

        let forward = async move {
            while let Some(ev) = se_rx.recv().await {
                let delta = match ev {
                    StreamEvent::TextDelta { text } => AgentEvent::TextDelta(text),
                    StreamEvent::ThinkingDelta { text } => AgentEvent::ThinkingDelta(text),
                    _ => continue,
                };
                for sink in &sinks {
                    sink.emit(delta.clone());
                }
            }
        };

        let (stream_result, _) = tokio::join!(provider.stream(request.clone(), se_tx), forward);

        match stream_result {
            Ok(response) => Ok((response, request.model)),
            Err(e) if is_fallback_worthy(&e) => {
                tracing::warn!(
                    provider = provider.name(),
                    error = %e,
                    "streaming call failed; retrying non-streamed"
                );
                self.call_model_complete(request).await
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Non-streaming call with backoff + fallback-model chain.
    async fn call_model_complete(
        &self,
        request: CompletionRequest,
    ) -> Result<(CompletionResponse, String), AgentError> {
        let primary_err =
            match retry::call_with_retry(self.provider.as_ref(), request.clone()).await {
                Ok(response) => return Ok((response, request.model)),
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
                    tracing::Span::current().record("fallback_model", fb.model.as_str());
                    tracing::warn!(
                        fallback_provider = fb.provider.name(),
                        fallback_model = %fb.model,
                        primary_error = %primary_err,
                        "primary model call failed; served from fallback"
                    );
                    return Ok((response, fb.model.clone()));
                }
                Err(e) => {
                    tracing::warn!(
                        fallback_provider = fb.provider.name(),
                        fallback_model = %fb.model,
                        error = %e,
                        "fallback failed"
                    );
                }
            }
        }

        Err(primary_err.into())
    }
}

fn first_stored_artifact(request: &CompletionRequest) -> Option<&str> {
    request.messages.iter().find_map(|message| {
        let MessageContent::Blocks(blocks) = &message.content else {
            return None;
        };
        blocks.iter().find_map(|block| match block {
            ContentBlock::Image { source, .. } | ContentBlock::File { source, .. } => {
                source.stored()
            }
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
