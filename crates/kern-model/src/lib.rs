//! Kern model gateway (ARCHITECTURE.md §8).
//!
//! Normalized types (`types`), the provider trait (`provider`), the policy
//! wrapper (`gateway`), and the adapters (`openai`, `anthropic`, `ollama`) plus
//! a deterministic scripted `mock` provider for tests. Adapters are raw
//! `reqwest` calls (no provider SDKs) so the request/response contract is
//! fully under our control and fixture-testable without live keys.
//!
//! `stream` holds the shared line framing every streaming adapter needs.

pub mod anthropic;
pub mod error;
pub mod gateway;
pub mod mock;
pub mod ollama;
pub mod openai;
pub mod provider;
pub mod stream;
pub mod tokens;
pub mod types;

pub use error::ModelError;
pub use gateway::{ModelGateway, DEFAULT_RETRIES, DEFAULT_TIMEOUT};
pub use mock::{MockProvider, ScriptedStep};
pub use provider::{delta_channel, emit_delta, DeltaSender, ModelProvider};
pub use types::{
    Completion, CompletionRequest, CompletionResponse, FinishReason, Message, Role, ToolCall,
    ToolSpec, Usage,
};
