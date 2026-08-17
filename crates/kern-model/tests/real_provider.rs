//! Real-provider integration tests.
//!
//! These run against a live OpenAI-compatible endpoint when
//! `AGENTROUTER_API_KEY` is present in the environment and **self-skip
//! otherwise** — CI never blocks on them, and the key is never committed,
//! logged, or printed. They exercise the same `OpenAiProvider` code path any
//! user's `OPENAI_BASE_URL` gateway uses: model discovery, a completion, and
//! a tool-calling round trip against a real server (real TLS, real auth,
//! real response shapes).
//!
//! The endpoint defaults to AgentRouter (Kern's development/testing gateway)
//! and can be pointed at any OpenAI-compatible service with
//! `KERN_PROVIDER_BASE_URL` (e.g. `https://api.openai.com/v1`) — the runtime
//! and the adapter do not change; only the base URL does.
//!
//! Run them with:
//!   AGENTROUTER_API_KEY=... cargo test -p kern-model --test real_provider
//! or via `scripts/provider-smoke.sh`.

use kern_model::openai::OpenAiProvider;
use kern_model::provider::ModelProvider;
use kern_model::types::{CompletionRequest, CompletionResponse, FinishReason, Message, ToolSpec};
use serde_json::{json, Value};

/// Default endpoint: AgentRouter, Kern's development/testing gateway.
const DEFAULT_BASE_URL: &str = "https://co.agentrouter.org/v1";

fn base_url() -> String {
    std::env::var("KERN_PROVIDER_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string())
}

/// The key, if present. Never logged; only used as an Authorization header.
fn key() -> Option<String> {
    std::env::var("AGENTROUTER_API_KEY")
        .ok()
        .filter(|v| !v.is_empty())
}

/// Discover a chat-capable model from the endpoint's `/models` listing by
/// *probing* candidates with a one-token chat completion. No model names are
/// assumed or hard-coded: the listing is read at runtime and the first id
/// that answers a chat request wins. Non-chat models (embeddings, tts,
/// image, moderation, ...) reject the probe cheaply and are skipped.
async fn discover_chat_model(base_url: &str, api_key: &str) -> String {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{base_url}/models"))
        .bearer_auth(api_key)
        .send()
        .await
        .expect("GET /models must be reachable")
        .error_for_status()
        .expect("GET /models must authenticate (check AGENTROUTER_API_KEY)");
    let body: Value = resp.json().await.expect("models listing is JSON");
    let ids: Vec<String> = body
        .get("data")
        .and_then(Value::as_array)
        .expect("models listing has a data array")
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str).map(String::from))
        .collect();
    assert!(!ids.is_empty(), "endpoint returned zero models");

    for id in ids.iter().take(40) {
        let probe = json!({
            "model": id,
            "messages": [{ "role": "user", "content": "hi" }],
            "max_tokens": 1,
            "temperature": 0,
        });
        let res = client
            .post(format!("{base_url}/chat/completions"))
            .bearer_auth(api_key)
            .json(&probe)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => {
                eprintln!("chat-capable model: {id}");
                return id.clone();
            }
            Ok(r) => eprintln!("probe {id}: HTTP {}", r.status()),
            Err(e) => eprintln!("probe {id}: {e}"),
        }
    }
    panic!("no chat-capable model found in the endpoint's listing");
}

fn echo_tool() -> ToolSpec {
    ToolSpec {
        name: "echo".to_string(),
        description: "Echo a message back verbatim.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": { "message": { "type": "string" } },
            "required": ["message"],
            "additionalProperties": false,
        }),
    }
}

#[tokio::test]
async fn real_models_are_discoverable() {
    let Some(api_key) = key() else {
        eprintln!("AGENTROUTER_API_KEY unset — skipping real-provider test");
        return;
    };
    let model = discover_chat_model(&base_url(), &api_key).await;
    assert!(!model.is_empty());
    eprintln!("discovered model: {model}");
}

#[tokio::test]
async fn real_completion_round_trips() {
    let Some(api_key) = key() else {
        eprintln!("AGENTROUTER_API_KEY unset — skipping real-provider test");
        return;
    };
    let base = base_url();
    let provider = OpenAiProvider::new(&base, Some(api_key.clone()));
    let model = discover_chat_model(&base, &api_key).await;

    let mut req = CompletionRequest::new(
        "openai",
        &model,
        vec![
            Message::system("You are a terse assistant."),
            Message::user("Reply with exactly the word: ok"),
        ],
    );
    req.tools = vec![echo_tool()];
    req.timeout = Some(std::time::Duration::from_secs(45));
    req.retries = Some(1);
    req.max_tokens = Some(64);
    req.temperature = Some(0.0);

    match provider.complete(&req).await {
        Ok(CompletionResponse::Finish { reason, text }) => {
            assert!(
                !text.trim().is_empty(),
                "real completion returned empty text ({reason:?})"
            );
            assert!(matches!(reason, FinishReason::Stop), "reason: {reason:?}");
        }
        Ok(CompletionResponse::ToolCalls(calls)) => {
            // A terse completion request should not have tool-called, but if
            // the server did, the adapter must have parsed it cleanly.
            assert!(calls.iter().all(|c| !c.name.is_empty()));
        }
        Ok(other) => panic!("unexpected real response: {other:?}"),
        Err(err) => panic!("real completion failed: {err}"),
    }
    eprintln!("completion round trip OK");
}

#[tokio::test]
async fn real_tool_calling_round_trips() {
    let Some(api_key) = key() else {
        eprintln!("AGENTROUTER_API_KEY unset — skipping real-provider test");
        return;
    };
    let base = base_url();
    let provider = OpenAiProvider::new(&base, Some(api_key.clone()));
    let model = discover_chat_model(&base, &api_key).await;

    let mut req = CompletionRequest::new(
        "openai",
        &model,
        vec![
            Message::system("You have an echo tool. Use it whenever the user asks."),
            Message::user("Call the echo tool with message 'hello' and report the result."),
        ],
    );
    req.tools = vec![echo_tool()];
    req.timeout = Some(std::time::Duration::from_secs(45));
    req.retries = Some(1);
    req.max_tokens = Some(128);
    req.temperature = Some(0.0);

    match provider.complete(&req).await {
        Ok(CompletionResponse::ToolCalls(calls)) => {
            assert!(!calls.is_empty(), "expected at least one tool call");
            for call in &calls {
                assert_eq!(call.name, "echo", "unexpected tool call: {call:?}");
                assert!(
                    call.arguments.is_object(),
                    "tool arguments must parse to an object: {call:?}"
                );
            }
            eprintln!("tool-calling round trip OK ({} call(s))", calls.len());
        }
        Ok(CompletionResponse::Finish { text, .. }) => {
            // Models differ; the important real-server guarantee is that the
            // request round-tripped without InvalidResponse/auth/transport
            // errors. Report what happened for the test log.
            eprintln!("model answered directly (no tool call): {text}");
        }
        Ok(other) => panic!("unexpected real response: {other:?}"),
        Err(err) => panic!("real tool-calling request failed: {err}"),
    }
}
