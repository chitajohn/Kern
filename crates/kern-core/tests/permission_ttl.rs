//! Approval TTL — the operator's decision
//! window is a first-class property of a permission request:
//!
//! - fresh requests carry `expires_at = requested_at + ask_timeout`;
//! - a decision on an expired request is rejected (`PERMISSION_REQUEST_EXPIRED`)
//!   and the request is sealed `expired` — a late `grant` can never resurrect it;
//! - the engine's poll primitive (`expire_permission_request`) CASes overdue
//!   pending requests to `expired` so a waiting agent un-parks as a denial
//!   instead of hanging forever;
//! - pre-v2 rows (no expiry) remain decidable — migration v2 is backward safe.
//!
//! Written as integration tests against the public store API (the same surface
//! the engine and HTTP API use).

use kern_core::error::ErrorCode;
use kern_core::store::{Agent, LifecycleState, PermissionStatus, Store};
use serde_json::Value;

fn test_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open store");
    (dir, store)
}

fn agent(store: &Store, name: &str) -> String {
    let agent = Agent::new(name, Value::Null, LifecycleState::Created);
    store.create_agent(&agent).expect("create agent");
    agent.id
}

#[test]
fn fresh_requests_carry_an_expiry() {
    let (_dir, store) = test_store();
    let agent_id = agent(&store, "ttl-fresh");

    let req = store
        .create_permission_request_with_ttl(
            &agent_id,
            Some("call-1"),
            "filesystem:write",
            "write ./workspace/out.txt",
            std::time::Duration::from_secs(120),
        )
        .expect("create request");

    let expiry = req.expires_at.expect("fresh request must carry an expiry");
    let window = expiry - req.requested_at;
    assert_eq!(window.num_seconds(), 120);
    assert_eq!(req.status, PermissionStatus::Pending);
}

#[test]
fn decision_after_expiry_is_rejected_and_sealed() {
    let (_dir, store) = test_store();
    let agent_id = agent(&store, "ttl-late");

    // A vanishingly short window: by the time we decide, it is long closed.
    let req = store
        .create_permission_request_with_ttl(
            &agent_id,
            Some("call-1"),
            "network:host",
            "GET api.example.com",
            std::time::Duration::from_millis(1),
        )
        .expect("create request");
    std::thread::sleep(std::time::Duration::from_millis(30));

    let err = store
        .decide_permission_request(&req.id, true)
        .expect_err("a grant on an expired request must be rejected");
    assert_eq!(err.code(), ErrorCode::PermissionRequestExpired);

    // Sealed: the row is `expired`, and even a now-consistent decision cannot
    // flip it (a deny can never become a grant, and vice versa).
    let current = store.get_permission_request(&req.id).expect("row exists");
    assert_eq!(current.status, PermissionStatus::Expired);
    let err = store
        .decide_permission_request(&req.id, false)
        .expect_err("expired requests are no longer decidable");
    assert_eq!(err.code(), ErrorCode::PermissionRequestAlreadyDecided);
}

#[test]
fn decision_within_window_is_recorded() {
    let (_dir, store) = test_store();
    let agent_id = agent(&store, "ttl-in-time");

    let req = store
        .create_permission_request_with_ttl(
            &agent_id,
            Some("call-1"),
            "filesystem:read",
            "read ./workspace",
            std::time::Duration::from_secs(60),
        )
        .expect("create request");
    let decided = store
        .decide_permission_request(&req.id, false)
        .expect("decision inside the window succeeds");
    assert_eq!(decided.status, PermissionStatus::Denied);
    assert!(decided.decided_at.is_some());
}

#[test]
fn expire_permission_request_is_a_cas_on_pending_and_overdue() {
    let (_dir, store) = test_store();
    let agent_id = agent(&store, "ttl-cas");

    // Not yet overdue: untouched.
    let fresh = store
        .create_permission_request_with_ttl(
            &agent_id,
            Some("call-1"),
            "network:host",
            "GET api.example.com",
            std::time::Duration::from_secs(60),
        )
        .expect("create fresh");
    assert!(!store.expire_permission_request(&fresh.id).expect("cas"));
    assert_eq!(
        store.get_permission_request(&fresh.id).unwrap().status,
        PermissionStatus::Pending
    );

    // Overdue pending: sealed expired.
    let overdue = store
        .create_permission_request_with_ttl(
            &agent_id,
            Some("call-2"),
            "network:host",
            "GET api.example.com",
            std::time::Duration::from_millis(1),
        )
        .expect("create overdue");
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert!(store.expire_permission_request(&overdue.id).expect("cas"));
    assert_eq!(
        store.get_permission_request(&overdue.id).unwrap().status,
        PermissionStatus::Expired
    );

    // Idempotent: already expired → untouched.
    assert!(!store.expire_permission_request(&overdue.id).expect("cas"));

    // Decided → untouched (a generous window so the round trip cannot
    // expire the request before the decision lands).
    let decided = store
        .create_permission_request_with_ttl(
            &agent_id,
            Some("call-3"),
            "network:host",
            "GET api.example.com",
            std::time::Duration::from_secs(60),
        )
        .expect("create decided");
    store
        .decide_permission_request(&decided.id, false)
        .expect("decide");
    assert!(!store
        .expire_permission_request(&decided.id)
        .expect("cas on decided row"));
}

/// Migration v2 is backward-safe: a v1 database (no `expires_at` column)
/// migrates cleanly, existing pending requests stay decidable, and fresh
/// requests carry expiries.
#[test]
fn v1_database_migrates_and_legacy_rows_stay_decidable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("state.db");

    // Build a genuine v1 database by hand: the v1 schema, schema version 1,
    // an agent, and a pending permission request (no expiry — the column did
    // not exist in v1).
    let conn = rusqlite::Connection::open(&db_path).expect("open raw db");
    conn.execute_batch(
        "CREATE TABLE kern_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE agents (
           id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, spec_version INTEGER NOT NULL,
           config_json TEXT NOT NULL, lifecycle_state TEXT NOT NULL, created_at TEXT NOT NULL,
           updated_at TEXT NOT NULL, last_error TEXT, auto_recover INTEGER NOT NULL DEFAULT 1,
           next_run_at TEXT
         );
         CREATE TABLE executions (
           id TEXT PRIMARY KEY, agent_id TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
           status TEXT NOT NULL, started_at TEXT, finished_at TEXT, latest_checkpoint_id TEXT
         );
         CREATE TABLE events (
           seq INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT NOT NULL, kind TEXT NOT NULL,
           agent_id TEXT, execution_id TEXT, payload TEXT NOT NULL
         );
         CREATE TABLE checkpoints (
           id TEXT PRIMARY KEY, agent_id TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
           execution_id TEXT NOT NULL, parent_id TEXT, format_version INTEGER NOT NULL,
           seq INTEGER NOT NULL, payload TEXT NOT NULL, created_at TEXT NOT NULL,
           UNIQUE (agent_id, seq)
         );
         CREATE TABLE state_variables (
           agent_id TEXT NOT NULL, execution_id TEXT NOT NULL, key TEXT NOT NULL,
           value TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY (agent_id, key)
         );
         CREATE TABLE memory (
           agent_id TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
           description TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
           PRIMARY KEY (agent_id, key)
         );
         CREATE TABLE tool_calls (
           id TEXT NOT NULL, agent_id TEXT NOT NULL, execution_id TEXT NOT NULL,
           tool_name TEXT NOT NULL, args_json TEXT NOT NULL, status TEXT NOT NULL,
           result_json TEXT, error_json TEXT, started_at TEXT, finished_at TEXT,
           PRIMARY KEY (execution_id, id)
         );
         CREATE TABLE permission_requests (
           id TEXT PRIMARY KEY, agent_id TEXT NOT NULL, tool_call_id TEXT,
           resource TEXT NOT NULL, action TEXT NOT NULL, status TEXT NOT NULL,
           requested_at TEXT NOT NULL, decided_at TEXT
         );
         INSERT INTO kern_meta (key, value) VALUES ('schema_version', '1');
         INSERT INTO kern_meta (key, value) VALUES ('instance_id', 'legacy');
         PRAGMA user_version = 1;",
    )
    .expect("build v1 schema");
    conn.execute(
        "INSERT INTO agents (id, name, spec_version, config_json, lifecycle_state, created_at, updated_at)
         VALUES ('a-legacy', 'legacy', 1, '{}', 'created', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        [],
    )
    .expect("insert legacy agent");
    conn.execute(
        "INSERT INTO permission_requests (id, agent_id, tool_call_id, resource, action, status, requested_at)
         VALUES ('pr-legacy', 'a-legacy', 'call-1', 'network:host', 'GET api.example.com', 'pending', '2026-01-01T00:00:00Z')",
        [],
    )
    .expect("insert legacy pending request");
    drop(conn);

    // Store::open migrates v1 → v2.
    let store = Store::open(dir.path()).expect("open migrates");
    let legacy = store
        .get_permission_request("pr-legacy")
        .expect("legacy request readable after migration");
    assert_eq!(legacy.status, PermissionStatus::Pending);
    assert!(
        legacy.expires_at.is_none(),
        "legacy rows have no expiry (nullable column)"
    );

    // A legacy pending request stays decidable (NULL expiry = no deadline).
    let decided = store
        .decide_permission_request(&legacy.id, true)
        .expect("legacy request is decidable");
    assert_eq!(decided.status, PermissionStatus::Granted);

    // Fresh rows carry an expiry.
    let agent_id = "a-legacy";
    let fresh = store
        .create_permission_request_with_ttl(
            agent_id,
            Some("call-2"),
            "network:host",
            "GET api.example.com",
            std::time::Duration::from_secs(60),
        )
        .expect("create fresh post-migration");
    assert!(fresh.expires_at.is_some());
}
