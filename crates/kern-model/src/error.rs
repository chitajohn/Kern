//! Model error taxonomy (SPEC.md §13 kinds: timeout, unavailable, auth,
//! rate-limited, invalid response, budget exhausted).
//!
//! Retry policy (SPEC.md §8.2): `RateLimited` and `Unavailable` (transport,
//! 5xx) and `Timeout` are transient and retried with exponential backoff up to
//! the per-agent budget; `Auth` and `InvalidResponse` are permanent and fail
//! immediately; exhausting the budget on a transient error surfaces as
//! `BudgetExhausted` (or `Timeout` when the final failure was a timeout).

use std::time::Duration;

/// Structured model error.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ModelError {
    #[error("model request timed out after {0:?}")]
    Timeout(Duration),
    #[error("model provider unavailable: {0}")]
    Unavailable(String),
    #[error("model provider rejected credentials: {0}")]
    Auth(String),
    #[error("model provider rate limited: {0}")]
    RateLimited(String),
    #[error("model returned an invalid response: {0}")]
    InvalidResponse(String),
    #[error("model retry budget exhausted: {0}")]
    BudgetExhausted(String),
}

impl ModelError {
    /// Whether the gateway should retry this error (transient kinds only).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            ModelError::Timeout(_) | ModelError::Unavailable(_) | ModelError::RateLimited(_)
        )
    }
}

/// Map an HTTP status onto a `ModelError`. `Timeout` (the gateway's own clock)
/// is only produced by the gateway; a provider's 408 is treated as a transient
/// `Unavailable`.
pub(crate) fn from_status(status: reqwest::StatusCode, provider: &str) -> ModelError {
    match status.as_u16() {
        401 | 403 => ModelError::Auth(format!("{provider} rejected the API key (HTTP {status})")),
        408 => ModelError::Unavailable(format!("{provider} request timed out (HTTP {status})")),
        429 => ModelError::RateLimited(format!("{provider} rate limited (HTTP {status})")),
        500..=599 => ModelError::Unavailable(format!("{provider} unavailable (HTTP {status})")),
        other => ModelError::Unavailable(format!("{provider} unexpected status (HTTP {other})")),
    }
}
