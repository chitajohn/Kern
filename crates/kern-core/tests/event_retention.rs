//! Event retention: with the opt-in knob,
//! only the newest N events per agent survive pruning — the one previously
//! unbounded growth path. The newest events always survive, so replay and
//! recovery keep the live tail of the execution record.

use kern_core::store::Store;
use serde_json::json;

fn test_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open store");
    (dir, store)
}

#[test]
fn prune_keeps_only_the_newest_per_agent() {
    let (_dir, store) = test_store();

    // Two agents with different event counts, interleaved.
    for i in 0..10 {
        store
            .append_event("tool.completed", Some("a"), Some("e1"), json!({ "n": i }))
            .unwrap();
        store
            .append_event("tool.completed", Some("b"), Some("e2"), json!({ "n": i }))
            .unwrap();
    }
    // Runtime events with no agent are bucketed separately.
    for _ in 0..5 {
        store
            .append_event("runtime.started", None, None, json!({}))
            .unwrap();
    }

    let total = store.latest_event_seq().unwrap();
    assert_eq!(total, 25);

    let pruned = store.prune_events(3).unwrap();
    assert_eq!(pruned, 16, "7 per agent (10-3) x2 + 2 runtime (5-3)");

    assert_eq!(store.event_count_for_agent("a").unwrap(), 3);
    assert_eq!(store.event_count_for_agent("b").unwrap(), 3);
    // The survivors are the NEWEST events of each agent.
    let tail = store.events_for_agent_after("a", 0, 10).unwrap();
    let ns: Vec<i64> = tail
        .iter()
        .map(|e| e.payload["n"].as_i64().unwrap())
        .collect();
    assert_eq!(ns, vec![7, 8, 9], "newest three survive: {ns:?}");
    // The global max seq is a survivor (it is in every bucket's newest tail),
    // so the live cursor still advances monotonically after pruning.
    assert_eq!(store.latest_event_seq().unwrap(), 25);
}

#[test]
fn prune_is_idempotent_and_safe_to_repeat() {
    let (_dir, store) = test_store();
    for i in 0..6 {
        store
            .append_event("agent.started", Some("a"), Some("e1"), json!({ "n": i }))
            .unwrap();
    }
    assert_eq!(store.prune_events(2).unwrap(), 4);
    assert_eq!(
        store.prune_events(2).unwrap(),
        0,
        "second prune deletes nothing"
    );
    assert_eq!(store.event_count_for_agent("a").unwrap(), 2);
}

#[test]
fn retention_of_one_still_keeps_a_replayable_tail() {
    let (_dir, store) = test_store();
    for i in 0..4 {
        store
            .append_event("agent.started", Some("a"), Some("e1"), json!({ "n": i }))
            .unwrap();
    }
    store.prune_events(1).unwrap();
    assert_eq!(store.event_count_for_agent("a").unwrap(), 1);
    let tail = store.events_for_agent_after("a", 0, 10).unwrap();
    assert_eq!(tail[0].payload["n"].as_i64().unwrap(), 3);
}
