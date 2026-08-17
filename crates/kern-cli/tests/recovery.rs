//! The crash/restart proof (SPEC.md §18.1):
//!
//! 1. Seed an agent; start the REAL `kern daemon` binary, which starts it
//!    (`KERN_TEST_AUTOSTART_AGENT` — the `kern run` equivalent).
//! 2. The scripted mock writes `a.txt`, then issues a batch (long sleep +
//!    read). We SIGKILL the daemon mid-batch, after `a.txt` exists.
//! 3. Restart the daemon: it reconciles the interrupted agent, restores the
//!    pre-batch checkpoint, re-drives the pending batch, dedups the completed
//!    write (never re-executed), and finishes.
//! 4. Kill the daemon again (it already completed), open the store, and
//!    assert on DURABLE effects, not timing: both files with the exact
//!    content, exactly ONE execution of the deduped call, and the
//!    checkpoint.restored / execution.restored events.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use kern_core::config::parse_agent_spec;
use kern_core::store::{Agent, LifecycleState, Store};

/// The scripted mock: fixed call ids so the re-issued turns after restart
/// collide with the recorded rows (that collision IS the dedup test).
const SCRIPT: &str = r#"[
  {"kind":"tool_calls","calls":[{"id":"write-a","name":"filesystem","args":{"action":"write","path":"./a.txt","content":"first"}}]},
  {"kind":"tool_calls","calls":[
    {"id":"sleep-1","name":"sleep","args":{"ms":1500}},
    {"id":"read-a","name":"filesystem","args":{"action":"read","path":"./a.txt"}}
  ]},
  {"kind":"tool_calls","calls":[{"id":"write-b","name":"filesystem","args":{"action":"write","path":"./b.txt","content":"second"}}]},
  {"kind":"finish","text":"phoenix done"}
]"#;

const AGENT_YAML: &str = "version: 1\nname: phoenix\nmodel:\n  provider: mock\n  model: test\ntools:\n  - filesystem\n  - sleep\npermissions:\n  filesystem:\n    read:\n      allow: [./]\n    write:\n      allow: [./]\nruntime:\n  checkpoint_interval: 1s\n";

fn seed_agent(home: &Path) -> String {
    let spec = parse_agent_spec(AGENT_YAML).expect("agent yaml must parse");
    let store = Store::open(home).expect("seed store");
    let agent = Agent::new(
        spec.name.clone(),
        serde_json::to_value(&spec).unwrap(),
        LifecycleState::Created,
    );
    store.create_agent(&agent).expect("create agent");
    let id = agent.id.clone();
    drop(store); // release daemon.lock BEFORE the daemon starts
    id
}

fn spawn_daemon(home: &Path, autostart: Option<&str>) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kern"));
    cmd.arg("daemon").arg("--home").arg(home);
    if let Some(agent_id) = autostart {
        cmd.env("KERN_TEST_AUTOSTART_AGENT", agent_id);
    }
    cmd.env("KERN_TEST_MOCK_SCRIPT", SCRIPT);
    cmd.env("RUST_LOG", "kern=debug");
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    cmd.spawn().expect("spawn kern daemon")
}

fn sigkill(child: &mut Child) {
    unsafe {
        libc::kill(child.id() as i32, libc::SIGKILL);
    }
    let _ = child.wait();
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Wait for the durable `agent.completed` event. This is the only reliable
/// "the run really finished" signal: the last workspace write (`b.txt`)
/// precedes the final state transition by a few milliseconds, so SIGKILL
/// right after the write races the completion commit. SQLite WAL allows a
/// read-only connection while the daemon holds `daemon.lock` and its own
/// connections.
fn wait_for_completed_event(home: &Path) {
    let db = home.join("state.db");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let conn = rusqlite::Connection::open(&db).expect("open db to poll events");
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM events WHERE kind = 'agent.completed'",
                [],
                |row| row.get(0),
            )
            .expect("count agent.completed events");
        drop(conn);
        if count > 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for agent.completed event"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn workspace(home: &Path, agent: &str) -> PathBuf {
    home.join("workspace").join(agent)
}

#[test]
fn agent_survives_sigkill_and_completes_without_double_execution() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().to_path_buf();
    let agent_id = seed_agent(&home);
    let ws = workspace(&home, "phoenix");
    let a_txt = ws.join("a.txt");
    let b_txt = ws.join("b.txt");

    // --- Run 1: fresh start; kill mid-batch-2 (after a.txt exists) ---
    let mut daemon = spawn_daemon(&home, Some(&agent_id));
    wait_for_file(&a_txt);
    // Give the pre-batch checkpoint of batch 2 a beat, then SIGKILL.
    std::thread::sleep(Duration::from_millis(150));
    sigkill(&mut daemon);

    // --- Run 2: recovery completes the run ---
    let mut daemon = spawn_daemon(&home, None);
    wait_for_file(&b_txt); // the final write
    wait_for_completed_event(&home); // …and the durable completion commit
    sigkill(&mut daemon);

    // --- Assert on durable state (the store is now free of daemon.lock) ---
    let store = Store::open(&home).expect("reopen store");
    let agent = store.get_agent(&agent_id).expect("agent row");
    assert_eq!(
        agent.state,
        LifecycleState::Completed,
        "agent must complete"
    );

    // Correct final state: both files, exact single-write content.
    assert_eq!(std::fs::read_to_string(&a_txt).unwrap(), "first");
    assert_eq!(std::fs::read_to_string(&b_txt).unwrap(), "second");

    // The deduped call executed exactly ONCE across both daemon runs.
    let events = store.events_after(0, 10_000).unwrap();
    let write_a_starts = events
        .iter()
        .filter(|e| e.kind == "tool.started" && e.payload["tool_call_id"] == "write-a")
        .count();
    assert_eq!(write_a_starts, 1, "deduped write-a must never re-execute");

    // The crash/restart sequence was observed.
    assert!(
        events.iter().any(|e| e.kind == "checkpoint.restored"),
        "checkpoint.restored must be emitted after restart"
    );
    assert!(
        events.iter().any(|e| e.kind == "execution.restored"),
        "execution.restored must be emitted after restart"
    );
    assert!(
        events.iter().any(|e| e.kind == "scheduler.recovered_agent"),
        "scheduler.recovered_agent must be emitted"
    );
    assert!(
        events.iter().any(|e| e.kind == "agent.resumed"),
        "agent.resumed must be emitted after recovery"
    );
    assert!(
        events.iter().any(|e| e.kind == "agent.completed"),
        "agent.completed must be emitted"
    );

    // The write-a tool row is terminal from the ORIGINAL run (replayed, not
    // overwritten by a second execution).
    let executions = store.list_executions_for_agent(&agent_id).unwrap();
    assert_eq!(executions.len(), 1, "one execution, resumed not recreated");
    let rows = store.tool_calls_for_execution(&executions[0].id).unwrap();
    let write_a = rows
        .iter()
        .find(|r| r.id == "write-a")
        .expect("write-a row");
    assert_eq!(write_a.status, kern_core::store::ToolCallStatus::Completed);
    assert_eq!(write_a.result.as_ref().unwrap()["ok"], true);
}
