//! Agent lifecycle state machine (SPEC.md §3).
//!
//! `LifecycleState::can_transition` implements the §3.2 transition table —
//! including the documented amendments: terminal states may transition to
//! `starting` for a new run (scheduled agents must be able to run again), and
//! `starting|running|waiting` may transition to `recovering` on daemon restart
//! (startup reconciliation, then full recovery).
//!
//! `Lifecycle` applies actions on top of `Store::transition`: every action
//! (1) reads the agent's current state, (2) validates the transition against
//! the table, (3) persists state + events in ONE transaction (guarded by the
//! expected state, so concurrent actions cannot double-apply), and (4)
//! broadcasts the persisted events. All store work is offloaded with
//! `spawn_blocking` so async callers never block the runtime.

use std::sync::Arc;

use chrono::Utc;

use crate::error::{ErrorCode, KernError, Result};
use crate::event::payload;
use crate::event::{EventBus, EventKind};
use crate::store::model::{EventRecord, ExecutionUpdate, Transition};
use crate::store::{Event, ExecutionStatus, LifecycleState, Store};

impl LifecycleState {
    /// The §3.2 transition table. `true` only for the transitions listed in
    /// `SPEC.md §3.2` (with the documented amendments: terminal → starting for
    /// a new run, and interrupted → recovering on daemon restart). Anything
    /// else is a programming error.
    pub fn can_transition(self, to: LifecycleState) -> bool {
        use LifecycleState::*;
        matches!(
            (self, to),
            // start (first run or new run)
            (Created | Completed | Failed | Terminated, Starting)
                | (Starting, Running)
                | (Starting, Failed)
                | (Starting, Recovering)
                | (Starting, Terminated)
                | (Running, Paused)
                | (Running, Waiting)
                | (Running, Sleeping)
                | (Running, Completed)
                | (Running, Failed)
                | (Running, Terminated)
                | (Running, Recovering)
                | (Waiting, Running)
                | (Waiting, Paused)
                | (Waiting, Failed)
                | (Waiting, Terminated)
                | (Waiting, Recovering)
                | (Sleeping, Running)
                | (Sleeping, Failed)
                | (Sleeping, Terminated)
                | (Paused, Running)
                | (Paused, Terminated)
                | (Recovering, Running)
                | (Recovering, Failed)
                | (Recovering, Terminated)
        )
    }
}

/// Applies lifecycle actions: validates against §3.2, persists atomically, and
/// broadcasts the resulting events.
#[derive(Clone)]
pub struct Lifecycle {
    store: Arc<Store>,
    bus: EventBus,
}

impl Lifecycle {
    pub fn new(store: Arc<Store>, bus: EventBus) -> Self {
        Self { store, bus }
    }

    /// `start`: created|completed|failed|terminated → starting (new run).
    /// Resets `last_error`; no event (the runner emits `execution.started` +
    /// `agent.started` when ready).
    pub async fn start(&self, agent_id: &str) -> Result<Vec<Event>> {
        self.apply(
            agent_id,
            LifecycleState::Starting,
            None,
            None,
            Vec::new(),
            |from| from.can_transition(LifecycleState::Starting),
        )
        .await
    }

    /// Runner ready: starting → running. Emits `execution.started` +
    /// `agent.started` and marks the execution running.
    pub async fn mark_started(&self, agent_id: &str, execution_id: &str) -> Result<Vec<Event>> {
        let events = vec![
            EventRecord {
                kind: EventKind::ExecutionStarted.as_str(),
                execution_id: Some(execution_id.to_string()),
                payload: payload::execution_started(execution_id, agent_id),
            },
            EventRecord {
                kind: EventKind::AgentStarted.as_str(),
                execution_id: Some(execution_id.to_string()),
                payload: payload::agent_started(agent_id, execution_id),
            },
        ];
        let execution = ExecutionUpdate {
            id: execution_id.to_string(),
            status: ExecutionStatus::Running,
            started_at: Some(Utc::now()),
            finished_at: None,
        };
        self.apply(
            agent_id,
            LifecycleState::Running,
            None,
            Some(execution),
            events,
            |from| from == LifecycleState::Starting,
        )
        .await
    }

    /// Loop finished: running → completed. Emits `execution.completed` +
    /// `agent.completed` and finishes the execution.
    pub async fn complete(
        &self,
        agent_id: &str,
        execution_id: &str,
        final_text: &str,
        steps: u64,
        checkpoints: u64,
    ) -> Result<Vec<Event>> {
        let events = vec![
            EventRecord {
                kind: EventKind::ExecutionCompleted.as_str(),
                execution_id: Some(execution_id.to_string()),
                payload: payload::execution_completed(execution_id, steps, checkpoints),
            },
            EventRecord {
                kind: EventKind::AgentCompleted.as_str(),
                execution_id: Some(execution_id.to_string()),
                payload: payload::agent_completed(agent_id, execution_id, final_text),
            },
        ];
        let execution = ExecutionUpdate {
            id: execution_id.to_string(),
            status: ExecutionStatus::Completed,
            started_at: None,
            finished_at: Some(Utc::now()),
        };
        self.apply(
            agent_id,
            LifecycleState::Completed,
            None,
            Some(execution),
            events,
            |from| from.can_transition(LifecycleState::Completed),
        )
        .await
    }

    /// Unrecoverable error: starting|running → failed. Emits
    /// `execution.failed` + `agent.failed`, attaches the error, and finishes
    /// the execution.
    pub async fn fail(
        &self,
        agent_id: &str,
        execution_id: &str,
        error: &KernError,
    ) -> Result<Vec<Event>> {
        let store = Arc::clone(&self.store);
        let agent_id_owned = agent_id.to_string();
        let agent = tokio::task::spawn_blocking(move || store.get_agent(&agent_id_owned))
            .await
            .map_err(|e| KernError::internal(format!("lifecycle read task failed: {e}")))??;

        if !agent.state.can_transition(LifecycleState::Failed) {
            // The run died before its first lifecycle transition — e.g. a
            // storage error in `lifecycle.start` or a runner
            // spawn that never ran. The agent row is untouched (it stays
            // `created`), but the execution must not linger `pending`: the
            // partial `ux_executions_one_active` index treats pending rows as
            // an active execution and would refuse every future run of this
            // agent with `EXECUTION_ALREADY_ACTIVE`. Fail the execution row
            // directly and record the failure events. The agent itself needs
            // no transition — it never started.
            let store = Arc::clone(&self.store);
            let execution_id_owned = execution_id.to_string();
            let failed = tokio::task::spawn_blocking(move || {
                store.fail_pending_execution(&execution_id_owned)
            })
            .await
            .map_err(|e| KernError::internal(format!("lifecycle task failed: {e}")))??;
            if failed {
                let mut persisted = Vec::with_capacity(2);
                persisted.push(
                    self.bus
                        .emit(
                            EventKind::ExecutionFailed,
                            Some(agent_id),
                            Some(execution_id),
                            payload::execution_failed(execution_id, error),
                        )
                        .await?,
                );
                persisted.push(
                    self.bus
                        .emit(
                            EventKind::AgentFailed,
                            Some(agent_id),
                            Some(execution_id),
                            payload::agent_failed(agent_id, execution_id, error),
                        )
                        .await?,
                );
                return Ok(persisted);
            }
            // Another path already failed the execution; nothing to repair.
            return Ok(Vec::new());
        }

        let events = vec![
            EventRecord {
                kind: EventKind::ExecutionFailed.as_str(),
                execution_id: Some(execution_id.to_string()),
                payload: payload::execution_failed(execution_id, error),
            },
            EventRecord {
                kind: EventKind::AgentFailed.as_str(),
                execution_id: Some(execution_id.to_string()),
                payload: payload::agent_failed(agent_id, execution_id, error),
            },
        ];
        let execution = ExecutionUpdate {
            id: execution_id.to_string(),
            status: ExecutionStatus::Failed,
            started_at: None,
            finished_at: Some(Utc::now()),
        };
        self.apply(
            agent_id,
            LifecycleState::Failed,
            Some(error.to_string()),
            Some(execution),
            events,
            |from| from.can_transition(LifecycleState::Failed),
        )
        .await
    }

    /// `pause`: running|waiting → paused. The caller MUST have checkpointed
    /// first (§3.2); the runner task is suspended by the caller (registry
    /// abort). Emits `agent.paused`.
    pub async fn pause(&self, agent_id: &str, checkpoint_id: &str) -> Result<Vec<Event>> {
        let events = vec![EventRecord {
            kind: EventKind::AgentPaused.as_str(),
            execution_id: None,
            payload: payload::agent_paused(agent_id, checkpoint_id),
        }];
        self.apply(
            agent_id,
            LifecycleState::Paused,
            None,
            None,
            events,
            |from| from.can_transition(LifecycleState::Paused),
        )
        .await
    }

    /// `resume`: paused|recovering → running. Emits `agent.resumed`
    /// (`checkpoint_id` null when no checkpoint was restored, per §6).
    pub async fn resume(&self, agent_id: &str, checkpoint_id: Option<&str>) -> Result<Vec<Event>> {
        let events = vec![EventRecord {
            kind: EventKind::AgentResumed.as_str(),
            execution_id: None,
            payload: payload::agent_resumed(agent_id, checkpoint_id),
        }];
        self.apply(
            agent_id,
            LifecycleState::Running,
            None,
            None,
            events,
            |from| {
                matches!(
                    from,
                    LifecycleState::Paused | LifecycleState::Recovering | LifecycleState::Sleeping
                )
            },
        )
        .await
    }

    /// Durable sleep: running → sleeping. The runner has already
    /// checkpointed (the terminal sleep row + result are in the session) and
    /// the execution's `wake_at` is persisted BEFORE this transition, so a
    /// crash between the two leaves a `running` agent that recovery resumes
    /// (benign early wake), never a sleeping agent with no wake time. Emits
    /// `agent.sleeping`.
    pub async fn park(&self, agent_id: &str, wake_at: &str) -> Result<Vec<Event>> {
        let events = vec![EventRecord {
            kind: EventKind::AgentSleeping.as_str(),
            execution_id: None,
            payload: payload::agent_sleeping(agent_id, wake_at),
        }];
        self.apply(
            agent_id,
            LifecycleState::Sleeping,
            None,
            None,
            events,
            |from| from.can_transition(LifecycleState::Sleeping),
        )
        .await
    }

    /// `terminate`: running|waiting|sleeping|paused|recovering → terminated. Aborts the
    /// runner (caller's registry action), emits `agent.terminated`, and marks
    /// the execution interrupted so a new run is possible.
    pub async fn terminate(
        &self,
        agent_id: &str,
        execution_id: Option<&str>,
    ) -> Result<Vec<Event>> {
        let events = vec![EventRecord {
            kind: EventKind::AgentTerminated.as_str(),
            execution_id: execution_id.map(str::to_string),
            payload: payload::agent_terminated(agent_id),
        }];
        let execution = execution_id.map(|id| ExecutionUpdate {
            id: id.to_string(),
            status: ExecutionStatus::Interrupted,
            started_at: None,
            finished_at: Some(Utc::now()),
        });
        self.apply(
            agent_id,
            LifecycleState::Terminated,
            None,
            execution,
            events,
            |from| from.can_transition(LifecycleState::Terminated),
        )
        .await
    }

    /// The ask park resolved WITHOUT an operator decision (every request
    /// expired): waiting → running so the loop can continue.
    /// Emits `agent.resumed` (checkpoint_id None). CAS-guarded on `waiting`:
    /// a decision that landed between the park poll and this call already
    /// resumed the agent, and the transition is then rejected.
    pub async fn unpark(&self, agent_id: &str) -> Result<Vec<Event>> {
        let events = vec![EventRecord {
            kind: EventKind::AgentResumed.as_str(),
            execution_id: None,
            payload: payload::agent_resumed(agent_id, None),
        }];
        self.apply(
            agent_id,
            LifecycleState::Running,
            None,
            None,
            events,
            |from| from == LifecycleState::Waiting,
        )
        .await
    }

    /// Policy = ask: running → waiting. Emits `permission.asked` +
    /// `agent.waiting` and suspends the loop until the decision.
    pub async fn wait(
        &self,
        agent_id: &str,
        permission_request_id: &str,
        resource: &str,
        action: &str,
    ) -> Result<Vec<Event>> {
        let events = vec![
            EventRecord {
                kind: EventKind::PermissionAsked.as_str(),
                execution_id: None,
                payload: payload::permission_asked(permission_request_id, resource, action),
            },
            EventRecord {
                kind: EventKind::AgentWaiting.as_str(),
                execution_id: None,
                payload: payload::agent_waiting(agent_id, permission_request_id, resource, action),
            },
        ];
        self.apply(
            agent_id,
            LifecycleState::Waiting,
            None,
            None,
            events,
            |from| from.can_transition(LifecycleState::Waiting),
        )
        .await
    }

    /// Permission decision: waiting → running. Emits `permission.granted` (or
    /// `permission.denied` with a reason) plus `agent.resumed`.
    ///
    /// When the agent is ALREADY running (a previous decision in the same ask
    /// batch resolved the wait), only the permission event is emitted — the
    /// transition would be invalid and `agent.resumed` already fired. This
    /// keeps multi-request batches event-complete.
    pub async fn resolve_wait(
        &self,
        agent_id: &str,
        permission_request_id: &str,
        resource: &str,
        granted: bool,
        reason: &str,
    ) -> Result<Vec<Event>> {
        let permission = if granted {
            EventRecord {
                kind: EventKind::PermissionGranted.as_str(),
                execution_id: None,
                payload: payload::permission_granted(permission_request_id, resource),
            }
        } else {
            EventRecord {
                kind: EventKind::PermissionDenied.as_str(),
                execution_id: None,
                payload: payload::permission_denied(permission_request_id, resource, reason),
            }
        };
        let store = Arc::clone(&self.store);
        let agent_owned = agent_id.to_string();
        let agent = tokio::task::spawn_blocking(move || store.get_agent(&agent_owned))
            .await
            .map_err(|e| KernError::internal(format!("lifecycle read task failed: {e}")))??;

        match agent.state {
            LifecycleState::Waiting => {
                let events = vec![
                    permission,
                    EventRecord {
                        kind: EventKind::AgentResumed.as_str(),
                        execution_id: None,
                        payload: payload::agent_resumed(agent_id, None),
                    },
                ];
                self.apply(
                    agent_id,
                    LifecycleState::Running,
                    None,
                    None,
                    events,
                    |from| from == LifecycleState::Waiting,
                )
                .await
            }
            LifecycleState::Running => {
                // The wait was already resolved by an earlier decision in the
                // batch; append only this decision's event (guarded by the
                // state, so a racing termination still rejects it).
                self.apply(
                    agent_id,
                    LifecycleState::Running,
                    None,
                    None,
                    vec![permission],
                    |from| from == LifecycleState::Running,
                )
                .await
            }
            other => Err(KernError::new(
                ErrorCode::InvalidTransition,
                format!(
                    "cannot resolve permission request {permission_request_id}: agent {agent_id} is {}",
                    other.as_str()
                ),
            )),
        }
    }

    /// Daemon-restart reconciliation: starting|running|waiting → recovering.
    /// Marks an interrupted agent for recovery; no event is emitted here.
    pub async fn recover(&self, agent_id: &str) -> Result<Vec<Event>> {
        self.apply(
            agent_id,
            LifecycleState::Recovering,
            None,
            None,
            Vec::new(),
            |from| {
                matches!(
                    from,
                    LifecycleState::Starting | LifecycleState::Running | LifecycleState::Waiting
                )
            },
        )
        .await
    }

    /// Shared apply: read current state, validate against `allowed_from`,
    /// persist atomically (guarded), broadcast, return the events.
    async fn apply(
        &self,
        agent_id: &str,
        to: LifecycleState,
        last_error: Option<String>,
        execution: Option<ExecutionUpdate>,
        events: Vec<EventRecord>,
        allowed_from: impl Fn(LifecycleState) -> bool,
    ) -> Result<Vec<Event>> {
        let store = Arc::clone(&self.store);
        let agent_id_owned = agent_id.to_string();
        let agent = tokio::task::spawn_blocking(move || store.get_agent(&agent_id_owned))
            .await
            .map_err(|e| KernError::internal(format!("lifecycle read task failed: {e}")))??;

        if !allowed_from(agent.state) {
            return Err(KernError::new(
                ErrorCode::InvalidTransition,
                format!(
                    "cannot transition agent {agent_id} from {} to {}",
                    agent.state.as_str(),
                    to.as_str()
                ),
            ));
        }

        let transition = Transition {
            agent_id: agent.id,
            expected_state: agent.state,
            new_state: to,
            last_error,
            execution,
            events,
        };
        let store = Arc::clone(&self.store);
        let events = tokio::task::spawn_blocking(move || store.transition(&transition))
            .await
            .map_err(|e| KernError::internal(format!("lifecycle task failed: {e}")))??;

        self.bus.publish(&events);
        Ok(events)
    }
}

/// Build an `INVALID_TRANSITION` error describing the rejected action (helper
/// for callers that want to surface which state was required).
pub fn invalid_transition(agent_id: &str, from: LifecycleState, to: LifecycleState) -> KernError {
    KernError::new(
        ErrorCode::InvalidTransition,
        format!(
            "cannot transition agent {agent_id} from {} to {}",
            from.as_str(),
            to.as_str()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::sync::Arc;

    async fn test_ctx() -> (tempfile::TempDir, Arc<Store>, EventBus, Lifecycle) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let bus = EventBus::new(Arc::clone(&store));
        let lifecycle = Lifecycle::new(Arc::clone(&store), bus.clone());
        (dir, store, bus, lifecycle)
    }

    fn create_agent(store: &Store, name: &str) -> crate::store::Agent {
        let agent = crate::store::Agent::new(name, Value::Null, LifecycleState::Created);
        store.create_agent(&agent).unwrap();
        agent
    }

    async fn start_and_run(lifecycle: &Lifecycle, store: &Store, agent_id: &str) -> String {
        lifecycle.start(agent_id).await.unwrap();
        let execution = crate::store::Execution::new(agent_id, ExecutionStatus::Pending);
        store.create_execution(&execution).unwrap();
        lifecycle
            .mark_started(agent_id, &execution.id)
            .await
            .unwrap();
        execution.id
    }

    /// The authoritative §3.2 table (with the documented amendments), encoded
    /// as (from, to) pairs. `can_transition` MUST match this exactly.
    const EXPECTED_TRANSITIONS: &[(LifecycleState, LifecycleState)] = &[
        (LifecycleState::Created, LifecycleState::Starting),
        (LifecycleState::Completed, LifecycleState::Starting),
        (LifecycleState::Failed, LifecycleState::Starting),
        (LifecycleState::Terminated, LifecycleState::Starting),
        (LifecycleState::Starting, LifecycleState::Running),
        (LifecycleState::Starting, LifecycleState::Failed),
        (LifecycleState::Starting, LifecycleState::Recovering),
        (LifecycleState::Starting, LifecycleState::Terminated),
        (LifecycleState::Running, LifecycleState::Paused),
        (LifecycleState::Running, LifecycleState::Waiting),
        (LifecycleState::Running, LifecycleState::Sleeping),
        (LifecycleState::Running, LifecycleState::Completed),
        (LifecycleState::Running, LifecycleState::Failed),
        (LifecycleState::Running, LifecycleState::Terminated),
        (LifecycleState::Running, LifecycleState::Recovering),
        (LifecycleState::Waiting, LifecycleState::Running),
        (LifecycleState::Waiting, LifecycleState::Paused),
        (LifecycleState::Waiting, LifecycleState::Failed),
        (LifecycleState::Waiting, LifecycleState::Terminated),
        (LifecycleState::Waiting, LifecycleState::Recovering),
        (LifecycleState::Sleeping, LifecycleState::Running),
        (LifecycleState::Sleeping, LifecycleState::Failed),
        (LifecycleState::Sleeping, LifecycleState::Terminated),
        (LifecycleState::Paused, LifecycleState::Running),
        (LifecycleState::Paused, LifecycleState::Terminated),
        (LifecycleState::Recovering, LifecycleState::Running),
        (LifecycleState::Recovering, LifecycleState::Failed),
        (LifecycleState::Recovering, LifecycleState::Terminated),
    ];

    #[test]
    fn transition_table_matches_spec() {
        let states = [
            LifecycleState::Created,
            LifecycleState::Starting,
            LifecycleState::Running,
            LifecycleState::Paused,
            LifecycleState::Waiting,
            LifecycleState::Sleeping,
            LifecycleState::Recovering,
            LifecycleState::Completed,
            LifecycleState::Failed,
            LifecycleState::Terminated,
        ];
        for from in states {
            for to in states {
                let expected = EXPECTED_TRANSITIONS.contains(&(from, to));
                assert_eq!(
                    from.can_transition(to),
                    expected,
                    "can_transition({from:?} -> {to:?}) disagrees with SPEC §3.2"
                );
            }
        }
        // The expected list itself must not contain duplicates or self-pairs.
        let mut seen = std::collections::HashSet::new();
        for (from, to) in EXPECTED_TRANSITIONS {
            assert!(seen.insert((*from, *to)), "duplicate table entry");
            assert_ne!(from, to, "self-transition in table");
        }
    }

    #[tokio::test]
    async fn full_lifecycle_emits_correct_events() {
        let (_dir, store, bus, lifecycle) = test_ctx().await;
        let agent = create_agent(&store, "full");

        // created → starting → running
        let events = lifecycle.start(&agent.id).await.unwrap();
        assert!(events.is_empty(), "start emits no event");
        assert_eq!(
            store.get_agent(&agent.id).unwrap().state,
            LifecycleState::Starting
        );

        let execution = crate::store::Execution::new(&agent.id, ExecutionStatus::Pending);
        store.create_execution(&execution).unwrap();
        let events = lifecycle
            .mark_started(&agent.id, &execution.id)
            .await
            .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "execution.started");
        assert_eq!(events[1].kind, "agent.started");
        assert_eq!(
            store.get_agent(&agent.id).unwrap().state,
            LifecycleState::Running
        );
        let exec = store.get_execution(&execution.id).unwrap();
        assert_eq!(exec.status, ExecutionStatus::Running);
        assert!(exec.started_at.is_some());

        // running → waiting → running (grant)
        let events = lifecycle
            .wait(&agent.id, "pr-1", "filesystem:write", "write ./x")
            .await
            .unwrap();
        assert_eq!(
            events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            ["permission.asked", "agent.waiting"]
        );
        assert_eq!(
            store.get_agent(&agent.id).unwrap().state,
            LifecycleState::Waiting
        );

        let events = lifecycle
            .resolve_wait(&agent.id, "pr-1", "filesystem:write", true, "")
            .await
            .unwrap();
        assert_eq!(
            events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            ["permission.granted", "agent.resumed"]
        );
        assert_eq!(
            store.get_agent(&agent.id).unwrap().state,
            LifecycleState::Running
        );

        // running → paused → running
        let events = lifecycle.pause(&agent.id, "cp-1").await.unwrap();
        assert_eq!(events[0].kind, "agent.paused");
        assert_eq!(events[0].payload["checkpoint_id"], "cp-1");
        assert_eq!(
            store.get_agent(&agent.id).unwrap().state,
            LifecycleState::Paused
        );

        let events = lifecycle.resume(&agent.id, Some("cp-1")).await.unwrap();
        assert_eq!(events[0].kind, "agent.resumed");
        assert_eq!(
            store.get_agent(&agent.id).unwrap().state,
            LifecycleState::Running
        );

        // running → completed
        let events = lifecycle
            .complete(&agent.id, &execution.id, "all done", 7, 3)
            .await
            .unwrap();
        assert_eq!(
            events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            ["execution.completed", "agent.completed"]
        );
        assert_eq!(events[1].payload["final_text"], "all done");
        assert_eq!(
            store.get_agent(&agent.id).unwrap().state,
            LifecycleState::Completed
        );
        assert_eq!(
            store.get_execution(&execution.id).unwrap().status,
            ExecutionStatus::Completed
        );

        // The whole sequence is durably replayable, in order.
        let replayed = bus.replay(0, 100).await.unwrap();
        let kinds: Vec<&str> = replayed.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "execution.started",
                "agent.started",
                "permission.asked",
                "agent.waiting",
                "permission.granted",
                "agent.resumed",
                "agent.paused",
                "agent.resumed",
                "execution.completed",
                "agent.completed",
            ]
        );
    }

    #[tokio::test]
    async fn fail_path_attaches_error_and_marks_execution_failed() {
        let (_dir, store, _bus, lifecycle) = test_ctx().await;
        let agent = create_agent(&store, "fail");
        let execution_id = start_and_run(&lifecycle, &store, &agent.id).await;

        let err = KernError::new(ErrorCode::ModelTimeout, "model timed out");
        let events = lifecycle
            .fail(&agent.id, &execution_id, &err)
            .await
            .unwrap();
        assert_eq!(
            events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            ["execution.failed", "agent.failed"]
        );
        assert_eq!(events[1].payload["error"]["code"], "MODEL_TIMEOUT");

        let agent = store.get_agent(&agent.id).unwrap();
        assert_eq!(agent.state, LifecycleState::Failed);
        assert!(agent
            .last_error
            .as_deref()
            .unwrap()
            .contains("MODEL_TIMEOUT"));
        assert_eq!(
            store.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Failed
        );
    }

    #[tokio::test]
    async fn deny_path_emits_permission_denied() {
        let (_dir, store, _bus, lifecycle) = test_ctx().await;
        let agent = create_agent(&store, "deny");
        start_and_run(&lifecycle, &store, &agent.id).await;
        lifecycle
            .wait(&agent.id, "pr-9", "network:host", "GET api.example.com")
            .await
            .unwrap();
        let events = lifecycle
            .resolve_wait(&agent.id, "pr-9", "network:host", false, "policy denies")
            .await
            .unwrap();
        assert_eq!(events[0].kind, "permission.denied");
        assert_eq!(events[0].payload["reason"], "policy denies");
        assert_eq!(events[1].kind, "agent.resumed");
    }

    #[tokio::test]
    async fn terminate_from_any_non_terminal_state() {
        for state in [
            LifecycleState::Starting,
            LifecycleState::Running,
            LifecycleState::Waiting,
            LifecycleState::Paused,
            LifecycleState::Recovering,
        ] {
            let (_dir, store, _bus, lifecycle) = test_ctx().await;
            let agent = create_agent(&store, &format!("term-{state:?}"));
            // Move the agent into `state` via the legal path(s).
            match state {
                LifecycleState::Starting => {
                    lifecycle.start(&agent.id).await.unwrap();
                }
                LifecycleState::Running => {
                    start_and_run(&lifecycle, &store, &agent.id).await;
                }
                LifecycleState::Waiting => {
                    start_and_run(&lifecycle, &store, &agent.id).await;
                    lifecycle
                        .wait(&agent.id, "pr-1", "resource", "action")
                        .await
                        .unwrap();
                }
                LifecycleState::Paused => {
                    start_and_run(&lifecycle, &store, &agent.id).await;
                    lifecycle.pause(&agent.id, "cp-1").await.unwrap();
                }
                LifecycleState::Recovering => {
                    lifecycle.start(&agent.id).await.unwrap();
                    lifecycle.recover(&agent.id).await.unwrap();
                }
                _ => unreachable!(),
            }
            let execution = store.list_executions_for_agent(&agent.id).unwrap().pop();
            let events = lifecycle
                .terminate(&agent.id, execution.as_ref().map(|e| e.id.as_str()))
                .await
                .unwrap();
            assert_eq!(events[0].kind, "agent.terminated");
            assert_eq!(
                store.get_agent(&agent.id).unwrap().state,
                LifecycleState::Terminated
            );
            if let Some(execution) = execution {
                assert_eq!(
                    store.get_execution(&execution.id).unwrap().status,
                    ExecutionStatus::Interrupted
                );
            }
        }
    }

    #[tokio::test]
    async fn terminal_states_can_start_new_run() {
        let (_dir, store, _bus, lifecycle) = test_ctx().await;
        let agent = create_agent(&store, "rerun");
        let execution_id = start_and_run(&lifecycle, &store, &agent.id).await;
        lifecycle
            .complete(&agent.id, &execution_id, "done", 1, 0)
            .await
            .unwrap();
        assert_eq!(
            store.get_agent(&agent.id).unwrap().state,
            LifecycleState::Completed
        );

        // New run: completed → starting → running, execution history retained.
        lifecycle.start(&agent.id).await.unwrap();
        assert_eq!(
            store.get_agent(&agent.id).unwrap().state,
            LifecycleState::Starting
        );
        let second = crate::store::Execution::new(&agent.id, ExecutionStatus::Pending);
        store.create_execution(&second).unwrap();
        lifecycle.mark_started(&agent.id, &second.id).await.unwrap();
        assert_eq!(
            store.get_agent(&agent.id).unwrap().state,
            LifecycleState::Running
        );
        assert_eq!(store.list_executions_for_agent(&agent.id).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn recover_marks_interrupted_agents() {
        let (_dir, store, _bus, lifecycle) = test_ctx().await;
        let agent = create_agent(&store, "interrupted");
        lifecycle.start(&agent.id).await.unwrap();
        assert_eq!(
            store.get_agent(&agent.id).unwrap().state,
            LifecycleState::Starting
        );
        let events = lifecycle.recover(&agent.id).await.unwrap();
        assert!(events.is_empty());
        assert_eq!(
            store.get_agent(&agent.id).unwrap().state,
            LifecycleState::Recovering
        );
    }

    #[tokio::test]
    async fn invalid_transitions_are_rejected() {
        let (_dir, store, _bus, lifecycle) = test_ctx().await;

        // mark_started on a created agent (must be starting).
        let agent = create_agent(&store, "inv");
        let err = lifecycle.mark_started(&agent.id, "ex-1").await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidTransition);

        // complete on a starting agent.
        lifecycle.start(&agent.id).await.unwrap();
        let err = lifecycle
            .complete(&agent.id, "ex-1", "x", 1, 0)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidTransition);

        // pause on a created agent.
        let fresh = create_agent(&store, "inv2");
        let err = lifecycle.pause(&fresh.id, "cp-1").await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidTransition);

        // resume on a running agent (must be paused/recovering).
        let runner = create_agent(&store, "inv-runner");
        start_and_run(&lifecycle, &store, &runner.id).await;
        let err = lifecycle.resume(&runner.id, None).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidTransition);

        // terminate on a created agent (no row in §3.2).
        let err = lifecycle.terminate(&fresh.id, None).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidTransition);

        // The agent state never changed on any rejected transition.
        assert_eq!(
            store.get_agent(&runner.id).unwrap().state,
            LifecycleState::Running
        );
        assert_eq!(
            store.get_agent(&fresh.id).unwrap().state,
            LifecycleState::Created
        );
    }

    #[tokio::test]
    async fn concurrent_transitions_only_one_wins() {
        let (_dir, store, _bus, lifecycle) = test_ctx().await;
        let agent = create_agent(&store, "race");
        lifecycle.start(&agent.id).await.unwrap();
        let execution_id = {
            let execution = crate::store::Execution::new(&agent.id, ExecutionStatus::Pending);
            store.create_execution(&execution).unwrap();
            execution.id
        };

        let mut handles = Vec::new();
        for _ in 0..8 {
            let lifecycle = lifecycle.clone();
            let agent_id = agent.id.clone();
            let execution_id = execution_id.clone();
            handles.push(tokio::spawn(async move {
                lifecycle.mark_started(&agent_id, &execution_id).await
            }));
        }
        let mut ok = 0;
        for handle in handles {
            match handle.await.unwrap() {
                Ok(_) => ok += 1,
                Err(e) => assert_eq!(e.code(), ErrorCode::InvalidTransition),
            }
        }
        assert_eq!(ok, 1, "exactly one concurrent transition may apply");
        assert_eq!(
            store.get_agent(&agent.id).unwrap().state,
            LifecycleState::Running
        );
        // Only one execution.started / agent.started pair was persisted.
        let events = store.events_after(0, 100).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == "execution.started")
                .count(),
            1
        );
    }
}
