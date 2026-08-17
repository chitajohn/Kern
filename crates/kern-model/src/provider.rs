//! The model provider abstraction (ARCHITECTURE.md §8.1).
//!
//! A provider translates a normalized `CompletionRequest` into its API call
//! and normalizes the response back. Providers are `Send + Sync` and shared
//! through `Arc<dyn ModelProvider>` in the gateway, so they must be stateless
//! for a request's lifetime (per-request state belongs in the request).

use async_trait::async_trait;

use crate::error::ModelError;
use crate::types::{CompletionRequest, CompletionResponse};

/// A model provider adapter.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// The provider id used in agent config (`openai | anthropic | ollama |
    /// mock`, or a custom id).
    fn id(&self) -> &str;

    /// Perform one completion. Must not enforce gateway policy (timeout,
    /// retries) — the gateway wraps this call. Should return reasonably
    /// quickly and never panic.
    async fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, ModelError>;
}
