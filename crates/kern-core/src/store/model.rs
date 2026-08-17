//! Domain model records persisted by the `Store` (SPEC.md §4).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::new_id;
use crate::version::CHECKPOINT_FORMAT_VERSION;

/// Agent lifecycle state (SPEC.md §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LifecycleState {
    Created,
    Starting,
    Running,
    Paused,
    Waiting,
    /// Parked on a durable sleep: the runner is unloaded, the execution's
    /// `wake_at` says when the scheduler must wake it.
    Sleeping,
    Recovering,
    Completed,
    Failed,
    Terminated,
}

impl LifecycleState {
    pub fn as_str(self) -> &'static str {
        match self {
            LifecycleState::Created => "created",
            LifecycleState::Starting => "starting",
            LifecycleState::Running => "running",
            LifecycleState::Paused => "paused",
            LifecycleState::Waiting => "waiting",
            LifecycleState::Sleeping => "sleeping",
            LifecycleState::Recovering => "recovering",
            LifecycleState::Completed => "completed",
            LifecycleState::Failed => "failed",
            LifecycleState::Terminated => "terminated",
        }
    }
}

impl std::str::FromStr for LifecycleState {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "created" => Ok(LifecycleState::Created),
            "starting" => Ok(LifecycleState::Starting),
            "running" => Ok(LifecycleState::Running),
            "paused" => Ok(LifecycleState::Paused),
            "waiting" => Ok(LifecycleState::Waiting),
            "sleeping" => Ok(LifecycleState::Sleeping),
            "recovering" => Ok(LifecycleState::Recovering),
            "completed" => Ok(LifecycleState::Completed),
            "failed" => Ok(LifecycleState::Failed),
            "terminated" => Ok(LifecycleState::Terminated),
            _ => Err(format!("invalid lifecycle state: {s}")),
        }
    }
}

/// Execution status (SPEC.md §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Interrupted,
}

impl ExecutionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ExecutionStatus::Pending => "pending",
            ExecutionStatus::Running => "running",
            ExecutionStatus::Completed => "completed",
            ExecutionStatus::Failed => "failed",
            ExecutionStatus::Interrupted => "interrupted",
        }
    }
}

impl std::str::FromStr for ExecutionStatus {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "pending" => Ok(ExecutionStatus::Pending),
            "running" => Ok(ExecutionStatus::Running),
            "completed" => Ok(ExecutionStatus::Completed),
            "failed" => Ok(ExecutionStatus::Failed),
            "interrupted" => Ok(ExecutionStatus::Interrupted),
            _ => Err(format!("invalid execution status: {s}")),
        }
    }
}

/// Tool call status (SPEC.md §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolCallStatus {
    Requested,
    Running,
    Completed,
    Failed,
}

impl ToolCallStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolCallStatus::Requested => "requested",
            ToolCallStatus::Running => "running",
            ToolCallStatus::Completed => "completed",
            ToolCallStatus::Failed => "failed",
        }
    }
}

impl std::str::FromStr for ToolCallStatus {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "requested" => Ok(ToolCallStatus::Requested),
            "running" => Ok(ToolCallStatus::Running),
            "completed" => Ok(ToolCallStatus::Completed),
            "failed" => Ok(ToolCallStatus::Failed),
            _ => Err(format!("invalid tool call status: {s}")),
        }
    }
}

/// Permission request status (SPEC.md §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionStatus {
    Pending,
    Granted,
    Denied,
    Expired,
}

impl PermissionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionStatus::Pending => "pending",
            PermissionStatus::Granted => "granted",
            PermissionStatus::Denied => "denied",
            PermissionStatus::Expired => "expired",
        }
    }
}

impl std::str::FromStr for PermissionStatus {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "pending" => Ok(PermissionStatus::Pending),
            "granted" => Ok(PermissionStatus::Granted),
            "denied" => Ok(PermissionStatus::Denied),
            "expired" => Ok(PermissionStatus::Expired),
            _ => Err(format!("invalid permission status: {s}")),
        }
    }
}

/// A named, validated agent configuration plus its durable lifecycle state (SPEC.md §4.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub spec_version: u32,
    pub config: Value,
    pub state: LifecycleState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_error: Option<String>,
    pub auto_recover: bool,
    pub next_run_at: Option<DateTime<Utc>>,
}

impl Agent {
    pub fn new(name: impl Into<String>, config: Value, state: LifecycleState) -> Self {
        let now = Utc::now();
        Self {
            id: new_id(),
            name: name.into(),
            spec_version: 1,
            config,
            state,
            created_at: now,
            updated_at: now,
            last_error: None,
            auto_recover: true,
            next_run_at: None,
        }
    }
}

/// One run of an agent from start to a terminal state (SPEC.md §4.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execution {
    pub id: String,
    pub agent_id: String,
    pub status: ExecutionStatus,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub latest_checkpoint_id: Option<String>,
    /// The pre-start task input (schema v3). Persisted so a crash before the
    /// first checkpoint still resumes with the real task, not a default.
    pub input: Option<String>,
    /// Scheduled wake time for a `sleeping` execution (schema v4).
    /// Set when the runner parks for a durable sleep; cleared on wake. A
    /// sleeping agent whose wake time is in the past wakes immediately on
    /// daemon startup (missed wake collapses, like the scheduler's
    /// fire-and-advance).
    pub wake_at: Option<DateTime<Utc>>,
}

impl Execution {
    pub fn new(agent_id: impl Into<String>, status: ExecutionStatus) -> Self {
        Self {
            id: new_id(),
            agent_id: agent_id.into(),
            status,
            started_at: None,
            finished_at: None,
            latest_checkpoint_id: None,
            input: None,
            wake_at: None,
        }
    }
}

/// Immutable, sequentially numbered record of a runtime action (SPEC.md §6).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub seq: i64,
    pub ts: DateTime<Utc>,
    pub kind: String,
    pub agent_id: Option<String>,
    pub execution_id: Option<String>,
    pub payload: Value,
}

/// Durable record of a requested tool invocation (SPEC.md §4.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub agent_id: String,
    pub execution_id: String,
    pub tool_name: String,
    pub args: Value,
    pub status: ToolCallStatus,
    pub result: Option<Value>,
    pub error: Option<Value>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        agent_id: impl Into<String>,
        execution_id: impl Into<String>,
        tool_name: impl Into<String>,
        args: Value,
    ) -> Self {
        Self {
            id: id.into(),
            agent_id: agent_id.into(),
            execution_id: execution_id.into(),
            tool_name: tool_name.into(),
            args,
            status: ToolCallStatus::Requested,
            result: None,
            error: None,
            started_at: None,
            finished_at: None,
        }
    }
}

/// Durable, versioned snapshot of a session (SPEC.md §7).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    pub agent_id: String,
    pub execution_id: String,
    pub parent_id: Option<String>,
    pub format_version: u32,
    pub seq: i64,
    pub payload: Value,
    pub created_at: DateTime<Utc>,
}

impl Checkpoint {
    pub fn new(
        agent_id: impl Into<String>,
        execution_id: impl Into<String>,
        seq: i64,
        payload: Value,
    ) -> Self {
        Self {
            id: new_id(),
            agent_id: agent_id.into(),
            execution_id: execution_id.into(),
            parent_id: None,
            format_version: CHECKPOINT_FORMAT_VERSION,
            seq,
            payload,
            created_at: Utc::now(),
        }
    }
}

/// Durable, agent-scoped key/value memory (SPEC.md §5 `memory` table).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub agent_id: String,
    pub key: String,
    pub value: Value,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Execution-scoped durable variable (SPEC.md §5 `state_variables` table).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateVariable {
    pub agent_id: String,
    pub execution_id: String,
    pub key: String,
    pub value: Value,
    pub updated_at: DateTime<Utc>,
}

/// An execution-row update applied atomically with a lifecycle transition
/// (`SPEC.md §3.2`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionUpdate {
    pub id: String,
    pub status: ExecutionStatus,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// An event appended atomically with a lifecycle transition. The envelope's
/// `agent_id` is always the transitioning agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub kind: &'static str,
    pub execution_id: Option<String>,
    pub payload: serde_json::Value,
}

/// A lifecycle transition persisted atomically (`SPEC.md §3.2`): the agent's
/// state change (guarded by the expected current state), an optional execution
/// update, and zero or more events — all in one transaction.
#[derive(Debug, Clone)]
pub struct Transition {
    pub agent_id: String,
    pub expected_state: LifecycleState,
    pub new_state: LifecycleState,
    pub last_error: Option<String>,
    pub execution: Option<ExecutionUpdate>,
    pub events: Vec<EventRecord>,
}

/// A pending grant/deny decision surfaced when policy evaluates to `ask` (SPEC.md §4.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub id: String,
    pub agent_id: String,
    pub tool_call_id: Option<String>,
    pub resource: String,
    pub action: String,
    pub status: PermissionStatus,
    pub requested_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
    /// Approval deadline. `None` only on
    /// pre-v2 rows; fresh requests always carry one.
    pub expires_at: Option<DateTime<Utc>>,
}

impl PermissionRequest {
    /// The operator's decision window has closed (or was never opened).
    /// Pending requests past this instant are expired by the engine's poll.
    pub fn is_overdue(&self) -> bool {
        self.expires_at.is_some_and(|e| e <= Utc::now())
    }
}
