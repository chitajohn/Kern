//! Runner task registry.
//!
//! One runner task per agent. The registry enforces the in-memory counterpart
//! of the store's one-active-execution constraint (a second live runner for an
//! agent is refused), tracks live tasks, and provides abort for
//! pause/terminate and for daemon shutdown.
//!
//! The runner *body* is supplied by the caller; the registry only manages
//! task lifetime. On natural completion
//! the task deregisters itself; on abort, the aborter removes it.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use crate::error::{ErrorCode, KernError, Result};

/// A live runner task's abort handle.
struct TaskHandle {
    abort: tokio::task::AbortHandle,
}

/// Shared agent_id → runner-task map with lifecycle helpers.
#[derive(Clone)]
pub struct TaskRegistry {
    inner: Arc<Mutex<HashMap<String, TaskHandle>>>,
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Spawn `fut` as the runner for `agent_id`. Fails with
    /// `EXECUTION_ALREADY_ACTIVE` if the agent already has a live runner.
    ///
    /// The spawned task deregisters itself on natural completion; an aborted
    /// task is removed by [`TaskRegistry::abort`].
    pub fn spawn<F>(&self, agent_id: String, fut: F) -> Result<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut tasks = self.inner.lock().expect("task registry mutex poisoned");
        if tasks.contains_key(&agent_id) {
            return Err(KernError::new(
                ErrorCode::ExecutionAlreadyActive,
                format!("agent {agent_id} already has a live runner"),
            ));
        }

        let registry = Arc::clone(&self.inner);
        let cleanup_id = agent_id.clone();
        let task = tokio::spawn(async move {
            fut.await;
            // Natural completion: deregister (no-op if aborted concurrently).
            let mut tasks = registry.lock().expect("task registry mutex poisoned");
            tasks.remove(&cleanup_id);
        });

        tasks.insert(
            agent_id,
            TaskHandle {
                abort: task.abort_handle(),
            },
        );
        Ok(())
    }

    /// Whether the agent currently has a live runner task.
    pub fn is_running(&self, agent_id: &str) -> bool {
        self.inner
            .lock()
            .expect("task registry mutex poisoned")
            .contains_key(agent_id)
    }

    /// Number of live runner tasks.
    pub fn active_count(&self) -> usize {
        self.inner
            .lock()
            .expect("task registry mutex poisoned")
            .len()
    }

    /// Abort the agent's runner task (pause/terminate). Returns whether a task
    /// was running. The aborted task is removed from the registry here —
    /// aborted tasks do not run their own cleanup.
    pub fn abort(&self, agent_id: &str) -> bool {
        let mut tasks = self.inner.lock().expect("task registry mutex poisoned");
        match tasks.remove(agent_id) {
            Some(handle) => {
                handle.abort.abort();
                true
            }
            None => false,
        }
    }

    /// Abort every live runner (daemon shutdown). The agents' durable state is
    /// untouched here; startup reconciliation (`Scheduler`) handles recovery.
    pub fn shutdown_all(&self) {
        let mut tasks = self.inner.lock().expect("task registry mutex poisoned");
        for (_, handle) in tasks.drain() {
            handle.abort.abort();
        }
    }
}

impl Default for TaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    #[tokio::test]
    async fn spawn_registers_and_completion_deregisters() {
        let registry = TaskRegistry::new();
        registry
            .spawn("a".to_string(), async {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            })
            .unwrap();
        assert!(registry.is_running("a"));
        assert_eq!(registry.active_count(), 1);
        // Wait for natural completion.
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        assert!(!registry.is_running("a"));
        assert_eq!(registry.active_count(), 0);
    }

    #[tokio::test]
    async fn duplicate_spawn_is_rejected() {
        let registry = TaskRegistry::new();
        registry
            .spawn("a".to_string(), async {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            })
            .unwrap();
        let err = registry.spawn("a".to_string(), async {}).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ExecutionAlreadyActive);
        registry.abort("a");
    }

    #[tokio::test]
    async fn abort_removes_and_stops_the_task() {
        let registry = TaskRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
        registry
            .spawn("a".to_string(), async move {
                // Never completes naturally; aborted from outside.
                let _ = tx.send(()).await;
                std::future::pending::<()>().await;
            })
            .unwrap();
        rx.recv().await.unwrap(); // task is now running
        assert!(registry.abort("a"));
        assert!(!registry.is_running("a"));
        assert!(!registry.abort("a"), "second abort is a no-op");
    }

    #[tokio::test]
    async fn shutdown_all_aborts_every_runner() {
        let registry = TaskRegistry::new();
        for name in ["a", "b", "c"] {
            registry
                .spawn(name.to_string(), async {
                    std::future::pending::<()>().await;
                })
                .unwrap();
        }
        assert_eq!(registry.active_count(), 3);
        registry.shutdown_all();
        assert_eq!(registry.active_count(), 0);
    }
}
