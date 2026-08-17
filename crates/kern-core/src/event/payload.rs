//! Typed payload builders for the event catalog (`SPEC.md §6`).
//!
//! One builder per catalog kind. Builders produce exactly the normative keys
//! (payloads are extensible: callers MAY add extra keys). The catalog-pinning
//! test in `super` verifies every kind has a builder and every builder emits
//! the required keys, so a catalog edit without a builder edit fails CI.

use serde_json::{json, Map, Value};

use crate::error::KernError;

/// `{ "code", "message", "detail"? }` — the structured error shape embedded in
/// `*.failed` payloads (`SPEC.md §13`).
pub fn error_payload(err: &KernError) -> Value {
    let mut payload = Map::new();
    payload.insert("code".to_string(), json!(err.code().as_str()));
    payload.insert("message".to_string(), json!(err.message));
    if let Some(detail) = &err.detail {
        payload.insert("detail".to_string(), detail.clone());
    }
    Value::Object(payload)
}

// ---------------------------------------------------------------------------
// runtime.*
// ---------------------------------------------------------------------------

pub fn runtime_started(instance_id: &str, schema_version: u32, runtime_version: &str) -> Value {
    json!({
        "instance_id": instance_id,
        "schema_version": schema_version,
        "runtime_version": runtime_version,
    })
}

pub fn runtime_shutting_down() -> Value {
    json!({})
}

// ---------------------------------------------------------------------------
// agent.*
// ---------------------------------------------------------------------------

pub fn agent_created(agent_id: &str, name: &str) -> Value {
    json!({ "agent_id": agent_id, "name": name })
}

pub fn agent_started(agent_id: &str, execution_id: &str) -> Value {
    json!({ "agent_id": agent_id, "execution_id": execution_id })
}

pub fn agent_paused(agent_id: &str, checkpoint_id: &str) -> Value {
    json!({ "agent_id": agent_id, "checkpoint_id": checkpoint_id })
}

pub fn agent_resumed(agent_id: &str, checkpoint_id: Option<&str>) -> Value {
    json!({ "agent_id": agent_id, "checkpoint_id": checkpoint_id })
}

pub fn agent_thinking(agent_id: &str, step: u64, text: &str) -> Value {
    json!({ "agent_id": agent_id, "step": step, "text": text })
}

pub fn agent_waiting(
    agent_id: &str,
    permission_request_id: &str,
    resource: &str,
    action: &str,
) -> Value {
    json!({
        "agent_id": agent_id,
        "permission_request_id": permission_request_id,
        "resource": resource,
        "action": action,
    })
}

/// Durable sleep park: carries the ISO wake time so an operator
/// can answer "why is this agent not running?" — sleeping until `wake_at`.
pub fn agent_sleeping(agent_id: &str, wake_at: &str) -> Value {
    json!({
        "agent_id": agent_id,
        "wake_at": wake_at,
    })
}

pub fn agent_completed(agent_id: &str, execution_id: &str, final_text: &str) -> Value {
    json!({
        "agent_id": agent_id,
        "execution_id": execution_id,
        "final_text": final_text,
    })
}

pub fn agent_failed(agent_id: &str, execution_id: &str, error: &KernError) -> Value {
    json!({
        "agent_id": agent_id,
        "execution_id": execution_id,
        "error": error_payload(error),
    })
}

pub fn agent_terminated(agent_id: &str) -> Value {
    json!({ "agent_id": agent_id })
}

// ---------------------------------------------------------------------------
// execution.*
// ---------------------------------------------------------------------------

pub fn execution_started(execution_id: &str, agent_id: &str) -> Value {
    json!({ "execution_id": execution_id, "agent_id": agent_id })
}

pub fn execution_completed(execution_id: &str, steps: u64, checkpoints: u64) -> Value {
    json!({
        "execution_id": execution_id,
        "steps": steps,
        "checkpoints": checkpoints,
    })
}

pub fn execution_failed(execution_id: &str, error: &KernError) -> Value {
    json!({ "execution_id": execution_id, "error": error_payload(error) })
}

pub fn execution_restored(execution_id: &str, checkpoint_id: &str) -> Value {
    json!({ "execution_id": execution_id, "checkpoint_id": checkpoint_id })
}

// ---------------------------------------------------------------------------
// model.*
// ---------------------------------------------------------------------------

/// The `model.completed` outcome kind (`SPEC.md §6`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelOutcomeKind {
    Finish,
    ToolCall,
}

impl ModelOutcomeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelOutcomeKind::Finish => "finish",
            ModelOutcomeKind::ToolCall => "tool_call",
        }
    }
}

pub fn model_requested(provider: &str, model: &str, step: u64) -> Value {
    json!({ "provider": provider, "model": model, "step": step })
}

pub fn model_completed(
    provider: &str,
    model: &str,
    kind: ModelOutcomeKind,
    latency_ms: u64,
) -> Value {
    json!({
        "provider": provider,
        "model": model,
        "kind": kind.as_str(),
        "latency_ms": latency_ms,
    })
}

pub fn model_failed(error: &KernError) -> Value {
    json!({ "error": error_payload(error) })
}

// ---------------------------------------------------------------------------
// tool.*
// ---------------------------------------------------------------------------

/// `args` is included only when tool arguments are logged (`log_tool_args`
/// opt-in, `SPEC.md §6`); pass `None` otherwise.
pub fn tool_requested(tool_call_id: &str, tool_name: &str, args: Option<&Value>) -> Value {
    let mut payload = Map::new();
    payload.insert("tool_call_id".to_string(), json!(tool_call_id));
    payload.insert("tool_name".to_string(), json!(tool_name));
    if let Some(args) = args {
        payload.insert("args".to_string(), args.clone());
    }
    Value::Object(payload)
}

pub fn tool_started(tool_call_id: &str, tool_name: &str) -> Value {
    json!({ "tool_call_id": tool_call_id, "tool_name": tool_name })
}

pub fn tool_completed(
    tool_call_id: &str,
    tool_name: &str,
    latency_ms: u64,
    result_size: u64,
) -> Value {
    json!({
        "tool_call_id": tool_call_id,
        "tool_name": tool_name,
        "latency_ms": latency_ms,
        "result_size": result_size,
    })
}

pub fn tool_failed(tool_call_id: &str, tool_name: &str, error: &KernError) -> Value {
    json!({
        "tool_call_id": tool_call_id,
        "tool_name": tool_name,
        "error": error_payload(error),
    })
}

// ---------------------------------------------------------------------------
// checkpoint.*
// ---------------------------------------------------------------------------

pub fn checkpoint_created(checkpoint_id: &str, execution_id: &str, seq: i64) -> Value {
    json!({
        "checkpoint_id": checkpoint_id,
        "execution_id": execution_id,
        "seq": seq,
    })
}

pub fn checkpoint_restored(checkpoint_id: &str, execution_id: &str) -> Value {
    json!({ "checkpoint_id": checkpoint_id, "execution_id": execution_id })
}

pub fn checkpoint_failed(error: &KernError) -> Value {
    json!({ "error": error_payload(error) })
}

// ---------------------------------------------------------------------------
// permission.*
// ---------------------------------------------------------------------------

pub fn permission_asked(permission_request_id: &str, resource: &str, action: &str) -> Value {
    json!({
        "permission_request_id": permission_request_id,
        "resource": resource,
        "action": action,
    })
}

pub fn permission_granted(permission_request_id: &str, resource: &str) -> Value {
    json!({ "permission_request_id": permission_request_id, "resource": resource })
}

pub fn permission_denied(permission_request_id: &str, resource: &str, reason: &str) -> Value {
    json!({
        "permission_request_id": permission_request_id,
        "resource": resource,
        "reason": reason,
    })
}

// ---------------------------------------------------------------------------
// scheduler.*
// ---------------------------------------------------------------------------

pub fn scheduler_recovered_agent(agent_id: &str, execution_id: &str, checkpoint_id: &str) -> Value {
    json!({
        "agent_id": agent_id,
        "execution_id": execution_id,
        "checkpoint_id": checkpoint_id,
    })
}

pub fn scheduler_run_due(agent_id: &str, scheduled_for: &str) -> Value {
    json!({ "agent_id": agent_id, "scheduled_for": scheduled_for })
}

pub fn scheduler_backoff(agent_id: &str, consecutive_failures: u32, next_run_at: &str) -> Value {
    json!({
        "agent_id": agent_id,
        "consecutive_failures": consecutive_failures,
        "next_run_at": next_run_at,
    })
}
