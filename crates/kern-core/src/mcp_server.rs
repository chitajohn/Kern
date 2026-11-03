//! Kern as an MCP tool server (ARCHITECTURE.md §36).
//!
//! The inverse of `kern_tool::mcp`: another agent (or an MCP-capable client)
//! speaks JSON-RPC to Kern over stdio and calls this agent's tools. The
//! security position is the same as the runtime's own tool execution:
//!
//! - **Policy first.** Every `tools/call` is classified by the SAME
//!   `crate::policy::classify` the engine uses, against the spec's permission
//!   engine and workspace. There is no operator channel on this surface, so an
//!   `ask` rule FAILS CLOSED (it is not silently treated as an allow).
//! - **No runtime state.** The server exposes the stateless tools
//!   (`filesystem`, `http`, `shell`, `noop`, `sleep`). Memory, sub-agents, and
//!   proxied MCP servers need the daemon's store or engine and are rejected at
//!   start rather than served with wrong state.
//! - **Bounded.** Each call is wrapped in `runtime.tool_timeout`, argument
//!   validation is the registry's, and responses are text with `isError`.
//!
//! Transport: newline-delimited JSON-RPC 2.0 on stdin/stdout, protocol
//! `2024-11-05`. Logs go to stderr.

use std::path::Path;
use std::sync::Arc;

use kern_tool::{ToolContext, ToolRegistry};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::config::AgentSpec;
use crate::error::{ErrorCode, KernError, Result};
use crate::permissions::PermissionEngine;
use crate::policy::{classify, CallDecision};
use crate::tools::build_server_registry;

/// The MCP protocol revision this server speaks.
pub const MCP_SERVER_PROTOCOL_VERSION: &str = "2024-11-05";

/// A prepared MCP serving surface for one agent spec.
pub struct McpToolServer {
    spec: AgentSpec,
    permissions: PermissionEngine,
    registry: Arc<ToolRegistry>,
}

impl McpToolServer {
    /// Build the surface. Fails closed on tools that need runtime state.
    pub fn build(spec: &AgentSpec, workspace: &Path) -> Result<Self> {
        spec.validate()?;
        let permissions = PermissionEngine::from_config(&spec.permissions, workspace)?;
        let registry = build_server_registry(spec, workspace)?;
        Ok(Self {
            spec: spec.clone(),
            permissions,
            registry,
        })
    }

    /// Handle one decoded JSON-RPC message. Returns `None` for notifications.
    pub async fn handle(&self, message: &Value) -> Option<Value> {
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        // A notification (no id) never gets a response.
        let id = id?;

        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": MCP_SERVER_PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "kern",
                    "version": crate::version::KERN_VERSION,
                },
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": self.tools() })),
            "tools/call" => self.call_tool(&params).await,
            other => Err(rpc_error(-32601, format!("method not found: {other}"))),
        };

        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
        })
    }

    /// The model-facing tool list (`tools/list`).
    pub fn tools(&self) -> Value {
        let visible: Vec<String> = self
            .spec
            .tools
            .iter()
            .filter(|t| self.registry.has(t))
            .cloned()
            .collect();
        let specs = self.registry.specs(&visible).unwrap_or_default();
        Value::Array(
            specs
                .into_iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "inputSchema": tool.input_schema,
                    })
                })
                .collect(),
        )
    }

    async fn call_tool(&self, params: &Value) -> std::result::Result<Value, Value> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| rpc_error(-32602, "tools/call requires a tool name"))?;
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        // Existence and argument validity are the registry's contract.
        if let Err(err) = self.registry.validate(name, &arguments) {
            return Ok(tool_result(&err.to_json().to_string(), true));
        }

        let classified = classify(&self.spec, &self.permissions, name, &arguments);
        match classified.decision {
            CallDecision::Allow => {}
            CallDecision::Deny { reason } => {
                return Ok(tool_result(
                    &json!({ "code": "PERMISSION_DENIED", "message": reason }).to_string(),
                    true,
                ))
            }
            CallDecision::Ask => {
                return Ok(tool_result(
                    &json!({
                        "code": "PERMISSION_DENIED",
                        "message": format!(
                            "{} requires an operator approval; the MCP surface cannot request one",
                            classified.resource
                        ),
                    })
                    .to_string(),
                    true,
                ))
            }
        }

        let Some(tool) = self.registry.get(name) else {
            return Ok(tool_result(
                &json!({ "code": "TOOL_UNAVAILABLE", "message": format!("tool {name:?}") })
                    .to_string(),
                true,
            ));
        };

        let execution_id = uuid::Uuid::new_v4().to_string();
        let ctx = ToolContext {
            agent_id: "mcp-server",
            execution_id: &execution_id,
            tool_call_id: &execution_id,
        };
        let timeout = self.spec.runtime.tool_timeout().as_std();
        match tokio::time::timeout(timeout, tool.run(&arguments, &ctx)).await {
            Ok(Ok(value)) => Ok(tool_result(
                &serde_json::to_string(&value).unwrap_or_else(|_| "{}".into()),
                false,
            )),
            Ok(Err(err)) => Ok(tool_result(&err.to_json().to_string(), true)),
            Err(_) => Ok(tool_result(
                &json!({
                    "code": "TOOL_TIMEOUT",
                    "message": format!("tool {name:?} exceeded {} ms", timeout.as_millis()),
                })
                .to_string(),
                true,
            )),
        }
    }
}

fn tool_result(text: &str, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

fn rpc_error(code: i64, message: impl Into<String>) -> Value {
    json!({ "code": code, "message": message.into() })
}

/// Serve the surface over stdio until stdin closes.
pub async fn serve_stdio(spec: &AgentSpec, workspace: &Path) -> Result<()> {
    let server = McpToolServer::build(spec, workspace)?;
    tracing::info!(
        agent = %spec.name,
        tools = server.registry.len(),
        "mcp server listening on stdio"
    );

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| KernError::internal(format!("mcp stdin: {e}")))?
    {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(e) => {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": rpc_error(-32700, format!("parse error: {e}")),
                });
                write_message(&mut stdout, &response).await?;
                continue;
            }
        };
        if let Some(response) = server.handle(&message).await {
            write_message(&mut stdout, &response).await?;
        }
    }
    Ok(())
}

async fn write_message(stdout: &mut tokio::io::Stdout, value: &Value) -> Result<()> {
    let mut line = serde_json::to_string(value)
        .map_err(|e| KernError::internal(format!("serialize mcp response: {e}")))?;
    line.push('\n');
    stdout
        .write_all(line.as_bytes())
        .await
        .map_err(|e| KernError::new(ErrorCode::Internal, format!("mcp stdout: {e}")))?;
    stdout
        .flush()
        .await
        .map_err(|e| KernError::new(ErrorCode::Internal, format!("mcp stdout flush: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SPEC: &str = "version: 1\nname: served\nmodel:\n  provider: mock\n  model: m\n\
                        tools: [noop, filesystem]\n\
                        permissions:\n  filesystem:\n    read:\n      allow: [\"./**\"]\n";

    fn server() -> (tempfile::TempDir, McpToolServer) {
        let dir = tempfile::tempdir().unwrap();
        let spec = crate::config::parse_agent_spec(SPEC).unwrap();
        let server = McpToolServer::build(&spec, dir.path()).unwrap();
        (dir, server)
    }

    #[tokio::test]
    async fn initialize_advertises_tools_and_protocol() {
        let (_dir, server) = server();
        let response = server
            .handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }))
            .await
            .unwrap();
        assert_eq!(
            response["result"]["protocolVersion"],
            MCP_SERVER_PROTOCOL_VERSION
        );
        assert_eq!(response["result"]["serverInfo"]["name"], "kern");
        assert!(response["result"]["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn notifications_get_no_response() {
        let (_dir, server) = server();
        assert!(server
            .handle(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn tools_list_is_the_configured_subset() {
        let (_dir, server) = server();
        let response = server
            .handle(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
            .await
            .unwrap();
        let tools = response["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["noop", "filesystem"]);
        assert!(tools[0]["inputSchema"].is_object());
    }

    #[tokio::test]
    async fn tool_call_runs_and_denies_by_policy() {
        let (_dir, server) = server();
        let ok = server
            .handle(&json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "noop", "arguments": {} }
            }))
            .await
            .unwrap();
        assert_eq!(ok["result"]["isError"], false);

        // A read outside the allowed root is denied by the same policy the
        // engine uses — not by a second, weaker check in this module.
        let denied = server
            .handle(&json!({
                "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": { "name": "filesystem", "arguments": { "action": "read", "path": "/etc/passwd" } }
            }))
            .await
            .unwrap();
        assert_eq!(denied["result"]["isError"], true);
        let text = denied["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("PERMISSION_DENIED"), "{text}");
    }

    #[tokio::test]
    async fn unknown_method_and_missing_tool_are_errors() {
        let (_dir, server) = server();
        let unknown = server
            .handle(&json!({ "jsonrpc": "2.0", "id": 5, "method": "resources/list" }))
            .await
            .unwrap();
        assert_eq!(unknown["error"]["code"], -32601);

        let missing = server
            .handle(&json!({
                "jsonrpc": "2.0", "id": 6, "method": "tools/call",
                "params": { "name": "ghost", "arguments": {} }
            }))
            .await
            .unwrap();
        assert_eq!(missing["result"]["isError"], true);
    }

    #[test]
    fn tools_needing_runtime_state_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let spec = crate::config::parse_agent_spec(
            "version: 1\nname: stateful\nmodel:\n  provider: mock\n  model: m\n\
             tools: [memory.read]\nmemory:\n  enabled: true\n\
             permissions:\n  memory:\n    read:\n      allow: [\"*\"]\n",
        )
        .unwrap();
        match McpToolServer::build(&spec, dir.path()) {
            Ok(_) => panic!("a stateful tool must not be served over MCP"),
            Err(err) => {
                assert_eq!(err.code(), ErrorCode::ConfigInvalid);
                assert!(err.to_string().contains("memory.read"), "{err}");
            }
        }
    }
}
