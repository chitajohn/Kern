//! Integration tests for the local HTTP API (`SPEC.md §15`) driven
//! end-to-end over real HTTP against an in-process axum server backed by a
//! real store + engine + mock provider.
//!
//! Covered: bearer auth, agent create/list/validation, lifecycle control
//! (start/pause/resume/terminate/checkpoint with §15.3 idempotency),
//! event replay, SSE replay→live handoff, the permission
//! ask→grant→resume flow, the execution transcript, and the
//! tools/models/health surface.

use std::sync::Arc;
use std::time::Duration;

use kern_core::api::{router, ApiState};
use kern_core::engine::Engine;
use kern_core::event::EventBus;
use kern_core::store::{ExecutionStatus, LifecycleState, Store};
use kern_model::gateway::ModelGateway;
use kern_model::mock::{MockProvider, ScriptedStep};
use kern_model::ToolCall;
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio_stream::StreamExt;

/// A live test server: real store + engine + mock gateway, served on an
/// ephemeral port. Dropping it aborts the server task.
struct TestServer {
    base: String,
    token: Option<String>,
    store: Arc<Store>,
    /// Kept alive so SSE live-forwarding (`shutdown.changed()`) works; the
    /// real daemon holds the sender for its whole lifetime.
    _shutdown_tx: tokio::sync::watch::Sender<bool>,
    _dir: tempfile::TempDir,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn spec(name: &str, tools: &[&str], extra: Value) -> Value {
    json!({
        "version": 1,
        "name": name,
        "model": { "provider": "mock", "model": "test" },
        "tools": tools,
    })
    .merge(extra)
}

/// Merge a JSON object into another (top-level keys overwrite).
trait Merge {
    fn merge(self, other: Value) -> Value;
}

impl Merge for Value {
    fn merge(mut self, other: Value) -> Value {
        if let (Some(base), Some(extra)) = (self.as_object_mut(), other.as_object()) {
            for (k, v) in extra {
                base.insert(k.clone(), v.clone());
            }
        }
        self
    }
}

async fn spawn_server(script: Vec<ScriptedStep>, token: Option<&str>) -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let bus = EventBus::new(Arc::clone(&store));
    let mut gateway = ModelGateway::new();
    gateway
        .register(Arc::new(MockProvider::new(script)))
        .unwrap();
    let gateway = Arc::new(gateway);
    let engine = Engine::new(Arc::clone(&store), bus.clone(), Arc::clone(&gateway), 8);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let state = ApiState {
        store: Arc::clone(&store),
        engine: engine.clone(),
        bus: bus.clone(),
        gateway,
        token: token.map(str::to_string),
        shutdown: shutdown_rx,
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    TestServer {
        base: format!("http://{addr}"),
        token: token.map(str::to_string),
        store,
        _shutdown_tx: shutdown_tx,
        _dir: dir,
        task,
    }
}

impl TestServer {
    fn client(&self) -> reqwest::Client {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(token) = &self.token {
            headers.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
            );
        }
        reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .unwrap()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    async fn create_agent(&self, spec: Value) -> (StatusCode, Value) {
        let resp = self
            .client()
            .post(self.url("/agents"))
            .json(&json!({ "spec": spec }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: Value = resp.json().await.unwrap();
        (status, body)
    }

    async fn wait_for_state(&self, agent_id: &str, state: LifecycleState) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if self.store.get_agent(agent_id).unwrap().state == state {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "agent {agent_id} did not reach {state:?} within 10s (at {:?}, last_error: {:?})",
                self.store.get_agent(agent_id).unwrap().state,
                self.store.get_agent(agent_id).unwrap().last_error
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bearer_token_is_required_when_configured() {
    let server = spawn_server(vec![ScriptedStep::Finish("hi".into())], Some("sekrit")).await;
    let client = server.client();

    // Without a token: 401 with the §13 shape.
    let resp = reqwest::Client::new()
        .get(server.url("/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "UNAUTHORIZED");
    assert!(body["message"].is_string());

    // Wrong token: 401.
    let wrong = reqwest::Client::new()
        .get(server.url("/health"))
        .header(reqwest::header::AUTHORIZATION, "Bearer wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    // Correct token: 200.
    let ok = client.get(server.url("/health")).send().await.unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    let body: Value = ok.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert!(body["version"].is_string());
    assert!(body["schema_version"].is_number());
}

#[tokio::test]
async fn api_is_open_when_no_token_configured() {
    let server = spawn_server(vec![ScriptedStep::Finish("hi".into())], None).await;
    let resp = reqwest::Client::new()
        .get(server.url("/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Agents: create, list, validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_list_and_get_agents() {
    let server = spawn_server(vec![ScriptedStep::Finish("hi".into())], None).await;
    let client = server.client();

    let (status, agent) = server
        .create_agent(spec("worker", &["noop"], json!({})))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(agent["name"], "worker");
    assert_eq!(agent["lifecycle_state"], "created");
    let agent_id = agent["id"].as_str().unwrap().to_string();

    // List contains it.
    let list: Value = client
        .get(server.url("/agents"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&agent_id.as_str()));

    // GET returns summary counts.
    let view: Value = client
        .get(server.url(&format!("/agents/{agent_id}")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(view["name"], "worker");
    assert_eq!(view["execution_count"], 0);
    assert_eq!(view["checkpoint_count"], 0);

    // Missing agent → 404 with the structured shape.
    let resp = client
        .get(server.url("/agents/does-not-exist"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["code"], "AGENT_NOT_FOUND");
}

#[tokio::test]
async fn invalid_specs_are_rejected_at_create() {
    let server = spawn_server(vec![ScriptedStep::Finish("hi".into())], None).await;
    let client = server.client();

    // Unknown tool name.
    let (status, err) = server
        .create_agent(spec("bad-tool", &["ghost"], json!({})))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["code"], "CONFIG_INVALID");

    // Unknown model provider.
    let (status, err) = server
        .create_agent(json!({
            "version": 1,
            "name": "bad-model",
            "model": { "provider": "nope", "model": "x" },
            "tools": ["noop"],
        }))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["code"], "CONFIG_INVALID");

    // Body name conflicting with spec name.
    let resp = client
        .post(server.url("/agents"))
        .json(&json!({ "name": "other", "spec": spec("mismatch", &["noop"], json!({})) }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Duplicate name → 409.
    let (status, _) = server
        .create_agent(spec("worker", &["noop"], json!({})))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, err) = server
        .create_agent(spec("worker", &["noop"], json!({})))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(err["code"], "AGENT_NAME_TAKEN");
}

// ---------------------------------------------------------------------------
// Lifecycle over HTTP
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_lifecycle_and_observability_flow() {
    let server = spawn_server(
        vec![
            ScriptedStep::Thinking("working on it".into()),
            ScriptedStep::Finish("all done".into()),
        ],
        None,
    )
    .await;
    let client = server.client();

    let (_, agent) = server
        .create_agent(spec("flow", &["noop"], json!({})))
        .await;
    let agent_id = agent["id"].as_str().unwrap().to_string();

    // start → 202 with execution id.
    let resp = client
        .post(server.url(&format!("/agents/{agent_id}/start")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let started: Value = resp.json().await.unwrap();
    let execution_id = started["execution_id"].as_str().unwrap().to_string();
    assert!(!execution_id.is_empty());

    // Idempotent: a second start while running is a 202 no-op with the same
    // active execution.
    let again: Value = client
        .post(server.url(&format!("/agents/{agent_id}/start")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(again["execution_id"], execution_id);

    // Run to completion (durably observed).
    server
        .wait_for_state(&agent_id, LifecycleState::Completed)
        .await;

    // Agent view: completed with counts.
    let view: Value = client
        .get(server.url(&format!("/agents/{agent_id}")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(view["lifecycle_state"], "completed");
    assert!(view["execution_count"].as_u64().unwrap() >= 1);

    // Executions history.
    let executions: Value = client
        .get(server.url(&format!("/agents/{agent_id}/executions")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let list = executions.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], execution_id);
    assert_eq!(list[0]["status"], "completed");

    // Execution detail with tool-call summary.
    let exec: Value = client
        .get(server.url(&format!("/executions/{execution_id}")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(exec["status"], "completed");
    assert!(exec["tool_calls"].is_array());

    // Agent event replay.
    let events: Value = client
        .get(server.url(&format!("/agents/{agent_id}/events")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let kinds: Vec<&str> = events
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"agent.started"));
    assert!(kinds.contains(&"agent.thinking"));
    assert!(kinds.contains(&"agent.completed"));

    // Transcript: the ordered record of the run.
    let transcript: Value = client
        .get(server.url(&format!("/executions/{execution_id}/transcript")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entries = transcript.as_array().unwrap();
    assert!(entries.iter().any(|e| e["kind"] == "execution.started"));
    assert!(entries
        .iter()
        .any(|e| e["kind"] == "agent.thinking" && e["role"] == "assistant"));
    let final_entry = entries
        .iter()
        .find(|e| e["kind"] == "agent.completed")
        .expect("transcript must contain agent.completed");
    assert_eq!(final_entry["content"], "all done");
    // Ordered by seq.
    assert!(entries
        .windows(2)
        .all(|w| w[0]["seq"].as_i64() < w[1]["seq"].as_i64()));

    // terminate on a completed agent is an idempotent 202.
    let resp = client
        .post(server.url(&format!("/agents/{agent_id}/terminate")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn invalid_transitions_are_409() {
    let server = spawn_server(vec![ScriptedStep::Finish("hi".into())], None).await;
    let client = server.client();

    let (_, agent) = server
        .create_agent(spec("stuck", &["noop"], json!({})))
        .await;
    let agent_id = agent["id"].as_str().unwrap().to_string();

    // resume on a created agent: not a §3.2 transition.
    let resp = client
        .post(server.url(&format!("/agents/{agent_id}/resume")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["code"], "INVALID_TRANSITION");

    // terminate on a created agent: also not in §3.2.
    let resp = client
        .post(server.url(&format!("/agents/{agent_id}/terminate")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // pause on a created agent: 409.
    let resp = client
        .post(server.url(&format!("/agents/{agent_id}/pause")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn pause_and_resume_roundtrip() {
    // A long tool call keeps the run alive long enough to pause mid-flight.
    let server = spawn_server(
        vec![
            ScriptedStep::ToolCalls(vec![ToolCall {
                id: "hold".into(),
                name: "sleep".into(),
                arguments: json!({ "ms": 2000 }),
            }]),
            ScriptedStep::Finish("done".into()),
        ],
        None,
    )
    .await;
    let client = server.client();

    let (_, agent) = server
        .create_agent(spec("pauser", &["sleep"], json!({})))
        .await;
    let agent_id = agent["id"].as_str().unwrap().to_string();
    client
        .post(server.url(&format!("/agents/{agent_id}/start")))
        .send()
        .await
        .unwrap();

    // Wait until the run is actually active, then pause it.
    server
        .wait_for_state(&agent_id, LifecycleState::Running)
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await; // enter the sleep tool
    let resp = client
        .post(server.url(&format!("/agents/{agent_id}/pause")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    server
        .wait_for_state(&agent_id, LifecycleState::Paused)
        .await;
    // Pause on paused is an idempotent 202.
    let resp = client
        .post(server.url(&format!("/agents/{agent_id}/pause")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Resume: restores the checkpoint and re-runs to completion.
    let resp = client
        .post(server.url(&format!("/agents/{agent_id}/resume")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    server
        .wait_for_state(&agent_id, LifecycleState::Completed)
        .await;

    // The session survived: the run produced exactly one execution row.
    let executions: Value = client
        .get(server.url(&format!("/agents/{agent_id}/executions")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(executions.as_array().unwrap().len(), 1);
    assert_eq!(executions.as_array().unwrap()[0]["status"], "completed");
}

#[tokio::test]
async fn manual_checkpoint_on_a_running_agent() {
    // The sleep holds the run open so the checkpoint request lands while the
    // runner is live; the checkpoint is written at the next safe point.
    let server = spawn_server(
        vec![
            ScriptedStep::ToolCalls(vec![ToolCall {
                id: "hold".into(),
                name: "sleep".into(),
                arguments: json!({ "ms": 1500 }),
            }]),
            ScriptedStep::Finish("done".into()),
        ],
        None,
    )
    .await;
    let client = server.client();

    let (_, agent) = server.create_agent(spec("cp", &["sleep"], json!({}))).await;
    let agent_id = agent["id"].as_str().unwrap().to_string();
    client
        .post(server.url(&format!("/agents/{agent_id}/start")))
        .send()
        .await
        .unwrap();
    server
        .wait_for_state(&agent_id, LifecycleState::Running)
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = client
        .post(server.url(&format!("/agents/{agent_id}/checkpoint")))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    assert!(
        status == StatusCode::CREATED || status == StatusCode::ACCEPTED,
        "checkpoint should land or be queued, got {status}"
    );
    let body: Value = resp.json().await.unwrap();
    if status == StatusCode::CREATED {
        assert!(body["checkpoint_id"].is_string());
    }

    // The checkpoint list endpoint exposes it (metadata, no payload).
    let checkpoints: Value = client
        .get(server.url(&format!("/agents/{agent_id}/checkpoints")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!checkpoints.as_array().unwrap().is_empty());
    assert!(checkpoints.as_array().unwrap()[0]["payload"].is_null());
    assert!(checkpoints.as_array().unwrap()[0]["seq"].is_number());
}

// ---------------------------------------------------------------------------
// SSE: replay → live handoff
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sse_replays_then_switches_to_live() {
    let server = spawn_server(
        vec![
            ScriptedStep::Finish("live!".into()),
            ScriptedStep::Finish("idle".into()),
        ],
        None,
    )
    .await;

    // Emit a durable event BEFORE connecting (the replay tail).
    let (_, agent) = server
        .create_agent(spec("streamy", &["noop"], json!({})))
        .await;
    let agent_id = agent["id"].as_str().unwrap().to_string();

    // Connect with after=0: replay delivers agent.created, then the run
    // started AFTER the connection arrives live.
    let mut stream = server
        .client()
        .get(server.url("/events/stream?after=0"))
        .send()
        .await
        .unwrap()
        .bytes_stream();
    let mut buf = String::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if buf.contains("event: agent.created") {
                return;
            }
            let chunk = stream.next().await.expect("stream must not end").unwrap();
            buf.push_str(&String::from_utf8_lossy(&chunk));
        }
    })
    .await
    .expect("timed out waiting for the replayed agent.created event");
    // Trigger a new event from the same connection's perspective.
    server
        .client()
        .post(server.url(&format!("/agents/{agent_id}/start")))
        .send()
        .await
        .unwrap();
    server
        .wait_for_state(&agent_id, LifecycleState::Completed)
        .await;

    // The live tail arrives on the SAME connection (no reconnect): the
    // completed event is delivered after the replay events.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if buf.contains("event: agent.completed") {
                return;
            }
            let chunk = stream.next().await.expect("stream must not end").unwrap();
            buf.push_str(&String::from_utf8_lossy(&chunk));
        }
    })
    .await
    .expect("timed out waiting for the live agent.completed event");
    // The envelope is the full §6 shape with a numeric seq.
    assert!(buf.contains("\"seq\":"));
    assert!(buf.contains("\"kind\":\"agent.completed\""));
}

// ---------------------------------------------------------------------------
// Permission ask → grant → resume over HTTP
// ---------------------------------------------------------------------------

#[tokio::test]
async fn permission_ask_grant_resume_flow() {
    // Two agents run on one shared mock provider, so the script carries a
    // full ask→decide cycle per agent: ToolCalls (ask) then Finish (after the
    // decision).
    let read_call = || ToolCall {
        id: "call-read".into(),
        name: "filesystem".into(),
        arguments: json!({ "action": "read", "path": "./workspace/x.txt" }),
    };
    let server = spawn_server(
        vec![
            ScriptedStep::ToolCalls(vec![read_call()]),
            ScriptedStep::Finish("read attempted".into()),
            ScriptedStep::ToolCalls(vec![read_call()]),
            ScriptedStep::Finish("granted and done".into()),
        ],
        None,
    )
    .await;
    let client = server.client();

    // `ask` on filesystem read: the batch must suspend in `waiting`.
    let (_, agent) = server
        .create_agent(spec(
            "asker",
            &["filesystem"],
            json!({
                // Relative rules resolve against the agent workspace root
                // (SPEC §10): `./` = ask for every read under it.
                "permissions": { "filesystem": { "read": { "ask": ["./"] } } }
            }),
        ))
        .await;
    let agent_id = agent["id"].as_str().unwrap().to_string();
    client
        .post(server.url(&format!("/agents/{agent_id}/start")))
        .send()
        .await
        .unwrap();
    server
        .wait_for_state(&agent_id, LifecycleState::Waiting)
        .await;

    // A pending request is exposed.
    let pending: Value = client
        .get(server.url("/permissions/pending"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let list = pending.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["agent_id"], agent_id);
    assert_eq!(list[0]["status"], "pending");
    assert!(list[0]["resource"].is_string());
    assert!(list[0]["action"].is_string());
    let request_id = list[0]["id"].as_str().unwrap().to_string();

    // Deny first — the agent resumes with a denial and completes.
    let resp = client
        .post(server.url(&format!("/permissions/{request_id}/deny")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    server
        .wait_for_state(&agent_id, LifecycleState::Completed)
        .await;
    let events = server.store.events_after(0, 10_000).unwrap();
    assert!(
        events.iter().any(|e| e.kind == "permission.denied"),
        "deny must emit permission.denied"
    );
    // The denied tool never executed.
    assert!(
        !events.iter().any(|e| e.kind == "tool.completed"),
        "denied tool must not execute"
    );
    // No pending requests remain.
    let pending: Value = client
        .get(server.url("/permissions/pending"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(pending.as_array().unwrap().is_empty());

    // Grant flow: a fresh run asks again; grant lets the tool execute.
    let (_, agent2) = server
        .create_agent(spec(
            "asker2",
            &["filesystem"],
            json!({
                // Relative rules resolve against the agent workspace root
                // (SPEC §10): `./` = ask for every read under it.
                "permissions": { "filesystem": { "read": { "ask": ["./"] } } }
            }),
        ))
        .await;
    let agent_id2 = agent2["id"].as_str().unwrap().to_string();
    client
        .post(server.url(&format!("/agents/{agent_id2}/start")))
        .send()
        .await
        .unwrap();
    server
        .wait_for_state(&agent_id2, LifecycleState::Waiting)
        .await;
    let pending: Value = client
        .get(server.url("/permissions/pending"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let request_id2 = pending.as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = client
        .post(server.url(&format!("/permissions/{request_id2}/grant")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    server
        .wait_for_state(&agent_id2, LifecycleState::Completed)
        .await;
    let events = server.store.events_after(0, 10_000).unwrap();
    assert!(
        events.iter().any(|e| e.kind == "permission.granted"),
        "grant must emit permission.granted"
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == "tool.completed" || e.kind == "tool.failed"),
        "granted tool must run (read of a missing file still executes)"
    );
}

// ---------------------------------------------------------------------------
// Capabilities: tools, models
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tools_and_models_catalog() {
    let server = spawn_server(vec![ScriptedStep::Finish("hi".into())], None).await;
    let client = server.client();

    let tools: Value = client
        .get(server.url("/tools"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in [
        "filesystem",
        "http",
        "memory.read",
        "memory.write",
        "memory.list",
        "noop",
        "sleep",
    ] {
        assert!(
            names.contains(&expected),
            "catalog missing {expected}: {names:?}"
        );
    }
    let filesystem = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "filesystem")
        .unwrap();
    assert_eq!(filesystem["permission"], "filesystem");
    assert!(filesystem["input_schema"].is_object());
    assert!(filesystem["description"].is_string());

    let models: Value = client
        .get(server.url("/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let providers: Vec<&str> = models
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["provider"].as_str().unwrap())
        .collect();
    assert!(providers.contains(&"mock"));
    let mock = models
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["provider"] == "mock")
        .unwrap();
    assert_eq!(mock["configured"], true);
    assert!(mock["models"].is_array());
}

/// Two clients race to start the same agent. Exactly one execution may exist and
/// the mock script must be consumed exactly once. A loser is either treated
/// idempotently (202 sharing the winner's execution id — the engine's
/// synchronous `is_running` registration) or rejected with 409
/// (`EXECUTION_ALREADY_ACTIVE` from the DB's one-active-execution
/// constraint, or `INVALID_TRANSITION` from the state CAS). It must NEVER
/// create a second execution or run the agent twice.
#[tokio::test]
async fn concurrent_starts_yield_exactly_one_execution() {
    let script = vec![
        ScriptedStep::ToolCalls(vec![ToolCall {
            id: "s1".into(),
            name: "sleep".into(),
            arguments: json!({ "ms": 3000 }),
        }]),
        ScriptedStep::Finish("done".into()),
    ];
    let server = spawn_server(script, None).await;
    let (status, body) = server
        .create_agent(spec("race", &["sleep"], json!({})))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let agent_id = body["id"].as_str().unwrap().to_string();

    let c1 = server.client();
    let c2 = server.client();
    let url = server.url(&format!("/agents/{agent_id}/start"));
    let (r1, r2) = tokio::join!(c1.post(&url).send(), c2.post(&url).send());
    let (s1, b1) = {
        let r = r1.unwrap();
        (r.status(), r.json::<Value>().await.unwrap())
    };
    let (s2, b2) = {
        let r = r2.unwrap();
        (r.status(), r.json::<Value>().await.unwrap())
    };

    let accepted = [s1, s2]
        .iter()
        .filter(|s| **s == StatusCode::ACCEPTED)
        .count();
    let conflicts = [s1, s2]
        .iter()
        .filter(|s| **s == StatusCode::CONFLICT)
        .count();
    assert!(
        accepted >= 1,
        "at least one start must be accepted ({s1}, {s2})"
    );
    assert!(
        accepted + conflicts == 2,
        "starts must be accepted or conflict, got {s1} and {s2}"
    );

    // Accepted responses must all reference ONE execution.
    let mut exec_ids: Vec<&str> = Vec::new();
    if s1 == StatusCode::ACCEPTED {
        exec_ids.push(b1["execution_id"].as_str().unwrap_or_default());
    }
    if s2 == StatusCode::ACCEPTED {
        exec_ids.push(b2["execution_id"].as_str().unwrap_or_default());
    }
    exec_ids.dedup();
    assert_eq!(
        exec_ids.len(),
        1,
        "accepted starts must share one execution id"
    );

    // Wait for the run to be live, then verify exactly one execution row.
    server
        .wait_for_state(&agent_id, LifecycleState::Running)
        .await;
    let executions = server.store.list_executions_for_agent(&agent_id).unwrap();
    assert_eq!(
        executions.len(),
        1,
        "a double start must never create a second execution"
    );
    assert_eq!(executions[0].status, ExecutionStatus::Running);
}

/// Malformed requests (§13): malformed JSON, unknown spec fields, and
/// oversized payloads must be rejected cleanly without corrupting runtime
/// state. The config parser is strict (`deny_unknown_fields`), and axum's
/// default 2 MiB body limit applies to the `Json` extractor.
#[tokio::test]
async fn malformed_and_oversized_requests_are_rejected() {
    let server = spawn_server(vec![ScriptedStep::Finish("hi".into())], None).await;

    // Malformed JSON → 400 with a structured body, no agent created.
    let resp = server
        .client()
        .post(server.url("/agents"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body("{ definitely not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: Value = resp.json().await.unwrap();
    assert!(body["code"].is_string(), "structured error: {body}");

    // Unknown spec field → 400 (strict config parse).
    let resp = server
        .client()
        .post(server.url("/agents"))
        .json(&json!({
            "spec": {
                "version": 1,
                "name": "x",
                "model": { "provider": "mock", "model": "t" },
                "bogus_field": 1
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Oversized body (> axum's 2 MiB default) → 413.
    let huge = format!("{{\"spec\":\"{}\"}}", "a".repeat(3 * 1024 * 1024));
    let send = server
        .client()
        .post(server.url("/agents"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(huge)
        .send();
    // When the server rejects the request mid-upload, Windows aborts the
    // connection (WSAECONNABORTED) instead of delivering the 413, so accept
    // the transport error as a rejection on Windows only. A transport error
    // can only mean the server killed the connection while the body was
    // still in flight — if the server accepted the body, the client would
    // receive a response — and the no-agent-created assertion below
    // independently proves the request had no effect.
    #[cfg(not(windows))]
    {
        let resp = send.await.unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
    #[cfg(windows)]
    {
        match send.await {
            Ok(resp) => assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE),
            Err(err) => assert!(
                err.is_connect() || err.is_body() || err.is_request(),
                "oversized upload must be rejected by the server: {err}"
            ),
        }
    }

    // None of the attacks created an agent.
    assert!(server.store.list_agents().unwrap().is_empty());
}
