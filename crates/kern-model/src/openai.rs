//! OpenAI Chat Completions adapter (ARCHITECTURE.md §8.2, decision D10).
//!
//! Raw `reqwest` call — no SDK. Environment: `OPENAI_API_KEY` (required),
//! `OPENAI_BASE_URL` (optional; default `https://api.openai.com/v1`) for
//! compatible gateways.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::{from_status, ModelError};
use crate::provider::ModelProvider;
use crate::types::{
    Completion, CompletionRequest, CompletionResponse, FinishReason, Message, Role, ToolCall, Usage,
};

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

    /// One Chat Completions call, carrying the provider's usage block when it
    /// sends one (`prompt_tokens`/`completion_tokens`).
    async fn fetch_completion(&self, req: &CompletionRequest) -> Result<Completion, ModelError> {
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

        Ok(Completion::new(normalize(&payload)?, usage_from(&payload)))
    }
}

#[async_trait]
impl ModelProvider for OpenAiProvider {
    fn id(&self) -> &str {
        "openai"
    }

    async fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, ModelError> {
        Ok(self.fetch_completion(req).await?.response)
    }

    async fn complete_counting(&self, req: &CompletionRequest) -> Result<Completion, ModelError> {
        self.fetch_completion(req).await
    }

    /// Stream the Chat Completions SSE stream (`stream: true`).
    ///
    /// Text deltas are forwarded as they arrive; tool-call fragments are
    /// accumulated by `index` (the id and name arrive once, the arguments in
    /// pieces). The turn is only complete once `[DONE]` — or the end of the
    /// body — arrives, and the accumulated result is what the engine records.
    async fn stream(
        &self,
        req: &CompletionRequest,
        deltas: &crate::provider::DeltaSender,
    ) -> Result<CompletionResponse, ModelError> {
        let key = self
            .api_key
            .as_deref()
            .ok_or_else(|| ModelError::Auth("OPENAI_API_KEY is not set".to_string()))?;

        let mut body = build_body(req);
        body["stream"] = json!(true);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let mut response = self
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

        let mut state = StreamState::default();
        // Buffer BYTES, not a String: a chunk boundary can split a multi-byte
        // character, and only complete lines are decoded.
        let mut buffer: Vec<u8> = Vec::new();
        let mut total = 0usize;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| ModelError::Unavailable(format!("openai stream transport error: {e}")))?
        {
            total += chunk.len();
            if total as u64 > MAX_RESPONSE_BYTES {
                return Err(ModelError::InvalidResponse(format!(
                    "openai stream exceeds {MAX_RESPONSE_BYTES} bytes"
                )));
            }
            buffer.extend_from_slice(&chunk);
            while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buffer.drain(..=end).collect();
                let line = String::from_utf8_lossy(&line);
                let Some(data) = line.trim().strip_prefix("data:") else {
                    continue; // blank lines, comments, event: fields
                };
                let data = data.trim();
                if data == "[DONE]" {
                    return state.finish();
                }
                if data.is_empty() {
                    continue;
                }
                let event: Value = serde_json::from_str(data).map_err(|e| {
                    ModelError::InvalidResponse(format!("openai stream event is not JSON: {e}"))
                })?;
                if let Some(error) = event.get("error") {
                    let message = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("stream error");
                    return Err(ModelError::Unavailable(format!(
                        "openai stream error: {message}"
                    )));
                }
                state.apply(&event, deltas)?;
            }
        }
        // A body that ends without `[DONE]` is still a complete turn.
        state.finish()
    }
}

/// Accumulates a Chat Completions stream into one normalized response.
#[derive(Default)]
struct StreamState {
    text: String,
    tool_calls: Vec<PartialToolCall>,
    finish_reason: Option<String>,
}

#[derive(Default, Clone)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl StreamState {
    fn apply(
        &mut self,
        event: &Value,
        deltas: &crate::provider::DeltaSender,
    ) -> Result<(), ModelError> {
        // Usage-only chunks carry no choices.
        let Some(choice) = event.pointer("/choices/0") else {
            return Ok(());
        };
        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                if !text.is_empty() {
                    self.text.push_str(text);
                    crate::provider::emit_delta(deltas, text);
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    while self.tool_calls.len() <= index {
                        self.tool_calls.push(PartialToolCall::default());
                    }
                    let slot = &mut self.tool_calls[index];
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        if !id.is_empty() {
                            slot.id = id.to_string();
                        }
                    }
                    if let Some(function) = call.get("function") {
                        if let Some(name) = function.get("name").and_then(Value::as_str) {
                            // The name arrives once; gateways that repeat it are
                            // tolerated by only taking the first non-empty value.
                            if slot.name.is_empty() {
                                slot.name = name.to_string();
                            }
                        }
                        if let Some(args) = function.get("arguments").and_then(Value::as_str) {
                            slot.arguments.push_str(args);
                        }
                    }
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
        Ok(())
    }

    fn finish(self) -> Result<CompletionResponse, ModelError> {
        let calls: Vec<&PartialToolCall> = self
            .tool_calls
            .iter()
            .filter(|c| !c.name.is_empty())
            .collect();
        if !calls.is_empty() {
            let mut parsed = Vec::with_capacity(calls.len());
            for (i, partial) in calls.iter().enumerate() {
                let arguments: Value = if partial.arguments.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&partial.arguments).map_err(|e| {
                        ModelError::InvalidResponse(format!(
                            "tool call arguments are not valid JSON: {e}"
                        ))
                    })?
                };
                if !arguments.is_object() {
                    return Err(ModelError::InvalidResponse(
                        "tool call arguments must be a JSON object".to_string(),
                    ));
                }
                parsed.push(ToolCall {
                    id: if partial.id.is_empty() {
                        format!("call_{i}")
                    } else {
                        partial.id.clone()
                    },
                    name: partial.name.clone(),
                    arguments,
                });
            }
            return Ok(CompletionResponse::ToolCalls(parsed));
        }
        let reason = match self.finish_reason.as_deref() {
            Some("length") => FinishReason::Length,
            Some("stop") | None => FinishReason::Stop,
            Some(other) => FinishReason::Other(other.to_string()),
        };
        Ok(CompletionResponse::Finish {
            reason,
            text: self.text,
        })
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

/// The provider's usage block, when present. OpenAI reports `prompt_tokens`
/// (older) or `input_tokens` (Responses-style compat gateways).
fn usage_from(payload: &Value) -> Usage {
    let Some(usage) = payload.get("usage") else {
        return Usage::default();
    };
    let input = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .or_else(|| usage.get("input_tokens").and_then(Value::as_u64));
    let output = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .or_else(|| usage.get("output_tokens").and_then(Value::as_u64));
    Usage::new(
        input.map(|v| v.min(u64::from(u32::MAX)) as u32),
        output.map(|v| v.min(u64::from(u32::MAX)) as u32),
    )
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

    /// A real SSE body: text deltas, then a tool call whose id/name arrive
    /// once and whose arguments arrive in pieces.
    const SSE_TEXT_AND_TOOL_CALLS: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo!\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"filesystem\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"/tmp/x\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    #[tokio::test]
    async fn stream_forwards_text_deltas_and_assembles_tool_calls() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .match_body(mockito::Matcher::PartialJsonString(
                json!({ "stream": true }).to_string(),
            ))
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(SSE_TEXT_AND_TOOL_CALLS)
            .create();

        let provider = OpenAiProvider::new(format!("{}/v1", server.url()), Some("k".into()));
        let (delta_tx, mut delta_rx) = crate::provider::delta_channel();
        let response = provider.stream(&req(), &delta_tx).await.unwrap();
        drop(delta_tx);
        let mut deltas: Vec<String> = Vec::new();
        while let Some(text) = delta_rx.recv().await {
            deltas.push(text);
        }

        assert_eq!(deltas, vec!["Hel".to_string(), "lo!".to_string()]);
        match response {
            CompletionResponse::ToolCalls(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "call_a");
                assert_eq!(calls[0].name, "filesystem");
                assert_eq!(calls[0].arguments["path"], "/tmp/x");
            }
            other => panic!("expected tool calls, got {other:?}"),
        }
        mock.assert();
    }

    #[tokio::test]
    async fn stream_without_tool_calls_finishes_with_the_joined_text() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(concat!(
                ": keep-alive comment\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\n",
            ))
            .create();

        let provider = OpenAiProvider::new(format!("{}/v1", server.url()), Some("k".into()));
        let (delta_tx, mut delta_rx) = crate::provider::delta_channel();
        let response = provider.stream(&req(), &delta_tx).await.unwrap();
        drop(delta_tx);
        let mut seen = String::new();
        while let Some(text) = delta_rx.recv().await {
            seen.push_str(&text);
        }
        assert_eq!(seen, "done");
        match response {
            CompletionResponse::Finish { reason, text } => {
                assert_eq!(reason, FinishReason::Stop);
                assert_eq!(text, "done");
            }
            other => panic!("expected finish, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stream_error_event_is_a_provider_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body("data: {\"error\":{\"message\":\"overloaded\"}}\n\n")
            .create();
        let provider = OpenAiProvider::new(format!("{}/v1", server.url()), Some("k".into()));
        let (delta_tx, _delta_rx) = crate::provider::delta_channel();
        let err = provider.stream(&req(), &delta_tx).await.unwrap_err();
        assert!(matches!(err, ModelError::Unavailable(_)), "{err:?}");
        assert!(err.to_string().contains("overloaded"), "{err}");
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
