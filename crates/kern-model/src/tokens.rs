//! Token estimation for providers that do not report usage.
//!
//! Kern does not ship per-provider tokenizers: they are large, license-bound,
//! and drift from the served model. Instead every count has a provenance —
//! `reported` (the provider's own `usage` block) or `estimated` (this module) —
//! and an estimate is only ever used to fill a half the provider left empty.
//!
//! The estimate is the classic byte-length approximation with a per-family
//! divisor, which is accurate to roughly ±15% on prose and deliberately
//! pessimistic on English (a 4.0 divisor undercounts CJK and code, so the
//! runtime's context budget is conservative rather than optimistic).

use crate::types::{CompletionRequest, Message, Usage};

/// Average characters per token for a model family.
///
/// Ordered most-specific first: Claude's tokenizer is slightly denser than the
/// OpenAI BPE families, and local Llama-family models sit between them.
pub fn chars_per_token(model: &str) -> f64 {
    let model = model.to_ascii_lowercase();
    if model.contains("claude") {
        3.5
    } else if model.contains("llama") || model.contains("mistral") || model.contains("qwen") {
        3.8
    } else {
        4.0
    }
}

/// Estimate the tokens in `text` for `model` (rounds up; empty is zero).
pub fn estimate_text(text: &str, model: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    let per_token = chars_per_token(model);
    let tokens = (text.chars().count() as f64 / per_token).ceil();
    tokens.min(f64::from(u32::MAX)) as u32
}

/// Per-message framing overhead (role markers, separators).
const MESSAGE_OVERHEAD_TOKENS: u32 = 4;
/// Per-tool-spec overhead (name, description framing).
const TOOL_OVERHEAD_TOKENS: u32 = 8;

fn estimate_message(message: &Message, model: &str) -> u32 {
    let mut tokens = MESSAGE_OVERHEAD_TOKENS + estimate_text(&message.content, model);
    for call in &message.tool_calls {
        tokens = tokens
            .saturating_add(estimate_text(&call.name, model))
            .saturating_add(estimate_text(&call.arguments.to_string(), model))
            .saturating_add(4);
    }
    if let Some(id) = &message.tool_call_id {
        tokens = tokens.saturating_add(estimate_text(id, model));
    }
    tokens
}

/// Estimate the tokens in a message list (the transcript size the runtime
/// budgets against).
pub fn estimate_messages(messages: &[Message], model: &str) -> u64 {
    messages
        .iter()
        .map(|m| u64::from(estimate_message(m, model)))
        .sum()
}

/// Estimate the prompt side of a request: messages plus advertised tool specs.
pub fn estimate_request(req: &CompletionRequest) -> Usage {
    let mut input: u32 = 0;
    for message in &req.messages {
        input = input.saturating_add(estimate_message(message, &req.model));
    }
    for tool in &req.tools {
        input = input
            .saturating_add(TOOL_OVERHEAD_TOKENS)
            .saturating_add(estimate_text(&tool.name, &req.model))
            .saturating_add(estimate_text(&tool.description, &req.model))
            .saturating_add(estimate_text(&tool.input_schema.to_string(), &req.model));
    }
    Usage::new(Some(input), None)
}

/// Estimate both halves for a turn whose provider reported nothing: the prompt
/// from the request, the response from its text and tool-call arguments.
pub fn estimate_turn(req: &CompletionRequest, output_text: &str) -> Usage {
    // Tool-call arguments are output tokens too; the caller passes their
    // serialized form through `output_text`, so nothing extra is added here.
    let output = estimate_text(output_text, &req.model);
    Usage::new(estimate_request(req).input_tokens, Some(output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, ToolCall, ToolSpec};
    use serde_json::json;

    #[test]
    fn model_families_pick_sensible_divisors() {
        assert_eq!(chars_per_token("claude-3-5-sonnet"), 3.5);
        assert_eq!(chars_per_token("llama3.1:8b"), 3.8);
        assert_eq!(chars_per_token("gpt-4o-mini"), 4.0);
        assert_eq!(chars_per_token("unknown-model"), 4.0);
    }

    #[test]
    fn estimates_scale_with_length_and_never_underflow() {
        assert_eq!(estimate_text("", "gpt-4o"), 0);
        assert_eq!(estimate_text("abcd", "gpt-4o"), 1);
        assert_eq!(estimate_text("abcde", "gpt-4o"), 2);
        assert_eq!(estimate_text("abcde", "claude-3"), 2); // ceil(5/3.5)
    }

    #[test]
    fn request_estimate_covers_messages_and_tools() {
        let mut req = CompletionRequest::new(
            "openai",
            "gpt-4o",
            vec![Message::system("be brief"), Message::user("do the thing")],
        );
        let without_tools = estimate_request(&req).input_tokens.unwrap();
        assert!(without_tools > 0);

        req.tools.push(ToolSpec {
            name: "noop".into(),
            description: "does nothing".into(),
            input_schema: json!({ "type": "object" }),
        });
        let with_tools = estimate_request(&req).input_tokens.unwrap();
        assert!(
            with_tools > without_tools,
            "tool specs must add prompt tokens"
        );
    }

    #[test]
    fn tool_calls_contribute_to_the_estimate() {
        let mut req = CompletionRequest::new("openai", "gpt-4o", vec![Message::user("hi")]);
        let base = estimate_request(&req).input_tokens.unwrap();

        req.messages.push(Message::assistant_with_tool_calls(
            "",
            vec![ToolCall {
                id: "c1".into(),
                name: "filesystem".into(),
                arguments: json!({ "action": "read", "path": "/tmp/x" }),
            }],
        ));
        assert!(estimate_request(&req).input_tokens.unwrap() > base);
    }

    #[test]
    fn usage_accumulates_and_fills_missing_halves() {
        let mut total = Usage::new(Some(10), Some(5));
        total.accumulate(Usage::new(Some(2), None));
        assert_eq!(total, Usage::new(Some(12), Some(5)));
        assert_eq!(total.total(), 17);

        let filled = Usage::new(None, Some(3)).or_estimate(Usage::new(Some(100), Some(200)));
        assert_eq!(filled, Usage::new(Some(100), Some(3)));

        // Nothing reported by either side stays unknown, never zero.
        assert_eq!(
            Usage::default().or_estimate(Usage::default()),
            Usage::default()
        );
    }
}
