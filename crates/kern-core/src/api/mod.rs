//! Local HTTP API (ARCHITECTURE.md §15, SPEC.md §15) — the daemon's
//! programmatic control surface.
//!
//! All routes live under `/api/v1` and speak JSON (`SPEC.md §15.1`). Errors
//! use the §13 shape `{ "code", "message", "detail"? }` mapped onto the right
//! HTTP status. When a bearer token is configured (`KERN_TOKEN` or
//! `$KERN_HOME/token`), every route requires it (401 otherwise) — a local
//! daemon must not expose agent control to anything that can reach the port.
//!
//! The API never touches the database directly through private paths: it is
//! a thin layer over `Store` (reads), `Engine` (lifecycle control), the
//! `EventBus` (replay + SSE), and `RecoveryManager` (resume). Lifecycle
//! semantics follow `SPEC.md §15.3` — idempotent no-ops for already-applied
//! actions, `409 INVALID_TRANSITION` for impossible ones.
//!
//! The SSE endpoint (`/events/stream`) lives in `sse.rs`: replay from an
//! `after=` cursor, then live, with keepalives and shutdown-driven close.

pub mod sse;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use kern_model::gateway::ModelGateway;
use kern_tool::Tool;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::{parse_agent_spec, SandboxMode, ShellRules};
use crate::engine::Engine;
use crate::error::{ErrorCode, KernError};
use crate::event::payload;
use crate::event::EventBus;
use crate::recovery::RecoveryManager;
use crate::store::{Execution, ExecutionStatus, LifecycleState, Store};
use crate::tools::{build_shell_tool, StoreMemoryProvider, HTTP_MAX_RESPONSE_BYTES};
use crate::version::{KERN_VERSION, STORAGE_SCHEMA_VERSION};

/// Replay cap for a single `GET /agents/{id}/events` page.
const EVENTS_PAGE_LIMIT: usize = 10_000;

/// How long the manual-checkpoint handler waits for a running runner to reach
/// a safe point before reporting the checkpoint as queued (202) instead of
/// completed (201). Runners inside a long tool/model call cannot checkpoint
/// until the call returns.
const CHECKPOINT_POLL: Duration = Duration::from_millis(100);
const CHECKPOINT_POLLS: u32 = 100; // ~10 s budget

// ---------------------------------------------------------------------------
// State and router
// ---------------------------------------------------------------------------

/// Everything the API needs from the runtime. Clone is cheap (shared
/// handles) and safe to pass into handlers.
#[derive(Clone)]
pub struct ApiState {
    pub store: Arc<Store>,
    pub engine: Engine,
    pub bus: EventBus,
    pub gateway: Arc<ModelGateway>,
    /// Bearer token; `None` ⇒ the API is open (only when no token is
    /// configured — never the default in production).
    pub token: Option<String>,
    /// Daemon shutdown signal: SSE streams close when it flips so graceful
    /// shutdown can drain connections.
    pub shutdown: tokio::sync::watch::Receiver<bool>,
}

/// Build the `/api/v1` router with auth. `state` is consumed (handlers hold
/// their own clone).
pub fn router(state: ApiState) -> Router {
    let auth = middleware::from_fn_with_state(state.clone(), require_auth);
    Router::new()
        .route("/agents", get(list_agents).post(create_agent))
        .route("/agents/{id}", get(get_agent))
        .route("/agents/{id}/start", post(start_agent))
        .route("/agents/{id}/pause", post(pause_agent))
        .route("/agents/{id}/resume", post(resume_agent))
        .route("/agents/{id}/terminate", post(terminate_agent))
        .route("/agents/{id}/checkpoint", post(checkpoint_agent))
        .route("/agents/{id}/checkpoints", get(list_checkpoints))
        .route(
            "/agents/{id}/checkpoints/{cid}/restore",
            post(restore_checkpoint),
        )
        .route("/agents/{id}/events", get(agent_events))
        .route("/agents/{id}/executions", get(agent_executions))
        .route("/events/stream", get(sse::stream_events))
        .route("/executions/{id}", get(get_execution))
        .route("/executions/{id}/transcript", get(transcript))
        .route("/tools", get(tools))
        .route("/models", get(models))
        .route("/permissions/pending", get(pending_permissions))
        .route("/permissions/{id}/grant", post(grant_permission))
        .route("/permissions/{id}/deny", post(deny_permission))
        .route("/health", get(health))
        .route_layer(auth)
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

/// Constant-time token equality (a local bearer token is a secret; a timing
/// difference over the network is not needed to leak it).
fn tokens_equal(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Bearer-token gate for every route. No token configured ⇒ pass-through.
async fn require_auth(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let Some(expected) = &state.token else {
        return Ok(next.run(request).await);
    };
    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match provided {
        Some(token) if tokens_equal(token, expected) => Ok(next.run(request).await),
        _ => Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "a valid bearer token is required (configured via KERN_TOKEN or $KERN_HOME/token)",
            None,
        )),
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A structured API error: HTTP status + §13 `{code, message, detail?}` body.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    body: Value,
}

impl ApiError {
    pub fn new(
        status: StatusCode,
        code: &str,
        message: impl Into<String>,
        detail: Option<Value>,
    ) -> Self {
        let mut body = json!({ "code": code, "message": message.into() });
        if let Some(detail) = detail {
            body["detail"] = detail;
        }
        Self { status, body }
    }

    fn from_kern(err: KernError) -> Self {
        Self::new(
            status_for(err.code()),
            err.code().as_str(),
            err.message,
            err.detail,
        )
    }
}

/// HTTP status for each structured error code (SPEC §13 → §15 mapping).
fn status_for(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::ConfigInvalid | ErrorCode::ToolInvalidArguments => StatusCode::BAD_REQUEST,
        ErrorCode::PermissionDenied | ErrorCode::ModelAuth => StatusCode::FORBIDDEN,
        ErrorCode::AgentNotFound
        | ErrorCode::ExecutionNotFound
        | ErrorCode::CheckpointNotFound
        | ErrorCode::PermissionRequestNotFound => StatusCode::NOT_FOUND,
        ErrorCode::InvalidTransition
        | ErrorCode::ExecutionAlreadyActive
        | ErrorCode::AgentNameTaken
        | ErrorCode::PermissionRequestAlreadyDecided
        | ErrorCode::PermissionRequestExpired => StatusCode::CONFLICT,
        ErrorCode::ModelTimeout
        | ErrorCode::ModelUnavailable
        | ErrorCode::ModelRateLimited
        | ErrorCode::ModelInvalidResponse
        | ErrorCode::ModelBudgetExhausted
        | ErrorCode::ToolTimeout
        | ErrorCode::ToolFailed
        | ErrorCode::ToolUnavailable
        | ErrorCode::StepLimitExceeded
        | ErrorCode::RunDurationExceeded
        | ErrorCode::ToolCallLimitExceeded
        | ErrorCode::RunnerPanic
        | ErrorCode::CheckpointFormatUnsupported
        | ErrorCode::CheckpointCorrupt
        | ErrorCode::SandboxUnavailable
        | ErrorCode::SandboxFailure
        | ErrorCode::StorageMigration => StatusCode::UNPROCESSABLE_ENTITY,
        ErrorCode::StorageCorruption
        | ErrorCode::StorageLocked
        | ErrorCode::StorageFailure
        | ErrorCode::RunnerLost
        | ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

impl From<KernError> for ApiError {
    fn from(err: KernError) -> Self {
        Self::from_kern(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

// ---------------------------------------------------------------------------
// Handlers — agents
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateAgentBody {
    /// Optional; when present MUST match the spec's `name` (the spec is the
    /// authoritative agent definition).
    name: Option<String>,
    spec: Value,
}

/// The wire shape of an agent (`SPEC.md §4.1`): the store row's `state`
/// surfaces as `lifecycle_state` for API consumers.
fn agent_json(agent: &crate::store::Agent) -> Value {
    json!({
        "id": agent.id,
        "name": agent.name,
        "spec_version": agent.spec_version,
        "config": agent.config,
        "lifecycle_state": agent.state.as_str(),
        "created_at": agent.created_at,
        "updated_at": agent.updated_at,
        "last_error": agent.last_error,
        "auto_recover": agent.auto_recover,
        "next_run_at": agent.next_run_at,
    })
}

/// `POST /agents` — validate the spec and create the agent (201).
async fn create_agent(
    State(state): State<ApiState>,
    body: Result<Json<CreateAgentBody>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let body = body.map_err(|rej| {
        // Preserve axum's status (e.g. 413 for an oversized body, which the
        // default 2 MiB body limit enforces) rather than flattening every
        // rejection into 400, which would hide the limit from clients.
        let status = rej.into_response().status();
        let code = if status == StatusCode::PAYLOAD_TOO_LARGE {
            "REQUEST_TOO_LARGE"
        } else {
            "CONFIG_INVALID"
        };
        ApiError::new(
            status,
            code,
            "request body must be JSON { \"spec\": {...} }",
            None,
        )
    })?;
    if !body.spec.is_object() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "CONFIG_INVALID",
            "spec must be a JSON object",
            None,
        ));
    }
    // The spec struct is `deny_unknown_fields`-strict; reusing the YAML
    // parser keeps validation in exactly one place.
    let yaml = serde_yaml::to_string(&body.spec).map_err(|e| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "CONFIG_INVALID",
            format!("spec is not serializable: {e}"),
            None,
        )
    })?;
    let spec = parse_agent_spec(&yaml).map_err(ApiError::from_kern)?;
    if let Some(name) = &body.name {
        if name != &spec.name {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "CONFIG_INVALID",
                format!(
                    "body name {name:?} does not match spec name {:?}",
                    spec.name
                ),
                None,
            ));
        }
    }

    let agent = crate::store::Agent::new(
        spec.name.clone(),
        serde_json::to_value(&spec)
            .map_err(|e| KernError::internal(format!("serialize spec: {e}")))?,
        LifecycleState::Created,
    );
    state
        .store
        .create_agent(&agent)
        .map_err(ApiError::from_kern)?;
    state
        .bus
        .emit(
            crate::event::EventKind::AgentCreated,
            Some(&agent.id),
            None,
            payload::agent_created(&agent.id, &agent.name),
        )
        .await
        .map_err(ApiError::from_kern)?;
    Ok((StatusCode::CREATED, Json(agent_json(&agent))))
}

/// `GET /agents` — all agents.
async fn list_agents(State(state): State<ApiState>) -> Result<Json<Vec<Value>>, ApiError> {
    let agents = state.store.list_agents().map_err(ApiError::from_kern)?;
    Ok(Json(agents.iter().map(agent_json).collect()))
}

/// `GET /agents/{id}` — one agent plus summary counts.
async fn get_agent(
    State(state): State<ApiState>,
    Path(agent_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let agent = state
        .store
        .get_agent(&agent_id)
        .map_err(ApiError::from_kern)?;
    let executions = state
        .store
        .list_executions_for_agent(&agent_id)
        .map_err(ApiError::from_kern)?;
    let checkpoints = state
        .store
        .list_checkpoints(&agent_id, 10_000)
        .map_err(ApiError::from_kern)?;
    let mut view = agent_json(&agent);
    view["execution_count"] = json!(executions.len());
    view["checkpoint_count"] = json!(checkpoints.len());
    Ok(Json(view))
}

/// `POST /agents/{id}/start` — start a new run, returning the execution id
/// (202). Idempotent: a running agent returns its active execution (202).
async fn start_agent(
    State(state): State<ApiState>,
    Path(agent_id): Path<String>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let agent = state
        .store
        .get_agent(&agent_id)
        .map_err(ApiError::from_kern)?;
    if state.engine.is_running(&agent_id) {
        let execution_id = active_execution_id(&state.store, &agent_id)?;
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({ "execution_id": execution_id.unwrap_or_default() })),
        ));
    }
    match agent.state {
        LifecycleState::Created
        | LifecycleState::Completed
        | LifecycleState::Failed
        | LifecycleState::Terminated => {}
        _ => {
            return Err(invalid_transition(&agent_id, agent.state, "start"));
        }
    }
    let execution_id = state
        .engine
        .start_agent(&agent_id, None)
        .await
        .map_err(ApiError::from_kern)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "execution_id": execution_id })),
    ))
}

/// `POST /agents/{id}/pause` — checkpoint + pause at the runner's next safe
/// point (202). Idempotent on `paused`.
async fn pause_agent(
    State(state): State<ApiState>,
    Path(agent_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let agent = state
        .store
        .get_agent(&agent_id)
        .map_err(ApiError::from_kern)?;
    match agent.state {
        LifecycleState::Paused => Ok(StatusCode::ACCEPTED), // idempotent
        LifecycleState::Starting | LifecycleState::Running | LifecycleState::Waiting => {
            if state.engine.request_pause(&agent_id) {
                Ok(StatusCode::ACCEPTED)
            } else {
                Err(invalid_transition(&agent_id, agent.state, "pause"))
            }
        }
        _ => Err(invalid_transition(&agent_id, agent.state, "pause")),
    }
}

/// `POST /agents/{id}/resume` — restore the latest checkpoint and re-spawn
/// the runner (202). Idempotent while already active.
async fn resume_agent(
    State(state): State<ApiState>,
    Path(agent_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let agent = state
        .store
        .get_agent(&agent_id)
        .map_err(ApiError::from_kern)?;
    match agent.state {
        LifecycleState::Starting | LifecycleState::Running | LifecycleState::Waiting => {
            Ok(StatusCode::ACCEPTED) // already active
        }
        LifecycleState::Paused | LifecycleState::Recovering => {
            RecoveryManager::new(state.engine.clone())
                .resume_agent(&agent_id)
                .await
                .map_err(ApiError::from_kern)?;
            Ok(StatusCode::ACCEPTED)
        }
        _ => Err(invalid_transition(&agent_id, agent.state, "resume")),
    }
}

/// `POST /agents/{id}/terminate` — abort the runner and mark the agent
/// `terminated` (202). Terminal states are idempotent no-ops.
async fn terminate_agent(
    State(state): State<ApiState>,
    Path(agent_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let agent = state
        .store
        .get_agent(&agent_id)
        .map_err(ApiError::from_kern)?;
    match agent.state {
        LifecycleState::Completed | LifecycleState::Failed | LifecycleState::Terminated => {
            Ok(StatusCode::ACCEPTED) // already terminal
        }
        LifecycleState::Created => Err(invalid_transition(&agent_id, agent.state, "terminate")),
        _ => {
            state
                .engine
                .terminate_agent(&agent_id)
                .await
                .map_err(ApiError::from_kern)?;
            Ok(StatusCode::ACCEPTED)
        }
    }
}

/// `POST /agents/{id}/checkpoint` — write a checkpoint now.
///
/// A running runner checkpoints at its next safe point (the handler polls
/// briefly and returns 201 once the checkpoint lands; if the runner is stuck
/// in a long call it returns 202 "queued" — the checkpoint lands when the
/// safe point arrives). A paused agent's session IS its latest checkpoint, so
/// a re-stamped snapshot (next seq) is created directly.
async fn checkpoint_agent(
    State(state): State<ApiState>,
    Path(agent_id): Path<String>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let agent = state
        .store
        .get_agent(&agent_id)
        .map_err(ApiError::from_kern)?;
    match agent.state {
        LifecycleState::Paused => {
            let latest = state
                .store
                .latest_checkpoint(&agent_id)
                .map_err(ApiError::from_kern)?
                .ok_or_else(|| {
                    ApiError::new(
                        StatusCode::CONFLICT,
                        "CHECKPOINT_NOT_FOUND",
                        format!("paused agent {agent_id} has no checkpoint to snapshot"),
                        None,
                    )
                })?;
            // A paused session IS its latest checkpoint: re-stamp it with the
            // next seq (same transaction discipline as any checkpoint).
            let cp = crate::store::Checkpoint {
                id: crate::store::new_id(),
                agent_id: latest.agent_id.clone(),
                execution_id: latest.execution_id.clone(),
                parent_id: Some(latest.id.clone()),
                format_version: latest.format_version,
                seq: latest.seq + 1,
                payload: latest.payload.clone(),
                created_at: chrono::Utc::now(),
            };
            let retention = checkpoint_retention(&agent);
            let event = state
                .store
                .create_checkpoint_tx(
                    &cp,
                    crate::event::EventKind::CheckpointCreated.as_str(),
                    payload::checkpoint_created(&cp.id, &cp.execution_id, cp.seq),
                    retention,
                )
                .map_err(ApiError::from_kern)?;
            state.bus.publish(std::slice::from_ref(&event));
            Ok((
                StatusCode::CREATED,
                Json(json!({ "checkpoint_id": cp.id, "seq": cp.seq })),
            ))
        }
        LifecycleState::Starting | LifecycleState::Running | LifecycleState::Waiting => {
            if !state.engine.request_checkpoint(&agent_id) {
                return Err(invalid_transition(&agent_id, agent.state, "checkpoint"));
            }
            let before = state
                .store
                .latest_checkpoint(&agent_id)
                .map_err(ApiError::from_kern)?
                .map(|c| c.seq)
                .unwrap_or(0);
            for _ in 0..CHECKPOINT_POLLS {
                tokio::time::sleep(CHECKPOINT_POLL).await;
                let latest = state
                    .store
                    .latest_checkpoint(&agent_id)
                    .map_err(ApiError::from_kern)?;
                if let Some(cp) = latest {
                    if cp.seq > before {
                        return Ok((
                            StatusCode::CREATED,
                            Json(json!({ "checkpoint_id": cp.id, "seq": cp.seq })),
                        ));
                    }
                }
            }
            tracing::warn!(
                agent_id,
                "manual checkpoint queued: runner did not reach a safe point in time"
            );
            Ok((StatusCode::ACCEPTED, Json(json!({ "status": "queued" }))))
        }
        _ => Err(invalid_transition(&agent_id, agent.state, "checkpoint")),
    }
}

/// `GET /agents/{id}/checkpoints` — checkpoint metadata (payload omitted).
async fn list_checkpoints(
    State(state): State<ApiState>,
    Path(agent_id): Path<String>,
) -> Result<Json<Vec<Value>>, ApiError> {
    state
        .store
        .get_agent(&agent_id)
        .map_err(ApiError::from_kern)?;
    let checkpoints = state
        .store
        .list_checkpoints(&agent_id, 100)
        .map_err(ApiError::from_kern)?;
    let metas = checkpoints
        .into_iter()
        .map(|c| {
            json!({
                "id": c.id,
                "execution_id": c.execution_id,
                "parent_id": c.parent_id,
                "format_version": c.format_version,
                "seq": c.seq,
                "created_at": c.created_at,
            })
        })
        .collect();
    Ok(Json(metas))
}

/// `POST /agents/{id}/checkpoints/{cid}/restore` — make `cid` the execution's
/// resume point. Only meaningful for a paused/recovering agent (the next
/// resume restores it); a running agent's in-memory session cannot be
/// rewound.
async fn restore_checkpoint(
    State(state): State<ApiState>,
    Path((agent_id, checkpoint_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let agent = state
        .store
        .get_agent(&agent_id)
        .map_err(ApiError::from_kern)?;
    if !matches!(
        agent.state,
        LifecycleState::Paused | LifecycleState::Recovering
    ) {
        return Err(invalid_transition(
            &agent_id,
            agent.state,
            "restore checkpoint",
        ));
    }
    let checkpoint = state
        .store
        .get_checkpoint(&checkpoint_id)
        .map_err(ApiError::from_kern)?;
    if checkpoint.agent_id != agent_id {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "CHECKPOINT_NOT_FOUND",
            format!("checkpoint {checkpoint_id} does not belong to agent {agent_id}"),
            None,
        ));
    }
    let mut execution = state
        .store
        .get_execution(&checkpoint.execution_id)
        .map_err(ApiError::from_kern)?;
    execution.latest_checkpoint_id = Some(checkpoint.id);
    state
        .store
        .update_execution(&execution)
        .map_err(ApiError::from_kern)?;
    Ok(StatusCode::ACCEPTED)
}

// ---------------------------------------------------------------------------
// Handlers — events, executions, transcript
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct EventsParams {
    after: Option<i64>,
    limit: Option<usize>,
}

/// `GET /agents/{id}/events?after=&limit=` — durable replay for one agent.
async fn agent_events(
    State(state): State<ApiState>,
    Path(agent_id): Path<String>,
    Query(params): Query<EventsParams>,
) -> Result<Json<Vec<crate::store::Event>>, ApiError> {
    state
        .store
        .get_agent(&agent_id)
        .map_err(ApiError::from_kern)?;
    let after = params.after.unwrap_or(0);
    let limit = params.limit.unwrap_or(1000).min(EVENTS_PAGE_LIMIT);
    let events = state
        .store
        .events_for_agent_after(&agent_id, after, limit)
        .map_err(ApiError::from_kern)?;
    Ok(Json(events))
}

/// `GET /agents/{id}/executions` — execution history.
async fn agent_executions(
    State(state): State<ApiState>,
    Path(agent_id): Path<String>,
) -> Result<Json<Vec<Execution>>, ApiError> {
    state
        .store
        .get_agent(&agent_id)
        .map_err(ApiError::from_kern)?;
    let executions = state
        .store
        .list_executions_for_agent(&agent_id)
        .map_err(ApiError::from_kern)?;
    Ok(Json(executions))
}

/// `GET /executions/{id}` — the execution plus its tool-call rows.
async fn get_execution(
    State(state): State<ApiState>,
    Path(execution_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let execution = state
        .store
        .get_execution(&execution_id)
        .map_err(ApiError::from_kern)?;
    let tool_calls = state
        .store
        .tool_calls_for_execution(&execution_id)
        .map_err(ApiError::from_kern)?;
    let mut view = serde_json::to_value(&execution)
        .map_err(|e| KernError::internal(format!("serialize execution: {e}")))?;
    view["tool_calls"] = serde_json::to_value(&tool_calls)
        .map_err(|e| KernError::internal(format!("serialize tool calls: {e}")))?;
    Ok(Json(view))
}

/// One ordered transcript entry (`SPEC.md §15.1`).
#[derive(Debug, serde::Serialize)]
struct TranscriptEntry {
    seq: i64,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool: Option<String>,
}

/// `GET /executions/{id}/transcript` — the complete ordered record of a run:
/// execution lifecycle events, model turns (`agent.thinking`,
/// `agent.completed`), and tool calls with their recorded args/result/error.
async fn transcript(
    State(state): State<ApiState>,
    Path(execution_id): Path<String>,
) -> Result<Json<Vec<TranscriptEntry>>, ApiError> {
    state
        .store
        .get_execution(&execution_id)
        .map_err(ApiError::from_kern)?;
    let events = state
        .store
        .events_for_execution_after(&execution_id, 0, 50_000)
        .map_err(ApiError::from_kern)?;
    let tool_calls: HashMap<String, crate::store::ToolCall> = state
        .store
        .tool_calls_for_execution(&execution_id)
        .map_err(ApiError::from_kern)?
        .into_iter()
        .map(|t| (t.id.clone(), t))
        .collect();

    let mut out = Vec::with_capacity(events.len());
    for e in events {
        let mut entry = TranscriptEntry {
            seq: e.seq,
            kind: e.kind.clone(),
            role: None,
            content: None,
            tool: None,
        };
        match e.kind.as_str() {
            "agent.thinking" => {
                entry.role = Some("assistant".into());
                entry.content = payload_str(&e.payload, "text");
            }
            "agent.completed" => {
                entry.role = Some("assistant".into());
                entry.content = payload_str(&e.payload, "final_text");
            }
            "agent.failed" | "model.failed" | "checkpoint.failed" => {
                entry.content = e.payload.get("error").map(|v| v.to_string());
            }
            "execution.completed" => {
                entry.content = Some(format!(
                    "{} steps, {} checkpoints",
                    e.payload["steps"], e.payload["checkpoints"]
                ));
            }
            "execution.failed" => {
                entry.content = e.payload.get("error").map(|v| v.to_string());
            }
            "tool.requested" | "tool.started" | "tool.completed" | "tool.failed" => {
                entry.tool = payload_str(&e.payload, "tool_name");
                if let Some(id) = e.payload.get("tool_call_id").and_then(Value::as_str) {
                    if let Some(row) = tool_calls.get(id) {
                        entry.content = match e.kind.as_str() {
                            "tool.completed" => row.result.as_ref().map(|v| v.to_string()),
                            "tool.failed" => row.error.as_ref().map(|v| v.to_string()),
                            _ => Some(row.args.to_string()),
                        };
                    }
                }
            }
            _ => {}
        }
        out.push(entry);
    }
    Ok(Json(out))
}

// ---------------------------------------------------------------------------
// Handlers — capabilities and permissions
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct ToolInfo {
    name: String,
    description: String,
    input_schema: Value,
    permission: &'static str,
}

/// `GET /tools` — the builtin tool catalog (constructed per request; cheap,
/// no I/O beyond schema inspection).
async fn tools(State(state): State<ApiState>) -> Result<Json<Vec<ToolInfo>>, ApiError> {
    let store = Arc::clone(&state.store);
    let mut catalog = Vec::new();
    let memory = Arc::new(StoreMemoryProvider::new(store));

    let mut push = |tool: Arc<dyn Tool>, permission: &'static str| {
        catalog.push(ToolInfo {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            input_schema: tool.input_schema().clone(),
            permission,
        });
    };
    push(
        Arc::new(kern_tool::builtins::filesystem::FileSystemTool::new(
            vec![],
            vec![],
        )),
        "filesystem",
    );
    push(
        Arc::new(kern_tool::builtins::http::HttpTool::new(
            vec![],
            HTTP_MAX_RESPONSE_BYTES,
        )),
        "network",
    );
    push(
        Arc::new(kern_tool::builtins::memory::MemoryReadTool::new(
            memory.clone(),
        )),
        "memory",
    );
    push(
        Arc::new(kern_tool::builtins::memory::MemoryWriteTool::with_limits(
            memory.clone(),
            100,
            65_536,
        )),
        "memory",
    );
    push(
        Arc::new(kern_tool::builtins::memory::MemoryListTool::new(memory)),
        "memory",
    );
    push(Arc::new(kern_tool::builtins::noop::NoopTool), "none");
    push(Arc::new(kern_tool::builtins::noop::SleepTool), "none");
    // Shell is constructed with sandbox `off` for cataloging only; the
    // per-agent construction is the fail-closed gate (SPEC §12.6).
    let shell = ShellRules {
        enabled: true,
        sandbox: Some(SandboxMode::Off),
    };
    if let Ok(Some(tool)) = build_shell_tool(
        Some(&shell),
        state.store.data_dir(),
        Duration::from_secs(30),
        None,
    ) {
        push(Arc::new(tool), "shell");
    }

    catalog.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(catalog))
}

#[derive(serde::Serialize)]
struct ModelInfo {
    provider: String,
    models: Vec<&'static str>,
    configured: bool,
}

/// `GET /models` — registered providers with informational default model
/// lists and whether the provider is configured via its env key. Model lists
/// are NOT enumerated from provider APIs in v0.1 (documented limitation).
async fn models(State(state): State<ApiState>) -> Result<Json<Vec<ModelInfo>>, ApiError> {
    let configured = |key: &str| std::env::var_os(key).is_some_and(|v| !v.is_empty());
    let infos = state
        .gateway
        .provider_ids()
        .into_iter()
        .map(|provider| {
            let (models, configured) = match provider.as_str() {
                "mock" => (vec!["test"], true),
                "openai" => (
                    vec!["gpt-4o-mini", "gpt-4o", "o3-mini"],
                    configured("OPENAI_API_KEY"),
                ),
                "anthropic" => (
                    vec!["claude-sonnet-4-5", "claude-opus-4-1", "claude-haiku-4-5"],
                    configured("ANTHROPIC_API_KEY"),
                ),
                "ollama" => (vec![], true), // model ids are local; none known statically
                _ => (vec![], false),
            };
            ModelInfo {
                provider,
                models,
                configured,
            }
        })
        .collect();
    Ok(Json(infos))
}

/// `GET /permissions/pending` — every undecided permission request.
async fn pending_permissions(
    State(state): State<ApiState>,
) -> Result<Json<Vec<crate::store::PermissionRequest>>, ApiError> {
    let requests = state
        .store
        .pending_permission_requests()
        .map_err(ApiError::from_kern)?;
    Ok(Json(requests))
}

/// `POST /permissions/{id}/grant|deny` — record the decision and nudge the
/// agent. A waiting runner observes the decision at its next park poll; a
/// recovering agent (parked across a daemon restart) is resumed so it
/// re-parks and applies the decision. Idempotent: an already-decided request
/// is returned as-is.
async fn decide_permission(
    State(state): State<ApiState>,
    Path(request_id): Path<String>,
    granted: bool,
) -> Result<Json<Value>, ApiError> {
    let request = state
        .store
        .get_permission_request(&request_id)
        .map_err(ApiError::from_kern)?;
    if request.status != crate::store::PermissionStatus::Pending {
        return Ok(Json(json!({ "status": request.status })));
    }
    let decided = state
        .store
        .decide_permission_request(&request_id, granted)
        .map_err(ApiError::from_kern)?;

    let agent = state
        .store
        .get_agent(&request.agent_id)
        .map_err(ApiError::from_kern)?;
    match agent.state {
        LifecycleState::Waiting => {
            if let Err(err) = state.engine.resume_agent(&request.agent_id).await {
                tracing::warn!(
                    request_id,
                    error = %err,
                    "resume after permission decision failed (racing completion?)"
                );
            }
        }
        LifecycleState::Recovering => {
            if let Err(err) = RecoveryManager::new(state.engine.clone())
                .resume_agent(&request.agent_id)
                .await
            {
                tracing::warn!(request_id, error = %err, "resume of recovering agent failed");
            }
        }
        _ => {}
    }
    Ok(Json(
        serde_json::to_value(&decided).map_err(|e| KernError::internal(e.to_string()))?,
    ))
}

async fn grant_permission(
    state: State<ApiState>,
    path: Path<String>,
) -> Result<Json<Value>, ApiError> {
    decide_permission(state, path, true).await
}

async fn deny_permission(
    state: State<ApiState>,
    path: Path<String>,
) -> Result<Json<Value>, ApiError> {
    decide_permission(state, path, false).await
}

/// `GET /health` — runtime liveness.
async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": KERN_VERSION,
        "schema_version": STORAGE_SCHEMA_VERSION,
        "sandbox": crate::sandbox::backend_name(),
    }))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The agent's configured checkpoint retention (default 50, SPEC §5). The
/// stored config is validated at create time, so a parse failure falls back
/// silently rather than failing the manual checkpoint.
fn checkpoint_retention(agent: &crate::store::Agent) -> u32 {
    serde_json::from_value::<crate::config::AgentSpec>(agent.config.clone())
        .map(|spec| spec.runtime.checkpoint_retention())
        .unwrap_or(50)
}

/// The active (pending|running) execution id of an agent, if any.
fn active_execution_id(store: &Store, agent_id: &str) -> crate::Result<Option<String>> {
    Ok(store
        .list_executions_for_agent(agent_id)?
        .into_iter()
        .find(|e| {
            matches!(
                e.status,
                ExecutionStatus::Pending | ExecutionStatus::Running
            )
        })
        .map(|e| e.id))
}

fn payload_str(payload: &Value, key: &str) -> Option<String> {
    payload.get(key).and_then(Value::as_str).map(str::to_string)
}

/// A §3.2-rejected action as a structured 409.
fn invalid_transition(agent_id: &str, from: LifecycleState, action: &str) -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "INVALID_TRANSITION",
        format!("cannot {action} agent {agent_id} from {}", from.as_str()),
        Some(json!({ "state": from.as_str(), "action": action })),
    )
}
