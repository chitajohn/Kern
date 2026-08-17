//! Tool execution with policy (SPEC.md §8.2, decision D17).
//!
//! The engine hands every tool call to the executor, which applies, in order:
//! 1. **Argument validation** (cheap, fails fast before any concurrency).
//! 2. **Per-agent concurrency cap** (`runtime.max_concurrent_tools`, default 4).
//! 3. **Global concurrency cap** (`KERN_MAX_CONCURRENT_TOOLS`, default 16) —
//!    the guard against unbounded subprocess/thread creation across agents.
//! 4. **Timeout** (`runtime.tool_timeout`, default 30s) → `ToolError::Timeout`,
//!    which is fed to the model; the agent MAY continue (SPEC §8.2).
//!
//! Cancellation semantics: for in-process tools, dropping the
//! future cancels the work. The `shell` tool runs a child process
//! and must kill it on drop — that is the tool's responsibility, documented
//! in its contract.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::ToolError;
use crate::registry::{ToolContext, ToolRegistry};

/// Default per-agent tool concurrency (SPEC.md §9 `runtime.max_concurrent_tools`).
pub const DEFAULT_PER_AGENT_CAP: usize = 4;
/// Default global tool concurrency (`KERN_MAX_CONCURRENT_TOOLS`).
pub const DEFAULT_GLOBAL_CAP: usize = 16;

/// Per-agent executor: one instance per running agent (owns the per-agent
/// semaphore), sharing the global semaphore with every other agent.
pub struct ToolExecutor {
    registry: Arc<ToolRegistry>,
    global: Arc<Semaphore>,
    per_agent: Arc<Semaphore>,
}

impl ToolExecutor {
    /// `global_cap` should be shared across all agents; `per_agent_cap` is
    /// this agent's own limit.
    pub fn new(registry: Arc<ToolRegistry>, global: Arc<Semaphore>, per_agent_cap: usize) -> Self {
        Self {
            registry,
            global,
            per_agent: Arc::new(Semaphore::new(per_agent_cap)),
        }
    }

    /// A global semaphore shared by every agent's executor.
    pub fn global_semaphore(global_cap: usize) -> Arc<Semaphore> {
        Arc::new(Semaphore::new(global_cap))
    }

    /// Resolve + validate a tool without executing it (the engine uses this
    /// to classify the whole batch before recording).
    pub fn validate(&self, name: &str, args: &Value) -> Result<(), ToolError> {
        self.registry.validate(name, args)
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.registry.has(name)
    }

    /// The model-facing spec snapshot for the agent's configured tools.
    pub fn specs(
        &self,
        configured: &[String],
    ) -> Result<Vec<crate::registry::ToolSpec>, ToolError> {
        self.registry.specs(configured)
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// Run `name` with `args`, enforcing caps and timeout.
    pub async fn run(
        &self,
        name: &str,
        args: &Value,
        ctx: &ToolContext<'_>,
        timeout: Duration,
    ) -> Result<Value, ToolError> {
        // Fail fast on bad args before waiting on any semaphore.
        self.registry.validate(name, args)?;

        let tool = self
            .registry
            .get(name)
            .ok_or_else(|| ToolError::Unavailable(format!("tool '{name}' is not registered")))?;

        let _per_agent = acquire(&self.per_agent).await;
        let _global = acquire(&self.global).await;

        match tokio::time::timeout(timeout, tool.run(args, ctx)).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => Err(err),
            Err(_elapsed) => Err(ToolError::Timeout(timeout)),
        }
    }
}

async fn acquire(semaphore: &Arc<Semaphore>) -> OwnedSemaphorePermit {
    // Semaphores are never closed; an error would be a bug. Treat it as a
    // tool failure rather than panicking.
    semaphore
        .clone()
        .acquire_owned()
        .await
        .expect("tool semaphore never closed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use crate::builtins::noop::{NoopTool, SleepTool};
    use crate::registry::Tool;

    fn ctx<'a>() -> ToolContext<'a> {
        ToolContext {
            agent_id: "a",
            execution_id: "e",
            tool_call_id: "c",
        }
    }

    fn registry_with(fixtures: Vec<Arc<dyn Tool>>) -> Arc<ToolRegistry> {
        let mut registry = ToolRegistry::new();
        for tool in fixtures {
            registry.register(tool).unwrap();
        }
        Arc::new(registry)
    }

    /// A tool that tracks the number of concurrently running invocations and
    /// asserts it never exceeds `cap` (deterministic concurrency test).
    struct CapProbe {
        active: Arc<AtomicUsize>,
        max_seen: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for CapProbe {
        fn name(&self) -> &str {
            "cap_probe"
        }
        fn description(&self) -> &str {
            ""
        }
        fn input_schema(&self) -> &Value {
            static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            SCHEMA.get_or_init(|| serde_json::json!({ "type": "object" }))
        }
        async fn run(&self, _args: &Value, _ctx: &ToolContext<'_>) -> Result<Value, ToolError> {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_seen.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(serde_json::json!({ "ok": true }))
        }
    }

    #[tokio::test]
    async fn timeout_produces_tool_timeout() {
        let registry = registry_with(vec![Arc::new(SleepTool)]);
        let global = ToolExecutor::global_semaphore(DEFAULT_GLOBAL_CAP);
        let executor = ToolExecutor::new(registry, global, DEFAULT_PER_AGENT_CAP);

        let err = executor
            .run(
                "sleep",
                &serde_json::json!({ "ms": 500 }),
                &ctx(),
                Duration::from_millis(30),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), "TOOL_TIMEOUT");
        assert_eq!(err.kind(), "timeout");
    }

    #[tokio::test]
    async fn per_agent_cap_is_enforced() {
        let probe = Arc::new(CapProbe {
            active: Arc::new(AtomicUsize::new(0)),
            max_seen: Arc::new(AtomicUsize::new(0)),
        });
        let registry = registry_with(vec![probe.clone()]);
        let global = ToolExecutor::global_semaphore(DEFAULT_GLOBAL_CAP);
        let executor = Arc::new(ToolExecutor::new(registry, global, 2));

        let mut handles = Vec::new();
        for _ in 0..4 {
            let executor = Arc::clone(&executor);
            handles.push(tokio::spawn(async move {
                executor
                    .run(
                        "cap_probe",
                        &serde_json::json!({}),
                        &ctx(),
                        Duration::from_secs(5),
                    )
                    .await
                    .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(
            probe.max_seen.load(Ordering::SeqCst),
            2,
            "per-agent cap 2 exceeded"
        );
    }

    #[tokio::test]
    async fn global_cap_is_enforced() {
        let probe = Arc::new(CapProbe {
            active: Arc::new(AtomicUsize::new(0)),
            max_seen: Arc::new(AtomicUsize::new(0)),
        });
        let registry = registry_with(vec![probe.clone()]);
        // Global cap of 1 shared by two executors (two agents).
        let global = ToolExecutor::global_semaphore(1);
        let executor_a = Arc::new(ToolExecutor::new(
            Arc::clone(&registry),
            Arc::clone(&global),
            4,
        ));
        let executor_b = Arc::new(ToolExecutor::new(registry, global, 4));

        let mut handles = Vec::new();
        for executor in [executor_a, executor_b] {
            for _ in 0..2 {
                let executor = Arc::clone(&executor);
                handles.push(tokio::spawn(async move {
                    executor
                        .run(
                            "cap_probe",
                            &serde_json::json!({}),
                            &ctx(),
                            Duration::from_secs(5),
                        )
                        .await
                        .unwrap();
                }));
            }
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(
            probe.max_seen.load(Ordering::SeqCst),
            1,
            "global cap 1 exceeded"
        );
    }

    #[tokio::test]
    async fn unknown_tool_and_bad_args_fail_fast() {
        let registry = registry_with(vec![Arc::new(NoopTool)]);
        let global = ToolExecutor::global_semaphore(DEFAULT_GLOBAL_CAP);
        let executor = ToolExecutor::new(registry, global, DEFAULT_PER_AGENT_CAP);

        let err = executor
            .run(
                "ghost",
                &serde_json::json!({}),
                &ctx(),
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), "TOOL_UNAVAILABLE");

        let err = executor
            .run(
                "noop",
                &serde_json::json!({ "extra": 1 }),
                &ctx(),
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), "TOOL_INVALID_ARGUMENTS");
    }

    #[test]
    fn caps_default_to_spec_values() {
        assert_eq!(DEFAULT_PER_AGENT_CAP, 4);
        assert_eq!(DEFAULT_GLOBAL_CAP, 16);
    }
}
