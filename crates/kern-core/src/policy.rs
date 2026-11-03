//! Call classification: tool name + arguments → policy decision (§10).
//!
//! This is the one place a tool call is mapped onto the permission engine's
//! rule classes. The execution engine and the MCP serving surface both use it,
//! so "what policy applies to this call" cannot drift between the two ways a
//! tool can be reached.

use std::path::Path;

use serde_json::Value;

use crate::config::AgentSpec;
use crate::permissions::{Decision, Effect, FsAction, KeyAction, PermissionEngine};

/// The per-call policy outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallDecision {
    Allow,
    /// Needs an operator decision; only a live runner can park for one.
    Ask,
    Deny {
        reason: String,
    },
}

/// A classified call: decision plus the resource/action strings that events
/// and error messages report.
#[derive(Debug, Clone)]
pub struct Classification {
    pub decision: CallDecision,
    pub resource: String,
    pub action: String,
}

/// Classify a validated call against the agent's policy.
pub fn classify(
    spec: &AgentSpec,
    permissions: &PermissionEngine,
    tool: &str,
    args: &Value,
) -> Classification {
    fn decide(d: &Decision) -> CallDecision {
        match d.effect {
            Effect::Allow => CallDecision::Allow,
            Effect::Ask => CallDecision::Ask,
            Effect::Deny => CallDecision::Deny {
                reason: d.reason.clone(),
            },
        }
    }
    fn out(decision: CallDecision, resource: impl Into<String>, action: &str) -> Classification {
        Classification {
            decision,
            resource: resource.into(),
            action: action.to_string(),
        }
    }

    match tool {
        "filesystem" => {
            let action = args.get("action").and_then(Value::as_str).unwrap_or("");
            let path = args.get("path").and_then(Value::as_str).unwrap_or("");
            let is_write = action == "write";
            let fs_action = if is_write {
                FsAction::Write
            } else {
                FsAction::Read
            };
            let d = permissions.evaluate_path(Path::new(path), fs_action);
            out(decide(&d), path, if is_write { "write" } else { "read" })
        }
        "http" => {
            let url = args.get("url").and_then(Value::as_str).unwrap_or("");
            match host_port(url) {
                Some(hostport) => {
                    let d = permissions.evaluate_host(&hostport);
                    out(decide(&d), hostport, "network")
                }
                None => out(
                    CallDecision::Deny {
                        reason: format!("invalid URL {url:?}"),
                    },
                    url,
                    "network",
                ),
            }
        }
        "memory.read" | "memory.list" => {
            let key = args
                .get("key")
                .and_then(Value::as_str)
                .or_else(|| args.get("prefix").and_then(Value::as_str))
                .unwrap_or("*");
            let d = permissions.evaluate_key(key, KeyAction::Read);
            out(decide(&d), key, "read")
        }
        "memory.write" => {
            let key = args.get("key").and_then(Value::as_str).unwrap_or("");
            let d = permissions.evaluate_key(key, KeyAction::Write);
            out(decide(&d), key, "write")
        }
        "shell" => {
            if permissions.shell_allowed() {
                out(CallDecision::Allow, "shell", "shell")
            } else {
                out(
                    CallDecision::Deny {
                        reason: "shell is disabled for this agent".to_string(),
                    },
                    "shell",
                    "shell",
                )
            }
        }
        _ if spec.mcp.is_mcp_tool(tool) => {
            let d = permissions.evaluate_mcp(tool);
            out(decide(&d), tool, "mcp")
        }
        // A delegation is allowed exactly when it is declared. The child's own
        // calls are classified against the same permission engine, so this
        // grant cannot widen what the delegation may actually do (§35).
        _ if tool.starts_with("agent.") => {
            let declared = spec
                .subagents
                .iter()
                .any(|s| crate::subagents::tool_name(s) == tool);
            if declared {
                out(CallDecision::Allow, tool, "agent")
            } else {
                out(
                    CallDecision::Deny {
                        reason: format!("sub-agent {tool:?} is not declared for this agent"),
                    },
                    tool,
                    "agent",
                )
            }
        }
        "noop" | "sleep" => out(CallDecision::Allow, tool, "none"),
        other => out(
            CallDecision::Deny {
                reason: format!("tool {other:?} is not configured for this agent"),
            },
            other,
            "none",
        ),
    }
}

/// The `host[:port]` form the permission engine matches against, with the
/// default port filled from the URL scheme (so `https://api.github.com`
/// becomes `api.github.com:443` and a `api.github.com:443` rule matches).
/// Returns `None` for unparseable or host-less URLs. IPv6 literals keep
/// host-only form: a v6+port request only matches port-less rules
/// (fail-closed; documented limitation in ARCHITECTURE.md).
pub fn host_port(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    match parsed.port_or_known_default() {
        Some(port) if !host.contains(':') => Some(format!("{host}:{port}")),
        _ => Some(host.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::PermissionEngine;
    use serde_json::json;

    fn spec() -> AgentSpec {
        crate::config::parse_agent_spec(
            "version: 1\nname: classifier\nmodel:\n  provider: mock\n  model: m\n\
             tools: [filesystem, http, noop]\n\
             permissions:\n  filesystem:\n    read:\n      allow: [\"./ws/**\"]\n    write:\n      allow: [\"./ws/out/**\"]\n\
             \n  network:\n    allow: [\"api.example.com\"]\n",
        )
        .unwrap()
    }

    fn permissions(spec: &AgentSpec) -> PermissionEngine {
        PermissionEngine::from_config(&spec.permissions, Path::new(".")).unwrap()
    }

    #[test]
    fn filesystem_rules_split_read_and_write() {
        let spec = spec();
        // Path rules resolve against the workspace, and resolution requires an
        // existing ancestor, so the granted root must exist on disk.
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join("ws")).unwrap();
        std::fs::write(workspace.path().join("ws/a.txt"), b"x").unwrap();
        let p = PermissionEngine::from_config(&spec.permissions, workspace.path()).unwrap();
        // Absolute paths are classified as-is (relative ones anchor to the
        // first allowed root, which is a different question).
        let file = workspace.path().join("ws/a.txt");
        let file = file.to_string_lossy().to_string();
        let read = classify(
            &spec,
            &p,
            "filesystem",
            &json!({ "action": "read", "path": file }),
        );
        assert_eq!(read.decision, CallDecision::Allow);
        assert_eq!(read.action, "read");

        // The write rule grants only ./ws/out/**, so the same file is denied
        // for writing.
        let write = classify(
            &spec,
            &p,
            "filesystem",
            &json!({ "action": "write", "path": file }),
        );
        assert!(
            matches!(write.decision, CallDecision::Deny { .. }),
            "{write:?}"
        );
    }

    #[test]
    fn network_rules_use_the_default_port() {
        let spec = spec();
        let p = permissions(&spec);
        let allowed = classify(
            &spec,
            &p,
            "http",
            &json!({ "method": "get", "url": "https://api.example.com/x" }),
        );
        assert_eq!(allowed.decision, CallDecision::Allow);
        assert_eq!(allowed.resource, "api.example.com:443");

        let denied = classify(
            &spec,
            &p,
            "http",
            &json!({ "method": "get", "url": "https://evil.example.org/x" }),
        );
        assert!(matches!(denied.decision, CallDecision::Deny { .. }));
    }

    #[test]
    fn unknown_tools_and_disabled_shell_are_denied() {
        let spec = spec();
        let p = permissions(&spec);
        let unknown = classify(&spec, &p, "ghost", &json!({}));
        assert!(matches!(unknown.decision, CallDecision::Deny { .. }));

        let shell = classify(&spec, &p, "shell", &json!({ "command": "true" }));
        assert!(matches!(shell.decision, CallDecision::Deny { .. }));

        let noop = classify(&spec, &p, "noop", &json!({}));
        assert_eq!(noop.decision, CallDecision::Allow);
    }
}
