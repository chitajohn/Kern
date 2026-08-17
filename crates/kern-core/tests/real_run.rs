//! Real-provider end-to-end runtime proof:
//! the full chain against a live OpenAI-compatible endpoint —
//!
//!   model request → provider adapter → engine → permission evaluation →
//!   tool execution → tool result → event persistence → checkpointing → completion
//!
//! Gated on `AGENTROUTER_API_KEY`; self-skips when absent. The endpoint
//! defaults to AgentRouter (Kern's development/testing gateway) and can be
//! pointed at any OpenAI-compatible service with `KERN_PROVIDER_BASE_URL`
//! (e.g. `https://api.openai.com/v1`) — the runtime never changes. The agent
//! spec uses the generic `openai` provider id with a base-URL override (the
//! exact code path a user's `OPENAI_BASE_URL` gateway uses); nothing in the
//! runtime knows the provider's name. The credential is only ever an
//! Authorization header.

use std::sync::Arc;
use std::time::Duration;

use kern_core::config::parse_agent_spec;
use kern_core::engine::{Engine, RunOutcome};
use kern_core::event::EventBus;
use kern_core::store::{Agent, LifecycleState, Store};
use kern_model::gateway::ModelGateway;
use kern_model::openai::OpenAiProvider;
use kern_model::provider::ModelProvider;
use serde_json::{json, Value};

const DEFAULT_BASE_URL: &str = "https://co.agentrouter.org/v1";

fn base_url() -> String {
    std::env::var("KERN_PROVIDER_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string())
}

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
        .expect("GET /models must authenticate");
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
            .timeout(Duration::from_secs(10))
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

/// The complete runtime chain against a real provider: the engine drives a
/// real model through a real filesystem tool call (write + read inside the
/// workspace), the permission engine authorizes it, the store persists the
/// events, and the run reaches a terminal state.
#[tokio::test]
async fn full_runtime_chain_with_a_real_provider() {
    let Some(api_key) = key() else {
        eprintln!("AGENTROUTER_API_KEY unset — skipping real-provider end-to-end run");
        return;
    };
    let base = base_url();
    let model = discover_chat_model(&base, &api_key).await;
    eprintln!("using real model: {model}");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Store::open(dir.path()).expect("open store"));
    let bus = EventBus::new(Arc::clone(&store));
    let mut gateway = ModelGateway::new();
    gateway
        .register(Arc::new(OpenAiProvider::new(&base, Some(api_key))))
        .expect("register openai adapter");
    let engine = Engine::new(Arc::clone(&store), bus, Arc::new(gateway), 4);

    let yaml = format!(
        "version: 1\n\
         name: real-run\n\
         model:\n  provider: openai\n  model: {model}\n\
         tools:\n  - filesystem\n\
         permissions:\n  filesystem:\n    write:\n      allow: [./workspace]\n    read:\n      allow: [./workspace]\n\
         runtime:\n  max_steps: 6\n  tool_timeout: 45s\n  checkpoint_interval: 2s\n  max_history_tokens: 4096\n"
    );
    let spec = parse_agent_spec(&yaml).expect("spec must parse");
    let agent = Agent::new(
        "real-run",
        serde_json::to_value(&spec).expect("spec serializes"),
        LifecycleState::Created,
    );
    store.create_agent(&agent).expect("create agent");
    let agent_id = agent.id.clone();

    let task =
        "Write a file named notes.txt inside your workspace containing exactly the word hello, \
                then read notes.txt back and report its contents.";
    let summary = engine
        .run_agent(&agent_id, Some(task))
        .await
        .unwrap_or_else(|e| panic!("real-provider run must not hit a runtime error: {e}"));

    match &summary.outcome {
        RunOutcome::Completed { final_text, steps } => {
            assert!(
                !final_text.trim().is_empty(),
                "final text must not be empty"
            );
            assert!(
                *steps >= 2,
                "a write+read task needs at least 2 turns, got {steps}"
            );
        }
        RunOutcome::Failed { .. } => {
            // The model may misbehave (wrong tool, no tool, step limit) —
            // that is model behavior, not a runtime defect. The runtime
            // guarantees are below; failure is only a violation if the
            // events/state are wrong.
            eprintln!("run ended Failed (model behavior); asserting runtime guarantees");
        }
        RunOutcome::Paused { .. } => panic!("run must not pause without a shutdown signal"),
        RunOutcome::Sleeping { .. } => {
            panic!("run must not park on a durable sleep without a sleep tool")
        }
    }

    let kinds: Vec<String> = store
        .events_after(0, 500)
        .expect("events readable")
        .into_iter()
        .map(|e| e.kind)
        .collect();

    // 1. A real model turn happened and events were persisted durably.
    assert!(kinds.contains(&"execution.started".to_string()));
    assert!(
        kinds.contains(&"agent.completed".to_string())
            || kinds.contains(&"agent.failed".to_string()),
        "run must reach a terminal state, events: {kinds:?}"
    );
    // 2. The tool chain executed: requested → executed → result recorded.
    assert!(
        kinds.contains(&"tool.requested".to_string()),
        "tool.requested missing: {kinds:?}"
    );
    assert!(
        kinds.contains(&"tool.completed".to_string()),
        "tool.completed missing: {kinds:?}"
    );
    // 3. The durable store recorded checkpoints during the run.
    assert!(
        kinds.contains(&"checkpoint.created".to_string()),
        "checkpoint.created missing: {kinds:?}"
    );
    // 4. The tool-call rows persisted with terminal statuses.
    let calls = store
        .tool_calls_for_execution(&summary.execution_id)
        .expect("tool calls");
    assert!(!calls.is_empty(), "at least one tool call row must persist");
    assert!(
        calls.iter().all(
            |c| c.status == kern_core::store::model::ToolCallStatus::Completed
                || c.status == kern_core::store::model::ToolCallStatus::Failed
        ),
        "every executed tool call must be terminal: {calls:?}"
    );

    // 5. The agent reached a terminal lifecycle state.
    let state = store.get_agent(&agent_id).expect("agent").state;
    assert!(
        matches!(state, LifecycleState::Completed | LifecycleState::Failed),
        "agent must be terminal, got {state:?}"
    );
    eprintln!(
        "full runtime chain OK ({} events, {} tool rows)",
        kinds.len(),
        calls.len()
    );
}

/// Provider-error handling against the real server: an invalid key must
/// surface as `ModelError::Auth` (permanent — the gateway never retries it),
/// not as a transport error or a panic.
#[tokio::test]
async fn bad_key_is_auth_error_against_the_real_server() {
    // Deliberately wrong key — never the configured one.
    let provider = OpenAiProvider::new(base_url(), Some("sk-invalid-for-test".to_string()));
    let mut req = kern_model::types::CompletionRequest::new(
        "openai",
        "gpt-4o-mini",
        vec![kern_model::types::Message::user("hi")],
    );
    req.timeout = Some(Duration::from_secs(30));
    req.retries = Some(0);
    let err = provider
        .complete(&req)
        .await
        .expect_err("bad key must fail");
    assert!(
        matches!(err, kern_model::error::ModelError::Auth(_)),
        "expected Auth, got {err:?}"
    );
    eprintln!("bad-key auth error surfaced correctly");
}

/// Model discovery through the public adapter surface (not a raw client): the
/// `/models` endpoint round-trips and yields at least one usable chat id.
#[tokio::test]
async fn real_model_listing_is_discoverable() {
    let Some(api_key) = key() else {
        eprintln!("AGENTROUTER_API_KEY unset — skipping real-provider test");
        return;
    };
    let model = discover_chat_model(&base_url(), &api_key).await;
    assert!(!model.is_empty());
    eprintln!("discovered model: {model}");
}

/// A deliberately short timeout must surface as `ModelError::Timeout` from the
/// gateway (the engine's cancel path), not hang the test forever.
#[tokio::test]
async fn gateway_timeout_surfaces_with_a_real_provider() {
    let Some(api_key) = key() else {
        eprintln!("AGENTROUTER_API_KEY unset — skipping real-provider test");
        return;
    };
    let base = base_url();
    let model = discover_chat_model(&base, &api_key).await;
    let mut gateway = ModelGateway::new();
    gateway
        .register(Arc::new(OpenAiProvider::new(&base, Some(api_key))))
        .expect("register");
    let mut req = kern_model::types::CompletionRequest::new(
        "openai",
        &model,
        vec![kern_model::types::Message::user("hi")],
    );
    req.timeout = Some(Duration::from_millis(1));
    req.retries = Some(0);
    let started = std::time::Instant::now();
    let err = gateway.complete(&req).await.expect_err("must time out");
    assert!(
        matches!(err, kern_model::error::ModelError::Timeout(_)),
        "expected Timeout, got {err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "timeout must return promptly"
    );
    eprintln!("gateway timeout surfaced correctly");
}
