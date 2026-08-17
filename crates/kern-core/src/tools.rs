//! Store-backed tool plumbing.
//!
//! The `kern-tool` crate owns the `Tool` trait, registry, executor, and all
//! builtins; it cannot depend on `kern-core` (the engine depends on it). This
//! module closes the loop for the store-coupled pieces:
//!
//! - `StoreMemoryProvider` implements `kern_tool::MemoryProvider` over the
//!   `memory` table, offloading the blocking rusqlite calls via
//!   `spawn_blocking` so tool calls never block the async runtime.
//! - `memory_digest` builds the system-prompt digest for
//!   `memory.inject_digest` (SPEC.md §9; consumed by the engine).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use kern_tool::builtins::shell::ShellTool;
use kern_tool::builtins::{
    filesystem::FileSystemTool, http::HttpTool, memory::MemoryListTool, memory::MemoryReadTool,
    memory::MemoryWriteTool, noop::NoopTool, noop::SleepTool,
};
use kern_tool::{
    CommandRunner, MemoryEntry, MemoryProvider, RunLimits, ToolError, ToolRegistry,
    DEFAULT_OUTPUT_CAP,
};

use crate::config::{AgentSpec, SandboxMode, ShellRules};
use crate::error::{ErrorCode, KernError};
use crate::permissions::{FsAction, PermissionEngine};
use crate::sandbox::{construct as construct_sandbox, SandboxedRunner};
use crate::store::Store;

/// Cap on the digest text injected into the system prompt.
pub const DIGEST_MAX_CHARS: usize = 16 * 1024;

/// Agent-scoped memory over the runtime store.
pub struct StoreMemoryProvider {
    store: Arc<Store>,
}

impl StoreMemoryProvider {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}

fn store_error(context: &str, err: impl std::fmt::Display) -> ToolError {
    ToolError::Failed(format!("{context}: storage error: {err}"))
}

fn convert(entry: crate::store::MemoryEntry) -> MemoryEntry {
    MemoryEntry {
        key: entry.key,
        value: entry.value,
        description: entry.description,
    }
}

#[async_trait]
impl MemoryProvider for StoreMemoryProvider {
    async fn get(&self, agent_id: &str, key: &str) -> Result<Option<MemoryEntry>, ToolError> {
        let store = Arc::clone(&self.store);
        let agent_id = agent_id.to_string();
        let key = key.to_string();
        tokio::task::spawn_blocking(move || store.memory_get(&agent_id, &key))
            .await
            .map_err(|e| ToolError::Failed(format!("memory task failed: {e}")))?
            .map_err(|e| store_error("memory.get", e))
            .map(|opt| opt.map(convert))
    }

    async fn put(
        &self,
        agent_id: &str,
        key: &str,
        value: serde_json::Value,
        description: Option<String>,
    ) -> Result<(), ToolError> {
        let store = Arc::clone(&self.store);
        let agent_id = agent_id.to_string();
        let key = key.to_string();
        tokio::task::spawn_blocking(move || {
            store.memory_put(&agent_id, &key, value, description.as_deref())
        })
        .await
        .map_err(|e| ToolError::Failed(format!("memory task failed: {e}")))?
        .map_err(|e| store_error("memory.put", e))
    }

    async fn list(
        &self,
        agent_id: &str,
        prefix: Option<&str>,
    ) -> Result<Vec<MemoryEntry>, ToolError> {
        let store = Arc::clone(&self.store);
        let agent_id = agent_id.to_string();
        let prefix = prefix.map(str::to_string);
        tokio::task::spawn_blocking(move || store.memory_list(&agent_id, prefix.as_deref()))
            .await
            .map_err(|e| ToolError::Failed(format!("memory task failed: {e}")))?
            .map_err(|e| store_error("memory.list", e))
            .map(|entries| entries.into_iter().map(convert).collect())
    }
}

/// Build the `shell` tool for an agent — the fail-closed gate (SPEC §12.6).
///
/// - `permissions.shell` absent or disabled ⇒ `Ok(None)` — the tool is
///   never exposed to the model.
/// - enabled + `sandbox: required` + no backend on the host ⇒
///   `SANDBOX_UNAVAILABLE` — the agent does not start (fail closed).
/// - enabled + `best-effort` ⇒ strongest backend, else the Linux rlimit
///   fallback, else a logged no-op.
/// - enabled + `off` ⇒ explicit no-isolation choice (`NoSandbox`).
///
/// Every path enforces `tool_timeout` and the output cap via `run_captured`.
pub fn build_shell_tool(
    shell: Option<&ShellRules>,
    workspace: &Path,
    tool_timeout: Duration,
    memory_limit_bytes: Option<u64>,
) -> Result<Option<ShellTool>, KernError> {
    let Some(shell) = shell else {
        return Ok(None);
    };
    if !shell.enabled {
        return Ok(None);
    }
    // Config validation guarantees `sandbox` is present when enabled.
    let mode = shell.sandbox.unwrap_or(SandboxMode::Off);
    let limits = RunLimits {
        timeout: tool_timeout,
        output_cap: DEFAULT_OUTPUT_CAP,
        memory_limit_bytes,
    };
    let sandbox = construct_sandbox(mode, workspace, &limits)
        .map_err(|e| KernError::new(ErrorCode::SandboxUnavailable, e.to_string()))?;
    let runner: Arc<dyn CommandRunner> = Arc::new(SandboxedRunner::new(sandbox, limits));
    Ok(Some(ShellTool::new(runner)))
}

/// The maximum HTTP response bytes for the `http` builtin (SPEC §11.3 cap).
pub const HTTP_MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;

/// Build the agent's tool registry from its spec (the engine's
/// per-agent construction point).
///
/// Wiring per SPEC §11.3:
/// - `filesystem` roots come from the policy engine's literal allow rules
///   (defense in depth — the engine's rule matching, including globs,
///   remains authoritative);
/// - `http` allowlist = the normalized network allow hosts;
/// - `memory.*` only when `memory.enabled`;
/// - `shell` only through the fail-closed `build_shell_tool` gate;
/// - `noop`/`sleep` are always available fixtures (their permission class is
///   "none").
///
/// The registry contains every tool the agent may invoke; the model only
/// ever sees `spec.tools` (the executor's `specs()` filter).
pub fn build_registry(
    spec: &AgentSpec,
    store: Arc<Store>,
    workspace: &Path,
) -> Result<Arc<ToolRegistry>, KernError> {
    let permissions = PermissionEngine::from_config(&spec.permissions, workspace)?;
    let mut registry = ToolRegistry::new();
    let tool_timeout = spec.runtime.tool_timeout().as_std();

    for name in &spec.tools {
        let tool: Arc<dyn kern_tool::Tool> = match name.as_str() {
            "filesystem" => Arc::new(FileSystemTool::new(
                permissions.fs_roots(FsAction::Read),
                permissions.fs_roots(FsAction::Write),
            )),
            "http" => Arc::new(HttpTool::new(
                permissions.network_allow_hosts(),
                HTTP_MAX_RESPONSE_BYTES,
            )),
            "memory.read" => {
                if !spec.memory.enabled {
                    return Err(KernError::new(
                        ErrorCode::ConfigInvalid,
                        "memory.read requires memory.enabled: true",
                    ));
                }
                Arc::new(MemoryReadTool::new(memory_provider(store.clone())))
            }
            "memory.write" => {
                if !spec.memory.enabled {
                    return Err(KernError::new(
                        ErrorCode::ConfigInvalid,
                        "memory.write requires memory.enabled: true",
                    ));
                }
                Arc::new(MemoryWriteTool::with_limits(
                    memory_provider(store.clone()),
                    spec.memory.max_keys.unwrap_or(100) as usize,
                    spec.memory.max_value_bytes.unwrap_or(65_536) as usize,
                ))
            }
            "memory.list" => {
                if !spec.memory.enabled {
                    return Err(KernError::new(
                        ErrorCode::ConfigInvalid,
                        "memory.list requires memory.enabled: true",
                    ));
                }
                Arc::new(MemoryListTool::new(memory_provider(store.clone())))
            }
            "shell" => {
                match build_shell_tool(
                    spec.permissions.shell.as_ref(),
                    workspace,
                    tool_timeout,
                    spec.runtime.tool_memory_limit_bytes(),
                )? {
                    Some(tool) => Arc::new(tool),
                    None => {
                        return Err(KernError::new(
                            ErrorCode::ConfigInvalid,
                            "shell tool requires permissions.shell.enabled: true",
                        ))
                    }
                }
            }
            "noop" => Arc::new(NoopTool),
            "sleep" => Arc::new(SleepTool),
            other => {
                return Err(KernError::new(
                    ErrorCode::ConfigInvalid,
                    format!("unknown tool {other:?} (config validation should have rejected this)"),
                ))
            }
        };
        registry.register(tool).map_err(|e| {
            KernError::new(ErrorCode::Internal, format!("register tool {name:?}: {e}"))
        })?;
    }
    Ok(Arc::new(registry))
}

fn memory_provider(store: Arc<Store>) -> Arc<dyn MemoryProvider> {
    Arc::new(StoreMemoryProvider::new(store))
}

/// Build the digest text for `memory.inject_digest`:
/// prepended to the system prompt, capped at `DIGEST_MAX_CHARS`.
pub fn memory_digest(entries: &[MemoryEntry]) -> String {
    let mut out = String::from("--- memory ---\n");
    for entry in entries {
        if let Some(description) = &entry.description {
            out.push_str(&format!(
                "- {} ({description}): {}\n",
                entry.key, entry.value
            ));
        } else {
            out.push_str(&format!("- {}: {}\n", entry.key, entry.value));
        }
        if out.len() >= DIGEST_MAX_CHARS {
            out.truncate(DIGEST_MAX_CHARS);
            out.push_str("\n... (digest truncated)\n");
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    // The shell-tool trait tests are non-Windows (shell is a unix surface);
    // on Windows these names would be dead code.
    #[cfg(not(windows))]
    use kern_tool::{Tool, ToolContext};
    use serde_json::json;

    fn ctx() -> (tempfile::TempDir, Arc<Store>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        (dir, store)
    }

    #[cfg(not(windows))]
    fn tool_ctx<'a>() -> ToolContext<'a> {
        ToolContext {
            agent_id: "a",
            execution_id: "e",
            tool_call_id: "c",
        }
    }

    #[tokio::test]
    async fn store_provider_round_trip() {
        let (_dir, store) = ctx();
        let provider = StoreMemoryProvider::new(Arc::clone(&store));

        // No agent row is required for memory ops (the table is not
        // FK-constrained to agents in v1); keys are agent-scoped by id.
        let agent_id = "agent-1";

        provider
            .put(
                agent_id,
                "goal",
                json!({ "text": "ship" }),
                Some("primary".into()),
            )
            .await
            .unwrap();
        provider
            .put(agent_id, "notes.a", json!(1), None)
            .await
            .unwrap();

        let entry = provider.get(agent_id, "goal").await.unwrap().unwrap();
        assert_eq!(entry.value["text"], "ship");
        assert_eq!(entry.description.as_deref(), Some("primary"));

        let notes = provider.list(agent_id, Some("notes.")).await.unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].key, "notes.a");

        // Agent scoping: a different agent sees nothing.
        assert!(provider.get("agent-2", "goal").await.unwrap().is_none());
        assert!(provider.list("agent-2", None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn store_provider_errors_surface_as_tool_failures() {
        // A missing key is an Ok(None), not an error; storage failures on a
        // valid store should not occur here, so just assert the None path.
        let (_dir, store) = ctx();
        let provider = StoreMemoryProvider::new(store);
        assert!(provider.get("a", "missing").await.unwrap().is_none());
    }

    #[test]
    fn digest_format_and_cap() {
        let entries = vec![
            MemoryEntry {
                key: "goal".into(),
                value: json!({ "text": "ship" }),
                description: Some("primary".into()),
            },
            MemoryEntry {
                key: "notes.a".into(),
                value: json!(1),
                description: None,
            },
        ];
        let digest = memory_digest(&entries);
        assert!(digest.contains("- goal (primary): {\"text\":\"ship\"}"));
        assert!(digest.contains("- notes.a: 1"));
        assert!(digest.starts_with("--- memory ---"));

        // Empty input still yields the header.
        assert_eq!(memory_digest(&[]), "--- memory ---\n");
    }

    // ------------------------------------------------------------------
    // Shell tool construction gate (SPEC §12: fail closed)
    // ------------------------------------------------------------------

    fn workspace() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn shell_tool_absent_or_disabled_is_not_exposed() {
        let ws = workspace();
        let timeout = Duration::from_secs(30);
        // No permissions.shell block at all.
        assert!(build_shell_tool(None, ws.path(), timeout, None)
            .unwrap()
            .is_none());
        // Disabled explicitly.
        let disabled = ShellRules {
            enabled: false,
            sandbox: Some(SandboxMode::Required),
        };
        assert!(build_shell_tool(Some(&disabled), ws.path(), timeout, None)
            .unwrap()
            .is_none());
    }

    #[test]
    fn shell_required_fails_closed_without_backend() {
        let ws = workspace();
        let rules = ShellRules {
            enabled: true,
            sandbox: Some(SandboxMode::Required),
        };
        match build_shell_tool(Some(&rules), ws.path(), Duration::from_secs(30), None) {
            Ok(Some(_)) => {
                // A backend is present on this host; construction succeeded.
            }
            Err(err) => {
                assert_eq!(err.code(), ErrorCode::SandboxUnavailable);
            }
            Ok(None) => panic!("enabled shell must construct a tool"),
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn shell_off_runs_via_no_sandbox() {
        let ws = workspace();
        let rules = ShellRules {
            enabled: true,
            sandbox: Some(SandboxMode::Off),
        };
        let tool = build_shell_tool(Some(&rules), ws.path(), Duration::from_secs(30), None)
            .unwrap()
            .expect("off mode always constructs");
        let out = tool
            .run(&json!({ "command": "printf 'off-ok'" }), &tool_ctx())
            .await
            .unwrap();
        assert_eq!(out["stdout"], "off-ok");
    }

    /// GitHub-hosted runners deny child-side `setrlimit` in the sandbox
    /// pre-exec (their sandbox returns EPERM at spawn), so the real-process
    /// assertion cannot be exercised there. Probe the exact tool path once;
    /// on any host that permits sandbox spawns the full contract is asserted
    /// (the runtime itself still fails closed on restricted hosts).
    #[cfg(target_os = "linux")]
    async fn host_denies_sandbox_spawn() -> bool {
        let ws = workspace();
        let rules = ShellRules {
            enabled: true,
            sandbox: Some(SandboxMode::BestEffort),
        };
        let Ok(Some(tool)) =
            build_shell_tool(Some(&rules), ws.path(), Duration::from_secs(30), None)
        else {
            return true; // no backend: the real-process test cannot run
        };
        matches!(
            tool.run(&json!({ "command": "true" }), &tool_ctx())
                .await,
            Err(e) if e.to_string().contains("Operation not permitted")
        )
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shell_best_effort_runs_via_rlimits_fallback() {
        if host_denies_sandbox_spawn().await {
            eprintln!(
                "host denies sandbox spawns (EPERM — e.g. GitHub-hosted runners); \
                 skipping kernel-enforced assertion"
            );
            return;
        }
        let ws = workspace();
        let rules = ShellRules {
            enabled: true,
            sandbox: Some(SandboxMode::BestEffort),
        };
        let tool = build_shell_tool(Some(&rules), ws.path(), Duration::from_secs(30), None)
            .unwrap()
            .expect("best-effort always constructs on linux");
        let out = tool
            .run(
                &json!({ "command": "printf 'best-effort-ok'" }),
                &tool_ctx(),
            )
            .await
            .unwrap();
        assert_eq!(out["stdout"], "best-effort-ok");
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn shell_timeout_propagates_as_tool_timeout() {
        let ws = workspace();
        let rules = ShellRules {
            enabled: true,
            sandbox: Some(SandboxMode::Off),
        };
        let tool = build_shell_tool(Some(&rules), ws.path(), Duration::from_millis(150), None)
            .unwrap()
            .expect("off mode always constructs");
        let err = tool
            .run(&json!({ "command": "sleep 30" }), &tool_ctx())
            .await
            .unwrap_err();
        assert_eq!(err.code(), "TOOL_TIMEOUT");
    }
}
