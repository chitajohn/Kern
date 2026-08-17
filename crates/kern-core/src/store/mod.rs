//! Durable store: SQLite-backed persistence (ARCHITECTURE.md §3.3, SPEC.md §5).
//!
//! Schema migrations, atomic multi-write transactions, CRUD for
//! every domain entity, the `daemon.lock` single-owner guard, integrity checks
//! on open, and corruption quarantine.
//!
//! The store is synchronous (`rusqlite`); the async facade (writer task /
//! `spawn_blocking`) keeps the async runtime non-blocking. One daemon owns a data
//! dir; a second daemon is refused with `STORAGE_LOCKED`. The runtime never
//! silently re-creates or overwrites a corrupted database.

mod migration;
pub mod model;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::types::Type;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;

use crate::error::{ErrorCode, KernError, Result};
use crate::fault::FaultInjector;
use crate::version::KERN_VERSION;

pub use model::*;

/// Database file name inside the data dir.
pub const DB_FILE: &str = "state.db";

/// Single-owner lock file name inside the data dir.
pub const LOCK_FILE: &str = "daemon.lock";

/// Generate a new v4 UUID string, used for entity ids.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Map a rusqlite error onto the structured taxonomy (SPEC.md §13).
pub(crate) fn map_sqlite_error(err: rusqlite::Error, ctx: &str) -> KernError {
    match &err {
        rusqlite::Error::SqliteFailure(e, _) if e.code == rusqlite::ErrorCode::NotADatabase => {
            KernError::new(
                ErrorCode::StorageCorruption,
                format!("{ctx}: file is not a database"),
            )
        }
        rusqlite::Error::SqliteFailure(e, _)
            if (e.extended_code & 0xff) == rusqlite::ffi::SQLITE_CORRUPT =>
        {
            KernError::new(
                ErrorCode::StorageCorruption,
                format!("{ctx}: database disk image is malformed"),
            )
        }
        rusqlite::Error::SqliteFailure(e, msg)
            if e.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            let msg = msg.as_deref().unwrap_or("");
            if e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE {
                if msg.contains("agents.name") {
                    return KernError::new(
                        ErrorCode::AgentNameTaken,
                        format!("{ctx}: agent name already exists"),
                    );
                }
                if msg.contains("executions") {
                    return KernError::new(
                        ErrorCode::ExecutionAlreadyActive,
                        format!("{ctx}: an execution is already active for this agent"),
                    );
                }
            }
            KernError::new(
                ErrorCode::StorageFailure,
                format!("{ctx}: constraint violation: {msg}"),
            )
        }
        _ => KernError::new(ErrorCode::StorageFailure, format!("{ctx}: {err}")),
    }
}

/// A single-owner, durable store over one SQLite database file.
pub struct Store {
    data_dir: PathBuf,
    db_path: PathBuf,
    writer: Mutex<Connection>,
    reader: Mutex<Connection>,
    _lock: File,
    /// Deterministic fault injection (`crate::fault`): `None` in
    /// production — every instrumented write then costs one `Option` check.
    /// Tests construct this via [`Store::open_with_faults`].
    faults: Option<std::sync::Arc<FaultInjector>>,
}

impl Store {
    /// Open (or create) the store for `data_dir`, acquiring the exclusive
    /// `daemon.lock`. Fails with `STORAGE_LOCKED` if another daemon owns it,
    /// and with `STORAGE_CORRUPTION` if the database fails its integrity check
    /// (the corrupted file is quarantined, never recreated).
    pub fn open(data_dir: &Path) -> Result<Self> {
        Self::open_with_faults(data_dir, None)
    }

    /// [`Store::open`] with a deterministic fault injector — test
    /// infrastructure only: scripts can fail individual
    /// persisted-write boundaries at chosen occurrence counts. Not part of
    /// the public API; the daemon and every production path use [`Store::open`].
    #[doc(hidden)]
    pub fn open_with_faults(
        data_dir: &Path,
        faults: Option<std::sync::Arc<FaultInjector>>,
    ) -> Result<Self> {
        std::fs::create_dir_all(data_dir).map_err(|e| {
            KernError::new(
                ErrorCode::StorageFailure,
                format!("cannot create data dir {}: {e}", data_dir.display()),
            )
        })?;

        let lock_file = File::options()
            .create(true)
            .write(true)
            .truncate(false)
            .open(data_dir.join(LOCK_FILE))
            .map_err(|e| {
                KernError::new(
                    ErrorCode::StorageFailure,
                    format!("cannot open {LOCK_FILE}: {e}"),
                )
            })?;

        match lock_file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(KernError::new(
                    ErrorCode::StorageLocked,
                    format!(
                        "data dir {} is locked by another daemon",
                        data_dir.display()
                    ),
                ));
            }
            Err(e) => {
                return Err(KernError::new(
                    ErrorCode::StorageFailure,
                    format!("cannot lock {LOCK_FILE}: {e}"),
                ));
            }
        }

        let db_path = data_dir.join(DB_FILE);
        let mut writer =
            Connection::open(&db_path).map_err(|e| map_sqlite_error(e, "open store"))?;

        if let Err(e) = writer.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;",
        ) {
            let mapped = map_sqlite_error(e, "configure store");
            if mapped.code() == ErrorCode::StorageCorruption {
                quarantine_corrupt(&db_path);
            }
            return Err(mapped);
        }

        let healthy = matches!(
            writer.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0)),
            Ok(s) if s == "ok"
        );
        if !healthy {
            quarantine_corrupt(&db_path);
            return Err(KernError::new(
                ErrorCode::StorageCorruption,
                format!("{} failed its integrity check", db_path.display()),
            ));
        }

        migration::migrate(&mut writer)?;

        writer
            .execute(
                "INSERT INTO kern_meta (key, value) VALUES ('runtime_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![KERN_VERSION],
            )
            .map_err(|e| map_sqlite_error(e, "record runtime version"))?;

        // Local-first security: the database holds agent configs,
        // tool results, memory, and event history — a copied DB is a data
        // leak. Restrict the DB, WAL, and lock files to the owning user
        // where the platform supports it (SQLite creates files with the
        // umask, which is commonly group/world readable).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for name in [
                DB_FILE.to_string(),
                format!("{DB_FILE}-wal"),
                format!("{DB_FILE}-shm"),
                LOCK_FILE.to_string(),
            ] {
                let path = data_dir.join(name);
                if let Ok(metadata) = std::fs::metadata(&path) {
                    let mut perms = metadata.permissions();
                    perms.set_mode(0o600);
                    let _ = std::fs::set_permissions(&path, perms);
                }
            }
        }

        let reader =
            Connection::open(&db_path).map_err(|e| map_sqlite_error(e, "open store reader"))?;
        reader
            .execute_batch("PRAGMA query_only=ON; PRAGMA busy_timeout=5000;")
            .map_err(|e| map_sqlite_error(e, "configure store reader"))?;

        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            db_path,
            writer: Mutex::new(writer),
            reader: Mutex::new(reader),
            _lock: lock_file,
            faults,
        })
    }

    /// Entry hook for the deterministic fault injector: fails the
    /// operation with a structured `STORAGE_FAILURE` when the configured
    /// script says this occurrence of `point` must fail. No-op in production
    /// (`faults` is `None`).
    #[inline]
    fn inject(&self, point: &str) -> Result<()> {
        if let Some(faults) = &self.faults {
            if let Some(err) = faults.try_fail(point) {
                return Err(err);
            }
        }
        Ok(())
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Run `f` against the writer connection. Each CRUD method is itself one
    /// autocommit transaction.
    fn with_writer<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let guard = self.writer.lock().expect("store writer mutex poisoned");
        f(&guard)
    }

    fn with_reader<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let guard = self.reader.lock().expect("store reader mutex poisoned");
        f(&guard)
    }

    /// Execute multiple writes atomically. Any `Err` returned by `f` rolls back
    /// the whole transaction. This is the primitive lifecycle transitions use;
    /// until then it is exercised only by the test suite.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn tx<T>(
        &self,
        f: impl FnOnce(&mut rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut guard = self.writer.lock().expect("store writer mutex poisoned");
        let mut tx = guard
            .transaction()
            .map_err(|e| map_sqlite_error(e, "begin transaction"))?;
        match f(&mut tx) {
            Ok(value) => {
                tx.commit()
                    .map_err(|e| map_sqlite_error(e, "commit transaction"))?;
                Ok(value)
            }
            Err(err) => {
                drop(tx); // rollback
                Err(err)
            }
        }
    }

    /// Atomically persist a lifecycle transition (`SPEC.md §3.2`): the agent's
    /// state update (conditionally guarded by `expected_state`, so a concurrent
    /// transition cannot double-apply), an optional execution update, and the
    /// appended events — all in one transaction. Returns the appended events
    /// with their durable seqs.
    ///
    /// Fails with `INVALID_TRANSITION` when the agent is missing or not in
    /// `expected_state`, and with `EXECUTION_NOT_FOUND` when an execution
    /// update targets a missing execution (both roll the whole transition
    /// back).
    pub(crate) fn transition(&self, t: &Transition) -> Result<Vec<Event>> {
        self.inject("transition")?;
        let ts = now_rfc3339();
        let ts_parsed = parse_ts(&ts).map_err(|e| map_sqlite_error(e, "transition timestamp"))?;
        self.tx(|tx| {
            let changed = tx
                .execute(
                    "UPDATE agents SET lifecycle_state = ?1, last_error = ?2, updated_at = ?3
                     WHERE id = ?4 AND lifecycle_state = ?5",
                    params![
                        t.new_state.as_str(),
                        t.last_error,
                        ts,
                        t.agent_id,
                        t.expected_state.as_str()
                    ],
                )
                .map_err(|e| map_sqlite_error(e, "transition agent"))?;
            if changed == 0 {
                return Err(KernError::new(
                    ErrorCode::InvalidTransition,
                    format!(
                        "agent {} is not in state {} (concurrent transition?)",
                        t.agent_id,
                        t.expected_state.as_str()
                    ),
                ));
            }

            if let Some(ex) = &t.execution {
                let changed = tx
                    .execute(
                        "UPDATE executions SET status = ?1, started_at = ?2, finished_at = ?3
                         WHERE id = ?4 AND agent_id = ?5",
                        params![
                            ex.status.as_str(),
                            opt_ts(ex.started_at),
                            opt_ts(ex.finished_at),
                            ex.id,
                            t.agent_id
                        ],
                    )
                    .map_err(|e| map_sqlite_error(e, "transition execution"))?;
                if changed == 0 {
                    return Err(KernError::new(
                        ErrorCode::ExecutionNotFound,
                        format!("execution {} for agent {} not found", ex.id, t.agent_id),
                    ));
                }
            }

            let mut events = Vec::with_capacity(t.events.len());
            for record in &t.events {
                let payload = serde_json::to_string(&record.payload)
                    .map_err(|e| KernError::internal(format!("serialize event payload: {e}")))?;
                tx.execute(
                    "INSERT INTO events (ts, kind, agent_id, execution_id, payload)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![ts, record.kind, t.agent_id, record.execution_id, payload],
                )
                .map_err(|e| map_sqlite_error(e, "transition event"))?;
                let seq = tx.last_insert_rowid();
                events.push(Event {
                    seq,
                    ts: ts_parsed,
                    kind: record.kind.to_string(),
                    agent_id: Some(t.agent_id.clone()),
                    execution_id: record.execution_id.clone(),
                    payload: record.payload.clone(),
                });
            }
            Ok(events)
        })
    }

    // ------------------------------------------------------------------
    // Agents
    // ------------------------------------------------------------------

    pub fn create_agent(&self, agent: &Agent) -> Result<()> {
        let config = serde_json::to_string(&agent.config)
            .map_err(|e| KernError::internal(format!("serialize agent config: {e}")))?;
        self.with_writer(|conn| {
            conn.execute(
                "INSERT INTO agents (id, name, spec_version, config_json, lifecycle_state, created_at, updated_at, last_error, auto_recover, next_run_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    agent.id,
                    agent.name,
                    agent.spec_version as i64,
                    config,
                    agent.state.as_str(),
                    ts(agent.created_at),
                    ts(agent.updated_at),
                    agent.last_error,
                    agent.auto_recover as i64,
                    opt_ts(agent.next_run_at),
                ],
            )
            .map_err(|e| map_sqlite_error(e, "create agent"))?;
            Ok(())
        })
    }

    pub fn get_agent(&self, id: &str) -> Result<Agent> {
        self.with_reader(|conn| {
            conn.query_row(
                "SELECT id, name, spec_version, config_json, lifecycle_state, created_at, updated_at, last_error, auto_recover, next_run_at
                 FROM agents WHERE id = ?1",
                params![id],
                agent_from_row,
            )
            .optional()
            .map_err(|e| map_sqlite_error(e, "get agent"))?
            .ok_or_else(|| KernError::new(ErrorCode::AgentNotFound, format!("agent {id} not found")))
        })
    }

    pub fn get_agent_by_name(&self, name: &str) -> Result<Option<Agent>> {
        self.with_reader(|conn| {
            conn.query_row(
                "SELECT id, name, spec_version, config_json, lifecycle_state, created_at, updated_at, last_error, auto_recover, next_run_at
                 FROM agents WHERE name = ?1",
                params![name],
                agent_from_row,
            )
            .optional()
            .map_err(|e| map_sqlite_error(e, "get agent by name"))
        })
    }

    pub fn list_agents(&self) -> Result<Vec<Agent>> {
        self.with_reader(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, name, spec_version, config_json, lifecycle_state, created_at, updated_at, last_error, auto_recover, next_run_at
                     FROM agents ORDER BY name",
                )
                .map_err(|e| map_sqlite_error(e, "list agents"))?;
            let rows = stmt
                .query_map([], agent_from_row)
                .map_err(|e| map_sqlite_error(e, "list agents"))?;
            collect(rows, "list agents")
        })
    }

    pub fn update_agent(&self, agent: &Agent) -> Result<()> {
        let config = serde_json::to_string(&agent.config)
            .map_err(|e| KernError::internal(format!("serialize agent config: {e}")))?;
        self.with_writer(|conn| {
            conn.execute(
                "UPDATE agents SET spec_version = ?1, config_json = ?2, lifecycle_state = ?3, updated_at = ?4,
                        last_error = ?5, auto_recover = ?6, next_run_at = ?7 WHERE id = ?8",
                params![
                    agent.spec_version as i64,
                    config,
                    agent.state.as_str(),
                    ts(agent.updated_at),
                    agent.last_error,
                    agent.auto_recover as i64,
                    opt_ts(agent.next_run_at),
                    agent.id,
                ],
            )
            .map_err(|e| map_sqlite_error(e, "update agent"))?;
            Ok(())
        })
    }

    /// Maintain the scheduler's `next_run_at` (SPEC.md §13).
    pub fn set_next_run_at(&self, agent_id: &str, next: Option<DateTime<Utc>>) -> Result<()> {
        self.with_writer(|conn| {
            conn.execute(
                "UPDATE agents SET next_run_at = ?1, updated_at = ?2 WHERE id = ?3",
                params![opt_ts(next), now_rfc3339(), agent_id],
            )
            .map_err(|e| map_sqlite_error(e, "set next run"))?;
            Ok(())
        })
    }

    pub fn delete_agent(&self, id: &str) -> Result<()> {
        self.with_writer(|conn| {
            conn.execute("DELETE FROM agents WHERE id = ?1", params![id])
                .map_err(|e| map_sqlite_error(e, "delete agent"))?;
            Ok(())
        })
    }

    // ------------------------------------------------------------------
    // Executions
    // ------------------------------------------------------------------

    pub fn create_execution(&self, execution: &Execution) -> Result<()> {
        self.with_writer(|conn| {
            conn.execute(
                "INSERT INTO executions (id, agent_id, status, started_at, finished_at, latest_checkpoint_id, input, wake_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    execution.id,
                    execution.agent_id,
                    execution.status.as_str(),
                    opt_ts(execution.started_at),
                    opt_ts(execution.finished_at),
                    execution.latest_checkpoint_id,
                    execution.input,
                    opt_ts(execution.wake_at),
                ],
            )
            .map_err(|e| map_sqlite_error(e, "create execution"))?;
            Ok(())
        })
    }

    pub fn get_execution(&self, id: &str) -> Result<Execution> {
        self.with_reader(|conn| {
            conn.query_row(
                "SELECT id, agent_id, status, started_at, finished_at, latest_checkpoint_id, input, wake_at FROM executions WHERE id = ?1",
                params![id],
                execution_from_row,
            )
            .optional()
            .map_err(|e| map_sqlite_error(e, "get execution"))?
            .ok_or_else(|| KernError::new(ErrorCode::ExecutionNotFound, format!("execution {id} not found")))
        })
    }

    pub fn list_executions_for_agent(&self, agent_id: &str) -> Result<Vec<Execution>> {
        self.with_reader(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, agent_id, status, started_at, finished_at, latest_checkpoint_id, input, wake_at
                     FROM executions WHERE agent_id = ?1 ORDER BY started_at DESC",
                )
                .map_err(|e| map_sqlite_error(e, "list executions"))?;
            let rows = stmt
                .query_map(params![agent_id], execution_from_row)
                .map_err(|e| map_sqlite_error(e, "list executions"))?;
            collect(rows, "list executions")
        })
    }

    /// Set (or clear) the execution's durable wake time (schema v4). Called
    /// when an agent parks for a durable sleep and when it wakes.
    pub fn set_wake_at(&self, execution_id: &str, wake_at: Option<DateTime<Utc>>) -> Result<()> {
        self.inject("set_wake_at")?;
        self.with_writer(|conn| {
            conn.execute(
                "UPDATE executions SET wake_at = ?1 WHERE id = ?2",
                params![opt_ts(wake_at), execution_id],
            )
            .map_err(|e| map_sqlite_error(e, "set wake at"))?;
            Ok(())
        })
    }

    /// Active executions of `sleeping` agents whose wake time has passed
    /// (durable wake/sleep). A missed wake collapses: it fires once.
    pub fn list_sleeping_due(&self, now: DateTime<Utc>) -> Result<Vec<Execution>> {
        self.with_reader(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT e.id, e.agent_id, e.status, e.started_at, e.finished_at,
                            e.latest_checkpoint_id, e.input, e.wake_at
                     FROM executions e
                     JOIN agents a ON a.id = e.agent_id
                     WHERE a.lifecycle_state = 'sleeping'
                       AND e.wake_at IS NOT NULL AND e.wake_at <= ?1
                       AND e.status IN ('pending', 'running')
                     ORDER BY e.wake_at ASC",
                )
                .map_err(|e| map_sqlite_error(e, "list sleeping due"))?;
            let rows = stmt
                .query_map(params![ts(now)], execution_from_row)
                .map_err(|e| map_sqlite_error(e, "list sleeping due"))?;
            collect(rows, "list sleeping due")
        })
    }

    /// The soonest future wake time across all sleeping executions (used by
    /// the scheduler's timer cadence). `None` when nothing is parked.
    pub fn soonest_wake_at(&self) -> Result<Option<DateTime<Utc>>> {
        self.with_reader(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT MIN(e.wake_at) FROM executions e
                     JOIN agents a ON a.id = e.agent_id
                     WHERE a.lifecycle_state = 'sleeping'
                       AND e.wake_at IS NOT NULL
                       AND e.status IN ('pending', 'running')",
                )
                .map_err(|e| map_sqlite_error(e, "soonest wake at"))?;
            let raw: Option<String> = stmt
                .query_row([], |row| row.get(0))
                .map_err(|e| map_sqlite_error(e, "soonest wake at"))?;
            Ok(raw.and_then(|s| {
                DateTime::parse_from_rfc3339(&s)
                    .ok()
                    .map(|d| d.with_timezone(&Utc))
            }))
        })
    }

    /// Fail an execution that never started (`pending` → `failed`). The
    /// release valve for a run that died before its first lifecycle
    /// transition: the partial `ux_executions_one_active` index
    /// treats a lingering `pending` row as an active execution and would
    /// refuse every future run of the agent with `EXECUTION_ALREADY_ACTIVE`.
    /// Returns `false` when the row was already terminal (nothing to do).
    pub fn fail_pending_execution(&self, execution_id: &str) -> Result<bool> {
        self.with_writer(|conn| {
            let changed = conn
                .execute(
                    "UPDATE executions SET status = 'failed', finished_at = ?1
                     WHERE id = ?2 AND status = 'pending'",
                    params![now_rfc3339(), execution_id],
                )
                .map_err(|e| map_sqlite_error(e, "fail pending execution"))?;
            Ok(changed > 0)
        })
    }

    pub fn update_execution(&self, execution: &Execution) -> Result<()> {
        self.with_writer(|conn| {
            conn.execute(
                "UPDATE executions SET status = ?1, started_at = ?2, finished_at = ?3, latest_checkpoint_id = ?4, wake_at = ?5 WHERE id = ?6",
                params![
                    execution.status.as_str(),
                    opt_ts(execution.started_at),
                    opt_ts(execution.finished_at),
                    execution.latest_checkpoint_id,
                    opt_ts(execution.wake_at),
                    execution.id,
                ],
            )
            .map_err(|e| map_sqlite_error(e, "update execution"))?;
            Ok(())
        })
    }

    // ------------------------------------------------------------------
    // Events
    // ------------------------------------------------------------------

    /// Append an event and return it with its durable, monotonic `seq`.
    pub fn append_event(
        &self,
        kind: &str,
        agent_id: Option<&str>,
        execution_id: Option<&str>,
        payload: Value,
    ) -> Result<Event> {
        self.inject("append_event")?;
        let ts = now_rfc3339();
        let payload_str = serde_json::to_string(&payload)
            .map_err(|e| KernError::internal(format!("serialize event payload: {e}")))?;
        self.with_writer(|conn| {
            conn.execute(
                "INSERT INTO events (ts, kind, agent_id, execution_id, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![ts, kind, agent_id, execution_id, payload_str],
            )
            .map_err(|e| map_sqlite_error(e, "append event"))?;
            let seq = conn.last_insert_rowid();
            Ok(Event {
                seq,
                ts: parse_ts(&ts).map_err(|e| map_sqlite_error(e, "append event"))?,
                kind: kind.to_string(),
                agent_id: agent_id.map(str::to_string),
                execution_id: execution_id.map(str::to_string),
                payload,
            })
        })
    }

    /// Replay events with `seq > after_seq`, oldest first.
    pub fn events_after(&self, after_seq: i64, limit: usize) -> Result<Vec<Event>> {
        self.with_reader(|conn| {
            let mut stmt = conn
                .prepare("SELECT seq, ts, kind, agent_id, execution_id, payload FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2")
                .map_err(|e| map_sqlite_error(e, "replay events"))?;
            let rows = stmt
                .query_map(params![after_seq, limit as i64], event_from_row)
                .map_err(|e| map_sqlite_error(e, "replay events"))?;
            collect(rows, "replay events")
        })
    }

    /// Number of persisted events for an agent (retention/size-warning support,
    /// `SPEC.md §6`).
    pub fn event_count_for_agent(&self, agent_id: &str) -> Result<i64> {
        self.with_reader(|conn| {
            conn.query_row(
                "SELECT count(*) FROM events WHERE agent_id = ?1",
                params![agent_id],
                |row| row.get(0),
            )
            .map_err(|e| map_sqlite_error(e, "count agent events"))
        })
    }

    /// Delete everything but the newest `per_agent` events per agent — the
    /// one bounded-growth knob for event history (opt-in via
    /// `KERN_EVENT_RETENTION`, unbounded by default). Runtime
    /// events with no agent are bucketed under NULL and pruned the same way.
    /// Returns the number of rows deleted. The newest `keep` events of each
    /// agent survive, so replay/recovery always has the live tail.
    pub fn prune_events(&self, per_agent: usize) -> Result<usize> {
        let keep = per_agent.max(1) as i64;
        let deleted = self.with_writer(|conn| {
            conn.execute(
                "DELETE FROM events WHERE seq IN (
                    SELECT seq FROM (
                        SELECT seq, ROW_NUMBER() OVER (PARTITION BY agent_id ORDER BY seq DESC) AS rn
                        FROM events
                    ) WHERE rn > ?1
                )",
                params![keep],
            )
            .map_err(|e| map_sqlite_error(e, "prune events"))
        })?;
        Ok(deleted)
    }

    /// Highest persisted event `seq` so far (`0` when the store has no events).
    /// This is the "live" cursor: a new subscriber can replay from it and then
    /// switch to the broadcast stream without missing or duplicating events.
    pub fn latest_event_seq(&self) -> Result<i64> {
        self.with_reader(|conn| {
            conn.query_row("SELECT COALESCE(max(seq), 0) FROM events", [], |row| {
                row.get(0)
            })
            .map_err(|e| map_sqlite_error(e, "latest event seq"))
        })
    }

    /// Replay events for one agent with `seq > after_seq`, oldest first.
    pub fn events_for_agent_after(
        &self,
        agent_id: &str,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<Event>> {
        self.with_reader(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT seq, ts, kind, agent_id, execution_id, payload FROM events
                     WHERE agent_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
                )
                .map_err(|e| map_sqlite_error(e, "replay agent events"))?;
            let rows = stmt
                .query_map(params![agent_id, after_seq, limit as i64], event_from_row)
                .map_err(|e| map_sqlite_error(e, "replay agent events"))?;
            collect(rows, "replay agent events")
        })
    }

    /// Replay events for one execution with `seq > after_seq`, oldest first
    /// (the transcript's ordered record, `SPEC.md §15.1`).
    pub fn events_for_execution_after(
        &self,
        execution_id: &str,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<Event>> {
        self.with_reader(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT seq, ts, kind, agent_id, execution_id, payload FROM events
                     WHERE execution_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
                )
                .map_err(|e| map_sqlite_error(e, "replay execution events"))?;
            let rows = stmt
                .query_map(
                    params![execution_id, after_seq, limit as i64],
                    event_from_row,
                )
                .map_err(|e| map_sqlite_error(e, "replay execution events"))?;
            collect(rows, "replay execution events")
        })
    }

    // ------------------------------------------------------------------
    // Tool calls
    // ------------------------------------------------------------------

    pub fn record_tool_call(&self, call: &ToolCall) -> Result<()> {
        self.inject("record_tool_call")?;
        let args = serde_json::to_string(&call.args)
            .map_err(|e| KernError::internal(format!("serialize tool args: {e}")))?;
        self.with_writer(|conn| {
            conn.execute(
                "INSERT INTO tool_calls (id, agent_id, execution_id, tool_name, args_json, status, result_json, error_json, started_at, finished_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    call.id,
                    call.agent_id,
                    call.execution_id,
                    call.tool_name,
                    args,
                    call.status.as_str(),
                    opt_json(call.result.as_ref()),
                    opt_json(call.error.as_ref()),
                    opt_ts(call.started_at),
                    opt_ts(call.finished_at),
                ],
            )
            .map_err(|e| map_sqlite_error(e, "record tool call"))?;
            Ok(())
        })
    }

    pub fn get_tool_call(&self, execution_id: &str, id: &str) -> Result<Option<ToolCall>> {
        self.with_reader(|conn| {
            conn.query_row(
                "SELECT id, agent_id, execution_id, tool_name, args_json, status, result_json, error_json, started_at, finished_at
                 FROM tool_calls WHERE execution_id = ?1 AND id = ?2",
                params![execution_id, id],
                tool_call_from_row,
            )
            .optional()
            .map_err(|e| map_sqlite_error(e, "get tool call"))
        })
    }

    pub fn update_tool_call(&self, call: &ToolCall) -> Result<()> {
        self.inject("update_tool_call")?;
        self.with_writer(|conn| {
            conn.execute(
                "UPDATE tool_calls SET status = ?1, result_json = ?2, error_json = ?3, started_at = ?4, finished_at = ?5
                 WHERE execution_id = ?6 AND id = ?7",
                params![
                    call.status.as_str(),
                    opt_json(call.result.as_ref()),
                    opt_json(call.error.as_ref()),
                    opt_ts(call.started_at),
                    opt_ts(call.finished_at),
                    call.execution_id,
                    call.id,
                ],
            )
            .map_err(|e| map_sqlite_error(e, "update tool call"))?;
            Ok(())
        })
    }

    /// All tool calls for an execution in recording order (the dedup replay order).
    pub fn tool_calls_for_execution(&self, execution_id: &str) -> Result<Vec<ToolCall>> {
        self.with_reader(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, agent_id, execution_id, tool_name, args_json, status, result_json, error_json, started_at, finished_at
                     FROM tool_calls WHERE execution_id = ?1 ORDER BY rowid",
                )
                .map_err(|e| map_sqlite_error(e, "list tool calls"))?;
            let rows = stmt
                .query_map(params![execution_id], tool_call_from_row)
                .map_err(|e| map_sqlite_error(e, "list tool calls"))?;
            collect(rows, "list tool calls")
        })
    }

    // ------------------------------------------------------------------
    // State variables (execution-scoped)
    // ------------------------------------------------------------------

    pub fn set_variable(
        &self,
        agent_id: &str,
        execution_id: &str,
        key: &str,
        value: Value,
    ) -> Result<()> {
        self.inject("set_variable")?;
        let now = now_rfc3339();
        let value_str = serde_json::to_string(&value)
            .map_err(|e| KernError::internal(format!("serialize variable: {e}")))?;
        self.with_writer(|conn| {
            conn.execute(
                "INSERT INTO state_variables (agent_id, execution_id, key, value, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(agent_id, key) DO UPDATE SET value = excluded.value, execution_id = excluded.execution_id, updated_at = excluded.updated_at",
                params![agent_id, execution_id, key, value_str, now],
            )
            .map_err(|e| map_sqlite_error(e, "set variable"))?;
            Ok(())
        })
    }

    pub fn get_variable(&self, agent_id: &str, key: &str) -> Result<Option<Value>> {
        self.with_reader(|conn| {
            conn.query_row(
                "SELECT value FROM state_variables WHERE agent_id = ?1 AND key = ?2",
                params![agent_id, key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| map_sqlite_error(e, "get variable"))?
            .map(|s| {
                serde_json::from_str(&s)
                    .map_err(|e| KernError::internal(format!("deserialize variable: {e}")))
            })
            .transpose()
        })
    }

    pub fn list_variables(&self, agent_id: &str) -> Result<Vec<(String, Value)>> {
        self.with_reader(|conn| {
            let mut stmt = conn
                .prepare("SELECT key, value FROM state_variables WHERE agent_id = ?1 ORDER BY key")
                .map_err(|e| map_sqlite_error(e, "list variables"))?;
            let rows = stmt
                .query_map(params![agent_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        serde_json::from_str::<Value>(&row.get::<_, String>(1)?).map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(1, Type::Text, Box::new(e))
                        })?,
                    ))
                })
                .map_err(|e| map_sqlite_error(e, "list variables"))?;
            collect(rows, "list variables")
        })
    }

    // ------------------------------------------------------------------
    // Memory (agent-scoped, survives executions)
    // ------------------------------------------------------------------

    pub fn memory_put(
        &self,
        agent_id: &str,
        key: &str,
        value: Value,
        description: Option<&str>,
    ) -> Result<()> {
        self.inject("memory_put")?;
        let now = now_rfc3339();
        let value_str = serde_json::to_string(&value)
            .map_err(|e| KernError::internal(format!("serialize memory value: {e}")))?;
        self.with_writer(|conn| {
            conn.execute(
                "INSERT INTO memory (agent_id, key, value, description, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                 ON CONFLICT(agent_id, key) DO UPDATE SET value = excluded.value, description = excluded.description, updated_at = excluded.updated_at",
                params![agent_id, key, value_str, description, now],
            )
            .map_err(|e| map_sqlite_error(e, "memory put"))?;
            Ok(())
        })
    }

    pub fn memory_get(&self, agent_id: &str, key: &str) -> Result<Option<MemoryEntry>> {
        self.with_reader(|conn| {
            conn.query_row(
                "SELECT agent_id, key, value, description, created_at, updated_at FROM memory WHERE agent_id = ?1 AND key = ?2",
                params![agent_id, key],
                memory_from_row,
            )
            .optional()
            .map_err(|e| map_sqlite_error(e, "memory get"))
        })
    }

    /// List memory entries for an agent, optionally filtered by key prefix
    /// (globs in `%`/`_` are matched literally).
    pub fn memory_list(&self, agent_id: &str, prefix: Option<&str>) -> Result<Vec<MemoryEntry>> {
        self.with_reader(|conn| {
            let sql = match prefix {
                Some(_) => {
                    "SELECT agent_id, key, value, description, created_at, updated_at FROM memory
                     WHERE agent_id = ?1 AND key LIKE ?2 ESCAPE '\\' ORDER BY key"
                }
                None => {
                    "SELECT agent_id, key, value, description, created_at, updated_at FROM memory
                     WHERE agent_id = ?1 ORDER BY key"
                }
            };
            let mut stmt = conn
                .prepare(sql)
                .map_err(|e| map_sqlite_error(e, "memory list"))?;
            let rows = match prefix {
                Some(prefix) => stmt
                    .query_map(params![agent_id, like_pattern(prefix)], memory_from_row)
                    .map_err(|e| map_sqlite_error(e, "memory list"))?,
                None => stmt
                    .query_map(params![agent_id], memory_from_row)
                    .map_err(|e| map_sqlite_error(e, "memory list"))?,
            };
            collect(rows, "memory list")
        })
    }

    pub fn memory_delete(&self, agent_id: &str, key: &str) -> Result<bool> {
        self.with_writer(|conn| {
            let affected = conn
                .execute(
                    "DELETE FROM memory WHERE agent_id = ?1 AND key = ?2",
                    params![agent_id, key],
                )
                .map_err(|e| map_sqlite_error(e, "memory delete"))?;
            Ok(affected > 0)
        })
    }

    // ------------------------------------------------------------------
    // Permission requests
    // ------------------------------------------------------------------

    /// Default approval window for `ask` requests (SPEC.md §10: 300 s).
    pub const DEFAULT_ASK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

    pub fn create_permission_request(
        &self,
        agent_id: &str,
        tool_call_id: Option<&str>,
        resource: &str,
        action: &str,
    ) -> Result<PermissionRequest> {
        self.create_permission_request_with_ttl(
            agent_id,
            tool_call_id,
            resource,
            action,
            Self::DEFAULT_ASK_TIMEOUT,
        )
    }

    /// Create a request with an explicit approval window. The engine passes
    /// the agent's `runtime.ask_timeout`; after the window closes the request
    /// is expired (never decidable, never parked on forever).
    pub fn create_permission_request_with_ttl(
        &self,
        agent_id: &str,
        tool_call_id: Option<&str>,
        resource: &str,
        action: &str,
        ask_timeout: std::time::Duration,
    ) -> Result<PermissionRequest> {
        self.inject("create_permission_request")?;
        let id = new_id();
        let now = now_rfc3339();
        // Approval TTL: the operator's window closes at `requested_at +
        // ask_timeout`; the engine expires overdue pending requests.
        let expires_at = (Utc::now()
            + chrono::Duration::from_std(ask_timeout)
                .expect("ask_timeout is a bounded config value"))
        .to_rfc3339();
        self.with_writer(|conn| {
            conn.execute(
                "INSERT INTO permission_requests (id, agent_id, tool_call_id, resource, action, status, requested_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7)",
                params![id, agent_id, tool_call_id, resource, action, now, expires_at],
            )
            .map_err(|e| map_sqlite_error(e, "create permission request"))?;
            Ok(())
        })?;
        self.get_permission_request(&id)
    }

    pub fn get_permission_request(&self, id: &str) -> Result<PermissionRequest> {
        self.with_reader(|conn| {
            conn.query_row(
                "SELECT id, agent_id, tool_call_id, resource, action, status, requested_at, decided_at, expires_at FROM permission_requests WHERE id = ?1",
                params![id],
                permission_from_row,
            )
            .optional()
            .map_err(|e| map_sqlite_error(e, "get permission request"))?
            .ok_or_else(|| {
                KernError::new(ErrorCode::PermissionRequestNotFound, format!("permission request {id} not found"))
            })
        })
    }

    /// Decide a permission request **once**. A request is only decidable in
    /// the `pending` state *and inside its expiry window* (compare-and-swap
    /// on the status column): a stale or replayed decision can never flip an
    /// earlier one, so a `deny` can never be overwritten by a late `grant`,
    /// and a decision on an expired request is rejected
    /// (`PERMISSION_REQUEST_EXPIRED`) — the human's window has closed.
    /// Replaying the *same* decision is idempotent (returns the current
    /// row); a conflicting decision is `PERMISSION_REQUEST_ALREADY_DECIDED`.
    pub fn decide_permission_request(&self, id: &str, granted: bool) -> Result<PermissionRequest> {
        self.inject("decide_permission_request")?;
        let target = if granted { "granted" } else { "denied" };
        let now = now_rfc3339();
        let changed = self.with_writer(|conn| {
            conn.execute(
                "UPDATE permission_requests SET status = ?1, decided_at = ?2
                 WHERE id = ?3 AND status = 'pending' AND (expires_at IS NULL OR expires_at > ?2)",
                params![target, now, id],
            )
            .map_err(|e| map_sqlite_error(e, "decide permission request"))
        })?;
        if changed == 0 {
            let current = self.get_permission_request(id)?;
            let current_status = match current.status {
                PermissionStatus::Granted => "granted",
                PermissionStatus::Denied => "denied",
                PermissionStatus::Pending => "pending",
                PermissionStatus::Expired => "expired",
            };
            if current_status == target {
                // Idempotent replay of the same decision.
                return Ok(current);
            }
            if current_status == "pending" {
                // Still pending but the CAS refused us: the window closed in
                // the race between read and write. Seal it expired so the
                // engine never re-parks on it, then report the conflict.
                self.expire_permission_request(id)?;
                return Err(KernError::new(
                    ErrorCode::PermissionRequestExpired,
                    format!(
                        "permission request {id} expired before the decision could be \
                         recorded; the agent has already been told it was denied"
                    ),
                ));
            }
            return Err(KernError::new(
                ErrorCode::PermissionRequestAlreadyDecided,
                format!(
                    "permission request {id} was already decided as {current_status}; \
                     a conflicting decision is rejected"
                ),
            ));
        }
        self.get_permission_request(id)
    }

    /// Mark an overdue pending request `expired` (CAS on `pending` + expiry
    /// window). Idempotent: already-decided, already-expired, or not-yet-
    /// overdue requests are untouched (`Ok(false)`). This is the engine's
    /// poll primitive — the human's window closing is what un-parks a
    /// waiting agent as a denial, never a hang.
    pub fn expire_permission_request(&self, id: &str) -> Result<bool> {
        let changed = self.with_writer(|conn| {
            conn.execute(
                "UPDATE permission_requests SET status = 'expired', decided_at = ?1
                 WHERE id = ?2 AND status = 'pending' AND expires_at IS NOT NULL AND expires_at <= ?1",
                params![now_rfc3339(), id],
            )
            .map_err(|e| map_sqlite_error(e, "expire permission request"))
        })?;
        Ok(changed > 0)
    }

    pub fn pending_permission_requests(&self) -> Result<Vec<PermissionRequest>> {
        self.with_reader(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, agent_id, tool_call_id, resource, action, status, requested_at, decided_at, expires_at
                     FROM permission_requests WHERE status = 'pending' ORDER BY requested_at",
                )
                .map_err(|e| map_sqlite_error(e, "list pending permission requests"))?;
            let rows = stmt
                .query_map([], permission_from_row)
                .map_err(|e| map_sqlite_error(e, "list pending permission requests"))?;
            collect(rows, "list pending permission requests")
        })
    }

    /// Pending requests of one agent (recovery leaves such agents parked for
    /// manual resume — they are waiting on a human, not a crash).
    pub fn pending_permission_requests_for_agent(
        &self,
        agent_id: &str,
    ) -> Result<Vec<PermissionRequest>> {
        self.with_reader(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, agent_id, tool_call_id, resource, action, status, requested_at, decided_at, expires_at
                     FROM permission_requests WHERE agent_id = ?1 AND status = 'pending' ORDER BY requested_at",
                )
                .map_err(|e| map_sqlite_error(e, "list pending permission requests for agent"))?;
            let rows = stmt
                .query_map(params![agent_id], permission_from_row)
                .map_err(|e| map_sqlite_error(e, "list pending permission requests for agent"))?;
            collect(rows, "list pending permission requests for agent")
        })
    }

    /// Decided (granted or denied) permission requests for an agent — the
    /// The engine's resume path re-reads these after a decision lands.
    pub fn decided_permission_requests_for_agent(
        &self,
        agent_id: &str,
    ) -> Result<Vec<PermissionRequest>> {
        self.with_reader(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, agent_id, tool_call_id, resource, action, status, requested_at, decided_at, expires_at
                     FROM permission_requests WHERE agent_id = ?1 AND status != 'pending' ORDER BY requested_at",
                )
                .map_err(|e| map_sqlite_error(e, "list decided permission requests"))?;
            let rows = stmt
                .query_map([agent_id], permission_from_row)
                .map_err(|e| map_sqlite_error(e, "list decided permission requests"))?;
            collect(rows, "list decided permission requests")
        })
    }

    // ------------------------------------------------------------------
    // Checkpoints
    // ------------------------------------------------------------------

    pub fn create_checkpoint(&self, checkpoint: &Checkpoint) -> Result<()> {
        let payload = serde_json::to_string(&checkpoint.payload)
            .map_err(|e| KernError::internal(format!("serialize checkpoint payload: {e}")))?;
        self.with_writer(|conn| {
            conn.execute(
                "INSERT INTO checkpoints (id, agent_id, execution_id, parent_id, format_version, seq, payload, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    checkpoint.id,
                    checkpoint.agent_id,
                    checkpoint.execution_id,
                    checkpoint.parent_id,
                    checkpoint.format_version as i64,
                    checkpoint.seq,
                    payload,
                    ts(checkpoint.created_at),
                ],
            )
            .map_err(|e| map_sqlite_error(e, "create checkpoint"))?;
            Ok(())
        })
    }

    pub fn get_checkpoint(&self, id: &str) -> Result<Checkpoint> {
        self.with_reader(|conn| {
            conn.query_row(
                "SELECT id, agent_id, execution_id, parent_id, format_version, seq, payload, created_at FROM checkpoints WHERE id = ?1",
                params![id],
                checkpoint_from_row,
            )
            .optional()
            .map_err(|e| map_sqlite_error(e, "get checkpoint"))?
            .ok_or_else(|| KernError::new(ErrorCode::CheckpointNotFound, format!("checkpoint {id} not found")))
        })
    }

    pub fn latest_checkpoint(&self, agent_id: &str) -> Result<Option<Checkpoint>> {
        self.with_reader(|conn| {
            conn.query_row(
                "SELECT id, agent_id, execution_id, parent_id, format_version, seq, payload, created_at FROM checkpoints
                 WHERE agent_id = ?1 ORDER BY seq DESC LIMIT 1",
                params![agent_id],
                checkpoint_from_row,
            )
            .optional()
            .map_err(|e| map_sqlite_error(e, "latest checkpoint"))
        })
    }

    pub fn list_checkpoints(&self, agent_id: &str, limit: usize) -> Result<Vec<Checkpoint>> {
        self.with_reader(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, agent_id, execution_id, parent_id, format_version, seq, payload, created_at FROM checkpoints
                     WHERE agent_id = ?1 ORDER BY seq DESC LIMIT ?2",
                )
                .map_err(|e| map_sqlite_error(e, "list checkpoints"))?;
            let rows = stmt
                .query_map(params![agent_id, limit as i64], checkpoint_from_row)
                .map_err(|e| map_sqlite_error(e, "list checkpoints"))?;
            collect(rows, "list checkpoints")
        })
    }

    /// The newest checkpoint of a specific execution (recovery scopes restores
    /// to the interrupted execution, never a sibling run).
    pub fn latest_checkpoint_for_execution(
        &self,
        agent_id: &str,
        execution_id: &str,
    ) -> Result<Option<Checkpoint>> {
        self.with_reader(|conn| {
            conn.query_row(
                "SELECT id, agent_id, execution_id, parent_id, format_version, seq, payload, created_at FROM checkpoints
                 WHERE agent_id = ?1 AND execution_id = ?2 ORDER BY seq DESC LIMIT 1",
                params![agent_id, execution_id],
                checkpoint_from_row,
            )
            .optional()
            .map_err(|e| map_sqlite_error(e, "latest checkpoint for execution"))
        })
    }

    /// Insert a checkpoint, link it to the execution, append the
    /// `checkpoint.created` event, and prune to `retention` checkpoints — all
    /// in one transaction (SPEC.md §7: creation and its event commit
    /// together). Returns the appended event with its durable seq (the caller
    /// broadcasts it; the DB row is the source of truth).
    pub(crate) fn create_checkpoint_tx(
        &self,
        checkpoint: &Checkpoint,
        event_kind: &str,
        event_payload: Value,
        retention: u32,
    ) -> Result<Event> {
        self.inject("create_checkpoint")?;
        let payload = serde_json::to_string(&checkpoint.payload)
            .map_err(|e| KernError::internal(format!("serialize checkpoint payload: {e}")))?;
        let event_ts = now_rfc3339();
        let event_ts_parsed =
            parse_ts(&event_ts).map_err(|e| map_sqlite_error(e, "checkpoint event timestamp"))?;
        let retention = (retention.max(1)) as i64;
        self.tx(|tx| {
            tx.execute(
                "INSERT INTO checkpoints (id, agent_id, execution_id, parent_id, format_version, seq, payload, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    checkpoint.id,
                    checkpoint.agent_id,
                    checkpoint.execution_id,
                    checkpoint.parent_id,
                    checkpoint.format_version as i64,
                    checkpoint.seq,
                    payload,
                    ts(checkpoint.created_at),
                ],
            )
            .map_err(|e| map_sqlite_error(e, "create checkpoint"))?;

            tx.execute(
                "UPDATE executions SET latest_checkpoint_id = ?1 WHERE id = ?2",
                params![checkpoint.id, checkpoint.execution_id],
            )
            .map_err(|e| map_sqlite_error(e, "link checkpoint to execution"))?;

            // Retention: keep the newest `retention` checkpoints of this
            // agent+execution (the just-inserted one is the newest, so the
            // latest is never pruned).
            tx.execute(
                "DELETE FROM checkpoints WHERE agent_id = ?1 AND execution_id = ?2 AND seq NOT IN (
                     SELECT seq FROM checkpoints WHERE agent_id = ?1 AND execution_id = ?2
                     ORDER BY seq DESC LIMIT ?3)",
                params![checkpoint.agent_id, checkpoint.execution_id, retention],
            )
            .map_err(|e| map_sqlite_error(e, "prune checkpoints"))?;

            let payload_str = serde_json::to_string(&event_payload)
                .map_err(|e| KernError::internal(format!("serialize checkpoint event: {e}")))?;
            tx.execute(
                "INSERT INTO events (ts, kind, agent_id, execution_id, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![event_ts, event_kind, checkpoint.agent_id, checkpoint.execution_id, payload_str],
            )
            .map_err(|e| map_sqlite_error(e, "append checkpoint event"))?;
            let seq = tx.last_insert_rowid();

            Ok(Event {
                seq,
                ts: event_ts_parsed,
                kind: event_kind.to_string(),
                agent_id: Some(checkpoint.agent_id.clone()),
                execution_id: Some(checkpoint.execution_id.clone()),
                payload: event_payload,
            })
        })
    }

    /// The permission request for a tool call id, if one was created (the
    /// recovery re-drive resolves a re-requested `ask` call against its
    /// original request instead of re-asking).
    /// Resolve an ask by its tool-call id WITHIN one agent (tool call ids
    /// are only unique per provider/execution, so an id may repeat across
    /// agents — an ask must never inherit another agent's decision).
    pub fn get_permission_request_by_tool_call(
        &self,
        agent_id: &str,
        tool_call_id: &str,
    ) -> Result<Option<PermissionRequest>> {
        self.with_reader(|conn| {
            conn.query_row(
                "SELECT id, agent_id, tool_call_id, resource, action, status, requested_at, decided_at
                 FROM permission_requests WHERE agent_id = ?1 AND tool_call_id = ?2",
                params![agent_id, tool_call_id],
                permission_from_row,
            )
            .optional()
            .map_err(|e| map_sqlite_error(e, "permission request by tool call"))
        })
    }

    /// Record a whole batch of `requested` tool rows in ONE transaction
    /// (SPEC.md §8.1 6a): a crash mid-batch leaves either all rows or none,
    /// never a partial batch.
    pub fn record_tool_calls_batch(&self, calls: &[ToolCall]) -> Result<()> {
        self.inject("record_tool_calls_batch")?;
        let serialized: Vec<(String, String, String, String, String)> = calls
            .iter()
            .map(|c| {
                Ok((
                    c.id.clone(),
                    c.agent_id.clone(),
                    c.execution_id.clone(),
                    c.tool_name.clone(),
                    serde_json::to_string(&c.args)
                        .map_err(|e| KernError::internal(format!("serialize tool args: {e}")))?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        self.tx(|tx| {
            for (id, agent_id, execution_id, tool_name, args) in &serialized {
                tx.execute(
                    "INSERT INTO tool_calls (id, agent_id, execution_id, tool_name, args_json, status, result_json, error_json, started_at, finished_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'requested', NULL, NULL, NULL, NULL)",
                    params![id, agent_id, execution_id, tool_name, args],
                )
                .map_err(|e| map_sqlite_error(e, "record tool call batch"))?;
            }
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// Row mappers and small helpers
// ---------------------------------------------------------------------------

fn agent_from_row(row: &Row) -> rusqlite::Result<Agent> {
    let next_run_at = match row.get::<_, Option<String>>(9)? {
        Some(s) => Some(parse_ts(&s)?),
        None => None,
    };
    Ok(Agent {
        id: row.get(0)?,
        name: row.get(1)?,
        spec_version: row.get::<_, i64>(2)? as u32,
        config: serde_json::from_str(&row.get::<_, String>(3)?).map_err(|e| conversion(3, e))?,
        state: LifecycleState::from_str(&row.get::<_, String>(4)?)
            .map_err(|e| invalid_enum(4, &e))?,
        created_at: parse_ts(&row.get::<_, String>(5)?)?,
        updated_at: parse_ts(&row.get::<_, String>(6)?)?,
        last_error: row.get(7)?,
        auto_recover: row.get::<_, i64>(8)? != 0,
        next_run_at,
    })
}

fn execution_from_row(row: &Row) -> rusqlite::Result<Execution> {
    let started_at = opt_ts_from_row(row.get::<_, Option<String>>(3)?)?;
    let finished_at = opt_ts_from_row(row.get::<_, Option<String>>(4)?)?;
    Ok(Execution {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        status: ExecutionStatus::from_str(&row.get::<_, String>(2)?)
            .map_err(|e| invalid_enum(2, &e))?,
        started_at,
        finished_at,
        latest_checkpoint_id: row.get(5)?,
        input: row.get(6)?,
        wake_at: opt_ts_from_row(row.get::<_, Option<String>>(7)?)?,
    })
}

fn event_from_row(row: &Row) -> rusqlite::Result<Event> {
    Ok(Event {
        seq: row.get(0)?,
        ts: parse_ts(&row.get::<_, String>(1)?)?,
        kind: row.get(2)?,
        agent_id: row.get(3)?,
        execution_id: row.get(4)?,
        payload: serde_json::from_str(&row.get::<_, String>(5)?).map_err(|e| conversion(5, e))?,
    })
}

fn tool_call_from_row(row: &Row) -> rusqlite::Result<ToolCall> {
    Ok(ToolCall {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        execution_id: row.get(2)?,
        tool_name: row.get(3)?,
        args: serde_json::from_str(&row.get::<_, String>(4)?).map_err(|e| conversion(4, e))?,
        status: ToolCallStatus::from_str(&row.get::<_, String>(5)?)
            .map_err(|e| invalid_enum(5, &e))?,
        result: opt_json_from_row(row.get::<_, Option<String>>(6)?)?,
        error: opt_json_from_row(row.get::<_, Option<String>>(7)?)?,
        started_at: opt_ts_from_row(row.get::<_, Option<String>>(8)?)?,
        finished_at: opt_ts_from_row(row.get::<_, Option<String>>(9)?)?,
    })
}

fn memory_from_row(row: &Row) -> rusqlite::Result<MemoryEntry> {
    Ok(MemoryEntry {
        agent_id: row.get(0)?,
        key: row.get(1)?,
        value: serde_json::from_str(&row.get::<_, String>(2)?).map_err(|e| conversion(2, e))?,
        description: row.get(3)?,
        created_at: parse_ts(&row.get::<_, String>(4)?)?,
        updated_at: parse_ts(&row.get::<_, String>(5)?)?,
    })
}

fn permission_from_row(row: &Row) -> rusqlite::Result<PermissionRequest> {
    Ok(PermissionRequest {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        tool_call_id: row.get(2)?,
        resource: row.get(3)?,
        action: row.get(4)?,
        status: PermissionStatus::from_str(&row.get::<_, String>(5)?)
            .map_err(|e| invalid_enum(5, &e))?,
        requested_at: parse_ts(&row.get::<_, String>(6)?)?,
        decided_at: opt_ts_from_row(row.get::<_, Option<String>>(7)?)?,
        expires_at: opt_ts_from_row(row.get::<_, Option<String>>(8)?)?,
    })
}

fn checkpoint_from_row(row: &Row) -> rusqlite::Result<Checkpoint> {
    Ok(Checkpoint {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        execution_id: row.get(2)?,
        parent_id: row.get(3)?,
        format_version: row.get::<_, i64>(4)? as u32,
        seq: row.get(5)?,
        payload: serde_json::from_str(&row.get::<_, String>(6)?).map_err(|e| conversion(6, e))?,
        created_at: parse_ts(&row.get::<_, String>(7)?)?,
    })
}

fn conversion<E: std::error::Error + Send + Sync + 'static>(idx: usize, e: E) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(e))
}

fn invalid_enum(idx: usize, what: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        idx,
        Type::Text,
        format!("invalid {what} value").into(),
    )
}

fn collect<T>(rows: impl Iterator<Item = rusqlite::Result<T>>, ctx: &str) -> Result<Vec<T>> {
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| map_sqlite_error(e, ctx))
}

/// Copy the corrupted database aside (quarantine). The original is never
/// deleted or overwritten; the runtime surfaces `STORAGE_CORRUPTION` instead.
fn quarantine_corrupt(db_path: &Path) {
    let ts = Utc::now().timestamp();
    let dest = PathBuf::from(format!("{}.corrupt-{ts}", db_path.display()));
    let _ = std::fs::copy(db_path, &dest);
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn opt_ts(t: Option<DateTime<Utc>>) -> Option<String> {
    t.map(ts)
}

fn parse_ts(s: &str) -> rusqlite::Result<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| conversion(0, e))
}

fn opt_ts_from_row(v: Option<String>) -> rusqlite::Result<Option<DateTime<Utc>>> {
    v.map(|s| parse_ts(&s)).transpose()
}

fn opt_json(v: Option<&Value>) -> Option<String> {
    v.map(|v| v.to_string())
}

fn opt_json_from_row(v: Option<String>) -> rusqlite::Result<Option<Value>> {
    v.map(|s| serde_json::from_str(&s).map_err(|e| conversion(0, e)))
        .transpose()
}

/// Escape a key prefix into a literal `LIKE` pattern.
fn like_pattern(prefix: &str) -> String {
    let escaped = prefix
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("{escaped}%")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::STORAGE_SCHEMA_VERSION;
    use std::sync::Arc;

    fn test_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn v3_database_upgrades_to_v4_preserving_data() {
        // Build a v3 database by running migrations 1..=3 only, seed a real
        // execution, then open it — the v4 migration must add `wake_at`
        // without losing or corrupting existing rows.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(DB_FILE);
        let mut conn = rusqlite::Connection::open(&db_path).unwrap();
        for m in crate::store::migration::MIGRATIONS
            .iter()
            .filter(|m| m.version() <= 3)
        {
            let tx = conn.transaction().unwrap();
            m.up(&tx).unwrap();
            tx.execute_batch(&format!("PRAGMA user_version = {}", m.version()))
                .unwrap();
            tx.execute(
                "INSERT INTO kern_meta (key, value) VALUES ('schema_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![m.version().to_string()],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        // Seed a pre-v4 execution exactly as v3 persisted it (no wake_at).
        conn.execute(
            "INSERT INTO agents (id, name, spec_version, config_json, lifecycle_state, created_at, updated_at, auto_recover)
             VALUES ('a1', 'legacy', 1, 'null', 'created', ?1, ?1, 1)",
            params![now_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO executions (id, agent_id, status, started_at, finished_at, latest_checkpoint_id, input)
             VALUES ('e1', 'a1', 'running', ?1, NULL, NULL, 'legacy task')",
            params![now_rfc3339()],
        )
        .unwrap();
        drop(conn);

        let store = Store::open(dir.path()).unwrap();
        let execution = store.get_execution("e1").unwrap();
        assert_eq!(execution.input.as_deref(), Some("legacy task"));
        assert_eq!(execution.wake_at, None, "v4 adds wake_at as NULL");
        // The new column is live: set + read through the store API. The store
        // truncates to seconds, so compare at that precision.
        let now = Utc::now();
        store.set_wake_at("e1", Some(now)).unwrap();
        let stored = store.get_execution("e1").unwrap().wake_at.unwrap();
        assert!(
            (stored - now).num_seconds().abs() <= 1,
            "stored wake_at differs from the set value by more than a second"
        );
        let uv: i64 = store
            .with_reader(|c| {
                c.query_row("PRAGMA user_version", [], |r| r.get(0))
                    .map_err(|e| map_sqlite_error(e, "read user_version"))
            })
            .unwrap();
        assert_eq!(uv, STORAGE_SCHEMA_VERSION as i64);
    }

    #[test]
    fn execution_input_round_trips_through_the_store() {
        let (_dir, store) = test_store();
        let a1 = Agent::new("a1", Value::Null, LifecycleState::Created);
        store.create_agent(&a1).unwrap();
        let a2 = Agent::new("a2", Value::Null, LifecycleState::Created);
        store.create_agent(&a2).unwrap();

        let mut with_input = Execution::new(&a1.id, ExecutionStatus::Pending);
        with_input.input = Some("durable task".to_string());
        store.create_execution(&with_input).unwrap();
        let loaded = store.get_execution(&with_input.id).unwrap();
        assert_eq!(loaded.input.as_deref(), Some("durable task"));

        let bare = Execution::new(&a2.id, ExecutionStatus::Pending);
        store.create_execution(&bare).unwrap();
        assert!(store.get_execution(&bare.id).unwrap().input.is_none());

        let listed = store.list_executions_for_agent(&a1.id).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].input.as_deref(), Some("durable task"));
    }

    #[test]
    fn wake_at_round_trips_and_due_scan_filters_correctly() {
        let (_dir, store) = test_store();
        // Sleeping agents hold an execution with a persisted wake
        // time; the due scan must only return those whose time has passed.
        let due = Agent::new("due-sleeper", Value::Null, LifecycleState::Sleeping);
        store.create_agent(&due).unwrap();
        let future = Agent::new("future-sleeper", Value::Null, LifecycleState::Sleeping);
        store.create_agent(&future).unwrap();
        let active = Agent::new("active", Value::Null, LifecycleState::Running);
        store.create_agent(&active).unwrap();

        let now = Utc::now();
        let mut due_exec = Execution::new(&due.id, ExecutionStatus::Running);
        due_exec.wake_at = Some(now - chrono::Duration::seconds(5));
        store.create_execution(&due_exec).unwrap();
        let mut future_exec = Execution::new(&future.id, ExecutionStatus::Running);
        future_exec.wake_at = Some(now + chrono::Duration::seconds(60));
        store.create_execution(&future_exec).unwrap();
        // A non-sleeping agent's future wake time must never be woken.
        let mut active_exec = Execution::new(&active.id, ExecutionStatus::Running);
        active_exec.wake_at = Some(now - chrono::Duration::seconds(5));
        store.create_execution(&active_exec).unwrap();

        let due_ids: Vec<String> = store
            .list_sleeping_due(now)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(
            due_ids,
            vec![due_exec.id.clone()],
            "only the due sleeper wakes"
        );

        let soonest = store.soonest_wake_at().unwrap().unwrap();
        assert!(
            (due_exec.wake_at.unwrap() - soonest).num_seconds().abs() <= 1,
            "soonest wake differs from the stored value by more than a second"
        );

        // Clearing the wake time removes the execution from both scans.
        store.set_wake_at(&due_exec.id, None).unwrap();
        assert!(store.list_sleeping_due(now).unwrap().is_empty());
        assert!(
            (future_exec.wake_at.unwrap() - store.soonest_wake_at().unwrap().unwrap())
                .num_seconds()
                .abs()
                <= 1,
            "soonest wake differs from the stored future wake by more than a second"
        );
        assert_eq!(store.get_execution(&due_exec.id).unwrap().wake_at, None);
    }

    #[test]
    fn fresh_db_migrates_to_current_schema() {
        let (dir, store) = test_store();
        let conn = rusqlite::Connection::open(dir.path().join(DB_FILE)).unwrap();
        let uv: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(uv, STORAGE_SCHEMA_VERSION as i64);
        let sv: String = conn
            .query_row(
                "SELECT value FROM kern_meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sv, STORAGE_SCHEMA_VERSION.to_string());
        let table_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 9, "expected the 9 SPEC tables");
        drop(store);
    }

    #[test]
    fn reopen_existing_db_is_a_noop() {
        let (dir, store) = test_store();
        drop(store);
        let _again = Store::open(dir.path()).unwrap();
    }

    #[test]
    fn newer_schema_is_rejected() {
        let (dir, store) = test_store();
        drop(store);
        let conn = rusqlite::Connection::open(dir.path().join(DB_FILE)).unwrap();
        conn.execute_batch(
            "PRAGMA user_version = 99; UPDATE kern_meta SET value = '99' WHERE key = 'schema_version';",
        )
        .unwrap();
        drop(conn);
        let err = Store::open(dir.path()).err().expect("open must fail");
        assert_eq!(err.code(), ErrorCode::StorageMigration);
    }

    #[test]
    fn downgraded_schema_is_rejected() {
        let (dir, store) = test_store();
        drop(store);
        let conn = rusqlite::Connection::open(dir.path().join(DB_FILE)).unwrap();
        conn.execute_batch("PRAGMA user_version = 0;").unwrap(); // kern_meta still says 1
        drop(conn);
        let err = Store::open(dir.path()).err().expect("open must fail");
        assert_eq!(err.code(), ErrorCode::StorageMigration);
    }

    #[test]
    fn data_dir_is_single_owner() {
        let dir = tempfile::tempdir().unwrap();
        let first = Store::open(dir.path()).unwrap();
        let err = Store::open(dir.path())
            .err()
            .expect("second open must fail");
        assert_eq!(err.code(), ErrorCode::StorageLocked);
        drop(first);
        // Lock is released on drop → reopening works.
        let _second = Store::open(dir.path()).unwrap();
    }

    #[test]
    fn tx_rolls_back_on_error() {
        let (_, store) = test_store();
        let agent = Agent::new("atomic", Value::Null, LifecycleState::Created);
        let result = store.tx(|tx| -> Result<()> {
            tx.execute(
                "INSERT INTO agents (id, name, spec_version, config_json, lifecycle_state, created_at, updated_at, auto_recover)
                 VALUES (?1, ?2, 1, 'null', 'created', ?3, ?3, 1)",
                params![agent.id, agent.name, now_rfc3339()],
            )
            .map_err(|e| map_sqlite_error(e, "insert"))?;
            Err(KernError::new(ErrorCode::Internal, "boom after insert"))
        });
        assert!(result.is_err());
        assert!(
            store.get_agent(&agent.id).is_err(),
            "transaction must roll back"
        );
    }

    #[test]
    fn duplicate_agent_name_is_rejected() {
        let (_dir, store) = test_store();
        store
            .create_agent(&Agent::new("dup", Value::Null, LifecycleState::Created))
            .unwrap();
        let err = store
            .create_agent(&Agent::new("dup", Value::Null, LifecycleState::Created))
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::AgentNameTaken);
    }

    #[test]
    fn only_one_active_execution_per_agent() {
        let (_dir, store) = test_store();
        let agent = Agent::new("exec", Value::Null, LifecycleState::Created);
        store.create_agent(&agent).unwrap();
        let e1 = Execution::new(&agent.id, ExecutionStatus::Running);
        store.create_execution(&e1).unwrap();
        let e2 = Execution::new(&agent.id, ExecutionStatus::Pending);
        let err = store.create_execution(&e2).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ExecutionAlreadyActive);
        // Finishing the first allows a new one.
        let mut done = e1.clone();
        done.status = ExecutionStatus::Completed;
        store.update_execution(&done).unwrap();
        store.create_execution(&e2).unwrap();
    }

    #[test]
    fn agent_crud_and_next_run_at() {
        let (_dir, store) = test_store();
        let mut agent = Agent::new("crud", Value::Null, LifecycleState::Created);
        store.create_agent(&agent).unwrap();
        assert_eq!(
            store.get_agent_by_name("crud").unwrap().unwrap().id,
            agent.id
        );
        assert_eq!(store.list_agents().unwrap().len(), 1);

        agent.state = LifecycleState::Running;
        agent.last_error = Some("nope".to_string());
        agent.next_run_at = Some(Utc::now() + chrono::Duration::hours(1));
        store.update_agent(&agent).unwrap();
        let loaded = store.get_agent(&agent.id).unwrap();
        assert_eq!(loaded.state, LifecycleState::Running);
        assert_eq!(loaded.last_error.as_deref(), Some("nope"));
        assert!(loaded.next_run_at.is_some());

        store.set_next_run_at(&agent.id, None).unwrap();
        assert!(store.get_agent(&agent.id).unwrap().next_run_at.is_none());
        assert!(store.get_agent("missing").is_err());

        store.delete_agent(&agent.id).unwrap();
        assert!(store.list_agents().unwrap().is_empty());
    }

    #[test]
    fn transition_applies_atomically_with_state_guard() {
        let (_dir, store) = test_store();
        let agent = Agent::new("tx", Value::Null, LifecycleState::Created);
        store.create_agent(&agent).unwrap();

        let transition = Transition {
            agent_id: agent.id.clone(),
            expected_state: LifecycleState::Created,
            new_state: LifecycleState::Starting,
            last_error: None,
            execution: None,
            events: vec![EventRecord {
                kind: "test.transition",
                execution_id: None,
                payload: serde_json::json!({ "note": "hi" }),
            }],
        };
        let events = store.transition(&transition).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "test.transition");
        assert!(events[0].seq > 0);
        assert_eq!(
            store.get_agent(&agent.id).unwrap().state,
            LifecycleState::Starting
        );

        // Stale expected state → INVALID_TRANSITION, nothing changed.
        let stale = Transition {
            expected_state: LifecycleState::Created, // agent is now Starting
            new_state: LifecycleState::Running,
            ..transition
        };
        let err = store.transition(&stale).unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidTransition);
        assert_eq!(
            store.get_agent(&agent.id).unwrap().state,
            LifecycleState::Starting
        );
        assert_eq!(
            store.events_after(0, 100).unwrap().len(),
            1,
            "no extra events"
        );
    }

    #[test]
    fn transition_updates_execution_and_rolls_back_on_missing() {
        let (_dir, store) = test_store();
        let agent = Agent::new("tx-ex", Value::Null, LifecycleState::Starting);
        store.create_agent(&agent).unwrap();
        let execution = Execution::new(&agent.id, ExecutionStatus::Pending);
        store.create_execution(&execution).unwrap();

        let mut t = Transition {
            agent_id: agent.id.clone(),
            expected_state: LifecycleState::Starting,
            new_state: LifecycleState::Running,
            last_error: None,
            execution: Some(ExecutionUpdate {
                id: execution.id.clone(),
                status: ExecutionStatus::Running,
                started_at: Some(Utc::now()),
                finished_at: None,
            }),
            events: vec![EventRecord {
                kind: "execution.started",
                execution_id: Some(execution.id.clone()),
                payload: serde_json::json!({}),
            }],
        };
        store.transition(&t).unwrap();
        let loaded = store.get_execution(&execution.id).unwrap();
        assert_eq!(loaded.status, ExecutionStatus::Running);
        assert!(loaded.started_at.is_some());

        // Missing execution → whole transition rolls back (agent untouched).
        t.execution = Some(ExecutionUpdate {
            id: "nope".to_string(),
            status: ExecutionStatus::Completed,
            started_at: None,
            finished_at: Some(Utc::now()),
        });
        t.expected_state = LifecycleState::Running;
        t.new_state = LifecycleState::Completed;
        let err = store.transition(&t).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ExecutionNotFound);
        assert_eq!(
            store.get_agent(&agent.id).unwrap().state,
            LifecycleState::Running,
            "agent update must roll back with the failed execution update"
        );
        assert_eq!(store.events_after(0, 100).unwrap().len(), 1);
    }

    #[test]
    fn uncommitted_transition_is_discarded_on_drop() {
        // Crash simulation: a writer begins the transition, writes the agent
        // state and an event, and dies without committing. SQLite discards the
        // uncommitted transaction — the store must show the pre-transition
        // state (same guarantee a SIGKILL mid-transition gets from WAL).
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let agent = Agent::new("crash", Value::Null, LifecycleState::Created);
        store.create_agent(&agent).unwrap();

        let db_path = store.db_path().to_path_buf();
        let crashed = rusqlite::Connection::open(&db_path).unwrap();
        crashed.execute_batch("BEGIN").unwrap();
        crashed
            .execute(
                "UPDATE agents SET lifecycle_state = 'starting' WHERE id = ?1",
                params![agent.id],
            )
            .unwrap();
        crashed
            .execute(
                "INSERT INTO events (ts, kind, agent_id, payload)
                 VALUES ('2026-08-15T00:00:00Z', 'agent.started', ?1, '{}')",
                params![agent.id],
            )
            .unwrap();
        drop(crashed); // no COMMIT: the transaction is discarded

        assert_eq!(
            store.get_agent(&agent.id).unwrap().state,
            LifecycleState::Created,
            "uncommitted state change must not survive"
        );
        assert_eq!(
            store.events_after(0, 100).unwrap().len(),
            0,
            "uncommitted event must not survive"
        );
    }

    #[test]
    fn events_append_and_replay() {
        let (_dir, store) = test_store();
        let agent = Agent::new("events", Value::Null, LifecycleState::Created);
        store.create_agent(&agent).unwrap();
        for i in 0..5 {
            store
                .append_event(
                    "agent.thinking",
                    Some(&agent.id),
                    None,
                    serde_json::json!({ "i": i }),
                )
                .unwrap();
        }
        store
            .append_event("tool.completed", None, Some("ex-1"), serde_json::json!({}))
            .unwrap();

        let all = store.events_after(0, 100).unwrap();
        assert_eq!(all.len(), 6);
        assert!(all.windows(2).all(|w| w[0].seq < w[1].seq));

        let replay = store.events_after(3, 10).unwrap();
        assert_eq!(replay.first().unwrap().seq, 4);

        let mine = store.events_for_agent_after(&agent.id, 0, 100).unwrap();
        assert_eq!(mine.len(), 5);
        assert!(mine
            .iter()
            .all(|e| e.agent_id.as_deref() == Some(&agent.id)));
    }

    #[test]
    fn tool_call_lifecycle() {
        let (_dir, store) = test_store();
        let agent = Agent::new("tools", Value::Null, LifecycleState::Created);
        store.create_agent(&agent).unwrap();
        let execution = Execution::new(&agent.id, ExecutionStatus::Running);
        store.create_execution(&execution).unwrap();

        let mut call = ToolCall::new(
            "call-1",
            &agent.id,
            &execution.id,
            "filesystem",
            serde_json::json!({ "action": "write", "path": "x" }),
        );
        store.record_tool_call(&call).unwrap();
        assert_eq!(
            store
                .get_tool_call(&execution.id, "call-1")
                .unwrap()
                .unwrap()
                .status,
            ToolCallStatus::Requested
        );

        call.status = ToolCallStatus::Completed;
        call.result = Some(serde_json::json!({ "ok": true }));
        call.finished_at = Some(Utc::now());
        store.update_tool_call(&call).unwrap();

        let loaded = store
            .get_tool_call(&execution.id, "call-1")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, ToolCallStatus::Completed);
        assert_eq!(loaded.result, Some(serde_json::json!({ "ok": true })));
        assert_eq!(
            store.tool_calls_for_execution(&execution.id).unwrap().len(),
            1
        );
        assert!(store
            .get_tool_call(&execution.id, "call-2")
            .unwrap()
            .is_none());
    }

    #[test]
    fn memory_lifecycle() {
        let (_dir, store) = test_store();
        let agent = Agent::new("mem", Value::Null, LifecycleState::Created);
        store.create_agent(&agent).unwrap();
        store
            .memory_put(
                &agent.id,
                "goal",
                serde_json::json!({ "text": "ship" }),
                Some("primary goal"),
            )
            .unwrap();
        store
            .memory_put(&agent.id, "notes.a", serde_json::json!(1), None)
            .unwrap();
        store
            .memory_put(&agent.id, "notes.b", serde_json::json!(2), None)
            .unwrap();

        let entry = store.memory_get(&agent.id, "goal").unwrap().unwrap();
        assert_eq!(entry.value["text"], "ship");
        assert_eq!(entry.description.as_deref(), Some("primary goal"));

        assert_eq!(
            store.memory_list(&agent.id, Some("notes.")).unwrap().len(),
            2
        );
        assert_eq!(store.memory_list(&agent.id, None).unwrap().len(), 3);

        assert!(store.memory_delete(&agent.id, "goal").unwrap());
        assert!(!store.memory_delete(&agent.id, "goal").unwrap());
        assert!(store.memory_get(&agent.id, "goal").unwrap().is_none());
    }

    #[test]
    fn state_variables_lifecycle() {
        let (_dir, store) = test_store();
        let agent = Agent::new("vars", Value::Null, LifecycleState::Created);
        store.create_agent(&agent).unwrap();
        store
            .set_variable(&agent.id, "ex-1", "count", serde_json::json!(3))
            .unwrap();
        store
            .set_variable(&agent.id, "ex-2", "count", serde_json::json!(4))
            .unwrap(); // overwrite
        assert_eq!(
            store.get_variable(&agent.id, "count").unwrap().unwrap(),
            serde_json::json!(4)
        );
        assert_eq!(store.list_variables(&agent.id).unwrap().len(), 1);
        assert!(store.get_variable(&agent.id, "missing").unwrap().is_none());
    }

    #[test]
    fn checkpoint_crud_and_ordering() {
        let (_dir, store) = test_store();
        let agent = Agent::new("cp", Value::Null, LifecycleState::Created);
        store.create_agent(&agent).unwrap();
        for seq in 1..=3 {
            store
                .create_checkpoint(&Checkpoint::new(
                    &agent.id,
                    "ex-1",
                    seq,
                    serde_json::json!({ "step": seq }),
                ))
                .unwrap();
        }
        assert_eq!(store.latest_checkpoint(&agent.id).unwrap().unwrap().seq, 3);
        let all = store.list_checkpoints(&agent.id, 10).unwrap();
        assert_eq!(all.iter().map(|c| c.seq).collect::<Vec<_>>(), vec![3, 2, 1]);
        let err = store.get_checkpoint("missing").unwrap_err();
        assert_eq!(err.code(), ErrorCode::CheckpointNotFound);
        assert!(store.latest_checkpoint("no-such-agent").unwrap().is_none());
    }

    #[test]
    fn permission_request_lifecycle() {
        let (_dir, store) = test_store();
        let agent = Agent::new("perm", Value::Null, LifecycleState::Created);
        store.create_agent(&agent).unwrap();
        let req = store
            .create_permission_request(
                &agent.id,
                Some("call-1"),
                "filesystem:write",
                "write ./workspace/out.txt",
            )
            .unwrap();
        assert_eq!(req.status, PermissionStatus::Pending);
        assert_eq!(store.pending_permission_requests().unwrap().len(), 1);

        let granted = store.decide_permission_request(&req.id, true).unwrap();
        assert_eq!(granted.status, PermissionStatus::Granted);
        assert!(granted.decided_at.is_some());
        assert!(store.pending_permission_requests().unwrap().is_empty());

        let denied = store
            .create_permission_request(&agent.id, None, "network:host", "GET api.example.com")
            .unwrap();
        let denied = store.decide_permission_request(&denied.id, false).unwrap();
        assert_eq!(denied.status, PermissionStatus::Denied);

        let err = store.get_permission_request("missing").unwrap_err();
        assert_eq!(err.code(), ErrorCode::PermissionRequestNotFound);
    }
    #[test]
    fn corruption_is_detected_and_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(dir.path()).unwrap();
            store
                .create_agent(&Agent::new("victim", Value::Null, LifecycleState::Created))
                .unwrap();
        } // drop: connections closed, WAL checkpointed, lock released

        let db = dir.path().join(DB_FILE);
        let _ = std::fs::remove_file(dir.path().join(format!("{DB_FILE}-wal")));
        let _ = std::fs::remove_file(dir.path().join(format!("{DB_FILE}-shm")));

        // Deterministically corrupt page 1's b-tree content (the schema page):
        // zero a chunk of the sqlite_master b-tree so `quick_check` fails.
        let mut file = std::fs::OpenOptions::new().write(true).open(&db).unwrap();
        use std::io::{Seek, SeekFrom, Write};
        file.seek(SeekFrom::Start(512)).unwrap();
        file.write_all(&[0u8; 64]).unwrap();
        drop(file);

        let err = Store::open(dir.path()).err().expect("open must fail");
        assert_eq!(err.code(), ErrorCode::StorageCorruption);

        let quarantined = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .any(|name| name.starts_with(&format!("{DB_FILE}.corrupt-")));
        assert!(
            quarantined,
            "expected a quarantine copy of the corrupted db"
        );

        // The runtime never silently recreates the database.
        assert!(Store::open(dir.path()).is_err());
    }

    #[test]
    fn non_database_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(DB_FILE);
        std::fs::write(
            &db,
            b"definitely not a sqlite database, just some plain text here",
        )
        .unwrap();
        let err = Store::open(dir.path()).err().expect("open must fail");
        assert_eq!(err.code(), ErrorCode::StorageCorruption);
    }

    #[test]
    fn concurrent_readers_do_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        for i in 0..10 {
            store
                .create_agent(&Agent::new(
                    format!("seed-{i}"),
                    Value::Null,
                    LifecycleState::Created,
                ))
                .unwrap();
        }

        let reader = Arc::clone(&store);
        let handle = std::thread::spawn(move || {
            let mut last = 0usize;
            for _ in 0..500 {
                let count = reader.list_agents().unwrap().len();
                assert!(
                    count >= last,
                    "reader observed a shrinking view: {count} < {last}"
                );
                last = count;
            }
            last
        });

        for i in 10..60 {
            store
                .create_agent(&Agent::new(
                    format!("writer-{i}"),
                    Value::Null,
                    LifecycleState::Created,
                ))
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        let final_count = handle.join().unwrap();
        // The reader may finish before or after the writer; the monotonicity
        // assertions inside the loop are the real guarantee. It must never see
        // fewer than the seeded agents or more than the total written.
        assert!(
            (10..=60).contains(&final_count),
            "reader saw {final_count} agents, expected between 10 and 60"
        );
    }
}
