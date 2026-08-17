//! Event system (ARCHITECTURE.md §12.1, SPEC.md §6).
//!
//! The catalog-pinned `EventKind` enum, typed payload builders
//! (`payload`), and the `EventBus` — an async facade over the durable `Store`
//! that persists every event and broadcasts it to live subscribers.
//!
//! Ordering contract:
//! - `seq` is assigned by SQLite (`AUTOINCREMENT`) and is monotonic across all
//!   writers; writes serialize on the store's writer mutex.
//! - Persistence happens *before* broadcast. SQLite is the source of truth;
//!   live delivery is best-effort. A subscriber that falls behind is dropped
//!   (bounded channel) with a logged warning and MUST resume by replaying from
//!   its last seen seq (`SubscriberError::Lagged`) — never by continuing to
//!   consume the live stream.
//!
//! The bus is the boundary between the synchronous store and the async engine:
//! every store call is offloaded with `tokio::task::spawn_blocking`, so engine
//! tasks never block the runtime on SQLite I/O.

pub mod payload;

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::error::{KernError, Result};
use crate::store::{Event, Store};

/// The catalog-pinned event kinds (`SPEC.md §6`). Adding or removing a kind
/// here without updating the pinned catalog test fails CI on purpose: the
/// catalog is the normative contract between the runtime and its consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventKind {
    #[serde(rename = "runtime.started")]
    RuntimeStarted,
    #[serde(rename = "runtime.shutting_down")]
    RuntimeShuttingDown,
    #[serde(rename = "agent.created")]
    AgentCreated,
    #[serde(rename = "agent.started")]
    AgentStarted,
    #[serde(rename = "agent.paused")]
    AgentPaused,
    #[serde(rename = "agent.resumed")]
    AgentResumed,
    #[serde(rename = "agent.thinking")]
    AgentThinking,
    #[serde(rename = "agent.waiting")]
    AgentWaiting,
    #[serde(rename = "agent.sleeping")]
    AgentSleeping,
    #[serde(rename = "agent.completed")]
    AgentCompleted,
    #[serde(rename = "agent.failed")]
    AgentFailed,
    #[serde(rename = "agent.terminated")]
    AgentTerminated,
    #[serde(rename = "execution.started")]
    ExecutionStarted,
    #[serde(rename = "execution.completed")]
    ExecutionCompleted,
    #[serde(rename = "execution.failed")]
    ExecutionFailed,
    #[serde(rename = "execution.restored")]
    ExecutionRestored,
    #[serde(rename = "model.requested")]
    ModelRequested,
    #[serde(rename = "model.completed")]
    ModelCompleted,
    #[serde(rename = "model.failed")]
    ModelFailed,
    #[serde(rename = "tool.requested")]
    ToolRequested,
    #[serde(rename = "tool.started")]
    ToolStarted,
    #[serde(rename = "tool.completed")]
    ToolCompleted,
    #[serde(rename = "tool.failed")]
    ToolFailed,
    #[serde(rename = "checkpoint.created")]
    CheckpointCreated,
    #[serde(rename = "checkpoint.restored")]
    CheckpointRestored,
    #[serde(rename = "checkpoint.failed")]
    CheckpointFailed,
    #[serde(rename = "permission.asked")]
    PermissionAsked,
    #[serde(rename = "permission.granted")]
    PermissionGranted,
    #[serde(rename = "permission.denied")]
    PermissionDenied,
    #[serde(rename = "scheduler.recovered_agent")]
    SchedulerRecoveredAgent,
    #[serde(rename = "scheduler.run_due")]
    SchedulerRunDue,
    #[serde(rename = "scheduler.backoff")]
    SchedulerBackoff,
}

impl EventKind {
    /// All catalog kinds in catalog order (`SPEC.md §6`).
    pub const ALL: [EventKind; 32] = [
        EventKind::RuntimeStarted,
        EventKind::RuntimeShuttingDown,
        EventKind::AgentCreated,
        EventKind::AgentStarted,
        EventKind::AgentPaused,
        EventKind::AgentResumed,
        EventKind::AgentThinking,
        EventKind::AgentWaiting,
        EventKind::AgentSleeping,
        EventKind::AgentCompleted,
        EventKind::AgentFailed,
        EventKind::AgentTerminated,
        EventKind::ExecutionStarted,
        EventKind::ExecutionCompleted,
        EventKind::ExecutionFailed,
        EventKind::ExecutionRestored,
        EventKind::ModelRequested,
        EventKind::ModelCompleted,
        EventKind::ModelFailed,
        EventKind::ToolRequested,
        EventKind::ToolStarted,
        EventKind::ToolCompleted,
        EventKind::ToolFailed,
        EventKind::CheckpointCreated,
        EventKind::CheckpointRestored,
        EventKind::CheckpointFailed,
        EventKind::PermissionAsked,
        EventKind::PermissionGranted,
        EventKind::PermissionDenied,
        EventKind::SchedulerRecoveredAgent,
        EventKind::SchedulerRunDue,
        EventKind::SchedulerBackoff,
    ];

    /// The canonical kind string (e.g. `"tool.completed"`).
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::RuntimeStarted => "runtime.started",
            EventKind::RuntimeShuttingDown => "runtime.shutting_down",
            EventKind::AgentCreated => "agent.created",
            EventKind::AgentStarted => "agent.started",
            EventKind::AgentPaused => "agent.paused",
            EventKind::AgentResumed => "agent.resumed",
            EventKind::AgentThinking => "agent.thinking",
            EventKind::AgentWaiting => "agent.waiting",
            EventKind::AgentSleeping => "agent.sleeping",
            EventKind::AgentCompleted => "agent.completed",
            EventKind::AgentFailed => "agent.failed",
            EventKind::AgentTerminated => "agent.terminated",
            EventKind::ExecutionStarted => "execution.started",
            EventKind::ExecutionCompleted => "execution.completed",
            EventKind::ExecutionFailed => "execution.failed",
            EventKind::ExecutionRestored => "execution.restored",
            EventKind::ModelRequested => "model.requested",
            EventKind::ModelCompleted => "model.completed",
            EventKind::ModelFailed => "model.failed",
            EventKind::ToolRequested => "tool.requested",
            EventKind::ToolStarted => "tool.started",
            EventKind::ToolCompleted => "tool.completed",
            EventKind::ToolFailed => "tool.failed",
            EventKind::CheckpointCreated => "checkpoint.created",
            EventKind::CheckpointRestored => "checkpoint.restored",
            EventKind::CheckpointFailed => "checkpoint.failed",
            EventKind::PermissionAsked => "permission.asked",
            EventKind::PermissionGranted => "permission.granted",
            EventKind::PermissionDenied => "permission.denied",
            EventKind::SchedulerRecoveredAgent => "scheduler.recovered_agent",
            EventKind::SchedulerRunDue => "scheduler.run_due",
            EventKind::SchedulerBackoff => "scheduler.backoff",
        }
    }
}

impl FromStr for EventKind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "runtime.started" => Ok(EventKind::RuntimeStarted),
            "runtime.shutting_down" => Ok(EventKind::RuntimeShuttingDown),
            "agent.created" => Ok(EventKind::AgentCreated),
            "agent.started" => Ok(EventKind::AgentStarted),
            "agent.paused" => Ok(EventKind::AgentPaused),
            "agent.resumed" => Ok(EventKind::AgentResumed),
            "agent.thinking" => Ok(EventKind::AgentThinking),
            "agent.waiting" => Ok(EventKind::AgentWaiting),
            "agent.sleeping" => Ok(EventKind::AgentSleeping),
            "agent.completed" => Ok(EventKind::AgentCompleted),
            "agent.failed" => Ok(EventKind::AgentFailed),
            "agent.terminated" => Ok(EventKind::AgentTerminated),
            "execution.started" => Ok(EventKind::ExecutionStarted),
            "execution.completed" => Ok(EventKind::ExecutionCompleted),
            "execution.failed" => Ok(EventKind::ExecutionFailed),
            "execution.restored" => Ok(EventKind::ExecutionRestored),
            "model.requested" => Ok(EventKind::ModelRequested),
            "model.completed" => Ok(EventKind::ModelCompleted),
            "model.failed" => Ok(EventKind::ModelFailed),
            "tool.requested" => Ok(EventKind::ToolRequested),
            "tool.started" => Ok(EventKind::ToolStarted),
            "tool.completed" => Ok(EventKind::ToolCompleted),
            "tool.failed" => Ok(EventKind::ToolFailed),
            "checkpoint.created" => Ok(EventKind::CheckpointCreated),
            "checkpoint.restored" => Ok(EventKind::CheckpointRestored),
            "checkpoint.failed" => Ok(EventKind::CheckpointFailed),
            "permission.asked" => Ok(EventKind::PermissionAsked),
            "permission.granted" => Ok(EventKind::PermissionGranted),
            "permission.denied" => Ok(EventKind::PermissionDenied),
            "scheduler.recovered_agent" => Ok(EventKind::SchedulerRecoveredAgent),
            "scheduler.run_due" => Ok(EventKind::SchedulerRunDue),
            "scheduler.backoff" => Ok(EventKind::SchedulerBackoff),
            _ => Err(format!("invalid event kind: {s}")),
        }
    }
}

impl std::fmt::Display for EventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Retention warning threshold: events per agent before we warn about disk
/// growth (default is keep-all, `SPEC.md §6`). Warning repeats at every
/// multiple of the threshold.
pub const WARN_EVENTS_PER_AGENT: i64 = 100_000;

/// Default broadcast channel capacity: events buffered per live subscriber
/// before it lags and must replay.
pub const DEFAULT_CHANNEL_CAPACITY: usize = 1024;

/// Return the next warning milestone crossed by `total` given `last_warned` and
/// `interval`, or `None` when no new milestone was crossed.
fn next_milestone(total: i64, last_warned: i64, interval: i64) -> Option<i64> {
    let next = last_warned + interval;
    (total >= next).then_some(next)
}

/// Per-agent event tally for the retention size warning.
#[derive(Debug, Default)]
struct AgentCount {
    total: i64,
    last_warned_milestone: i64,
}

/// The runtime's event bus: durable append + live broadcast + replay.
///
/// Clone is cheap (shared store + channel) and safe to pass into tasks.
#[derive(Clone)]
pub struct EventBus {
    store: Arc<Store>,
    sender: broadcast::Sender<Event>,
    /// agent_id → (persisted count, last warned milestone). Seeded once per
    /// agent from the store so warnings stay accurate across daemon restarts.
    counts: Arc<Mutex<HashMap<String, AgentCount>>>,
}

impl EventBus {
    pub fn new(store: Arc<Store>) -> Self {
        Self::with_capacity(store, DEFAULT_CHANNEL_CAPACITY)
    }

    pub fn with_capacity(store: Arc<Store>, capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self {
            store,
            sender,
            counts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Persist `payload` as `kind` and broadcast it to live subscribers.
    ///
    /// The returned event carries its durable, monotonic `seq`. Persistence
    /// and broadcast are ordered: a subscriber never sees an event that is not
    /// already durable.
    pub async fn emit(
        &self,
        kind: EventKind,
        agent_id: Option<&str>,
        execution_id: Option<&str>,
        payload: Value,
    ) -> Result<Event> {
        let store = Arc::clone(&self.store);
        let counts = Arc::clone(&self.counts);
        let agent_owned = agent_id.map(str::to_string);
        let execution_owned = execution_id.map(str::to_string);
        let kind_str = kind.as_str();

        let event = tokio::task::spawn_blocking(move || {
            if let Some(agent) = agent_owned.as_deref() {
                let mut map = counts.lock().expect("event counters mutex poisoned");
                let state = map.entry(agent.to_string()).or_insert_with(|| AgentCount {
                    total: store.event_count_for_agent(agent).unwrap_or(0),
                    last_warned_milestone: 0,
                });
                state.total += 1;
                if let Some(milestone) = next_milestone(
                    state.total,
                    state.last_warned_milestone,
                    WARN_EVENTS_PER_AGENT,
                ) {
                    tracing::warn!(
                        agent_id = agent,
                        events = state.total,
                        "event retention: agent crossed {WARN_EVENTS_PER_AGENT} persisted events; \
                         enable event-retention pruning to bound disk growth"
                    );
                    state.last_warned_milestone = milestone;
                }
            }
            store.append_event(
                kind_str,
                agent_owned.as_deref(),
                execution_owned.as_deref(),
                payload,
            )
        })
        .await
        .map_err(|e| KernError::internal(format!("event writer task failed: {e}")))??;

        // Broadcast is best-effort live delivery; `Closed` just means no
        // subscribers are left, which is fine.
        let _ = self.sender.send(event.clone());
        Ok(event)
    }

    /// Replay persisted events with `seq > after_seq`, oldest first.
    pub async fn replay(&self, after_seq: i64, limit: usize) -> Result<Vec<Event>> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || store.events_after(after_seq, limit))
            .await
            .map_err(|e| KernError::internal(format!("event replay task failed: {e}")))?
    }

    /// Replay persisted events for one agent with `seq > after_seq`, oldest
    /// first.
    pub async fn replay_for_agent(
        &self,
        agent_id: &str,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<Event>> {
        let store = Arc::clone(&self.store);
        let agent_id = agent_id.to_string();
        tokio::task::spawn_blocking(move || {
            store.events_for_agent_after(&agent_id, after_seq, limit)
        })
        .await
        .map_err(|e| KernError::internal(format!("event replay task failed: {e}")))?
    }

    /// Highest persisted `seq` (0 when the store has no events). A subscriber
    /// joining late can replay from here and then switch to live delivery.
    pub async fn latest_seq(&self) -> Result<i64> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || store.latest_event_seq())
            .await
            .map_err(|e| KernError::internal(format!("event replay task failed: {e}")))?
    }

    /// Broadcast already-persisted events (e.g. appended atomically with a
    /// lifecycle transition by `Store::transition`) to live subscribers.
    /// No persistence happens here; SQLite is already the source of truth.
    pub fn publish(&self, events: &[Event]) {
        for event in events {
            let _ = self.sender.send(event.clone());
        }
    }

    /// Open a live subscription with bounded buffering. A subscriber that
    /// falls behind is dropped with a logged warning and must replay.
    pub fn subscribe(&self) -> Subscriber {
        Subscriber {
            rx: self.sender.subscribe(),
        }
    }
}

/// A live event subscription (bounded buffer).
///
/// `Lagged` means the subscriber fell behind and the missed events were
/// dropped: replay from the last seen seq before resuming, or continue with a
/// gap. `Closed` means the bus is gone (daemon shutdown).
#[derive(Debug)]
pub struct Subscriber {
    rx: broadcast::Receiver<Event>,
}

impl Subscriber {
    pub async fn recv(&mut self) -> std::result::Result<Event, SubscriberError> {
        match self.rx.recv().await {
            Ok(event) => Ok(event),
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    skipped,
                    "event subscriber fell behind; missed events dropped — replay from last seq"
                );
                Err(SubscriberError::Lagged { skipped })
            }
            Err(broadcast::error::RecvError::Closed) => Err(SubscriberError::Closed),
        }
    }
}

/// Errors a live subscriber can observe (`broadcast`-specific conditions).
#[derive(Debug, thiserror::Error)]
pub enum SubscriberError {
    #[error("event subscriber fell behind; {skipped} events were dropped")]
    Lagged { skipped: u64 },
    #[error("event stream closed")]
    Closed,
}

#[cfg(test)]
mod tests {
    use super::payload::{self, ModelOutcomeKind};
    use super::*;
    use crate::error::ErrorCode;
    use crate::version::STORAGE_SCHEMA_VERSION;
    use serde_json::json;

    /// The `SPEC.md §6` catalog: kind string → required payload keys. This is
    /// the pinned contract; any drift from the spec fails the catalog tests.
    const CATALOG: &[(&str, &[&str])] = &[
        (
            "runtime.started",
            &["instance_id", "schema_version", "runtime_version"],
        ),
        ("runtime.shutting_down", &[]),
        ("agent.created", &["agent_id", "name"]),
        ("agent.started", &["agent_id", "execution_id"]),
        ("agent.paused", &["agent_id", "checkpoint_id"]),
        ("agent.resumed", &["agent_id", "checkpoint_id"]),
        ("agent.thinking", &["agent_id", "step", "text"]),
        (
            "agent.waiting",
            &["agent_id", "permission_request_id", "resource", "action"],
        ),
        ("agent.sleeping", &["agent_id", "wake_at"]),
        (
            "agent.completed",
            &["agent_id", "execution_id", "final_text"],
        ),
        ("agent.failed", &["agent_id", "execution_id", "error"]),
        ("agent.terminated", &["agent_id"]),
        ("execution.started", &["execution_id", "agent_id"]),
        (
            "execution.completed",
            &["execution_id", "steps", "checkpoints"],
        ),
        ("execution.failed", &["execution_id", "error"]),
        ("execution.restored", &["execution_id", "checkpoint_id"]),
        ("model.requested", &["provider", "model", "step"]),
        (
            "model.completed",
            &["provider", "model", "kind", "latency_ms"],
        ),
        ("model.failed", &["error"]),
        ("tool.requested", &["tool_call_id", "tool_name", "args"]),
        ("tool.started", &["tool_call_id", "tool_name"]),
        (
            "tool.completed",
            &["tool_call_id", "tool_name", "latency_ms", "result_size"],
        ),
        ("tool.failed", &["tool_call_id", "tool_name", "error"]),
        (
            "checkpoint.created",
            &["checkpoint_id", "execution_id", "seq"],
        ),
        ("checkpoint.restored", &["checkpoint_id", "execution_id"]),
        ("checkpoint.failed", &["error"]),
        (
            "permission.asked",
            &["permission_request_id", "resource", "action"],
        ),
        ("permission.granted", &["permission_request_id", "resource"]),
        (
            "permission.denied",
            &["permission_request_id", "resource", "reason"],
        ),
        (
            "scheduler.recovered_agent",
            &["agent_id", "execution_id", "checkpoint_id"],
        ),
        ("scheduler.run_due", &["agent_id", "scheduled_for"]),
        (
            "scheduler.backoff",
            &["agent_id", "consecutive_failures", "next_run_at"],
        ),
    ];

    async fn test_bus() -> (tempfile::TempDir, Arc<Store>, EventBus) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let bus = EventBus::new(Arc::clone(&store));
        (dir, store, bus)
    }

    // ------------------------------------------------------------------
    // Catalog pinning
    // ------------------------------------------------------------------

    #[test]
    fn event_kind_catalog_is_pinned_to_spec() {
        let expected: Vec<&str> = CATALOG.iter().map(|(k, _)| *k).collect();
        let actual: Vec<&str> = EventKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(
            actual, expected,
            "EventKind::ALL drifted from SPEC.md §6 (add/remove kinds and update the test)"
        );

        let mut seen = std::collections::HashSet::new();
        for kind in EventKind::ALL {
            assert!(seen.insert(kind.as_str()), "duplicate kind: {kind}");
        }

        for (s, _) in CATALOG {
            let kind = EventKind::from_str(s).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(kind.as_str(), *s, "as_str round-trip failed");
            let ser = serde_json::to_value(kind).unwrap();
            assert_eq!(ser, json!(s), "serde name differs from catalog");
            let back: EventKind = serde_json::from_value(ser).unwrap();
            assert_eq!(back, kind, "deserialization round-trip failed");
        }

        assert!(EventKind::from_str("not.a.kind").is_err());
    }

    #[test]
    fn payload_builders_match_catalog_schema() {
        let err = KernError::new(ErrorCode::ModelTimeout, "model timed out");
        let checks: Vec<(EventKind, Value, &[&str])> = vec![
            (
                EventKind::RuntimeStarted,
                payload::runtime_started("inst-1", STORAGE_SCHEMA_VERSION, "0.1.0"),
                &["instance_id", "schema_version", "runtime_version"],
            ),
            (
                EventKind::RuntimeShuttingDown,
                payload::runtime_shutting_down(),
                &[],
            ),
            (
                EventKind::AgentCreated,
                payload::agent_created("a", "researcher"),
                &["agent_id", "name"],
            ),
            (
                EventKind::AgentStarted,
                payload::agent_started("a", "ex-1"),
                &["agent_id", "execution_id"],
            ),
            (
                EventKind::AgentPaused,
                payload::agent_paused("a", "cp-1"),
                &["agent_id", "checkpoint_id"],
            ),
            (
                EventKind::AgentResumed,
                payload::agent_resumed("a", Some("cp-1")),
                &["agent_id", "checkpoint_id"],
            ),
            (
                EventKind::AgentThinking,
                payload::agent_thinking("a", 3, "working"),
                &["agent_id", "step", "text"],
            ),
            (
                EventKind::AgentWaiting,
                payload::agent_waiting("a", "pr-1", "filesystem:write", "write x"),
                &["agent_id", "permission_request_id", "resource", "action"],
            ),
            (
                EventKind::AgentSleeping,
                payload::agent_sleeping("a", "2026-08-16T10:00:00Z"),
                &["agent_id", "wake_at"],
            ),
            (
                EventKind::AgentCompleted,
                payload::agent_completed("a", "ex-1", "done"),
                &["agent_id", "execution_id", "final_text"],
            ),
            (
                EventKind::AgentFailed,
                payload::agent_failed("a", "ex-1", &err),
                &["agent_id", "execution_id", "error"],
            ),
            (
                EventKind::AgentTerminated,
                payload::agent_terminated("a"),
                &["agent_id"],
            ),
            (
                EventKind::ExecutionStarted,
                payload::execution_started("ex-1", "a"),
                &["execution_id", "agent_id"],
            ),
            (
                EventKind::ExecutionCompleted,
                payload::execution_completed("ex-1", 7, 3),
                &["execution_id", "steps", "checkpoints"],
            ),
            (
                EventKind::ExecutionFailed,
                payload::execution_failed("ex-1", &err),
                &["execution_id", "error"],
            ),
            (
                EventKind::ExecutionRestored,
                payload::execution_restored("ex-1", "cp-1"),
                &["execution_id", "checkpoint_id"],
            ),
            (
                EventKind::ModelRequested,
                payload::model_requested("openai", "gpt-4o-mini", 1),
                &["provider", "model", "step"],
            ),
            (
                EventKind::ModelCompleted,
                payload::model_completed("openai", "gpt-4o-mini", ModelOutcomeKind::Finish, 812),
                &["provider", "model", "kind", "latency_ms"],
            ),
            (
                EventKind::ModelFailed,
                payload::model_failed(&err),
                &["error"],
            ),
            (
                EventKind::ToolRequested,
                payload::tool_requested("call-1", "filesystem", Some(&json!({ "path": "/x" }))),
                &["tool_call_id", "tool_name", "args"],
            ),
            (
                EventKind::ToolStarted,
                payload::tool_started("call-1", "filesystem"),
                &["tool_call_id", "tool_name"],
            ),
            (
                EventKind::ToolCompleted,
                payload::tool_completed("call-1", "filesystem", 42, 128),
                &["tool_call_id", "tool_name", "latency_ms", "result_size"],
            ),
            (
                EventKind::ToolFailed,
                payload::tool_failed("call-1", "filesystem", &err),
                &["tool_call_id", "tool_name", "error"],
            ),
            (
                EventKind::CheckpointCreated,
                payload::checkpoint_created("cp-1", "ex-1", 4),
                &["checkpoint_id", "execution_id", "seq"],
            ),
            (
                EventKind::CheckpointRestored,
                payload::checkpoint_restored("cp-1", "ex-1"),
                &["checkpoint_id", "execution_id"],
            ),
            (
                EventKind::CheckpointFailed,
                payload::checkpoint_failed(&err),
                &["error"],
            ),
            (
                EventKind::PermissionAsked,
                payload::permission_asked("pr-1", "network:host", "GET api.example.com"),
                &["permission_request_id", "resource", "action"],
            ),
            (
                EventKind::PermissionGranted,
                payload::permission_granted("pr-1", "network:host"),
                &["permission_request_id", "resource"],
            ),
            (
                EventKind::PermissionDenied,
                payload::permission_denied("pr-1", "network:host", "policy"),
                &["permission_request_id", "resource", "reason"],
            ),
            (
                EventKind::SchedulerRecoveredAgent,
                payload::scheduler_recovered_agent("a", "ex-1", "cp-1"),
                &["agent_id", "execution_id", "checkpoint_id"],
            ),
            (
                EventKind::SchedulerRunDue,
                payload::scheduler_run_due("a", "2026-08-15T00:00:00Z"),
                &["agent_id", "scheduled_for"],
            ),
            (
                EventKind::SchedulerBackoff,
                payload::scheduler_backoff("a", 3, "2026-08-15T00:00:00Z"),
                &["agent_id", "consecutive_failures", "next_run_at"],
            ),
        ];

        assert_eq!(
            checks.len(),
            CATALOG.len(),
            "every catalog kind must have a builder check"
        );
        for (kind, p, keys) in &checks {
            for key in *keys {
                assert!(
                    p.get(*key).is_some(),
                    "{} payload missing required key {key}: {p}",
                    kind.as_str()
                );
            }
        }
    }

    #[test]
    fn tool_requested_omits_args_by_default() {
        let with_args = payload::tool_requested("call-1", "http", Some(&json!({ "url": "x" })));
        assert_eq!(with_args["args"]["url"], "x");
        let without_args = payload::tool_requested("call-1", "http", None);
        assert!(
            without_args.get("args").is_none(),
            "args must be omitted unless log_tool_args is enabled: {without_args}"
        );
    }

    #[test]
    fn agent_resumed_payload_keeps_null_key() {
        let p = payload::agent_resumed("a", None);
        assert_eq!(p["agent_id"], "a");
        assert!(p.get("checkpoint_id").is_some(), "key must be present");
        assert!(p["checkpoint_id"].is_null());
    }

    #[test]
    fn error_payload_has_spec_shape() {
        let err = KernError::new(ErrorCode::PermissionDenied, "not allowed")
            .with_detail(json!({ "resource": "filesystem:write" }));
        let p = payload::error_payload(&err);
        assert_eq!(p["code"], "PERMISSION_DENIED");
        assert_eq!(p["message"], "not allowed");
        assert_eq!(p["detail"]["resource"], "filesystem:write");

        let plain = payload::error_payload(&KernError::new(ErrorCode::Internal, "boom"));
        assert!(
            plain.get("detail").is_none(),
            "detail must be omitted: {plain}"
        );
    }

    // ------------------------------------------------------------------
    // Bus behavior
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn emit_persists_and_broadcasts_in_order() {
        let (_dir, _store, bus) = test_bus().await;
        let mut sub = bus.subscribe();
        for i in 0..5 {
            bus.emit(
                EventKind::AgentThinking,
                Some("agent-1"),
                None,
                payload::agent_thinking("agent-1", i, "working"),
            )
            .await
            .unwrap();
        }

        // Live delivery, in order.
        for i in 0..5 {
            let event = sub.recv().await.unwrap();
            assert_eq!(event.kind, "agent.thinking");
            assert_eq!(event.agent_id.as_deref(), Some("agent-1"));
            assert_eq!(event.payload["step"], json!(i));
        }

        // Durable, monotonic seqs.
        let replayed = bus.replay(0, 100).await.unwrap();
        assert_eq!(replayed.len(), 5);
        assert!(replayed.windows(2).all(|w| w[0].seq < w[1].seq));
    }

    #[tokio::test]
    async fn replay_from_arbitrary_seq_and_limits() {
        let (_dir, _store, bus) = test_bus().await;
        for _ in 0..5 {
            bus.emit(
                EventKind::RuntimeShuttingDown,
                None,
                None,
                payload::runtime_shutting_down(),
            )
            .await
            .unwrap();
        }
        let after = bus.replay(2, 100).await.unwrap();
        assert_eq!(
            after.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![3, 4, 5]
        );

        let limited = bus.replay(0, 2).await.unwrap();
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].seq, 1);
    }

    #[tokio::test]
    async fn replay_for_agent_filters() {
        let (_dir, _store, bus) = test_bus().await;
        for i in 0..3 {
            bus.emit(
                EventKind::AgentThinking,
                Some("a"),
                None,
                payload::agent_thinking("a", i, "t"),
            )
            .await
            .unwrap();
        }
        bus.emit(
            EventKind::AgentThinking,
            Some("b"),
            None,
            payload::agent_thinking("b", 0, "t"),
        )
        .await
        .unwrap();

        let mine = bus.replay_for_agent("a", 0, 100).await.unwrap();
        assert_eq!(mine.len(), 3);
        assert!(mine.iter().all(|e| e.agent_id.as_deref() == Some("a")));
        assert!(mine.iter().all(|e| e.kind == "agent.thinking"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_emits_have_monotonic_seqs() {
        let (_dir, store, bus) = test_bus().await;
        let mut handles = Vec::new();
        for w in 0..4 {
            let bus = bus.clone();
            handles.push(tokio::spawn(async move {
                for i in 0..25 {
                    bus.emit(
                        EventKind::AgentThinking,
                        Some("agent-x"),
                        None,
                        payload::agent_thinking("agent-x", i, &format!("w{w}-{i}")),
                    )
                    .await
                    .unwrap();
                }
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }

        let all = store.events_after(0, 10_000).unwrap();
        assert_eq!(all.len(), 100);
        assert!(
            all.windows(2).all(|w| w[0].seq < w[1].seq),
            "seqs must be strictly increasing across writers"
        );
    }

    #[tokio::test]
    async fn slow_subscriber_lags_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        // Bus capacity 2: emit enough to overflow the subscriber's buffer.
        let bus = EventBus::with_capacity(Arc::clone(&store), 2);
        let mut sub = bus.subscribe();
        for i in 0..16 {
            bus.emit(
                EventKind::AgentThinking,
                Some("a"),
                None,
                payload::agent_thinking("a", i, "t"),
            )
            .await
            .unwrap();
        }
        match sub.recv().await {
            Err(SubscriberError::Lagged { skipped }) => {
                assert!(skipped >= 1, "expected at least one dropped event");
            }
            other => panic!("expected Lagged, got {other:?}"),
        }
        // The store still has everything: replay is the recovery path.
        assert_eq!(bus.replay(0, 100).await.unwrap().len(), 16);
    }

    #[tokio::test]
    async fn subscriber_reports_closed_when_bus_dropped() {
        let (_tx, rx) = broadcast::channel::<Event>(8);
        drop(_tx);
        let mut sub = Subscriber { rx };
        assert!(matches!(sub.recv().await, Err(SubscriberError::Closed)));
    }

    #[tokio::test]
    async fn per_agent_counts_and_latest_seq() {
        let (_dir, store, bus) = test_bus().await;
        assert_eq!(bus.latest_seq().await.unwrap(), 0);

        bus.emit(
            EventKind::AgentThinking,
            Some("a"),
            None,
            payload::agent_thinking("a", 0, "t"),
        )
        .await
        .unwrap();
        bus.emit(
            EventKind::AgentThinking,
            Some("a"),
            None,
            payload::agent_thinking("a", 1, "t"),
        )
        .await
        .unwrap();
        bus.emit(
            EventKind::AgentThinking,
            Some("b"),
            None,
            payload::agent_thinking("b", 0, "t"),
        )
        .await
        .unwrap();
        bus.emit(
            EventKind::RuntimeShuttingDown,
            None,
            None,
            payload::runtime_shutting_down(),
        )
        .await
        .unwrap();

        assert_eq!(store.event_count_for_agent("a").unwrap(), 2);
        assert_eq!(store.event_count_for_agent("b").unwrap(), 1);
        assert_eq!(store.event_count_for_agent("nope").unwrap(), 0);
        assert_eq!(bus.latest_seq().await.unwrap(), 4);
    }

    #[test]
    fn milestone_warning_logic() {
        assert_eq!(next_milestone(99, 0, 100), None);
        assert_eq!(next_milestone(100, 0, 100), Some(100));
        assert_eq!(next_milestone(150, 0, 100), Some(100));
        assert_eq!(next_milestone(250, 100, 100), Some(200));
        assert_eq!(next_milestone(250, 200, 100), None);
        assert_eq!(next_milestone(300, 200, 100), Some(300));
    }
}
