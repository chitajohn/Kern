//! CLI integration suite (SPEC.md §16, acceptance criterion §18.7): every
//! command exercised against the REAL `kern daemon` binary over its HTTP API.
//!
//! Auth is exercised end-to-end: `kern init` writes `$KERN_HOME/token` and
//! the daemon requires it; the CLI client picks it up from the same home.
//!
//! The daemon's scripted mock (`KERN_TEST_MOCK_SCRIPT`) is a shared FIFO per
//! daemon process, so each scenario restarts the daemon — a fresh script for
//! every agent:
//!
//! - scenario 1 `quick.yaml` — completes (~2s) under `run --wait`.
//! - scenario 2 `slow.yaml` — long enough to pause/resume/checkpoint/terminate.
//! - scenario 3 `ask.yaml`  — `filesystem.write.ask` policy: parks in `waiting`
//!   until `kern grant` resumes it.
//!
//! A per-test free port avoids colliding with `recovery.rs` (which runs in a
//! separate test binary, in parallel).

#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// One sleep tool call, then a workspace write, then finish.
const SCRIPT: &str = r#"[
  {"kind":"tool_calls","calls":[{"id":"sleep-1","name":"sleep","args":{"ms":1500}}]},
  {"kind":"tool_calls","calls":[{"id":"write-out","name":"filesystem","args":{"action":"write","path":"./out.txt","content":"cli done"}}]},
  {"kind":"finish","text":"cli finished"}
]"#;

const QUICK_YAML: &str = "version: 1\nname: quick\nmodel:\n  provider: mock\n  model: test\ntools:\n  - filesystem\n  - sleep\npermissions:\n  filesystem:\n    read:\n      allow: [./]\n    write:\n      allow: [./]\nruntime:\n  checkpoint_interval: 1s\n";

const SLOW_YAML: &str = "version: 1\nname: slow\nmodel:\n  provider: mock\n  model: test\ntools:\n  - filesystem\n  - sleep\npermissions:\n  filesystem:\n    read:\n      allow: [./]\n    write:\n      allow: [./]\n";

/// The write is NOT allowed up front: the engine parks the agent and raises
/// a permission request; `kern grant` must unblock it.
const ASK_YAML: &str = "version: 1\nname: ask\nmodel:\n  provider: mock\n  model: test\ntools:\n  - filesystem\n  - sleep\npermissions:\n  filesystem:\n    read:\n      allow: [./]\n    write:\n      ask: [./]\n";

struct TestEnv {
    dir: TempDir,
    home: PathBuf,
    addr: String,
    daemon: Option<Child>,
}

impl TestEnv {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("kern-home");
        let port = free_port();
        Self {
            dir,
            home,
            addr: format!("127.0.0.1:{port}"),
            daemon: None,
        }
    }

    fn spec_path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Run the real `kern` binary with the test env vars.
    fn kern(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_kern"))
            .args(args)
            .current_dir(self.dir.path())
            .env("KERN_HOME", &self.home)
            .env("KERN_API_ADDR", &self.addr)
            .env_remove("KERN_TOKEN")
            .output()
            .expect("run kern")
    }

    fn kern_stdout(&self, args: &[&str]) -> String {
        String::from_utf8_lossy(&self.kern(args).stdout).to_string()
    }

    /// Kill any running daemon (the store survives; a restart's recovery
    /// sweep finds nothing interrupted after a clean run).
    fn kill_daemon(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
    }

    /// Spawn a fresh daemon (fresh mock script FIFO) and wait for the API.
    fn spawn_daemon(&mut self) {
        self.kill_daemon();
        let child = Command::new(env!("CARGO_BIN_EXE_kern"))
            .args(["daemon", "--home"])
            .arg(&self.home)
            .env("KERN_HOME", &self.home)
            .env("KERN_API_ADDR", &self.addr)
            .env("KERN_TEST_MOCK_SCRIPT", SCRIPT)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn kern daemon");
        self.daemon = Some(child);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if self.kern(&["ps"]).status.success() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "daemon did not become reachable within 30s"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        self.kill_daemon();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("local addr")
        .port()
}

/// Poll until `kern ps` shows `name` in the given state (bounded).
fn wait_for_state(env: &TestEnv, name: &str, state: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let ps = env.kern_stdout(&["ps"]);
        if ps
            .lines()
            .any(|l| l.trim_start().starts_with(name) && l.split_whitespace().any(|c| c == state))
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "agent {name} never reached {state}; last ps:\n{ps}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn workspace_file(env: &TestEnv, agent: &str) -> PathBuf {
    env.home.join("workspace").join(agent).join("out.txt")
}

#[test]
fn full_cli_flow_against_live_daemon() {
    let mut env = TestEnv::new();

    // --- init: home dir + token + agent.yaml scaffold -------------------
    let out = env.kern(&["init"]);
    assert!(
        out.status.success(),
        "init: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        env.home.join("token").exists(),
        "init must generate the API token"
    );
    assert!(
        env.spec_path("agent.yaml").exists(),
        "init must scaffold agent.yaml"
    );
    // The scaffold must parse as a valid spec (mock needs no keys).
    let scaffold = std::fs::read_to_string(env.spec_path("agent.yaml")).unwrap();
    kern_core::config::parse_agent_spec(&scaffold).expect("scaffolded spec is valid");

    // Idempotent: a second init leaves both files untouched.
    let out = env.kern(&["init"]);
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("already"),
        "second init should report existing files"
    );

    // --- specs for the flow ---------------------------------------------
    std::fs::write(env.spec_path("quick.yaml"), QUICK_YAML).unwrap();
    std::fs::write(env.spec_path("slow.yaml"), SLOW_YAML).unwrap();
    std::fs::write(env.spec_path("ask.yaml"), ASK_YAML).unwrap();

    // =====================================================================
    // 1. Quick run — run --wait, ps, logs, inspect, schedule,
    // tools, models, doctor, version.
    // =====================================================================
    env.spawn_daemon();

    let out = env.kern(&["run", "quick.yaml", "--wait"]);
    assert!(
        out.status.success(),
        "run --wait: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("created agent quick"),
        "missing create line: {stdout}"
    );
    assert!(
        stdout.contains("started execution"),
        "missing start line: {stdout}"
    );
    assert!(
        stdout.contains("agent quick completed"),
        "missing completion: {stdout}"
    );
    assert!(
        workspace_file(&env, "quick").exists(),
        "the scripted tool must have written the workspace file"
    );

    let ps = env.kern_stdout(&["ps"]);
    assert!(ps.contains("quick") && ps.contains("completed"), "ps: {ps}");

    let logs = env.kern_stdout(&["logs", "quick"]);
    assert!(
        logs.contains("tool.completed"),
        "logs must show tool events: {logs}"
    );
    assert!(
        logs.contains("filesystem"),
        "logs must name the tool: {logs}"
    );
    assert!(
        logs.contains("agent.completed"),
        "logs must show completion: {logs}"
    );

    let inspect = env.kern_stdout(&["inspect", "quick"]);
    assert!(
        inspect.contains("state:          completed"),
        "inspect state: {inspect}"
    );
    assert!(
        inspect.contains("executions:"),
        "inspect executions: {inspect}"
    );
    assert!(
        inspect.contains("latest cp:"),
        "inspect checkpoint: {inspect}"
    );
    assert!(
        inspect.contains("model:          mock / test"),
        "inspect model: {inspect}"
    );

    let schedule = env.kern_stdout(&["schedule", "quick"]);
    assert!(schedule.contains("(none)"), "schedule: {schedule}");

    let tools = env.kern_stdout(&["tools"]);
    assert!(
        tools.contains("filesystem") && tools.contains("PERMISSION"),
        "tools: {tools}"
    );
    let models = env.kern_stdout(&["models"]);
    assert!(
        models.contains("mock") && models.contains("PROVIDER"),
        "models: {models}"
    );

    let out = env.kern(&["doctor"]);
    assert!(
        out.status.success(),
        "doctor must exit 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("all checks passed"));

    let version = env.kern_stdout(&["version"]);
    assert!(version.contains("kern 0.1.0"), "version: {version}");
    assert!(
        version.contains("daemon kern"),
        "version should report the daemon: {version}"
    );

    env.kill_daemon();

    // =====================================================================
    // 2. Lifecycle on a running agent.
    // =====================================================================
    env.spawn_daemon();
    let out = env.kern(&["run", "slow.yaml"]);
    assert!(
        out.status.success(),
        "run slow: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_for_state(&env, "slow", "running");

    for (args, expected) in [
        (vec!["pause", "slow"], "paused"),
        (vec!["resume", "slow"], "resumed"),
        (vec!["checkpoint", "slow"], "checkpoint"),
        (vec!["terminate", "slow"], "terminated"),
    ] {
        let out = env.kern(&args);
        assert!(
            out.status.success(),
            "kern {} must exit 0: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains(expected),
            "kern {} output: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout)
        );
    }
    env.kill_daemon();

    // =====================================================================
    // 3. Ask mode — permissions -> grant -> resumes to completion.
    // =====================================================================
    env.spawn_daemon();
    let out = env.kern(&["run", "ask.yaml"]);
    assert!(
        out.status.success(),
        "run ask: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_for_state(&env, "ask", "waiting");

    let pending = env.kern_stdout(&["permissions"]);
    assert!(
        pending.contains("./out.txt")
            && pending.contains("write")
            && !pending.contains("no pending"),
        "permissions must list the ask: {pending}"
    );
    let request_id = pending
        .lines()
        .skip(1) // header
        .find_map(|line| {
            if line.contains("./out.txt") {
                line.split_whitespace().next().map(str::to_string)
            } else {
                None
            }
        })
        .expect("a pending request row");
    assert_eq!(
        request_id.chars().count(),
        36,
        "permissions must print full request ids"
    );

    let out = env.kern(&["grant", &request_id]);
    assert!(
        out.status.success(),
        "grant: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("granted"));

    wait_for_state(&env, "ask", "completed");
    let pending = env.kern_stdout(&["permissions"]);
    assert!(pending.contains("no pending"), "no asks left: {pending}");

    // =====================================================================
    // Error paths and exit codes.
    // =====================================================================
    // Unknown agent while the daemon is up: structured 404 -> exit 1.
    let out = env.kern(&["pause", "no-such-agent"]);
    assert_eq!(out.status.code(), Some(1), "unknown agent must exit 1");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("AGENT_NOT_FOUND"),
        "structured error expected: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    env.kill_daemon();

    // Missing spec file: local validation error -> exit 1.
    let out = env.kern(&["run", "missing.yaml"]);
    assert_eq!(out.status.code(), Some(1), "missing spec must exit 1");

    // Usage error (clap) exits 2.
    let out = Command::new(env!("CARGO_BIN_EXE_kern"))
        .env("KERN_HOME", &env.home)
        .env("KERN_API_ADDR", &env.addr)
        .arg("definitely-not-a-command")
        .output()
        .expect("run kern");
    assert_eq!(out.status.code(), Some(2), "unknown subcommand must exit 2");

    // Daemon down: client commands fail with the actionable hint.
    let out = env.kern(&["ps"]);
    assert_eq!(out.status.code(), Some(1), "daemon-down ps must exit 1");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cannot reach the Kern daemon"),
        "unreachable hint expected: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
