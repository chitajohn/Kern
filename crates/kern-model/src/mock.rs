//! Deterministic mock provider (ARCHITECTURE.md §8.2).
//!
//! Serves a FIFO script of steps: finish text, thinking text, single or
//! multi-call tool batches, failures, or a hang (for gateway timeout tests).
//! This is what makes engine tests deterministic despite model
//! nondeterminism. An exhausted script fails loudly (`INVALID_RESPONSE`) so
//! tests surface script/step-count bugs instead of silently succeeding.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::ModelError;
use crate::provider::ModelProvider;
use crate::types::{CompletionRequest, CompletionResponse, FinishReason, ToolCall};

/// One scripted provider response.
#[derive(Debug, Clone)]
pub enum ScriptedStep {
    /// A finished turn with the given text.
    Finish(String),
    /// Reasoning text (`CompletionResponse::Thinking`).
    Thinking(String),
    /// A batch of requested tool calls (1..N).
    ToolCalls(Vec<ToolCall>),
    /// Return an error (e.g. rate-limited or auth) as-is.
    Fail(ModelError),
    /// Never return (gateway timeout tests).
    Hang,
}

/// A scripted provider. Clone is cheap and shares the script with observers.
#[derive(Clone)]
pub struct MockProvider {
    script: Arc<Mutex<VecDeque<ScriptedStep>>>,
    /// The original script for [`MockProvider::looping`]: when the live FIFO
    /// empties, it is refilled from this copy so every agent/execution served
    /// by the shared provider runs the same steps from the start.
    original: Option<Arc<Mutex<VecDeque<ScriptedStep>>>>,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl MockProvider {
    pub fn new(steps: impl IntoIterator<Item = ScriptedStep>) -> Self {
        Self {
            script: Arc::new(Mutex::new(steps.into_iter().collect())),
            original: None,
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A provider that ALWAYS finishes with `text`.
    pub fn finishing(text: impl Into<String>) -> Self {
        Self::new([ScriptedStep::Finish(text.into())])
    }

    /// A provider that replays `steps` from the start every time the script is
    /// exhausted. Needed wherever ONE shared mock serves many agents/runs
    /// (multi-agent daemon tests, benchmarks): each run deterministically
    /// starts at step 0 instead of hitting the exhausted-script error.
    pub fn looping(steps: impl IntoIterator<Item = ScriptedStep>) -> Self {
        let steps: VecDeque<ScriptedStep> = steps.into_iter().collect();
        Self {
            script: Arc::new(Mutex::new(steps.clone())),
            original: Some(Arc::new(Mutex::new(steps))),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Steps remaining in the script (test observation).
    pub fn remaining(&self) -> usize {
        self.script
            .lock()
            .expect("mock script mutex poisoned")
            .len()
    }

    /// Every completion request received so far, drained (test observation:
    /// the engine's request-building contract — system prompt, tool specs,
    /// bounded history).
    pub fn take_requests(&self) -> Vec<CompletionRequest> {
        let mut requests = self.requests.lock().expect("mock request log poisoned");
        std::mem::take(&mut *requests)
    }
}

#[async_trait]
impl ModelProvider for MockProvider {
    fn id(&self) -> &str {
        "mock"
    }

    async fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, ModelError> {
        self.requests
            .lock()
            .expect("mock request log poisoned")
            .push(req.clone());
        let step = {
            let mut script = self.script.lock().expect("mock script mutex poisoned");
            match script.pop_front() {
                Some(step) => step,
                None => {
                    // Looping provider: refill from the original script, then
                    // serve the first step of the replay.
                    let original = self.original.as_ref().ok_or_else(|| {
                        ModelError::InvalidResponse(
                            "mock script exhausted — script more steps".to_string(),
                        )
                    })?;
                    *script = original
                        .lock()
                        .expect("mock original mutex poisoned")
                        .clone();
                    script.pop_front().ok_or_else(|| {
                        ModelError::InvalidResponse(
                            "mock script exhausted — script more steps".to_string(),
                        )
                    })?
                }
            }
        };
        match step {
            ScriptedStep::Finish(text) => Ok(CompletionResponse::Finish {
                reason: FinishReason::Stop,
                text,
            }),
            ScriptedStep::Thinking(text) => Ok(CompletionResponse::Thinking(text)),
            ScriptedStep::ToolCalls(calls) => Ok(CompletionResponse::ToolCalls(calls)),
            ScriptedStep::Fail(err) => Err(err),
            ScriptedStep::Hang => {
                std::future::pending::<Result<CompletionResponse, ModelError>>().await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn looping_provider_restarts_the_script_when_exhausted() {
        let provider = MockProvider::looping([ScriptedStep::Finish("again".into())]);
        for _ in 0..3 {
            let req = CompletionRequest::new("mock", "test", Vec::new());
            match provider.complete(&req).await.unwrap() {
                CompletionResponse::Finish { text, .. } => assert_eq!(text, "again"),
                other => panic!("expected finish, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn non_looping_provider_exhausts_with_an_error() {
        let provider = MockProvider::new([ScriptedStep::Finish("once".into())]);
        let req = CompletionRequest::new("mock", "test", Vec::new());
        assert!(provider.complete(&req).await.is_ok());
        let err = provider.complete(&req).await.unwrap_err();
        assert!(err.to_string().contains("exhausted"), "{err}");
    }
}
