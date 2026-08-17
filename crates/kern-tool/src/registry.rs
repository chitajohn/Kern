//! Tool abstraction (ARCHITECTURE.md §9).
//!
//! - `Tool` is the unit of capability: a name, description, JSON Schema input,
//!   and an async `run`. Tools are stateless across calls (per-call context
//!   arrives in `ToolContext`) and shared via `Arc<dyn Tool>`.
//! - `ToolRegistry` maps names to tools and validates arguments against each
//!   tool's JSON Schema (compiled once at registration, cached).
//! - `MemoryProvider` is the seam between the store-coupled memory builtins
//!   and the runtime: `kern-core` implements it over the `memory` table so
//!   `kern-tool` never needs the store (the crate dependency direction is
//!   core → tool, never tool → core).
//!
//! Tools MUST NOT enforce gateway/engine policy (timeouts, concurrency,
//! permissions beyond their own in-tool containment): the `ToolExecutor` and
//! the permission engine own that. Tools MUST be idempotent for a given
//! `tool_call_id` where it matters (SPEC.md §11.1); the id is carried in
//! `ToolContext` for exactly that purpose.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use jsonschema::Validator;
use serde_json::Value;

pub use kern_model::ToolSpec;

use crate::error::ToolError;

/// Per-call context passed to a tool.
#[derive(Debug, Clone, Copy)]
pub struct ToolContext<'a> {
    pub agent_id: &'a str,
    pub execution_id: &'a str,
    /// The model-supplied (or synthesized) tool-call id — the dedup key
    /// (SPEC.md §4.3). Tools that perform side effects keyed by call id can
    /// use it for tool-side idempotency (SPEC.md §11.1).
    pub tool_call_id: &'a str,
}

/// A tool capability.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// The JSON Schema object describing valid `run` arguments.
    fn input_schema(&self) -> &Value;
    /// Execute the tool. Must not panic; must return reasonably quickly
    /// (timeouts are the executor's job, but a tool SHOULD respect short
    /// request-level deadlines so cancellation is prompt).
    async fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> Result<Value, ToolError>;
}

/// A registered tool with its compiled schema validator.
struct RegisteredTool {
    tool: Arc<dyn Tool>,
    validator: Validator,
}

/// The registry: name → tool + cached argument validation.
pub struct ToolRegistry {
    tools: HashMap<String, RegisteredTool>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool. Duplicate names are rejected (`TOOL_FAILED` — this is
    /// a programming error, not a model error).
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolError> {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            return Err(ToolError::Failed(format!(
                "tool '{name}' is already registered"
            )));
        }
        let validator = jsonschema::validator_for(tool.input_schema()).map_err(|e| {
            ToolError::Failed(format!("tool '{name}' has an invalid input schema: {e}"))
        })?;
        self.tools.insert(name, RegisteredTool { tool, validator });
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).map(|r| Arc::clone(&r.tool))
    }

    pub fn has(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tools.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Validate `args` against the named tool's schema.
    /// Unknown tool → `TOOL_UNAVAILABLE`; invalid args → `TOOL_INVALID_ARGUMENTS`.
    pub fn validate(&self, name: &str, args: &Value) -> Result<(), ToolError> {
        let registered = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::Unavailable(format!("tool '{name}' is not registered")))?;
        registered
            .validator
            .validate(args)
            .map_err(|e| ToolError::InvalidArguments(format!("{}: {e}", name)))?;
        Ok(())
    }

    /// The model-facing spec snapshot for the agent's configured tool list.
    /// Unknown tool → `TOOL_UNAVAILABLE`.
    pub fn specs(&self, configured: &[String]) -> Result<Vec<ToolSpec>, ToolError> {
        let mut specs = Vec::with_capacity(configured.len());
        for name in configured {
            let registered = self.tools.get(name).ok_or_else(|| {
                ToolError::Unavailable(format!("tool '{name}' is not registered"))
            })?;
            specs.push(ToolSpec {
                name: name.clone(),
                description: registered.tool.description().to_string(),
                input_schema: registered.tool.input_schema().clone(),
            });
        }
        Ok(specs)
    }
}

/// One memory entry (the store-coupled shape `kern-core` maps onto).
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryEntry {
    pub key: String,
    pub value: Value,
    pub description: Option<String>,
}

/// The memory seam: agent-scoped durable KV implemented by `kern-core` over
/// the `memory` table (SPEC.md §5). Methods are async so the store-backed
/// implementation can offload blocking rusqlite calls via `spawn_blocking`.
#[async_trait]
pub trait MemoryProvider: Send + Sync {
    async fn get(&self, agent_id: &str, key: &str) -> Result<Option<MemoryEntry>, ToolError>;
    async fn put(
        &self,
        agent_id: &str,
        key: &str,
        value: Value,
        description: Option<String>,
    ) -> Result<(), ToolError>;
    async fn list(
        &self,
        agent_id: &str,
        prefix: Option<&str>,
    ) -> Result<Vec<MemoryEntry>, ToolError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoTool;
    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echo args back"
        }
        fn input_schema(&self) -> &Value {
            static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            SCHEMA.get_or_init(|| {
                serde_json::json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"],
                    "additionalProperties": false,
                })
            })
        }
        async fn run(&self, args: &Value, _ctx: &ToolContext<'_>) -> Result<Value, ToolError> {
            Ok(args.clone())
        }
    }

    fn ctx<'a>() -> ToolContext<'a> {
        ToolContext {
            agent_id: "a",
            execution_id: "e",
            tool_call_id: "c",
        }
    }

    #[test]
    fn register_and_lookup() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool)).unwrap();
        assert!(registry.has("echo"));
        assert!(!registry.has("nope"));
        assert_eq!(registry.names(), vec!["echo".to_string()]);
        assert!(registry.get("echo").is_some());
        assert!(registry.get("nope").is_none());
    }

    #[test]
    fn duplicate_registration_rejected() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool)).unwrap();
        let err = registry.register(Arc::new(EchoTool)).unwrap_err();
        assert_eq!(err.code(), "TOOL_FAILED");
    }

    #[test]
    fn invalid_schema_rejected_at_registration() {
        struct BadSchemaTool;
        #[async_trait]
        impl Tool for BadSchemaTool {
            fn name(&self) -> &str {
                "bad"
            }
            fn description(&self) -> &str {
                ""
            }
            fn input_schema(&self) -> &Value {
                static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
                SCHEMA.get_or_init(|| serde_json::json!({ "not": "a schema" }))
            }
            async fn run(&self, _a: &Value, _c: &ToolContext<'_>) -> Result<Value, ToolError> {
                unreachable!()
            }
        }
        let mut registry = ToolRegistry::new();
        assert!(registry.register(Arc::new(BadSchemaTool)).is_err());
    }

    #[test]
    fn validation_accepts_and_rejects() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool)).unwrap();

        registry
            .validate("echo", &serde_json::json!({ "text": "hi" }))
            .unwrap();

        // Missing required field.
        let err = registry
            .validate("echo", &serde_json::json!({}))
            .unwrap_err();
        assert_eq!(err.code(), "TOOL_INVALID_ARGUMENTS");

        // Wrong type.
        let err = registry
            .validate("echo", &serde_json::json!({ "text": 42 }))
            .unwrap_err();
        assert_eq!(err.code(), "TOOL_INVALID_ARGUMENTS");

        // Extra property (additionalProperties: false).
        let err = registry
            .validate("echo", &serde_json::json!({ "text": "hi", "x": 1 }))
            .unwrap_err();
        assert_eq!(err.code(), "TOOL_INVALID_ARGUMENTS");

        // Unknown tool.
        let err = registry
            .validate("nope", &serde_json::json!({}))
            .unwrap_err();
        assert_eq!(err.code(), "TOOL_UNAVAILABLE");
    }

    #[test]
    fn specs_snapshot_matches_configured_subset() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool)).unwrap();

        let specs = registry.specs(&["echo".to_string()]).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "echo");
        assert_eq!(specs[0].description, "echo args back");
        assert!(specs[0].input_schema.is_object());

        let err = registry.specs(&["ghost".to_string()]).unwrap_err();
        assert_eq!(err.code(), "TOOL_UNAVAILABLE");
    }

    #[tokio::test]
    async fn tool_runs_with_context() {
        let tool = EchoTool;
        let out = tool
            .run(&serde_json::json!({ "text": "hi" }), &ctx())
            .await
            .unwrap();
        assert_eq!(out["text"], "hi");
    }
}
