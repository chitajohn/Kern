//! MCP client integration: a real child process speaking JSON-RPC on stdio.
//!
//! Drives the fixture server in `tests/support/mcp_stub_server.rs` through the
//! full lifecycle — spawn, handshake, enumeration (including pagination),
//! tool calls, error results, timeouts, and the credential boundary.

use std::collections::BTreeMap;
use std::time::Duration;

use kern_tool::mcp::{McpServer, McpServerSpec};
use kern_tool::registry::ToolContext;
use kern_tool::ToolError;

fn spec(env: &[(&str, &str)], timeout: Duration) -> McpServerSpec {
    McpServerSpec {
        name: "stub".to_string(),
        command: env!("CARGO_BIN_EXE_kern_mcp_stub").to_string(),
        args: Vec::new(),
        env: env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<BTreeMap<_, _>>(),
        timeout,
    }
}

fn ctx<'a>() -> ToolContext<'a> {
    ToolContext {
        agent_id: "a",
        execution_id: "e",
        tool_call_id: "c",
    }
}

#[tokio::test]
async fn connects_and_lists_qualified_tools() {
    let server = McpServer::connect(&spec(&[], Duration::from_secs(5)))
        .await
        .expect("stub server must connect");
    let names = server.tool_names();
    assert_eq!(
        names,
        vec![
            "stub.echo".to_string(),
            "stub.slow".to_string(),
            "stub.boom".to_string(),
            "stub.env".to_string(),
        ],
        "tools are namespaced by server"
    );
}

#[tokio::test]
async fn calls_a_tool_and_returns_its_text() {
    let server = McpServer::connect(&spec(&[], Duration::from_secs(5)))
        .await
        .unwrap();
    let tools = server.into_tools();
    let echo = tools
        .iter()
        .find(|t| t.name() == "stub.echo")
        .expect("echo tool");
    assert_eq!(echo.description(), "echo the arguments back");
    assert_eq!(echo.input_schema()["required"][0], "text");

    let out = echo
        .run(&serde_json::json!({ "text": "hi" }), &ctx())
        .await
        .unwrap();
    assert_eq!(out["text"], r#"{"text":"hi"}"#);
    assert_eq!(out["server"], "stub");
    assert!(out["content"].is_array());
}

#[tokio::test]
async fn follows_tools_list_pagination() {
    let server = McpServer::connect(&spec(&[("MCP_STUB_PAGINATE", "1")], Duration::from_secs(5)))
        .await
        .unwrap();
    assert_eq!(
        server.tool_names().len(),
        4,
        "both pages must be collected: {:?}",
        server.tool_names()
    );
}

#[tokio::test]
async fn is_error_results_become_tool_failures() {
    let server = McpServer::connect(&spec(&[], Duration::from_secs(5)))
        .await
        .unwrap();
    let boom = server
        .into_tools()
        .into_iter()
        .find(|t| t.name() == "stub.boom")
        .expect("boom tool");
    let err = boom.run(&serde_json::json!({}), &ctx()).await.unwrap_err();
    assert_eq!(err.code(), "TOOL_FAILED");
    assert!(err.to_string().contains("boom"), "{err}");
}

#[tokio::test]
async fn request_timeout_is_reported_as_tool_timeout() {
    let server = McpServer::connect(&spec(&[], Duration::from_millis(300)))
        .await
        .unwrap();
    let slow = server
        .into_tools()
        .into_iter()
        .find(|t| t.name() == "stub.slow")
        .expect("slow tool");
    let err = slow.run(&serde_json::json!({}), &ctx()).await.unwrap_err();
    assert_eq!(err.code(), "TOOL_TIMEOUT");
}

#[tokio::test]
async fn server_env_is_scrubbed_except_the_configured_map() {
    std::env::set_var("KERN_MCP_LEAK_PROBE", "parent-secret");
    let server = McpServer::connect(&spec(
        &[("KERN_MCP_EXPLICIT", "configured")],
        Duration::from_secs(5),
    ))
    .await
    .unwrap();
    let env_tool = server
        .into_tools()
        .into_iter()
        .find(|t| t.name() == "stub.env")
        .expect("env tool");

    let leaked = env_tool
        .run(
            &serde_json::json!({ "name": "KERN_MCP_LEAK_PROBE" }),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(leaked["text"], "", "the daemon env must not be inherited");

    let explicit = env_tool
        .run(&serde_json::json!({ "name": "KERN_MCP_EXPLICIT" }), &ctx())
        .await
        .unwrap();
    assert_eq!(explicit["text"], "configured");

    // PATH still arrives: servers must be able to find their own runtime.
    let path = env_tool
        .run(&serde_json::json!({ "name": "PATH" }), &ctx())
        .await
        .unwrap();
    assert!(!path["text"].as_str().unwrap_or("").is_empty());
}

#[tokio::test]
async fn missing_command_fails_with_a_named_error() {
    let mut bad = spec(&[], Duration::from_secs(2));
    bad.command = "kern-no-such-mcp-server-xyz".to_string();
    match McpServer::connect(&bad).await {
        Ok(_) => panic!("a missing server binary must not connect"),
        Err(err) => {
            assert_eq!(err.code(), "TOOL_FAILED");
            assert!(err.to_string().contains("stub"), "{err}");
            assert!(matches!(err, ToolError::Failed(_)));
        }
    }
}
