//! Execution engine (SPEC.md §8) — the loop that binds the model gateway,
//! permission engine, tool system, and lifecycle together.
//!
//! `Engine` owns the shared runtime pieces; each `run_agent` spawns an
//! `AgentRunner` task that drives one execution through the §8.1 loop:
//!
//! ```text
//! load session → build request → model call → [finish | thinking | tool batch]
//!                                            └─ validate → record → classify →
//!                                               deny | ask (wait+resume) | execute (bounded)
//! ```
//!
//! Contract points worth calling out:
//! - **Model calls are at-least-once** (§8.1.3): the gateway retries transient
//!   errors with backoff; the run itself never re-requests after a successful
//!   response (recovery replays recorded results).
//! - **Tools never execute without an `Allow`.** Policy denies are recorded
//!   as terminal `Failed` tool rows (so dedup replays the denial, not
//!   the execution) and fed to the model as `PERMISSION_DENIED` results.
//! - **`ask` suspends the loop** (running → waiting, `agent.waiting`). The
//!   runner parks, polling the store until the operator's decision lands
//!   (250 ms; human-in-the-loop latency, crash-safe, no signal races), then
//!   granted calls execute and denied ones become denial results.
//! - **Bounded history** (§8.4): messages are trimmed by the
//!   ~4-chars-per-token approximation after every tool batch, never dropping
//!   the initial task message.
//! - Batch members run concurrently under the per-agent + global tool caps;
//!   ordering across calls is not preserved (§8.1.6e).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use kern_model::{
    gateway::ModelGateway, CompletionRequest, CompletionResponse, Message,
    ToolCall as ModelToolCall,
};
use kern_tool::{ToolContext, ToolError, ToolExecutor};
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::checkpoint::{CheckpointManager, CheckpointRequest, PendingCall, SessionState};
use crate::config::AgentSpec;
use crate::error::{ErrorCode, KernError, Result};
use crate::event::payload::{self, ModelOutcomeKind};
use crate::event::{EventBus, EventKind};
use crate::lifecycle::Lifecycle;
use crate::permissions::{Decision, Effect, FsAction, KeyAction, PermissionEngine};
use crate::store::model::{Execution, PermissionStatus, ToolCall, ToolCallStatus};
use crate::store::{ExecutionStatus, LifecycleState, Store};
use crate::tasks::TaskRegistry;
use crate::tools::{build_registry, memory_digest};

/// How often a parked (waiting-on-permission) runner re-checks the store.
/// Human-in-the-loop decisions take seconds; 250 ms polling is both
/// responsive and crash-safe (no in-memory signal can be lost or raced).
const PARK_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Default user message when a run is started without input (scheduled runs).
const DEFAULT_INPUT: &str = "Continue your assigned work.";

/// Per-agent control flags polled by the runner at its safe points (the API
/// drives these: `pause` and manual `checkpoint`). Flags are
/// one-shot: the runner clears them when it acts, so a late request applies
/// at the next safe point rather than accumulating.
#[derive(Default)]
struct AgentControls {
    pause: AtomicBool,
    checkpoint_now: AtomicBool,
}

/// What a tool-batch turn asks the caller to do next.
enum RunControl {
    Continue,
    /// The runner was asked to pause while handling the batch (checkpoint +
    /// `lifecycle.pause` already applied); the loop must stop.
    Paused {
        checkpoint_id: String,
    },
    /// The batch contained a durable sleep (≥ `runtime.durable_sleep_min`):
    /// the sleep call is already recorded terminal with its wake time and the
    /// session already carries the result. The runner must park the agent
    /// (`sleeping`, unload) until `wake_at`.
    Sleeping {
        wake_at: DateTime<Utc>,
    },
}

/// Outcome of parking on permission decisions.
enum ParkOutcome {
    AllDecided,
    Paused { checkpoint_id: String },
}

// ---------------------------------------------------------------------------
// Outcomes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum RunOutcome {
    Completed {
        final_text: String,
        steps: u64,
    },
    /// The runner checkpointed and transitioned to `paused` because a graceful
    /// shutdown was requested (daemon SIGINT/SIGTERM).
    Paused {
        checkpoint_id: String,
    },
    /// The runner parked the agent on a durable sleep until `wake_at`
    /// No runner is alive; the scheduler wakes it at that time.
    Sleeping {
        wake_at: DateTime<Utc>,
    },
    Failed {
        error: KernError,
    },
}

#[derive(Debug, Clone)]
pub struct RunSummary {
    pub execution_id: String,
    pub outcome: RunOutcome,
}

/// Outcome of one runner-liveness sweep.
#[derive(Debug, Default, Clone)]
pub struct SupervisorSummary {
    /// Agents the sweep failed with `RUNNER_LOST` (lifecycle said
    /// `starting|running|waiting`, no live runner, grace elapsed).
    pub failed: usize,
    /// Agents examined and left untouched (live runner, within grace, or not
    /// in an active lifecycle state).
    pub checked: usize,
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// The shared runtime: store, event bus, lifecycle, model gateway, the global
/// tool semaphore, checkpoint manager, and the runner task registry.
#[derive(Clone)]
pub struct Engine {
    pub(crate) store: Arc<Store>,
    pub(crate) bus: EventBus,
    lifecycle: Lifecycle,
    gateway: Arc<ModelGateway>,
    global_tools: Arc<Semaphore>,
    pub(crate) checkpoint: CheckpointManager,
    tasks: TaskRegistry,
    /// Graceful-shutdown signal; runners poll it at safe points.
    shutdown: tokio::sync::watch::Sender<bool>,
    /// Per-agent pause/checkpoint flags (API control). Entries are tiny and
    /// bounded by the agent count, so they are kept for the daemon's lifetime.
    controls: Arc<Mutex<HashMap<String, Arc<AgentControls>>>>,
}

impl Engine {
    pub fn new(
        store: Arc<Store>,
        bus: EventBus,
        gateway: Arc<ModelGateway>,
        global_tool_cap: usize,
    ) -> Self {
        let (shutdown, _) = tokio::sync::watch::channel(false);
        Self {
            lifecycle: Lifecycle::new(Arc::clone(&store), bus.clone()),
            checkpoint: CheckpointManager::new(Arc::clone(&store), bus.clone()),
            store,
            bus,
            gateway,
            global_tools: Arc::new(Semaphore::new(global_tool_cap)),
            tasks: TaskRegistry::new(),
            shutdown,
            controls: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get (or create) the agent's control flags.
    fn controls_for(&self, agent_id: &str) -> Arc<AgentControls> {
        let mut map = self.controls.lock().expect("controls mutex poisoned");
        Arc::clone(map.entry(agent_id.to_string()).or_default())
    }

    /// The model gateway (tests register their provider here before running).
    pub fn gateway(&self) -> &ModelGateway {
        &self.gateway
    }

    pub fn is_running(&self, agent_id: &str) -> bool {
        self.tasks.is_running(agent_id)
    }

    /// Number of live runner tasks (the daemon drains to zero on shutdown).
    pub fn active_count(&self) -> usize {
        self.tasks.active_count()
    }

    /// Ask the agent's runner to pause at its next safe point (checkpoint +
    /// transition to `paused`, then the runner task ends). Returns whether a
    /// runner is live; `false` means the caller must handle the state itself.
    pub fn request_pause(&self, agent_id: &str) -> bool {
        if !self.tasks.is_running(agent_id) {
            return false;
        }
        self.controls_for(agent_id)
            .pause
            .store(true, Ordering::Release);
        true
    }

    /// Ask the agent's runner to write a checkpoint at its next safe point
    /// (the API's manual checkpoint). Returns whether a runner is live.
    pub fn request_checkpoint(&self, agent_id: &str) -> bool {
        if !self.tasks.is_running(agent_id) {
            return false;
        }
        self.controls_for(agent_id)
            .checkpoint_now
            .store(true, Ordering::Release);
        true
    }

    /// Request a graceful shutdown: every runner checkpoints and pauses at its
    /// next safe point. Use [`Engine::shutdown`] to force-abort instead.
    pub fn request_shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// A receiver of the graceful-shutdown signal (the scheduler timer parks
    /// on it alongside the daemon's signal handlers).
    pub fn shutdown_receiver(&self) -> tokio::sync::watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    pub fn shutdown(&self) {
        self.tasks.shutdown_all();
    }

    /// Mark an agent failed with an error (recovery failure routing: a stuck
    /// `recovering` agent must not stay stuck forever).
    pub(crate) async fn fail_agent(
        &self,
        agent_id: &str,
        execution_id: &str,
        err: &KernError,
    ) -> Result<()> {
        self.lifecycle
            .fail(agent_id, execution_id, err)
            .await
            .map(|_| ())
    }

    /// Start one execution of `agent_id` and return immediately with the
    /// execution id (the API's `POST /agents/{id}/start`: 202
    /// `{ "execution_id" }`). The runner task is spawned detached; the
    /// caller observes completion through events/state. The execution row is
    /// created BEFORE the runner starts so the response carries the id; the
    /// store's one-active-execution partial index rejects a second concurrent
    /// start (`EXECUTION_ALREADY_ACTIVE`).
    pub async fn start_agent(&self, agent_id: &str, input: Option<&str>) -> Result<String> {
        if self.tasks.is_running(agent_id) {
            return Err(KernError::new(
                ErrorCode::ExecutionAlreadyActive,
                format!("agent {agent_id} already has an active execution"),
            ));
        }
        let agent = self.store.get_agent(agent_id)?;
        let spec: AgentSpec = serde_json::from_value(agent.config.clone()).map_err(|e| {
            KernError::new(
                ErrorCode::ConfigInvalid,
                format!("stored config for agent {} is invalid: {e}", agent.name),
            )
        })?;
        let workspace = workspace_dir(&self.store, &agent.name)?;
        let registry = build_registry(&spec, Arc::clone(&self.store), &workspace)?;
        let executor = Arc::new(ToolExecutor::new(
            registry,
            Arc::clone(&self.global_tools),
            spec.runtime.max_concurrent_tools() as usize,
        ));
        let permissions = PermissionEngine::from_config(&spec.permissions, &workspace)?;

        let mut execution = Execution::new(&agent.id, ExecutionStatus::Pending);
        execution.input = input.map(str::to_string);
        let execution_id = execution.id.clone();
        let store = Arc::clone(&self.store);
        store_blocking(store, move |s| s.create_execution(&execution)).await?;

        let mut runner = self.build_runner(agent, spec, workspace, executor, permissions);
        runner.input = input.map(str::to_string);
        runner.execution_id = execution_id.clone();
        let agent_owned = agent_id.to_string();
        let engine = self.clone();
        let execution_id_for_body = execution_id.clone();
        self.tasks.spawn(agent_owned.clone(), async move {
            let _ = run_runner_safely(&engine, &agent_owned, &execution_id_for_body, runner).await;
        })?;
        Ok(execution_id)
    }

    /// Abort the agent's runner (if any) and transition to `terminated`,
    /// marking the active execution `interrupted`. The lifecycle guard rejects
    /// a race where the agent completed between the read and the abort
    /// (`INVALID_TRANSITION`).
    pub async fn terminate_agent(&self, agent_id: &str) -> Result<()> {
        let execution_id = self.active_execution_id(agent_id).await?;
        self.tasks.abort(agent_id);
        self.lifecycle
            .terminate(agent_id, execution_id.as_deref())
            .await?;
        Ok(())
    }

    /// The id of the agent's active (pending|running) execution, if any.
    async fn active_execution_id(&self, agent_id: &str) -> Result<Option<String>> {
        let store = Arc::clone(&self.store);
        let agent_owned = agent_id.to_string();
        let executions =
            store_blocking(store, move |s| s.list_executions_for_agent(&agent_owned)).await?;
        Ok(executions
            .into_iter()
            .find(|e| {
                matches!(
                    e.status,
                    ExecutionStatus::Pending | ExecutionStatus::Running
                )
            })
            .map(|e| e.id))
    }

    /// Run one execution of `agent_id` from the current lifecycle state.
    /// Awaits the run to its terminal state and returns the summary. A second
    /// concurrent run for the same agent is refused
    /// (`EXECUTION_ALREADY_ACTIVE`).
    pub async fn run_agent(&self, agent_id: &str, input: Option<&str>) -> Result<RunSummary> {
        if self.tasks.is_running(agent_id) {
            return Err(KernError::new(
                ErrorCode::ExecutionAlreadyActive,
                format!("agent {agent_id} already has an active execution"),
            ));
        }
        let agent = self.store.get_agent(agent_id)?;
        let spec: AgentSpec = serde_json::from_value(agent.config.clone()).map_err(|e| {
            KernError::new(
                ErrorCode::ConfigInvalid,
                format!("stored config for agent {} is invalid: {e}", agent.name),
            )
        })?;
        let workspace = workspace_dir(&self.store, &agent.name)?;
        let registry = build_registry(&spec, Arc::clone(&self.store), &workspace)?;
        let executor = Arc::new(ToolExecutor::new(
            registry,
            Arc::clone(&self.global_tools),
            spec.runtime.max_concurrent_tools() as usize,
        ));
        let permissions = PermissionEngine::from_config(&spec.permissions, &workspace)?;

        let mut runner = self.build_runner(agent, spec, workspace, executor, permissions);
        runner.input = input.map(str::to_string);
        // Pre-create the execution so the id is stable before the runner
        // starts: the panic-containment path can then fail the correct
        // execution, and recovery can restore the task input.
        let mut execution = Execution::new(agent_id, ExecutionStatus::Pending);
        execution.input = runner.input.clone();
        let execution_id = execution.id.clone();
        let store = Arc::clone(&self.store);
        store_blocking(store, move |s| s.create_execution(&execution)).await?;
        runner.execution_id = execution_id;
        self.spawn_and_wait(agent_id, runner).await
    }

    /// Resume an interrupted execution from a restored checkpoint
    /// `state`/`pending` come from
    /// `CheckpointManager::restore`; the run continues the SAME execution id
    /// (so tool dedup scopes to the recorded rows) without creating a new one.
    pub async fn resume_execution(
        &self,
        agent_id: &str,
        execution_id: &str,
        state: SessionState,
        pending: Vec<PendingCall>,
        checkpoint_id: Option<String>,
        input: Option<String>,
    ) -> Result<RunSummary> {
        if self.tasks.is_running(agent_id) {
            return Err(KernError::new(
                ErrorCode::ExecutionAlreadyActive,
                format!("agent {agent_id} already has an active execution"),
            ));
        }
        let agent = self.store.get_agent(agent_id)?;
        let spec: AgentSpec = serde_json::from_value(agent.config.clone()).map_err(|e| {
            KernError::new(
                ErrorCode::ConfigInvalid,
                format!("stored config for agent {} is invalid: {e}", agent.name),
            )
        })?;
        let workspace = workspace_dir(&self.store, &agent.name)?;
        let registry = build_registry(&spec, Arc::clone(&self.store), &workspace)?;
        let executor = Arc::new(ToolExecutor::new(
            registry,
            Arc::clone(&self.global_tools),
            spec.runtime.max_concurrent_tools() as usize,
        ));
        let permissions = PermissionEngine::from_config(&spec.permissions, &workspace)?;

        let mut runner = self.build_runner(agent, spec, workspace, executor, permissions);
        runner.execution_id = execution_id.to_string();
        runner.session = Session {
            messages: state.messages,
            history_trimmed: state.history_trimmed,
            steps: state.steps,
            final_text: state.final_text,
            checkpoints: state.checkpoints,
            tool_calls: state.tool_calls,
            last_checkpoint_at: None,
        };
        runner.resumed = true;
        runner.resume_checkpoint_id = checkpoint_id;
        runner.pending_calls = pending;
        runner.input = input;

        self.spawn_and_wait(agent_id, runner).await
    }

    /// Build the per-agent runner from a loaded spec (shared setup between
    /// fresh runs and resumed runs).
    fn build_runner(
        &self,
        agent: crate::store::Agent,
        spec: AgentSpec,
        workspace: std::path::PathBuf,
        executor: Arc<ToolExecutor>,
        permissions: PermissionEngine,
    ) -> AgentRunner {
        let runtime_meta = json!({
            "provider": spec.model.provider.as_str(),
            "model": spec.model.model,
        });
        let tool_timeout = spec.runtime.tool_timeout().as_std();
        let controls = self.controls_for(&agent.id);
        AgentRunner {
            store: Arc::clone(&self.store),
            bus: self.bus.clone(),
            lifecycle: self.lifecycle.clone(),
            gateway: Arc::clone(&self.gateway),
            checkpoint: self.checkpoint.clone(),
            agent,
            spec,
            execution_id: String::new(),
            workspace,
            executor,
            permissions,
            session: Session::default(),
            tool_timeout,
            deadline: None,
            runtime_meta,
            shutdown_rx: self.shutdown.subscribe(),
            controls,
            input: None,
            resumed: false,
            resume_checkpoint_id: None,
            pending_calls: Vec::new(),
        }
    }

    /// Spawn the runner as the agent's live task and await its summary.
    async fn spawn_and_wait(&self, agent_id: &str, runner: AgentRunner) -> Result<RunSummary> {
        let (done_tx, mut done_rx) = tokio::sync::watch::channel(None);
        let agent_id_owned = agent_id.to_string();
        let execution_id = runner.execution_id.clone();
        let execution_id_for_task = execution_id.clone();
        let engine = self.clone();
        if let Err(err) = self.tasks.spawn(agent_id_owned.clone(), async move {
            let summary =
                run_runner_safely(&engine, &agent_id_owned, &execution_id_for_task, runner).await;
            let _ = done_tx.send(Some(summary));
        }) {
            // The runner never started: the pre-created
            // execution row would linger `pending` and block every future
            // run of this agent. Fail it so the index releases.
            let store = Arc::clone(&self.store);
            let execution_id_owned = execution_id.clone();
            let _ = store_blocking(store, move |s| {
                s.fail_pending_execution(&execution_id_owned)
            })
            .await;
            return Err(err);
        }

        done_rx
            .changed()
            .await
            .map_err(|_| KernError::internal("runner task ended without a summary"))?;
        let summary = done_rx.borrow().clone();
        summary.ok_or_else(|| KernError::internal("runner task ended without a summary"))
    }

    /// Runner-liveness supervision: an agent whose lifecycle says
    /// `starting|running|waiting` but whose runner task is gone is failed
    /// with `RUNNER_LOST` once the grace period has elapsed. This catches
    /// the paths the crash-recovery sweep cannot see: a spawn that never ran,
    /// a runner task lost to an internal bug, or a hang that outlives the
    /// in-process panic containment — an execution must never stay active
    /// forever with no live runner. `waiting` is included because the park
    /// poll that seals expired permission requests lives inside the runner;
    /// without it, an operator's late decision could never be observed.
    /// The fail transition is CAS'd on the current lifecycle state, so an
    /// agent that legitimately ended between the read and the transition is
    /// never double-failed.
    pub async fn supervisor_sweep(&self, grace: Duration) -> Result<SupervisorSummary> {
        let store = Arc::clone(&self.store);
        let agents = store_blocking(store, |s| s.list_agents()).await?;
        let mut summary = SupervisorSummary::default();
        let grace = chrono::Duration::from_std(grace).unwrap_or_default();

        for agent in agents {
            if !matches!(
                agent.state,
                LifecycleState::Starting | LifecycleState::Running | LifecycleState::Waiting
            ) {
                continue;
            }
            if self.tasks.is_running(&agent.id) {
                continue;
            }
            let store = Arc::clone(&self.store);
            let agent_id = agent.id.clone();
            let executions =
                store_blocking(store, move |s| s.list_executions_for_agent(&agent_id)).await?;
            let Some(execution) = executions.into_iter().find(|e| {
                matches!(
                    e.status,
                    ExecutionStatus::Pending | ExecutionStatus::Running
                )
            }) else {
                continue;
            };
            // Anchor: the persisted execution start for `running`; the last
            // transition for `starting` (no execution is started yet).
            let anchor = execution.started_at.unwrap_or(agent.updated_at);
            if Utc::now() - anchor < grace {
                continue;
            }
            summary.checked += 1;
            let err = KernError::new(
                ErrorCode::RunnerLost,
                format!(
                    "runner for agent {} is no longer alive but execution {} is still {}; \
                     lifecycle state is {}",
                    agent.name,
                    execution.id,
                    execution.status.as_str(),
                    agent.state.as_str()
                ),
            );
            match self.fail_agent(&agent.id, &execution.id, &err).await {
                Ok(_) => {
                    tracing::warn!(
                        agent_id = %agent.id,
                        execution_id = %execution.id,
                        error = %err,
                        "supervisor sweep failed a stuck execution"
                    );
                    summary.failed += 1;
                }
                Err(e) => {
                    // The agent transitioned between the read and the CAS
                    // (completed/paused/terminated) — the runner ended
                    // normally; nothing to do.
                    tracing::warn!(
                        agent_id = %agent.id,
                        error = %e,
                        "supervisor sweep could not fail agent (concurrent transition?)"
                    );
                }
            }
        }
        Ok(summary)
    }

    /// Restore the execution's latest checkpoint for resuming. A missing
    /// checkpoint (crash in the first moments) resumes from an empty session
    /// seeded with the execution's durable input (schema v3) — the real task
    /// is never silently lost. Shared by crash recovery, manual resume, and
    /// the scheduler's wake path.
    pub(crate) async fn prepare_resume(
        &self,
        agent_id: &str,
        execution_id: &str,
    ) -> Result<(
        SessionState,
        Vec<PendingCall>,
        Option<String>,
        Option<String>,
    )> {
        // A resume supersedes any durable sleep: clear the stale wake time
        // (crash between set_wake_at and park, or an early manual wake). If
        // the spawn fails afterward, the fail transition (sleeping → failed)
        // keeps the agent observable — it never sleeps forever silently.
        let store = Arc::clone(&self.store);
        let execution_id_owned = execution_id.to_string();
        store_blocking(store, move |s| s.set_wake_at(&execution_id_owned, None)).await?;
        match self.checkpoint.restore(agent_id, execution_id).await {
            Ok(restored) => Ok((
                restored.state,
                restored.pending,
                Some(restored.checkpoint_id),
                self.execution_input(execution_id).await?,
            )),
            Err(err) if err.code() == ErrorCode::CheckpointNotFound => {
                tracing::warn!(
                    agent_id,
                    execution_id,
                    "no checkpoint to restore; resuming from an empty session with the \
                     durable execution input"
                );
                Ok((
                    SessionState::default(),
                    Vec::new(),
                    None,
                    self.execution_input(execution_id).await?,
                ))
            }
            Err(err) => Err(err),
        }
    }

    /// The execution's persisted pre-start input (schema v3), if any.
    async fn execution_input(&self, execution_id: &str) -> Result<Option<String>> {
        let store = Arc::clone(&self.store);
        let execution_id = execution_id.to_string();
        let execution = store_blocking(store, move |s| s.get_execution(&execution_id)).await?;
        Ok(execution.input)
    }

    /// Spawn the runner detached on a restored session (dedup re-drives the
    /// in-flight batch against the recorded rows). Errors after spawn fail
    /// the agent with a structured error rather than leaving it stuck.
    pub(crate) fn spawn_resumed(
        &self,
        agent_id: &str,
        execution_id: &str,
        state: SessionState,
        pending_calls: Vec<PendingCall>,
        checkpoint_id: Option<String>,
        input: Option<String>,
    ) {
        let engine = self.clone();
        let agent_owned = agent_id.to_string();
        let execution_id = execution_id.to_string();
        tokio::spawn(async move {
            match engine
                .resume_execution(
                    &agent_owned,
                    &execution_id,
                    state,
                    pending_calls,
                    checkpoint_id,
                    input,
                )
                .await
            {
                Ok(_) => {}
                Err(err) => {
                    tracing::error!(
                        agent_id = %agent_owned,
                        execution_id = %execution_id,
                        error = %err,
                        "resuming execution failed after restore"
                    );
                    let _ = engine.fail_agent(&agent_owned, &execution_id, &err).await;
                }
            }
        });
    }

    /// Deliver a permission decision to a waiting agent: resolves the
    /// lifecycle transition (waiting → running) for every decided request of
    /// the agent and lets the parked runner observe it. Idempotent — a second
    /// call for an already-resolved agent is a no-op warning.
    pub async fn resume_agent(&self, agent_id: &str) -> Result<()> {
        let store = Arc::clone(&self.store);
        let agent_id_owned = agent_id.to_string();
        let requests = store_blocking(store, move |s| {
            s.decided_permission_requests_for_agent(&agent_id_owned)
        })
        .await?;
        for req in requests {
            let granted = req.status == PermissionStatus::Granted;
            let reason = if granted {
                "granted by operator"
            } else {
                "denied by operator"
            };
            match self
                .lifecycle
                .resolve_wait(agent_id, &req.id, &req.resource, granted, reason)
                .await
            {
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        "resolve_wait for request {} failed (already resolved?): {e}",
                        req.id
                    );
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Runner panic containment
// ---------------------------------------------------------------------------

/// Panic containment: a runner body that panics — a bug in Kern or
/// in a provider adapter — must not leave the agent `running` with a dead
/// runner, leak the task-registry entry, or hang an awaiter. The body runs in
/// an inner task (tokio isolates the panic at the task boundary); the outer
/// task observes the `JoinError`, fails the execution with `RUNNER_PANIC`, and
/// the registry deregisters on natural completion. Aborting the outer task
/// (pause / terminate / shutdown) aborts the inner task via the guard, so no
/// orphaned runner survives an abort.
struct RunnerAbortGuard {
    abort: tokio::task::AbortHandle,
}

impl Drop for RunnerAbortGuard {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

async fn run_runner_safely(
    engine: &Engine,
    agent_id: &str,
    execution_id: &str,
    runner: AgentRunner,
) -> RunSummary {
    let inner = tokio::spawn(async move { runner.run().await });
    let _guard = RunnerAbortGuard {
        abort: inner.abort_handle(),
    };
    match inner.await {
        Ok(summary) => summary,
        Err(join) => {
            let detail = match join.try_into_panic() {
                Ok(panic) => panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "non-string panic payload".to_string()),
                Err(join) => format!(
                    "runner task ended abnormally (cancelled: {})",
                    join.is_cancelled()
                ),
            };
            let err = KernError::new(
                ErrorCode::RunnerPanic,
                format!("agent runner panicked: {detail}"),
            );
            tracing::error!(
                agent_id = %agent_id,
                execution_id = %execution_id,
                error = %err,
                "runner task panicked; failing the execution"
            );
            if !execution_id.is_empty() {
                let _ = engine.lifecycle.fail(agent_id, execution_id, &err).await;
            }
            RunSummary {
                execution_id: execution_id.to_string(),
                outcome: RunOutcome::Failed { error: err },
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// The in-memory run state; `SessionState` is the checkpoint-serializable
/// projection of it.
#[derive(Debug, Default)]
struct Session {
    messages: Vec<Message>,
    history_trimmed: bool,
    steps: u64,
    final_text: String,
    /// Checkpoints created so far in this execution.
    checkpoints: u64,
    /// Fresh tool calls issued so far in this execution (the execution budget;
    /// serialized into every checkpoint so a recovered run keeps its budget).
    tool_calls: u64,
    /// When the last checkpoint was written (interval scheduling is per
    /// wall-clock, so this is deliberately NOT serialized).
    last_checkpoint_at: Option<Instant>,
}

/// Bound `messages` to `max_history_tokens` using the §8.4 character
/// approximation (~4 chars ≈ 1 token). Oldest messages are dropped first;
/// the initial task message (index 0) is never dropped. Returns whether
/// anything was trimmed.
pub fn trim_messages(messages: &mut Vec<Message>, max_history_tokens: u64) -> bool {
    if messages.len() <= 1 {
        return false;
    }
    let max_chars = (max_history_tokens as usize).saturating_mul(4);
    let mut trimmed = false;
    while messages.len() > 1 && total_chars(messages) > max_chars {
        messages.remove(1);
        trimmed = true;
    }
    trimmed
}

fn total_chars(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| {
            m.content.len()
                + m.tool_calls
                    .iter()
                    .map(|c| c.name.len() + c.arguments.to_string().len() + 32)
                    .sum::<usize>()
                + m.tool_call_id.as_deref().map(str::len).unwrap_or(0)
        })
        .sum()
}

// ---------------------------------------------------------------------------
// AgentRunner
// ---------------------------------------------------------------------------

struct AgentRunner {
    store: Arc<Store>,
    bus: EventBus,
    lifecycle: Lifecycle,
    gateway: Arc<ModelGateway>,
    checkpoint: CheckpointManager,
    agent: crate::store::Agent,
    spec: AgentSpec,
    execution_id: String,
    workspace: std::path::PathBuf,
    executor: Arc<ToolExecutor>,
    permissions: PermissionEngine,
    session: Session,
    tool_timeout: Duration,
    /// Monotonic wall-clock deadline for this execution
    /// (anchored to the persisted `started_at`; `None` = unbounded).
    deadline: Option<Instant>,
    /// §7 `runtime_meta` (provider/model) written into every checkpoint.
    runtime_meta: Value,
    /// Graceful-shutdown signal (polled between steps).
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
    /// The task message for fresh runs (lost on no-checkpoint resume — the
    /// documented fallback uses `DEFAULT_INPUT` instead).
    input: Option<String>,
    /// True when this run continues a restored execution.
    resumed: bool,
    /// The restored checkpoint id (carried into `agent.resumed`).
    resume_checkpoint_id: Option<String>,
    /// The restored in-flight batch to re-drive before the next model call.
    pending_calls: Vec<PendingCall>,
    /// Per-agent control flags (pause / manual checkpoint).
    controls: Arc<AgentControls>,
}

/// The §8.1 per-call classification.
#[derive(Debug)]
enum CallDecision {
    Allow,
    Ask,
    Deny { reason: String },
}

/// The `host[:port]` form the permission engine matches against, with the
/// default port filled from the URL scheme (so `https://api.github.com`
/// becomes `api.github.com:443` and a `api.github.com:443` rule matches).
/// Returns `None` for unparseable or host-less URLs. IPv6 literals keep
/// host-only form: a v6+port request only matches port-less rules
/// (fail-closed; documented limitation in ARCHITECTURE.md).
fn http_host_port(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    match parsed.port_or_known_default() {
        Some(port) if !host.contains(':') => Some(format!("{host}:{port}")),
        _ => Some(host.to_string()),
    }
}

impl AgentRunner {
    async fn run(mut self) -> RunSummary {
        tracing::debug!(
            agent = %self.agent.name,
            workspace = %self.workspace.display(),
            resumed = self.resumed,
            "agent run starting"
        );
        let outcome = self.run_inner().await;
        match outcome {
            Ok(outcome) => RunSummary {
                execution_id: self.execution_id.clone(),
                outcome,
            },
            Err(err) => {
                // The error is structured and evented; the failed transition
                // attaches it and emits execution.failed + agent.failed.
                let _ = self
                    .lifecycle
                    .fail(&self.agent.id, &self.execution_id, &err)
                    .await;
                RunSummary {
                    execution_id: self.execution_id.clone(),
                    outcome: RunOutcome::Failed { error: err },
                }
            }
        }
    }

    async fn run_inner(&mut self) -> Result<RunOutcome> {
        if self.resumed {
            // Continue the interrupted execution: recovering → running, then
            // re-drive the in-flight batch captured by the last checkpoint
            // (§7 pending_tool_calls) before issuing any new model call.
            // A crash before the FIRST checkpoint left no session at all —
            // seed it from the durable execution input (or the default),
            // so recovery never silently loses the task.
            if self.session.messages.is_empty() {
                let task = self
                    .input
                    .clone()
                    .unwrap_or_else(|| DEFAULT_INPUT.to_string());
                self.session.messages.push(Message::user(task));
            }
            self.lifecycle
                .resume(&self.agent.id, self.resume_checkpoint_id.as_deref())
                .await?;
            if !self.pending_calls.is_empty() {
                let pending = std::mem::take(&mut self.pending_calls);
                let calls: Vec<ModelToolCall> = pending
                    .into_iter()
                    .map(|p| ModelToolCall {
                        id: p.id,
                        name: p.name,
                        arguments: p.args,
                    })
                    .collect();
                match self.handle_tool_batch(calls).await? {
                    RunControl::Paused { checkpoint_id } => {
                        return Ok(RunOutcome::Paused { checkpoint_id })
                    }
                    RunControl::Sleeping { wake_at } => {
                        self.sleep_until(wake_at).await?;
                        return Ok(RunOutcome::Sleeping { wake_at });
                    }
                    RunControl::Continue => {}
                }
                self.trim_history();
            }
        } else {
            self.lifecycle.start(&self.agent.id).await?;
            // `start_agent` and `run_agent` pre-create the execution row so
            // the id is stable before the runner starts (the panic-containment
            // path can fail the right execution); this branch is the safety
            // net for any future caller that does not.
            if self.execution_id.is_empty() {
                let mut execution = Execution::new(&self.agent.id, ExecutionStatus::Pending);
                execution.input = self.input.clone();
                let execution_id = execution.id.clone();
                let store = Arc::clone(&self.store);
                store_blocking(store, move |s| s.create_execution(&execution)).await?;
                self.execution_id = execution_id;
            }
            self.lifecycle
                .mark_started(&self.agent.id, &self.execution_id)
                .await?;

            // The task message seeds the conversation (§8.1: load session).
            self.session.messages.push(Message::user(
                self.input.as_deref().unwrap_or(DEFAULT_INPUT),
            ));

            // An early checkpoint so even a crash before the first batch has
            // durable state (recovery resumes from it with the task message).
            self.checkpoint_now("running", &[]).await?;
        }

        // The wall-clock deadline anchors to the execution's
        // persisted start, so a recovered run cannot restart its clock.
        if let Some(max_duration) = self.spec.runtime.max_duration() {
            let store = Arc::clone(&self.store);
            let execution_id = self.execution_id.clone();
            let execution = store_blocking(store, move |s| s.get_execution(&execution_id)).await?;
            let started = execution.started_at.unwrap_or_else(Utc::now);
            let elapsed = (Utc::now() - started).to_std().unwrap_or_default();
            let remaining = max_duration.as_std().saturating_sub(elapsed);
            self.deadline = Some(Instant::now() + remaining);
        }

        loop {
            if *self.shutdown_rx.borrow() {
                return self.shutdown_gracefully().await;
            }
            if self.controls.pause.swap(false, Ordering::AcqRel) {
                return self.shutdown_gracefully().await;
            }
            if self.controls.checkpoint_now.swap(false, Ordering::AcqRel) {
                self.checkpoint_now("running", &[]).await?;
            }
            self.checkpoint_interval_if_due().await?;
            self.check_deadline()?;
            if self.session.steps >= self.spec.runtime.max_steps() as u64 {
                return Err(KernError::new(
                    ErrorCode::StepLimitExceeded,
                    format!(
                        "agent exceeded runtime.max_steps ({})",
                        self.spec.runtime.max_steps()
                    ),
                ));
            }
            let response = self.model_call().await?;
            self.session.steps += 1;
            match response {
                CompletionResponse::Finish { text, .. } => {
                    let final_text = text.clone();
                    if !text.is_empty() {
                        self.session.messages.push(Message::assistant(text));
                    }
                    self.session.final_text = final_text.clone();
                    // Final checkpoint (§8.1 step 4): the completed execution
                    // always has a checkpoint behind it.
                    self.checkpoint_now("completed", &[]).await?;
                    self.lifecycle
                        .complete(
                            &self.agent.id,
                            &self.execution_id,
                            &final_text,
                            self.session.steps,
                            self.session.checkpoints,
                        )
                        .await?;
                    return Ok(RunOutcome::Completed {
                        final_text,
                        steps: self.session.steps,
                    });
                }
                CompletionResponse::Thinking(text) => {
                    // No state change, no checkpoint (§8.1.5); the text is
                    // surfaced for observability only.
                    self.bus
                        .emit(
                            EventKind::AgentThinking,
                            Some(&self.agent.id),
                            Some(&self.execution_id),
                            payload::agent_thinking(&self.agent.id, self.session.steps, &text),
                        )
                        .await?;
                }
                CompletionResponse::ToolCalls(calls) => {
                    match self.handle_tool_batch(calls).await? {
                        RunControl::Paused { checkpoint_id } => {
                            return Ok(RunOutcome::Paused { checkpoint_id })
                        }
                        RunControl::Sleeping { wake_at } => {
                            self.sleep_until(wake_at).await?;
                            return Ok(RunOutcome::Sleeping { wake_at });
                        }
                        RunControl::Continue => {}
                    }
                    self.trim_history();
                }
            }
        }
    }

    /// Write a checkpoint of the current session. Increments the per-run
    /// counter and resets the interval timer. Propagates errors: a durable
    /// runtime must not continue without durable state.
    async fn checkpoint_now(
        &mut self,
        lifecycle_state: &str,
        pending: &[PendingCall],
    ) -> Result<crate::store::Checkpoint> {
        let state = SessionState {
            messages: self.session.messages.clone(),
            history_trimmed: self.session.history_trimmed,
            steps: self.session.steps,
            final_text: self.session.final_text.clone(),
            checkpoints: self.session.checkpoints,
            tool_calls: self.session.tool_calls,
        };
        let checkpoint = self
            .checkpoint
            .create(&CheckpointRequest {
                agent_id: &self.agent.id,
                execution_id: &self.execution_id,
                lifecycle_state,
                state: &state,
                pending,
                runtime_meta: &self.runtime_meta,
                retention: self.spec.runtime.checkpoint_retention(),
            })
            .await?;
        self.session.checkpoints += 1;
        self.session.last_checkpoint_at = Some(Instant::now());
        Ok(checkpoint)
    }

    /// Fail the run once the wall-clock deadline passed
    /// (checked between steps and while parked for approval).
    fn check_deadline(&self) -> Result<()> {
        if let Some(deadline) = self.deadline {
            if Instant::now() >= deadline {
                return Err(KernError::new(
                    ErrorCode::RunDurationExceeded,
                    format!(
                        "execution exceeded runtime.max_duration ({} ms)",
                        self.spec
                            .runtime
                            .max_duration()
                            .expect("a deadline implies a configured duration")
                            .as_millis()
                    ),
                ));
            }
        }
        Ok(())
    }

    /// `checkpoint_interval` (default 30s) elapsed since the last checkpoint
    /// ⇒ checkpoint between turns.
    async fn checkpoint_interval_if_due(&mut self) -> Result<()> {
        let interval = self.spec.runtime.checkpoint_interval().as_std();
        let due = matches!(self.session.last_checkpoint_at, Some(t) if t.elapsed() >= interval);
        if due {
            self.checkpoint_now("running", &[]).await?;
        }
        Ok(())
    }

    /// Park the agent on a durable sleep. The wake time is
    /// persisted BEFORE the lifecycle transition so a crash between the two
    /// leaves a `running` agent that recovery resumes (benign early wake),
    /// never a sleeping agent with no wake time. The runner task ends; the
    /// scheduler wakes the agent at `wake_at`.
    async fn sleep_until(&mut self, wake_at: DateTime<Utc>) -> Result<()> {
        let store = Arc::clone(&self.store);
        let execution_id = self.execution_id.clone();
        store_blocking(store, move |s| s.set_wake_at(&execution_id, Some(wake_at))).await?;
        self.lifecycle
            .park(&self.agent.id, &wake_at.to_rfc3339())
            .await?;
        Ok(())
    }

    /// A `sleep` call at or above `runtime.durable_sleep_min` returns its
    /// wake time (the agent parks instead of blocking the runner). Shorter
    /// sleeps (and every other tool) return `None` and execute normally.
    fn durable_sleep_call(&self, call: &ModelToolCall) -> Option<DateTime<Utc>> {
        if call.name != "sleep" {
            return None;
        }
        let ms = call.arguments.get("ms").and_then(Value::as_u64)?;
        if ms < self.spec.runtime.durable_sleep_min().as_millis() {
            return None;
        }
        let capped = ms.min(i64::MAX as u64) as i64;
        Some(Utc::now() + chrono::Duration::milliseconds(capped))
    }

    /// Graceful shutdown at a safe point: final checkpoint, pause the agent
    /// (the runner task then deregisters). Recovery never touches paused
    /// agents — a shutdown is a deliberate stop, not an interruption.
    async fn shutdown_gracefully(&mut self) -> Result<RunOutcome> {
        tracing::info!(
            agent_id = %self.agent.id,
            "graceful shutdown: checkpointing and pausing"
        );
        let checkpoint = self.checkpoint_now("paused", &[]).await?;
        self.lifecycle.pause(&self.agent.id, &checkpoint.id).await?;
        Ok(RunOutcome::Paused {
            checkpoint_id: checkpoint.id,
        })
    }

    /// Build the completion request (§8.1.2): system prompt (+ memory digest
    /// + durable variables) + bounded history + configured tool specs.
    async fn build_request(&self) -> Result<CompletionRequest> {
        let mut messages = vec![Message::system(self.system_prompt().await?)];
        messages.extend(self.session.messages.iter().cloned());

        let tools = self
            .executor
            .specs(&self.spec.tools)
            .map_err(|e| tool_error_to_kern(&e))?;

        Ok(CompletionRequest {
            provider: self.spec.model.provider.as_str().to_string(),
            model: self.spec.model.model.clone(),
            messages,
            tools,
            max_tokens: self.spec.model.max_tokens.map(|t| t as u32),
            temperature: Some(self.spec.model.temperature),
            timeout: self.spec.model.timeout.as_ref().map(|d| d.as_std()),
            retries: Some(self.spec.runtime.model_retries()),
        })
    }

    async fn system_prompt(&self) -> Result<String> {
        let mut prompt = format!(
            "You are an autonomous agent named \"{}\" running on the Kern runtime.\n",
            self.agent.name
        );
        if let Some(description) = &self.spec.description {
            prompt.push_str(&format!("Description: {description}\n"));
        }
        prompt.push_str(
            "Use the provided tools to accomplish your task; tool results are returned to you \
             as messages. Never assume an action succeeded without a tool result.\n",
        );
        if self.spec.memory.enabled && self.spec.memory.inject_digest {
            let store = Arc::clone(&self.store);
            let agent_id = self.agent.id.clone();
            let entries = store_blocking(store, move |s| s.memory_list(&agent_id, None)).await?;
            prompt.push_str(&memory_digest(
                &entries
                    .into_iter()
                    .map(|e| kern_tool::MemoryEntry {
                        key: e.key,
                        value: e.value,
                        description: e.description,
                    })
                    .collect::<Vec<_>>(),
            ));
        }
        Ok(prompt)
    }

    /// One model call with §8.1 events. The gateway applies timeout/retry;
    /// any error is evented as `model.failed` and propagated (the run fails
    /// unless the caller chooses otherwise).
    async fn model_call(&mut self) -> Result<CompletionResponse> {
        let step = self.session.steps + 1;
        let request = self.build_request().await?;
        self.bus
            .emit(
                EventKind::ModelRequested,
                Some(&self.agent.id),
                Some(&self.execution_id),
                payload::model_requested(
                    self.spec.model.provider.as_str(),
                    &self.spec.model.model,
                    step,
                ),
            )
            .await?;
        let started = Instant::now();
        let result = self.gateway.complete(&request).await;
        let latency_ms = started.elapsed().as_millis() as u64;

        match result {
            Ok(CompletionResponse::Finish { reason, text }) => {
                self.bus
                    .emit(
                        EventKind::ModelCompleted,
                        Some(&self.agent.id),
                        Some(&self.execution_id),
                        payload::model_completed(
                            self.spec.model.provider.as_str(),
                            &self.spec.model.model,
                            ModelOutcomeKind::Finish,
                            latency_ms,
                        ),
                    )
                    .await?;
                Ok(CompletionResponse::Finish { reason, text })
            }
            Ok(CompletionResponse::Thinking(text)) => {
                // The §6 catalog's model.completed kind is finish|tool_call,
                // so a thinking turn emits only agent.thinking (§8.1.5).
                Ok(CompletionResponse::Thinking(text))
            }
            Ok(CompletionResponse::ToolCalls(calls)) => {
                self.bus
                    .emit(
                        EventKind::ModelCompleted,
                        Some(&self.agent.id),
                        Some(&self.execution_id),
                        payload::model_completed(
                            self.spec.model.provider.as_str(),
                            &self.spec.model.model,
                            ModelOutcomeKind::ToolCall,
                            latency_ms,
                        ),
                    )
                    .await?;
                Ok(CompletionResponse::ToolCalls(calls))
            }
            Err(err) => {
                let kern = model_error_to_kern(&err);
                self.bus
                    .emit(
                        EventKind::ModelFailed,
                        Some(&self.agent.id),
                        Some(&self.execution_id),
                        payload::model_failed(&kern),
                    )
                    .await?;
                Err(kern)
            }
        }
    }

    // ------------------------------------------------------------------
    // Tool batches (§8.1.6)
    // ------------------------------------------------------------------

    async fn handle_tool_batch(&mut self, calls: Vec<ModelToolCall>) -> Result<RunControl> {
        // 6a. Validate every call; record fresh ones as `requested` in ONE
        //     transaction (all or nothing). Dedup (§11.2): a terminal row
        //     (completed/failed/denied) is never re-run — its recorded result
        //     is replayed; a row left `requested` by an interrupted run is
        //     re-driven. Then checkpoint BEFORE the batch (§8.1 6a) with the
        //     pending calls.
        let mut allow: Vec<ModelToolCall> = Vec::new();
        let mut asks: Vec<(ModelToolCall, String, String, String)> = Vec::new();
        let mut results: Vec<(String, String)> = Vec::new(); // (call_id, model-visible content)
        let mut fresh: Vec<ToolCall> = Vec::new(); // rows to insert as requested
        let mut pending: Vec<PendingCall> = Vec::new(); // pre-batch checkpoint payload
        let mut candidates: Vec<ModelToolCall> = Vec::new(); // to classify

        for call in &calls {
            let store = Arc::clone(&self.store);
            let execution_id = self.execution_id.clone();
            let call_id = call.id.clone();
            let existing =
                store_blocking(store, move |s| s.get_tool_call(&execution_id, &call_id)).await?;

            if let Some(row) = &existing {
                if row.status == ToolCallStatus::Completed || row.status == ToolCallStatus::Failed {
                    // Replay the recorded result/error; the tool is NOT run
                    // again and no tool events are re-emitted (the stream
                    // shows it happened once).
                    let content = match &row.result {
                        Some(v) => v.to_string(),
                        None => match &row.error {
                            Some(e) => e.to_string(),
                            None => deny_result("no recorded result for this call"),
                        },
                    };
                    results.push((call.id.clone(), content));
                    continue;
                }
            }

            // Invalid args never execute; record as failed and feed the error.
            if let Err(e) = self.executor.validate(&call.name, &call.arguments) {
                let record = ToolCall::new(
                    &call.id,
                    &self.agent.id,
                    &self.execution_id,
                    &call.name,
                    call.arguments.clone(),
                );
                let record = terminal_record(record, None, Some(e.to_json()));
                let store = Arc::clone(&self.store);
                if existing.is_some() {
                    store_blocking(store, move |s| s.update_tool_call(&record)).await?;
                } else {
                    store_blocking(store, move |s| s.record_tool_call(&record)).await?;
                }
                self.bus
                    .emit(
                        EventKind::ToolFailed,
                        Some(&self.agent.id),
                        Some(&self.execution_id),
                        payload::tool_failed(&call.id, &call.name, &tool_error_to_kern(&e)),
                    )
                    .await?;
                results.push((call.id.clone(), e.to_json().to_string()));
                continue;
            }

            pending.push(PendingCall::new(
                &call.id,
                &call.name,
                call.arguments.clone(),
            ));
            if existing.is_none() {
                fresh.push(ToolCall::new(
                    &call.id,
                    &self.agent.id,
                    &self.execution_id,
                    &call.name,
                    call.arguments.clone(),
                ));
            }
            candidates.push(call.clone());
        }

        // Bound the tool work a single execution may issue.
        // Fresh (model-issued) calls count — deduped replays of terminal rows
        // do not — so a recovered run keeps its budget across restarts.
        if let Some(max) = self.spec.runtime.max_tool_calls() {
            let issued = self.session.tool_calls + fresh.len() as u64;
            if issued > max as u64 {
                return Err(KernError::new(
                    ErrorCode::ToolCallLimitExceeded,
                    format!(
                        "execution exceeded runtime.max_tool_calls ({max}); {issued} calls issued"
                    ),
                ));
            }
        }
        if !fresh.is_empty() {
            self.session.tool_calls += fresh.len() as u64;
            let store = Arc::clone(&self.store);
            store_blocking(store, move |s| s.record_tool_calls_batch(&fresh)).await?;
        }
        if !pending.is_empty() {
            self.checkpoint_now("running", &pending).await?;
        }

        // 6b. Classify each remaining call via policy (§10).
        for call in candidates {
            let (decision, resource, action) = self.classify(&call.name, &call.arguments);
            match decision {
                CallDecision::Allow => {
                    self.emit_tool_requested(&call).await?;
                    allow.push(call);
                }
                CallDecision::Deny { reason } => {
                    // Terminal so recovery replays the denial, never the
                    // execution (§11.2).
                    let error = json!({ "code": "PERMISSION_DENIED", "message": reason });
                    let record = ToolCall::new(
                        &call.id,
                        &self.agent.id,
                        &self.execution_id,
                        &call.name,
                        call.arguments.clone(),
                    );
                    self.store_tool_terminal(record, None, Some(error)).await?;
                    self.bus
                        .emit(
                            EventKind::PermissionDenied,
                            Some(&self.agent.id),
                            Some(&self.execution_id),
                            payload::permission_denied(&call.id, &resource, &reason),
                        )
                        .await?;
                    results.push((call.id.clone(), deny_result(&reason)));
                }
                CallDecision::Ask => {
                    // Re-drive resolution: a request already created for this
                    // call id applies without re-asking (decided → apply;
                    // pending → re-park on the same request).
                    let store = Arc::clone(&self.store);
                    let agent_id = self.agent.id.clone();
                    let call_id = call.id.clone();
                    let request = store_blocking(store, move |s| {
                        s.get_permission_request_by_tool_call(&agent_id, &call_id)
                    })
                    .await?;
                    match request {
                        Some(req) if req.status == PermissionStatus::Granted => {
                            self.emit_tool_requested(&call).await?;
                            allow.push(call);
                        }
                        Some(req) if req.status == PermissionStatus::Denied => {
                            let reason =
                                format!("operator denied {resource} ({action}) for {}", call.id);
                            let error = json!({ "code": "PERMISSION_DENIED", "message": reason });
                            let record = ToolCall::new(
                                &call.id,
                                &self.agent.id,
                                &self.execution_id,
                                &call.name,
                                call.arguments.clone(),
                            );
                            self.store_tool_terminal(record, None, Some(error)).await?;
                            results.push((call.id.clone(), deny_result(&reason)));
                        }
                        Some(req) if req.status == PermissionStatus::Expired => {
                            // The approval window closed while the agent was
                            // parked or recovering: fail closed, exactly like
                            // a deny (never a hang, never a silent allow).
                            let reason = format!(
                                "permission request for {resource} ({action}) expired before \
                                 an operator decided it"
                            );
                            let error = json!({ "code": "PERMISSION_DENIED", "message": reason });
                            let record = ToolCall::new(
                                &call.id,
                                &self.agent.id,
                                &self.execution_id,
                                &call.name,
                                call.arguments.clone(),
                            );
                            self.store_tool_terminal(record, None, Some(error)).await?;
                            results.push((call.id.clone(), deny_result(&reason)));
                        }
                        Some(req) => {
                            // Pending: park on the ORIGINAL request id.
                            self.emit_tool_requested(&call).await?;
                            asks.push((call, req.id, resource, action));
                        }
                        None => {
                            let store = Arc::clone(&self.store);
                            let agent_id = self.agent.id.clone();
                            let call_id = call.id.clone();
                            let resource_owned = resource.clone();
                            let action_owned = action.clone();
                            let timeout = self.spec.runtime.ask_timeout().as_std();
                            let request = store_blocking(store, move |s| {
                                s.create_permission_request_with_ttl(
                                    &agent_id,
                                    Some(&call_id),
                                    &resource_owned,
                                    &action_owned,
                                    timeout,
                                )
                            })
                            .await?;
                            self.emit_tool_requested(&call).await?;
                            asks.push((call, request.id, resource, action));
                        }
                    }
                }
            }
        }

        // 6b (cont). Ask: suspend until the operator decides every request.
        if !asks.is_empty() {
            let (_, first_request, resource, action) = &asks[0];
            self.lifecycle
                .wait(&self.agent.id, first_request, resource, action)
                .await?;
            // Checkpoint the waiting state (§8.1 6b): restore re-parks on the
            // same pending requests (dedup by tool call id).
            let ask_pending: Vec<PendingCall> = asks
                .iter()
                .map(|(c, _, _, _)| PendingCall::new(&c.id, &c.name, c.arguments.clone()))
                .collect();
            self.checkpoint_now("waiting", &ask_pending).await?;
            let request_ids: Vec<String> = asks.iter().map(|(_, id, _, _)| id.clone()).collect();
            match self.park_for_decisions(&request_ids, &ask_pending).await? {
                ParkOutcome::Paused { checkpoint_id } => {
                    return Ok(RunControl::Paused { checkpoint_id })
                }
                ParkOutcome::AllDecided => {}
            }

            // When the park resolved WITHOUT an operator
            // decision (every request expired), the agent is still `waiting`
            // — the next lifecycle action (e.g. complete) would be an
            // invalid waiting → … transition and the run would die. Resume
            // it now; a decision that landed between the poll and this read
            // already applied waiting → running, and the CAS rejects the
            // redundant unpark.
            let store = Arc::clone(&self.store);
            let agent_owned = self.agent.id.clone();
            let still_waiting = store_blocking(store, move |s| {
                s.get_agent(&agent_owned)
                    .map(|a| a.state == LifecycleState::Waiting)
            })
            .await?;
            if still_waiting {
                match self.lifecycle.unpark(&self.agent.id).await {
                    Ok(_) => {}
                    Err(e) if e.code() == ErrorCode::InvalidTransition => {
                        // A decision landed first; the apply loop below
                        // handles it. Not an error.
                    }
                    Err(e) => return Err(e),
                }
            }

            for (call, request_id, resource, action) in asks {
                let store = Arc::clone(&self.store);
                let req_id = request_id.clone();
                let request =
                    store_blocking(store, move |s| s.get_permission_request(&req_id)).await?;
                match request.status {
                    PermissionStatus::Granted => allow.push(call),
                    PermissionStatus::Denied => {
                        let reason =
                            format!("operator denied {resource} ({action}) for {}", call.id);
                        // The row was recorded as `requested` in 6a; update it
                        // to terminal instead of inserting a duplicate.
                        let record = ToolCall::new(
                            &call.id,
                            &self.agent.id,
                            &self.execution_id,
                            &call.name,
                            call.arguments.clone(),
                        );
                        let error = json!({ "code": "PERMISSION_DENIED", "message": reason });
                        self.store_tool_terminal(record, None, Some(error)).await?;
                        results.push((call.id.clone(), deny_result(&reason)));
                    }
                    PermissionStatus::Expired => {
                        // The window closed while parked: fail closed. The
                        // park poll seals it (CAS) so this is deterministic.
                        let reason = format!(
                            "permission request for {resource} ({action}) expired before an \
                             operator decided it"
                        );
                        let error = json!({ "code": "PERMISSION_DENIED", "message": reason });
                        let record = ToolCall::new(
                            &call.id,
                            &self.agent.id,
                            &self.execution_id,
                            &call.name,
                            call.arguments.clone(),
                        );
                        self.store_tool_terminal(record, None, Some(error)).await?;
                        results.push((call.id.clone(), deny_result(&reason)));
                    }
                    other => {
                        return Err(KernError::internal(format!(
                            "permission request {request_id} resolved to unexpected status {}",
                            other.as_str()
                        )))
                    }
                }
            }
        }

        // 6c. Execute allowed calls concurrently (bounded by the executor's
        // per-agent + global semaphores), then record + event each result.
        // A durable sleep (≥ runtime.durable_sleep_min) is NOT executed: it
        // is recorded terminal with its wake time and the whole execution
        // parks — the sleep survives restarts, and recovery replays
        // the recorded result instead of re-sleeping.
        let mut to_execute = Vec::new();
        let mut park_until: Option<DateTime<Utc>> = None;
        for call in &allow {
            if let Some(wake) = self.durable_sleep_call(call) {
                let result = json!({ "slept": true, "until": wake.to_rfc3339() });
                let record = ToolCall::new(
                    &call.id,
                    &self.agent.id,
                    &self.execution_id,
                    &call.name,
                    call.arguments.clone(),
                );
                self.store_tool_terminal(record, Some(result.clone()), None)
                    .await?;
                results.push((
                    call.id.clone(),
                    serde_json::to_string(&result).unwrap_or_default(),
                ));
                park_until = Some(wake);
                continue;
            }
            to_execute.push(call);
        }
        if !to_execute.is_empty() {
            let mut set = JoinSet::new();
            for call in &to_execute {
                let executor = Arc::clone(&self.executor);
                let bus = self.bus.clone();
                let agent_id = self.agent.id.clone();
                let execution_id = self.execution_id.clone();
                let name = call.name.clone();
                let args = call.arguments.clone();
                let id = call.id.clone();
                let timeout = self.tool_timeout;
                set.spawn(async move {
                    let ctx = ToolContext {
                        agent_id: &agent_id,
                        execution_id: &execution_id,
                        tool_call_id: &id,
                    };
                    let _ = bus
                        .emit(
                            EventKind::ToolStarted,
                            Some(&agent_id),
                            Some(&execution_id),
                            payload::tool_started(&id, &name),
                        )
                        .await;
                    let started = Instant::now();
                    let result = executor.run(&name, &args, &ctx, timeout).await;
                    let latency_ms = started.elapsed().as_millis() as u64;
                    (id, name, args, latency_ms, result)
                });
            }
            let mut finished = Vec::with_capacity(to_execute.len());
            while let Some(joined) = set.join_next().await {
                let (id, name, args, latency_ms, result) =
                    joined.map_err(|e| KernError::internal(format!("tool task panicked: {e}")))?;
                match result {
                    Ok(value) => {
                        let record = ToolCall {
                            id: id.clone(),
                            agent_id: self.agent.id.clone(),
                            execution_id: self.execution_id.clone(),
                            tool_name: name.clone(),
                            args: args.clone(),
                            status: ToolCallStatus::Completed,
                            result: Some(value.clone()),
                            error: None,
                            started_at: Some(Utc::now()),
                            finished_at: Some(Utc::now()),
                        };
                        self.store_tool_terminal(record, Some(value.clone()), None)
                            .await?;
                        self.bus
                            .emit(
                                EventKind::ToolCompleted,
                                Some(&self.agent.id),
                                Some(&self.execution_id),
                                payload::tool_completed(
                                    &id,
                                    &name,
                                    latency_ms,
                                    serde_json::to_string(&value)
                                        .map(|s| s.len() as u64)
                                        .unwrap_or(0),
                                ),
                            )
                            .await?;
                        finished.push((
                            id,
                            serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string()),
                        ));
                    }
                    Err(err) => {
                        let record = ToolCall {
                            id: id.clone(),
                            agent_id: self.agent.id.clone(),
                            execution_id: self.execution_id.clone(),
                            tool_name: name.clone(),
                            args: args.clone(),
                            status: ToolCallStatus::Failed,
                            result: None,
                            error: Some(err.to_json()),
                            started_at: Some(Utc::now()),
                            finished_at: Some(Utc::now()),
                        };
                        self.store_tool_terminal(record, None, Some(err.to_json()))
                            .await?;
                        let kern = tool_error_to_kern(&err);
                        self.bus
                            .emit(
                                EventKind::ToolFailed,
                                Some(&self.agent.id),
                                Some(&self.execution_id),
                                payload::tool_failed(&id, &name, &kern),
                            )
                            .await?;
                        finished.push((id, err.to_json().to_string()));
                    }
                }
            }
            results.extend(finished);
        }

        // 6d. Feed everything back in ONE follow-up turn, in original order.
        //     A batch whose results are ALREADY in the session (a re-issued
        //     model turn after restore, deduped against terminal rows) is a
        //     transcript no-op — never duplicate entries.
        let all_already_fed = calls.iter().all(|c| self.session_has_tool_result(&c.id));
        if !all_already_fed {
            self.session
                .messages
                .push(Message::assistant_with_tool_calls("", calls.clone()));
            for call in &calls {
                if self.session_has_tool_result(&call.id) {
                    continue;
                }
                let content = results
                    .iter()
                    .find(|(id, _)| id == &call.id)
                    .map(|(_, c)| c.clone())
                    .unwrap_or_else(|| deny_result("no result recorded for this call"));
                self.session
                    .messages
                    .push(Message::tool_result(&call.id, content));
            }
        }

        // Checkpoint AFTER the batch (§8.1 6d): the session is consistent
        // here, so a crash now resumes from a clean turn boundary. For a
        // durable sleep the checkpoint already carries the terminal sleep row
        // and its result message — recovery replays it, never re-sleeps.
        self.checkpoint_now("running", &[]).await?;
        if let Some(wake_at) = park_until {
            return Ok(RunControl::Sleeping { wake_at });
        }
        Ok(RunControl::Continue)
    }

    /// Whether the session already contains a tool result for `call_id`
    /// (restored transcript guard for deduped replays).
    fn session_has_tool_result(&self, call_id: &str) -> bool {
        self.session
            .messages
            .iter()
            .any(|m| m.role == kern_model::Role::Tool && m.tool_call_id.as_deref() == Some(call_id))
    }

    /// Persist a terminal tool row for an already-recorded call (the
    /// executed-call paths build a fresh record and update it in place).
    async fn store_tool_terminal(
        &self,
        record: ToolCall,
        result: Option<Value>,
        error: Option<Value>,
    ) -> Result<()> {
        let record = terminal_record(record, result, error);
        let store = Arc::clone(&self.store);
        store_blocking(store, move |s| s.update_tool_call(&record)).await
    }

    async fn emit_tool_requested(&self, call: &ModelToolCall) -> Result<()> {
        let args = if self.spec.runtime.log_tool_args() {
            Some(&call.arguments)
        } else {
            None
        };
        self.bus
            .emit(
                EventKind::ToolRequested,
                Some(&self.agent.id),
                Some(&self.execution_id),
                payload::tool_requested(&call.id, &call.name, args),
            )
            .await
            .map(|_| ())
    }

    /// Classify a validated call against policy (§10). Returns the decision
    /// plus the resource/action strings carried by events.
    fn classify(&self, tool: &str, args: &Value) -> (CallDecision, String, String) {
        let decide = |d: &Decision| match d.effect {
            Effect::Allow => CallDecision::Allow,
            Effect::Ask => CallDecision::Ask,
            Effect::Deny => CallDecision::Deny {
                reason: d.reason.clone(),
            },
        };
        match tool {
            "filesystem" => {
                let action = args.get("action").and_then(Value::as_str).unwrap_or("");
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                let is_write = action == "write";
                let fs_action = if is_write {
                    FsAction::Write
                } else {
                    FsAction::Read
                };
                let d = self.permissions.evaluate_path(Path::new(path), fs_action);
                (
                    decide(&d),
                    path.to_string(),
                    if is_write { "write" } else { "read" }.to_string(),
                )
            }
            "http" => {
                let url = args.get("url").and_then(Value::as_str).unwrap_or("");
                match http_host_port(url) {
                    Some(hostport) => {
                        let d = self.permissions.evaluate_host(&hostport);
                        (decide(&d), hostport, "network".to_string())
                    }
                    None => (
                        CallDecision::Deny {
                            reason: format!("invalid URL {url:?}"),
                        },
                        url.to_string(),
                        "network".to_string(),
                    ),
                }
            }
            "memory.read" | "memory.list" => {
                let key = args
                    .get("key")
                    .and_then(Value::as_str)
                    .or_else(|| args.get("prefix").and_then(Value::as_str))
                    .unwrap_or("*");
                let d = self.permissions.evaluate_key(key, KeyAction::Read);
                (decide(&d), key.to_string(), "read".to_string())
            }
            "memory.write" => {
                let key = args.get("key").and_then(Value::as_str).unwrap_or("");
                let d = self.permissions.evaluate_key(key, KeyAction::Write);
                (decide(&d), key.to_string(), "write".to_string())
            }
            "shell" => {
                if self.permissions.shell_allowed() {
                    (
                        CallDecision::Allow,
                        "shell".to_string(),
                        "shell".to_string(),
                    )
                } else {
                    (
                        CallDecision::Deny {
                            reason: "shell is disabled for this agent".to_string(),
                        },
                        "shell".to_string(),
                        "shell".to_string(),
                    )
                }
            }
            "noop" | "sleep" => (CallDecision::Allow, tool.to_string(), "none".to_string()),
            other => (
                CallDecision::Deny {
                    reason: format!("tool {other:?} is not configured for this agent"),
                },
                other.to_string(),
                "none".to_string(),
            ),
        }
    }

    /// Park until every ask request is decided (poll-based, crash-safe). A
    /// pause request interrupts the park at its next poll: the runner
    /// checkpoints the pending batch and transitions to `paused` (the
    /// operator's decision is durable and still applies on resume).
    async fn park_for_decisions(
        &mut self,
        request_ids: &[String],
        pending: &[PendingCall],
    ) -> Result<ParkOutcome> {
        loop {
            // A run may not park past its deadline even on a
            // human — the ask has its own TTL; the budget is the run's cap.
            self.check_deadline()?;
            if self.controls.pause.swap(false, Ordering::AcqRel) {
                let checkpoint = self.checkpoint_now("paused", pending).await?;
                self.lifecycle.pause(&self.agent.id, &checkpoint.id).await?;
                return Ok(ParkOutcome::Paused {
                    checkpoint_id: checkpoint.id,
                });
            }
            let store = Arc::clone(&self.store);
            let ids = request_ids.to_vec();
            let all_decided = store_blocking(store, move |s| {
                let mut decided = true;
                for id in &ids {
                    match s.get_permission_request(id) {
                        Ok(r) if r.status != PermissionStatus::Pending => {}
                        Ok(r) if r.is_overdue() => {
                            // The operator's window closed while we waited:
                            // seal it expired (CAS) so the apply loop below
                            // treats it as a denial — a waiting agent can
                            // never park forever on a stale ask.
                            let _ = s.expire_permission_request(&r.id);
                        }
                        _ => decided = false,
                    }
                }
                Ok(decided)
            })
            .await?;
            if all_decided {
                return Ok(ParkOutcome::AllDecided);
            }
            tokio::time::sleep(PARK_POLL_INTERVAL).await;
        }
    }

    fn trim_history(&mut self) {
        let max_tokens = self.spec.runtime.max_history_tokens();
        let trimmed = trim_messages(&mut self.session.messages, max_tokens);
        self.session.history_trimmed |= trimmed;
    }
}

fn deny_result(reason: &str) -> String {
    json!({ "code": "PERMISSION_DENIED", "message": reason }).to_string()
}

/// Set a tool row to its terminal state (failed when an error is present,
/// completed otherwise) and stamp the finish time.
fn terminal_record(mut record: ToolCall, result: Option<Value>, error: Option<Value>) -> ToolCall {
    record.status = if error.is_some() {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    };
    record.result = result;
    record.error = error;
    record.finished_at = Some(Utc::now());
    record
}

/// The agent's workspace root under `KERN_HOME` (SPEC §9): created on demand.
fn workspace_dir(store: &Store, agent_name: &str) -> Result<std::path::PathBuf> {
    let dir = store.data_dir().join("workspace").join(agent_name);
    std::fs::create_dir_all(&dir).map_err(|e| {
        KernError::new(
            ErrorCode::StorageFailure,
            format!("create workspace {}: {e}", dir.display()),
        )
    })?;
    Ok(dir)
}

/// Run a blocking store call off the async runtime (same discipline as the
/// event bus and lifecycle).
async fn store_blocking<T, F>(store: Arc<Store>, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Store) -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || f(&store))
        .await
        .map_err(|e| KernError::internal(format!("store task join failed: {e}")))?
}

fn model_error_to_kern(err: &kern_model::ModelError) -> KernError {
    let code = match err {
        kern_model::ModelError::Timeout(_) => ErrorCode::ModelTimeout,
        kern_model::ModelError::Unavailable(_) => ErrorCode::ModelUnavailable,
        kern_model::ModelError::Auth(_) => ErrorCode::ModelAuth,
        kern_model::ModelError::RateLimited(_) => ErrorCode::ModelRateLimited,
        kern_model::ModelError::InvalidResponse(_) => ErrorCode::ModelInvalidResponse,
        kern_model::ModelError::BudgetExhausted(_) => ErrorCode::ModelBudgetExhausted,
    };
    KernError::new(code, err.to_string())
}

fn tool_error_to_kern(err: &ToolError) -> KernError {
    let code = match err {
        ToolError::InvalidArguments(_) => ErrorCode::ToolInvalidArguments,
        ToolError::Timeout(_) => ErrorCode::ToolTimeout,
        ToolError::Failed(_) => ErrorCode::ToolFailed,
        ToolError::Unavailable(_) => ErrorCode::ToolUnavailable,
        ToolError::PermissionDenied(_) => ErrorCode::PermissionDenied,
    };
    KernError::new(code, err.to_string())
}

#[cfg(test)]
mod tests;
