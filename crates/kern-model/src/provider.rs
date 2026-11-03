//! The model provider abstraction (ARCHITECTURE.md §8.1).
//!
//! A provider translates a normalized `CompletionRequest` into its API call
//! and normalizes the response back. Providers are `Send + Sync` and shared
//! through `Arc<dyn ModelProvider>` in the gateway, so they must be stateless
//! for a request's lifetime (per-request state belongs in the request).

use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedSender;

use crate::error::ModelError;
use crate::types::{Completion, CompletionRequest, CompletionResponse, Usage};

/// Live delta sink: text fragments are pushed as they arrive.
///
/// A closed receiver is not an error — nobody is listening, and streaming is
/// purely observational (ARCHITECTURE §29).
pub type DeltaSender = UnboundedSender<String>;

/// Push one fragment onto a delta sink, ignoring a closed receiver.
pub fn emit_delta(deltas: &DeltaSender, text: &str) {
    let _ = deltas.send(text.to_string());
}

/// A matched delta channel: the provider writes, the runtime drains.
pub fn delta_channel() -> (DeltaSender, tokio::sync::mpsc::UnboundedReceiver<String>) {
    tokio::sync::mpsc::unbounded_channel()
}

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

    /// Usage-aware variant of [`ModelProvider::complete`].
    ///
    /// Adapters that parse a provider usage block override this; the default
    /// reports no usage at all (`None` halves — not zeroes), so callers know to
    /// estimate instead of trusting a fabricated count.
    async fn complete_counting(&self, req: &CompletionRequest) -> Result<Completion, ModelError> {
        Ok(Completion::new(self.complete(req).await?, Usage::default()))
    }

    /// Stream one completion, delivering text fragments through `on_delta` as
    /// they arrive.
    ///
    /// The default implementation calls [`ModelProvider::complete`] and hands
    /// the finished text over in one piece, so an adapter without streaming
    /// support still satisfies the contract. Deltas are **advisory**: they are
    /// never persisted, and the returned `CompletionResponse` remains the
    /// authoritative turn — a stream that dies mid-flight has produced no
    /// durable effect (ARCHITECTURE §29).
    async fn stream(
        &self,
        req: &CompletionRequest,
        deltas: &DeltaSender,
    ) -> Result<CompletionResponse, ModelError> {
        let response = self.complete(req).await?;
        if let CompletionResponse::Finish { text, .. } = &response {
            if !text.is_empty() {
                emit_delta(deltas, text);
            }
        }
        Ok(response)
    }

    /// Usage-aware variant of [`ModelProvider::stream`].
    async fn stream_counting(
        &self,
        req: &CompletionRequest,
        deltas: &DeltaSender,
    ) -> Result<Completion, ModelError> {
        Ok(Completion::new(
            self.stream(req, deltas).await?,
            Usage::default(),
        ))
    }
}
