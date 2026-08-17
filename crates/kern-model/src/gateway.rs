//! Model gateway (ARCHITECTURE.md §8.2, SPEC.md §8.2).
//!
//! Policy, enforced here (never in the adapters):
//! - **Selection:** the request's `provider` id resolves to a registered
//!   adapter. An unknown id is a structured `Unavailable` error (config
//!   validation normally catches this earlier).
//! - **Timeout:** each attempt is wrapped in `model.timeout` (default 60s).
//!   A clock expiry produces `ModelError::Timeout`.
//! - **Retries:** transient kinds only (`Timeout`, `Unavailable`, `RateLimited`)
//!   are retried up to `model.retries` (default 2) with exponential backoff
//!   (`base * 2^attempt`, capped). `Auth` and `InvalidResponse` are permanent
//!   and fail immediately.
//! - **Budget:** exhausting the retry budget on a transient error surfaces as
//!   `BudgetExhausted` — or `Timeout` when the final failure was a timeout —
//!   so callers can route the agent to `failed` with an actionable error.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use crate::anthropic::AnthropicProvider;
use crate::error::ModelError;
use crate::ollama::OllamaProvider;
use crate::openai::OpenAiProvider;
use crate::provider::ModelProvider;
use crate::types::{CompletionRequest, CompletionResponse};

/// `model.timeout` default (SPEC.md §8.2).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// `model.retries` default (SPEC.md §8.2): attempts beyond the first.
pub const DEFAULT_RETRIES: u32 = 2;
/// Exponential backoff base for transient retries.
const DEFAULT_BACKOFF_BASE: Duration = Duration::from_millis(250);
/// Backoff cap so a long retry chain never sleeps unboundedly.
const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(4);

/// Backoff for attempt `attempt` (0-based): `base * 2^attempt`, capped at `max`.
fn backoff(attempt: u32, base: Duration, max: Duration) -> Duration {
    let shift = u32::min(attempt, 31);
    let doubled = base.checked_mul(2u32.saturating_pow(shift)).unwrap_or(max);
    doubled.min(max)
}

pub struct ModelGateway {
    providers: HashMap<String, Arc<dyn ModelProvider>>,
    backoff_base: Duration,
    backoff_max: Duration,
}

impl Default for ModelGateway {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelGateway {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
            backoff_base: DEFAULT_BACKOFF_BASE,
            backoff_max: DEFAULT_BACKOFF_MAX,
        }
    }

    /// Test/instrumentation knob: override the retry backoff schedule.
    pub fn with_backoff(mut self, base: Duration, max: Duration) -> Self {
        self.backoff_base = base;
        self.backoff_max = max;
        self
    }

    /// Register an adapter under its `id()`. Duplicate ids are rejected.
    pub fn register(&mut self, provider: Arc<dyn ModelProvider>) -> Result<(), ModelError> {
        let id = provider.id().to_string();
        if self.providers.contains_key(&id) {
            return Err(ModelError::InvalidResponse(format!(
                "provider '{id}' is already registered"
            )));
        }
        self.providers.insert(id, provider);
        Ok(())
    }

    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }

    pub fn has_provider(&self, id: &str) -> bool {
        self.providers.contains_key(id)
    }

    /// Registered provider ids, sorted (the `/models` API surface).
    pub fn provider_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.providers.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// A gateway pre-populated with the builtin adapters read from the
    /// environment (missing keys surface as `Auth` at call time, so
    /// registration never fails for a missing key).
    pub fn with_default_providers() -> Self {
        let mut gateway = Self::new();
        gateway
            .register(Arc::new(OpenAiProvider::from_env()))
            .expect("builtin provider ids are distinct");
        gateway
            .register(Arc::new(AnthropicProvider::from_env()))
            .expect("builtin provider ids are distinct");
        gateway
            .register(Arc::new(OllamaProvider::from_env()))
            .expect("builtin provider ids are distinct");
        gateway
    }

    /// Run one completion with the full timeout/retry/budget policy.
    pub async fn complete(
        &self,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, ModelError> {
        let provider = self.providers.get(&req.provider).ok_or_else(|| {
            ModelError::Unavailable(format!(
                "no provider adapter registered for '{}'",
                req.provider
            ))
        })?;

        let timeout = req.timeout.unwrap_or(DEFAULT_TIMEOUT);
        let retries = req.retries.unwrap_or(DEFAULT_RETRIES);

        for attempt in 0..=retries {
            if attempt > 0 {
                // Exponential backoff between attempts.
                tokio::time::sleep(backoff(attempt - 1, self.backoff_base, self.backoff_max)).await;
            }

            let deadline = Instant::now() + timeout;
            let outcome = tokio::time::timeout_at(deadline, provider.complete(req)).await;

            let err = match outcome {
                Ok(Ok(response)) => return Ok(response),
                Ok(Err(err)) => err,
                Err(_elapsed) => ModelError::Timeout(timeout),
            };

            // Permanent errors fail immediately (no retries); transient errors
            // consume the budget until `retries` is exhausted.
            if !err.is_retryable() || attempt == retries {
                return Err(finalize(err, attempt));
            }
        }
        unreachable!("loop always returns")
    }
}

/// Map the final attempt's error onto the public surface: permanent errors
/// pass through unchanged; an exhausted transient budget becomes
/// `BudgetExhausted` (or `Timeout` when the final failure was a timeout).
fn finalize(last: ModelError, attempts: u32) -> ModelError {
    match &last {
        ModelError::Timeout(_) => last,
        ModelError::Auth(_) | ModelError::InvalidResponse(_) => last,
        ModelError::Unavailable(_)
        | ModelError::RateLimited(_)
        | ModelError::BudgetExhausted(_) => ModelError::BudgetExhausted(format!(
            "retry budget exhausted after {} attempt(s): {last}",
            attempts + 1
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockProvider, ScriptedStep};
    use crate::types::{FinishReason, Message, ToolCall};
    use serde_json::json;

    fn req(provider: &str) -> CompletionRequest {
        CompletionRequest::new(provider, "test-model", vec![Message::user("hi")])
    }

    fn tool_call(name: &str) -> ToolCall {
        ToolCall {
            id: format!("call-{name}"),
            name: name.to_string(),
            arguments: json!({}),
        }
    }

    #[tokio::test]
    async fn timeout_returns_without_hanging() {
        let provider = MockProvider::new([ScriptedStep::Hang]);
        let mut gateway =
            ModelGateway::new().with_backoff(Duration::from_millis(1), Duration::from_millis(2));
        gateway.register(Arc::new(provider)).unwrap();

        let mut r = req("mock");
        r.timeout = Some(Duration::from_millis(50));
        r.retries = Some(0);

        let started = Instant::now();
        let err = gateway.complete(&r).await.unwrap_err();
        assert!(
            matches!(err, ModelError::Timeout(_)),
            "expected Timeout, got {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout must return promptly"
        );
    }

    #[tokio::test]
    async fn timeout_honors_retry_budget() {
        // One Hang step per attempt: a popped step can only hang once.
        let provider =
            MockProvider::new([ScriptedStep::Hang, ScriptedStep::Hang, ScriptedStep::Hang]);
        let mut gateway =
            ModelGateway::new().with_backoff(Duration::from_millis(1), Duration::from_millis(2));
        gateway.register(Arc::new(provider)).unwrap();

        let mut r = req("mock");
        r.timeout = Some(Duration::from_millis(30));
        r.retries = Some(2);

        let started = Instant::now();
        let err = gateway.complete(&r).await.unwrap_err();
        // The final failure was a timeout, so the surface error stays Timeout
        // (not BudgetExhausted) per SPEC §8.2.
        assert!(matches!(err, ModelError::Timeout(_)), "got {err:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn rate_limit_exhausts_budget() {
        let provider = MockProvider::new([
            ScriptedStep::Fail(ModelError::RateLimited("quota".into())),
            ScriptedStep::Fail(ModelError::RateLimited("quota".into())),
            ScriptedStep::Fail(ModelError::RateLimited("quota".into())),
        ]);
        let mut gateway =
            ModelGateway::new().with_backoff(Duration::from_millis(1), Duration::from_millis(2));
        gateway.register(Arc::new(provider.clone())).unwrap();

        let mut r = req("mock");
        r.retries = Some(2);
        let err = gateway.complete(&r).await.unwrap_err();
        assert!(matches!(err, ModelError::BudgetExhausted(_)), "got {err:?}");
        assert!(
            provider.remaining() == 0,
            "all script steps must be consumed"
        );
    }

    #[tokio::test]
    async fn retry_then_success() {
        let provider = MockProvider::new([
            ScriptedStep::Fail(ModelError::Unavailable("boom".into())),
            ScriptedStep::Fail(ModelError::Unavailable("boom".into())),
            ScriptedStep::Finish("recovered".into()),
        ]);
        let mut gateway =
            ModelGateway::new().with_backoff(Duration::from_millis(1), Duration::from_millis(2));
        gateway.register(Arc::new(provider.clone())).unwrap();

        let mut r = req("mock");
        r.retries = Some(3);
        let response = gateway.complete(&r).await.unwrap();
        match response {
            CompletionResponse::Finish { reason, text } => {
                assert_eq!(reason, FinishReason::Stop);
                assert_eq!(text, "recovered");
            }
            other => panic!("expected finish, got {other:?}"),
        }
        assert_eq!(provider.remaining(), 0);
    }

    #[tokio::test]
    async fn permanent_error_fails_immediately_without_retries() {
        // Two scripted steps: if the gateway retried, it would pop the second
        // one. Remaining == 1 proves exactly one attempt was made.
        let provider = MockProvider::new([
            ScriptedStep::Fail(ModelError::Auth("bad key".into())),
            ScriptedStep::Finish("must never be reached".into()),
        ]);
        let mut gateway =
            ModelGateway::new().with_backoff(Duration::from_millis(1), Duration::from_millis(2));
        gateway.register(Arc::new(provider.clone())).unwrap();

        let mut r = req("mock");
        r.retries = Some(5);
        let err = gateway.complete(&r).await.unwrap_err();
        assert!(matches!(err, ModelError::Auth(_)), "got {err:?}");
        assert_eq!(
            provider.remaining(),
            1,
            "permanent errors must not consume the retry budget"
        );
    }

    #[tokio::test]
    async fn unknown_provider_is_unavailable() {
        let gateway = ModelGateway::new();
        let err = gateway
            .complete(&req("no-such-provider"))
            .await
            .unwrap_err();
        assert!(matches!(err, ModelError::Unavailable(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn duplicate_registration_is_rejected() {
        let mut gateway = ModelGateway::new();
        gateway
            .register(Arc::new(MockProvider::finishing("a")))
            .unwrap();
        let err = gateway
            .register(Arc::new(MockProvider::finishing("b")))
            .unwrap_err();
        assert!(
            matches!(err, ModelError::InvalidResponse(_)),
            "duplicate id must be rejected, got {err:?}"
        );
        assert_eq!(gateway.provider_count(), 1);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let base = Duration::from_millis(100);
        let max = Duration::from_millis(1000);
        assert_eq!(backoff(0, base, max), Duration::from_millis(100));
        assert_eq!(backoff(1, base, max), Duration::from_millis(200));
        assert_eq!(backoff(2, base, max), Duration::from_millis(400));
        assert_eq!(backoff(3, base, max), Duration::from_millis(800));
        assert_eq!(backoff(4, base, max), Duration::from_millis(1000), "capped");
        assert_eq!(
            backoff(20, base, max),
            Duration::from_millis(1000),
            "capped"
        );
    }

    #[test]
    fn policy_defaults_match_spec() {
        assert_eq!(DEFAULT_TIMEOUT, Duration::from_secs(60));
        assert_eq!(DEFAULT_RETRIES, 2);
    }

    #[tokio::test]
    async fn multi_call_batch_passthrough() {
        let provider = MockProvider::new([ScriptedStep::ToolCalls(vec![
            tool_call("filesystem"),
            tool_call("http"),
        ])]);
        let mut gateway = ModelGateway::new();
        gateway.register(Arc::new(provider)).unwrap();
        let response = gateway.complete(&req("mock")).await.unwrap();
        match response {
            CompletionResponse::ToolCalls(calls) => assert_eq!(calls.len(), 2),
            other => panic!("expected tool calls, got {other:?}"),
        }
    }
}
