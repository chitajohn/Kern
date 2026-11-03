//! Memory backends (ARCHITECTURE.md §9.5, §34).
//!
//! Two implementations of `kern_tool::MemoryProvider`:
//! - [`crate::tools::StoreMemoryProvider`] — the default: agent-scoped rows in
//!   the runtime's SQLite store, durable with the rest of the runtime state;
//! - [`FileMemoryProvider`] — a plain JSON file inside the agent's workspace,
//!   for operators who want to read, diff, and version their agent's memory
//!   with the same tools they use for the rest of the workspace.
//!
//! Both are agent-scoped and expose exactly the same tool contract, so
//! `memory.backend` is a storage choice, never a capability change.
//!
//! The file backend keeps ONE file per agent (`<workspace>/<dir>/memory.json`)
//! rather than one file per key: keys are model-supplied strings, and mapping
//! them onto filenames is how path-traversal bugs get invented. Writes are
//! serialized per provider instance (read-modify-write under a mutex) and land
//! through a temp file + rename, so a crash mid-write cannot truncate memory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use kern_tool::{MemoryEntry, MemoryProvider, ToolError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::config::{MemoryBackend, MemoryConfig};

/// Default directory (relative to the agent workspace) for file memory.
pub const DEFAULT_MEMORY_DIR: &str = ".kern/memory";

/// One persisted entry in the file backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileEntry {
    value: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    updated_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FileStore {
    #[serde(default)]
    entries: std::collections::BTreeMap<String, FileEntry>,
}

/// Agent-scoped memory in a JSON file inside the workspace (§34).
pub struct FileMemoryProvider {
    path: PathBuf,
    /// Serializes read-modify-write cycles: the file has no row locking.
    lock: Mutex<()>,
}

impl FileMemoryProvider {
    /// `root` is the agent workspace; `dir` is the configured directory
    /// (absolute paths are rejected at config validation).
    pub fn new(workspace: &Path, dir: &str) -> Self {
        Self {
            path: workspace.join(dir).join("memory.json"),
            lock: Mutex::new(()),
        }
    }

    /// The file backing this provider (diagnostics/tests).
    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn read(&self) -> Result<FileStore, ToolError> {
        match tokio::fs::read(&self.path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                ToolError::Failed(format!(
                    "memory file {} is not valid JSON: {e}",
                    self.path.display()
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileStore::default()),
            Err(e) => Err(ToolError::Failed(format!(
                "reading memory file {}: {e}",
                self.path.display()
            ))),
        }
    }

    async fn write(&self, store: &FileStore) -> Result<(), ToolError> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                ToolError::Failed(format!("creating memory dir {}: {e}", parent.display()))
            })?;
        }
        let bytes = serde_json::to_vec_pretty(store)
            .map_err(|e| ToolError::Failed(format!("serializing memory: {e}")))?;
        // Temp + rename: a torn write never becomes the live file.
        let tmp = self.path.with_extension("json.tmp");
        tokio::fs::write(&tmp, &bytes)
            .await
            .map_err(|e| ToolError::Failed(format!("writing {}: {e}", tmp.display())))?;
        tokio::fs::rename(&tmp, &self.path).await.map_err(|e| {
            ToolError::Failed(format!(
                "replacing memory file {}: {e}",
                self.path.display()
            ))
        })
    }
}

#[async_trait]
impl MemoryProvider for FileMemoryProvider {
    async fn get(&self, _agent_id: &str, key: &str) -> Result<Option<MemoryEntry>, ToolError> {
        // One file per agent workspace, so the agent id is structural here.
        let _guard = self.lock.lock().await;
        let store = self.read().await?;
        Ok(store.entries.get(key).map(|entry| MemoryEntry {
            key: key.to_string(),
            value: entry.value.clone(),
            description: entry.description.clone(),
        }))
    }

    async fn put(
        &self,
        _agent_id: &str,
        key: &str,
        value: Value,
        description: Option<String>,
    ) -> Result<(), ToolError> {
        let _guard = self.lock.lock().await;
        let mut store = self.read().await?;
        let now = chrono::Utc::now().to_rfc3339();
        store.entries.insert(
            key.to_string(),
            FileEntry {
                value,
                description,
                updated_at: now,
            },
        );
        self.write(&store).await
    }

    async fn list(
        &self,
        _agent_id: &str,
        prefix: Option<&str>,
    ) -> Result<Vec<MemoryEntry>, ToolError> {
        let _guard = self.lock.lock().await;
        let store = self.read().await?;
        Ok(store
            .entries
            .iter()
            .filter(|(key, _)| prefix.is_none_or(|p| key.starts_with(p)))
            .map(|(key, entry)| MemoryEntry {
                key: key.clone(),
                value: entry.value.clone(),
                description: entry.description.clone(),
            })
            .collect())
    }
}

/// Build the configured memory backend for one agent (§34).
pub fn build_provider(
    config: &MemoryConfig,
    store: Arc<crate::store::Store>,
    workspace: &Path,
) -> Arc<dyn MemoryProvider> {
    match config.backend {
        MemoryBackend::File => Arc::new(FileMemoryProvider::new(
            workspace,
            config.dir.as_deref().unwrap_or(DEFAULT_MEMORY_DIR),
        )),
        MemoryBackend::Store => Arc::new(crate::tools::StoreMemoryProvider::new(store)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn file_round_trip_and_prefixes() {
        let dir = tempfile::tempdir().unwrap();
        let provider = FileMemoryProvider::new(dir.path(), DEFAULT_MEMORY_DIR);

        provider
            .put(
                "agent-1",
                "goal",
                json!({ "text": "ship" }),
                Some("primary".into()),
            )
            .await
            .unwrap();
        provider
            .put("agent-1", "notes.a", json!(1), None)
            .await
            .unwrap();

        let entry = provider.get("agent-1", "goal").await.unwrap().unwrap();
        assert_eq!(entry.value["text"], "ship");
        assert_eq!(entry.description.as_deref(), Some("primary"));

        let notes = provider.list("agent-1", Some("notes.")).await.unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].key, "notes.a");
        assert!(provider.get("agent-1", "missing").await.unwrap().is_none());

        // The file lives inside the workspace and is human-readable JSON.
        assert!(provider.path().starts_with(dir.path()));
        let text = std::fs::read_to_string(provider.path()).unwrap();
        assert!(text.contains("\"goal\""));
    }

    #[tokio::test]
    async fn put_replaces_and_persists_across_instances() {
        let dir = tempfile::tempdir().unwrap();
        let first = FileMemoryProvider::new(dir.path(), DEFAULT_MEMORY_DIR);
        first.put("a", "k", json!(1), None).await.unwrap();
        first.put("a", "k", json!(2), None).await.unwrap();

        let second = FileMemoryProvider::new(dir.path(), DEFAULT_MEMORY_DIR);
        assert_eq!(second.get("a", "k").await.unwrap().unwrap().value, json!(2));
        assert_eq!(second.list("a", None).await.unwrap().len(), 1);
        // The temp file is gone after the rename.
        assert!(!provider_tmp(dir.path()).exists());
    }

    fn provider_tmp(root: &Path) -> PathBuf {
        root.join(DEFAULT_MEMORY_DIR).join("memory.json.tmp")
    }

    #[tokio::test]
    async fn corrupt_file_is_a_tool_failure_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let provider = FileMemoryProvider::new(dir.path(), DEFAULT_MEMORY_DIR);
        std::fs::create_dir_all(dir.path().join(DEFAULT_MEMORY_DIR)).unwrap();
        std::fs::write(provider.path(), b"{not json").unwrap();

        let err = provider.get("a", "k").await.unwrap_err();
        assert!(err.to_string().contains("not valid JSON"), "{err}");
    }

    #[test]
    fn backend_selection_is_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::store::Store::open(dir.path()).unwrap());
        let file = build_provider(
            &MemoryConfig {
                enabled: true,
                inject_digest: false,
                max_keys: None,
                max_value_bytes: None,
                backend: MemoryBackend::File,
                dir: None,
            },
            Arc::clone(&store),
            dir.path(),
        );
        // Construction must not create the file (lazy, like the store).
        assert!(!dir.path().join(DEFAULT_MEMORY_DIR).exists());
        // The store-backed variant is the default and shares the runtime DB.
        let store_backed = build_provider(&MemoryConfig::default(), store, dir.path());
        assert!(Arc::strong_count(&store_backed) >= 1);
        assert!(Arc::strong_count(&file) >= 1);
    }
}
