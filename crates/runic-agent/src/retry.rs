//! `retry` — a thin backoff wrapper around `Provider::complete` (OpenFang's
//! `call_with_retry`, trimmed). Step 1 handles the retryable transient classes
//! (`RateLimited`, `Overloaded`) with exponential backoff; the fallback-model
//! chain (needs a provider registry) lands in a later step.

use std::time::Duration;

use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};

/// Max retry attempts after the initial try.
const MAX_RETRIES: u32 = 3;
/// Base backoff; doubles each attempt.
const BASE_BACKOFF_MS: u64 = 1000;

/// Call `provider.complete`, retrying transient failures with exponential
/// backoff. Non-transient errors return immediately.
pub async fn call_with_retry(
    provider: &dyn Provider,
    request: CompletionRequest,
) -> Result<CompletionResponse, ProviderError> {
    let mut attempt = 0;
    loop {
        match provider.complete(request.clone()).await {
            Ok(resp) => return Ok(resp),
            Err(err) => {
                let delay = match &err {
                    ProviderError::RateLimited { retry_after_ms }
                    | ProviderError::Overloaded { retry_after_ms } => {
                        let backoff = BASE_BACKOFF_MS << attempt;
                        Duration::from_millis((*retry_after_ms).max(backoff))
                    }
                    _ => return Err(err), // not retryable
                };
                if attempt >= MAX_RETRIES {
                    return Err(err);
                }
                tracing::warn!(
                    provider = provider.name(),
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    "provider call transient failure; retrying"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
}
