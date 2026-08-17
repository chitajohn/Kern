//! Recovery manager (ARCHITECTURE.md §9, SPEC.md §18.1) — the startup side of
//! the crash/restart proof.
//!
//! After `Scheduler::reconcile_interrupted` marks `starting|running|waiting`
//! agents as `recovering`, this module restores each one:
//!
//! 1. Find the interrupted (non-terminal) execution.
//! 2. Skip agents parked behind **pending** permission requests — they wait on
//!    a human, not a crash; the API resume delivers the decision and
//!    restarts the runner. They stay `recovering`, which is honest.
//! 3. Respect `runtime.auto_recover` (default true); disabled agents stay
//!    `recovering` for a manual `kern resume`.
//! 4. Restore the latest checkpoint (`checkpoint.restored` +
//!    `execution.restored` events) and re-spawn the runner via
//!    `Engine::resume_execution`, which re-drives the in-flight batch and
//!    dedups tool calls against the recorded rows (§11.2) — no recorded
//!    result is ever executed twice.
//! 5. An execution with NO checkpoint yet (crash in the first moments) resumes
//!    from an empty session with the default input — the pre-start input is
//!    not durable in v0.1 (documented limitation).
//! 6. Restore/parse failures fail the agent with a structured error
//!    (`checkpoint.failed` + `agent.failed`) instead of leaving it stuck.

use std::sync::Arc;

use crate::config::AgentSpec;
use crate::engine::Engine;
use crate::error::{ErrorCode, KernError, Result};
use crate::event::{payload, EventKind};
use crate::store::{ExecutionStatus, LifecycleState, Store};

/// Outcome of a startup recovery sweep.
#[derive(Debug, Default)]
pub struct RecoverySummary {
    /// Agents resumed (runner re-spawned from a checkpoint or empty session).
    pub recovered: usize,
    /// Agents left `recovering` (pending permission decision, or
    /// `auto_recover: false`).
    pub skipped: usize,
    /// Agents whose recovery failed and were transitioned to `failed`.
    pub failed: usize,
}

#[derive(Clone)]
pub struct RecoveryManager {
    engine: Engine,
    store: Arc<Store>,
}

impl RecoveryManager {
    pub fn new(engine: Engine) -> Self {
        Self {
            store: Arc::clone(&engine.store),
            engine,
        }
    }

    /// Recover every agent currently in `recovering`. Failures are surfaced
    /// (agent → failed) and counted, never swallowed.
    pub async fn recover_interrupted(&self) -> Result<RecoverySummary> {
        let store = Arc::clone(&self.store);
        let agents = tokio::task::spawn_blocking(move || store.list_agents())
            .await
            .map_err(|e| KernError::internal(format!("recovery scan task failed: {e}")))??;

        let mut summary = RecoverySummary::default();
        for agent in agents {
            if agent.state != LifecycleState::Recovering {
                continue;
            }
            match self.recover_one(&agent.id).await {
                Ok(RecoverAction::Resumed) => summary.recovered += 1,
                Ok(RecoverAction::Deferred) => summary.skipped += 1,
                Err(err) => {
                    summary.failed += 1;
                    tracing::error!(agent_id = %agent.id, error = %err, "recovery failed");
                    let executions = self.interrupted_execution(&agent.id).await.ok().flatten();
                    let execution_id = executions.map(|e| e.id).unwrap_or_default();
                    let _ = self.engine.fail_agent(&agent.id, &execution_id, &err).await;
                }
            }
        }
        Ok(summary)
    }

    /// The interrupted (non-terminal) execution of an agent, newest first.
    async fn interrupted_execution(
        &self,
        agent_id: &str,
    ) -> Result<Option<crate::store::Execution>> {
        let store = Arc::clone(&self.store);
        let agent_owned = agent_id.to_string();
        let executions =
            tokio::task::spawn_blocking(move || store.list_executions_for_agent(&agent_owned))
                .await
                .map_err(|e| KernError::internal(format!("execution scan task failed: {e}")))??;
        Ok(executions.into_iter().find(|e| {
            matches!(
                e.status,
                ExecutionStatus::Pending | ExecutionStatus::Running | ExecutionStatus::Interrupted
            )
        }))
    }

    /// Recover one agent. Returns whether the runner was re-spawned or the
    /// agent was deliberately left `recovering`.
    async fn recover_one(&self, agent_id: &str) -> Result<RecoverAction> {
        let store = Arc::clone(&self.store);
        let agent_owned = agent_id.to_string();
        let agent = tokio::task::spawn_blocking(move || store.get_agent(&agent_owned))
            .await
            .map_err(|e| KernError::internal(format!("agent read task failed: {e}")))??;
        let spec: AgentSpec = serde_json::from_value(agent.config.clone()).map_err(|e| {
            KernError::new(
                ErrorCode::ConfigInvalid,
                format!("stored config for agent {} is invalid: {e}", agent.name),
            )
        })?;

        let execution = self.interrupted_execution(agent_id).await?.ok_or_else(|| {
            KernError::new(
                ErrorCode::Internal,
                format!("agent {agent_id} is recovering but has no interrupted execution"),
            )
        })?;

        // A human is mid-decision: leave the agent parked. Recovery must not
        // re-drive (and re-ask) a batch the operator is already adjudicating.
        let store = Arc::clone(&self.store);
        let agent_owned = agent_id.to_string();
        let pending = tokio::task::spawn_blocking(move || {
            store.pending_permission_requests_for_agent(&agent_owned)
        })
        .await
        .map_err(|e| KernError::internal(format!("pending request scan failed: {e}")))??;
        if !pending.is_empty() {
            tracing::info!(
                agent_id,
                requests = pending.len(),
                "leaving agent recovering: pending permission requests need manual resume"
            );
            return Ok(RecoverAction::Deferred);
        }

        if !spec.runtime.auto_recover() && !agent.auto_recover {
            tracing::info!(
                agent_id,
                "auto_recover is disabled; leaving agent recovering"
            );
            return Ok(RecoverAction::Deferred);
        }

        let (state, pending_calls, checkpoint_id, input) =
            self.engine.prepare_resume(agent_id, &execution.id).await?;
        self.bus_emit_recovered(agent_id, &execution.id, checkpoint_id.as_deref())
            .await?;
        self.engine.spawn_resumed(
            agent_id,
            &execution.id,
            state,
            pending_calls,
            checkpoint_id,
            input,
        );

        Ok(RecoverAction::Resumed)
    }

    /// Manually resume a paused, recovering, or sleeping agent — the API's
    /// `POST /agents/{id}/resume`. Restores the latest checkpoint (or resumes
    /// from an empty session when there is none) and re-spawns the runner
    /// detached. Unlike daemon-restart recovery this does NOT skip agents
    /// with pending permission requests: an explicit resume re-parks the
    /// runner on them, so a grant/deny issued afterward applies. A sleeping
    /// agent resumes immediately (its durable wake is a convenience, not a
    /// contract).
    pub async fn resume_agent(&self, agent_id: &str) -> Result<()> {
        let store = Arc::clone(&self.store);
        let agent_owned = agent_id.to_string();
        let agent = tokio::task::spawn_blocking(move || store.get_agent(&agent_owned))
            .await
            .map_err(|e| KernError::internal(format!("agent read task failed: {e}")))??;
        if !matches!(
            agent.state,
            LifecycleState::Paused | LifecycleState::Recovering | LifecycleState::Sleeping
        ) {
            return Err(KernError::new(
                ErrorCode::InvalidTransition,
                format!(
                    "cannot resume agent {agent_id} from {}",
                    agent.state.as_str()
                ),
            ));
        }

        let execution = self.interrupted_execution(agent_id).await?.ok_or_else(|| {
            KernError::new(
                ErrorCode::Internal,
                format!("agent {agent_id} has no interrupted execution to resume"),
            )
        })?;
        let (state, pending_calls, checkpoint_id, input) =
            self.engine.prepare_resume(agent_id, &execution.id).await?;
        self.engine.spawn_resumed(
            agent_id,
            &execution.id,
            state,
            pending_calls,
            checkpoint_id,
            input,
        );
        Ok(())
    }

    async fn bus_emit_recovered(
        &self,
        agent_id: &str,
        execution_id: &str,
        checkpoint_id: Option<&str>,
    ) -> Result<()> {
        self.engine
            .bus
            .emit(
                EventKind::SchedulerRecoveredAgent,
                Some(agent_id),
                Some(execution_id),
                payload::scheduler_recovered_agent(
                    agent_id,
                    execution_id,
                    checkpoint_id.unwrap_or(""),
                ),
            )
            .await
            .map(|_| ())
    }
}

enum RecoverAction {
    Resumed,
    Deferred,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::{CheckpointManager, SessionState};
    use crate::event::EventBus;
    use crate::lifecycle::Lifecycle;
    use crate::store::{Agent, Execution, ExecutionStatus};
    use kern_model::mock::{MockProvider, ScriptedStep};
    use serde_json::json;

    struct Env {
        _dir: tempfile::TempDir,
        store: Arc<Store>,
        engine: Engine,
        lifecycle: Lifecycle,
        checkpoints: CheckpointManager,
    }

    fn env(script: Vec<ScriptedStep>) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let bus = EventBus::new(Arc::clone(&store));
        let mut gateway = kern_model::gateway::ModelGateway::new();
        gateway
            .register(Arc::new(MockProvider::new(script)))
            .unwrap();
        let engine = Engine::new(Arc::clone(&store), bus.clone(), Arc::new(gateway), 8);
        let lifecycle = Lifecycle::new(Arc::clone(&store), bus.clone());
        let checkpoints = CheckpointManager::new(Arc::clone(&store), bus);
        Env {
            _dir: dir,
            store,
            engine,
            lifecycle,
            checkpoints,
        }
    }

    fn agent(store: &Store, name: &str) -> String {
        let agent = Agent::new(
            name,
            serde_json::json!({
                "version": 1,
                "name": name,
                "model": { "provider": "mock", "model": "test" },
                "tools": ["noop"],
            }),
            crate::store::LifecycleState::Created,
        );
        store.create_agent(&agent).unwrap();
        agent.id.clone()
    }

    /// Drive an agent to `recovering` with one running execution and a
    /// checkpoint whose session ends after `steps` steps (the crash point).
    async fn interrupted(
        env: &Env,
        agent_id: &str,
        checkpoint_state: Option<SessionState>,
    ) -> String {
        env.lifecycle.start(agent_id).await.unwrap();
        let execution = Execution::new(agent_id, ExecutionStatus::Running);
        env.store.create_execution(&execution).unwrap();
        env.lifecycle
            .mark_started(agent_id, &execution.id)
            .await
            .unwrap();
        if let Some(state) = checkpoint_state {
            env.checkpoints
                .create(&crate::checkpoint::CheckpointRequest {
                    agent_id,
                    execution_id: &execution.id,
                    lifecycle_state: "running",
                    state: &state,
                    pending: &[],
                    runtime_meta: &json!({}),
                    retention: 10,
                })
                .await
                .unwrap();
        }
        env.lifecycle.recover(agent_id).await.unwrap();
        execution.id.clone()
    }

    async fn wait_for_state(store: &Store, agent_id: &str, state: crate::store::LifecycleState) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if store.get_agent(agent_id).unwrap().state == state {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "agent {agent_id} did not reach {state:?} within 5s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn recovers_an_interrupted_execution_from_its_checkpoint() {
        let env = env(vec![ScriptedStep::Finish("resumed!".into())]);
        let agent_id = agent(&env.store, "revive");
        let state = SessionState {
            messages: vec![kern_model::Message::user("the durable task")],
            history_trimmed: false,
            steps: 3,
            final_text: String::new(),
            checkpoints: 1,
            tool_calls: 0,
        };
        let execution_id = interrupted(&env, &agent_id, Some(state)).await;

        let summary = RecoveryManager::new(env.engine.clone())
            .recover_interrupted()
            .await
            .unwrap();
        assert_eq!(summary.recovered, 1);
        assert_eq!(summary.skipped, 0);
        assert_eq!(summary.failed, 0);

        wait_for_state(
            &env.store,
            &agent_id,
            crate::store::LifecycleState::Completed,
        )
        .await;
        let events = env.store.events_after(0, 500).unwrap();
        assert!(events.iter().any(|e| e.kind == "checkpoint.restored"));
        assert!(events.iter().any(|e| e.kind == "execution.restored"));
        assert!(events.iter().any(|e| e.kind == "scheduler.recovered_agent"));
        assert!(events.iter().any(|e| e.kind == "agent.resumed"));
        // The restored session carried the original task message.
        assert!(events.iter().any(|e| e.kind == "agent.completed"));
        // Same execution continued (no new execution was created).
        let executions = env.store.list_executions_for_agent(&agent_id).unwrap();
        assert_eq!(executions.len(), 1);
        assert_eq!(executions[0].id, execution_id);
        assert_eq!(executions[0].status, ExecutionStatus::Completed);
    }

    #[tokio::test]
    async fn defers_agents_waiting_on_permission_decisions() {
        let env = env(vec![ScriptedStep::Finish("x".into())]);
        let agent_id = agent(&env.store, "asker");
        let state = SessionState {
            messages: vec![kern_model::Message::user("task")],
            history_trimmed: false,
            steps: 1,
            final_text: String::new(),
            checkpoints: 1,
            tool_calls: 0,
        };
        let execution_id = interrupted(&env, &agent_id, Some(state)).await;
        env.store
            .create_permission_request(&agent_id, Some("c1"), "./ws", "read")
            .unwrap();

        let summary = RecoveryManager::new(env.engine.clone())
            .recover_interrupted()
            .await
            .unwrap();
        assert_eq!(summary.recovered, 0);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.failed, 0);
        assert_eq!(
            env.store.get_agent(&agent_id).unwrap().state,
            crate::store::LifecycleState::Recovering
        );
        // No runner was spawned; the execution stays untouched.
        assert!(!env.engine.is_running(&agent_id));
        assert_eq!(
            env.store.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Running
        );
    }

    #[tokio::test]
    async fn no_checkpoint_resume_seeds_the_durable_execution_input() {
        let env = env(vec![ScriptedStep::Finish("done".into())]);
        let agent_id = agent(&env.store, "crash-early");

        // Crash in the first moments: the execution row EXISTS with its input,
        // but no checkpoint was ever written. Recovery must resume with the
        // real task, not the default input.
        env.lifecycle.start(&agent_id).await.unwrap();
        let mut execution = Execution::new(agent_id.clone(), ExecutionStatus::Running);
        execution.input = Some("the real task, not the default".to_string());
        env.store.create_execution(&execution).unwrap();
        env.lifecycle
            .mark_started(&agent_id, &execution.id)
            .await
            .unwrap();
        env.lifecycle.recover(&agent_id).await.unwrap();

        let summary = RecoveryManager::new(env.engine.clone())
            .recover_interrupted()
            .await
            .unwrap();
        assert_eq!(summary.recovered, 1);
        assert_eq!(summary.failed, 0);
        wait_for_state(
            &env.store,
            &agent_id,
            crate::store::LifecycleState::Completed,
        )
        .await;

        // The completed run's final checkpoint session carries the durable
        // task message — proof the input survived the no-checkpoint crash.
        let checkpoints = env.store.list_checkpoints(&agent_id, 10).unwrap();
        let latest = checkpoints
            .first()
            .expect("the completed run writes a final checkpoint");
        let messages = latest.payload["messages"]
            .as_array()
            .expect("payload has messages");
        assert!(
            messages.iter().any(|m| {
                m["role"] == "user" && m["content"] == "the real task, not the default"
            }),
            "durable input must seed the resumed session, messages: {messages:?}"
        );
        // The execution completed on the SAME execution (no new one).
        let executions = env.store.list_executions_for_agent(&agent_id).unwrap();
        assert_eq!(executions.len(), 1);
        assert_eq!(executions[0].status, ExecutionStatus::Completed);
    }

    #[tokio::test]
    async fn failing_recovery_marks_the_agent_failed() {
        let env = env(vec![]);
        let agent_id = agent(&env.store, "corrupt");
        // A checkpoint whose payload cannot parse as the §7 shape.
        let execution_id = interrupted(&env, &agent_id, None).await;
        env.store
            .create_checkpoint(&crate::store::Checkpoint::new(
                agent_id.clone(),
                execution_id.clone(),
                1,
                json!({ "not": "a checkpoint" }),
            ))
            .unwrap();

        let summary = RecoveryManager::new(env.engine.clone())
            .recover_interrupted()
            .await
            .unwrap();
        assert_eq!(summary.failed, 1);
        assert_eq!(
            env.store.get_agent(&agent_id).unwrap().state,
            crate::store::LifecycleState::Failed
        );
        assert!(env.store.get_agent(&agent_id).unwrap().last_error.is_some());
        let events = env.store.events_after(0, 500).unwrap();
        assert!(events.iter().any(|e| e.kind == "agent.failed"));
    }
}
