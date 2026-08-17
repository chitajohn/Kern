//! `shell` builtin (SPEC.md §11.3) — the sandbox-gated process tool.
//!
//! Security model: the shell tool MUST NOT be constructed unless the agent's
//! `permissions.shell.enabled` is true, and MUST run through an isolation
//! layer (`sandbox: required | best-effort`). The `CommandRunner` seam keeps
//! that policy in `kern-core`: this crate ships the bare `SystemRunner`
//! (no sandbox — used only for `sandbox: off` and tests), while `kern-core`
//! wraps it with the OS sandbox backend and passes the wrapped runner in.
//!
//! The tool itself enforces the output cap (default 1 MiB) and the process
//! lifecycle (timeout, kill-on-drop) via `crate::process`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::process::{run_captured, CommandOutput, RunLimits};
use crate::registry::{Tool, ToolContext};

/// The command to run, as authorized by the engine.
#[derive(Debug, Clone)]
pub struct CommandRequest {
    pub command: String,
    pub cwd: Option<PathBuf>,
}

/// The subprocess transport. `kern-core`'s sandboxed runner implements this
/// by applying the OS sandbox to the command before spawning; tests use a
/// scripted fake.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run_command(&self, req: CommandRequest) -> Result<CommandOutput, ToolError>;
}

/// Runs `sh -c <command>` directly, without an OS sandbox. Used when
/// `sandbox: off` (an explicit operator choice) and in tests.
#[derive(Default)]
pub struct SystemRunner {
    pub limits: RunLimits,
}

/// The `(program, args)` form of the shell invocation — `sh -c <command>`
/// (or `cmd /C` on Windows). Sandboxed runners feed this to the OS sandbox
/// wrapper; `SystemRunner` spawns it directly.
pub fn shell_spec(command: &str) -> (std::ffi::OsString, Vec<std::ffi::OsString>) {
    #[cfg(windows)]
    {
        ("cmd".into(), vec!["/C".into(), command.to_string().into()])
    }
    #[cfg(not(windows))]
    {
        ("sh".into(), vec!["-c".into(), command.to_string().into()])
    }
}

#[async_trait]
impl CommandRunner for SystemRunner {
    async fn run_command(&self, req: CommandRequest) -> Result<CommandOutput, ToolError> {
        let (program, args) = shell_spec(&req.command);
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        if let Some(cwd) = req.cwd {
            cmd.current_dir(cwd);
        }
        run_captured(cmd, &self.limits).await
    }
}

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "minLength": 1 },
            "cwd": { "type": "string" }
        },
        "required": ["command"],
        "additionalProperties": false
    })
}

/// The shell tool. Construct only via `kern-core`'s gate (enabled + sandbox).
/// The output cap and timeout live in the runner's `RunLimits` (enforced by
/// `run_captured`), not here — every runner must apply them.
pub struct ShellTool {
    runner: Arc<dyn CommandRunner>,
}

impl ShellTool {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Run a shell command inside the agent's sandbox."
    }

    fn input_schema(&self) -> &Value {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(schema)
    }

    async fn run(&self, args: &Value, _ctx: &ToolContext<'_>) -> Result<Value, ToolError> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolError::InvalidArguments("shell requires a string 'command' field".to_string())
            })?
            .to_string();
        let cwd = args
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);

        let out = self
            .runner
            .run_command(CommandRequest { command, cwd })
            .await?;

        Ok(json!({
            "stdout": out.stdout,
            "stderr": out.stderr,
            "code": out.code,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::RunLimits;
    use std::time::Duration;

    const FAKE_RUN_DELAY: Duration = Duration::from_millis(5);

    /// A scripted runner with interior mutability.
    struct ScriptedRunner {
        queue: tokio::sync::Mutex<std::collections::VecDeque<Result<CommandOutput, ToolError>>>,
    }

    impl ScriptedRunner {
        fn new(script: Vec<Result<CommandOutput, ToolError>>) -> Arc<Self> {
            Arc::new(Self {
                queue: tokio::sync::Mutex::new(script.into()),
            })
        }
    }

    #[async_trait]
    impl CommandRunner for ScriptedRunner {
        async fn run_command(&self, req: CommandRequest) -> Result<CommandOutput, ToolError> {
            assert!(!req.command.is_empty());
            let mut queue = self.queue.lock().await;
            queue
                .pop_front()
                .unwrap_or_else(|| Err(ToolError::Failed("script exhausted".to_string())))
        }
    }

    fn ctx<'a>() -> ToolContext<'a> {
        ToolContext {
            agent_id: "a",
            execution_id: "e",
            tool_call_id: "c",
        }
    }

    fn output(stdout: &str, code: Option<i32>) -> CommandOutput {
        CommandOutput {
            stdout: stdout.to_string(),
            stderr: String::new(),
            code,
        }
    }

    #[tokio::test]
    async fn returns_stdout_and_code() {
        let runner = ScriptedRunner::new(vec![Ok(output("hello", Some(0)))]);
        let tool = ShellTool::new(runner);
        let out = tool
            .run(&json!({ "command": "echo hello" }), &ctx())
            .await
            .unwrap();
        assert_eq!(out["stdout"], "hello");
        assert_eq!(out["code"], 0);
    }

    #[tokio::test]
    async fn propagates_runner_errors() {
        let runner = ScriptedRunner::new(vec![Err(ToolError::Timeout(FAKE_RUN_DELAY))]);
        let tool = ShellTool::new(runner);
        let err = tool
            .run(&json!({ "command": "sleep 100" }), &ctx())
            .await
            .unwrap_err();
        assert_eq!(err.code(), "TOOL_TIMEOUT");
    }

    #[tokio::test]
    async fn rejects_missing_command() {
        let runner = ScriptedRunner::new(vec![]);
        let tool = ShellTool::new(runner);
        let err = tool.run(&json!({}), &ctx()).await.unwrap_err();
        assert_eq!(err.code(), "TOOL_INVALID_ARGUMENTS");
    }

    #[tokio::test]
    async fn real_system_runner_executes() {
        let runner: Arc<dyn CommandRunner> = Arc::new(SystemRunner::default());
        let tool = ShellTool::new(runner);
        let out = tool
            .run(&json!({ "command": "printf 'hi' && exit 7" }), &ctx())
            .await
            .unwrap();
        assert_eq!(out["stdout"], "hi");
        assert_eq!(out["code"], 7);
    }

    #[tokio::test]
    async fn real_system_runner_timeouts_and_kills() {
        let runner: Arc<dyn CommandRunner> = Arc::new(SystemRunner {
            limits: RunLimits {
                timeout: Duration::from_millis(150),
                ..RunLimits::default()
            },
        });
        let tool = ShellTool::new(runner);
        let err = tool
            .run(&json!({ "command": "sleep 30" }), &ctx())
            .await
            .unwrap_err();
        assert_eq!(err.code(), "TOOL_TIMEOUT");
    }
}
