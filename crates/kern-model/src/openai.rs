//! OpenAI Chat Completions adapter (ARCHITECTURE.md §8.2, decision D10).
//!
//! Raw `reqwest` call — no SDK. Environment: `OPENAI_API_KEY` (required),
//! `OPENAI_BASE_URL` (optional; default `https://api.openai.com/v1`) for
//! compatible gateways.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::{from_status, ModelError};
use crate::provider::ModelProvider;
use crate::types::{CompletionRequest, CompletionResponse, FinishReason, Message, Role, ToolCall};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
/// Guard against unbounded provider responses.
const MAX_RESPONSE_BYTES: u64 = 10 * 1024 * 1024;

pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl OpenAiProvider {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key,
        }
    }

    /// Build from the environment (`OPENAI_API_KEY`, `OPENAI_BASE_URL`).
    pub fn from_env() -> Self {
        let base_url =
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        Self::new(base_url, std::env::var("OPENAI_API_KEY").ok())
    }
}

#[async_trait]
impl ModelProvider for OpenAiProvider {
    fn id(&self) -> &str {
        "openai"
    }

    async fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, ModelError> {
        let key = self
            .api_key
            .as_deref()
            .ok_or_else(|| ModelError::Auth("OPENAI_API_KEY is not set".to_string()))?;

        let body = build_body(req);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {key}"))
            .json(&body)
            .send()
            .await
            .map_err(|e| ModelError::Unavailable(format!("openai transport error: {e}")))?;

        if !response.status().is_success() {
            return Err(from_status(response.status(), "openai"));
        }
        if let Some(len) = response.content_length() {
            if len > MAX_RESPONSE_BYTES {
                return Err(ModelError::InvalidResponse(format!(
                    "openai response exceeds {MAX_RESPONSE_BYTES} bytes"
                )));
            }
        }
        let payload: Value = response.json().await.map_err(|e| {
            ModelError::InvalidResponse(format!("openai response is not JSON: {e}"))
        })?;

        normalize(&payload)
    }
}

/// Serialize a normalized request into the OpenAI Chat Completions body.
fn build_body(req: &CompletionRequest) -> Value {
    let mut body = json!({
        "model": req.model,
        "messages": req.messages.iter().map(openai_message).collect::<Vec<_>>(),
    });
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect(),
        );
    }
    if let Some(temperature) = req.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(max_tokens) = req.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    body
}

fn openai_message(msg: &Message) -> Value {
    match msg.role {
        Role::System | Role::User => json!({ "role": msg.role_str(), "content": msg.content }),
        Role::Assistant => {
            let mut m = json!({ "role": "assistant", "content": msg.content });
            if !msg.tool_calls.is_empty() {
                m["tool_calls"] = Value::Array(
                    msg.tool_calls
                        .iter()
                        .map(|c| {
                            json!({
                                "id": c.id,
                                "type": "function",
                                "function": {
                                    "name": c.name,
                                    "arguments": serde_json::to_string(&c.arguments)
                                        .unwrap_or_else(|_| "{}".to_string()),
                                }
                            })
                        })
                        .collect(),
                );
            }
            m
        }
        Role::Tool => json!({
            "role": "tool",
            "tool_call_id": msg.tool_call_id,
            "content": msg.content,
        }),
    }
}

/// Normalize a Chat Completions response payload.
fn normalize(payload: &Value) -> Result<CompletionResponse, ModelError> {
    let message = payload.pointer("/choices/0/message").ok_or_else(|| {
        ModelError::InvalidResponse("openai response missing choices[0].message".to_string())
    })?;

    let text = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let mut calls = Vec::new();
    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for raw in tool_calls {
            let id = raw
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| ModelError::InvalidResponse("tool call missing id".to_string()))?
                .to_string();
            let function = raw.pointer("/function").ok_or_else(|| {
                ModelError::InvalidResponse("tool call missing function".to_string())
            })?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| ModelError::InvalidResponse("tool call missing name".to_string()))?
                .to_string();
            let arguments_raw = function
                .get("arguments")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ModelError::InvalidResponse("tool call missing arguments".to_string())
                })?;
            let arguments: Value = serde_json::from_str(arguments_raw).map_err(|e| {
                ModelError::InvalidResponse(format!("tool call arguments are not valid JSON: {e}"))
            })?;
            if !arguments.is_object() {
                return Err(ModelError::InvalidResponse(
                    "tool call arguments must be a JSON object".to_string(),
                ));
            }
            calls.push(ToolCall {
                id,
                name,
                arguments,
            });
        }
    }

    if !calls.is_empty() {
        return Ok(CompletionResponse::ToolCalls(calls));
    }

    let reason = match payload
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("stop")
    {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        other => FinishReason::Other(other.to_string()),
    };
    Ok(CompletionResponse::Finish { reason, text })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, ToolSpec};

    fn req() -> CompletionRequest {
        let mut r = CompletionRequest::new(
            "openai",
            "gpt-4o-mini",
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
            .mock("POST", "/v1/chat/completions")
            .match_header("authorization", "Bearer test-key")
            .match_body(mockito::Matcher::PartialJsonString(
                json!({ "model": "gpt-4o-mini", "temperature": 0.2, "max_tokens": 2048 })
                    .to_string(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"choices":[{"message":{"role":"assistant","content":"Hello!"},"finish_reason":"stop"}]}"#,
            )
            .create();

        let provider = OpenAiProvider::new(format!("{}/v1", server.url()), Some("test-key".into()));
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
    async fn tool_calls_fixture_parses_arguments() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .match_header("authorization", "Bearer test-key")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"choices":[{"message":{"role":"assistant","content":"","tool_calls":[
                    {"id":"call_1","type":"function","function":{"name":"filesystem","arguments":"{\"path\":\"/tmp/x\"}"}},
                    {"id":"call_2","type":"function","function":{"name":"http","arguments":"{\"url\":\"https://example.com\"}"}}
                ]},"finish_reason":"tool_calls"}]}"#,
            )
            .create();

        let provider = OpenAiProvider::new(format!("{}/v1", server.url()), Some("test-key".into()));
        let response = provider.complete(&req()).await.unwrap();
        match response {
            CompletionResponse::ToolCalls(calls) => {
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[0].id, "call_1");
                assert_eq!(calls[0].name, "filesystem");
                assert_eq!(calls[0].arguments["path"], "/tmp/x");
                assert_eq!(calls[1].name, "http");
            }
            other => panic!("expected tool calls, got {other:?}"),
        }
        mock.assert();
    }

    #[tokio::test]
    async fn tool_messages_are_serialized_with_call_id() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .match_body(mockito::Matcher::PartialJsonString(
                json!({ "messages": [
                    { "role": "system", "content": "You are a research assistant." },
                    { "role": "user", "content": "Write a file." },
                    { "role": "assistant", "content": "", "tool_calls": [{
                        "id": "call_1", "type": "function",
                        "function": { "name": "filesystem", "arguments": "{\"path\":\"/tmp/x\"}" }
                    }]},
                    { "role": "tool", "tool_call_id": "call_1", "content": "ok" }
                ] }).to_string(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#)
            .create();

        let mut r = req();
        r.messages.push(Message::assistant_with_tool_calls(
            "",
            vec![ToolCall {
                id: "call_1".into(),
                name: "filesystem".into(),
                arguments: json!({ "path": "/tmp/x" }),
            }],
        ));
        r.messages.push(Message::tool_result("call_1", "ok"));

        let provider = OpenAiProvider::new(format!("{}/v1", server.url()), Some("test-key".into()));
        provider.complete(&r).await.unwrap();
        mock.assert();
    }

    #[tokio::test]
    async fn missing_key_is_auth_error() {
        let provider = OpenAiProvider::new("http://localhost:1/v1", None);
        let err = provider.complete(&req()).await.unwrap_err();
        assert!(matches!(err, ModelError::Auth(_)));
    }

    #[tokio::test]
    async fn status_codes_map_to_errors() {
        for (status, expected) in [(401, "auth"), (429, "rate"), (500, "unavail")] {
            let mut server = mockito::Server::new_async().await;
            server
                .mock("POST", "/v1/chat/completions")
                .with_status(status)
                .with_body("{}")
                .create();
            let provider = OpenAiProvider::new(format!("{}/v1", server.url()), Some("k".into()));
            let err = provider.complete(&req()).await.unwrap_err();
            let kind = match &err {
                ModelError::Auth(_) => "auth",
                ModelError::RateLimited(_) => "rate",
                ModelError::Unavailable(_) => "unavail",
                other => {
                    panic!("unexpected error {other:?}");
                }
            };
            assert_eq!(kind, expected, "status {status}: {err:?}");
        }
    }

    #[tokio::test]
    async fn malformed_body_is_invalid_response() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"not": "a valid completion"}"#)
            .create();
        let provider = OpenAiProvider::new(format!("{}/v1", server.url()), Some("k".into()));
        let err = provider.complete(&req()).await.unwrap_err();
        assert!(matches!(err, ModelError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn non_json_body_is_invalid_response() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_header("content-type", "text/plain")
            .with_body("definitely not json")
            .create();
        let provider = OpenAiProvider::new(format!("{}/v1", server.url()), Some("k".into()));
        let err = provider.complete(&req()).await.unwrap_err();
        assert!(matches!(err, ModelError::InvalidResponse(_)));
    }
}
