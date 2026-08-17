//! Anthropic Messages API adapter (ARCHITECTURE.md §8.2, decision D10).
//!
//! Raw `reqwest` call — no SDK. Environment: `ANTHROPIC_API_KEY` (required),
//! `ANTHROPIC_BASE_URL` (optional; default `https://api.anthropic.com`) for
//! compatible gateways (e.g. Bedrock proxy).
//!
//! Provider-specific translation performed here:
//! - `system` messages move to the top-level `system` field (Anthropic has no
//!   `system` message role).
//! - Tool results (`Role::Tool`) become `tool_result` content blocks inside
//!   `user` messages, keyed by `tool_use_id`.
//! - `max_tokens` is required by the Anthropic API; a request without one gets
//!   a documented default (the engine always sets it from agent config).

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::{from_status, ModelError};
use crate::provider::ModelProvider;
use crate::types::{CompletionRequest, CompletionResponse, FinishReason, Message, Role, ToolCall};

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
/// The API requires `max_tokens`; this is the fallback when a request omits it.
pub const DEFAULT_MAX_TOKENS: u32 = 1024;
/// Guard against unbounded provider responses.
const MAX_RESPONSE_BYTES: u64 = 10 * 1024 * 1024;

pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl AnthropicProvider {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key,
        }
    }

    /// Build from the environment (`ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`).
    pub fn from_env() -> Self {
        let base_url =
            std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        Self::new(base_url, std::env::var("ANTHROPIC_API_KEY").ok())
    }
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    async fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, ModelError> {
        let key = self
            .api_key
            .as_deref()
            .ok_or_else(|| ModelError::Auth("ANTHROPIC_API_KEY is not set".to_string()))?;

        let body = build_body(req);
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));

        let response = self
            .client
            .post(&url)
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(|e| ModelError::Unavailable(format!("anthropic transport error: {e}")))?;

        if !response.status().is_success() {
            return Err(from_status(response.status(), "anthropic"));
        }
        if let Some(len) = response.content_length() {
            if len > MAX_RESPONSE_BYTES {
                return Err(ModelError::InvalidResponse(format!(
                    "anthropic response exceeds {MAX_RESPONSE_BYTES} bytes"
                )));
            }
        }
        let payload: Value = response.json().await.map_err(|e| {
            ModelError::InvalidResponse(format!("anthropic response is not JSON: {e}"))
        })?;

        normalize(&payload)
    }
}

/// Serialize a normalized request into the Anthropic Messages body.
fn build_body(req: &CompletionRequest) -> Value {
    let system = req
        .messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");

    let mut body = json!({
        "model": req.model,
        "max_tokens": req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "messages": req
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .map(anthropic_message)
            .collect::<Vec<_>>(),
    });
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                    })
                })
                .collect(),
        );
    }
    if let Some(temperature) = req.temperature {
        body["temperature"] = json!(temperature);
    }
    body
}

fn anthropic_message(msg: &Message) -> Value {
    match msg.role {
        Role::User => json!({ "role": "user", "content": msg.content }),
        Role::Assistant => {
            let mut blocks = Vec::new();
            if !msg.content.is_empty() {
                blocks.push(json!({ "type": "text", "text": msg.content }));
            }
            for call in &msg.tool_calls {
                blocks.push(json!({
                    "type": "tool_use",
                    "id": call.id,
                    "name": call.name,
                    "input": call.arguments,
                }));
            }
            json!({ "role": "assistant", "content": blocks })
        }
        Role::Tool => json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": msg.tool_call_id,
                "content": msg.content,
            }],
        }),
        Role::System => unreachable!("system messages are extracted into the top-level field"),
    }
}

/// Normalize a Messages API response payload.
fn normalize(payload: &Value) -> Result<CompletionResponse, ModelError> {
    let content = payload
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ModelError::InvalidResponse("anthropic response missing content".to_string())
        })?;

    let mut text = String::new();
    let mut calls = Vec::new();
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    text.push_str(t);
                }
            }
            Some("tool_use") => {
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ModelError::InvalidResponse("tool_use block missing id".to_string())
                    })?
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ModelError::InvalidResponse("tool_use block missing name".to_string())
                    })?
                    .to_string();
                let input = block
                    .get("input")
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));
                if !input.is_object() {
                    return Err(ModelError::InvalidResponse(
                        "tool_use input must be a JSON object".to_string(),
                    ));
                }
                calls.push(ToolCall {
                    id,
                    name,
                    arguments: input,
                });
            }
            Some(other) => {
                return Err(ModelError::InvalidResponse(format!(
                    "anthropic content block of unknown type: {other}"
                )));
            }
            None => {
                return Err(ModelError::InvalidResponse(
                    "anthropic content block missing type".to_string(),
                ));
            }
        }
    }

    if !calls.is_empty() {
        return Ok(CompletionResponse::ToolCalls(calls));
    }

    let reason = match payload
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or("end_turn")
    {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "max_tokens" => FinishReason::Length,
        other => FinishReason::Other(other.to_string()),
    };
    Ok(CompletionResponse::Finish { reason, text })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolSpec;

    fn req() -> CompletionRequest {
        let mut r = CompletionRequest::new(
            "anthropic",
            "claude-sonnet-4-5",
            vec![
                Message::system("You are a research assistant."),
                Message::user("Write a file."),
            ],
        );
        r.tools = vec![ToolSpec {
            name: "filesystem".to_string(),
            description: "read/write files".to_string(),
            input_schema: json!({ "type": "object" }),
        }];
        r.temperature = Some(0.2);
        r.max_tokens = Some(2048);
        r
    }

    #[tokio::test]
    async fn finish_fixture_round_trips() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/messages")
            .match_header("x-api-key", "test-key")
            .match_header("anthropic-version", "2023-06-01")
            .match_body(mockito::Matcher::PartialJsonString(
                json!({
                    "model": "claude-sonnet-4-5",
                    "max_tokens": 2048,
                    "temperature": 0.2,
                    "system": "You are a research assistant.",
                })
                .to_string(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"content":[{"type":"text","text":"Hello!"}],"stop_reason":"end_turn"}"#)
            .create();

        let provider = AnthropicProvider::new(server.url(), Some("test-key".into()));
        let response = provider.complete(&req()).await.unwrap();
        match response {
            CompletionResponse::Finish { reason, text } => {
                assert_eq!(reason, FinishReason::Stop);
                assert_eq!(text, "Hello!");
            }
            other => panic!("expected finish, got {other:?}"),
        }
        mock.assert();
    }

    #[tokio::test]
    async fn tool_use_fixture_parses_input() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/messages")
            .match_header("x-api-key", "test-key")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"content":[
                    {"type":"text","text":"Let me check."},
                    {"type":"tool_use","id":"toolu_1","name":"filesystem","input":{"path":"/tmp/x"}},
                    {"type":"tool_use","id":"toolu_2","name":"http","input":{"url":"https://example.com"}}
                ],"stop_reason":"tool_use"}"#,
            )
            .create();

        let provider = AnthropicProvider::new(server.url(), Some("test-key".into()));
        let response = provider.complete(&req()).await.unwrap();
        match response {
            CompletionResponse::ToolCalls(calls) => {
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[0].id, "toolu_1");
                assert_eq!(calls[0].name, "filesystem");
                assert_eq!(calls[0].arguments["path"], "/tmp/x");
                assert_eq!(calls[1].name, "http");
            }
            other => panic!("expected tool calls, got {other:?}"),
        }
        mock.assert();
    }

    #[tokio::test]
    async fn tool_results_become_tool_result_blocks() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/messages")
            .match_body(mockito::Matcher::PartialJsonString(
                json!({ "messages": [
                    { "role": "user", "content": "Write a file." },
                    { "role": "assistant", "content": [
                        { "type": "tool_use", "id": "toolu_1", "name": "filesystem", "input": { "path": "/tmp/x" } }
                    ]},
                    { "role": "user", "content": [
                        { "type": "tool_result", "tool_use_id": "toolu_1", "content": "ok" }
                    ]}
                ] }).to_string(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}"#)
            .create();

        let mut r = req();
        r.messages.push(Message::assistant_with_tool_calls(
            "",
            vec![ToolCall {
                id: "toolu_1".into(),
                name: "filesystem".into(),
                arguments: json!({ "path": "/tmp/x" }),
            }],
        ));
        r.messages.push(Message::tool_result("toolu_1", "ok"));

        let provider = AnthropicProvider::new(server.url(), Some("test-key".into()));
        provider.complete(&r).await.unwrap();
        mock.assert();
    }

    #[tokio::test]
    async fn missing_key_is_auth_error() {
        let provider = AnthropicProvider::new("http://localhost:1", None);
        let err = provider.complete(&req()).await.unwrap_err();
        assert!(matches!(err, ModelError::Auth(_)));
    }

    #[tokio::test]
    async fn status_codes_map_to_errors() {
        for (status, expected) in [(401, "auth"), (429, "rate"), (500, "unavail")] {
            let mut server = mockito::Server::new_async().await;
            server
                .mock("POST", "/v1/messages")
                .with_status(status)
                .with_body("{}")
                .create();
            let provider = AnthropicProvider::new(server.url(), Some("k".into()));
            let err = provider.complete(&req()).await.unwrap_err();
            let kind = match &err {
                ModelError::Auth(_) => "auth",
                ModelError::RateLimited(_) => "rate",
                ModelError::Unavailable(_) => "unavail",
                other => panic!("unexpected error {other:?}"),
            };
            assert_eq!(kind, expected, "status {status}: {err:?}");
        }
    }

    #[tokio::test]
    async fn malformed_body_is_invalid_response() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/v1/messages")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"not": "a valid message response"}"#)
            .create();
        let provider = AnthropicProvider::new(server.url(), Some("k".into()));
        let err = provider.complete(&req()).await.unwrap_err();
        assert!(matches!(err, ModelError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn unknown_content_block_is_invalid_response() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/v1/messages")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"content":[{"type":"image","source":{}}],"stop_reason":"end_turn"}"#)
            .create();
        let provider = AnthropicProvider::new(server.url(), Some("k".into()));
        let err = provider.complete(&req()).await.unwrap_err();
        assert!(matches!(err, ModelError::InvalidResponse(_)));
    }
}
