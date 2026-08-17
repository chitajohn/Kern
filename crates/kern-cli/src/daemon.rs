//! `kern daemon` — the foreground runtime process (SPEC.md §16).
//!
//! Startup: open the store (acquiring `daemon.lock` — a second daemon on the
//! same data dir is refused), emit `runtime.started`, mark interrupted agents
//! `recovering` (scheduler), and recover them (checkpoint restore + runner
//! respawn). Then it serves the local HTTP API (§15). On SIGINT/SIGTERM the
//! graceful shutdown flips the engine's shutdown watch: every runner
//! checkpoints and pauses, SSE streams close, and the daemon exits.
//!
//! Providers: openai/anthropic/ollama are registered from their env config;
//! `mock` is always registered — with `KERN_TEST_MOCK_SCRIPT` (a JSON array of
//! scripted steps) when set, which is how the recovery integration test drives
//! deterministic runs across daemon restarts.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kern_core::api::ApiState;
use kern_core::engine::Engine;
use kern_core::event::{payload, EventBus, EventKind};
use kern_core::lifecycle::Lifecycle;
use kern_core::recovery::RecoveryManager;
use kern_core::scheduler::Scheduler;
use kern_core::store::Store;
use kern_core::version::{KERN_VERSION, STORAGE_SCHEMA_VERSION};
use kern_model::anthropic::AnthropicProvider;
use kern_model::gateway::ModelGateway;
use kern_model::mock::{MockProvider, ScriptedStep};
use kern_model::ollama::OllamaProvider;
use kern_model::openai::OpenAiProvider;
use kern_model::types::ToolCall as ModelToolCall;
use serde::Deserialize;
use serde_json::Value;

/// Maximum concurrently executing agents (SPEC.md §17 default).
const MAX_CONCURRENT_AGENTS: usize = 8;

pub async fn run(home: Option<PathBuf>) -> Result<(), String> {
    let home = home
        .or_else(|| std::env::var_os("KERN_HOME").map(PathBuf::from))
        .unwrap_or_else(default_home);
    std::fs::create_dir_all(&home).map_err(|e| format!("create {}: {e}", home.display()))?;

    let store =
        Arc::new(Store::open(&home).map_err(|e| format!("open store at {}: {e}", home.display()))?);
    let bus = EventBus::new(Arc::clone(&store));
    let gateway = Arc::new(build_gateway()?);
    let engine = Engine::new(
        Arc::clone(&store),
        bus.clone(),
        Arc::clone(&gateway),
        MAX_CONCURRENT_AGENTS,
    );

    bus.emit(
        EventKind::RuntimeStarted,
        None,
        None,
        payload::runtime_started("local", STORAGE_SCHEMA_VERSION, KERN_VERSION),
    )
    .await
    .map_err(|e| format!("emit runtime.started: {e}"))?;

    // Event retention (opt-in): keep the newest N events per agent. Prune at
    // startup, then periodically, so long-running agents cannot grow the
    // event table without bound.
    let event_retention = std::env::var("KERN_EVENT_RETENTION")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0);
    if let Some(retention) = event_retention {
        match store.prune_events(retention) {
            Ok(pruned) => {
                tracing::info!(pruned, retention, "event retention: startup prune done");
            }
            Err(err) => {
                tracing::warn!(error = %err, "event retention: startup prune failed");
            }
        }
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(6 * 60 * 60));
            loop {
                interval.tick().await;
                match store.prune_events(retention) {
                    Ok(pruned) if pruned > 0 => {
                        tracing::info!(pruned, retention, "event retention: periodic prune");
                    }
                    Ok(_) => {}
                    Err(err) => {
                        tracing::warn!(error = %err, "event retention: periodic prune failed");
                    }
                }
            }
        });
    }

    // 1. Mark agents left `starting|running|waiting` by a dead daemon.
    let lifecycle = Arc::new(Lifecycle::new(Arc::clone(&store), bus.clone()));
    let scheduler = Scheduler::new(
        Arc::clone(&store),
        lifecycle,
        engine.clone(),
        MAX_CONCURRENT_AGENTS,
    );
    let interrupted = scheduler
        .reconcile_interrupted()
        .await
        .map_err(|e| format!("reconcile interrupted agents: {e}"))?;
    tracing::info!(marked = interrupted, "startup reconciliation done");

    // 2. Recover them (checkpoint restore + runner respawn).
    let recovery = RecoveryManager::new(engine.clone());
    let summary = recovery
        .recover_interrupted()
        .await
        .map_err(|e| format!("recover interrupted agents: {e}"))?;
    tracing::info!(
        recovered = summary.recovered,
        deferred = summary.skipped,
        failed = summary.failed,
        "recovery sweep done"
    );

    // 3. Supervision: runner-liveness sweep. An agent whose
    //    lifecycle says `starting|running|waiting` but whose runner is gone
    //    beyond a 60s grace is failed with `RUNNER_LOST` instead of staying
    //    stuck forever — the paths crash-recovery cannot see (a spawn that
    //    never ran, a runner task lost to an internal bug, an in-daemon hang).
    let sweep_engine = engine.clone();
    let sweep_task = tokio::spawn(async move {
        let grace = std::time::Duration::from_secs(60);
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
        let mut shutdown = sweep_engine.shutdown_receiver();
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    match sweep_engine.supervisor_sweep(grace).await {
                        Ok(summary) if summary.failed > 0 => {
                            tracing::warn!(failed = summary.failed, "supervisor sweep failed stuck runners");
                        }
                        Ok(_) => {}
                        Err(err) => {
                            tracing::warn!(error = %err, "supervisor sweep failed");
                        }
                    }
                }
                _ = shutdown.changed() => break,
            }
        }
    });

    // 4. Schedules: initialize missing `next_run_at`s and start the timer
    //    (bounded-cadence due firing until shutdown).
    let initialized = scheduler
        .reconcile_schedules()
        .await
        .map_err(|e| format!("reconcile schedules: {e}"))?;
    tracing::info!(initialized = initialized, "schedule reconciliation done");

    // 5. Durable sleep: sleeping agents persisted their wake time;
    //    a daemon that was down past that time wakes them immediately here
    //    (missed wakes collapse). Future wakes are fired by the timer loop.
    let woken = scheduler
        .reconcile_sleeping()
        .await
        .map_err(|e| format!("reconcile sleeping agents: {e}"))?;
    tracing::info!(woken = woken, "sleeping-agent reconciliation done");
    let timer = scheduler.timer_loop(engine.shutdown_receiver());
    let timer_task = tokio::spawn(timer);

    // TEST-ONLY hook (`kern run` replaces this): start a fresh
    // agent by id so the recovery integration test can exercise the full
    // crash/restart cycle through the real binary.
    if let Ok(autostart) = std::env::var("KERN_TEST_AUTOSTART_AGENT") {
        let engine = engine.clone();
        tokio::spawn(async move {
            if let Err(err) = engine.run_agent(&autostart, None).await {
                tracing::error!(agent_id = %autostart, error = %err, "autostarted agent failed");
            }
        });
    }

    // 5. Serve the local API (SPEC §15). Graceful shutdown: on
    //    SIGINT/SIGTERM the signal handler flips the engine's shutdown watch
    //    (runners checkpoint + pause at their safe points; SSE streams end),
    //    then axum drains in-flight connections and the daemon drains the
    //    runners below.
    let token = load_api_token(&home)?;
    let api_state = ApiState {
        store: Arc::clone(&store),
        engine: engine.clone(),
        bus: bus.clone(),
        gateway: gateway.clone(),
        token,
        shutdown: engine.shutdown_receiver(),
    };
    let addr: SocketAddr = std::env::var("KERN_API_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8787".to_string())
        .parse()
        .map_err(|e| format!("invalid KERN_API_ADDR: {e}"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind API listener at {addr}: {e}"))?;
    let local = listener
        .local_addr()
        .map_err(|e| format!("read API address: {e}"))?;
    tracing::info!(address = %local, "local API listening (override with KERN_API_ADDR)");

    // Register shutdown-signal handling before serving: SIGINT/SIGTERM on
    // unix, Ctrl-C on Windows (the only portable console signal). A
    // registration failure aborts startup loudly rather than leaving the
    // daemon unable to stop gracefully.
    #[cfg(unix)]
    let shutdown_signal = {
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .map_err(|e| format!("install SIGINT handler: {e}"))?;
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|e| format!("install SIGTERM handler: {e}"))?;
        Box::pin(async move {
            tokio::select! {
                _ = sigint.recv() => {}
                _ = sigterm.recv() => {}
            }
        })
    };
    #[cfg(not(unix))]
    let shutdown_signal = Box::pin(async move {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("install Ctrl-C handler: {e}");
        }
    });
    let shutdown_engine = engine.clone();
    let server = axum::serve(listener, kern_core::api::router(api_state)).with_graceful_shutdown(
        async move {
            shutdown_signal.await;
            tracing::info!("shutdown signal received; checkpointing and pausing runners");
            shutdown_engine.request_shutdown();
        },
    );
    server.await.map_err(|e| format!("API server: {e}"))?;

    let _ = timer_task.await; // the scheduler stops on the same signal
    let _ = sweep_task.await; // the supervisor stops on the same signal
                              // Runners checkpoint and pause at their next safe point; drain them
                              // (bounded — a stuck runner must not hang shutdown forever).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while engine.active_count() > 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    if engine.active_count() > 0 {
        tracing::warn!(
            active = engine.active_count(),
            "runner drain timed out; aborting remaining tasks"
        );
        engine.shutdown();
    }

    bus.emit(
        EventKind::RuntimeShuttingDown,
        None,
        None,
        payload::runtime_shutting_down(),
    )
    .await
    .map_err(|e| format!("emit runtime.shutting_down: {e}"))?;
    tracing::info!("daemon stopped");
    Ok(())
}

/// The API bearer token: `KERN_TOKEN` wins, else `$KERN_HOME/token`. `None`
/// means the API is unauthenticated — only when no token exists at all.
fn load_api_token(home: &Path) -> Result<Option<String>, String> {
    if let Ok(token) = std::env::var("KERN_TOKEN") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Ok(Some(token));
        }
    }
    match std::fs::read_to_string(home.join("token")) {
        Ok(contents) => {
            let token = contents.trim().to_string();
            if token.is_empty() {
                tracing::warn!("$KERN_HOME/token is empty; the API is unauthenticated");
            }
            Ok((!token.is_empty()).then_some(token))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("read {}: {err}", home.join("token").display())),
    }
}

fn default_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".kern")
}

/// Register all providers. The mock is always available (local testing); a
/// `KERN_TEST_MOCK_SCRIPT` env var supplies its scripted steps as JSON.
fn build_gateway() -> Result<ModelGateway, String> {
    let mut gateway = ModelGateway::new();
    let mock = match std::env::var("KERN_TEST_MOCK_SCRIPT") {
        Ok(script) => {
            let steps: Vec<MockStepJson> = serde_json::from_str(&script)
                .map_err(|e| format!("invalid KERN_TEST_MOCK_SCRIPT: {e}"))?;
            MockProvider::new(steps.into_iter().map(MockStepJson::into_step))
        }
        Err(_) => MockProvider::finishing("mock provider: no script configured"),
    };
    gateway
        .register(Arc::new(mock))
        .map_err(|e| format!("register mock provider: {e}"))?;
    gateway
        .register(Arc::new(OpenAiProvider::from_env()))
        .map_err(|e| format!("register openai provider: {e}"))?;
    gateway
        .register(Arc::new(AnthropicProvider::from_env()))
        .map_err(|e| format!("register anthropic provider: {e}"))?;
    gateway
        .register(Arc::new(OllamaProvider::from_env()))
        .map_err(|e| format!("register ollama provider: {e}"))?;
    Ok(gateway)
}

/// JSON wire format for `KERN_TEST_MOCK_SCRIPT`.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum MockStepJson {
    Finish { text: String },
    Thinking { text: String },
    ToolCalls { calls: Vec<MockCallJson> },
}

#[derive(Debug, Deserialize)]
struct MockCallJson {
    id: String,
    name: String,
    args: Value,
}

impl MockStepJson {
    fn into_step(self) -> ScriptedStep {
        match self {
            MockStepJson::Finish { text } => ScriptedStep::Finish(text),
            MockStepJson::Thinking { text } => ScriptedStep::Thinking(text),
            MockStepJson::ToolCalls { calls } => ScriptedStep::ToolCalls(
                calls
                    .into_iter()
                    .map(|c| ModelToolCall {
                        id: c.id,
                        name: c.name,
                        arguments: c.args,
                    })
                    .collect(),
            ),
        }
    }
}
