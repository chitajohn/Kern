//! Engine integration tests: full agent runs against the mock
//! provider, exercising §8.1 end to end — happy path, tool batches, policy
//! deny/ask/resume, step limits, timeouts, model failures, request shape.

use std::time::{Duration, Instant};

use kern_model::mock::{MockProvider, ScriptedStep};
use kern_model::provider::ModelProvider;
use kern_model::types::{CompletionRequest, CompletionResponse};
use kern_model::{gateway::ModelGateway, ModelError, ToolCall as ModelToolCall};

use super::*;
use crate::config::parse_agent_spec;
use crate::store::{Agent, LifecycleState};

fn tool_call(id: &str, name: &str, args: Value) -> ModelToolCall {
    ModelToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: args,
    }
}

struct TestEnv {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    bus: EventBus,
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
        let engine = Engine::new(Arc::clone(&store), bus.clone(), Arc::new(gateway), 8);
        Self {
            _dir: dir,
            store,
            bus,
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

    fn events(&self) -> Vec<crate::store::Event> {
        self.store.events_after(0, 500).unwrap()
    }

    fn kinds(&self) -> Vec<String> {
        self.events().into_iter().map(|e| e.kind).collect()
    }
}

async fn wait_for_state(store: &Store, agent_id: &str, state: LifecycleState) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if store.get_agent(agent_id).unwrap().state == state {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "agent {agent_id} did not reach {state:?} within 5s"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------
// Happy path + request shape
// ---------------------------------------------------------------------------

#[tokio::test]
async fn happy_path_completes_with_correct_event_sequence() {
    let env = TestEnv::new(vec![ScriptedStep::Finish("hello world".into())]);
    let agent_id = env.create_agent(
        "version: 1\nname: happy\nmodel:\n  provider: mock\n  model: test\ntools:\n  - noop\n",
    );

    let summary = env.engine.run_agent(&agent_id, None).await.unwrap();
    let RunOutcome::Completed { final_text, steps } = &summary.outcome else {
        panic!("expected completion, got {:?}", summary.outcome);
    };
    assert_eq!(final_text, "hello world");
    assert_eq!(*steps, 1);

    assert_eq!(
        env.store.get_agent(&agent_id).unwrap().state,
        LifecycleState::Completed
    );
    let execution = env
        .store
        .list_executions_for_agent(&agent_id)
        .unwrap()
        .into_iter()
        .find(|e| e.id == summary.execution_id)
        .unwrap();
    assert_eq!(execution.status, ExecutionStatus::Completed);

    let kinds = env.kinds();
    let expected = [
        "execution.started",
        "agent.started",
        "checkpoint.created", // early checkpoint (first durable state)
        "model.requested",
        "model.completed",
        "checkpoint.created", // final checkpoint (§8.1 step 4)
        "execution.completed",
        "agent.completed",
    ];
    assert_eq!(&kinds, &expected, "event sequence drift: {kinds:?}");

    // The completed event reports the checkpoint count (early + final = 2).
    let completed = env
        .events()
        .into_iter()
        .find(|e| e.kind == "execution.completed")
        .expect("execution.completed event");
    assert_eq!(completed.payload["checkpoints"], 2);

    // Request shape: system prompt names the agent; only configured tools.
    let requests = env.provider.take_requests();
    assert_eq!(requests.len(), 1);
    let req = &requests[0];
    assert!(req.messages[0].content.contains("happy"));
    assert_eq!(req.tools.len(), 1);
    assert_eq!(req.tools[0].name, "noop");
}

#[tokio::test]
async fn request_carries_config_knobs_and_input() {
    let env = TestEnv::new(vec![ScriptedStep::Finish("ok".into())]);
    let agent_id = env.create_agent(
        "version: 1\nname: knobs\nmodel:\n  provider: mock\n  model: test\n  temperature: 0.7\n  max_tokens: 512\n  timeout: 5s\ntools:\n  - noop\nruntime:\n  model_retries: 1\n",
    );

    env.engine
        .run_agent(&agent_id, Some("write a poem"))
        .await
        .unwrap();
    let req = env.provider.take_requests().pop().unwrap();
    assert_eq!(req.provider, "mock");
    assert_eq!(req.model, "test");
    assert_eq!(req.temperature, Some(0.7));
    assert_eq!(req.max_tokens, Some(512));
    assert_eq!(req.timeout, Some(Duration::from_secs(5)));
    assert_eq!(req.retries, Some(1));
    // The run input is the first user message; the system prompt comes first.
    assert!(req.messages[1].content.contains("write a poem"));
}

// ---------------------------------------------------------------------------
// Tool loop and batches
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_loop_executes_and_feeds_results() {
    let env = TestEnv::new(vec![
        ScriptedStep::ToolCalls(vec![tool_call("c1", "noop", json!({}))]),
        ScriptedStep::Finish("done".into()),
    ]);
    let agent_id = env.create_agent(
        "version: 1\nname: loop\ntools:\n  - noop\nmodel:\n  provider: mock\n  model: test\n",
    );

    let summary = env.engine.run_agent(&agent_id, None).await.unwrap();
    let RunOutcome::Completed { final_text, steps } = &summary.outcome else {
        panic!("expected completion, got {:?}", summary.outcome);
    };
    assert_eq!(final_text, "done");
    assert_eq!(*steps, 2);

    let rows = env
        .store
        .tool_calls_for_execution(&summary.execution_id)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, ToolCallStatus::Completed);
    assert_eq!(rows[0].result.as_ref().unwrap()["ok"], true);

    let kinds = env.kinds();
    assert!(kinds.contains(&"tool.requested".to_string()));
    assert!(kinds.contains(&"tool.started".to_string()));
    assert!(kinds.contains(&"tool.completed".to_string()));
    let model_completed = kinds.iter().filter(|k| *k == "model.completed").count();
    assert_eq!(model_completed, 2);

    // The follow-up turn carries the tool result keyed by call id.
    let requests = env.provider.take_requests();
    assert_eq!(requests.len(), 2);
    let tool_messages: Vec<_> = requests[1]
        .messages
        .iter()
        .filter(|m| m.role == kern_model::Role::Tool)
        .collect();
    assert_eq!(tool_messages.len(), 1);
    assert_eq!(tool_messages[0].tool_call_id.as_deref(), Some("c1"));
    assert!(tool_messages[0].content.contains("ok"));
}

#[tokio::test]
async fn parallel_batch_runs_under_cap_and_all_complete() {
    // Per-agent cap 1 serializes the three 100ms sleeps: wall time proves
    // the concurrency bound, and every call still completes.
    let mut gateway = ModelGateway::new();
    let provider = MockProvider::new(vec![
        ScriptedStep::ToolCalls(vec![
            tool_call("c1", "sleep", json!({ "ms": 100 })),
            tool_call("c2", "sleep", json!({ "ms": 100 })),
            tool_call("c3", "noop", json!({})),
        ]),
        ScriptedStep::Finish("done".into()),
    ]);
    gateway.register(Arc::new(provider.clone())).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let bus = EventBus::new(Arc::clone(&store));
    let engine = Engine::new(Arc::clone(&store), bus, Arc::new(gateway), 8);

    let agent_id = {
        let spec = parse_agent_spec(
            "version: 1\nname: batch\ntools:\n  - noop\n  - sleep\nmodel:\n  provider: mock\n  model: test\nruntime:\n  max_concurrent_tools: 1\n",
        )
        .unwrap();
        let agent = Agent::new(
            spec.name.clone(),
            serde_json::to_value(&spec).unwrap(),
            LifecycleState::Created,
        );
        store.create_agent(&agent).unwrap();
        agent.id.clone()
    };

    let started = Instant::now();
    let summary = engine.run_agent(&agent_id, None).await.unwrap();
    let elapsed = started.elapsed();
    // Two serialized 100ms sleeps (noop is instant) ⇒ ≥ ~200ms.
    assert!(
        elapsed >= Duration::from_millis(180),
        "batch ran in {elapsed:?}"
    );

    let rows = store
        .tool_calls_for_execution(&summary.execution_id)
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|r| r.status == ToolCallStatus::Completed));
}

// ---------------------------------------------------------------------------
// Policy: deny, ask, resume
// ---------------------------------------------------------------------------

#[tokio::test]
async fn policy_deny_never_executes_the_tool() {
    // The engine denies `./ws/secret/x.txt` via the deny rule even though
    // the tool's own roots (from the allow rule `./`) would permit it —
    // proving the engine gate fires before execution.
    let env = TestEnv::new(vec![
        ScriptedStep::ToolCalls(vec![tool_call(
            "c1",
            "filesystem",
            json!({ "action": "read", "path": "./ws/secret/x.txt" }),
        )]),
        ScriptedStep::Finish("denied gracefully".into()),
    ]);
    let agent_id = env.create_agent(
        "version: 1\nname: deny\ntools:\n  - filesystem\nmodel:\n  provider: mock\n  model: test\npermissions:\n  filesystem:\n    read:\n      allow: [./]\n      deny: [./ws/secret]\n",
    );

    let summary = env.engine.run_agent(&agent_id, None).await.unwrap();
    let RunOutcome::Completed { final_text, .. } = &summary.outcome else {
        panic!("expected completion, got {:?}", summary.outcome);
    };
    assert_eq!(final_text, "denied gracefully");

    let rows = env
        .store
        .tool_calls_for_execution(&summary.execution_id)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, ToolCallStatus::Failed);
    assert_eq!(rows[0].error.as_ref().unwrap()["code"], "PERMISSION_DENIED");

    let kinds = env.kinds();
    assert!(kinds.contains(&"permission.denied".to_string()));
    assert!(!kinds.contains(&"tool.completed".to_string()));
}

#[tokio::test]
async fn ask_suspends_then_grant_resumes_and_executes() {
    let env = TestEnv::new(vec![
        ScriptedStep::ToolCalls(vec![tool_call(
            "c1",
            "filesystem",
            json!({ "action": "read", "path": "./ws/a.txt" }),
        )]),
        ScriptedStep::Finish("after-grant".into()),
    ]);
    let agent_id = env.create_agent(
        "version: 1\nname: asker\ntools:\n  - filesystem\nmodel:\n  provider: mock\n  model: test\npermissions:\n  filesystem:\n    read:\n      ask: [./]\n",
    );
    // Make the target readable so a granted call succeeds.
    let ws = env.store.data_dir().join("workspace/asker/ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "data").unwrap();

    let engine = env.engine.clone();
    let spawn_agent_id = agent_id.clone();
    let run = tokio::spawn(async move { engine.run_agent(&spawn_agent_id, None).await });

    // Sync point: wait until the spawned runner has registered in the task
    // registry, so the probe below deterministically races a *live* run.
    while !env.engine.is_running(&agent_id) {
        tokio::task::yield_now().await;
    }

    // A second concurrent run must be refused while the first is active.
    let err = env.engine.run_agent(&agent_id, None).await.unwrap_err();
    assert_eq!(err.code(), ErrorCode::ExecutionAlreadyActive);

    // The agent parks in waiting with a pending request.
    wait_for_state(&env.store, &agent_id, LifecycleState::Waiting).await;
    let pending = env.store.pending_permission_requests().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].agent_id, agent_id);
    // The request names the concrete target, not the matched rule.
    assert_eq!(pending[0].resource, "./ws/a.txt");

    // Grant it and resume.
    env.store
        .decide_permission_request(&pending[0].id, true)
        .unwrap();
    env.engine.resume_agent(&agent_id).await.unwrap();

    let summary: RunSummary = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("run must finish within 5s")
        .expect("runner task must not panic")
        .unwrap();
    let RunOutcome::Completed { final_text, .. } = &summary.outcome else {
        panic!("expected completion, got {:?}", summary.outcome);
    };
    assert_eq!(final_text, "after-grant");

    let rows = env
        .store
        .tool_calls_for_execution(&summary.execution_id)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, ToolCallStatus::Completed);
    assert_eq!(rows[0].result.as_ref().unwrap()["content"], "data");

    let kinds = env.kinds();
    assert!(kinds.contains(&"permission.asked".to_string()));
    assert!(kinds.contains(&"agent.waiting".to_string()));
    assert!(kinds.contains(&"permission.granted".to_string()));
    assert!(kinds.contains(&"agent.resumed".to_string()));
}

#[tokio::test]
async fn ask_denied_fed_to_model_and_run_continues() {
    let env = TestEnv::new(vec![
        ScriptedStep::ToolCalls(vec![tool_call(
            "c1",
            "filesystem",
            json!({ "action": "read", "path": "./ws/x.txt" }),
        )]),
        ScriptedStep::Finish("after-deny".into()),
    ]);
    let agent_id = env.create_agent(
        "version: 1\nname: askdeny\ntools:\n  - filesystem\nmodel:\n  provider: mock\n  model: test\npermissions:\n  filesystem:\n    read:\n      ask: [./]\n",
    );

    let engine = env.engine.clone();
    let spawn_agent_id = agent_id.clone();
    let run = tokio::spawn(async move { engine.run_agent(&spawn_agent_id, None).await });
    wait_for_state(&env.store, &agent_id, LifecycleState::Waiting).await;
    let pending = env.store.pending_permission_requests().unwrap();
    env.store
        .decide_permission_request(&pending[0].id, false)
        .unwrap();
    env.engine.resume_agent(&agent_id).await.unwrap();

    let summary: RunSummary = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("run must finish within 5s")
        .expect("runner task must not panic")
        .unwrap();
    let RunOutcome::Completed { final_text, .. } = &summary.outcome else {
        panic!("expected completion, got {:?}", summary.outcome);
    };
    assert_eq!(final_text, "after-deny");

    let rows = env
        .store
        .tool_calls_for_execution(&summary.execution_id)
        .unwrap();
    assert_eq!(rows[0].status, ToolCallStatus::Failed);
    assert_eq!(rows[0].error.as_ref().unwrap()["code"], "PERMISSION_DENIED");
    let kinds = env.kinds();
    assert!(kinds.contains(&"permission.denied".to_string()));
}

// ---------------------------------------------------------------------------
// Failure routing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn step_limit_fails_execution() {
    let env = TestEnv::new(vec![
        ScriptedStep::ToolCalls(vec![tool_call("c1", "noop", json!({}))]),
        ScriptedStep::ToolCalls(vec![tool_call("c2", "noop", json!({}))]),
        ScriptedStep::Finish("never reached".into()),
    ]);
    let agent_id = env.create_agent(
        "version: 1\nname: steppy\ntools:\n  - noop\nmodel:\n  provider: mock\n  model: test\nruntime:\n  max_steps: 2\n",
    );

    let summary = env.engine.run_agent(&agent_id, None).await.unwrap();
    let RunOutcome::Failed { error } = &summary.outcome else {
        panic!("expected failure, got {:?}", summary.outcome);
    };
    assert_eq!(error.code(), ErrorCode::StepLimitExceeded);
    assert_eq!(
        env.store.get_agent(&agent_id).unwrap().state,
        LifecycleState::Failed
    );
    assert!(env.store.get_agent(&agent_id).unwrap().last_error.is_some());

    let kinds = env.kinds();
    assert!(kinds.contains(&"execution.failed".to_string()));
    assert!(kinds.contains(&"agent.failed".to_string()));
}

#[tokio::test]
async fn tool_timeout_is_fed_to_model_and_run_continues() {
    let env = TestEnv::new(vec![
        ScriptedStep::ToolCalls(vec![tool_call("c1", "sleep", json!({ "ms": 5000 }))]),
        ScriptedStep::Finish("recovered".into()),
    ]);
    let agent_id = env.create_agent(
        "version: 1\nname: timeouty\ntools:\n  - sleep\nmodel:\n  provider: mock\n  model: test\nruntime:\n  tool_timeout: 50ms\n",
    );

    let summary = env.engine.run_agent(&agent_id, None).await.unwrap();
    let RunOutcome::Completed { final_text, .. } = &summary.outcome else {
        panic!("expected completion, got {:?}", summary.outcome);
    };
    assert_eq!(final_text, "recovered");

    let rows = env
        .store
        .tool_calls_for_execution(&summary.execution_id)
        .unwrap();
    assert_eq!(rows[0].status, ToolCallStatus::Failed);
    assert_eq!(rows[0].error.as_ref().unwrap()["code"], "TOOL_TIMEOUT");

    // The model saw the timeout as a tool result and still finished.
    let requests = env.provider.take_requests();
    let last = requests.last().unwrap();
    assert!(last
        .messages
        .iter()
        .any(|m| m.role == kern_model::Role::Tool && m.content.contains("TOOL_TIMEOUT")));
}

#[tokio::test]
async fn permanent_model_error_fails_the_run() {
    let env = TestEnv::new(vec![ScriptedStep::Fail(ModelError::Auth("bad key".into()))]);
    let agent_id = env.create_agent(
        "version: 1\nname: modelfail\ntools:\n  - noop\nmodel:\n  provider: mock\n  model: test\n",
    );

    let summary = env.engine.run_agent(&agent_id, None).await.unwrap();
    let RunOutcome::Failed { error } = &summary.outcome else {
        panic!("expected failure, got {:?}", summary.outcome);
    };
    assert_eq!(error.code(), ErrorCode::ModelAuth);
    assert_eq!(
        env.store.get_agent(&agent_id).unwrap().state,
        LifecycleState::Failed
    );
    let kinds = env.kinds();
    assert!(kinds.contains(&"model.failed".to_string()));
    assert!(kinds.contains(&"agent.failed".to_string()));
}

#[tokio::test]
async fn thinking_emits_agent_thinking_and_continues() {
    let env = TestEnv::new(vec![
        ScriptedStep::Thinking("let me think".into()),
        ScriptedStep::Finish("thought done".into()),
    ]);
    let agent_id = env.create_agent(
        "version: 1\nname: thinker\ntools:\n  - noop\nmodel:\n  provider: mock\n  model: test\n",
    );

    let summary = env.engine.run_agent(&agent_id, None).await.unwrap();
    let RunOutcome::Completed { final_text, steps } = &summary.outcome else {
        panic!("expected completion, got {:?}", summary.outcome);
    };
    assert_eq!(final_text, "thought done");
    assert_eq!(*steps, 2);
    let kinds = env.kinds();
    assert!(kinds.contains(&"agent.thinking".to_string()));
    // Thinking is not a state change: no checkpoint/execution events.
    assert!(!kinds.contains(&"execution.failed".to_string()));
}

// ---------------------------------------------------------------------------
// Memory digest + history bounding
// ---------------------------------------------------------------------------

#[tokio::test]
async fn memory_digest_is_injected_when_enabled() {
    let env = TestEnv::new(vec![ScriptedStep::Finish("ok".into())]);
    let agent_id = env.create_agent(
        "version: 1\nname: mem\ntools:\n  - noop\nmodel:\n  provider: mock\n  model: test\nmemory:\n  enabled: true\n  inject_digest: true\n",
    );
    env.store
        .memory_put(&agent_id, "goal", json!("ship it"), Some("primary"))
        .unwrap();

    env.engine.run_agent(&agent_id, None).await.unwrap();
    let req = env.provider.take_requests().pop().unwrap();
    assert!(
        req.messages[0].content.contains("goal"),
        "{}",
        req.messages[0].content
    );
    assert!(
        req.messages[0].content.contains("ship it"),
        "{}",
        req.messages[0].content
    );
}

// ---------------------------------------------------------------------------
// Dedup replay + graceful shutdown
// ---------------------------------------------------------------------------

#[tokio::test]
async fn restored_batch_replays_terminal_rows_without_reexecuting() {
    // Simulate a restored execution: the execution row exists and c1 is
    // terminal-completed from the ORIGINAL run. Resuming with the pending
    // batch [c1] must replay the recorded result — never re-run the tool —
    // and continue to completion.
    let env = TestEnv::new(vec![
        ScriptedStep::ToolCalls(vec![tool_call("c1", "noop", json!({}))]),
        ScriptedStep::Finish("done".into()),
    ]);
    let agent_id = env.create_agent(
        "version: 1\nname: dedup\ntools:\n  - noop\nmodel:\n  provider: mock\n  model: test\n",
    );
    // Drive the agent to `recovering` exactly as a daemon restart would:
    // started → running (with this execution) → interrupted → recovering.
    let lifecycle = crate::lifecycle::Lifecycle::new(Arc::clone(&env.store), env.bus.clone());
    lifecycle.start(&agent_id).await.unwrap();
    let execution = Execution::new(&agent_id, ExecutionStatus::Running);
    env.store.create_execution(&execution).unwrap();
    lifecycle
        .mark_started(&agent_id, &execution.id)
        .await
        .unwrap();
    lifecycle.recover(&agent_id).await.unwrap();

    let mut row = ToolCall::new("c1", &agent_id, &execution.id, "noop", json!({}));
    row.status = ToolCallStatus::Completed;
    row.result = Some(json!({ "ok": true, "from": "original" }));
    row.finished_at = Some(Utc::now());
    env.store.record_tool_call(&row).unwrap();

    let state = crate::checkpoint::SessionState {
        messages: vec![Message::user("the task")],
        history_trimmed: false,
        steps: 1,
        final_text: String::new(),
        checkpoints: 0,
        tool_calls: 0,
    };
    let summary = env
        .engine
        .resume_execution(
            &agent_id,
            &execution.id,
            state,
            vec![crate::checkpoint::PendingCall::new("c1", "noop", json!({}))],
            None,
            None,
        )
        .await
        .unwrap();
    let RunOutcome::Completed { final_text, .. } = &summary.outcome else {
        panic!("expected completion, got {:?}", summary.outcome);
    };
    assert_eq!(final_text, "done");

    // The tool never ran after restore: no tool.started, one terminal row.
    let kinds = env.kinds();
    assert!(
        !kinds.contains(&"tool.started".to_string()),
        "replayed call must not execute: {kinds:?}"
    );
    let rows = env.store.tool_calls_for_execution(&execution.id).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, ToolCallStatus::Completed);
    assert_eq!(rows[0].result.as_ref().unwrap()["from"], "original");

    // The model saw the replayed result keyed by the call id.
    let requests = env.provider.take_requests();
    assert_eq!(requests.len(), 2);
    let tool_messages: Vec<_> = requests[0]
        .messages
        .iter()
        .filter(|m| m.role == kern_model::Role::Tool)
        .collect();
    assert_eq!(tool_messages.len(), 1);
    assert!(tool_messages[0].content.contains("original"));
}

#[tokio::test]
async fn graceful_shutdown_checkpoints_and_pauses() {
    // Two batches so the runner is still inside a batch when shutdown lands;
    // at the next safe point it checkpoints and pauses instead of finishing.
    // Long sleeps keep the runner inside a batch when the flag lands (the
    // shutdown check happens at the next safe point, so the run MUST pause).
    let env = TestEnv::new(vec![
        ScriptedStep::ToolCalls(vec![tool_call("c1", "sleep", json!({ "ms": 800 }))]),
        ScriptedStep::ToolCalls(vec![tool_call("c2", "sleep", json!({ "ms": 800 }))]),
        ScriptedStep::Finish("never reached".into()),
    ]);
    let agent_id = env.create_agent(
        "version: 1\nname: shudowny\ntools:\n  - sleep\nmodel:\n  provider: mock\n  model: test\n",
    );

    let engine = env.engine.clone();
    let spawn_id = agent_id.clone();
    let run = tokio::spawn(async move { engine.run_agent(&spawn_id, None).await });
    while !env.engine.is_running(&agent_id) {
        tokio::task::yield_now().await;
    }
    // Mid-first-batch: signal shutdown; the runner pauses after the batch.
    tokio::time::sleep(Duration::from_millis(200)).await;
    env.engine.request_shutdown();

    let summary: RunSummary = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("run must finish within 5s")
        .expect("runner task must not panic")
        .unwrap();
    let RunOutcome::Paused { checkpoint_id } = &summary.outcome else {
        panic!("expected pause, got {:?}", summary.outcome);
    };
    assert_eq!(
        env.store.get_agent(&agent_id).unwrap().state,
        LifecycleState::Paused
    );
    // The pause references a real checkpoint.
    assert!(env.store.get_checkpoint(checkpoint_id).is_ok());
    assert!(env.kinds().contains(&"agent.paused".to_string()));
    assert!(env.kinds().contains(&"checkpoint.created".to_string()));
}

#[test]
fn history_is_bounded_and_never_drops_the_task() {
    let mut messages = vec![
        Message::user("the task"),
        Message::assistant("a"),
        Message::assistant("bb"),
        Message::assistant("ccc"),
        Message::assistant("dddd"),
    ];
    // 4 tokens ≈ 16 chars: 8 (task) + 1 + 2 + 3 + 4 = 18 exceeds it, so the
    // oldest assistant messages are dropped until the total fits.
    let trimmed = trim_messages(&mut messages, 4);
    assert!(trimmed);
    assert_eq!(messages[0].content, "the task", "task message must survive");
    assert_eq!(
        messages.len(),
        3,
        "kept task + newest fitting messages: {messages:?}"
    );
    assert_eq!(messages[1].content, "ccc");
    assert_eq!(messages[2].content, "dddd");

    // Small history that already fits trims nothing.
    let mut tiny = vec![Message::user("task"), Message::assistant("hi")];
    assert!(!trim_messages(&mut tiny, 10_000));
    assert_eq!(tiny.len(), 2);
}

/// Port-scoped network rules (`api.github.com:443`) must actually be
/// enforceable. Previously the engine stripped the port (`host_str()`) before
/// `evaluate_host`, so a port-bearing rule never matched and every request
/// was denied. The engine now passes `host:port` (default port filled from
/// the URL scheme), so port rules match exactly and port-less rules keep
/// matching any port.
#[test]
fn port_scoped_network_rules_are_enforced() {
    // Engine side: the exact target string the gate hands to the policy.
    assert_eq!(
        http_host_port("https://api.github.com/repos/x").as_deref(),
        Some("api.github.com:443")
    );
    assert_eq!(
        http_host_port("http://api.github.com/x").as_deref(),
        Some("api.github.com:80")
    );
    assert_eq!(
        http_host_port("https://api.github.com:8443/x").as_deref(),
        Some("api.github.com:8443")
    );
    assert_eq!(
        http_host_port("http://[::1]:8080/x").as_deref(),
        Some("[::1]"),
        "IPv6 with port keeps host-only form (fail-closed, documented)"
    );
    assert!(http_host_port("not a url").is_none());

    // Policy side: `host:port` targets against a port-scoped rule set.
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let spec = parse_agent_spec(
        "version: 1\nname: netport\nmodel:\n  provider: mock\n  model: test\ntools:\n  - http\npermissions:\n  network:\n    allow: [api.github.com:443]\n",
    )
    .unwrap();
    let perms =
        crate::permissions::PermissionEngine::from_config(&spec.permissions, &workspace).unwrap();
    assert!(
        perms.evaluate_host("api.github.com:443").is_allow(),
        "https (default :443) must be allowed by the port-scoped rule"
    );
    assert!(
        perms.evaluate_host("api.github.com:80").is_deny(),
        "http (default :80) must be denied by the port-scoped rule"
    );
    assert!(perms.evaluate_host("api.github.com:8443").is_deny());

    // A port-less rule keeps matching any port (unchanged semantics).
    let spec2 = parse_agent_spec(
        "version: 1\nname: netport2\nmodel:\n  provider: mock\n  model: test\ntools:\n  - http\npermissions:\n  network:\n    allow: [api.github.com]\n",
    )
    .unwrap();
    let perms2 =
        crate::permissions::PermissionEngine::from_config(&spec2.permissions, &workspace).unwrap();
    assert!(
        perms2.evaluate_host("api.github.com:443").is_allow(),
        "port-less rule must match any port"
    );
}

// ---------------------------------------------------------------------------
// Execution budget (max_duration, max_tool_calls)
// ---------------------------------------------------------------------------

/// A runaway provider: issues a fresh sleep tool call with a UNIQUE id every
/// turn (a fixed id would be dedup-replayed by the runtime and never sleep,
/// which is not the shape this test needs).
#[derive(Default)]
struct SleepLoopProvider {
    next: Arc<std::sync::atomic::AtomicU64>,
}

#[async_trait::async_trait]
impl ModelProvider for SleepLoopProvider {
    fn id(&self) -> &str {
        "mock"
    }
    async fn complete(
        &self,
        _req: &CompletionRequest,
    ) -> std::result::Result<CompletionResponse, ModelError> {
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(CompletionResponse::ToolCalls(vec![tool_call(
            &format!("c{n}"),
            "sleep",
            json!({ "ms": 30 }),
        )]))
    }
}

#[tokio::test]
async fn max_duration_bounds_a_looping_run() {
    // A runaway agent: every turn issues a real sleep tool call and keeps
    // going. Without a wall-clock cap it would loop until max_steps (100);
    // with `runtime.max_duration` the run must fail at the deadline.
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let bus = EventBus::new(Arc::clone(&store));
    let mut gateway = ModelGateway::new();
    gateway
        .register(Arc::new(SleepLoopProvider::default()))
        .unwrap();
    let engine = Engine::new(Arc::clone(&store), bus, Arc::new(gateway), 8);
    // Budget sized well under the natural run length (100 steps x 30ms sleep
    // + overhead ~= 3.5s+) but with enough headroom that a loaded parallel
    // test box can still finish the first iteration before the deadline.
    let spec = parse_agent_spec(
        "version: 1\nname: runaway\nmodel:\n  provider: mock\n  model: test\ntools:\n  - sleep\nruntime:\n  max_duration: 2s\n",
    )
    .unwrap();
    let agent = Agent::new(
        "runaway",
        serde_json::to_value(&spec).unwrap(),
        LifecycleState::Created,
    );
    store.create_agent(&agent).unwrap();

    let summary = engine.run_agent(&agent.id, None).await.unwrap();
    let RunOutcome::Failed { error } = &summary.outcome else {
        panic!("expected budget failure, got {:?}", summary.outcome);
    };
    assert_eq!(
        error.code(),
        ErrorCode::RunDurationExceeded,
        "expected RUN_DURATION_EXCEEDED, got {error}"
    );
    assert_eq!(
        store.get_agent(&agent.id).unwrap().state,
        LifecycleState::Failed
    );
    // The loop DID make progress before the cap — a budget is not a stall.
    let completed = store
        .events_after(0, 1000)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "model.completed")
        .count();
    assert!(
        completed >= 2,
        "run must make progress before the cap (model.completed = {completed})"
    );
}

#[tokio::test]
async fn max_tool_calls_bounds_issued_calls() {
    // One turn, three calls, budget of two: the batch is rejected BEFORE any
    // tool executes — no rows, no tool.started events, structured failure.
    let env = TestEnv::new(vec![ScriptedStep::ToolCalls(vec![
        tool_call("c1", "noop", json!({})),
        tool_call("c2", "noop", json!({})),
        tool_call("c3", "noop", json!({})),
    ])]);
    let agent_id = env.create_agent(
        "version: 1\nname: budget\nmodel:\n  provider: mock\n  model: test\ntools:\n  - noop\nruntime:\n  max_tool_calls: 2\n",
    );

    let summary = env.engine.run_agent(&agent_id, None).await.unwrap();
    let RunOutcome::Failed { error } = &summary.outcome else {
        panic!("expected budget failure, got {:?}", summary.outcome);
    };
    assert_eq!(
        error.code(),
        ErrorCode::ToolCallLimitExceeded,
        "expected TOOL_CALL_LIMIT_EXCEEDED, got {error}"
    );
    assert_eq!(
        env.store
            .tool_calls_for_execution(&summary.execution_id)
            .unwrap()
            .len(),
        0,
        "no tool call may execute past the budget"
    );
    assert!(
        env.kinds().iter().all(|k| k != "tool.started"),
        "no tool may start past the budget"
    );
    assert_eq!(
        env.store.get_agent(&agent_id).unwrap().state,
        LifecycleState::Failed
    );
}

// ---------------------------------------------------------------------------
// Runner panic containment
// ---------------------------------------------------------------------------

/// A broken provider adapter: panics on every completion (a bug in a provider
/// or in Kern must never strand an agent in `running`). Registered under the
/// `mock` id so the config enum accepts it.
struct PanicProvider;

#[async_trait::async_trait]
impl ModelProvider for PanicProvider {
    fn id(&self) -> &str {
        "mock"
    }
    async fn complete(
        &self,
        _req: &CompletionRequest,
    ) -> std::result::Result<CompletionResponse, ModelError> {
        panic!("deliberate provider panic for the containment test")
    }
}

#[tokio::test]
async fn runner_panic_fails_the_agent_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let bus = EventBus::new(Arc::clone(&store));
    let mut gateway = ModelGateway::new();
    gateway.register(Arc::new(PanicProvider)).unwrap();
    let engine = Engine::new(Arc::clone(&store), bus, Arc::new(gateway), 8);
    let spec = parse_agent_spec(
        "version: 1\nname: boom\nmodel:\n  provider: mock\n  model: test\ntools:\n  - noop\n",
    )
    .unwrap();
    let agent = Agent::new(
        "boom",
        serde_json::to_value(&spec).unwrap(),
        LifecycleState::Created,
    );
    store.create_agent(&agent).unwrap();

    // Must return a summary — never hang on a dead runner.
    let summary = engine.run_agent(&agent.id, None).await.expect("summary");
    let RunOutcome::Failed { error } = &summary.outcome else {
        panic!("expected failure, got {:?}", summary.outcome);
    };
    assert_eq!(
        error.code(),
        ErrorCode::RunnerPanic,
        "expected RUNNER_PANIC, got {error}"
    );
    assert_eq!(
        store.get_agent(&agent.id).unwrap().state,
        LifecycleState::Failed,
        "a panicked runner must not leave the agent running"
    );
    assert!(!engine.is_running(&agent.id), "registry must deregister");

    // The agent is not stuck: a fresh run starts a new execution.
    let second = engine
        .run_agent(&agent.id, None)
        .await
        .expect("second run must be allowed");
    assert!(matches!(second.outcome, RunOutcome::Failed { .. }));
    assert_eq!(
        store.list_executions_for_agent(&agent.id).unwrap().len(),
        2,
        "the panic must not leak the one-active-execution slot"
    );
    let failed_events = env_events(&store);
    assert!(
        failed_events
            .iter()
            .filter(|e| e.kind == "agent.failed")
            .count()
            >= 2
    );
}

fn env_events(store: &Store) -> Vec<crate::store::Event> {
    store.events_after(0, 1000).unwrap()
}

// ---------------------------------------------------------------------------
// Supervisor: runner-liveness sweep
// ---------------------------------------------------------------------------

/// Drive an agent to `running` with a started execution but NO runner task
/// (the failure mode the supervisor exists for: the row says running, the
/// runner is gone).
async fn drive_to_running_without_runner(env: &TestEnv, agent_id: &str) -> String {
    let lifecycle = crate::lifecycle::Lifecycle::new(Arc::clone(&env.store), env.bus.clone());
    lifecycle.start(agent_id).await.unwrap();
    let execution = Execution::new(agent_id, ExecutionStatus::Running);
    env.store.create_execution(&execution).unwrap();
    lifecycle
        .mark_started(agent_id, &execution.id)
        .await
        .unwrap();
    execution.id
}

#[tokio::test]
async fn supervisor_fails_a_running_agent_whose_runner_is_gone() {
    let env = TestEnv::new(vec![ScriptedStep::Finish("x".into())]);
    let agent_id = env.create_agent(
        "version: 1\nname: stuck\nmodel:\n  provider: mock\n  model: test\ntools:\n  - noop\n",
    );
    let execution_id = drive_to_running_without_runner(&env, &agent_id).await;

    // Grace zero: the started execution is immediately overdue.
    let summary = env
        .engine
        .supervisor_sweep(Duration::from_millis(0))
        .await
        .unwrap();
    assert_eq!(summary.failed, 1, "the orphaned execution must be failed");
    assert_eq!(
        env.store.get_agent(&agent_id).unwrap().state,
        LifecycleState::Failed
    );
    assert_eq!(
        env.store.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Failed
    );
    let failed = env
        .events()
        .into_iter()
        .find(|e| e.kind == "agent.failed")
        .expect("agent.failed event");
    assert_eq!(failed.payload["error"]["code"], "RUNNER_LOST");
    // The failure is structured and attributable (last_error carries it).
    assert!(env
        .store
        .get_agent(&agent_id)
        .unwrap()
        .last_error
        .unwrap_or_default()
        .contains("RUNNER_LOST"));
}

#[tokio::test]
async fn supervisor_fails_a_starting_agent_whose_runner_is_gone() {
    let env = TestEnv::new(vec![ScriptedStep::Finish("x".into())]);
    let agent_id = env.create_agent(
        "version: 1\nname: stuck-start\nmodel:\n  provider: mock\n  model: test\ntools:\n  - noop\n",
    );
    // start() moves created -> starting; the runner died before mark_started.
    // `start_agent` pre-creates the execution row, so it exists as `pending`;
    // the anchor falls back to the transition timestamp, so the sweep still
    // fails it after grace.
    let lifecycle = crate::lifecycle::Lifecycle::new(Arc::clone(&env.store), env.bus.clone());
    lifecycle.start(&agent_id).await.unwrap();
    let execution = Execution::new(&agent_id, ExecutionStatus::Pending);
    env.store.create_execution(&execution).unwrap();

    let summary = env
        .engine
        .supervisor_sweep(Duration::from_millis(0))
        .await
        .unwrap();
    assert_eq!(summary.failed, 1);
    assert_eq!(
        env.store.get_agent(&agent_id).unwrap().state,
        LifecycleState::Failed
    );
}

#[tokio::test]
async fn supervisor_skips_agents_within_the_grace_window() {
    let env = TestEnv::new(vec![ScriptedStep::Finish("x".into())]);
    let agent_id = env.create_agent(
        "version: 1\nname: fresh\nmodel:\n  provider: mock\n  model: test\ntools:\n  - noop\n",
    );
    let _execution_id = drive_to_running_without_runner(&env, &agent_id).await;

    let summary = env
        .engine
        .supervisor_sweep(Duration::from_secs(3600))
        .await
        .unwrap();
    assert_eq!(summary.failed, 0, "within grace: nothing may be failed");
    assert_eq!(
        env.store.get_agent(&agent_id).unwrap().state,
        LifecycleState::Running
    );
}

#[tokio::test]
async fn supervisor_skips_agents_with_a_live_runner() {
    let env = TestEnv::new(vec![
        ScriptedStep::ToolCalls(vec![tool_call("c1", "sleep", json!({ "ms": 400 }))]),
        ScriptedStep::Finish("done".into()),
    ]);
    let agent_id = env.create_agent(
        "version: 1\nname: live\ntools:\n  - sleep\nmodel:\n  provider: mock\n  model: test\n",
    );
    let handle = tokio::spawn({
        let engine = env.engine.clone();
        let agent_id = agent_id.clone();
        async move { engine.run_agent(&agent_id, None).await }
    });
    // The runner is alive and parked inside the sleep tool call; even a
    // zero-grace sweep must not touch it.
    wait_for_state(&env.store, &agent_id, LifecycleState::Running).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let summary = env
        .engine
        .supervisor_sweep(Duration::from_millis(0))
        .await
        .unwrap();
    assert_eq!(summary.failed, 0, "a live runner must never be failed");
    let summary = handle.await.unwrap().unwrap();
    assert!(matches!(summary.outcome, RunOutcome::Completed { .. }));
}

#[tokio::test]
async fn supervisor_fails_a_waiting_agent_whose_runner_is_gone() {
    let env = TestEnv::new(vec![ScriptedStep::Finish("x".into())]);
    let agent_id = env.create_agent(
        "version: 1\nname: asker-stuck\nmodel:\n  provider: mock\n  model: test\ntools:\n  - noop\n",
    );
    let _execution_id = drive_to_running_without_runner(&env, &agent_id).await;
    // A waiting agent's park poll (which seals expired requests) lives inside
    // the runner; a runner that vanished leaves the agent parked forever on
    // an undecided request. The supervisor must fail it, not wait on it.
    let lifecycle = crate::lifecycle::Lifecycle::new(Arc::clone(&env.store), env.bus.clone());
    let req = env
        .store
        .create_permission_request(&agent_id, Some("c1"), "./ws", "read")
        .unwrap();
    lifecycle
        .wait(&agent_id, &req.id, "./ws", "read")
        .await
        .unwrap();
    assert_eq!(
        env.store.get_agent(&agent_id).unwrap().state,
        LifecycleState::Waiting
    );

    let summary = env
        .engine
        .supervisor_sweep(Duration::from_millis(0))
        .await
        .unwrap();
    assert_eq!(
        summary.failed, 1,
        "an orphaned waiting agent must be failed"
    );
    assert_eq!(
        env.store.get_agent(&agent_id).unwrap().state,
        LifecycleState::Failed
    );
}

#[tokio::test]
async fn supervisor_skips_paused_agents() {
    let env = TestEnv::new(vec![ScriptedStep::Finish("x".into())]);
    let agent_id = env.create_agent(
        "version: 1\nname: parked\nmodel:\n  provider: mock\n  model: test\ntools:\n  - noop\n",
    );
    let execution_id = drive_to_running_without_runner(&env, &agent_id).await;
    // A deliberately paused agent keeps its running execution row but has no
    // runner — recovery owns it, the supervisor must NOT fail it.
    let lifecycle = crate::lifecycle::Lifecycle::new(Arc::clone(&env.store), env.bus.clone());
    lifecycle.pause(&agent_id, "cp-1").await.unwrap();
    assert_eq!(
        env.store.get_agent(&agent_id).unwrap().state,
        LifecycleState::Paused
    );

    let summary = env
        .engine
        .supervisor_sweep(Duration::from_millis(0))
        .await
        .unwrap();
    assert_eq!(summary.failed, 0, "paused agents are recovery's domain");
    assert_eq!(
        env.store.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Running
    );
}

// ---------------------------------------------------------------------------
// Durable wake/sleep — a sleep at or above
// `runtime.durable-sleep-min` parks the agent (runner unloaded, wake_at
// persisted); the scheduler wakes it later and the recorded result is
// replayed, never re-slept.
// ---------------------------------------------------------------------------

fn sleeper_spec() -> &'static str {
    "version: 1\nname: sleeper\nmodel:\n  provider: mock\n  model: test\ntools:\n  - sleep\nruntime:\n  durable_sleep_min: 1ms\n"
}

#[tokio::test]
async fn durable_sleep_parks_agent_and_persists_wake_at() {
    let env = TestEnv::new(vec![
        ScriptedStep::ToolCalls(vec![tool_call("s1", "sleep", json!({ "ms": 30_000 }))]),
        ScriptedStep::Finish("done".into()),
    ]);
    let agent_id = env.create_agent(sleeper_spec());

    let summary = env.engine.run_agent(&agent_id, None).await.unwrap();
    let RunOutcome::Sleeping { wake_at } = &summary.outcome else {
        panic!("expected sleeping outcome, got {:?}", summary.outcome);
    };
    assert!(
        wake_at > &chrono::Utc::now(),
        "wake time must be in the future (was {wake_at})"
    );

    // Lifecycle parked; execution still `running` with the wake time durable.
    assert_eq!(
        env.store.get_agent(&agent_id).unwrap().state,
        LifecycleState::Sleeping
    );
    let execution = env.store.get_execution(&summary.execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::Running);
    let persisted = execution.wake_at.expect("wake_at must be persisted");
    assert!(
        (*wake_at - persisted).num_seconds().abs() <= 1,
        "persisted wake_at {persisted} differs from the outcome {wake_at} (store truncates to seconds)"
    );

    // The sleep was recorded terminal, never executed in-process.
    let calls = env
        .store
        .tool_calls_for_execution(&summary.execution_id)
        .unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].tool_name, "sleep");
    assert_eq!(calls[0].status, crate::store::ToolCallStatus::Completed);
    assert_eq!(
        calls[0]
            .result
            .as_ref()
            .and_then(|r| r.get("slept"))
            .and_then(|v| v.as_bool()),
        Some(true),
        "the parked sleep must be recorded with its terminal result"
    );

    // Observable: agent.sleeping carries the wake time.
    let events = env.events();
    let sleeping = events
        .iter()
        .find(|e| e.kind == "agent.sleeping")
        .expect("agent.sleeping event");
    assert_eq!(
        sleeping.payload["wake_at"].as_str().unwrap(),
        wake_at.to_rfc3339()
    );

    // A sleeping agent has no runner — but it is NOT a lost runner: the
    // supervisor must leave it to the scheduler's wake path.
    let summary = env
        .engine
        .supervisor_sweep(Duration::from_millis(0))
        .await
        .unwrap();
    assert_eq!(summary.failed, 0, "sleeping agents are not runner-lost");
}

#[tokio::test]
async fn wake_resumes_and_replays_the_recorded_sleep() {
    let env = TestEnv::new(vec![
        ScriptedStep::ToolCalls(vec![tool_call("s1", "sleep", json!({ "ms": 30_000 }))]),
        ScriptedStep::Finish("done".into()),
    ]);
    let agent_id = env.create_agent(sleeper_spec());

    let summary = env.engine.run_agent(&agent_id, None).await.unwrap();
    let RunOutcome::Sleeping { .. } = &summary.outcome else {
        panic!("expected sleeping outcome, got {:?}", summary.outcome);
    };

    // Wake exactly as the scheduler does: restore the checkpoint, clear the
    // stale wake time, respawn the runner detached.
    let (state, pending, checkpoint_id, input) = env
        .engine
        .prepare_resume(&agent_id, &summary.execution_id)
        .await
        .unwrap();
    env.engine.spawn_resumed(
        &agent_id,
        &summary.execution_id,
        state,
        pending,
        checkpoint_id,
        input,
    );
    wait_for_state(&env.store, &agent_id, LifecycleState::Completed).await;

    let execution = env.store.get_execution(&summary.execution_id).unwrap();
    assert!(
        execution.wake_at.is_none(),
        "wake must clear the persisted wake time"
    );
    assert_eq!(execution.status, ExecutionStatus::Completed);
    let calls = env
        .store
        .tool_calls_for_execution(&summary.execution_id)
        .unwrap();
    assert_eq!(
        calls.len(),
        1,
        "the parked sleep must be replayed from the transcript, never re-executed \
         (re-executing a 30s sleep would hang the test)"
    );
    assert!(
        env.kinds().iter().any(|k| k == "agent.resumed"),
        "the wake path must emit agent.resumed"
    );
    let completed = env
        .store
        .list_executions_for_agent(&agent_id)
        .unwrap()
        .into_iter()
        .find(|e| e.id == summary.execution_id)
        .unwrap();
    assert_eq!(completed.status, ExecutionStatus::Completed);
}
