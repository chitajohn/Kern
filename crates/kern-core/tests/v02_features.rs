//! v0.2.0 feature tests: token accounting, model routing, sub-agents,
//! credentials, and the MCP serving surface.
//!
//! These live outside the crate so they exercise exactly what an embedder can
//! reach through the public API.

use std::sync::Arc;

use kern_core::config::parse_agent_spec;
use kern_core::engine::{Engine, RunOutcome};
use kern_core::event::EventBus;
use kern_core::mcp_server::McpToolServer;
use kern_core::store::{Agent, LifecycleState, Store};
use kern_model::gateway::ModelGateway;
use kern_model::mock::{MockProvider, ScriptedStep};
use kern_model::types::ToolCall;
use kern_model::Usage;
use serde_json::{json, Value};

struct TestEnv {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    engine: Engine,
    provider: MockProvider,
}

impl TestEnv {
    fn new(script: Vec<ScriptedStep>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let bus = EventBus::new(Arc::clone(&store));
        let provider = MockProvider::new(script);
        let mut gateway = ModelGateway::new();
        gateway.register(Arc::new(provider.clone())).unwrap();
        let engine = Engine::new(Arc::clone(&store), bus, Arc::new(gateway), 8);
        Self {
            _dir: dir,
            store,
            engine,
            provider,
        }
    }

    fn create_agent(&self, yaml: &str) -> String {
        let spec = parse_agent_spec(yaml).expect("test spec must parse");
        let agent = Agent::new(
            spec.name.clone(),
            serde_json::to_value(&spec).unwrap(),
            LifecycleState::Created,
        );
        self.store.create_agent(&agent).unwrap();
        agent.id.clone()
    }
}

fn tool_call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: args,
    }
}

const NOOP_AGENT: &str = "version: 1\nname: counter\nmodel:\n  provider: mock\n  model: test\n\
                          tools:\n  - noop\n";

// ---------------------------------------------------------------------------
// Token accounting (§31)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn provider_usage_is_persisted_and_aggregated() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let bus = EventBus::new(Arc::clone(&store));
    let provider = MockProvider::new([ScriptedStep::Finish("counted".into())])
        .with_usage(Usage::new(Some(120), Some(30)));
    let mut gateway = ModelGateway::new();
    gateway.register(Arc::new(provider)).unwrap();
    let engine = Engine::new(Arc::clone(&store), bus, Arc::new(gateway), 8);

    let spec = parse_agent_spec(NOOP_AGENT).unwrap();
    let agent = Agent::new(
        spec.name.clone(),
        serde_json::to_value(&spec).unwrap(),
        LifecycleState::Created,
    );
    store.create_agent(&agent).unwrap();

    let summary = engine.run_agent(&agent.id, None).await.unwrap();
    let execution = store.get_execution(&summary.execution_id).unwrap();
    assert_eq!(execution.input_tokens, 120);
    assert_eq!(execution.output_tokens, 30);

    let totals = store.usage_totals(Some(&agent.id)).unwrap();
    assert_eq!(totals.input_tokens, 120);
    assert_eq!(totals.output_tokens, 30);
    assert_eq!(totals.total(), 150);
    assert_eq!(totals.executions, 1);

    // The global aggregate covers every agent.
    assert_eq!(store.usage_totals(None).unwrap().total(), 150);

    // The model.completed event carried the reported (not estimated) numbers.
    let event = store
        .events_after(0, 500)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "model.completed")
        .expect("model.completed");
    assert_eq!(event.payload["input_tokens"], 120);
    assert_eq!(event.payload["output_tokens"], 30);
    assert_eq!(event.payload["estimated"], false);
}

#[tokio::test]
async fn unreported_usage_is_estimated_rather_than_zeroed() {
    let env = TestEnv::new(vec![ScriptedStep::Finish("some answer".into())]);
    let agent_id = env.create_agent(NOOP_AGENT);
    let summary = env.engine.run_agent(&agent_id, None).await.unwrap();

    let execution = env.store.get_execution(&summary.execution_id).unwrap();
    assert!(
        execution.input_tokens > 0 && execution.output_tokens > 0,
        "estimation must fill both halves: {execution:?}"
    );
    let event = env
        .store
        .events_after(0, 500)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "model.completed")
        .expect("model.completed");
    assert_eq!(event.payload["estimated"], true);
}

// ---------------------------------------------------------------------------
// Model routing (§32)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn routing_escalates_the_model_mid_run() {
    let env = TestEnv::new(vec![
        ScriptedStep::ToolCalls(vec![tool_call("c1", "noop", json!({}))]),
        ScriptedStep::Finish("done".into()),
    ]);
    let agent_id = env.create_agent(
        "version: 1\nname: router\nmodel:\n  provider: mock\n  model: base\n\
         \x20 routing:\n    - if_step_above: 0\n      provider: mock\n      model: escalated\n\
         tools: [noop]\n",
    );

    env.engine.run_agent(&agent_id, None).await.unwrap();
    let requests = env.provider.take_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].model, "base");
    assert_eq!(requests[1].model, "escalated");
}

#[tokio::test]
async fn unconditional_routing_rule_must_be_last() {
    let err = parse_agent_spec(
        "version: 1\nname: bad-routing\nmodel:\n  provider: mock\n  model: m\n\
         \x20 routing:\n    - provider: mock\n      model: always\n\
         \x20   - if_step_above: 3\n      provider: mock\n      model: never\n\
         tools: [noop]\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("last rule"), "{err}");
}

// ---------------------------------------------------------------------------
// Sub-agents (§35)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delegation_runs_a_real_child_execution() {
    let env = TestEnv::new(vec![
        ScriptedStep::ToolCalls(vec![tool_call(
            "c1",
            "agent.helper",
            json!({ "input": "summarize the plan" }),
        )]),
        ScriptedStep::Finish("child result".into()),
        ScriptedStep::Finish("parent result".into()),
    ]);
    let agent_id = env.create_agent(
        "version: 1\nname: delegator\nmodel:\n  provider: mock\n  model: test\n\
         tools: [noop, agent.helper]\n\
         subagents:\n  - name: helper\n    description: helps\n    tools: [noop]\n",
    );

    let summary = env.engine.run_agent(&agent_id, None).await.unwrap();
    let RunOutcome::Completed { final_text, .. } = summary.outcome else {
        panic!("expected completion, got {:?}", summary.outcome);
    };
    assert_eq!(final_text, "parent result");

    let calls = env
        .store
        .tool_calls_for_execution(&summary.execution_id)
        .unwrap();
    assert_eq!(calls.len(), 1);
    let result = calls[0].result.as_ref().expect("recorded result");
    assert_eq!(result["result"], "child result");
    assert_eq!(result["agent"], "helper");

    // The child is a real agent while it runs, and is cleaned up afterwards.
    let agents = env.store.list_agents().unwrap();
    assert_eq!(agents.len(), 1, "child agent row must be removed");
    let child_completions = env
        .store
        .events_after(0, 500)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "execution.completed")
        .filter(|e| e.agent_id.as_deref() != Some(agent_id.as_str()))
        .count();
    assert_eq!(child_completions, 1, "the child ran its own execution");
}

#[tokio::test]
async fn subagent_capabilities_cannot_exceed_the_parent() {
    let err = parse_agent_spec(
        "version: 1\nname: escalator\nmodel:\n  provider: mock\n  model: m\n\
         tools: [noop, agent.helper]\n\
         subagents:\n  - name: helper\n    description: helps\n    tools: [http]\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("parent"), "{err}");
}

#[tokio::test]
async fn subagents_cannot_nest_and_must_be_declared() {
    let nested = parse_agent_spec(
        "version: 1\nname: nester\nmodel:\n  provider: mock\n  model: m\n\
         tools: [noop, agent.helper]\n\
         subagents:\n  - name: helper\n    description: helps\n    tools: [agent.other]\n",
    )
    .unwrap_err();
    assert!(nested.to_string().contains("nest"), "{nested}");

    let undeclared = parse_agent_spec(
        "version: 1\nname: ghostly\nmodel:\n  provider: mock\n  model: m\ntools: [noop, agent.ghost]\n",
    )
    .unwrap_err();
    assert!(undeclared.to_string().contains("subagents"), "{undeclared}");
}

// ---------------------------------------------------------------------------
// Credentials (§33)
// ---------------------------------------------------------------------------

#[test]
fn credential_grants_are_scoped_and_redacted() {
    let spec = parse_agent_spec(
        "version: 1\nname: creds\nmodel:\n  provider: mock\n  model: m\ntools: [noop]\n\
         credentials:\n  - name: MY_TOKEN\n    from: \"literal-secret\"\n",
    )
    .unwrap();
    let resolved = spec.resolve_credentials().unwrap();
    assert_eq!(resolved[0].shell_env().unwrap().0, "MY_TOKEN");
    assert!(resolved[0].http_header().is_none(), "shell-only grant");
    assert!(
        !format!("{resolved:?}").contains("literal-secret"),
        "Debug must redact the value"
    );

    let http = parse_agent_spec(
        "version: 1\nname: http-creds\nmodel:\n  provider: mock\n  model: m\ntools: [noop]\n\
         credentials:\n  - name: token\n    from: abc123\n    tools: [http]\n    header: X-Api-Key\n\
         \x20   prefix: \"Bearer \"\n",
    )
    .unwrap()
    .resolve_credentials()
    .unwrap();
    assert_eq!(
        http[0].http_header().unwrap(),
        ("X-Api-Key".to_string(), "Bearer abc123".to_string())
    );
    assert!(http[0].shell_env().is_none());
}

#[test]
fn credential_grants_reject_unsafe_shapes() {
    // header without the http target
    let err = parse_agent_spec(
        "version: 1\nname: bad-creds\nmodel:\n  provider: mock\n  model: m\ntools: [noop]\n\
         credentials:\n  - name: TOKEN\n    from: x\n    header: Authorization\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("header"), "{err}");

    // a name that is not a valid environment variable
    let err = parse_agent_spec(
        "version: 1\nname: bad-name\nmodel:\n  provider: mock\n  model: m\ntools: [noop]\n\
         credentials:\n  - name: my-token\n    from: x\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("environment variable"), "{err}");

    // duplicate grants
    let err = parse_agent_spec(
        "version: 1\nname: dup\nmodel:\n  provider: mock\n  model: m\ntools: [noop]\n\
         credentials:\n  - name: TOKEN\n    from: a\n  - name: TOKEN\n    from: b\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("duplicate"), "{err}");
}

// ---------------------------------------------------------------------------
// MCP serving surface (§36)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mcp_server_serves_tools_under_agent_policy() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("ws")).unwrap();
    std::fs::write(dir.path().join("ws/note.txt"), b"hello").unwrap();

    let spec = parse_agent_spec(
        "version: 1\nname: served\nmodel:\n  provider: mock\n  model: m\n\
         tools: [noop, filesystem]\n\
         permissions:\n  filesystem:\n    read:\n      allow: [\"./ws/**\"]\n",
    )
    .unwrap();
    let server = McpToolServer::build(&spec, dir.path()).unwrap();

    let init = server
        .handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }))
        .await
        .unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "kern");
    assert_eq!(init["result"]["capabilities"]["tools"], json!({}));

    let list = server
        .handle(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
        .await
        .unwrap();
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["noop", "filesystem"]);

    // An allowed read succeeds...
    let file = dir.path().join("ws/note.txt").to_string_lossy().to_string();
    let read = server
        .handle(&json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "filesystem", "arguments": { "action": "read", "path": file } }
        }))
        .await
        .unwrap();
    assert_eq!(read["result"]["isError"], false, "{read}");
    assert!(read["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("hello"));

    // ...and a path outside the granted root is denied by the same policy.
    let denied = server
        .handle(&json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": { "name": "filesystem", "arguments": { "action": "read", "path": "/etc/passwd" } }
        }))
        .await
        .unwrap();
    assert_eq!(denied["result"]["isError"], true);
    assert!(denied["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("PERMISSION_DENIED"));
}

#[tokio::test]
async fn mcp_server_refuses_stateful_tools() {
    let dir = tempfile::tempdir().unwrap();
    let spec = parse_agent_spec(
        "version: 1\nname: stateful\nmodel:\n  provider: mock\n  model: m\n\
         tools: [memory.read]\nmemory:\n  enabled: true\n\
         permissions:\n  memory:\n    read:\n      allow: [\"*\"]\n",
    )
    .unwrap();
    assert!(McpToolServer::build(&spec, dir.path()).is_err());
}
