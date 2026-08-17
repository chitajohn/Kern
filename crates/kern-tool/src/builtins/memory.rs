//! `memory.read` / `memory.write` / `memory.list` builtins (SPEC.md §11.3,
//! decision D14).
//!
//! Durable agent-scoped KV over the runtime's `memory` table, accessed via
//! the `MemoryProvider` seam so this crate never touches the store.
//!
//! Inputs:
//! - `memory.read`   `{ key }`
//! - `memory.write`  `{ key, value, description? }`
//! - `memory.list`   `{ prefix? }`
//!
//! Enforcement here (defense in depth; the permission engine owns
//! key-glob policy):
//! - key charset `[a-zA-Z0-9._-]`, length 1..=256 (`TOOL_INVALID_ARGUMENTS`);
//! - value size ≤ `max_value_bytes` and key count ≤ `max_keys`
//!   (`TOOL_FAILED`);
//! - list results capped at `max_entries`, with a `truncated` flag.
//!
//! Honest limitation: the key-count check is read-then-write (not atomic with
//! the put); concurrent writers can exceed `max_keys` slightly. v0.1 accepts
//! this; the cap is a guard, not a security boundary.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::{MemoryProvider, Tool, ToolContext};

/// Key charset: `[a-zA-Z0-9._-]`.
fn valid_key(key: &str) -> bool {
    if key.is_empty() || key.len() > 256 {
        return false;
    }
    key.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Defaults matching SPEC.md §9 (`max_keys: 100`, `max_value_bytes: 65536`).
pub const DEFAULT_MAX_KEYS: usize = 100;
pub const DEFAULT_MAX_VALUE_BYTES: usize = 65536;
/// Cap on entries returned by one `memory.list` call.
pub const DEFAULT_MAX_LIST_ENTRIES: usize = 200;

fn read_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "key": { "type": "string" } },
        "required": ["key"],
        "additionalProperties": false
    })
}

fn write_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "key": { "type": "string" },
            "value": {},
            "description": { "type": "string" }
        },
        "required": ["key", "value"],
        "additionalProperties": false
    })
}

fn list_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "prefix": { "type": "string" } },
        "additionalProperties": false
    })
}

pub struct MemoryReadTool {
    provider: Arc<dyn MemoryProvider>,
}

impl MemoryReadTool {
    pub fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl Tool for MemoryReadTool {
    fn name(&self) -> &str {
        "memory.read"
    }

    fn description(&self) -> &str {
        "Read a value from the agent's durable memory by key."
    }

    fn input_schema(&self) -> &Value {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(read_schema)
    }

    async fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> Result<Value, ToolError> {
        let key = args["key"].as_str().unwrap_or_default();
        if !valid_key(key) {
            return Err(ToolError::InvalidArguments(format!(
                "invalid memory key '{key}' (chars [a-zA-Z0-9._-], 1..=256)"
            )));
        }
        let entry = self
            .provider
            .get(ctx.agent_id, key)
            .await?
            .ok_or_else(|| ToolError::Failed(format!("memory key '{key}' not found")))?;
        let mut out = json!({ "key": key, "value": entry.value });
        if let Some(desc) = entry.description {
            out["description"] = json!(desc);
        }
        Ok(out)
    }
}

pub struct MemoryWriteTool {
    provider: Arc<dyn MemoryProvider>,
    max_keys: usize,
    max_value_bytes: usize,
}

impl MemoryWriteTool {
    pub fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self::with_limits(provider, DEFAULT_MAX_KEYS, DEFAULT_MAX_VALUE_BYTES)
    }

    pub fn with_limits(
        provider: Arc<dyn MemoryProvider>,
        max_keys: usize,
        max_value_bytes: usize,
    ) -> Self {
        Self {
            provider,
            max_keys,
            max_value_bytes,
        }
    }
}

#[async_trait]
impl Tool for MemoryWriteTool {
    fn name(&self) -> &str {
        "memory.write"
    }

    fn description(&self) -> &str {
        "Store a value in the agent's durable memory under a key."
    }

    fn input_schema(&self) -> &Value {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(write_schema)
    }

    async fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> Result<Value, ToolError> {
        let key = args["key"].as_str().unwrap_or_default();
        if !valid_key(key) {
            return Err(ToolError::InvalidArguments(format!(
                "invalid memory key '{key}' (chars [a-zA-Z0-9._-], 1..=256)"
            )));
        }
        let value = args.get("value").cloned().unwrap_or(Value::Null);
        let serialized = serde_json::to_string(&value).unwrap_or_default();
        if serialized.len() > self.max_value_bytes {
            return Err(ToolError::Failed(format!(
                "memory value exceeds the {}-byte cap",
                self.max_value_bytes
            )));
        }

        // Key-count guard (read-then-write; see module docs).
        let existing = self.provider.get(ctx.agent_id, key).await?;
        if existing.is_none() {
            let count = self.provider.list(ctx.agent_id, None).await?.len();
            if count >= self.max_keys {
                return Err(ToolError::Failed(format!(
                    "memory key limit reached ({})",
                    self.max_keys
                )));
            }
        }

        let description = args
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.provider
            .put(ctx.agent_id, key, value, description)
            .await?;
        Ok(json!({ "ok": true, "key": key }))
    }
}

pub struct MemoryListTool {
    provider: Arc<dyn MemoryProvider>,
    max_entries: usize,
}

impl MemoryListTool {
    pub fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self::with_limits(provider, DEFAULT_MAX_LIST_ENTRIES)
    }

    pub fn with_limits(provider: Arc<dyn MemoryProvider>, max_entries: usize) -> Self {
        Self {
            provider,
            max_entries,
        }
    }
}

#[async_trait]
impl Tool for MemoryListTool {
    fn name(&self) -> &str {
        "memory.list"
    }

    fn description(&self) -> &str {
        "List memory entries, optionally filtered by key prefix."
    }

    fn input_schema(&self) -> &Value {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(list_schema)
    }

    async fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> Result<Value, ToolError> {
        let prefix = args.get("prefix").and_then(Value::as_str);
        let entries = self.provider.list(ctx.agent_id, prefix).await?;
        let total = entries.len();
        let truncated = total > self.max_entries;
        let shown = entries
            .into_iter()
            .take(self.max_entries)
            .collect::<Vec<_>>();
        let items: Vec<Value> = shown
            .into_iter()
            .map(|e| {
                let mut item = json!({ "key": e.key, "value": e.value });
                if let Some(desc) = e.description {
                    item["description"] = json!(desc);
                }
                item
            })
            .collect();
        Ok(json!({
            "entries": items,
            "count": total,
            "truncated": truncated,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory provider for tests (also the shape kern-core implements).
    #[derive(Default)]
    struct FakeMemory {
        entries: Mutex<HashMap<String, (Value, Option<String>)>>,
    }

    #[async_trait]
    impl MemoryProvider for FakeMemory {
        async fn get(
            &self,
            _agent_id: &str,
            key: &str,
        ) -> Result<Option<crate::registry::MemoryEntry>, ToolError> {
            let entries = self.entries.lock().unwrap();
            Ok(entries
                .get(key)
                .map(|(value, description)| crate::registry::MemoryEntry {
                    key: key.to_string(),
                    value: value.clone(),
                    description: description.clone(),
                }))
        }

        async fn put(
            &self,
            _agent_id: &str,
            key: &str,
            value: Value,
            description: Option<String>,
        ) -> Result<(), ToolError> {
            self.entries
                .lock()
                .unwrap()
                .insert(key.to_string(), (value, description));
            Ok(())
        }

        async fn list(
            &self,
            _agent_id: &str,
            prefix: Option<&str>,
        ) -> Result<Vec<crate::registry::MemoryEntry>, ToolError> {
            let entries = self.entries.lock().unwrap();
            let mut out: Vec<crate::registry::MemoryEntry> = entries
                .iter()
                .filter(|(k, _)| prefix.is_none_or(|p| k.starts_with(p)))
                .map(|(k, (value, description))| crate::registry::MemoryEntry {
                    key: k.clone(),
                    value: value.clone(),
                    description: description.clone(),
                })
                .collect();
            out.sort_by(|a, b| a.key.cmp(&b.key));
            Ok(out)
        }
    }

    fn ctx<'a>() -> ToolContext<'a> {
        ToolContext {
            agent_id: "a",
            execution_id: "e",
            tool_call_id: "c",
        }
    }

    fn provider() -> Arc<FakeMemory> {
        Arc::new(FakeMemory::default())
    }

    #[tokio::test]
    async fn write_read_list_round_trip() {
        let mem = provider();
        let write = MemoryWriteTool::new(mem.clone());
        let read = MemoryReadTool::new(mem.clone());
        let list = MemoryListTool::new(mem.clone());

        write
            .run(
                &json!({ "key": "goal", "value": { "text": "ship" }, "description": "primary goal" }),
                &ctx(),
            )
            .await
            .unwrap();
        write
            .run(&json!({ "key": "notes.a", "value": 1 }), &ctx())
            .await
            .unwrap();
        write
            .run(&json!({ "key": "notes.b", "value": 2 }), &ctx())
            .await
            .unwrap();

        let out = read.run(&json!({ "key": "goal" }), &ctx()).await.unwrap();
        assert_eq!(out["value"]["text"], "ship");
        assert_eq!(out["description"], "primary goal");

        let all = list.run(&json!({}), &ctx()).await.unwrap();
        assert_eq!(all["count"], 3);
        assert_eq!(all["truncated"], false);

        let notes = list
            .run(&json!({ "prefix": "notes." }), &ctx())
            .await
            .unwrap();
        assert_eq!(notes["count"], 2);
        assert_eq!(notes["entries"][0]["key"], "notes.a");
    }

    #[tokio::test]
    async fn missing_key_is_an_error() {
        let mem = provider();
        let read = MemoryReadTool::new(mem);
        let err = read
            .run(&json!({ "key": "nope" }), &ctx())
            .await
            .unwrap_err();
        assert_eq!(err.code(), "TOOL_FAILED");
        assert!(err.to_string().contains("nope"));
    }

    #[tokio::test]
    async fn bad_keys_rejected() {
        let mem = provider();
        let write = MemoryWriteTool::new(mem.clone());
        for bad in ["", "has space", "sla/sh", "../up", "ünïcode"] {
            let err = write
                .run(&json!({ "key": bad, "value": 1 }), &ctx())
                .await
                .unwrap_err();
            assert_eq!(err.code(), "TOOL_INVALID_ARGUMENTS", "key {bad:?}");
        }
        let read = MemoryReadTool::new(mem);
        let err = read
            .run(&json!({ "key": "bad key" }), &ctx())
            .await
            .unwrap_err();
        assert_eq!(err.code(), "TOOL_INVALID_ARGUMENTS");
    }

    #[tokio::test]
    async fn value_size_cap_enforced() {
        let mem = provider();
        let write = MemoryWriteTool::with_limits(mem, 10, 16);
        let err = write
            .run(&json!({ "key": "big", "value": "x".repeat(32) }), &ctx())
            .await
            .unwrap_err();
        assert_eq!(err.code(), "TOOL_FAILED");
        assert!(err.to_string().contains("cap"));
    }

    #[tokio::test]
    async fn key_count_cap_enforced() {
        let mem = provider();
        let write = MemoryWriteTool::with_limits(mem.clone(), 2, 1024);
        write
            .run(&json!({ "key": "a", "value": 1 }), &ctx())
            .await
            .unwrap();
        write
            .run(&json!({ "key": "b", "value": 1 }), &ctx())
            .await
            .unwrap();
        let err = write
            .run(&json!({ "key": "c", "value": 1 }), &ctx())
            .await
            .unwrap_err();
        assert_eq!(err.code(), "TOOL_FAILED");
        assert!(err.to_string().contains("limit"));

        // Overwriting an existing key does not count against the cap.
        write
            .run(&json!({ "key": "a", "value": 2 }), &ctx())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn list_truncates_with_flag() {
        let mem = provider();
        let write = MemoryWriteTool::new(mem.clone());
        for i in 0..5 {
            write
                .run(&json!({ "key": format!("k{i}"), "value": i }), &ctx())
                .await
                .unwrap();
        }
        let list = MemoryListTool::with_limits(mem, 2);
        let out = list.run(&json!({}), &ctx()).await.unwrap();
        assert_eq!(out["count"], 5);
        assert_eq!(out["truncated"], true);
        assert_eq!(out["entries"].as_array().unwrap().len(), 2);
    }
}
