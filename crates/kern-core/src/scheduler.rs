//! Scheduler (ARCHITECTURE.md §13, SPEC.md §13) — recurring and one-shot
//! agent runs.
//!
//! The concurrency semaphore and startup reconciliation of interrupted
//! agents came first; the actual firing machinery:
//!
//! - `reconcile_schedules` (daemon startup): scheduled agents get their
//!   `next_run_at` initialized (`every`/missed `at` fire immediately — the
//!   first occurrence; `cron`/future `at` wait for their computed time).
//!   A `next_run_at` already in the past is left due — the sweep fires it
//!   ONCE and advances, so missed runs collapse instead of storming.
//! - `run_due_once` (each timer tick): every agent whose `next_run_at` has
//!   passed AND whose state allows a new run (created/completed/failed/
//!   terminated — never running/waiting/paused/recovering) is fired once:
//!   `next_run_at` is advanced immediately (a long run does not stall the
//!   schedule), the concurrency slot is held for the run's lifetime, and the
//!   run is spawned detached on the engine. An agent already running is
//!   skipped (`skip_if_running`, or the engine's one-run-per-agent
//!   constraint) and its schedule still advances.
//! - `timer_loop`: bounded-cadence wakeups (max 5 s so freshly created
//!   agents are noticed) that fire due runs until the shutdown signal.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};

use crate::config::AgentSpec;
use crate::engine::Engine;
use crate::error::{ErrorCode, KernError, Result};
use crate::event::{payload, EventKind};
use crate::lifecycle::Lifecycle;
use crate::schedule::Schedule;
use crate::store::{Agent, ExecutionStatus, LifecycleState, Store};

/// Maximum wait between timer scans: long enough to be idle-friendly, short
/// enough that a newly created scheduled agent starts within this window.
pub const TIMER_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Crash-loop backoff (SPEC.md §13): 1 min base, doubling, capped at
/// 24 h. Applied once `backoff_after_failures` consecutive runs have failed;
/// the offset is the number of failures past the threshold.
const BACKOFF_BASE: Duration = Duration::from_secs(60);
const BACKOFF_MAX: Duration = Duration::from_secs(24 * 60 * 60);

fn backoff_duration(past_threshold: u32) -> Duration {
    let shift = u32::min(past_threshold, 20);
    BACKOFF_BASE
        .checked_mul(2u32.saturating_pow(shift))
        .unwrap_or(BACKOFF_MAX)
        .min(BACKOFF_MAX)
}

/// Lifecycle states from which a schedule may start a new run.
fn schedulable(state: LifecycleState) -> bool {
    matches!(
        state,
        LifecycleState::Created
            | LifecycleState::Completed
            | LifecycleState::Failed
            | LifecycleState::Terminated
    )
}

#[derive(Clone)]
pub struct Scheduler {
    store: Arc<Store>,
    lifecycle: Arc<Lifecycle>,
    engine: Engine,
    semaphore: Arc<Semaphore>,
}

impl Scheduler {
    /// `max_concurrent_agents` is the semaphore capacity (default 8, §17).
    pub fn new(
        store: Arc<Store>,
        lifecycle: Arc<Lifecycle>,
        engine: Engine,
        max_concurrent_agents: usize,
    ) -> Self {
        Self {
            store,
            lifecycle,
            engine,
            semaphore: Arc::new(Semaphore::new(max_concurrent_agents)),
        }
    }

    /// Acquire an agent slot, waiting until one is free. Owned permits keep
    /// the slot held for the execution's lifetime.
    pub async fn acquire_slot(&self) -> OwnedSemaphorePermit {
        self.semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore never closed")
    }

    /// Try to acquire without waiting (`None` when at capacity).
    pub fn try_acquire_slot(&self) -> Option<OwnedSemaphorePermit> {
        self.semaphore.clone().try_acquire_owned().ok()
    }

    /// Startup reconciliation: mark every agent in `starting|running|waiting`
    /// as `recovering`. Returns how many were marked. Failures are logged, not
    /// fatal (a stuck agent stays stuck rather than crashing the daemon).
    pub async fn reconcile_interrupted(&self) -> Result<usize> {
        let store = Arc::clone(&self.store);
        let agents = tokio::task::spawn_blocking(move || store.list_agents())
            .await
            .map_err(|e| {
                crate::error::KernError::internal(format!("scheduler task failed: {e}"))
            })??;

        let mut count = 0usize;
        for agent in agents {
            let interrupted = matches!(
                agent.state,
                LifecycleState::Starting | LifecycleState::Running | LifecycleState::Waiting
            );
            if !interrupted {
                continue;
            }
            match self.lifecycle.recover(&agent.id).await {
                Ok(_) => count += 1,
                Err(e) => {
                    tracing::warn!(
                        agent_id = %agent.id,
                        error = %e,
                        "failed to mark interrupted agent recovering"
                    );
                }
            }
        }
        Ok(count)
    }

    /// Daemon startup: initialize `next_run_at` for scheduled agents that do
    /// not have one yet. A due (past) `next_run_at` is deliberately left — the
    /// first sweep fires it once. Returns how many were initialized.
    pub async fn reconcile_schedules(&self) -> Result<usize> {
        let now = Utc::now();
        let store = Arc::clone(&self.store);
        let agents = tokio::task::spawn_blocking(move || store.list_agents())
            .await
            .map_err(|e| KernError::internal(format!("schedule scan task failed: {e}")))??;

        let mut initialized = 0usize;
        for agent in agents {
            if agent.next_run_at.is_some() {
                continue;
            }
            let Some(schedule) = self.compile_schedule(&agent)? else {
                continue;
            };
            let first = schedule.first_occurrence(now);
            let store = Arc::clone(&self.store);
            let agent_id = agent.id.clone();
            tokio::task::spawn_blocking(move || store.set_next_run_at(&agent_id, first))
                .await
                .map_err(|e| KernError::internal(format!("next_run_at write failed: {e}")))??;
            initialized += 1;
        }
        Ok(initialized)
    }

    /// Agents whose schedule is due right now and whose state allows a run.
    fn due_agents(&self, now: DateTime<Utc>) -> Result<Vec<Agent>> {
        let agents = self
            .store
            .list_agents()
            .map_err(|e| KernError::internal(format!("list agents for due scan: {e}")))?;
        Ok(agents
            .into_iter()
            .filter(|a| schedulable(a.state))
            .filter(|a| a.next_run_at.is_some_and(|t| t <= now))
            .collect())
    }

    /// Fire every due agent once. Returns how many runs were started. Each
    /// fired agent's `next_run_at` advances immediately (schedule continues
    /// even for long runs; missed occurrences collapse).
    pub async fn run_due_once(&self) -> Result<usize> {
        let now = Utc::now();
        let due = self.due_agents(now)?;
        let mut started = 0usize;

        for agent in due {
            let Some(schedule) = self.compile_schedule(&agent)? else {
                // No schedule (config changed underneath us): retire the run.
                let store = Arc::clone(&self.store);
                let agent_id = agent.id.clone();
                tokio::task::spawn_blocking(move || store.set_next_run_at(&agent_id, None))
                    .await
                    .map_err(|e| KernError::internal(format!("next_run_at write failed: {e}")))??;
                continue;
            };

            // A crash-looping agent (consecutive failed
            // runs) backs off exponentially instead of failing forever — the
            // next occurrence is booked at now+backoff and the run is skipped.
            let threshold = self.backoff_after_failures(&agent)?;
            if threshold > 0 {
                let streak = self.consecutive_failures(&agent.id).await?;
                if streak >= threshold {
                    let backoff = backoff_duration(streak - threshold);
                    let next = Utc::now()
                        + chrono::Duration::from_std(backoff)
                            .unwrap_or_else(|_| chrono::Duration::hours(1));
                    let store = Arc::clone(&self.store);
                    let agent_id = agent.id.clone();
                    tokio::task::spawn_blocking(move || {
                        store.set_next_run_at(&agent_id, Some(next))
                    })
                    .await
                    .map_err(|e| KernError::internal(format!("next_run_at write failed: {e}")))??;
                    self.engine
                        .bus
                        .emit(
                            EventKind::SchedulerBackoff,
                            Some(&agent.id),
                            None,
                            payload::scheduler_backoff(&agent.id, streak, &next.to_rfc3339()),
                        )
                        .await?;
                    tracing::warn!(
                        agent_id = %agent.id,
                        consecutive_failures = streak,
                        next_run_at = %next.to_rfc3339(),
                        "schedule backing off after consecutive failures"
                    );
                    continue;
                }
            }

            // Advance the schedule first: the next occurrence is booked even
            // if this run is long or gets skipped.
            let next = schedule.next_after(Utc::now());
            let store = Arc::clone(&self.store);
            let agent_id = agent.id.clone();
            tokio::task::spawn_blocking(move || store.set_next_run_at(&agent_id, next))
                .await
                .map_err(|e| KernError::internal(format!("next_run_at write failed: {e}")))??;

            let scheduled_for = agent
                .next_run_at
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| now.to_rfc3339());

            if self.engine.is_running(&agent.id) {
                tracing::info!(
                    agent_id = %agent.id,
                    scheduled_for = %scheduled_for,
                    "schedule due but agent is already running; skipping"
                );
                continue;
            }

            self.engine
                .bus
                .emit(
                    EventKind::SchedulerRunDue,
                    Some(&agent.id),
                    None,
                    payload::scheduler_run_due(&agent.id, &scheduled_for),
                )
                .await?;

            let permit = self.acquire_slot().await;
            let engine = self.engine.clone();
            let agent_id = agent.id.clone();
            tokio::spawn(async move {
                if let Err(err) = engine.run_agent(&agent_id, None).await {
                    tracing::error!(agent_id = %agent_id, error = %err, "scheduled run failed");
                }
                drop(permit);
            });
            started += 1;
        }
        Ok(started)
    }

    /// Consecutive trailing failed executions for an agent (newest first).
    /// A single success resets the streak — the crash-loop detector.
    async fn consecutive_failures(&self, agent_id: &str) -> Result<u32> {
        let store = Arc::clone(&self.store);
        let agent_id = agent_id.to_string();
        let executions =
            tokio::task::spawn_blocking(move || store.list_executions_for_agent(&agent_id))
                .await
                .map_err(|e| KernError::internal(format!("execution scan task failed: {e}")))??;
        let mut streak = 0u32;
        for execution in executions {
            if execution.status == ExecutionStatus::Failed {
                streak += 1;
            } else {
                break;
            }
        }
        Ok(streak)
    }

    /// The agent's schedule `backoff_after_failures` knob (default 3 via the
    /// config accessor; 0 = disabled).
    fn backoff_after_failures(&self, agent: &Agent) -> Result<u32> {
        let spec: AgentSpec = serde_json::from_value(agent.config.clone()).map_err(|e| {
            KernError::new(
                ErrorCode::ConfigInvalid,
                format!("stored config for agent {} is invalid: {e}", agent.name),
            )
        })?;
        Ok(spec
            .schedule
            .as_ref()
            .map(|c| c.backoff_after_failures())
            .unwrap_or(0))
    }

    /// Compile the agent's schedule from its stored spec.
    fn compile_schedule(&self, agent: &Agent) -> Result<Option<Schedule>> {
        let spec: AgentSpec = serde_json::from_value(agent.config.clone()).map_err(|e| {
            KernError::new(
                ErrorCode::ConfigInvalid,
                format!("stored config for agent {} is invalid: {e}", agent.name),
            )
        })?;
        match spec.schedule.as_ref() {
            Some(cfg) => Schedule::from_config(cfg),
            None => Ok(None),
        }
    }

    /// The next sleep duration for the timer: a due time (past or imminent)
    /// wakes the loop at the minimum cadence; a future occurrence within the
    /// poll window wakes exactly at it; otherwise the loop polls at the
    /// bounded cadence (so new scheduled agents are noticed promptly).
    /// Sleeping agents' wake times participate too (durable sleep).
    fn next_sleep(&self, now: DateTime<Utc>) -> Duration {
        let soonest = self
            .store
            .list_agents()
            .ok()
            .and_then(|a| a.into_iter().filter_map(|a| a.next_run_at).min());
        let soonest_wake = self.store.soonest_wake_at().ok().flatten();
        let soonest = match (soonest, soonest_wake) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let min = Duration::from_millis(50);
        match soonest {
            Some(t) if t <= now => min,
            Some(t) => (t - now)
                .to_std()
                .unwrap_or(min)
                .min(TIMER_POLL_INTERVAL)
                .max(min),
            None => TIMER_POLL_INTERVAL,
        }
    }

    /// Wake every sleeping agent whose wake time has passed. A
    /// missed wake — daemon was down past the wake time — collapses: it fires
    /// once. Returns how many runners were respawned. Called at daemon
    /// startup (via [`Scheduler::reconcile_sleeping`]) and every timer tick.
    pub async fn wake_due_once(&self) -> Result<usize> {
        let now = Utc::now();
        let store = Arc::clone(&self.store);
        let executions = tokio::task::spawn_blocking(move || store.list_sleeping_due(now))
            .await
            .map_err(|e| KernError::internal(format!("sleeping scan task failed: {e}")))??;
        let mut woken = 0usize;
        for execution in executions {
            let agent_id = execution.agent_id.clone();
            let execution_id = execution.id.clone();
            tracing::info!(agent_id, execution_id, "waking sleeping agent");
            match self.engine.prepare_resume(&agent_id, &execution_id).await {
                Ok((state, pending, checkpoint_id, input)) => {
                    self.engine.spawn_resumed(
                        &agent_id,
                        &execution_id,
                        state,
                        pending,
                        checkpoint_id,
                        input,
                    );
                    woken += 1;
                }
                Err(err) => {
                    tracing::error!(
                        agent_id,
                        execution_id,
                        error = %err,
                        "waking sleeping agent failed; failing it"
                    );
                    let _ = self.engine.fail_agent(&agent_id, &execution_id, &err).await;
                }
            }
        }
        Ok(woken)
    }

    /// Daemon startup: wake sleeping agents whose wake time already passed
    /// while the daemon was down. Future wakes are left for the timer loop.
    pub async fn reconcile_sleeping(&self) -> Result<usize> {
        self.wake_due_once().await
    }

    /// The background scheduling loop. Runs until the shutdown signal flips:
    /// wakes when the soonest schedule is due (or at the bounded poll), fires
    /// due runs and due sleeps, repeats.
    pub async fn timer_loop(self, mut shutdown: watch::Receiver<bool>) {
        tracing::info!("scheduler timer started");
        loop {
            if *shutdown.borrow() {
                break;
            }
            let sleep = self.next_sleep(Utc::now());
            tokio::select! {
                _ = tokio::time::sleep(sleep) => {}
                _ = shutdown.changed() => break,
            }
            if *shutdown.borrow() {
                break;
            }
            match self.run_due_once().await {
                Ok(n) if n > 0 => tracing::info!(started = n, "scheduled runs started"),
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "scheduled run sweep failed"),
            }
            match self.wake_due_once().await {
                Ok(n) if n > 0 => tracing::info!(woken = n, "sleeping agents woken"),
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "wake sweep failed"),
            }
        }
        tracing::info!("scheduler timer stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventBus;
    use crate::store::Agent;
    use crate::store::Execution;
    use kern_model::mock::{MockProvider, ScriptedStep};
    use kern_model::ModelError;
    use serde_json::json;

    async fn test_ctx(max: usize) -> (tempfile::TempDir, Arc<Store>, Scheduler) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let bus = EventBus::new(Arc::clone(&store));
        let lifecycle = Arc::new(Lifecycle::new(Arc::clone(&store), bus.clone()));
        let mut gateway = kern_model::gateway::ModelGateway::new();
        gateway
            .register(Arc::new(MockProvider::finishing("scheduled")))
            .unwrap();
        let engine = Engine::new(Arc::clone(&store), bus, Arc::new(gateway), 8);
        let scheduler = Scheduler::new(Arc::clone(&store), lifecycle, engine, max);
        (dir, store, scheduler)
    }

    fn seed_agent(store: &Store, name: &str, state: LifecycleState) -> String {
        let spec = crate::config::parse_agent_spec(
            "version: 1\nname: x\nmodel:\n  provider: mock\n  model: test\ntools:\n  - noop\n",
        )
        .unwrap();
        let agent = Agent::new(name, serde_json::to_value(&spec).unwrap(), state);
        store.create_agent(&agent).unwrap();
        agent.id
    }

    fn scheduled_agent(store: &Store, name: &str, schedule_yaml: &str) -> String {
        let spec = crate::config::parse_agent_spec(&format!(
            "version: 1\nname: {name}\nmodel:\n  provider: mock\n  model: test\ntools:\n  - noop\nschedule:\n{schedule_yaml}\n"
        ))
        .unwrap();
        let agent = Agent::new(
            spec.name.clone(),
            serde_json::to_value(&spec).unwrap(),
            LifecycleState::Completed,
        );
        store.create_agent(&agent).unwrap();
        agent.id.clone()
    }

    fn set_next(store: &Store, agent_id: &str, at: DateTime<Utc>) {
        store.set_next_run_at(agent_id, Some(at)).unwrap();
    }

    async fn wait_for_count(store: &Store, agent_id: &str, count: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let n = store.list_executions_for_agent(agent_id).unwrap().len();
            if n >= count {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "agent {agent_id} did not reach {count} executions within 5s (at {n})"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn semaphore_bounds_concurrency() {
        let (_dir, _store, scheduler) = test_ctx(2).await;
        let p1 = scheduler.try_acquire_slot().expect("first slot");
        let p2 = scheduler.try_acquire_slot().expect("second slot");
        assert!(scheduler.try_acquire_slot().is_none(), "capacity exceeded");
        drop(p1);
        drop(p2);
        assert!(
            scheduler.try_acquire_slot().is_some(),
            "slot freed after release"
        );
    }

    #[tokio::test]
    async fn reconciliation_marks_only_interrupted_agents() {
        let (_dir, store, scheduler) = test_ctx(8).await;
        let running = seed_agent(&store, "running", LifecycleState::Running);
        let starting = seed_agent(&store, "starting", LifecycleState::Starting);
        let waiting = seed_agent(&store, "waiting", LifecycleState::Waiting);
        let paused = seed_agent(&store, "paused", LifecycleState::Paused);
        let created = seed_agent(&store, "created", LifecycleState::Created);

        let count = scheduler.reconcile_interrupted().await.unwrap();
        assert_eq!(count, 3);
        assert_eq!(
            store.get_agent(&running).unwrap().state,
            LifecycleState::Recovering
        );
        assert_eq!(
            store.get_agent(&starting).unwrap().state,
            LifecycleState::Recovering
        );
        assert_eq!(
            store.get_agent(&waiting).unwrap().state,
            LifecycleState::Recovering
        );
        assert_eq!(
            store.get_agent(&paused).unwrap().state,
            LifecycleState::Paused
        );
        assert_eq!(
            store.get_agent(&created).unwrap().state,
            LifecycleState::Created
        );
    }

    #[tokio::test]
    async fn due_agent_fires_and_advances_next_run() {
        let (_dir, store, scheduler) = test_ctx(8).await;
        let agent_id = scheduled_agent(&store, "tick", "  every: 1h\n");
        set_next(&store, &agent_id, Utc::now() - chrono::Duration::seconds(1));

        let started = scheduler.run_due_once().await.unwrap();
        assert_eq!(started, 1);
        wait_for_count(&store, &agent_id, 1).await;
        // Advanced: the next occurrence is in the future, so a second sweep
        // does NOT fire a catch-up run.
        let agent = store.get_agent(&agent_id).unwrap();
        assert!(agent.next_run_at.is_some_and(|t| t > Utc::now()));
        assert_eq!(scheduler.run_due_once().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn every_agent_fires_repeatedly() {
        let (_dir, store, scheduler) = test_ctx(8).await;
        let agent_id =
            scheduled_agent(&store, "pulse", "  every: 100ms\n  skip_if_running: true\n");
        set_next(&store, &agent_id, Utc::now());

        // Sweep 1 fires; sweep 2 (after the interval passed) fires again.
        assert_eq!(scheduler.run_due_once().await.unwrap(), 1);
        wait_for_count(&store, &agent_id, 1).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(scheduler.run_due_once().await.unwrap(), 1);
        wait_for_count(&store, &agent_id, 2).await;
        let events = store.events_after(0, 500).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == "scheduler.run_due")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn skip_if_running_defers_due_runs_while_active() {
        // A long-running first run (400ms sleep) keeps the agent active; a due
        // occurrence during it must be skipped, not queued or re-fired.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let bus = EventBus::new(Arc::clone(&store));
        let lifecycle = Arc::new(Lifecycle::new(Arc::clone(&store), bus.clone()));
        let mut gateway = kern_model::gateway::ModelGateway::new();
        gateway
            .register(Arc::new(MockProvider::new(vec![
                ScriptedStep::ToolCalls(vec![kern_model::ToolCall {
                    id: "c1".into(),
                    name: "sleep".into(),
                    arguments: json!({ "ms": 400 }),
                }]),
                ScriptedStep::Finish("done".into()),
            ])))
            .unwrap();
        let engine = Engine::new(Arc::clone(&store), bus, Arc::new(gateway), 8);
        let scheduler = Scheduler::new(Arc::clone(&store), lifecycle, engine.clone(), 8);

        let spec = crate::config::parse_agent_spec(
            "version: 1\nname: skipper\nmodel:\n  provider: mock\n  model: test\ntools:\n  - sleep\nschedule:\n  every: 100ms\n  skip_if_running: true\n",
        )
        .unwrap();
        let agent = Agent::new(
            spec.name.clone(),
            serde_json::to_value(&spec).unwrap(),
            LifecycleState::Completed,
        );
        store.create_agent(&agent).unwrap();
        set_next(&store, &agent.id, Utc::now() - chrono::Duration::seconds(1));

        // Fire run 1; while it is still active, the schedule comes due again.
        assert_eq!(scheduler.run_due_once().await.unwrap(), 1);
        wait_for_count(&store, &agent.id, 1).await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(engine.is_running(&agent.id), "run 1 must still be active");
        assert_eq!(
            scheduler.run_due_once().await.unwrap(),
            0,
            "skipped while running"
        );
        // The skip still advanced the schedule; no second execution appears.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert_eq!(store.list_executions_for_agent(&agent.id).unwrap().len(), 1);
    }

    /// A scheduler wired to a provider that fails every completion.
    fn failing_ctx() -> (tempfile::TempDir, Arc<Store>, Scheduler) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let bus = EventBus::new(Arc::clone(&store));
        let lifecycle = Arc::new(Lifecycle::new(Arc::clone(&store), bus.clone()));
        let mut gateway = kern_model::gateway::ModelGateway::new();
        gateway
            .register(Arc::new(MockProvider::looping([ScriptedStep::Fail(
                ModelError::Unavailable("boom".into()),
            )])))
            .unwrap();
        let engine = Engine::new(Arc::clone(&store), bus, Arc::new(gateway), 8);
        let scheduler = Scheduler::new(Arc::clone(&store), lifecycle, engine, 8);
        (dir, store, scheduler)
    }

    async fn wait_for_failed_executions(store: &Store, agent_id: &str, count: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let n = store.list_executions_for_agent(agent_id).unwrap().len();
            let state = store.get_agent(agent_id).unwrap().state;
            if n >= count && state == LifecycleState::Failed {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "run {count} did not fail within 20s (n={n}, state={state:?})"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    async fn consecutive_failures_back_off_exponentially() {
        let (_dir, store, scheduler) = failing_ctx();
        let agent_id = scheduled_agent(
            &store,
            "flapper",
            "  every: 1s\n  backoff_after_failures: 3\n",
        );

        // Three consecutive failing runs fire normally.
        for n in 1..=3 {
            set_next(&store, &agent_id, Utc::now() - chrono::Duration::seconds(1));
            assert_eq!(scheduler.run_due_once().await.unwrap(), 1, "run {n} fires");
            wait_for_failed_executions(&store, &agent_id, n).await;
        }

        // The fourth due sweep finds a streak of 3 >= threshold: it backs off
        // (no 4th execution) and books the next occurrence at now + 1 min.
        set_next(&store, &agent_id, Utc::now() - chrono::Duration::seconds(1));
        assert_eq!(
            scheduler.run_due_once().await.unwrap(),
            0,
            "crash-looping agent must back off instead of firing"
        );
        assert_eq!(store.list_executions_for_agent(&agent_id).unwrap().len(), 3);
        let next = store.get_agent(&agent_id).unwrap().next_run_at.unwrap();
        assert!(
            next > Utc::now() + chrono::Duration::seconds(30),
            "next run must be pushed back, got {next}"
        );
        assert!(next < Utc::now() + chrono::Duration::minutes(10));

        let events = store.events_after(0, 1000).unwrap();
        let backoff = events
            .iter()
            .find(|e| e.kind == "scheduler.backoff")
            .expect("scheduler.backoff event must be emitted");
        assert_eq!(backoff.payload["consecutive_failures"], 3);
        assert!(backoff.payload["next_run_at"].is_string());
    }

    #[tokio::test]
    async fn backoff_can_be_disabled() {
        let (_dir, store, scheduler) = failing_ctx();
        let agent_id = scheduled_agent(
            &store,
            "flapper-off",
            "  every: 1s\n  backoff_after_failures: 0\n",
        );

        // With backoff disabled the schedule fires despite failures.
        set_next(&store, &agent_id, Utc::now() - chrono::Duration::seconds(1));
        assert_eq!(scheduler.run_due_once().await.unwrap(), 1);
        wait_for_failed_executions(&store, &agent_id, 1).await;
        set_next(&store, &agent_id, Utc::now() - chrono::Duration::seconds(1));
        assert_eq!(
            scheduler.run_due_once().await.unwrap(),
            1,
            "disabled backoff must still fire on failures"
        );
        wait_for_failed_executions(&store, &agent_id, 2).await;
        assert!(store
            .events_after(0, 1000)
            .unwrap()
            .iter()
            .all(|e| e.kind != "scheduler.backoff"));
    }

    #[tokio::test]
    async fn startup_reconcile_initializes_missing_next_runs() {
        let (_dir, store, scheduler) = test_ctx(8).await;
        // `every` initializes to ~now (fires immediately); `cron` waits for
        // its computed occurrence; no-schedule agents are untouched.
        let pulse = scheduled_agent(&store, "pulse", "  every: 5s\n");
        let cron = scheduled_agent(&store, "daily", "  cron: '0 0 * * *'\n");
        let plain = seed_agent(&store, "plain", LifecycleState::Completed);

        let initialized = scheduler.reconcile_schedules().await.unwrap();
        assert_eq!(initialized, 2);
        let pulse_at = store.get_agent(&pulse).unwrap().next_run_at.unwrap();
        assert!(
            (pulse_at - Utc::now()).num_seconds().abs() < 5,
            "every initializes to ~now, got {pulse_at}"
        );
        let cron_at = store.get_agent(&cron).unwrap().next_run_at.unwrap();
        assert!(cron_at > Utc::now(), "cron waits for its occurrence");
        assert!(store.get_agent(&plain).unwrap().next_run_at.is_none());
    }

    #[tokio::test]
    async fn wake_due_once_respawns_due_sleeping_agents() {
        let (_dir, store, scheduler) = test_ctx(8).await;
        // A sleeping agent whose wake time has passed — the daemon was down
        // past it. Seed exactly what the runtime persists: lifecycle
        // `sleeping`, execution `running` with a past `wake_at`.
        let agent_id = seed_agent(&store, "sleeper", LifecycleState::Sleeping);
        let mut execution = Execution::new(&agent_id, ExecutionStatus::Running);
        execution.wake_at = Some(Utc::now() - chrono::Duration::seconds(1));
        store.create_execution(&execution).unwrap();

        assert_eq!(
            scheduler.wake_due_once().await.unwrap(),
            1,
            "one due sleeper must be woken"
        );
        // The respawned runner drives the finishing provider to completion.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if store.get_agent(&agent_id).unwrap().state == LifecycleState::Completed {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "woken agent did not complete within 5s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let execution = store.get_execution(&execution.id).unwrap();
        assert!(
            execution.wake_at.is_none(),
            "wake must clear the persisted wake time"
        );
        assert_eq!(execution.status, ExecutionStatus::Completed);
    }

    #[tokio::test]
    async fn wake_due_once_skips_future_wakes() {
        let (_dir, store, scheduler) = test_ctx(8).await;
        let agent_id = seed_agent(&store, "future-sleeper", LifecycleState::Sleeping);
        let mut execution = Execution::new(&agent_id, ExecutionStatus::Running);
        execution.wake_at = Some(Utc::now() + chrono::Duration::seconds(60));
        store.create_execution(&execution).unwrap();

        assert_eq!(
            scheduler.wake_due_once().await.unwrap(),
            0,
            "a future wake must not fire early"
        );
        assert_eq!(
            store.get_agent(&agent_id).unwrap().state,
            LifecycleState::Sleeping
        );
    }

    #[tokio::test]
    async fn timer_loop_fires_until_shutdown() {
        let (_dir, store, scheduler) = test_ctx(8).await;
        let agent_id = scheduled_agent(&store, "looper", "  every: 50ms\n");
        set_next(&store, &agent_id, Utc::now());
        let (tx, rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(scheduler.timer_loop(rx));

        wait_for_count(&store, &agent_id, 3).await;
        tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("timer loop must stop on shutdown")
            .unwrap();
    }
}
