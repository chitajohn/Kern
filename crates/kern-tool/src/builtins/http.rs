//! `http` builtin (SPEC.md §11.3).
//!
//! Input: `{ method: get|post, url, headers?, body?, timeout? }`.
//!
//! Security model (defense in depth — the permission engine
//! re-enforces host policy at the policy layer):
//! - **Host allowlist:** exact match on the normalized host (lowercase,
//!   trailing dot stripped). An empty allowlist denies everything (default
//!   deny). Full IDN/IPv6 normalization lives in the permission
//!   engine; this layer is a fast exact check.
//! - **No redirects:** redirects are NOT followed (a `3xx` is returned to the
//!   model), so an allowlisted host cannot redirect the request to a
//!   non-allowlisted one.
//! - **TLS verified:** the client verifies TLS by default (rustls); there is
//!   no way to disable it in v0.1.
//! - **Response cap:** bodies larger than `max_response_bytes` fail with
//!   `TOOL_FAILED` rather than being silently truncated.
//!
//! Honest limitations: bodies are returned as lossy UTF-8 strings (binary
//! payloads are mangled); user-supplied headers are passed through as-is.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::{Tool, ToolContext};

/// Default response cap for a single request.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// Default per-request timeout when the args do not specify one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "method": { "type": "string", "enum": ["get", "post"] },
            "url": { "type": "string" },
            "headers": {
                "type": "object",
                "additionalProperties": { "type": "string" }
            },
            "body": { "type": "string" },
            "timeout": { "type": "integer", "minimum": 1, "maximum": 120_000 }
        },
        "required": ["method", "url"],
        "additionalProperties": false
    })
}

pub struct HttpTool {
    /// Normalized allowed hosts. Empty ⇒ no network access at all.
    allowed_hosts: Vec<String>,
    max_response_bytes: usize,
    client: reqwest::Client,
}

impl HttpTool {
    /// `allowed_hosts` may contain `host` or `host:port` entries; matching is
    /// on the normalized host after any port is stripped.
    pub fn new(allowed_hosts: Vec<String>, max_response_bytes: usize) -> Self {
        let normalized = allowed_hosts
            .into_iter()
            .map(|h| normalize_host(&h))
            .collect();
        // Redirects disabled so an allowlisted host cannot bounce the
        // request to an unvetted one (defense in depth).
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client builder with fixed options cannot fail");
        Self {
            allowed_hosts: normalized,
            max_response_bytes,
            client,
        }
    }
}

/// Lowercase and strip a trailing dot (the fast in-tool normalization).
fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_lowercase()
}

#[async_trait]
impl Tool for HttpTool {
    fn name(&self) -> &str {
        "http"
    }

    fn description(&self) -> &str {
        "Make HTTP GET or POST requests to allowlisted hosts."
    }

    fn input_schema(&self) -> &Value {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(schema)
    }

    async fn run(&self, args: &Value, _ctx: &ToolContext<'_>) -> Result<Value, ToolError> {
        let method = args["method"].as_str().unwrap_or_default();
        let url_str = args["url"].as_str().unwrap_or_default();

        let url = url::Url::parse(url_str)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid url '{url_str}': {e}")))?;
        let host = url
            .host_str()
            .ok_or_else(|| ToolError::InvalidArguments(format!("url '{url_str}' has no host")))?;
        let normalized = normalize_host(host);
        if self.allowed_hosts.is_empty() {
            return Err(ToolError::PermissionDenied(
                "no network hosts are allowed for this agent".to_string(),
            ));
        }
        if !self.allowed_hosts.contains(&normalized) {
            return Err(ToolError::PermissionDenied(format!(
                "host '{normalized}' is not in the agent's allowed list"
            )));
        }

        // Build headers (reject invalid names via reqwest's parse).
        let mut headers = HeaderMap::new();
        if let Some(raw) = args.get("headers").and_then(Value::as_object) {
            for (name, value) in raw {
                let value = value.as_str().unwrap_or_default();
                let header =
                    reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                        ToolError::InvalidArguments(format!("invalid header name '{name}': {e}"))
                    })?;
                let header_value = reqwest::header::HeaderValue::from_str(value).map_err(|e| {
                    ToolError::InvalidArguments(format!("invalid header value for '{name}': {e}"))
                })?;
                headers.insert(header, header_value);
            }
        }

        let timeout = args
            .get("timeout")
            .and_then(Value::as_u64)
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_TIMEOUT);

        let mut request = match method {
            "get" => self.client.get(url),
            "post" => {
                let body = args.get("body").and_then(Value::as_str).unwrap_or("");
                self.client.post(url).body(body.to_string())
            }
            other => {
                return Err(ToolError::InvalidArguments(format!(
                    "unknown method '{other}'"
                )));
            }
        };
        request = request.headers(headers).timeout(timeout);

        let response = request
            .send()
            .await
            .map_err(|e| ToolError::Failed(format!("request to '{url_str}' failed: {e}")))?;

        // Capture status/headers before consuming the body, then enforce the
        // cap before and after reading.
        let status = response.status().as_u16();
        let response_headers = response.headers().clone();
        if let Some(len) = response.content_length() {
            if len as usize > self.max_response_bytes {
                return Err(ToolError::Failed(format!(
                    "response exceeds the {}-byte cap",
                    self.max_response_bytes
                )));
            }
        }
        let body = response
            .bytes()
            .await
            .map_err(|e| ToolError::Failed(format!("reading response body failed: {e}")))?;
        if body.len() > self.max_response_bytes {
            return Err(ToolError::Failed(format!(
                "response exceeds the {}-byte cap",
                self.max_response_bytes
            )));
        }

        let mut headers_json = serde_json::Map::new();
        for (name, value) in &response_headers {
            if let Ok(value_str) = value.to_str() {
                headers_json.insert(name.as_str().to_string(), json!(value_str));
            }
        }

        Ok(json!({
            "status": status,
            "headers": headers_json,
            "body": String::from_utf8_lossy(&body),
            "bytes": body.len(),
        }))
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

    async fn run_tool(tool: &HttpTool, args: Value) -> Result<Value, ToolError> {
        tool.run(&args, &ctx()).await
    }

    fn tool(hosts: &[&str]) -> HttpTool {
        HttpTool::new(
            hosts.iter().map(|h| h.to_string()).collect(),
            DEFAULT_MAX_RESPONSE_BYTES,
        )
    }

    #[tokio::test]
    async fn get_round_trip() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/data")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"hello":"world"}"#)
            .create();
        let host = url::Url::parse(&server.url())
            .unwrap()
            .host_str()
            .unwrap()
            .to_string();
        let http = tool(&[&host]);
        let out = run_tool(
            &http,
            json!({ "method": "get", "url": format!("{}/data", server.url()) }),
        )
        .await
        .unwrap();
        assert_eq!(out["status"], 200);
        assert_eq!(out["body"], r#"{"hello":"world"}"#);
        assert_eq!(out["headers"]["content-type"], "application/json");
        mock.assert();
    }

    #[tokio::test]
    async fn post_sends_body_and_headers() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/submit")
            .match_header("x-token", "abc")
            .match_body("payload")
            .with_status(201)
            .with_body("created")
            .create();
        let host = url::Url::parse(&server.url())
            .unwrap()
            .host_str()
            .unwrap()
            .to_string();
        let http = tool(&[&host]);
        let out = run_tool(
            &http,
            json!({
                "method": "post",
                "url": format!("{}/submit", server.url()),
                "body": "payload",
                "headers": { "x-token": "abc" },
            }),
        )
        .await
        .unwrap();
        assert_eq!(out["status"], 201);
        mock.assert();
    }

    #[tokio::test]
    async fn non_allowlisted_host_denied() {
        let http = tool(&["api.example.com"]);
        let err = run_tool(
            &http,
            json!({ "method": "get", "url": "https://evil.example.org/x" }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "PERMISSION_DENIED");
    }

    #[tokio::test]
    async fn empty_allowlist_denies_everything() {
        let http = tool(&[]);
        let err = run_tool(
            &http,
            json!({ "method": "get", "url": "https://api.example.com/x" }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "PERMISSION_DENIED");
        assert!(err.to_string().contains("no network hosts"));
    }

    #[tokio::test]
    async fn host_normalization_matches() {
        // "API.Example.COM." normalizes to "api.example.com" (lowercase,
        // trailing dot stripped) — so it PASSES the allowlist and proceeds to
        // the transport layer (which then fails on DNS). The point is that it
        // is not denied: the error must be TOOL_FAILED, not PERMISSION_DENIED.
        let http = tool(&["api.example.com"]);
        let err = run_tool(
            &http,
            json!({ "method": "get", "url": "https://API.Example.COM./x" }),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.code(),
            "TOOL_FAILED",
            "normalized host must pass the allowlist"
        );
    }

    #[tokio::test]
    async fn oversized_response_fails_without_truncation() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/big")
            .with_status(200)
            .with_body("x".repeat(200))
            .create();
        let host = url::Url::parse(&server.url())
            .unwrap()
            .host_str()
            .unwrap()
            .to_string();
        let http = HttpTool::new(vec![host], 100);
        let err = run_tool(
            &http,
            json!({ "method": "get", "url": format!("{}/big", server.url()) }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "TOOL_FAILED");
        assert!(err.to_string().contains("cap"));
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        let mut server = mockito::Server::new_async().await;
        let landing = server
            .mock("GET", "/landing")
            .with_status(200)
            .with_body("landed")
            .expect(0)
            .create();
        let _redirect = server
            .mock("GET", "/hop")
            .with_status(302)
            .with_header("location", "/landing")
            .with_body("")
            .create();
        let host = url::Url::parse(&server.url())
            .unwrap()
            .host_str()
            .unwrap()
            .to_string();
        let http = tool(&[&host]);
        let out = run_tool(
            &http,
            json!({ "method": "get", "url": format!("{}/hop", server.url()) }),
        )
        .await
        .unwrap();
        assert_eq!(out["status"], 302, "redirect must not be followed");
        landing.assert(); // expect(0): fails if the redirect WAS followed
    }

    #[tokio::test]
    async fn invalid_url_is_invalid_arguments() {
        let http = tool(&["example.com"]);
        let err = run_tool(&http, json!({ "method": "get", "url": "not a url" }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "TOOL_INVALID_ARGUMENTS");
    }
}
