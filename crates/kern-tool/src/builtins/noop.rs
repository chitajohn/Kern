//! `noop` / `sleep` fixture builtins (SPEC.md §11.3).
//!
//! `noop` returns `{ "ok": true }` — the identity tool for tests and demos.
//! `sleep` sleeps `ms` milliseconds (cap 60s) and is the deterministic
//! fixture for timeout and concurrency tests.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::{Tool, ToolContext};

fn noop_schema() -> Value {
    json!({ "type": "object", "additionalProperties": false })
}

fn sleep_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "ms": { "type": "integer", "minimum": 0, "maximum": 60_000 } },
        "required": ["ms"],
        "additionalProperties": false
    })
}

pub struct NoopTool;

#[async_trait]
impl Tool for NoopTool {
    fn name(&self) -> &str {
        "noop"
    }

    fn description(&self) -> &str {
        "Does nothing and returns ok. Useful for tests and demos."
    }

    fn input_schema(&self) -> &Value {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(noop_schema)
    }

    async fn run(&self, _args: &Value, _ctx: &ToolContext<'_>) -> Result<Value, ToolError> {
        Ok(json!({ "ok": true }))
    }
}

pub struct SleepTool;

#[async_trait]
impl Tool for SleepTool {
    fn name(&self) -> &str {
        "sleep"
    }

    fn description(&self) -> &str {
        "Sleeps for the given number of milliseconds (max 60000)."
    }

    fn input_schema(&self) -> &Value {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(sleep_schema)
    }

    async fn run(&self, args: &Value, _ctx: &ToolContext<'_>) -> Result<Value, ToolError> {
        let ms = args["ms"].as_u64().unwrap_or(0);
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        Ok(json!({ "slept_ms": ms }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>() -> ToolContext<'a> {
        ToolContext {
            agent_id: "a",
            execution_id: "e",
            tool_call_id: "c",
        }
    }

    #[tokio::test]
    async fn noop_returns_ok() {
        let out = NoopTool.run(&json!({}), &ctx()).await.unwrap();
        assert_eq!(out, json!({ "ok": true }));
    }

    #[tokio::test]
    async fn sleep_returns_slept_ms() {
        let out = SleepTool.run(&json!({ "ms": 10 }), &ctx()).await.unwrap();
        assert_eq!(out["slept_ms"], 10);
    }
}
