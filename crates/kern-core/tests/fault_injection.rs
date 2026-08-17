//! Deterministic fault injection — the fault matrix (see `crate::fault`).
//!
//! The SIGKILL recovery test proves crash recovery with a real kill at a
//! hostile moment; it is a black box. This suite is the white-box complement:
//! every recovery-relevant *persisted-write boundary* of the store is
//! scripted to fail at a chosen occurrence count, and the same scripted agent
//! is run against each. The invariants asserted after every injection:
//!
//! - the run returns — a storage failure never hangs the runtime;
//! - the agent ends in a **valid** lifecycle state, never a split-brain row;
//! - failure is **structured and observable** (`agent.last_error`,
//!   `execution.failed` event) wherever the terminal transition could apply;
//! - a tool call whose terminal row was durably recorded is **never
//!   re-executed** on recovery (exactly-once per recorded row);
//! - a tool call whose terminal row write failed is re-executed once on
//!   recovery — the documented at-least-once crash window, asserted as such,
//!   not hidden;
//! - per-agent event sequences stay strictly increasing;
//! - when the failure lands *inside* the fail transition itself, the
//!   supervisor sweep (`RUNNER_LOST`) is the mechanism that restores
//!   consistency — asserted, because that is the designed backstop.
//!
//! Recovery is driven the way production drives it: the agent is marked
//! `recovering` (what a daemon restart leaves behind) and
//! `RecoveryManager::recover_interrupted` re-spawns the runner. The mock
//! provider is `looping` so a recovered run deterministically starts at step
//! 0 — exactly like a fresh model call after restore.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kern_core::config::parse_agent_spec;
use kern_core::engine::{Engine, RunOutcome};
use kern_core::event::EventBus;
use kern_core::fault::{FaultInjector, FaultScript};
use kern_core::recovery::RecoveryManager;
use kern_core::store::{Agent, ExecutionStatus, LifecycleState, Store};
use kern_model::mock::{MockProvider, ScriptedStep};
use kern_model::{gateway::ModelGateway, ModelError, ToolCall as ModelToolCall};

/// The matrix scenario: one allowed `filesystem.write` followed by a finish.
/// With a looping provider, a recovered run deterministically repeats the
/// same model turns (dedup replays the recorded tool row).
const AGENT_YAML: &str = "version: 1\nname: faultee\nmodel:\n  provider: mock\n  model: test\ntools:\n  - filesystem\npermissions:\n  filesystem:\n    read:\n      allow: [./]\n    write:\n      allow: [./]\nruntime:\n  max_steps: 20\n";

const OUT_FILE: &str = "out.txt";

fn tool_call(id: &str, name: &str, args: serde_json::Value) -> ModelToolCall {
    ModelToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: args,
    }
}

fn write_script() -> Vec<ScriptedStep> {
    vec![
        ScriptedStep::ToolCalls(vec![tool_call(
            "call-1",
            "filesystem",
            serde_json::json!({ "action": "write", "path": OUT_FILE, "content": "x" }),
        )]),
        ScriptedStep::Finish("done".into()),
    ]
}

struct Env {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    engine: Engine,
    injector: Arc<FaultInjector>,
}

fn env(script: Vec<ScriptedStep>) -> (Env, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let injector = Arc::new(FaultInjector::new());
    let store = Arc::new(
        Store::open_with_faults(dir.path(), Some(Arc::clone(&injector)))
            .expect("store opens with fault injector"),
    );
    let bus = EventBus::new(Arc::clone(&store));
    let provider = MockProvider::looping(script);
    let mut gateway = ModelGateway::new();
    gateway
        .register(Arc::new(provider.clone()))
        .expect("mock registers");
    let engine = Engine::new(Arc::clone(&store), bus, Arc::new(gateway), 8);

    let spec = parse_agent_spec(AGENT_YAML).expect("scenario spec parses");
    let agent = Agent::new(
        spec.name.clone(),
        serde_json::to_value(&spec).expect("spec serializes"),
        LifecycleState::Created,
    );
    store.create_agent(&agent).expect("agent created");
    (
        Env {
            _dir: dir,
            store,
            engine,
            injector,
        },
        agent.id,
    )
}

fn workspace_file(env: &Env, name: &str) -> PathBuf {
    env.store
        .data_dir()
        .join("workspace")
        .join("faultee")
        .join(name)
}

fn read_file(env: &Env) -> Option<String> {
    std::fs::read_to_string(workspace_file(env, OUT_FILE)).ok()
}

fn agent_events(store: &Store, agent_id: &str) -> Vec<kern_core::store::Event> {
    store
        .events_after(0, 10_000)
        .expect("replay events")
        .into_iter()
        .filter(|e| e.agent_id.as_deref() == Some(agent_id))
        .collect()
}

fn seqs_are_strictly_increasing(events: &[kern_core::store::Event]) -> bool {
    events.windows(2).all(|w| w[0].seq < w[1].seq)
}

/// Simulate what a daemon restart leaves behind: the agent is `recovering`
/// with its execution interrupted, then the production recovery path runs.
async fn drive_recovery(env: &Env, agent_id: &str, execution_id: &str) {
    let mut agent = env.store.get_agent(agent_id).expect("agent exists");
    agent.state = LifecycleState::Recovering;
    env.store.update_agent(&agent).expect("mark recovering");
    let mut execution = env
        .store
        .get_execution(execution_id)
        .expect("execution exists");
    execution.status = ExecutionStatus::Interrupted;
    env.store
        .update_execution(&execution)
        .expect("mark execution interrupted");

    let manager = RecoveryManager::new(env.engine.clone());
    manager
        .recover_interrupted()
        .await
        .expect("recovery manager runs");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = env.store.get_agent(agent_id).expect("agent exists").state;
        if matches!(
            state,
            LifecycleState::Completed | LifecycleState::Failed | LifecycleState::Terminated
        ) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "recovery did not reach a terminal state; agent is {state:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn assert_no_duplicate_completion(env: &Env, agent_id: &str) {
    let events = agent_events(&env.store, agent_id);
    assert!(
        seqs_are_strictly_increasing(&events),
        "per-agent event seqs must be strictly increasing: {:?}",
        events
            .iter()
            .map(|e| (e.seq, e.kind.as_str()))
            .collect::<Vec<_>>()
    );
    let completed = events
        .iter()
        .filter(|e| e.kind == "tool.completed" && e.execution_id.is_some())
        .count();
    assert!(
        completed <= 1,
        "a tool call must not complete twice in the event history ({completed})"
    );
}

/// How many times the engine STARTED executing `call-1` (the `tool.started`
/// event fires per execution attempt). The filesystem write tool overwrites,
/// so the side-effect file cannot distinguish one execution from two — the
/// event stream can.
fn execution_attempts(env: &Env, agent_id: &str) -> usize {
    agent_events(&env.store, agent_id)
        .iter()
        .filter(|e| {
            e.kind == "tool.started"
                && e.payload
                    .get("tool_call_id")
                    .and_then(serde_json::Value::as_str)
                    == Some("call-1")
        })
        .count()
}

/// Run the matrix scenario with `point` failing at occurrence `at`, then
/// assert the post-failure invariants and (where the case calls for it)
/// drive recovery and assert the exactly-once / at-least-once boundary.
async fn run_case(point: &str, at: usize) {
    let (env, agent_id) = env(write_script());
    env.injector.set(point, FaultScript::fail_at([at]));

    let summary = env
        .engine
        .run_agent(&agent_id, None)
        .await
        .expect("run_agent returns a summary (never hangs)");
    let execution_id = summary.execution_id.clone();

    // The tool row's recorded status (if any).
    let row = env
        .store
        .get_tool_call(&execution_id, "call-1")
        .expect("tool call lookup works");
    let row_terminal = matches!(
        row.as_ref().map(|r| &r.status),
        Some(kern_core::store::ToolCallStatus::Completed)
            | Some(kern_core::store::ToolCallStatus::Failed)
    );

    let state = env.store.get_agent(&agent_id).expect("agent exists").state;
    match &summary.outcome {
        RunOutcome::Completed { .. } => {
            // The fault never fired (scripted occurrence beyond the run's
            // call count): the run must be fully consistent.
            assert_eq!(state, LifecycleState::Completed);
            assert_eq!(read_file(&env).as_deref(), Some("x"));
            assert!(row_terminal);
            assert_no_duplicate_completion(&env, &agent_id);
            return;
        }
        RunOutcome::Failed { error } => {
            assert!(
                error.message.contains("injected fault"),
                "failure must be the injected fault, got: {error}"
            );
        }
        other => panic!("unexpected outcome for {point}@{at}: {other:?}"),
    }

    match state {
        LifecycleState::Created => {
            // F1: the run died before its first lifecycle transition (the
            // faulted `lifecycle.start`). The fix fails the execution row so
            // the active-execution index releases; the agent itself is
            // untouched and immediately runnable again.
            assert_eq!((point, at), ("transition", 1));
            let execution = env
                .store
                .get_execution(&execution_id)
                .expect("execution exists");
            assert_eq!(
                execution.status,
                ExecutionStatus::Failed,
                "a pre-start failure must fail the execution row, not leave it pending"
            );
            let kinds: Vec<String> = agent_events(&env.store, &agent_id)
                .into_iter()
                .map(|e| e.kind)
                .collect();
            assert!(
                kinds.contains(&"execution.failed".to_string()),
                "pre-start failure must be observable: {kinds:?}"
            );
            // The agent is NOT orphaned: once the storage fault clears, the
            // very next run completes (the regression F1 protects).
            env.injector.clear(point);
            let retry = env
                .engine
                .run_agent(&agent_id, None)
                .await
                .expect("retry runs after the fault clears");
            assert!(
                matches!(retry.outcome, RunOutcome::Completed { .. }),
                "the agent must be runnable again after a pre-start failure: {retry:?}"
            );
            assert_eq!(read_file(&env).as_deref(), Some("x"));
            assert_no_duplicate_completion(&env, &agent_id);
        }
        LifecycleState::Failed => {
            let agent = env.store.get_agent(&agent_id).expect("agent exists");
            assert!(
                agent.last_error.is_some(),
                "a failed agent must carry a structured last_error"
            );
            let kinds: Vec<String> = agent_events(&env.store, &agent_id)
                .into_iter()
                .map(|e| e.kind)
                .collect();
            assert!(
                kinds.contains(&"execution.failed".to_string()),
                "execution.failed must be recorded: {kinds:?}"
            );
            assert_no_duplicate_completion(&env, &agent_id);

            // Recovery assertions for the cases where the tool row survived:
            match (point, at) {
                // The terminal row is durable → recovery REPLAYS the recorded
                // result and never re-executes the tool. (transition 3 = the
                // completion transition fails after the batch fully
                // committed; create_checkpoint 3/4 = the post-batch/final
                // checkpoint fails.)
                ("transition", 3) | ("create_checkpoint", 3 | 4) => {
                    assert!(row_terminal, "tool row must be terminal");
                    assert_eq!(read_file(&env).as_deref(), Some("x"));
                    drive_recovery(&env, &agent_id, &execution_id).await;
                    let agent = env.store.get_agent(&agent_id).expect("agent exists");
                    assert_eq!(agent.state, LifecycleState::Completed, "recovery completes");
                    assert_eq!(
                        execution_attempts(&env, &agent_id),
                        1,
                        "a recorded terminal row must never re-execute"
                    );
                    assert_eq!(
                        read_file(&env).as_deref(),
                        Some("x"),
                        "the side effect survives recovery exactly once"
                    );
                    assert_no_duplicate_completion(&env, &agent_id);
                }
                // The terminal-row write itself failed: the row stayed
                // `requested`, so recovery re-drives it — the documented
                // at-least-once crash window. Asserted, not hidden.
                ("update_tool_call", 1) => {
                    assert!(!row_terminal, "row must still be requested");
                    assert_eq!(read_file(&env).as_deref(), Some("x"));
                    drive_recovery(&env, &agent_id, &execution_id).await;
                    let agent = env.store.get_agent(&agent_id).expect("agent exists");
                    assert_eq!(agent.state, LifecycleState::Completed);
                    assert_eq!(
                        execution_attempts(&env, &agent_id),
                        2,
                        "a row whose terminal write failed is re-executed once (at-least-once window)"
                    );
                    assert_no_duplicate_completion(&env, &agent_id);
                }
                // Everything else: the tool never executed, so there is no
                // side effect and nothing to replay.
                _ => {
                    assert!(
                        read_file(&env).is_none(),
                        "{point}@{at}: tool must not have executed"
                    );
                }
            }
        }
        LifecycleState::Running => {
            // The injected failure landed inside the fail transition itself:
            // the summary says Failed but the row could not be updated. The
            // designed backstop is the supervisor sweep (the runner is gone).
            assert_eq!(
                (point, at),
                ("transition", 3),
                "only the fail-during-fail case may leave a running row, got {point}@{at}"
            );
            env.engine
                .supervisor_sweep(Duration::ZERO)
                .await
                .expect("sweep runs");
            let agent = env.store.get_agent(&agent_id).expect("agent exists");
            assert_eq!(agent.state, LifecycleState::Failed);
            assert!(
                agent
                    .last_error
                    .as_deref()
                    .map(|e| e.contains("RUNNER_LOST"))
                    .unwrap_or(false),
                "supervisor must fail the orphaned runner with RUNNER_LOST"
            );
        }
        other => panic!(
            "{point}@{at}: unexpected post-failure state {other:?} (injected: {})",
            env.injector.occurrences_of(point)
        ),
    }
}

#[tokio::test]
async fn every_injected_store_failure_ends_consistently() {
    let cases: &[(&str, usize)] = &[
        // Lifecycle transition (agent + execution + events, one tx):
        // 1 = lifecycle.start, 2 = mark_started, 3 = complete (completing
        // run) / fail (failing run — the fail-during-fail case).
        ("transition", 1),
        ("transition", 2),
        ("transition", 3),
        // Checkpoint create: 1 = early, 2 = pre-batch, 3 = post-batch,
        // 4 = final.
        ("create_checkpoint", 1),
        ("create_checkpoint", 2),
        ("create_checkpoint", 3),
        ("create_checkpoint", 4),
        // Requested-rows batch write (the dedup source).
        ("record_tool_calls_batch", 1),
        // The terminal-row write — the at-least-once window boundary.
        ("update_tool_call", 1),
        // A persisted event write failing mid-run.
        ("append_event", 1),
    ];
    for (point, at) in cases {
        run_case(point, *at).await;
    }
}

/// The fail-during-fail window with the *failing* script: a permanent model
/// error fails the run, and the fail transition itself is faulted. The agent
/// must end `running` (the fail transition could not apply) and the
/// supervisor sweep must be the mechanism that fails it with `RUNNER_LOST`.
#[tokio::test]
async fn faulted_fail_transition_is_sealed_by_supervisor_sweep() {
    let script = vec![ScriptedStep::Fail(ModelError::InvalidResponse(
        "permanent provider failure".into(),
    ))];
    let (env, agent_id) = env(script);
    // start = transition 1, mark_started = transition 2, fail = transition 3.
    env.injector.set("transition", FaultScript::fail_at([3]));

    let summary = env
        .engine
        .run_agent(&agent_id, None)
        .await
        .expect("run_agent returns (never hangs)");
    assert!(
        matches!(summary.outcome, RunOutcome::Failed { .. }),
        "the injected model error must fail the run"
    );
    let state = env.store.get_agent(&agent_id).expect("agent exists").state;
    assert_eq!(
        state,
        LifecycleState::Running,
        "the fail transition was faulted; the row stays running until supervised"
    );

    env.engine
        .supervisor_sweep(Duration::ZERO)
        .await
        .expect("sweep runs");
    let agent = env.store.get_agent(&agent_id).expect("agent exists");
    assert_eq!(agent.state, LifecycleState::Failed);
    assert!(
        agent
            .last_error
            .as_deref()
            .map(|e| e.contains("RUNNER_LOST"))
            .unwrap_or(false),
        "supervisor must fail with RUNNER_LOST, got {:?}",
        agent.last_error
    );
    let kinds: Vec<String> = agent_events(&env.store, &agent_id)
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(
        kinds.contains(&"execution.failed".to_string()),
        "the sweep must record execution.failed: {kinds:?}"
    );
}

/// The permission-ask boundary: a failed decision write must not silently
/// apply, must not un-park the agent, and must resolve once the storage fault
/// clears.
#[tokio::test]
async fn faulted_permission_decision_is_never_silently_lost() {
    let yaml = "version: 1\nname: asker\nmodel:\n  provider: mock\n  model: test\ntools:\n  - filesystem\npermissions:\n  filesystem:\n    read:\n      allow: [./]\n    write:\n      ask: [./]\nruntime:\n  ask_timeout: 30s\n  max_steps: 20\n";
    let dir = tempfile::tempdir().expect("tempdir");
    let injector = Arc::new(FaultInjector::new());
    let store = Arc::new(
        Store::open_with_faults(dir.path(), Some(Arc::clone(&injector))).expect("store opens"),
    );
    let bus = EventBus::new(Arc::clone(&store));
    let provider = MockProvider::looping(vec![
        ScriptedStep::ToolCalls(vec![tool_call(
            "call-1",
            "filesystem",
            serde_json::json!({ "action": "write", "path": OUT_FILE, "content": "x" }),
        )]),
        ScriptedStep::Finish("done".into()),
    ]);
    let mut gateway = ModelGateway::new();
    gateway
        .register(Arc::new(provider.clone()))
        .expect("mock registers");
    let engine = Engine::new(Arc::clone(&store), bus, Arc::new(gateway), 8);

    let spec = parse_agent_spec(yaml).expect("spec parses");
    let agent = Agent::new(
        spec.name.clone(),
        serde_json::to_value(&spec).expect("spec serializes"),
        LifecycleState::Created,
    );
    store.create_agent(&agent).expect("agent created");

    // The ask parks the runner; drive the run from a task so the test can
    // deliver the decision while it waits.
    let runner_task = tokio::spawn({
        let engine = engine.clone();
        let agent_id = agent.id.clone();
        async move { engine.run_agent(&agent_id, None).await }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = store.get_agent(&agent.id).expect("agent exists").state;
        if state == LifecycleState::Waiting {
            break;
        }
        assert!(Instant::now() < deadline, "agent never parked waiting");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let request = store
        .pending_permission_requests_for_agent(&agent.id)
        .expect("pending requests")
        .into_iter()
        .find(|r| r.tool_call_id.as_deref() == Some("call-1"))
        .expect("ask request exists");

    // The decision write fails: nothing applies, the agent stays parked.
    injector.set("decide_permission_request", FaultScript::fail_at([1]));
    let err = store
        .decide_permission_request(&request.id, true)
        .expect_err("decision write fails");
    assert!(err.message.contains("injected fault"));
    assert_eq!(
        store.get_agent(&agent.id).expect("agent exists").state,
        LifecycleState::Waiting,
        "a failed decision must not un-park the agent"
    );

    // Fault clears; the same decision applies and the agent completes.
    injector.clear("decide_permission_request");
    let decided = store
        .decide_permission_request(&request.id, true)
        .expect("decision applies after the fault clears");
    assert_eq!(decided.status, kern_core::store::PermissionStatus::Granted);
    engine
        .resume_agent(&agent.id)
        .await
        .expect("resume applies");

    let summary = runner_task
        .await
        .expect("runner task joins")
        .expect("run_agent returns");
    assert!(
        matches!(summary.outcome, RunOutcome::Completed { .. }),
        "the granted ask must complete: {summary:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("workspace").join("asker").join(OUT_FILE))
            .expect("tool wrote the file")
            .as_str(),
        "x",
        "the granted tool call executed exactly once"
    );
}

/// Regression: an ask whose requests ALL expire (no operator
/// decision ever arrives) must NOT kill the run with an invalid
/// `waiting → completed` transition. Expired ≡ denied: the tool call is
/// recorded failed, the denial is fed to the model, and the run completes.
#[tokio::test]
async fn fully_expired_ask_continues_and_completes() {
    let yaml = "version: 1\nname: expirer\nmodel:\n  provider: mock\n  model: test\ntools:\n  - filesystem\npermissions:\n  filesystem:\n    read:\n      allow: [./]\n    write:\n      ask: [./]\nruntime:\n  ask_timeout: 150ms\n  max_steps: 20\n";
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Store::open(dir.path()).expect("store opens"));
    let bus = EventBus::new(Arc::clone(&store));
    let provider = MockProvider::looping(vec![
        ScriptedStep::ToolCalls(vec![tool_call(
            "call-1",
            "filesystem",
            serde_json::json!({ "action": "write", "path": OUT_FILE, "content": "x" }),
        )]),
        ScriptedStep::Finish("done".into()),
    ]);
    let mut gateway = ModelGateway::new();
    gateway
        .register(Arc::new(provider.clone()))
        .expect("mock registers");
    let engine = Engine::new(Arc::clone(&store), bus, Arc::new(gateway), 8);

    let spec = parse_agent_spec(yaml).expect("spec parses");
    let agent = Agent::new(
        spec.name.clone(),
        serde_json::to_value(&spec).expect("spec serializes"),
        LifecycleState::Created,
    );
    store.create_agent(&agent).expect("agent created");

    let runner_task = tokio::spawn({
        let engine = engine.clone();
        let agent_id = agent.id.clone();
        async move { engine.run_agent(&agent_id, None).await }
    });

    // The agent parks waiting (no decision is ever delivered).
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = store.get_agent(&agent.id).expect("agent exists").state;
        if state == LifecycleState::Waiting {
            break;
        }
        assert!(Instant::now() < deadline, "agent never parked waiting");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let summary = runner_task
        .await
        .expect("runner task joins")
        .expect("run_agent returns — expiry must not hang the run");
    assert!(
        matches!(summary.outcome, RunOutcome::Completed { .. }),
        "a fully-expired ask must complete, not die with an invalid transition: {summary:?}"
    );
    assert_eq!(
        store.get_agent(&agent.id).expect("agent exists").state,
        LifecycleState::Completed
    );

    // Expired ≡ denied: the tool row is terminal-failed with the expiry
    // reason, and the tool never executed (no file).
    let execution_id = &summary.execution_id;
    let row = store
        .get_tool_call(execution_id, "call-1")
        .expect("tool call lookup")
        .expect("tool row recorded");
    assert_eq!(row.status, kern_core::store::ToolCallStatus::Failed);
    let error = row.error.expect("row carries the denial");
    assert!(
        error.to_string().contains("expired"),
        "expired must surface as the denial reason: {error}"
    );
    assert!(
        !dir.path()
            .join("workspace")
            .join("expirer")
            .join(OUT_FILE)
            .exists(),
        "an expired ask must never execute the tool"
    );
    assert_eq!(
        store.get_execution(execution_id).expect("execution").status,
        ExecutionStatus::Completed
    );
}

/// The invalid-args record boundary: a failed write of a terminal
/// invalid-arguments row must fail the run loudly, never hang or silently
/// skip the record.
#[tokio::test]
async fn faulted_invalid_args_record_fails_loudly() {
    let yaml = "version: 1\nname: badargs\nmodel:\n  provider: mock\n  model: test\ntools:\n  - filesystem\npermissions:\n  filesystem:\n    read:\n      allow: [./]\n    write:\n      allow: [./]\nruntime:\n  max_steps: 20\n";
    let dir = tempfile::tempdir().expect("tempdir");
    let injector = Arc::new(FaultInjector::new());
    let store = Arc::new(
        Store::open_with_faults(dir.path(), Some(Arc::clone(&injector))).expect("store opens"),
    );
    let bus = EventBus::new(Arc::clone(&store));
    // `path` as a number: rejected by schema validation (the schema requires
    // a string), so the call is recorded terminal-failed and never executed.
    let provider = MockProvider::looping(vec![
        ScriptedStep::ToolCalls(vec![tool_call(
            "call-1",
            "filesystem",
            serde_json::json!({ "action": "write", "path": 123, "content": "x" }),
        )]),
        ScriptedStep::Finish("done".into()),
    ]);
    let mut gateway = ModelGateway::new();
    gateway
        .register(Arc::new(provider.clone()))
        .expect("mock registers");
    let engine = Engine::new(Arc::clone(&store), bus, Arc::new(gateway), 8);

    let spec = parse_agent_spec(yaml).expect("spec parses");
    let agent = Agent::new(
        spec.name.clone(),
        serde_json::to_value(&spec).expect("spec serializes"),
        LifecycleState::Created,
    );
    store.create_agent(&agent).expect("agent created");

    injector.set("record_tool_call", FaultScript::fail_at([1]));
    let summary = engine
        .run_agent(&agent.id, None)
        .await
        .expect("run returns (never hangs)");
    assert!(
        matches!(summary.outcome, RunOutcome::Failed { .. }),
        "a failed invalid-args record must fail the run: {summary:?}"
    );
    assert_eq!(
        store.get_agent(&agent.id).expect("agent exists").state,
        LifecycleState::Failed
    );
    let kinds: Vec<String> = store
        .events_after(0, 10_000)
        .expect("replay")
        .into_iter()
        .filter(|e| e.agent_id.as_deref() == Some(&agent.id))
        .map(|e| e.kind)
        .collect();
    assert!(
        kinds.contains(&"execution.failed".to_string()),
        "failure must be observable: {kinds:?}"
    );
    assert!(
        !dir.path()
            .join("workspace")
            .join("badargs")
            .join(OUT_FILE)
            .exists(),
        "invalid args must never reach the filesystem"
    );
}

/// The durable-memory boundary: a failed `memory_put` is a TOOL failure, not
/// a runtime failure — the run continues and completes (a soft, observable
/// error path, unlike the hard store boundaries above).
#[tokio::test]
async fn faulted_memory_write_is_a_tool_failure_and_run_continues() {
    let yaml = "version: 1\nname: memorizer\nmodel:\n  provider: mock\n  model: test\ntools:\n  - memory.write\nmemory:\n  enabled: true\npermissions:\n  memory:\n    read:\n      allow: [\"*\"]\n    write:\n      allow: [\"*\"]\nruntime:\n  max_steps: 20\n";
    let dir = tempfile::tempdir().expect("tempdir");
    let injector = Arc::new(FaultInjector::new());
    let store = Arc::new(
        Store::open_with_faults(dir.path(), Some(Arc::clone(&injector))).expect("store opens"),
    );
    let bus = EventBus::new(Arc::clone(&store));
    let provider = MockProvider::looping(vec![
        ScriptedStep::ToolCalls(vec![tool_call(
            "call-1",
            "memory.write",
            serde_json::json!({ "key": "k", "value": "v" }),
        )]),
        ScriptedStep::Finish("done".into()),
    ]);
    let mut gateway = ModelGateway::new();
    gateway
        .register(Arc::new(provider.clone()))
        .expect("mock registers");
    let engine = Engine::new(Arc::clone(&store), bus, Arc::new(gateway), 8);

    let spec = parse_agent_spec(yaml).expect("spec parses");
    let agent = Agent::new(
        spec.name.clone(),
        serde_json::to_value(&spec).expect("spec serializes"),
        LifecycleState::Created,
    );
    store.create_agent(&agent).expect("agent created");

    injector.set("memory_put", FaultScript::fail_at([1]));
    let summary = engine
        .run_agent(&agent.id, None)
        .await
        .expect("run returns (never hangs)");
    assert!(
        matches!(summary.outcome, RunOutcome::Completed { .. }),
        "a failed memory write must not fail the run: {summary:?}"
    );
    assert_eq!(
        store.get_agent(&agent.id).expect("agent exists").state,
        LifecycleState::Completed
    );
    // The tool row records the failure; nothing was persisted.
    let execution_id = &summary.execution_id;
    let row = store
        .get_tool_call(execution_id, "call-1")
        .expect("tool call lookup")
        .expect("tool row recorded");
    assert_eq!(row.status, kern_core::store::ToolCallStatus::Failed);
    assert!(
        row.error
            .as_ref()
            .map(|e| e.to_string().contains("STORAGE_FAILURE"))
            .unwrap_or(false),
        "the storage fault must surface as a structured tool failure: {:?}",
        row.error
    );
    assert!(
        store
            .memory_get(&agent.id, "k")
            .expect("memory lookup")
            .is_none(),
        "the faulted write must not persist"
    );
}

/// The durable-sleep boundary: `set_wake_at` failing before the park must
/// fail the run — an agent must never be `sleeping` without a wake time.
#[tokio::test]
async fn faulted_wake_at_write_never_parks_without_a_wake_time() {
    let yaml = "version: 1\nname: sleeper\nmodel:\n  provider: mock\n  model: test\ntools:\n  - sleep\nruntime:\n  max_steps: 20\n";
    let dir = tempfile::tempdir().expect("tempdir");
    let injector = Arc::new(FaultInjector::new());
    let store = Arc::new(
        Store::open_with_faults(dir.path(), Some(Arc::clone(&injector))).expect("store opens"),
    );
    let bus = EventBus::new(Arc::clone(&store));
    let provider = MockProvider::looping(vec![
        ScriptedStep::ToolCalls(vec![tool_call(
            "sleep-1",
            "sleep",
            serde_json::json!({ "ms": 20_000 }),
        )]),
        ScriptedStep::Finish("awake".into()),
    ]);
    let mut gateway = ModelGateway::new();
    gateway
        .register(Arc::new(provider.clone()))
        .expect("mock registers");
    let engine = Engine::new(Arc::clone(&store), bus, Arc::new(gateway), 8);

    let spec = parse_agent_spec(yaml).expect("spec parses");
    let agent = Agent::new(
        spec.name.clone(),
        serde_json::to_value(&spec).expect("spec serializes"),
        LifecycleState::Created,
    );
    store.create_agent(&agent).expect("agent created");

    injector.set("set_wake_at", FaultScript::fail_at([1]));
    let summary = engine
        .run_agent(&agent.id, None)
        .await
        .expect("run returns (never hangs)");
    assert!(
        matches!(summary.outcome, RunOutcome::Failed { .. }),
        "the failed wake-time write must fail the run: {summary:?}"
    );
    let agent = store.get_agent(&agent.id).expect("agent exists");
    assert_eq!(
        agent.state,
        LifecycleState::Failed,
        "a faulted park must fail the agent, never sleep without a wake time"
    );
    for execution in store
        .list_executions_for_agent(&agent.id)
        .expect("executions")
    {
        assert!(
            !matches!(agent.state, LifecycleState::Sleeping) || execution.wake_at.is_some(),
            "no sleeping agent may lack a wake time"
        );
        assert!(
            execution.wake_at.is_none(),
            "the faulted write never persisted a wake time"
        );
    }
}
