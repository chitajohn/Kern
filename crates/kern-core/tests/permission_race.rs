//! Regression tests.
//!
//! A permission request is decidable exactly once. A stale or replayed
//! decision must never flip an earlier one: a `grant` replaying a `deny`
//! (or vice versa) is rejected with `PERMISSION_REQUEST_ALREADY_DECIDED`,
//! because the engine reads decided requests after a decision lands and a
//! flipped row would let a denied action execute.
//!
//! Written as an integration test against the public store API so the CAS
//! guard is exercised through the same surface the API/CLI use.

use kern_core::error::ErrorCode;
use kern_core::store::{Agent, LifecycleState, PermissionStatus, Store};
use serde_json::Value;

fn test_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open store");
    (dir, store)
}

#[test]
fn conflicting_decision_is_rejected_and_original_stands() {
    let (_dir, store) = test_store();
    let agent = Agent::new("perm-race", Value::Null, LifecycleState::Created);
    store.create_agent(&agent).expect("create agent");

    let req = store
        .create_permission_request(
            &agent.id,
            Some("call-1"),
            "filesystem:write",
            "write ./workspace/out.txt",
        )
        .expect("create request");
    assert_eq!(req.status, PermissionStatus::Pending);

    // Deny first, then a stale grant arrives: rejected, stays denied.
    let denied = store
        .decide_permission_request(&req.id, false)
        .expect("deny");
    assert_eq!(denied.status, PermissionStatus::Denied);
    let err = store
        .decide_permission_request(&req.id, true)
        .expect_err("conflicting grant must be rejected");
    assert_eq!(err.code(), ErrorCode::PermissionRequestAlreadyDecided);
    let after = store.get_permission_request(&req.id).expect("reload");
    assert_eq!(
        after.status,
        PermissionStatus::Denied,
        "a deny must never be flipped to grant"
    );

    // Replaying the SAME decision is idempotent (client retry safety).
    let again = store
        .decide_permission_request(&req.id, false)
        .expect("idempotent replay of the same decision");
    assert_eq!(again.status, PermissionStatus::Denied);

    // Symmetric: grant first, then a stale deny is rejected.
    let req2 = store
        .create_permission_request(
            &agent.id,
            Some("call-2"),
            "network:host",
            "GET api.example.com",
        )
        .expect("create request 2");
    store
        .decide_permission_request(&req2.id, true)
        .expect("grant");
    let err = store
        .decide_permission_request(&req2.id, false)
        .expect_err("conflicting deny must be rejected");
    assert_eq!(err.code(), ErrorCode::PermissionRequestAlreadyDecided);
    assert_eq!(
        store
            .get_permission_request(&req2.id)
            .expect("reload")
            .status,
        PermissionStatus::Granted
    );
}

#[test]
fn missing_request_still_reports_not_found_not_conflict() {
    let (_dir, store) = test_store();
    let err = store
        .decide_permission_request("no-such-id", true)
        .expect_err("missing request");
    assert_eq!(err.code(), ErrorCode::PermissionRequestNotFound);
}
