//! MCP (Model Context Protocol) client — external tool servers as Kern tools.
//!
//! A configured server is spawned as a child process speaking newline-delimited
//! JSON-RPC 2.0 on stdio (the MCP stdio transport). After the `initialize`
//! handshake the client enumerates `tools/list` and wraps every remote tool as
//! a `Tool` named `<server>.<tool>`. Nothing about the protocol is trusted:
//!
//! - Every write goes through the same `PermissionEngine` class as any other
//!   tool call, so an MCP tool is `allow`/`ask`/`deny` like the rest — server
//!   authors cannot grant themselves capability by listing a tool.
//! - Requests are serialized per server (MCP servers are not required to
//!   handle concurrent requests) and bounded by a per-request timeout.
//! - The child is a `ContainedChild`: dropping the last tool kills the whole
//!   process tree (an `npx` launcher must not leave an orphaned `node`).
//! - The server never inherits the daemon environment; it receives a non-secret
//!   allowlist plus exactly the `env:` map from `agent.yaml` (SPEC §12
//!   credential boundary).
//!
//! Deliberately absent in this version: HTTP/SSE transports, server-initiated
//! requests (sampling, roots, elicitation — ignored), and `resources`/`prompts`
//! (tools only).

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use crate::error::ToolError;
use crate::process::ContainedChild;
use crate::registry::{Tool, ToolContext};

/// The MCP revision this client advertises in `initialize`.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Default per-request deadline when the config does not set one.
pub const DEFAULT_MCP_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on the text handed back to the model from one `tools/call` result.
/// Tool results are additionally bounded by the history trim, but a runaway
/// server must not be able to balloon a single turn.
pub const MAX_RESULT_CHARS: usize = 256 * 1024;

/// Environment variables an MCP server inherits. Everything else (provider
/// keys, `KERN_TOKEN`, …) is scrubbed: servers are configured by the operator,
/// not trusted with the daemon's secrets.
const ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "USERNAME",
    "USER",
    "SystemRoot",
    "windir",
    "TEMP",
    "TMP",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "TZ",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PATHEXT",
    "COMSPEC",
    "SHELL",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
];

/// One `mcp.servers[]` entry, ready to spawn.
#[derive(Debug, Clone)]
pub struct McpServerSpec {
    /// Server name; tool names are `<name>.<remote tool>`.
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// Explicit environment for the server (the only way it sees secrets).
    pub env: BTreeMap<String, String>,
    /// Per-request deadline.
    pub timeout: Duration,
}

struct ToolDescriptor {
    remote_name: String,
    description: String,
    input_schema: Value,
}

/// A live stdio connection. Owns the process, so dropping it kills the tree.
struct Connection {
    process: ContainedChild,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    timeout: Duration,
    server: String,
}

impl Connection {
    async fn send(&mut self, message: Value) -> Result<(), ToolError> {
        let mut line = serde_json::to_string(&message)
            .map_err(|e| ToolError::Failed(format!("serialize mcp message: {e}")))?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| self.closed("write", e))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| self.closed("flush", e))
    }

    fn closed(&self, op: &str, err: std::io::Error) -> ToolError {
        ToolError::Failed(format!("mcp server '{}': {op} failed: {err}", self.server))
    }

    /// Send a notification (no response is expected).
    async fn notify(&mut self, method: &str, params: Value) -> Result<(), ToolError> {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    /// Send a request and wait for the response with the matching id. Messages
    /// that are not that response (notifications, server-initiated requests)
    /// are ignored rather than misinterpreted.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, ToolError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;

        let server = self.server.clone();
        let timeout = self.timeout;
        let stdout = &mut self.stdout;
        let read = async move {
            loop {
                let mut line = String::new();
                let read = stdout.read_line(&mut line).await.map_err(|e| {
                    ToolError::Failed(format!("mcp server '{server}': read failed: {e}"))
                })?;
                if read == 0 {
                    return Err(ToolError::Failed(format!(
                        "mcp server '{server}' closed the connection"
                    )));
                }
                // Servers that log to stdout produce non-JSON lines; only
                // JSON-RPC messages are consumed.
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if message.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                if let Some(error) = message.get("error") {
                    let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
                    let text = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    return Err(ToolError::Failed(format!(
                        "mcp server '{server}' error {code}: {text}"
                    )));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
        };

        match tokio::time::timeout(timeout, read).await {
            Ok(result) => result,
            Err(_) => Err(ToolError::Timeout(timeout)),
        }
    }

    async fn list_tools(&mut self) -> Result<Vec<Value>, ToolError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = self.request("tools/list", params).await?;
            if let Some(page) = result.get("tools").and_then(Value::as_array) {
                tools.extend(page.iter().cloned());
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => return Ok(tools),
            }
        }
    }
}

/// A connected MCP server: its tools, and the process behind them.
pub struct McpServer {
    connection: Arc<Mutex<Connection>>,
    server: String,
    tools: Vec<ToolDescriptor>,
}

impl McpServer {
    /// Spawn the server, complete the handshake, and enumerate its tools.
    ///
    /// Every failure is `TOOL_FAILED`/`TOOL_TIMEOUT` with the server named: an
    /// unusable MCP server fails the agent's start rather than silently
    /// exposing a tool that cannot run.
    pub async fn connect(spec: &McpServerSpec) -> Result<Self, ToolError> {
        let mut cmd = Command::new(&spec.command);
        cmd.args(&spec.args);
        // Credential boundary: never inherit the daemon env.
        cmd.env_clear();
        for key in ENV_ALLOWLIST {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        for (key, value) in &spec.env {
            cmd.env(key, value);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut process = ContainedChild::spawn(cmd, None).map_err(|e| {
            ToolError::Failed(format!(
                "mcp server '{}': spawn {} failed: {e}",
                spec.name, spec.command
            ))
        })?;
        let stdin = process
            .take_stdin()
            .ok_or_else(|| ToolError::Failed(format!("mcp server '{}': no stdin", spec.name)))?;
        let stdout = process
            .take_stdout()
            .ok_or_else(|| ToolError::Failed(format!("mcp server '{}': no stdout", spec.name)))?;
        if let Some(stderr) = process.take_stderr() {
            // Drain stderr or a chatty server blocks on a full pipe.
            let name = spec.name.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(server = %name, "mcp stderr: {line}");
                }
            });
        }

        let mut connection = Connection {
            process,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            timeout: spec.timeout,
            server: spec.name.clone(),
        };

        connection
            .request(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "kern", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .await?;
        connection
            .notify("notifications/initialized", json!({}))
            .await?;

        tracing::debug!(
            server = %spec.name,
            pid = connection.process.id(),
            "mcp server initialized"
        );

        let tools = connection
            .list_tools()
            .await?
            .into_iter()
            .filter_map(|tool| {
                let remote_name = tool.get("name").and_then(Value::as_str)?.to_string();
                let description = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let input_schema = tool
                    .get("inputSchema")
                    .filter(|s| s.is_object())
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object" }));
                Some(ToolDescriptor {
                    remote_name,
                    description,
                    input_schema,
                })
            })
            .collect();

        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            server: spec.name.clone(),
            tools,
        })
    }

    /// The fully-qualified tool names this server exposes (`<server>.<tool>`).
    pub fn tool_names(&self) -> Vec<String> {
        self.tools
            .iter()
            .map(|t| format!("{}.{}", self.server, t.remote_name))
            .collect()
    }

    /// Wrap every remote tool as a `Tool`. Each shares the one connection.
    pub fn into_tools(self) -> Vec<Arc<dyn Tool>> {
        self.tools
            .into_iter()
            .map(|descriptor| {
                Arc::new(McpTool {
                    full_name: format!("{}.{}", self.server, descriptor.remote_name),
                    server: self.server.clone(),
                    remote_name: descriptor.remote_name,
                    description: descriptor.description,
                    input_schema: descriptor.input_schema,
                    connection: Arc::clone(&self.connection),
                }) as Arc<dyn Tool>
            })
            .collect()
    }
}

/// One remote tool, proxied over its server's connection.
struct McpTool {
    full_name: String,
    server: String,
    remote_name: String,
    description: String,
    input_schema: Value,
    connection: Arc<Mutex<Connection>>,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.full_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> &Value {
        &self.input_schema
    }

    async fn run(&self, args: &Value, _ctx: &ToolContext<'_>) -> Result<Value, ToolError> {
        // Serialized per server: MCP does not require servers to handle
        // concurrent requests, and a single ordered pipe keeps ids meaningful.
        let mut connection = self.connection.lock().await;
        let result = connection
            .request(
                "tools/call",
                json!({ "name": self.remote_name, "arguments": args }),
            )
            .await?;
        let text = render_content(result.get("content"));
        if result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(ToolError::Failed(format!(
                "mcp tool '{}' reported an error: {}",
                self.full_name,
                if text.is_empty() {
                    "(no message)"
                } else {
                    &text
                }
            )));
        }
        Ok(json!({
            "text": text,
            "content": result.get("content").cloned().unwrap_or(Value::Null),
            "structured": result.get("structuredContent").cloned().unwrap_or(Value::Null),
            "server": self.server,
        }))
    }
}

/// Flatten an MCP content array into model-facing text.
fn render_content(content: Option<&Value>) -> String {
    let Some(items) = content.and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = String::new();
    for item in items {
        let rendered = match item.get("type").and_then(Value::as_str) {
            Some("text") => item
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            Some(other) => format!("[{other} content omitted]"),
            None => continue,
        };
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&rendered);
    }
    if out.chars().count() > MAX_RESULT_CHARS {
        out = out.chars().take(MAX_RESULT_CHARS).collect();
        out.push_str("\n... (mcp result truncated)");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_text_items_and_marks_binary() {
        let content = json!([
            { "type": "text", "text": "hello" },
            { "type": "image", "data": "…" },
            { "type": "text", "text": "world" }
        ]);
        assert_eq!(
            render_content(Some(&content)),
            "hello\n[image content omitted]\nworld"
        );
        assert_eq!(render_content(None), "");
        // Unknown item shapes are skipped, not fatal.
        assert_eq!(render_content(Some(&json!([{ "no_type": 1 }]))), "");
    }

    #[test]
    fn long_results_are_truncated() {
        let text = "x".repeat(MAX_RESULT_CHARS + 10);
        let out = render_content(Some(&json!([{ "type": "text", "text": text }])));
        assert!(out.ends_with("(mcp result truncated)"));
        assert!(out.chars().count() <= MAX_RESULT_CHARS + 40);
    }
}
