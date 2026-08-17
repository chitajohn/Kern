//! Regression tests.
//!
//! Deliberately corrupt durable state and throw hostile inputs at parsers.
//! Kern must either recover safely or fail loudly and diagnostically — it
//! must never silently reinterpret corrupted state as valid state, and it
//! must never panic on malformed input.

use std::sync::Arc;

use kern_core::checkpoint::CheckpointManager;
use kern_core::config::parse_agent_spec;
use kern_core::error::ErrorCode;
use kern_core::event::EventBus;
use kern_core::store::{Agent, Checkpoint, Execution, ExecutionStatus, LifecycleState, Store};
use rusqlite::params;
use serde_json::{json, Value};

fn test_store() -> (tempfile::TempDir, Arc<Store>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Store::open(dir.path()).expect("open store"));
    (dir, store)
}

/// Open a second raw connection to poke bytes the Store API never writes.
fn raw_conn(store: &Store) -> rusqlite::Connection {
    rusqlite::Connection::open(store.db_path()).expect("raw connection")
}

// ---------------------------------------------------------------------------
// §2 state corruption: loud failure, never silent reinterpretation
// ---------------------------------------------------------------------------

#[test]
fn corrupt_agent_lifecycle_state_fails_loudly() {
    let (_dir, store) = test_store();
    let agent = Agent::new("victim", Value::Null, LifecycleState::Created);
    store.create_agent(&agent).expect("create agent");

    // A state string that no version of Kern has ever produced.
    raw_conn(&store)
        .execute(
            "UPDATE agents SET lifecycle_state = 'runningg' WHERE id = ?1",
            params![agent.id],
        )
        .expect("inject corrupt state");

    let err = store
        .get_agent(&agent.id)
        .expect_err("corrupt state must surface");
    // Not a silent default, not a panic: a structured storage error.
    assert!(
        matches!(
            err.code(),
            ErrorCode::StorageFailure | ErrorCode::StorageCorruption
        ),
        "unexpected code: {:?}",
        err.code()
    );
}

#[test]
fn corrupt_event_payload_fails_loudly() {
    let (_dir, store) = test_store();
    store
        .append_event("agent.created", None, None, json!({ "ok": true }))
        .expect("append event");
    raw_conn(&store)
        .execute(
            "INSERT INTO events (ts, kind, agent_id, payload) VALUES ('2026-08-15T00:00:00Z', 'agent.thinking', NULL, 'this is not json')",
            [],
        )
        .expect("inject corrupt event");

    let err = store
        .events_after(0, 100)
        .expect_err("corrupt event must surface");
    assert!(
        matches!(
            err.code(),
            ErrorCode::StorageFailure | ErrorCode::StorageCorruption
        ),
        "unexpected code: {:?}",
        err.code()
    );
}

#[tokio::test]
async fn corrupt_checkpoint_payload_is_rejected_not_reinterpreted() {
    let (_dir, store) = test_store();
    let bus = EventBus::new(Arc::clone(&store));
    let cm = CheckpointManager::new(Arc::clone(&store), bus);

    let agent = Agent::new("cp-victim", Value::Null, LifecycleState::Running);
    store.create_agent(&agent).expect("create agent");
    let execution = Execution::new(&agent.id, ExecutionStatus::Running);
    store
        .create_execution(&execution)
        .expect("create execution");

    // Valid JSON, but NOT a CheckpointPayload: restore must not guess.
    store
        .create_checkpoint(&Checkpoint::new(
            &agent.id,
            &execution.id,
            1,
            json!({ "definitely": "not a checkpoint payload" }),
        ))
        .expect("insert corrupt checkpoint");

    let err = cm
        .restore(&agent.id, &execution.id)
        .await
        .expect_err("corrupt payload must fail restore");
    assert_eq!(err.code(), ErrorCode::CheckpointCorrupt);
    assert!(
        err.to_string().contains("corrupt"),
        "diagnostic must say what is wrong: {err}"
    );
}

#[tokio::test]
async fn restore_rejects_future_checkpoint_format_versions() {
    let (_dir, store) = test_store();
    let bus = EventBus::new(Arc::clone(&store));
    let cm = CheckpointManager::new(Arc::clone(&store), bus);

    let agent = Agent::new("cp-future", Value::Null, LifecycleState::Running);
    store.create_agent(&agent).expect("create agent");
    let execution = Execution::new(&agent.id, ExecutionStatus::Running);
    store
        .create_execution(&execution)
        .expect("create execution");

    let mut checkpoint = Checkpoint::new(&agent.id, &execution.id, 1, json!({}));
    checkpoint.format_version = u32::MAX;
    store
        .create_checkpoint(&checkpoint)
        .expect("insert future checkpoint");

    let err = cm
        .restore(&agent.id, &execution.id)
        .await
        .expect_err("future version must fail restore");
    assert_eq!(err.code(), ErrorCode::CheckpointFormatUnsupported);
}

// ---------------------------------------------------------------------------
// §18 fuzz-lite: deterministic malformed input must never panic
// ---------------------------------------------------------------------------

/// Deterministic xorshift64 PRNG so failures are reproducible.
struct XorShift64(u64);

impl XorShift64 {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| (self.next() & 0xff) as u8).collect()
    }
}

#[test]
fn config_parser_never_panics_on_hostile_input() {
    let mut rng = XorShift64(0x5eed_2026_0801_0001);
    let seeds = [
        "version: 1\nname: x\nmodel:\n  provider: mock\n  model: test\n".to_string(),
        "version: 1\nname: x\n".to_string(),
        "".to_string(),
        "---\n".to_string(),
        "a: [1,2,3]\n".to_string(),
        "!!binary |\n  AAAA\n".to_string(),
        "key: \"\\u0000\\ud800\"\n".to_string(),
    ];
    for _ in 0..400 {
        let n = (rng.next() % 200) as usize;
        let mut blob = rng.bytes(n);
        // Sprinkle in real YAML structure so we exercise parser internals,
        // not just the lexer's error path.
        if rng.next().is_multiple_of(3) {
            blob.extend_from_slice(b"\nname: [\nmodel: {provider: mock}\n");
        }
        let input = String::from_utf8_lossy(&blob);
        // Never panic: Ok or Err are both fine, a panic is a bug.
        let _ = parse_agent_spec(&input);
    }
    // Structured seeds must not panic either.
    for seed in seeds {
        let _ = parse_agent_spec(&seed);
    }
}

#[test]
fn tool_argument_validation_never_panics_on_hostile_json() {
    // Build the real per-agent registry the engine would build (all tools).
    let (_dir, store) = test_store();
    let workspace = _dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let spec = parse_agent_spec(
        "version: 1\nname: fuzz\nmodel:\n  provider: mock\n  model: test\n\
         tools: [filesystem, http, shell, sleep, memory.read, memory.write, memory.list, noop]\n\
         memory:\n  enabled: true\npermissions:\n  shell:\n    enabled: true\n    sandbox: off\n  filesystem:\n    read:\n      allow: [./]\n    write:\n      allow: [./]\n  network:\n    allow: [api.example.com]\n",
    )
    .unwrap();
    let registry = kern_core::tools::build_registry(&spec, Arc::clone(&store), &workspace)
        .expect("registry builds");

    let mut rng = XorShift64(0xfeed_2026_0801_0002);
    for _ in 0..400 {
        let value = random_json(&mut rng);
        for name in [
            "filesystem",
            "http",
            "shell",
            "sleep",
            "memory.read",
            "memory.write",
            "memory.list",
        ] {
            // Never panic: invalid args must be rejected by the schema, not
            // crash the validator.
            let _ = registry.validate(name, &value);
        }
    }
}

fn random_json(rng: &mut XorShift64) -> Value {
    match rng.next() % 6 {
        0 => Value::Null,
        1 => Value::Bool(rng.next().is_multiple_of(2)),
        2 => Value::Number((rng.next() % 1_000_000).into()),
        3 => {
            let n = (rng.next() % 40) as usize;
            Value::String(String::from_utf8_lossy(&rng.bytes(n)).into_owned())
        }
        4 => {
            let n = (rng.next() % 5) as usize;
            let mut arr = Vec::new();
            for _ in 0..n {
                arr.push(random_json(rng));
            }
            Value::Array(arr)
        }
        _ => {
            let n = (rng.next() % 5) as usize;
            let mut map = serde_json::Map::new();
            for _ in 0..n {
                let key = {
                    let kn = (rng.next() % 8) as usize;
                    String::from_utf8_lossy(&rng.bytes(kn)).into_owned()
                };
                map.insert(key, random_json(rng));
            }
            Value::Object(map)
        }
    }
}
