//! Redaction audit (SPEC.md §14.3/§14.4, acceptance criterion §18.4):
//!
//! Run the REAL daemon with fake provider keys and a bearer token, execute a
//! scripted agent end-to-end, then assert the secret VALUES appear nowhere:
//!
//! - `$KERN_HOME/logs/runtime.jsonl` (the daemon's structured log sink)
//! - the SQLite store (events payloads, agent configs, checkpoint payloads,
//!   tool-call args/results/errors, memory)
//! - CLI output (`kern logs`)
//!
//! The keys are only ever present in the environment — if the runtime leaks an
//! env value anywhere durable or observable, this test fails.

#![cfg(unix)]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use kern_core::store::Store;
use tempfile::TempDir;

/// Values that must never appear anywhere. Each is long enough that the
/// pattern redaction (`sk-`, `sk-ant-`, `Bearer`) would mask it in logs.
const FAKE_OPENAI: &str = "sk-fake-openai-key-1234567890abcdef";
const FAKE_ANTHROPIC: &str = "sk-ant-api03-fake-key-9876543210fedcba";
const FAKE_TOKEN: &str = "kern-test-bearer-token-0123456789abcdef";

const SCRIPT: &str = r#"[
  {"kind":"thinking","text":"plain reasoning, no secrets"},
  {"kind":"tool_calls","calls":[{"id":"write-note","name":"filesystem","args":{"action":"write","path":"./note.txt","content":"plain content"}}]},
  {"kind":"finish","text":"audit done"}
]"#;

const AGENT_YAML: &str = "version: 1\nname: audit\nmodel:\n  provider: mock\n  model: test\ntools:\n  - filesystem\npermissions:\n  filesystem:\n    read:\n      allow: [./]\n    write:\n      allow: [./]\n";

struct Env {
    dir: TempDir,
    home: std::path::PathBuf,
    addr: String,
    daemon: Option<Child>,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("kern-home");
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        Self {
            dir,
            home,
            addr: format!("127.0.0.1:{port}"),
            daemon: None,
        }
    }

    /// Run the CLI with the fake secrets in the environment.
    fn kern(&self, args: &[&str], token: Option<&str>) -> std::process::Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kern"));
        cmd.args(args)
            .current_dir(self.dir.path())
            .env("KERN_HOME", &self.home)
            .env("KERN_API_ADDR", &self.addr)
            .env("OPENAI_API_KEY", FAKE_OPENAI)
            .env("ANTHROPIC_API_KEY", FAKE_ANTHROPIC);
        match token {
            Some(token) => {
                cmd.env("KERN_TOKEN", token);
            }
            None => {
                cmd.env_remove("KERN_TOKEN");
            }
        }
        cmd.output().expect("run kern")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
    }
}

/// Every durable byte the runtime could have written, as one string.
fn dump_store(home: &Path) -> String {
    let store = Store::open(home).expect("reopen store after daemon kill");
    let mut out = String::new();
    fn push<T: serde::Serialize>(out: &mut String, value: &T) {
        out.push_str(&serde_json::to_string(value).unwrap_or_default());
        out.push('\n');
    }
    for event in store.events_after(0, 100_000).unwrap() {
        push(&mut out, &event);
    }
    for agent in store.list_agents().unwrap() {
        push(&mut out, &agent);
        for cp in store.list_checkpoints(&agent.id, 10_000).unwrap() {
            push(&mut out, &cp);
        }
        for execution in store.list_executions_for_agent(&agent.id).unwrap() {
            for call in store.tool_calls_for_execution(&execution.id).unwrap() {
                push(&mut out, &call);
            }
        }
        for memory in store.memory_list(&agent.id, None).unwrap() {
            push(&mut out, &memory);
        }
    }
    out
}

#[test]
fn secrets_never_reach_logs_store_or_cli_output() {
    let mut env = Env::new();
    std::fs::write(env.dir.path().join("agent.yaml"), AGENT_YAML).unwrap();

    // Spawn the daemon with fake secrets in its environment.
    let child = Command::new(env!("CARGO_BIN_EXE_kern"))
        .args(["daemon", "--home"])
        .arg(&env.home)
        .env("KERN_HOME", &env.home)
        .env("KERN_API_ADDR", &env.addr)
        .env("OPENAI_API_KEY", FAKE_OPENAI)
        .env("ANTHROPIC_API_KEY", FAKE_ANTHROPIC)
        .env("KERN_TOKEN", FAKE_TOKEN)
        .env("KERN_TEST_MOCK_SCRIPT", SCRIPT)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");
    env.daemon = Some(child);

    // Wait for the API (auth is required: the CLI must present KERN_TOKEN).
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if env.kern(&["ps"], Some(FAKE_TOKEN)).status.success() {
            break;
        }
        assert!(Instant::now() < deadline, "daemon did not become reachable");
        std::thread::sleep(Duration::from_millis(50));
    }

    // A client WITHOUT the token must be rejected (auth enforced).
    let out = env.kern(&["ps"], None);
    assert_eq!(
        out.status.code(),
        Some(1),
        "tokenless client must be rejected"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("UNAUTHORIZED"),
        "expected UNAUTHORIZED: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Run an agent to completion (thinking + tool call + finish).
    let out = env.kern(&["run", "agent.yaml", "--wait"], Some(FAKE_TOKEN));
    assert!(
        out.status.success(),
        "run --wait failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("audit done"),
        "agent must complete"
    );

    // CLI output must not leak the secrets.
    let logs_output = env.kern(&["logs", "audit"], Some(FAKE_TOKEN));
    let logs = String::from_utf8_lossy(&logs_output.stdout);
    for secret in [FAKE_OPENAI, FAKE_ANTHROPIC, FAKE_TOKEN] {
        assert!(!logs.contains(secret), "kern logs leaked {secret}: {logs}");
    }

    // Kill the daemon, then audit every durable artifact.
    if let Some(mut daemon) = env.daemon.take() {
        let _ = daemon.kill();
        let _ = daemon.wait();
    }

    // Structured log file (written by the daemon into $KERN_HOME/logs/).
    let log_file = std::fs::read_to_string(env.home.join("logs").join("runtime.jsonl"))
        .expect("daemon must write $KERN_HOME/logs/runtime.jsonl");
    for secret in [FAKE_OPENAI, FAKE_ANTHROPIC, FAKE_TOKEN] {
        assert!(!log_file.contains(secret), "runtime.jsonl leaked {secret}");
    }

    // The whole store: events, configs, checkpoints, tool rows, memory.
    let store_dump = dump_store(&env.home);
    for secret in [FAKE_OPENAI, FAKE_ANTHROPIC, FAKE_TOKEN] {
        assert!(!store_dump.contains(secret), "the store leaked {secret}");
    }

    // The agent ran and its workspace file exists (the run was real).
    assert!(env
        .home
        .join("workspace")
        .join("audit")
        .join("note.txt")
        .exists());
}
