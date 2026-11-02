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
//! - Extended-thinking blocks (`thinking` / `redacted_thinking`) carry no tool
//!   call and no answer text, so they are skipped rather than rejected; the
//!   normalized response is the same shape with or without them.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::{from_status, ModelError};
use crate::provider::ModelProvider;
use crate::stream::LineReader;
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

    fn api_key(&self) -> Result<&str, ModelError> {
        self.api_key
            .as_deref()
            .ok_or_else(|| ModelError::Auth("ANTHROPIC_API_KEY is not set".to_string()))
    }

    /// POST to `/v1/messages`, mapping transport and status failures.
    async fn post(&self, body: &Value) -> Result<reqwest::Response, ModelError> {
        let key = self.api_key()?;
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let response = self
            .client
            .post(&url)
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .json(body)
            .send()
            .await
            .map_err(|e| ModelError::Unavailable(format!("anthropic transport error: {e}")))?;
        if !response.status().is_success() {
            return Err(from_status(response.status(), "anthropic"));
        }
        Ok(response)
    }
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    async fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, ModelError> {
        let response = self.post(&build_body(req)).await?;
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

    /// Stream the Messages API SSE stream (`stream: true`).
    ///
    /// Text arrives as `text_delta` fragments; a `tool_use` block announces its
    /// id and name once, then streams its input as `input_json_delta` pieces
    /// that are joined and parsed at the end. Thinking deltas are dropped — the
    /// recorded turn is the answer and its tool calls (ARCHITECTURE §29).
    async fn stream(
        &self,
        req: &CompletionRequest,
        deltas: &crate::provider::DeltaSender,
    ) -> Result<CompletionResponse, ModelError> {
        let mut body = build_body(req);
        body["stream"] = json!(true);
        let response = self.post(&body).await?;

        let mut reader = LineReader::new(response, "anthropic");
        let mut state = StreamState::default();
        while let Some(line) = reader.next_line().await? {
            // Only `data:` lines carry payloads; `event:` and comments do not.
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() {
                continue;
            }
            let event: Value = serde_json::from_str(data).map_err(|e| {
                ModelError::InvalidResponse(format!("anthropic stream event is not JSON: {e}"))
            })?;
            match event.get("type").and_then(Value::as_str) {
                Some("error") => {
                    let message = event
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("stream error");
                    return Err(ModelError::Unavailable(format!(
                        "anthropic stream error: {message}"
                    )));
                }
                Some("message_stop") => return state.finish(),
                _ => state.apply(&event, deltas)?,
            }
        }
        // A body that ends without `message_stop` is still a complete turn.
        state.finish()
    }
}

/// One streamed `tool_use` block: the id and name arrive with the block start,
/// the input as JSON fragments.
#[derive(Default)]
struct PartialToolUse {
    id: String,
    name: String,
    json: String,
}

/// A content block seen while streaming, by index.
#[derive(Default)]
struct StreamState {
    text: String,
    blocks: std::collections::BTreeMap<usize, PartialToolUse>,
    stop_reason: Option<String>,
}

impl StreamState {
    fn apply(
        &mut self,
        event: &Value,
        deltas: &crate::provider::DeltaSender,
    ) -> Result<(), ModelError> {
        let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        match event.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                let Some(block) = event.get("content_block") else {
                    return Ok(());
                };
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                ModelError::InvalidResponse(
                                    "anthropic tool_use block missing id".to_string(),
                                )
                            })?
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                ModelError::InvalidResponse(
                                    "anthropic tool_use block missing name".to_string(),
                                )
                            })?
                            .to_string();
                        self.blocks.insert(
                            index,
                            PartialToolUse {
                                id,
                                name,
                                json: String::new(),
                            },
                        );
                    }
                    // A text block's content arrives as deltas; thinking blocks
                    // are skipped (see the module docs).
                    Some("text") | Some("thinking") | Some("redacted_thinking") => {}
                    Some(other) => {
                        return Err(ModelError::InvalidResponse(format!(
                            "anthropic content block of unknown type: {other}"
                        )))
                    }
                    None => {
                        return Err(ModelError::InvalidResponse(
                            "anthropic content block missing type".to_string(),
                        ))
                    }
                }
            }
            Some("content_block_delta") => {
                let Some(delta) = event.get("delta") else {
                    return Ok(());
                };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                self.text.push_str(text);
                                crate::provider::emit_delta(deltas, text);
                            }
                        }
                    }
                    Some("input_json_delta") => {
                        let Some(block) = self.blocks.get_mut(&index) else {
                            return Err(ModelError::InvalidResponse(
                                "anthropic input_json_delta without a tool_use block".to_string(),
                            ));
                        };
                        if let Some(fragment) = delta.get("partial_json").and_then(Value::as_str) {
                            block.json.push_str(fragment);
                        }
                    }
                    // `thinking_delta` / `signature_delta` carry no recorded
                    // content; unknown delta types are tolerated for forward
                    // compatibility rather than failing a live stream.
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(reason) = event.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(reason.to_string());
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(self) -> Result<CompletionResponse, ModelError> {
        if !self.blocks.is_empty() {
            let mut calls = Vec::with_capacity(self.blocks.len());
            for partial in self.blocks.into_values() {
                let arguments: Value = if partial.json.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&partial.json).map_err(|e| {
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
                calls.push(ToolCall {
                    id: partial.id,
                    name: partial.name,
                    arguments,
                });
            }
            return Ok(CompletionResponse::ToolCalls(calls));
        }
        let reason = match self.stop_reason.as_deref() {
            Some("max_tokens") => FinishReason::Length,
            Some("end_turn") | Some("stop_sequence") | None => FinishReason::Stop,
            Some(other) => FinishReason::Other(other.to_string()),
        };
        Ok(CompletionResponse::Finish {
            reason,
            text: self.text,
        })
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
            Some("text") | Some("thinking") | Some("redacted_thinking") => {
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

    /// A thinking block is not a reason to fail the turn: its signature and
    /// sealed content carry nothing the engine records.
    #[tokio::test]
    async fn thinking_blocks_are_skipped() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/v1/messages")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"content":[
                    {"type":"thinking","thinking":"hmm"},
                    {"type":"text","text":"answer"}
                ],"stop_reason":"end_turn"}"#,
            )
            .create();

        let provider = AnthropicProvider::new(server.url(), Some("k".into()));
        let response = provider.complete(&req()).await.unwrap();
        match response {
            CompletionResponse::Finish { text, .. } => assert_eq!(text, "answer"),
            other => panic!("expected finish, got {other:?}"),
        }
    }

    /// A real Messages SSE stream: text deltas, then a `tool_use` block whose
    /// input arrives as fragments, then the stop reason.
    const SSE_TEXT_AND_TOOL_USE: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"content\":[],\"stop_reason\":null}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: ping\n",
        "data: {\"type\":\"ping\"}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo!\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"filesystem\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"/tmp/x\\\"}\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    #[tokio::test]
    async fn stream_forwards_text_deltas_and_assembles_tool_use() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/messages")
            .match_header("x-api-key", "test-key")
            .match_body(mockito::Matcher::PartialJsonString(
                json!({ "stream": true }).to_string(),
            ))
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(SSE_TEXT_AND_TOOL_USE)
            .create();

        let provider = AnthropicProvider::new(server.url(), Some("test-key".into()));
        let (delta_tx, mut delta_rx) = crate::provider::delta_channel();
        let response = provider.stream(&req(), &delta_tx).await.unwrap();
        drop(delta_tx);
        let mut deltas: Vec<String> = Vec::new();
        while let Some(text) = delta_rx.recv().await {
            deltas.push(text);
        }

        // Thinking deltas are not surfaced as answer text.
        assert_eq!(deltas, vec!["Hel".to_string(), "lo!".to_string()]);
        match response {
            CompletionResponse::ToolCalls(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "toolu_1");
                assert_eq!(calls[0].name, "filesystem");
                assert_eq!(calls[0].arguments["path"], "/tmp/x");
            }
            other => panic!("expected tool calls, got {other:?}"),
        }
        mock.assert();
    }

    #[tokio::test]
    async fn stream_without_tool_use_finishes_with_the_joined_text() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/v1/messages")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"do\"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ne\"}}\n\n",
                "event: message_delta\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"}}\n\n",
                "event: message_stop\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            ))
            .create();

        let provider = AnthropicProvider::new(server.url(), Some("k".into()));
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
                assert_eq!(reason, FinishReason::Length);
                assert_eq!(text, "done");
            }
            other => panic!("expected finish, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stream_error_event_is_a_provider_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/v1/messages")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(concat!(
                "event: error\n",
                "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded\"}}\n\n",
            ))
            .create();
        let provider = AnthropicProvider::new(server.url(), Some("k".into()));
        let (delta_tx, _delta_rx) = crate::provider::delta_channel();
        let err = provider.stream(&req(), &delta_tx).await.unwrap_err();
        assert!(matches!(err, ModelError::Unavailable(_)), "{err:?}");
        assert!(err.to_string().contains("overloaded"), "{err}");
    }
}
