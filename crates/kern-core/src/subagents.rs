//! Sub-agents / agents-as-tools (ARCHITECTURE.md §35).
//!
//! A sub-agent is not a separate runtime: it is a real agent row plus a real
//! execution, driven by the same `Engine` that drives its parent. The child
//! gets:
//!
//! - its own agent id, workspace, execution, checkpoints, and event stream, so
//!   its transcript is inspectable and its crash recovery is the runtime's
//!   normal recovery;
//! - a capability set that is a validated SUBSET of the parent's (config), and
//!   the parent's permission engine unchanged — a delegation can never widen
//!   what an agent may do;
//! - no ability to declare sub-agents, so the call graph is exactly one level
//!   deep;
//! - a bounded lifetime (`max_steps`, `max_duration`, and the parent's
//!   `runtime.subagent_timeout` as the tool-call budget).
//!
//! Deadlock avoidance: every sub-agent tool call holds one PERMIT from the
//! engine's sub-agent semaphore (`global_tool_cap / 2`) while its child runs.
//! A child's own tools therefore always have global permits left, even in the
//! worst case where every slot is occupied.

use async_trait::async_trait;
use kern_tool::{Tool, ToolContext, ToolError};
use serde_json::{json, Value};

use crate::config::{AgentRuntime, AgentSpec, MemoryBackend, SubagentConfig};
use crate::engine::Engine;
use crate::store::{Agent, LifecycleState};

/// The `agent.<name>` tool name for a sub-agent.
pub fn tool_name(sub: &SubagentConfig) -> String {
    format!("agent.{}", sub.name)
}

/// Build the child's spec from its parent's (§35).
///
/// The model block is the child's override or the parent's; permissions are
/// the parent's verbatim; memory is forced onto the durable store backend (a
/// child must not share a file-backed workspace memory file with its parent).
pub fn child_spec(parent: &AgentSpec, sub: &SubagentConfig, name: &str) -> AgentSpec {
    let mut memory = parent.memory.clone();
    memory.backend = MemoryBackend::Store;
    memory.dir = None;
    let mut runtime: AgentRuntime = parent.runtime.clone();
    if let Some(steps) = sub.max_steps {
        runtime.max_steps = Some(steps);
    }
    if let Some(duration) = sub.max_duration {
        runtime.max_duration = Some(duration);
    }
    // A sub-agent never streams: its output is a tool result, not a live view,
    // and its deltas would be indistinguishable from its parent's on the bus.
    runtime.stream = Some(false);
    AgentSpec {
        version: 1,
        name: name.to_string(),
        description: Some(sub.description.clone()),
        model: sub.model.clone().unwrap_or_else(|| parent.model.clone()),
        tools: sub.tools.clone(),
        mcp: parent.mcp.clone(),
        memory,
        permissions: parent.permissions.clone(),
        schedule: None,
        runtime,
        subagents: Vec::new(),
        credentials: parent.credentials.clone(),
    }
}

/// One `agent.<name>` tool: runs the child and returns its final text.
pub struct SubagentTool {
    engine: Engine,
    parent: AgentSpec,
    sub: SubagentConfig,
    name: String,
    description: String,
    schema: Value,
}

impl SubagentTool {
    pub fn new(engine: Engine, parent: &AgentSpec, sub: &SubagentConfig) -> Self {
        let name = tool_name(sub);
        let description = format!(
            "Delegate a self-contained task to the \"{}\" sub-agent. {}",
            sub.name, sub.description
        );
        let schema = json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The task for the sub-agent, with every fact it needs."
                },
                "context": {
                    "type": "string",
                    "description": "Optional background the sub-agent should read before starting."
                }
            },
            "required": ["input"],
            "additionalProperties": false
        });
        Self {
            engine,
            parent: parent.clone(),
            sub: sub.clone(),
            name,
            description,
            schema,
        }
    }

    /// A unique, valid agent name: `<parent>--<sub>-<6 hex>`.
    fn child_name(&self) -> String {
        let suffix = &uuid::Uuid::new_v4().simple().to_string()[..6];
        format!("{}--{}-{suffix}", self.parent.name, self.sub.name)
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> &Value {
        &self.schema
    }

    async fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> Result<Value, ToolError> {
        let input = args
            .get("input")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                ToolError::InvalidArguments(format!("{} requires a non-empty 'input'", self.name))
            })?;
        let task = match args.get("context").and_then(Value::as_str) {
            Some(context) if !context.trim().is_empty() => {
                format!("Context:\n{context}\n\nTask:\n{input}")
            }
            _ => input.to_string(),
        };

        // One slot per running child (see the module docs): acquired BEFORE the
        // child agent row exists, so a failed acquisition spawns nothing.
        let _slot = self.engine.acquire_subagent_slot().await;

        let name = self.child_name();
        let spec = child_spec(&self.parent, &self.sub, &name);
        spec.validate().map_err(|e| {
            ToolError::Failed(format!("sub-agent {} spec is invalid: {e}", self.sub.name))
        })?;
        let config = serde_json::to_value(&spec)
            .map_err(|e| ToolError::Failed(format!("serialize sub-agent spec: {e}")))?;

        let agent = Agent::new(&name, config, LifecycleState::Created);
        let agent_id = agent.id.clone();
        self.engine
            .store
            .create_agent(&agent)
            .map_err(|e| ToolError::Failed(format!("create sub-agent {name}: {e}")))?;

        tracing::info!(
            parent_agent = %ctx.agent_id,
            parent_execution = %ctx.execution_id,
            sub_agent = %name,
            sub_agent_id = %agent_id,
            "sub-agent started"
        );

        let summary = self.engine.run_agent(&agent_id, Some(&task)).await;
        // The child's row is removed once it is done: its transcript lives in
        // the event stream, but it is not a long-lived agent the operator
        // manages (`kern ps` would otherwise fill with transient children).
        let deleted = self.engine.store.delete_agent(&agent_id);
        if let Err(e) = deleted {
            tracing::warn!(sub_agent_id = %agent_id, error = %e, "could not delete sub-agent row");
        }

        let summary = summary.map_err(|e| {
            ToolError::Failed(format!("sub-agent {} failed to run: {e}", self.sub.name))
        })?;
        match summary.outcome {
            crate::engine::RunOutcome::Completed { final_text, steps } => Ok(json!({
                "agent": self.sub.name,
                "agent_id": agent_id,
                "execution_id": summary.execution_id,
                "steps": steps,
                "result": final_text,
            })),
            crate::engine::RunOutcome::Failed { error } => Err(ToolError::Failed(format!(
                "sub-agent {} failed: {}",
                self.sub.name, error
            ))),
            crate::engine::RunOutcome::Paused { checkpoint_id } => Err(ToolError::Failed(format!(
                "sub-agent {} paused at checkpoint {checkpoint_id} (a delegation cannot be resumed \
                 by its parent; retry the task)",
                self.sub.name
            ))),
            crate::engine::RunOutcome::Sleeping { wake_at } => Err(ToolError::Failed(format!(
                "sub-agent {} parked until {wake_at}; a delegation must complete synchronously",
                self.sub.name
            ))),
        }
    }
}
