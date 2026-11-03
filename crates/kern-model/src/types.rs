//! Normalized model types (ARCHITECTURE.md §8.1).
//!
//! These are the wire-agnostic types the engine and the adapters share.
//! Adapters translate provider-specific shapes (OpenAI Chat Completions,
//! Anthropic Messages, Ollama chat) into this model and back.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Message role. Tool results are carried as `Tool` messages keyed by
/// `tool_call_id` (SPEC.md §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One message in the conversation history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Requested tool calls (assistant messages only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// The tool call this message answers (tool messages only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// An assistant message that requested tool calls.
    pub fn assistant_with_tool_calls(
        content: impl Into<String>,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
        }
    }

    /// A tool result message answering `tool_call_id`.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    /// The wire-agnostic role string used by adapters.
    pub fn role_str(&self) -> &'static str {
        match self.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// A model-requested tool invocation (SPEC.md §4.3): `id` is the dedup key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Parsed JSON arguments (object), provider-agnostic.
    pub arguments: Value,
}

/// A tool definition advertised to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema object describing the input.
    pub input_schema: Value,
}

/// Why the model stopped generating (normalized across providers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FinishReason {
    /// The model finished its turn naturally.
    Stop,
    /// Output hit `max_tokens`; the response may be truncated.
    Length,
    /// Any provider-specific reason not mapped above.
    Other(String),
}

/// A completion request.
///
/// `provider` selects the registered adapter; `timeout`/`retries` are the
/// per-agent knobs (the gateway applies defaults when absent) — both were
/// added to the §8.1 sketch so a shared gateway can enforce per-agent policy.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionRequest {
    pub provider: String,
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub timeout: Option<std::time::Duration>,
    pub retries: Option<u32>,
}

impl CompletionRequest {
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        messages: Vec<Message>,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            messages,
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
            timeout: None,
            retries: None,
        }
    }
}

/// Normalized model output (ARCHITECTURE.md §8.1).
#[derive(Debug, Clone, PartialEq)]
pub enum CompletionResponse {
    Finish {
        reason: FinishReason,
        text: String,
    },
    /// Reasoning text to surface as `agent.thinking` (no state change).
    Thinking(String),
    /// One or more requested tool calls to execute.
    ToolCalls(Vec<ToolCall>),
}

/// Token usage when the provider reports it (optional; observability only).
///
/// `None` means "the provider did not say", never "zero": the runtime
/// estimates a missing count rather than persisting a fabricated zero
/// (see [`Usage::or_estimate`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

impl Usage {
    pub fn new(input_tokens: Option<u32>, output_tokens: Option<u32>) -> Self {
        Self {
            input_tokens,
            output_tokens,
        }
    }

    /// Whether the provider reported both counts.
    pub fn is_complete(&self) -> bool {
        self.input_tokens.is_some() && self.output_tokens.is_some()
    }

    /// Sum of the reported counts (missing counts contribute zero).
    pub fn total(&self) -> u64 {
        u64::from(self.input_tokens.unwrap_or(0)) + u64::from(self.output_tokens.unwrap_or(0))
    }

    /// Add another turn's usage, keeping `None` when neither side reported it.
    pub fn accumulate(&mut self, other: Usage) {
        self.input_tokens = merge(self.input_tokens, other.input_tokens);
        self.output_tokens = merge(self.output_tokens, other.output_tokens);
    }

    /// Fill missing halves from an independent estimate.
    pub fn or_estimate(self, estimate: Usage) -> Usage {
        Usage {
            input_tokens: self.input_tokens.or(estimate.input_tokens),
            output_tokens: self.output_tokens.or(estimate.output_tokens),
        }
    }
}

fn merge(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (Some(v), None) | (None, Some(v)) => Some(v),
        (None, None) => None,
    }
}

/// One completed model turn: the normalized response plus its token usage.
///
/// Usage travels WITH the response (never as a side channel) so a turn and its
/// accounting can never drift apart, including across a retry or a stream that
/// produced no durable effect.
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub response: CompletionResponse,
    pub usage: Usage,
}

impl Completion {
    pub fn new(response: CompletionResponse, usage: Usage) -> Self {
        Self { response, usage }
    }

    /// A completion whose usage the provider did not report.
    pub fn uncounted(response: CompletionResponse) -> Self {
        Self {
            response,
            usage: Usage::default(),
        }
    }
}

impl From<CompletionResponse> for Completion {
    fn from(response: CompletionResponse) -> Self {
        Completion::uncounted(response)
    }
}
