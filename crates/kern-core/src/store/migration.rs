//! Forward-only schema migrations (SPEC.md §5 rules).
//!
//! The schema version lives in two places that MUST agree after any open:
//! `PRAGMA user_version` and `kern_meta.schema_version`. Migrations run in
//! order, each in its own transaction; a downgrade or a database from a newer
//! Kern version is rejected with `STORAGE_MIGRATION`.

use rusqlite::{params, Connection, Transaction};

use crate::error::{ErrorCode, KernError, Result};
use crate::store::new_id;
use crate::version::STORAGE_SCHEMA_VERSION;

pub(crate) const MIGRATIONS: &[&dyn Migration] =
    &[&MigrationV1, &MigrationV2, &MigrationV3, &MigrationV4];

pub(crate) trait Migration {
    fn version(&self) -> u32;
    fn name(&self) -> &'static str;
    fn up(&self, tx: &Transaction<'_>) -> rusqlite::Result<()>;
}

struct MigrationV1;

impl Migration for MigrationV1 {
    fn version(&self) -> u32 {
        1
    }

    fn name(&self) -> &'static str {
        "v1-initial-schema"
    }

    fn up(&self, tx: &Transaction<'_>) -> rusqlite::Result<()> {
        tx.execute_batch(SCHEMA_V1)?;
        tx.execute(
            "INSERT INTO kern_meta (key, value) VALUES ('instance_id', ?1)",
            params![new_id()],
        )?;
        Ok(())
    }
}

struct MigrationV2;

impl Migration for MigrationV2 {
    fn version(&self) -> u32 {
        2
    }

    fn name(&self) -> &'static str {
        "v2-permission-request-expiry"
    }

    fn up(&self, tx: &Transaction<'_>) -> rusqlite::Result<()> {
        // Approval TTL: nullable so
        // pre-v2 rows (and the pending-request lifetime) stay valid; fresh
        // requests always carry an expiry.
        tx.execute_batch("ALTER TABLE permission_requests ADD COLUMN expires_at TEXT;")
    }
}

struct MigrationV4;

impl Migration for MigrationV4 {
    fn version(&self) -> u32 {
        4
    }

    fn name(&self) -> &'static str {
        "v4-durable-wake-at"
    }

    fn up(&self, tx: &Transaction<'_>) -> rusqlite::Result<()> {
        // Durable wake/sleep: nullable so pre-v4 executions (and
        // non-sleeping ones) stay valid; only `sleeping` executions carry it.
        tx.execute_batch("ALTER TABLE executions ADD COLUMN wake_at TEXT;")
    }
}

struct MigrationV3;

impl Migration for MigrationV3 {
    fn version(&self) -> u32 {
        3
    }

    fn name(&self) -> &'static str {
        "v3-execution-input"
    }

    fn up(&self, tx: &Transaction<'_>) -> rusqlite::Result<()> {
        // Durable pre-start task input: nullable so
        // pre-v3 executions (and all created rows) read as `None`; fresh
        // executions carry the task when one was provided.
        tx.execute_batch("ALTER TABLE executions ADD COLUMN input TEXT;")
    }
}

/// Bring `conn` to the current schema version. Safe to run on a fresh database
/// or one already at the current version.
pub(crate) fn migrate(conn: &mut Connection) -> Result<()> {
    let user_version = conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(|e| store_err(e, "read user_version"))? as u32;

    let meta_version = read_meta_version(conn)?;

    let consistent = match (user_version, meta_version) {
        (0, None) => true, // fresh database
        (uv, Some(mv)) => uv == mv,
        _ => false,
    };
    if !consistent {
        let meta = match meta_version {
            Some(v) => v.to_string(),
            None => "missing".to_string(),
        };
        return Err(KernError::new(
            ErrorCode::StorageMigration,
            format!("schema mismatch: PRAGMA user_version={user_version}, kern_meta.schema_version={meta}"),
        ));
    }

    if user_version > STORAGE_SCHEMA_VERSION {
        return Err(KernError::new(
            ErrorCode::StorageMigration,
            format!(
                "database schema v{user_version} is newer than this runtime supports (v{STORAGE_SCHEMA_VERSION}); upgrade Kern"
            ),
        ));
    }
    if user_version == STORAGE_SCHEMA_VERSION {
        return Ok(());
    }

    for migration in MIGRATIONS.iter().filter(|m| m.version() > user_version) {
        let tx = conn
            .transaction()
            .map_err(|e| store_err(e, "begin migration transaction"))?;
        migration.up(&tx).map_err(|e| {
            KernError::new(
                ErrorCode::StorageMigration,
                format!("migration {} failed: {e}", migration.name()),
            )
        })?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", migration.version()))
            .map_err(|e| store_err(e, "set user_version"))?;
        tx.execute(
            "INSERT INTO kern_meta (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![migration.version().to_string()],
        )
        .map_err(|e| store_err(e, "set schema_version"))?;
        tx.commit().map_err(|e| store_err(e, "commit migration"))?;
    }

    Ok(())
}

/// Read `kern_meta.schema_version`. `None` means the table does not exist yet
/// (fresh database); `Some(0)` means the table exists without the row.
fn read_meta_version(conn: &Connection) -> Result<Option<u32>> {
    let table_exists = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params!["kern_meta"],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|e| store_err(e, "inspect schema"))?
        > 0;
    if !table_exists {
        return Ok(None);
    }
    match conn.query_row(
        "SELECT value FROM kern_meta WHERE key = 'schema_version'",
        [],
        |row| row.get::<_, String>(0),
    ) {
        Ok(s) => s.parse::<u32>().map(Some).map_err(|_| {
            KernError::new(
                ErrorCode::StorageMigration,
                format!("invalid schema_version in kern_meta: {s}"),
            )
        }),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(Some(0)),
        Err(e) => Err(store_err(e, "read schema_version")),
    }
}

fn store_err(e: rusqlite::Error, ctx: &str) -> KernError {
    crate::store::map_sqlite_error(e, ctx)
}

/// Normative schema v1 (SPEC.md §5). `kern_meta` is created idempotently so the
/// degenerate "table exists, version 0" state still migrates cleanly.
const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS kern_meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE agents (
  id               TEXT PRIMARY KEY,
  name             TEXT NOT NULL UNIQUE,
  spec_version     INTEGER NOT NULL,
  config_json      TEXT NOT NULL,
  lifecycle_state  TEXT NOT NULL,
  created_at       TEXT NOT NULL,
  updated_at       TEXT NOT NULL,
  last_error       TEXT,
  auto_recover     INTEGER NOT NULL DEFAULT 1,
  next_run_at      TEXT
);

CREATE TABLE executions (
  id                   TEXT PRIMARY KEY,
  agent_id             TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  status               TEXT NOT NULL,
  started_at           TEXT,
  finished_at          TEXT,
  latest_checkpoint_id TEXT
);
CREATE UNIQUE INDEX ux_executions_one_active
  ON executions(agent_id) WHERE status IN ('pending','running');

CREATE TABLE events (
  seq          INTEGER PRIMARY KEY AUTOINCREMENT,
  ts           TEXT NOT NULL,
  kind         TEXT NOT NULL,
  agent_id     TEXT,
  execution_id TEXT,
  payload      TEXT NOT NULL
);
CREATE INDEX ix_events_agent ON events(agent_id, seq);
CREATE INDEX ix_events_kind  ON events(kind, seq);

CREATE TABLE checkpoints (
  id               TEXT PRIMARY KEY,
  agent_id         TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  execution_id     TEXT NOT NULL,
  parent_id        TEXT,
  format_version   INTEGER NOT NULL,
  seq              INTEGER NOT NULL,
  payload          TEXT NOT NULL,
  created_at       TEXT NOT NULL,
  UNIQUE (agent_id, seq)
);

CREATE TABLE state_variables (
  agent_id     TEXT NOT NULL,
  execution_id TEXT NOT NULL,
  key          TEXT NOT NULL,
  value        TEXT NOT NULL,
  updated_at   TEXT NOT NULL,
  PRIMARY KEY (agent_id, key)
);

CREATE TABLE memory (
  agent_id    TEXT NOT NULL,
  key         TEXT NOT NULL,
  value       TEXT NOT NULL,
  description TEXT,
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL,
  PRIMARY KEY (agent_id, key)
);

CREATE TABLE tool_calls (
  id           TEXT NOT NULL,
  agent_id     TEXT NOT NULL,
  execution_id TEXT NOT NULL,
  tool_name    TEXT NOT NULL,
  args_json    TEXT NOT NULL,
  status       TEXT NOT NULL,
  result_json  TEXT,
  error_json   TEXT,
  started_at   TEXT,
  finished_at  TEXT,
  PRIMARY KEY (execution_id, id)
);

CREATE TABLE permission_requests (
  id           TEXT PRIMARY KEY,
  agent_id     TEXT NOT NULL,
  tool_call_id TEXT,
  resource     TEXT NOT NULL,
  action       TEXT NOT NULL,
  status       TEXT NOT NULL,
  requested_at TEXT NOT NULL,
  decided_at   TEXT
);
"#;
