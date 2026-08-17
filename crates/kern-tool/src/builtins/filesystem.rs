//! `filesystem` builtin (SPEC.md §11.3).
//!
//! Input: `{ action: read|write|list|stat, path, content? }`.
//!
//! Security model (defense in depth — the permission engine re-enforces this
//! at the policy layer):
//! - **Root containment:** every resolved path MUST be inside one of the
//!   configured read/write roots. Relative roots are resolved against the
//!   daemon's current working directory at construction.
//! - **Canonicalization:** the target is canonicalized (symlinks followed,
//!   `.`/`..` resolved). A symlink inside a root that points outside
//!   canonicalizes to the outside target and is denied.
//! - **Write to a new path:** the parent directory is canonicalized and the
//!   final component appended, so a not-yet-existing file cannot escape via a
//!   symlinked parent.
//!
//! Honest limitations: canonicalize-then-use has a TOCTOU window (a path can
//! be swapped for a symlink between the check and the I/O); the permission
//! engine and the OS sandbox are the stronger layers. Text
//! files only in v0.1 (binary read/write errors clearly).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::{Tool, ToolContext};

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": { "type": "string", "enum": ["read", "write", "list", "stat"] },
            "path": { "type": "string" },
            "content": { "type": "string" }
        },
        "required": ["action", "path"],
        "additionalProperties": false
    })
}

pub struct FileSystemTool {
    read_roots: Vec<PathBuf>,
    write_roots: Vec<PathBuf>,
}

impl FileSystemTool {
    pub fn new(read_roots: Vec<PathBuf>, write_roots: Vec<PathBuf>) -> Self {
        Self {
            read_roots: absolutize(read_roots),
            write_roots: absolutize(write_roots),
        }
    }
}

/// Resolve relative roots against the daemon cwd once, at construction.
fn absolutize(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    roots
        .into_iter()
        .map(|root| {
            if root.is_absolute() {
                root
            } else {
                std::env::current_dir()
                    .map(|cwd| cwd.join(&root))
                    .unwrap_or(root)
            }
        })
        .collect()
}

/// Resolve `path` to an absolute canonical path contained in one of `roots`.
///
/// Relative paths resolve against the FIRST root — the agent's primary
/// workspace — so `out.txt` means `workspace/out.txt`, never the daemon's cwd.
/// Canonicalization (symlink-following, `..` resolution) is shared with the
/// permission engine via `crate::path` so the two layers cannot drift.
fn resolve_within(path: &str, roots: &[PathBuf]) -> Result<PathBuf, ToolError> {
    if roots.is_empty() {
        return Err(ToolError::PermissionDenied(format!(
            "no filesystem roots are allowed for this agent (path '{path}')"
        )));
    }
    let raw = Path::new(path);
    let abs = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        roots[0].join(raw)
    };
    let resolved = crate::path::canonicalize_path(&abs).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ToolError::Failed(format!("path does not exist: {}", abs.display()))
        } else {
            ToolError::Failed(format!("canonicalize {}: {e}", abs.display()))
        }
    })?;
    for root in roots {
        if crate::path::is_within(&resolved, &crate::path::canonical_root(root)) {
            return Ok(resolved);
        }
    }
    Err(ToolError::PermissionDenied(format!(
        "path '{}' is outside the allowed roots",
        path
    )))
}

#[async_trait]
impl Tool for FileSystemTool {
    fn name(&self) -> &str {
        "filesystem"
    }

    fn description(&self) -> &str {
        "Read, write, list, and stat files inside the agent's allowed workspace."
    }

    fn input_schema(&self) -> &Value {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(schema)
    }

    async fn run(&self, args: &Value, _ctx: &ToolContext<'_>) -> Result<Value, ToolError> {
        let action = args["action"].as_str().unwrap_or_default();
        let path = args["path"].as_str().unwrap_or_default();

        match action {
            "read" => {
                let target = resolve_within(path, &self.read_roots)?;
                let content = std::fs::read_to_string(&target)
                    .map_err(|e| ToolError::Failed(format!("read {}: {e}", target.display())))?;
                Ok(json!({ "content": content, "path": target.display().to_string() }))
            }
            "write" => {
                let content = args.get("content").and_then(Value::as_str).ok_or_else(|| {
                    ToolError::InvalidArguments(
                        "write requires a string 'content' field".to_string(),
                    )
                })?;
                let target = resolve_within(path, &self.write_roots)?;
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        ToolError::Failed(format!(
                            "create parent dirs for {}: {e}",
                            target.display()
                        ))
                    })?;
                }
                std::fs::write(&target, content)
                    .map_err(|e| ToolError::Failed(format!("write {}: {e}", target.display())))?;
                Ok(json!({
                    "ok": true,
                    "path": target.display().to_string(),
                    "bytes": content.len(),
                }))
            }
            "list" => {
                let target = resolve_within(path, &self.read_roots)?;
                let mut entries = Vec::new();
                for entry in std::fs::read_dir(&target)
                    .map_err(|e| ToolError::Failed(format!("list {}: {e}", target.display())))?
                {
                    let entry = entry.map_err(|e| ToolError::Failed(format!("list: {e}")))?;
                    let file_type = entry.file_type().map_err(|e| {
                        ToolError::Failed(format!("stat {}: {e}", entry.path().display()))
                    })?;
                    entries.push(json!({
                        "name": entry.file_name().to_string_lossy(),
                        "type": if file_type.is_dir() { "dir" } else { "file" },
                    }));
                }
                entries.sort_by_key(|e| e["name"].as_str().unwrap_or_default().to_string());
                Ok(json!({
                    "path": target.display().to_string(),
                    "entries": entries,
                }))
            }
            "stat" => {
                let target = resolve_within(path, &self.read_roots)?;
                let meta = std::fs::metadata(&target)
                    .map_err(|e| ToolError::Failed(format!("stat {}: {e}", target.display())))?;
                let modified_ms = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64);
                Ok(json!({
                    "path": target.display().to_string(),
                    "size": meta.len(),
                    "is_file": meta.is_file(),
                    "is_dir": meta.is_dir(),
                    "modified_ms": modified_ms,
                }))
            }
            other => Err(ToolError::InvalidArguments(format!(
                "unknown action '{other}'"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    fn ctx<'a>() -> ToolContext<'a> {
        ToolContext {
            agent_id: "a",
            execution_id: "e",
            tool_call_id: "c",
        }
    }

    async fn run_tool(tool: &FileSystemTool, args: Value) -> Result<Value, ToolError> {
        tool.run(&args, &ctx()).await
    }

    fn tool_with(dir: &Path) -> FileSystemTool {
        FileSystemTool::new(vec![dir.to_path_buf()], vec![dir.to_path_buf()])
    }

    #[tokio::test]
    async fn write_read_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tool_with(tmp.path());

        let write = run_tool(
            &tool,
            json!({ "action": "write", "path": "out.txt", "content": "hello" }),
        )
        .await
        .unwrap();
        assert_eq!(write["ok"], true);
        assert_eq!(write["bytes"], 5);

        let read = run_tool(&tool, json!({ "action": "read", "path": "out.txt" }))
            .await
            .unwrap();
        assert_eq!(read["content"], "hello");
        assert!(tmp.path().join("out.txt").exists());
    }

    #[tokio::test]
    async fn nested_write_creates_parents() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tool_with(tmp.path());
        run_tool(
            &tool,
            json!({ "action": "write", "path": "a/b/c.txt", "content": "x" }),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a/b/c.txt")).unwrap(),
            "x"
        );
    }

    #[tokio::test]
    async fn parent_traversal_denied() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tool_with(tmp.path());
        let outside = tmp.path().parent().unwrap().join("escape-target");
        std::fs::write(&outside, "no").unwrap();

        for path in [
            "../escape-target",
            "../../escape-target",
            "sub/../../../escape-target",
        ] {
            let err = run_tool(&tool, json!({ "action": "read", "path": path }))
                .await
                .unwrap_err();
            assert_eq!(err.code(), "PERMISSION_DENIED", "path {path:?}");
        }
        let _ = std::fs::remove_file(&outside);
    }

    #[tokio::test]
    async fn absolute_path_outside_root_denied() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tool_with(tmp.path());
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("x.txt");
        std::fs::write(&target, "no").unwrap();

        let err = run_tool(
            &tool,
            json!({ "action": "read", "path": target.display().to_string() }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "PERMISSION_DENIED");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escape_denied() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tool_with(tmp.path());

        // Symlink inside the root pointing outside the root.
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "secret").unwrap();
        symlink(&secret, tmp.path().join("leak")).unwrap();

        let err = run_tool(&tool, json!({ "action": "read", "path": "leak" }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "PERMISSION_DENIED");

        // A symlink that stays inside the root is fine.
        std::fs::write(tmp.path().join("real.txt"), "ok").unwrap();
        symlink(tmp.path().join("real.txt"), tmp.path().join("link")).unwrap();
        let read = run_tool(&tool, json!({ "action": "read", "path": "link" }))
            .await
            .unwrap();
        assert_eq!(read["content"], "ok");
    }

    #[tokio::test]
    async fn list_and_stat() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tool_with(tmp.path());
        std::fs::write(tmp.path().join("a.txt"), "aaa").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();

        let list = run_tool(&tool, json!({ "action": "list", "path": "." }))
            .await
            .unwrap();
        let entries = list["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["name"], "a.txt");
        assert_eq!(entries[0]["type"], "file");
        assert_eq!(entries[1]["name"], "sub");
        assert_eq!(entries[1]["type"], "dir");

        let stat = run_tool(&tool, json!({ "action": "stat", "path": "a.txt" }))
            .await
            .unwrap();
        assert_eq!(stat["size"], 3);
        assert_eq!(stat["is_file"], true);
        assert_eq!(stat["is_dir"], false);
    }

    #[tokio::test]
    async fn write_requires_content() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tool_with(tmp.path());
        let err = run_tool(&tool, json!({ "action": "write", "path": "x.txt" }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "TOOL_INVALID_ARGUMENTS");
    }

    #[tokio::test]
    async fn unknown_action_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tool_with(tmp.path());
        let err = run_tool(&tool, json!({ "action": "delete", "path": "x" }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "TOOL_INVALID_ARGUMENTS");
    }

    #[tokio::test]
    async fn missing_file_reads_fail_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tool_with(tmp.path());
        let err = run_tool(&tool, json!({ "action": "read", "path": "nope.txt" }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "TOOL_FAILED");
        assert!(err.to_string().contains("nope.txt"));
    }
}
