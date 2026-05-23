//! Provider trait and shared HTTP plumbing for runic LLM backends.
//!
//! Distilled from jcode-provider-core. The original trait carried ~40 methods
//! to drive jcode's multi-provider TUI (model picker, reasoning-effort UI,
//! service tiers, premium-request conservation, transport switching, ...).
//! This version keeps only the surface a headless agent loop needs.

use async_trait::async_trait;
use futures::Stream;
use runic_message_types::{
    ContentBlock, Message, Role, StreamEvent, ToolDefinition, messages_with_dynamic_system_context,
};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// Stream of events from a provider.
pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>;

/// Errors a provider can raise.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("authentication failed: {0}")]
    Auth(String),

    #[error("HTTP error ({status}): {body}")]
    Http { status: u16, body: String },

    #[error("rate limited (retry after {retry_after_secs:?}s): {message}")]
    RateLimit {
        message: String,
        retry_after_secs: Option<u64>,
    },

    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),

    #[error("decode error: {0}")]
    Decode(String),

    #[error("model not supported: {0}")]
    UnsupportedModel(String),

    #[error("model switching not supported by this provider")]
    ModelSwitchUnsupported,

    #[error("{0}")]
    Other(String),
}

impl ProviderError {
    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }

    pub fn decode(msg: impl Into<String>) -> Self {
        Self::Decode(msg.into())
    }
}

/// Provider trait for LLM backends.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Send messages and get a streaming response.
    ///
    /// `resume_session_id`: optional provider-specific session ID for conversation resume.
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        system: &str,
        resume_session_id: Option<&str>,
    ) -> Result<EventStream, ProviderError>;

    /// Send messages with a split system prompt: a stable cached prefix
    /// (`system_static`) plus volatile context (`system_dynamic`) that gets
    /// injected after the latest user prompt so the cache prefix stays warm.
    async fn complete_split(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        system_static: &str,
        system_dynamic: &str,
        resume_session_id: Option<&str>,
    ) -> Result<EventStream, ProviderError> {
        let dynamic_messages = messages_with_dynamic_system_context(messages, system_dynamic);
        self.complete(&dynamic_messages, tools, system_static, resume_session_id)
            .await
    }

    /// Provider identifier (e.g. "anthropic").
    fn name(&self) -> &str;

    /// Current model identifier.
    fn model(&self) -> String;

    /// Whether this provider path can safely receive `ContentBlock::Image` inputs.
    fn supports_image_input(&self) -> bool {
        false
    }

    /// List models known to this provider.
    fn available_models(&self) -> Vec<&'static str> {
        vec![]
    }

    /// Switch to a different model. Default: unsupported.
    fn set_model(&self, _model: &str) -> Result<(), ProviderError> {
        Err(ProviderError::ModelSwitchUnsupported)
    }

    /// Maximum context window in tokens for the active model. Default: 200_000.
    fn context_window(&self) -> usize {
        200_000
    }

    /// Create a new provider instance with independent mutable state.
    /// Used by subagent spawning so the child does not share runtime state.
    fn fork(&self) -> Arc<dyn Provider>;

    /// Simple non-streaming completion that returns the assembled text.
    async fn complete_simple(
        &self,
        prompt: &str,
        system: &str,
    ) -> Result<String, ProviderError> {
        use futures::StreamExt;

        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: prompt.to_string(),
                cache_control: None,
            }],
            timestamp: None,
            tool_duration_ms: None,
        }];

        let response = self.complete(&messages, &[], system, None).await?;
        let mut result = String::new();
        tokio::pin!(response);

        while let Some(event) = response.next().await {
            match event {
                Ok(StreamEvent::TextDelta(text)) => result.push_str(&text),
                Ok(_) => {}
                Err(err) => return Err(err),
            }
        }

        Ok(result)
    }
}

// ─── Retry policy ───────────────────────────────────────────────────────────

/// Per-request retry policy applied around the HTTP send (NOT around the
/// streaming body — once bytes start flowing, we don't retry).
///
/// Exponential backoff with the same shape jcode uses:
/// `base_delay * 2^(attempt-1)`, capped at `max_delay`. For `RateLimit`
/// errors that carry `retry_after_secs`, we honor the header instead of
/// the computed backoff.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Total attempts including the first. 1 = no retries.
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        // Matches jcode: 5 attempts, 1s → 2s → 4s → 8s between, capped at 60s.
        Self {
            max_attempts: 5,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
        }
    }
}

/// Classify whether a `ProviderError` is worth retrying.
///
/// - Transport errors → yes (connection blip, DNS, TLS)
/// - RateLimit → yes (we'll wait per the retry-after if present)
/// - HTTP 408, 429, 500, 502, 503, 504 → yes
/// - Auth / Decode / UnsupportedModel / ModelSwitchUnsupported / Other → no
pub fn is_retryable(err: &ProviderError) -> bool {
    use ProviderError::*;
    match err {
        Transport(_) => true,
        RateLimit { .. } => true,
        Http { status, .. } => matches!(*status, 408 | 429 | 500 | 502 | 503 | 504),
        Auth(_) | Decode(_) | UnsupportedModel(_) | ModelSwitchUnsupported | Other(_) => false,
    }
}

/// Compute the delay before the next attempt.
///
/// Honors `RateLimit::retry_after_secs` when present; otherwise computes
/// exponential backoff `base * 2^(attempt-1)` capped at `max_delay`.
pub fn next_delay(policy: &RetryPolicy, err: &ProviderError, attempt: u32) -> Duration {
    if let ProviderError::RateLimit {
        retry_after_secs: Some(secs),
        ..
    } = err
    {
        // Cap server-suggested delay too so we don't sleep absurdly long.
        return Duration::from_secs(*secs).min(policy.max_delay);
    }
    let shift = attempt.saturating_sub(1).min(20);
    let factor: u64 = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    let backoff = policy.base_delay.saturating_mul(factor.min(u32::MAX as u64) as u32);
    backoff.min(policy.max_delay)
}

/// Retry an async operation according to `policy`. Calls `on_retry` after a
/// failed attempt and before sleeping, so the caller can emit a
/// `StreamEvent::ConnectionPhase { Retrying { attempt, max } }` to the UI.
pub async fn with_retry<T, F, Fut, R>(
    policy: &RetryPolicy,
    mut on_retry: R,
    mut op: F,
) -> Result<T, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ProviderError>>,
    R: FnMut(u32, u32, Duration),
{
    let max = policy.max_attempts.max(1);
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match op().await {
            Ok(v) => return Ok(v),
            Err(err) => {
                if !is_retryable(&err) || attempt >= max {
                    return Err(err);
                }
                let delay = next_delay(policy, &err, attempt);
                on_retry(attempt, max, delay);
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Canonical User-Agent for runic outbound HTTP.
pub const RUNIC_USER_AGENT: &str = concat!("runic/", env!("CARGO_PKG_VERSION"));

/// Shared HTTP client for all provider requests. Building a `reqwest::Client`
/// is expensive (~10ms TLS init), so we reuse a single process-wide instance.
pub fn shared_http_client() -> reqwest::Client {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(RUNIC_USER_AGENT)
                .connect_timeout(Duration::from_secs(15))
                .tcp_keepalive(Some(Duration::from_secs(30)))
                .pool_idle_timeout(Duration::from_secs(90))
                .pool_max_idle_per_host(8)
                .build()
                .unwrap_or_else(|err| {
                    eprintln!("runic: failed to build shared provider HTTP client: {err}");
                    reqwest::Client::builder()
                        .user_agent(RUNIC_USER_AGENT)
                        .build()
                        .expect("fallback runic HTTP client should build")
                })
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_user_agent_identifies_runic() {
        assert!(RUNIC_USER_AGENT.starts_with("runic/"));
    }

    #[test]
    fn shared_http_client_returns_clones() {
        let _a = shared_http_client();
        let _b = shared_http_client();
    }

    // ─── Retry classifier ────────────────────────────────────────────────────

    #[test]
    fn retryable_classification_covers_expected_5xx_and_429() {
        for s in [408, 429, 500, 502, 503, 504] {
            assert!(
                is_retryable(&ProviderError::Http {
                    status: s,
                    body: "".into(),
                }),
                "{} should be retryable",
                s
            );
        }
        for s in [400, 401, 403, 404, 422] {
            assert!(
                !is_retryable(&ProviderError::Http {
                    status: s,
                    body: "".into(),
                }),
                "{} should NOT be retryable",
                s
            );
        }
    }

    #[test]
    fn auth_and_decode_errors_are_never_retryable() {
        assert!(!is_retryable(&ProviderError::Auth("bad key".into())));
        assert!(!is_retryable(&ProviderError::Decode("bad json".into())));
        assert!(!is_retryable(&ProviderError::UnsupportedModel("x".into())));
        assert!(!is_retryable(&ProviderError::ModelSwitchUnsupported));
        assert!(!is_retryable(&ProviderError::other("nope")));
    }

    #[test]
    fn rate_limit_is_retryable() {
        assert!(is_retryable(&ProviderError::RateLimit {
            message: "slow down".into(),
            retry_after_secs: Some(3),
        }));
    }

    // ─── Backoff computation ─────────────────────────────────────────────────

    #[test]
    fn next_delay_uses_exponential_backoff_when_no_retry_after() {
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(60),
        };
        let err = ProviderError::Http {
            status: 503,
            body: "".into(),
        };
        assert_eq!(next_delay(&policy, &err, 1), Duration::from_millis(100));
        assert_eq!(next_delay(&policy, &err, 2), Duration::from_millis(200));
        assert_eq!(next_delay(&policy, &err, 3), Duration::from_millis(400));
        assert_eq!(next_delay(&policy, &err, 4), Duration::from_millis(800));
    }

    #[test]
    fn next_delay_honors_retry_after_when_present() {
        let policy = RetryPolicy::default();
        let err = ProviderError::RateLimit {
            message: "".into(),
            retry_after_secs: Some(5),
        };
        assert_eq!(next_delay(&policy, &err, 1), Duration::from_secs(5));
    }

    #[test]
    fn next_delay_caps_at_max_delay() {
        let policy = RetryPolicy {
            max_attempts: 30,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(1000),
        };
        let err = ProviderError::Http {
            status: 503,
            body: "".into(),
        };
        // attempt 10 -> 100ms * 2^9 = 51200ms, capped to 1000
        assert_eq!(next_delay(&policy, &err, 10), Duration::from_millis(1000));
    }

    // ─── with_retry integration ──────────────────────────────────────────────

    #[tokio::test]
    async fn with_retry_returns_immediately_on_success() {
        let policy = RetryPolicy::default();
        let mut retries = 0u32;
        let result = with_retry(
            &policy,
            |_, _, _| retries += 1,
            || async { Ok::<u32, ProviderError>(42) },
        )
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(retries, 0);
    }

    #[tokio::test]
    async fn with_retry_does_not_retry_non_retryable() {
        let policy = RetryPolicy::default();
        let mut retries = 0u32;
        let result: Result<(), ProviderError> = with_retry(
            &policy,
            |_, _, _| retries += 1,
            || async { Err(ProviderError::Auth("nope".into())) },
        )
        .await;
        assert!(matches!(result, Err(ProviderError::Auth(_))));
        assert_eq!(retries, 0, "auth errors must not trigger retries");
    }

    #[tokio::test]
    async fn with_retry_retries_until_success() {
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
        };
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter_for_op = counter.clone();
        let mut retries = 0u32;

        let result = with_retry(
            &policy,
            |_, _, _| retries += 1,
            || {
                let c = counter_for_op.clone();
                async move {
                    let attempt = c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if attempt < 2 {
                        Err(ProviderError::Http {
                            status: 503,
                            body: "transient".into(),
                        })
                    } else {
                        Ok::<u32, ProviderError>(7)
                    }
                }
            },
        )
        .await;

        assert_eq!(result.unwrap(), 7);
        assert_eq!(retries, 2, "should retry twice before the 3rd attempt succeeds");
    }

    #[tokio::test]
    async fn with_retry_exhausts_and_returns_last_error() {
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
        };
        let mut retries = 0u32;

        let result: Result<u32, ProviderError> = with_retry(
            &policy,
            |_, _, _| retries += 1,
            || async {
                Err(ProviderError::Http {
                    status: 503,
                    body: "still bad".into(),
                })
            },
        )
        .await;

        assert!(matches!(result, Err(ProviderError::Http { status: 503, .. })));
        // 3 attempts means 2 retries (after attempts 1 and 2, no retry after final attempt 3)
        assert_eq!(retries, 2);
    }
}
