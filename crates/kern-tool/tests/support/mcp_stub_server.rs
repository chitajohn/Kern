//! A minimal MCP stdio server used by `tests/mcp.rs`.
//!
//! It exists so the client can be exercised against a real child process on
//! every CI runner without depending on node/python. It speaks just enough of
//! the protocol: `initialize`, `tools/list` (optionally paginated), and
//! `tools/call`.
//!
//! Tools:
//! - `echo`  — returns its arguments as JSON text
//! - `slow`  — sleeps 5 s, then returns (drives the timeout test)
//! - `boom`  — returns `isError: true`
//! - `env`   — returns an environment variable (drives the env-scrub test)

use std::io::{self, BufRead, Write};
use std::time::Duration;

use serde_json::{json, Value};

fn main() {
    // A plain log line on stdout: clients must skip non-JSON noise.
    println!("kern-mcp-stub starting");
    eprintln!("kern-mcp-stub: stderr is drained by the client");
    let paginate = std::env::var("MCP_STUB_PAGINATE").is_ok();
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let id = message.get("id").cloned();
        match method {
            "initialize" => send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "kern-mcp-stub", "version": "0" },
                },
            })),
            // Notification: no response.
            "notifications/initialized" => {}
            "tools/list" => send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": list_page(paginate, &message),
            })),
            "tools/call" => send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": call(&message),
            })),
            _ => {
                if id.is_some() {
                    send(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": format!("method not found: {method}") },
                    }));
                }
            }
        }
    }
}

fn send(value: Value) {
    println!("{value}");
    let _ = io::stdout().flush();
}

fn tool(name: &str, description: &str, schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": schema })
}

fn list_page(paginate: bool, message: &Value) -> Value {
    let cursor = message
        .get("params")
        .and_then(|p| p.get("cursor"))
        .and_then(Value::as_str);
    let echo = tool(
        "echo",
        "echo the arguments back",
        json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"],
            "additionalProperties": false,
        }),
    );
    let others = vec![
        tool(
            "slow",
            "sleeps before answering",
            json!({ "type": "object" }),
        ),
        tool(
            "boom",
            "always fails",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "env",
            "read an environment variable",
            json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"],
            }),
        ),
    ];
    if !paginate {
        let mut tools = vec![echo];
        tools.extend(others);
        return json!({ "tools": tools });
    }
    match cursor {
        None => json!({ "tools": [echo], "nextCursor": "page-2" }),
        Some("page-2") => json!({ "tools": others }),
        Some(_) => json!({ "tools": [] }),
    }
}

fn text(value: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": value }], "isError": false })
}

fn call(message: &Value) -> Value {
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    match name {
        "echo" => text(&args.to_string()),
        "slow" => {
            std::thread::sleep(Duration::from_secs(5));
            text("slow done")
        }
        "boom" => json!({
            "content": [{ "type": "text", "text": "boom" }],
            "isError": true,
        }),
        "env" => {
            let key = args.get("name").and_then(Value::as_str).unwrap_or("");
            text(&std::env::var(key).unwrap_or_default())
        }
        other => json!({
            "content": [{ "type": "text", "text": format!("unknown tool {other}") }],
            "isError": true,
        }),
    }
}
