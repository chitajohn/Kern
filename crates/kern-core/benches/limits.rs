//! Benchmarks for the SPEC.md §19 informational targets (not gated):
//!
//! - SQLite event append  < 5 ms
//! - checkpoint create    < 20 ms
//! - agent loop overhead  < 50 ms/step (excluding model/tool latency)
//!
//! These measure the LOCAL store and the in-process mock-driven engine loop
//! against a real temp database — the same artifacts the tests exercise, so
//! the numbers are honest proxies for loop/checkpoint/event cost on this
//! machine. Run with `cargo bench -p kern-core`.

use std::sync::Arc;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use kern_core::config::parse_agent_spec;
use kern_core::engine::Engine;
use kern_core::event::EventBus;
use kern_core::store::{Agent, Checkpoint, LifecycleState, Store};
use kern_model::gateway::ModelGateway;
use kern_model::mock::{MockProvider, ScriptedStep};
use serde_json::{json, Value};

/// The agent spec used by the loop-overhead bench: mock provider + noop tool,
/// no checkpoint interval (so the hot path is session → model call → finish).
const LOOP_SPEC: &str =
    "version: 1\nname: loop\nmodel:\n  provider: mock\n  model: test\ntools:\n  - noop\n";

fn event_append(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    c.bench_function("store/event_append", |b| {
        b.iter(|| {
            store
                .append_event(
                    "agent.thinking",
                    Some("agent-a"),
                    None,
                    json!({ "text": "reasoning" }),
                )
                .unwrap();
        });
    });
}

fn checkpoint_create(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let agent = Agent::new("cp-agent", Value::Null, LifecycleState::Created);
    store.create_agent(&agent).unwrap();
    let mut seq = 0i64;
    c.bench_function("store/checkpoint_create", |b| {
        b.iter(|| {
            seq += 1;
            store
                .create_checkpoint(&Checkpoint::new(
                    &agent.id,
                    "ex-1",
                    seq,
                    json!({ "step": seq, "session": [] }),
                ))
                .unwrap();
        });
    });
}

fn agent_loop_overhead(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let bus = EventBus::new(Arc::clone(&store));
    // Looping mock: every run deterministically serves `finish` (one model
    // call per run) — the measured fixed cost of the §8.1 loop.
    let provider = MockProvider::looping([ScriptedStep::Finish("done".into())]);
    let mut gateway = ModelGateway::new();
    gateway.register(Arc::new(provider)).unwrap();
    let engine = Engine::new(Arc::clone(&store), bus, Arc::new(gateway), 8);

    let mut seq = 0u64;
    c.bench_function("engine/loop_overhead_per_run", |b| {
        b.iter_batched(
            || {
                // A fresh agent per run (the mock loops, so every run is a
                // deterministic one-turn finish). The insert is ~50 µs against
                // a multi-ms run — negligible setup noise.
                seq += 1;
                let spec = parse_agent_spec(LOOP_SPEC).unwrap();
                let agent = Agent::new(
                    format!("loop-{seq}"),
                    serde_json::to_value(&spec).unwrap(),
                    LifecycleState::Created,
                );
                store.create_agent(&agent).unwrap();
                agent.id
            },
            |id| rt.block_on(engine.run_agent(&id, None)).unwrap(),
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(
    name = limits;
    config = Criterion::default().sample_size(50);
    targets = event_append, checkpoint_create, agent_loop_overhead
);
criterion_main!(limits);
