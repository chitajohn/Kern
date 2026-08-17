//! Deterministic fault injection — test infrastructure.
//!
//! The runtime's failure handling is proven by two complementary means:
//!
//! 1. **Real process termination** — `crates/kern-cli/tests/recovery.rs`
//!    SIGKILLs the daemon at a hostile moment and proves checkpoint/restore.
//!    It is a black box: the kill lands wherever the scheduler happens to be.
//! 2. **This harness** — each recovery-relevant *persisted-write boundary* of
//!    the store can be scripted to fail at a chosen occurrence count, so the
//!    engine's error path is exercised deterministically at every point where
//!    a storage failure could corrupt, lose, or duplicate state.
//!
//! The matrix test (`crates/kern-core/tests/fault_injection.rs`) runs the same
//! scripted agent with every instrumented write boundary failing on the 1st
//! (and, for the highest-value windows, later) occurrence and asserts the
//! invariants: no hang, no silent state loss, no duplicated tool execution,
//! structured failure, monotonic event sequences, and recovery to a
//! consistent terminal state.
//!
//! The injector is deliberately **not** environment-driven (a process-global
//! env var would leak into parallel tests) and **not** reachable through the
//! API/CLI: it is constructed explicitly via `Store::open_with_faults`, which
//! is `#[doc(hidden)]` and used only by tests. When absent (`Store::open`),
//! the cost is a single `Option` check per instrumented operation.
//!
//! `ErrorCode::StorageFailure` is used for injected errors: a mid-write
//! storage failure is the realistic failure class, and the store maps every
//! real SQLite failure onto the same code.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::error::{ErrorCode, KernError};

/// A fault script for one named fault point: the 1-based occurrence numbers
/// of that store operation that must fail.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FaultScript {
    fail_at: Vec<usize>,
}

impl FaultScript {
    /// Fail the given 1-based occurrences (sorted, deduplicated).
    pub fn fail_at(occurrences: impl IntoIterator<Item = usize>) -> Self {
        let mut fail_at: Vec<usize> = occurrences.into_iter().collect();
        fail_at.sort_unstable();
        fail_at.dedup();
        Self { fail_at }
    }

    /// The configured occurrence numbers (test observation).
    pub fn occurrences(&self) -> &[usize] {
        &self.fail_at
    }
}

/// Per-point bookkeeping behind the injector's mutex.
#[derive(Debug, Default)]
struct Inner {
    scripts: HashMap<String, FaultScript>,
    /// 1-based count of calls seen so far per point.
    occurrences: HashMap<String, usize>,
}

/// Thread-safe fault injector. Clone-free by design: one `Arc<FaultInjector>`
/// is shared by the store (and therefore the event bus, checkpoint manager,
/// lifecycle, engine, and every tool) so occurrence counters are exact across
/// the whole runtime.
#[derive(Debug, Default)]
pub struct FaultInjector {
    inner: Mutex<Inner>,
}

impl FaultInjector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure a fault point. Calling `set` replaces any previous script
    /// for that point and resets its occurrence counter.
    pub fn set(&self, point: &str, script: FaultScript) {
        let mut inner = self.inner.lock().expect("fault injector mutex poisoned");
        inner.scripts.insert(point.to_string(), script);
        inner.occurrences.remove(point);
    }

    /// Remove a fault point (and its counter).
    pub fn clear(&self, point: &str) {
        let mut inner = self.inner.lock().expect("fault injector mutex poisoned");
        inner.scripts.remove(point);
        inner.occurrences.remove(point);
    }

    /// Called at the entry of every instrumented store operation. Returns the
    /// injected error when the current occurrence is scripted, else `None`.
    /// Counters advance on every call to a CONFIGURED point so `fail_at` is a
    /// position in the total call sequence, not in the failing subsequence;
    /// unconfigured points are a no-op (no counter, no allocation).
    pub fn try_fail(&self, point: &str) -> Option<KernError> {
        let mut inner = self.inner.lock().expect("fault injector mutex poisoned");
        let script = inner.scripts.get(point)?.clone();
        let n = inner.occurrences.entry(point.to_string()).or_insert(0);
        *n += 1;
        if script.fail_at.contains(n) {
            Some(KernError::new(
                ErrorCode::StorageFailure,
                format!("injected fault: {point} (occurrence {n})"),
            ))
        } else {
            None
        }
    }

    /// The current occurrence count for a point (test observation).
    pub fn occurrences_of(&self, point: &str) -> usize {
        self.inner
            .lock()
            .expect("fault injector mutex poisoned")
            .occurrences
            .get(point)
            .copied()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unconfigured_point_never_fails() {
        let injector = FaultInjector::new();
        assert!(injector.try_fail("anything").is_none());
        // Unconfigured points advance no counter (pure no-op path).
        assert_eq!(injector.occurrences_of("anything"), 0);
    }

    #[test]
    fn scripted_occurrences_fail_in_order() {
        let injector = FaultInjector::new();
        injector.set("append_event", FaultScript::fail_at([2, 3]));
        assert!(injector.try_fail("append_event").is_none(), "call 1 passes");
        let err = injector.try_fail("append_event").expect("call 2 fails");
        assert_eq!(err.code(), ErrorCode::StorageFailure);
        assert!(err.message.contains("append_event"));
        let _ = injector.try_fail("append_event").expect("call 3 fails");
        assert!(injector.try_fail("append_event").is_none(), "call 4 passes");
    }

    #[test]
    fn points_are_independent() {
        let injector = FaultInjector::new();
        injector.set("a", FaultScript::fail_at([1]));
        injector.set("b", FaultScript::fail_at([1]));
        assert!(injector.try_fail("a").is_some());
        // `b` has its own counter: its first call still fails.
        assert!(injector.try_fail("b").is_some());
        assert!(injector.try_fail("a").is_none());
    }

    #[test]
    fn set_resets_the_counter() {
        let injector = FaultInjector::new();
        injector.set("x", FaultScript::fail_at([1]));
        let _ = injector.try_fail("x");
        injector.set("x", FaultScript::fail_at([1]));
        assert!(injector.try_fail("x").is_some(), "counter reset by set");
    }

    #[test]
    fn occurrences_are_deduped_and_sorted() {
        let script = FaultScript::fail_at([3, 1, 3, 2]);
        assert_eq!(script.occurrences(), &[1, 2, 3]);
    }
}
