//! Checkpoint manager (ARCHITECTURE.md §7, SPEC.md §7) — the durable snapshot
//! layer of the recovery story.
//!
//! A checkpoint captures the runner's serializable session plus the in-flight
//! batch (`pending_tool_calls`, SPEC §7) at a consistent point:
//!
//! - **pre-batch** (SPEC §8.1 6a): all calls of a batch recorded `requested`,
//!   nothing executed yet — the recovery re-drive source;
//! - **post-batch** (6d): every result fed to the model, session consistent;
//! - **waiting transition** (6b): the ask batch is parked behind pending
//!   permission requests;
//! - **interval**: `checkpoint_interval` elapsed between turns;
//! - **final** (step 4): on completion, so `execution.completed` always has a
//!   checkpoint behind it.
//!
//! Atomicity (SPEC §7): the checkpoint row, the execution's
//! `latest_checkpoint_id` link, the `checkpoint.created` event, and the
//! retention prune commit in ONE transaction
//! (`Store::create_checkpoint_tx`); live broadcast of the event happens after
//! and is best-effort. A crash between create and broadcast loses nothing —
//! the event is already durable.
//!
//! Restore validates `format_version` (reject > current, never silently
//! upgrade), reconstructs the session, and emits `checkpoint.restored` +
//! `execution.restored` before the runner continues.

use std::sync::Arc;

use chrono::Utc;
use kern_model::Message;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{ErrorCode, KernError, Result};
use crate::event::{payload, EventBus, EventKind};
use crate::store::{new_id, Checkpoint, Store};
use crate::version::CHECKPOINT_FORMAT_VERSION;

/// The runner state a checkpoint must preserve to resume an execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionState {
    pub messages: Vec<Message>,
    pub history_trimmed: bool,
    pub steps: u64,
    pub final_text: String,
    /// Checkpoints created so far in this execution (the `seq` of the next
    /// one is `checkpoints + 1`; the completed event reports the count).
    pub checkpoints: u64,
    /// Fresh tool calls issued so far in this execution (the execution budget;
    /// `#[serde(default)]` so pre-v1 checkpoints resume with a fresh count).
    #[serde(default)]
    pub tool_calls: u64,
}

/// A requested-but-undecided tool call captured pre-batch (SPEC §7
/// `pending_tool_calls`) — the recovery re-drive source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

impl PendingCall {
    pub fn new(id: impl Into<String>, name: impl Into<String>, args: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            args,
        }
    }
}

/// Everything a restore needs to resume a run.
#[derive(Debug)]
pub struct RestoredState {
    pub state: SessionState,
    pub pending: Vec<PendingCall>,
    pub runtime_meta: Value,
    pub checkpoint_id: String,
}

/// The §7 payload shape (the row's columns carry id/seq/format/created_at;
/// the payload repeats the identity fields so a payload is self-describing).
#[derive(Debug, Serialize, Deserialize)]
struct CheckpointPayload {
    format_version: u32,
    checkpoint_id: String,
    agent_id: String,
    execution_id: String,
    parent_checkpoint_id: Option<String>,
    created_at: String,
    lifecycle_state: String,
    step: u64,
    messages: Vec<Message>,
    history_trimmed: bool,
    pending_tool_calls: Vec<PendingCall>,
    variables: Value,
    memory_refs: Vec<String>,
    runtime_meta: Value,
    /// Fresh tool calls issued so far in this execution. Old
    /// checkpoints (pre-v1 format) deserialize with the serde default 0.
    #[serde(default)]
    tool_calls: u64,
}

/// Everything `CheckpointManager::create` needs — bundled so the method stays
/// under the clippy argument cap and call sites read as one unit.
#[derive(Debug)]
pub struct CheckpointRequest<'a> {
    pub agent_id: &'a str,
    pub execution_id: &'a str,
    pub lifecycle_state: &'a str,
    pub state: &'a SessionState,
    pub pending: &'a [PendingCall],
    pub runtime_meta: &'a Value,
    pub retention: u32,
}

#[derive(Clone)]
pub struct CheckpointManager {
    store: Arc<Store>,
    bus: EventBus,
}

impl CheckpointManager {
    pub fn new(store: Arc<Store>, bus: EventBus) -> Self {
        Self { store, bus }
    }

    /// Create a checkpoint atomically (row + execution link + event + prune).
    /// `req.pending` carries the in-flight batch for pre-batch checkpoints
    /// and is empty otherwise. Returns the created checkpoint.
    pub async fn create(&self, req: &CheckpointRequest<'_>) -> Result<Checkpoint> {
        let CheckpointRequest {
            agent_id,
            execution_id,
            lifecycle_state,
            state,
            pending,
            runtime_meta,
            retention,
        } = req;
        let store = Arc::clone(&self.store);
        let agent_owned = agent_id.to_string();
        let execution_owned = execution_id.to_string();
        let parent = store_blocking(store, move |s| {
            s.latest_checkpoint_for_execution(&agent_owned, &execution_owned)
        })
        .await?
        .map(|c| c.id);

        let seq = state.checkpoints as i64 + 1;
        let payload = CheckpointPayload {
            format_version: CHECKPOINT_FORMAT_VERSION,
            checkpoint_id: new_id(),
            agent_id: agent_id.to_string(),
            execution_id: execution_id.to_string(),
            parent_checkpoint_id: parent.clone(),
            created_at: Utc::now().to_rfc3339(),
            lifecycle_state: lifecycle_state.to_string(),
            step: state.steps,
            messages: state.messages.clone(),
            history_trimmed: state.history_trimmed,
            pending_tool_calls: pending.to_vec(),
            variables: json!({}),
            memory_refs: Vec::new(),
            runtime_meta: (*runtime_meta).clone(),
            tool_calls: state.tool_calls,
        };
        let payload_value = serde_json::to_value(&payload)
            .map_err(|e| KernError::internal(format!("serialize checkpoint payload: {e}")))?;

        let checkpoint = Checkpoint {
            id: payload.checkpoint_id.clone(),
            agent_id: agent_id.to_string(),
            execution_id: execution_id.to_string(),
            parent_id: parent,
            format_version: CHECKPOINT_FORMAT_VERSION,
            seq,
            payload: payload_value,
            created_at: Utc::now(),
        };

        let store = Arc::clone(&self.store);
        let event_payload = payload::checkpoint_created(&checkpoint.id, execution_id, seq);
        let checkpoint_tx = checkpoint.clone();
        let retention = *retention;
        let persisted = store_blocking(store, move |s| {
            s.create_checkpoint_tx(
                &checkpoint_tx,
                EventKind::CheckpointCreated.as_str(),
                event_payload,
                retention,
            )
        })
        .await?;
        self.bus.publish(std::slice::from_ref(&persisted));
        Ok(checkpoint)
    }

    /// Restore the newest checkpoint of an execution. Rejects formats newer
    /// than the current one (`CHECKPOINT_FORMAT_UNSUPPORTED`) and corrupted
    /// payloads (`CHECKPOINT_CORRUPT`) — never silently upgrades or guesses.
    pub async fn restore(&self, agent_id: &str, execution_id: &str) -> Result<RestoredState> {
        let store = Arc::clone(&self.store);
        let agent_owned = agent_id.to_string();
        let execution_owned = execution_id.to_string();
        let checkpoint = store_blocking(store, move |s| {
            s.latest_checkpoint_for_execution(&agent_owned, &execution_owned)
        })
        .await?
        .ok_or_else(|| {
            KernError::new(
                ErrorCode::CheckpointNotFound,
                format!(
                    "no checkpoint for execution {execution_id} of agent {agent_id} to restore"
                ),
            )
        })?;

        if checkpoint.format_version > CHECKPOINT_FORMAT_VERSION {
            return Err(KernError::new(
                ErrorCode::CheckpointFormatUnsupported,
                format!(
                    "checkpoint {} uses format v{}, this runtime supports up to v{}",
                    checkpoint.id, checkpoint.format_version, CHECKPOINT_FORMAT_VERSION
                ),
            ));
        }

        let parsed: CheckpointPayload = serde_json::from_value(checkpoint.payload.clone())
            .map_err(|e| {
                KernError::new(
                    ErrorCode::CheckpointCorrupt,
                    format!("checkpoint {} payload is corrupt: {e}", checkpoint.id),
                )
            })?;

        self.bus
            .emit(
                EventKind::CheckpointRestored,
                Some(agent_id),
                Some(execution_id),
                payload::checkpoint_restored(&checkpoint.id, execution_id),
            )
            .await?;
        self.bus
            .emit(
                EventKind::ExecutionRestored,
                Some(agent_id),
                Some(execution_id),
                payload::execution_restored(execution_id, &checkpoint.id),
            )
            .await?;

        Ok(RestoredState {
            state: SessionState {
                messages: parsed.messages,
                history_trimmed: parsed.history_trimmed,
                steps: parsed.step,
                final_text: String::new(),
                checkpoints: checkpoint.seq as u64,
                tool_calls: parsed.tool_calls,
            },
            pending: parsed.pending_tool_calls,
            runtime_meta: parsed.runtime_meta,
            checkpoint_id: checkpoint.id,
        })
    }
}

/// Run a blocking store call off the async runtime (same discipline as the
/// engine and event bus).
async fn store_blocking<T, F>(store: Arc<Store>, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Store) -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || f(&store))
        .await
        .map_err(|e| KernError::internal(format!("store task join failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Execution, ExecutionStatus, LifecycleState, ToolCall, ToolCallStatus};

    fn env() -> (tempfile::TempDir, Arc<Store>, EventBus, CheckpointManager) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let bus = EventBus::new(Arc::clone(&store));
        let manager = CheckpointManager::new(Arc::clone(&store), bus.clone());
        (dir, store, bus, manager)
    }

    fn session(step: u64) -> SessionState {
        SessionState {
            messages: vec![
                Message::user("the task"),
                Message::assistant("thinking out loud"),
            ],
            history_trimmed: false,
            steps: step,
            final_text: String::new(),
            checkpoints: 0,
            tool_calls: 0,
        }
    }

    fn seed_execution(store: &Store, agent_id: &str) -> String {
        let execution = Execution::new(agent_id, ExecutionStatus::Running);
        store.create_execution(&execution).unwrap();
        execution.id.clone()
    }

    fn create_running_agent(store: &Store, name: &str) -> String {
        let agent = crate::store::Agent::new(name, json!({}), LifecycleState::Running);
        store.create_agent(&agent).unwrap();
        agent.id.clone()
    }

    #[tokio::test]
    async fn checkpoint_roundtrips_the_session() {
        let (_dir, store, _bus, manager) = env();
        let agent_id = create_running_agent(&store, "cp");
        let execution_id = seed_execution(&store, &agent_id);

        let state = session(7);
        let created = manager
            .create(&CheckpointRequest {
                agent_id: &agent_id,
                execution_id: &execution_id,
                lifecycle_state: "running",
                state: &state,
                pending: &[PendingCall::new("c1", "noop", json!({}))],
                runtime_meta: &json!({ "provider": "mock", "model": "test" }),
                retention: 10,
            })
            .await
            .unwrap();
        assert_eq!(created.seq, 1);

        let restored = manager.restore(&agent_id, &execution_id).await.unwrap();
        assert_eq!(restored.state.steps, 7);
        assert_eq!(restored.state.messages.len(), 2);
        assert_eq!(restored.state.messages[0].content, "the task");
        assert_eq!(restored.pending.len(), 1);
        assert_eq!(restored.pending[0].id, "c1");
        assert_eq!(restored.runtime_meta["provider"], "mock");
        assert_eq!(restored.checkpoint_id, created.id);

        // The event committed with the checkpoint (SPEC §7: same transaction).
        let kinds = store.events_after(0, 100).unwrap();
        assert_eq!(
            kinds
                .iter()
                .filter(|e| e.kind == "checkpoint.created")
                .count(),
            1
        );
        let created_event = kinds
            .iter()
            .find(|e| e.kind == "checkpoint.created")
            .unwrap();
        assert_eq!(created_event.payload["checkpoint_id"], created.id);
        // Restore emitted checkpoint.restored + execution.restored.
        assert_eq!(
            kinds
                .iter()
                .filter(|e| e.kind == "checkpoint.restored")
                .count(),
            1
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|e| e.kind == "execution.restored")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn restore_rejects_future_versions() {
        let (_dir, store, _bus, manager) = env();
        let agent_id = create_running_agent(&store, "ver");
        let execution_id = seed_execution(&store, &agent_id);

        let mut future = Checkpoint::new(
            agent_id.clone(),
            execution_id.clone(),
            1,
            json!({"format_version": 99}),
        );
        future.format_version = 99;
        store.create_checkpoint(&future).unwrap();

        let err = manager.restore(&agent_id, &execution_id).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::CheckpointFormatUnsupported);
    }

    #[tokio::test]
    async fn retention_prunes_oldest_keeps_newest() {
        let (_dir, store, _bus, manager) = env();
        let agent_id = create_running_agent(&store, "ret");
        let execution_id = seed_execution(&store, &agent_id);

        for i in 0..5 {
            let mut state = session(i);
            state.checkpoints = i;
            manager
                .create(&CheckpointRequest {
                    agent_id: &agent_id,
                    execution_id: &execution_id,
                    lifecycle_state: "running",
                    state: &state,
                    pending: &[],
                    runtime_meta: &json!({}),
                    retention: 2,
                })
                .await
                .unwrap();
        }

        let kept = store.list_checkpoints(&agent_id, 100).unwrap();
        assert_eq!(kept.len(), 2, "retention keeps the newest 2");
        assert_eq!(kept[0].seq, 5, "the newest is never pruned");
        assert_eq!(kept[1].seq, 4);
        let execution = store
            .list_executions_for_agent(&agent_id)
            .unwrap()
            .remove(0);
        assert_eq!(
            execution.latest_checkpoint_id.as_deref(),
            Some(kept[0].id.as_str())
        );
    }

    #[tokio::test]
    async fn failed_create_rolls_back_completely() {
        let (_dir, store, _bus, manager) = env();
        let agent_id = create_running_agent(&store, "atomic");
        let execution_id = seed_execution(&store, &agent_id);
        let original = manager
            .create(&CheckpointRequest {
                agent_id: &agent_id,
                execution_id: &execution_id,
                lifecycle_state: "running",
                state: &session(1),
                pending: &[],
                runtime_meta: &json!({}),
                retention: 10,
            })
            .await
            .unwrap();
        let before = store.events_after(0, 100).unwrap().len();

        // Force a mid-transaction failure: insert the SAME checkpoint id twice
        // (unique constraint) — the duplicate row, its event, and the
        // execution link must all roll back.
        let dup = Checkpoint {
            id: original.id.clone(),
            ..Checkpoint::new(
                agent_id.clone(),
                execution_id.clone(),
                2,
                json!({"format_version": 1}),
            )
        };
        let err = store
            .create_checkpoint_tx(&dup, "checkpoint.created", json!({}), 10)
            .unwrap_err();
        assert!(err.to_string().contains("constraint"));

        // Nothing partial landed: checkpoint list + event stream unchanged,
        // and the execution still links the ORIGINAL checkpoint.
        let kept = store.list_checkpoints(&agent_id, 100).unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(store.events_after(0, 100).unwrap().len(), before);
        let execution = store
            .list_executions_for_agent(&agent_id)
            .unwrap()
            .remove(0);
        assert_eq!(
            execution.latest_checkpoint_id.as_deref(),
            Some(original.id.as_str())
        );
    }

    #[tokio::test]
    async fn interrupted_tool_rows_roundtrip_as_pending_calls() {
        let (_dir, store, _bus, manager) = env();
        let agent_id = create_running_agent(&store, "pend");
        let execution_id = seed_execution(&store, &agent_id);

        // Simulate an interrupted batch: two requested rows, one terminal.
        let batch = vec![
            ToolCall::new("c1", &agent_id, &execution_id, "noop", json!({})),
            ToolCall::new("c2", &agent_id, &execution_id, "noop", json!({})),
        ];
        store.record_tool_calls_batch(&batch).unwrap();
        let mut done = batch[0].clone();
        done.status = ToolCallStatus::Completed;
        done.result = Some(json!({ "ok": true }));
        store.update_tool_call(&done).unwrap();

        let pending = vec![
            PendingCall::new("c1", "noop", json!({})),
            PendingCall::new("c2", "noop", json!({})),
        ];
        manager
            .create(&CheckpointRequest {
                agent_id: &agent_id,
                execution_id: &execution_id,
                lifecycle_state: "running",
                state: &session(2),
                pending: &pending,
                runtime_meta: &json!({}),
                retention: 10,
            })
            .await
            .unwrap();

        let restored = manager.restore(&agent_id, &execution_id).await.unwrap();
        assert_eq!(restored.pending.len(), 2);
    }
}
