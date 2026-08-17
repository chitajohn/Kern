# Kern Architecture

**Status:** Draft for v0.1.0 (pre-release)
**Owner:** Kern core maintainers
**Applies to:** Kern v0.1.0

This document is the authoritative description of the Kern v0.1.0 runtime architecture: the
decisions we made, why we made them, the risks we accepted, and the boundaries of the system.

It is written to be read alongside:

- `SPEC.md` — the normative contract (state machine, data schemas, event catalog, API, config).

---

## 1. What Kern is

Kern is an open-source runtime that executes AI agents as **durable software processes**.

An agent is a declarative configuration (see `SPEC.md §Agent configuration`) that combines a
model, a set of tools, memory, and a permission policy. Kern executes that configuration with:

- a **lifecycle** (created → starting → running → … → completed/failed/terminated),
- **durable state** that survives process death,
- **memory primitives** (durable, agent-scoped key/value storage exposed as tools),
- **checkpoints** that capture enough of an execution to resume it,
- **recovery** that restores interrupted executions when the runtime restarts,
- a **tool system** where every tool has a name, description, input schema, permission
  requirements, and observable execution,
- a **permission engine** that enforces policy *independently of the model*,
- a **sandbox** that constrains what tool processes can touch,
- a **structured event stream** that makes every meaningful action observable and replayable,
- a **scheduler** for recurring and one-shot scheduled agent runs,
- a **local HTTP API** and a **CLI** as control interfaces to the same runtime.

Kern is **not** a chat application, a prompt library, or an agent framework that prescribes an
application loop. It is the runtime layer underneath agents.

---

## 2. Design decisions (summary)

| # | Decision | Rationale (short) |
|---|----------|-------------------|
| D1 | Rust + `tokio` async runtime | Process management, sandboxing, concurrency, cross-platform; strong ecosystem; single language in core. |
| D2 | Cargo workspace with 4 crates | Enforces component boundaries (core / model / tool / cli) at compile time. |
| D3 | SQLite (WAL) as the durable store | Local-first, transactional, crash-safe, zero-ops, single-file, testable. |
| D4 | Versioned JSON for checkpoints & state payloads | Human-inspectable, debuggable, trivially versionable; binary formats deferred until needed. |
| D5 | Tool calls are **effectively-once** (at-least-once execution + result dedup) | Exactly-once is not generally achievable for side-effecting tools; dedup by tool-call id prevents double execution of recorded results. |
| D6 | `deny` by default; explicit allow/ask policy | Model output is untrusted; the runtime is the security boundary. Fail closed. |
| D7 | Sandbox backends per OS, fail-closed when required sandboxing is unavailable | Honest, documented security posture (Linux: bubblewrap, rlimit fallback for best-effort; macOS: seatbelt, deprecated+untested-on-CI; Windows: none in v0.1 — job objects deferred). |
| D8 | Local HTTP/JSON API on `127.0.0.1` + SSE; CLI is a thin client | Cross-platform, simple, stable interface; matches "CLI is a control interface, not the runtime". |
| D9 | One async task per running agent; bounded concurrency | No unbounded concurrency; backpressure is explicit. |
| D10 | Provider adapters use raw HTTP via `reqwest` (no provider SDKs) | Keeps dependencies intentional, gives full control over timeouts/retries/errors, easy fixture testing. |
| D11 | `tracing` for structured logs; events persisted + broadcast + SSE | Two observability layers: logs for operators, events for systems. |
| D12 | Secrets only via environment variables; never persisted | Nothing sensitive written to disk; redaction at logging boundaries. |
| D13 | Parallel tool-call batches from one model response | Modern models return multiple calls; sequential execution would be slow and wasteful. |
| D14 | Memory primitives exposed as policy-gated tools | v0.1 scope ("Memory primitives"); observable, permissioned, model-driven. |
| D15 | Minimal scheduler (cron/every/at) in v0.1 | v0.1 scope ("Scheduling"); recurring agents are core to "long-running". |
| D16 | Checkpoint retention + DB-row-assisted restore dedup | Bounds storage growth; shrinks the re-execution window on recovery. |
| D17 | Global + per-agent tool concurrency caps | Prevents unbounded subprocess creation. |

Sections 3–13 expand each decision with enough detail to implement against.

---

## 3. Technology evaluation

### 3.1 Language choice: Rust

Rust was evaluated against the requirements rather than assumed. The evaluation:

**Requirements that drove the choice**

- Concurrency: many agents running concurrently, each with async I/O (model calls, tool calls).
- Process management: tools are child processes (shell, scripts); must be spawned, killed,
  time-boxed, and resource-limited.
- Networking: HTTP client for model providers, HTTP server for the local API.
- Filesystem: safe path handling for the filesystem tool and checkpoint storage.
- Sandboxing: OS-level isolation (bubblewrap on Linux — no seccomp filter in v0.1 —, seatbelt on macOS, job objects on
  Windows) is only realistically accessible from a systems language.
- Reliability: memory safety, no GC pauses, deterministic resource management.
- Cross-platform: one codebase targeting Linux/macOS/Windows, with `cfg(target_os)` isolating
  platform-specific sandbox code.

**Alternatives considered**

- **Go:** strong for servers, but OS sandboxing and process-control ergonomics are weaker, and
  the toolchain story for fine-grained OS primitives is worse than Rust's.
- **TypeScript/Node:** fine for a framework, poor fit for sandboxing, process control, and the
  security posture Kern requires. Node is present in this repo's dev environment but is not a
  candidate for the core runtime.
- **C/C++:** no safety guarantees; unacceptable for a runtime that executes untrusted tool code.
- **Zig/Nim/…:** ecosystem too thin for networking + async + SQLite + cross-platform tooling.

**Verdict: Rust.** The decision is not "Rust because it is conventional"; it is Rust because the
requirement set (process management, sandboxing, cross-platform, reliability) is exactly the
problem space Rust was built for.

### 3.2 Async runtime: `tokio`

- Multi-threaded worker runtime.
- One `AgentRunner` task per running agent, tracked in a task registry.
- All blocking work (SQLite writes, filesystem) is pushed to `spawn_blocking` or a dedicated
  writer task; the engine loop never blocks.
- Timeouts everywhere: model calls, tool calls, and lifecycle operations are wrapped in
  `tokio::time::timeout`.

### 3.3 Storage: SQLite (decision D3)

**Why not other options**

- **Plain files + JSON:** no transactions across entities (agent + execution + events +
  checkpoints must be consistent as a unit); no atomicity guarantees; corruption handling is
  manual. Violates Kern's state/persistence requirements.
- **sled / redb (embedded KV):** solid, but we would still hand-roll schema, transactions, and
  migrations on top; SQLite gives us ACID, WAL, and 30 years of hardening for free.
- **Postgres / external DB:** violates local-first; requires a server process.
- **In-memory only:** violates durability by definition.

**How we use it**

- Single database file: `$KERN_HOME/state.db` (default `~/.kern/state.db`).
- `PRAGMA journal_mode=WAL` for concurrent readers + single writer.
- Schema version managed by `PRAGMA user_version` plus a `kern_meta` table carrying
  `schema_version`, `runtime_version`, and `instance_id`. Migrations are forward-only,
  versioned modules; a fresh database migrates from 0 to current in order.
- One logical writer: all writes go through a single owned connection (wrapped in a small
  `Store` facade) to avoid `SQLITE_BUSY` churn; reads may use a second pooled connection.
  `tokio::task::spawn_blocking` or a dedicated writer task keeps the async runtime non-blocking.
- Every state mutation that must be consistent is one transaction (e.g. transition
  `running → paused` + append `agent.paused` event + update `updated_at` commit atomically).

Schema DDL lives in `SPEC.md §Storage schema` and is normative.

### 3.4 Serialization: versioned JSON (decision D4)

- Checkpoints, agent configs, event payloads, and tool arguments/results are JSON documents
  with an explicit `format_version`/`version` field.
- Versioning strategy: additive evolution — a reader must accept a document with a *lower*
  `format_version` than current and reject a *higher* one with a structured "unsupported
  version" error (never silently upgrade).
- Cost/benefit: JSON is debuggable and diff-able, which matters more in v0.1 than bytes on
  disk. `postcard`/`bincode` are noted as a future optimization, gated behind the versioned
  envelope so a format swap is possible without breaking old checkpoints.

---

## 4. Workspace and crate layout (decision D2)

```
Kern/
├── Cargo.toml              # workspace
├── crates/
│   ├── kern-core/          # runtime: lifecycle, engine, store, events, checkpoint,
│   │                       #   recovery, permissions, sandbox, scheduler, config, API server
│   ├── kern-model/         # ModelProvider trait, gateway, adapters (openai, anthropic,
│   │                       #   ollama, mock), timeout/retry policy
│   ├── kern-tool/          # Tool trait, ToolRegistry, builtins (filesystem, http, shell,
│   │                       #   noop/sleep for tests), input-schema validation
│   └── kern-cli/           # binary `kern`: daemon mode + client subcommands
├── ARCHITECTURE.md
└── SPEC.md
```

Crate boundaries exist to enforce composability:

- `kern-core` depends on `kern-model` and `kern-tool` *traits only*. It never names a concrete
  provider or tool.
- `kern-model` and `kern-tool` know nothing about the runtime; they are pure libraries.
- `kern-cli` is the only place that assembles the pieces (dependency injection at the root).

This keeps model providers, tools, storage, and the API independently replaceable — a provider
or tool can be swapped without touching the core, and a user's custom tool can be added as a
small crate implementing the `Tool` trait.

---

## 5. Core abstractions and domain model

The domain entities (normative definitions in `SPEC.md §Domain model`):

| Entity | Responsibility |
|--------|----------------|
| `Agent` | Named, validated configuration (model, tools, memory, permissions, runtime knobs) plus lifecycle state. Durable. |
| `Execution` | One run of an agent from start to a terminal state. An agent may have many executions over time; at most one is active at a time. Durable. |
| `Session` | In-memory execution context: message history, current step, variables, pending tool call. Reconstructed from checkpoints. |
| `Checkpoint` | Durable, versioned snapshot of a session + metadata, linked to its parent. |
| `MemoryEntry` | Durable, agent-scoped key/value memory (survives executions), accessed via tools. |
| `Event` | Immutable, sequentially numbered, persisted record of a runtime action. |
| `ToolCall` | Durable record of a requested tool invocation, its status, and its result/error. The dedup key for tool execution. |
| `PermissionRequest` | Pending allow/deny decision surfaced when policy evaluates to `ask`. |
| `StateVariable` | Key/value durable memory scoped to an agent + execution. |

### 5.1 The `Runtime` struct

`kern-core` exposes one assembly point:

```rust
pub struct Runtime {
    store: Store,                    // SQLite facade (D3)
    events: EventBus,                // persistence + broadcast (D11)
    lifecycle: Lifecycle,            // state machine + transition validation
    tasks: TaskRegistry,             // live AgentRunner handles, shutdown signaling
    scheduler: Scheduler,            // start/recover decisions, concurrency limit
    gateway: Arc<dyn ModelGateway>,  // constructed from config
    tools: ToolRegistry,             // constructed from config
    permissions: PermissionEngine,   // policy evaluation (D6)
    sandbox: Sandbox,                // per-OS backend (D7)
    checkpoints: CheckpointManager,
    recovery: RecoveryManager,
    config: RuntimeConfig,
}
```

Components talk to each other only through these facades; there is no shared mutable global
state. This makes the runtime embeddable (tests construct a `Runtime` directly over a temp
directory) and later reusable by a desktop app or server build.

---

## 6. Agent lifecycle

### 6.1 State machine

States (normative):

```
created → starting → running ⇄ paused
                       │  ↑
                       ↓  │
                    waiting (permission ask)
                       │
                       ↓
                   recovering
                       │
                       ↓
          ┌────────────┴────────────┐
          ▼                         ▼
      completed / failed        terminated
```

Valid transitions (enforced by the `Lifecycle` component; any other transition is a
programming error):

| From         | Trigger                          | To          | Notes |
|--------------|----------------------------------|-------------|-------|
| created      | `start`                          | starting    | Agent runner spawned |
| starting     | runner ready                     | running     | execution.started emitted |
| starting     | initialization error             | failed      | structured error attached |
| running      | `pause`                          | paused      | runner suspended, checkpointed |
| running      | permission policy = `ask`        | waiting     | permission.requested event emitted |
| running      | model/tool loop finished         | completed   | final checkpoint, execution.completed |
| running      | unrecoverable error              | failed      | execution.failed |
| running      | `terminate`                      | terminated  | runner aborted |
| waiting      | permission granted + resume      | running     | permission.granted event |
| waiting      | permission denied                | running     | denial fed back to model as tool error |
| waiting      | `pause` / `terminate`            | paused / terminated | |
| paused       | `resume`                         | running     | restored from checkpoint first |
| paused       | `terminate`                      | terminated  | |
| recovering   | recovery completes               | running     | checkpoint.restored event |
| recovering   | recovery fails                   | failed      | |
| recovering   | `terminate`                      | terminated  | |

Terminal states (`completed`, `failed`, `terminated`) have no outgoing transitions. A terminal
agent can only run again as a *new execution*.

### 6.2 Transition atomicity

A state transition is a single SQLite transaction:

1. validate the transition against the state machine,
2. update `agents.lifecycle_state` (and `executions.status` where applicable),
3. append the corresponding event (e.g. `agent.paused`),
4. commit.

If the process dies mid-transaction, the database rolls back to the previous consistent state.
This is the durability backbone of the lifecycle.

---

## 7. Execution engine

### 7.1 The agent loop

Each running agent is driven by one async task. The loop:

```
loop {
    session = restore_session(agent)            // from memory or latest checkpoint
    request = build_completion_request(session) // system prompt + history + tool specs + memory
    event(model.requested)

    response = gateway.complete(request)        // with timeout + retries (D10)

    match response {
        Finish(reason, text) => {
            event(model.completed)
            session.finish(text)
            checkpoint()                        // final
            transition(running → completed)
            break
        }
        Thinking(text) => {
            event(agent.thinking)               // reasoning surfaced to UIs/CLI; no state change
            continue
        }
        ToolCalls(batch) => {                   // 1..N tool calls from one model response
            event(model.completed)
            for call in batch { validate args; record_tool_call(call, requested) }  // one tx
            checkpoint()                                    // BEFORE the batch (D5)

            resolved = classify(batch)          // per call: Deny | Ask | Allow (§10)
            if any Ask {
                create PermissionRequests; event(permission.asked)
                transition(running → waiting); checkpoint()
                await decisions                             // resume via API
                // granted → Allow; denied → Deny
            }

            pending = [Allow calls]
            results = execute_batch(pending)    // bounded concurrency (§7.4, D17)
            for (call, result) in results {
                record_tool_call(call, terminal, result)    // one tx, durable (D5)
                event(tool.completed | tool.failed)
            }
            session.feed_tool_results(all classified + results)
            checkpoint()                                    // AFTER the batch
        }
    }

    if session.step >= max_steps { fail with "step limit exceeded" }
    if session.checkpoint_due(checkpoint_interval) { checkpoint() }
}
```

### 7.4 Parallel tool calls

Modern models frequently return multiple tool calls in a single response. Kern executes a
batch as one unit: all calls are validated and recorded, one checkpoint is taken before the
batch and one after, and `Allow`-classified calls run concurrently under two semaphores — a
per-agent parallelism cap (`max_concurrent_tools`, default 4) and a global process cap. The
model receives every result (or denial) back in a single follow-up turn.

**Ordering caveat (documented):** tools in a batch run concurrently; the model must not assume
ordering or exclusivity between batch members. State-changing tools should be issued in
separate turns when ordering matters, or be idempotent by `tool_call_id` (D5).

### 7.2 Effectively-once tool semantics (decision D5)

Exactly-once execution of a side-effecting tool is not achievable in general (the tool may
execute and the process dies before the result is recorded). Kern therefore provides:

- **checkpoint before the batch** — on recovery we know which tool calls were *requested*;
- **durable tool-call records keyed by `tool_call_id`** — a recorded `completed`/`failed`
  result is never re-executed. Restore consults both the checkpoint payload and the
  `tool_calls` table: a call with a terminal record is replayed, never re-executed
  (algorithm in `SPEC.md §11.2`);
- **model calls are at-least-once** — they have no side effects besides cost, so a lost
  response is simply re-requested on restore (bounded by retry policy);
- **documented contract**: tools must be idempotent if they have side effects, because a crash
  between process-spawn and result-record can re-execute. Tools receive their `tool_call_id`
  in `ToolContext` so they can dedupe themselves (§9.4).

This yields *effectively-once* for well-behaved tools and *at-most-once per recorded result*
always.

### 7.3 Bounded history

Message history is bounded (token/step budget, configurable). When the budget is exceeded, the
oldest messages are summarized or dropped and a `session.history_trimmed` marker is recorded in
the checkpoint payload. This prevents unbounded memory/disk growth (a performance
requirement) while keeping the model within its context window. v0.1 uses a simple
oldest-drop policy with an explicit marker and a character-based token approximation
(~4 chars ≈ 1 token); per-provider tokenizers and summarization are v0.2 concerns.

---

## 8. Model gateway (`kern-model`)

### 8.1 Trait

```rust
#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn id(&self) -> &str;                       // "openai" | "anthropic" | "ollama" | "mock" | custom
    async fn complete(&self, req: &CompletionRequest)
        -> Result<CompletionResponse, ModelError>;
}

pub struct CompletionRequest {
    pub provider: String,                       // selects the registered adapter
    pub model: String,
    pub messages: Vec<Message>,                 // role + content (+ tool results as tool messages)
    pub tools: Vec<ToolSpec>,                   // name, description, JSON Schema input
    pub max_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub timeout: Option<Duration>,              // per-agent model.timeout (gateway default 60s)
    pub retries: Option<u32>,                   // per-agent model.retries (gateway default 2)
}

pub enum CompletionResponse {
    Finish { reason: FinishReason, text: String },
    Thinking(String),                           // reasoning text (surface as agent.thinking)
    ToolCalls(Vec<ToolCall>),                   // 1..N calls, id, name, args (JSON)
}
```

`provider`/`timeout`/`retries` are filled from the validated agent config so a shared gateway
can enforce per-agent policy.

The `ModelGateway` is a registry of adapters (registered by `id()`, duplicate ids rejected)
plus the policy layer: per-attempt `timeout` (default 60s), exponential backoff between
retries (`250ms * 2^n`, capped at 4s), and budget semantics per SPEC §8.2 — transient kinds
(`Timeout`, `Unavailable`, `RateLimited`) are retried up to `model.retries` (default 2);
`Auth` and `InvalidResponse` fail immediately; an exhausted transient budget surfaces as
`BudgetExhausted` (or `Timeout` when the final failure was a timeout). Adapters are stateless
(`Send + Sync`, shared via `Arc<dyn ModelProvider>`) and never enforce policy themselves.

### 8.2 v0.1 providers

- `openai` — Chat Completions API. Env: `OPENAI_API_KEY`. Optional `OPENAI_BASE_URL` for
  compatible gateways.
- `anthropic` — Messages API. Env: `ANTHROPIC_API_KEY`. Optional `ANTHROPIC_BASE_URL`.
- `ollama` — local models. `OLLAMA_BASE_URL` (default `http://localhost:11434`); no key
  required. Ollama does not assign tool-call ids, so the adapter synthesizes deterministic
  `ollama-<index>` ids (stable across a crash-replay of the same recorded response).
- `mock` — deterministic provider used by tests and demos: scripted responses (finish text, a
  sequence of tool calls, or multi-call batches) driven by a fixture. This is what makes the
  engine tests deterministic despite model nondeterminism.

No provider SDKs: adapters are raw `reqwest` calls (decision D10). Rationale: fewer, larger
dependencies; precise control over timeouts; recorded-response fixtures for tests; no risk of
SDK upgrades silently changing behavior.

### 8.3 Errors

Model failures are **never hidden**. They surface as `model.failed` events with a
structured error, and — depending on policy — either the execution retries (bounded by
`model_retries` with backoff) or the agent transitions to `failed` with the error attached. A
timeout is a first-class error kind, not a hang.

---

## 9. Tool system (`kern-tool`)

### 9.1 Trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> &Value;           // JSON Schema object
    async fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> Result<Value, ToolError>;
}

pub struct ToolContext<'a> {
    pub agent_id: &'a str,
    pub execution_id: &'a str,
    pub tool_call_id: &'a str,                  // dedup key for tool-side idempotency (§11.1)
}
```

`input_schema` returns a reference (schemas are static or `OnceLock`-cached), `run` takes
`&ToolContext` (the context is per-call, not per-agent), and policy (timeout, sandbox,
concurrency) is applied by the `ToolExecutor`/permission engine, never by the tool, so tools
stay policy-free and testable in isolation. Tool policy (filesystem roots, http hosts, memory
caps) is carried by constructor fields on each builtin, configured by the engine from the
validated agent spec.

### 9.2 Registry and memory seam

`ToolRegistry` maps name → boxed tool (with a compiled JSON Schema validator cached at
registration — `TOOL_INVALID_ARGUMENTS` on bad args) and exposes `ToolSpec` snapshots for
model requests. Runtime config determines which tools an agent can see; an agent configured
with `tools: [filesystem]` does not get `shell` in its model context at all.

Memory builtins need the `memory` table, but `kern-tool` must not depend on `kern-core`
(dependency direction is core → tool). The seam is `MemoryProvider`, an async trait
implemented by `kern-core` over the store (offloading rusqlite via `spawn_blocking`); the
memory tools are plain `kern-tool` builtins constructed with the provider.

### 9.3 v0.1 builtins

| Tool | Capability | Permission class | Notes |
|------|-----------|------------------|-------|
| `filesystem` | read/write/list/stat within allowed roots | `filesystem:{read,write}` on roots | Paths canonicalized; symlink escape prevented; never follows links out of roots |
| `http` | GET/POST with JSON/text bodies | `network:{host}` | Host allowlist enforced in the engine, not the tool; TLS verified; response size-capped |
| `memory.read` / `memory.write` / `memory.list` | durable agent-scoped KV | `memory:{read,write}` on keys | Backed by the `MemoryProvider` (§9.5); key globs; size caps |
| `shell` | run a command via the sandbox | `shell` | **Disabled unless the agent policy explicitly enables it** (D6); sandbox required by default; output size-capped |
| `noop`, `sleep` | test/demo | `none` | Used by tests and the crash-recovery suite |

Filesystem and HTTP tools enforce policy *inside* the tool too (defense in depth), but the
authoritative decision is the permission engine's.

### 9.4 Tool execution

- Arguments are validated against the tool's JSON Schema **before** execution; invalid
  arguments produce a structured `ToolError::InvalidArguments` fed back to the model (not a
  crash).
- `ToolContext` carries the `tool_call_id`, the agent's sandbox, workdir, and timeout — tools
  receive their id so they can implement their own idempotency on top of the engine's dedup
  (D5).
- Every execution is time-boxed (`tool_timeout`), output size-capped, and recorded in
  `tool_calls`. Concurrency is bounded by per-agent and global semaphores (D17).
- Tool crashes (non-zero exit, panic, sandbox failure) become `tool.failed` events with
  structured errors; the agent loop decides whether to continue (feeding the error to the
  model) or fail, per agent policy.

### 9.5 Memory primitives

Memory primitives are part of Kern's v0.1 scope. In v0.1, memory is **durable,
agent-scoped key/value storage exposed as tools** — not invisible prompt injection:

- `MemoryProvider` trait (replaceable); the default implementation is the
  SQLite-backed `memory` table (survives executions and restarts).
- Tools `memory.read(key)`, `memory.write(key, value, description?)`, `memory.list(prefix?)`
  — ordinary tools with ordinary policy (`permissions.memory.read/write`, glob-matched keys),
  so memory access is observable (`tool.*` events) and enforceable.
- Optional `memory.inject_digest: true` prepends a compact digest of relevant entries to the
  system prompt each step (off by default; avoids prompt bloat).
- Caps: per-key size, key count, and agent total (configurable; defaults prevent unbounded
  growth).

Why tools, not hidden context: memory becomes inspectable, permissioned, and model-driven —
the agent decides what to remember, the runtime enforces policy, and events make it visible.
Pluggable backends (file, vector, external) are drop-in via the trait.

---

## 10. Permission engine and sandbox

### 10.1 Policy model (decision D6)

Policy is expressed per resource class. Evaluation for a request:

1. **Deny rules win** over allow rules over ask rules (most specific match wins within a class).
2. Default is **deny** for anything not explicitly allowed.
3. Paths are canonicalized before matching; roots and rules support globs.
4. Network rules match host (optionally `host:port`); IP literals are normalized.

The agent config in `SPEC.md §Agent configuration` shows the policy surface:

```yaml
permissions:
  filesystem:
    read:
      allow: [./workspace]
      ask:   [./shared/**]
      deny:  [./workspace/secret/**]
    write:
      allow: [./workspace]
  network:
    allow: [api.github.com]
    # ask:  [...]          # optional
    # deny: [...]          # optional
  memory:
    read:
      allow: ["*"]        # glob-matched keys
    write:
      allow: ["*"]
  shell:
    enabled: false        # fail closed
    sandbox: required     # required | best-effort | off
```

`ask` mode: a rule may resolve to `ask`, which creates a durable `PermissionRequest`, emits
`permission.asked`, and moves the agent to `waiting`. The decision is delivered through the API
(`POST /api/v1/permissions/{id}/grant|deny`). A denial is fed back to the model as a tool
error; a grant proceeds to execution. This proves the runtime — not the model — owns
authorization.

### 10.2 Sandbox backends (decision D7)

| OS | Backend | v0.1 posture |
|----|---------|--------------|
| Linux | `bubblewrap` (`bwrap`) when installed | unshared namespaces (net/pid/mount/ipc/uts), read-only root with writable workspace, rlimits, dropped capabilities, `--die-with-parent`. No seccomp filter in v0.1 (documented). |
| Linux (no bwrap) | rlimits fallback (`best-effort` only) | CPU/FSIZE/NOFILE limits in the child; no network or memory isolation. If an agent requires `shell` with `sandbox: required`, the agent **fails to start** with `SandboxError::Unavailable`. Fail closed. |
| macOS | `sandbox-exec` (seatbelt) | deny-default profile: read-only root except workspace, no network; `sandbox-exec` is deprecated by Apple and untested on CI — documented as such. |
| Windows | none in v0.1 | `sandbox: required` fails closed; `best-effort` logs the limitation and continues without OS isolation (in-tool containment only). Job objects + restricted tokens are deferred. |
| Windows | Job objects + restricted tokens | process limits + restricted token; documented as the weakest backend in v0.1 |

Honesty requirement: `SPEC.md §Sandbox` and the README must state exactly what each backend
does and does not contain. We never claim a boundary that is not enforced.

The filesystem and HTTP tools also enforce their own path/host constraints (defense in depth),
so even in `best-effort` mode the *tools* remain constrained; only the OS-level process
boundary weakens.

---

## 11. Checkpointing and recovery

### 11.1 Checkpoint contents (format v1, normative in `SPEC.md §Checkpoint format`)

```json
{
  "format_version": 1,
  "checkpoint_id": "uuid",
  "agent_id": "uuid",
  "execution_id": "uuid",
  "parent_checkpoint_id": null,
  "created_at": "2026-08-14T12:00:00Z",
  "lifecycle_state": "running",
  "step": 12,
  "messages": [ ... ],
  "pending_tool_call": null,
  "tool_calls": [ ... ],          // requested + completed calls with results (dedup source)
  "variables": { ... },
  "memory_refs": [],
  "runtime_meta": { "provider": "openai", "model": "gpt-4o-mini" }
}
```

Checkpoints are stored in SQLite as rows (not loose files) so checkpoint creation and state
mutation are the same atomic unit, and restore does not have to reconcile two stores.

### 11.2 When we checkpoint

- before and after every tool execution (the crash-recovery critical points),
- on pause/terminate,
- on lifecycle transitions that leave a resumable state (`waiting`, `paused`, `recovering`),
- on `checkpoint_interval` during long loops,
- final checkpoint on completion.

### 11.3 Recovery procedure

At daemon startup, `RecoveryManager` runs:

1. Read all agents in a non-terminal state with no live runner.
2. For each, load the latest checkpoint; validate `format_version` (reject future versions with
   a structured error — never silently read).
3. Reconstruct the session from the checkpoint (messages, step, variables, tool records).
4. Resolve every `requested` tool call against both the checkpoint payload and the
   `tool_calls` table (precise algorithm in `SPEC.md §11.2`): a terminal record is replayed
   (dedup, D5); a `running`/missing record is re-executed (the documented at-least-once
   window).
5. Transition `→ recovering`, emit `checkpoint.restored`, then `→ running` if
   `auto_recover: true` (default) or wait for explicit `resume`.
6. A recovery failure transitions the agent to `failed` with the error attached; the database is
   left untouched (recovery is read + one atomic transition).

### 11.4 The required proof

Recovery is not "done" because the code compiles. The acceptance test (`SPEC.md §Acceptance criteria`) is a real interruption:

```
daemon starts → agent runs → checkpoint created → SIGKILL the daemon
→ restart daemon → checkpoint restored → agent continues → agent completes
```

This runs against the real binary in an integration test.

---

## 12. Events, observability, and API

### 12.1 Event system (decision D11)

- Envelope: `{ seq, ts, kind, agent_id, execution_id, payload }`, persisted in `events`.
- `seq` is a monotonic `AUTOINCREMENT` key — durable, ordered, and the cursor for replay and
  resumable SSE streams.
- Live subscribers receive events via `tokio::sync::broadcast`; a subscriber joining late can
  replay from any `seq` (SQLite read) and then switch to the live stream.
- Event catalog (normative list in `SPEC.md §Event catalog`):

```
agent.created          agent.started       agent.paused       agent.resumed
agent.waiting          agent.completed     agent.failed       agent.terminated
execution.started      execution.completed execution.failed   execution.restored
model.requested        model.completed     model.failed
tool.requested         tool.started        tool.completed     tool.failed
checkpoint.created     checkpoint.restored checkpoint.failed
permission.asked       permission.granted  permission.denied
runtime.started        runtime.shutting_down scheduler.recovered_agent
```

### 12.2 Logging

- `tracing` + `tracing-subscriber`; structured JSON to `$KERN_HOME/logs/runtime.jsonl` plus
  human-readable console output.
- A redaction layer wraps every logging macro: env values, API keys, bearer tokens, and
  `Authorization` headers are never logged. Tool arguments are logged only under a
  `log_tool_args` opt-in (default off). This is enforced by a logging guard + audit test that
  greps recorded logs for key patterns.

### 12.3 Local API (decision D8)

`axum` HTTP/JSON server bound to `KERN_API_ADDR` (default `127.0.0.1:8787`). Full contract in
`SPEC.md §API`. Highlights:

| Method & path | Purpose |
|---|---|
| `POST /api/v1/agents` | create agent (validate spec; no execution) |
| `GET /api/v1/agents` · `GET /api/v1/agents/{id}` | list / inspect |
| `POST /api/v1/agents/{id}/start` · `/pause` · `/resume` · `/terminate` | lifecycle |
| `POST /api/v1/agents/{id}/checkpoint` | create checkpoint now |
| `GET /api/v1/agents/{id}/checkpoints` · `POST /api/v1/agents/{id}/checkpoints/{cid}/restore` | checkpoint management |
| `GET /api/v1/agents/{id}/events?after={seq}` · `GET /api/v1/events/stream?after={seq}` | event replay / SSE stream |
| `GET /api/v1/executions/{id}` | execution detail |
| `GET /api/v1/executions/{id}/transcript` | ordered full transcript (model turns, tool calls, results) |
| `GET /api/v1/agents/{id}/executions` | execution history for an agent |
| `GET /api/v1/tools` · `GET /api/v1/models` | capability discovery |
| `GET /api/v1/permissions/pending` · `POST /api/v1/permissions/{id}/grant` · `/deny` | permission approvals |
| `GET /api/v1/health` | liveness |

- Errors are structured JSON: `{ "error": { "code": "AGENT_NOT_FOUND", "message": "...", "detail": {...} } }`.
- Optional bearer-token auth: `kern init` generates `$KERN_HOME/token`; CLI presents it; API
  rejects requests without it when `KERN_TOKEN` or the token file exists. Binding is loopback by
  default so v0.1 does not depend on the token for local safety, but honors it.

### 12.4 CLI

`kern` binary modes:

- `kern daemon` — run the runtime in the foreground (v0.1; no background-service install).
  Holds an exclusive `daemon.lock` on the data dir; a second daemon refuses to start
  (`STORAGE_LOCKED`).
- `kern init` — scaffold `agent.yaml` + generate API token.
- `kern run agent.yaml` — create + start an agent (prints agent id); `--wait` follows the run
  to completion and exits with the agent's status.
- `kern doctor` — environment health report (DB integrity, schema version, sandbox backend
  availability, provider key presence, API reachability).
- `kern ps` · `kern logs <agent>` · `kern inspect <agent>` — status/observability (logs tail
  events from the store, not terminal scrapes; `ps` shows `next_run_at` for scheduled agents).
- `kern schedule <agent>` — show the agent's schedule and next run time.
- `kern pause|resume|checkpoint|terminate <agent>` — lifecycle control.
- `kern tools` · `kern models` · `kern version`.

Every CLI command is a thin client over the API (decision D8); it never touches the database or
internal state directly.

---

## 13. Scheduler

Scheduling is part of Kern's v0.1 scope. The scheduler is deliberately small but real:

- **Schedules** from agent config: `every: 12h` (interval), `cron: "0 3 * * *"` (cron
  expression), or `at: <RFC3339>` (one-shot), with an optional `timezone` (default UTC).
- `agents.next_run_at` is maintained durably; a timer task wakes on the nearest due time and
  starts an execution (emitting `scheduler.run_due`).
- `skip_if_running: true` (default) skips a due run when an execution is active and recomputes
  the next run.
- `next_run_at` is recomputed after every execution start/finish and at daemon startup (cron
  semantics with a monotonic timer; wall-clock recompute on startup).
- **Concurrency limiting**: `max_concurrent_agents` semaphore; excess starts queue or fail per
  config (default: queue with `waiting` event).

The `Scheduler` owns schedules, the semaphore, and the queue; the `Lifecycle` component
validates state changes; neither can force an invalid transition.

---

## 14. Configuration

Two configuration surfaces (normative in `SPEC.md §Configuration`):

1. **Agent config** (`agent.yaml`, versioned, `deny_unknown_fields`): `version`, `name`,
   `description`, `model`, `tools`, `memory`, `permissions`, `schedule`, `runtime`. Parsed and
   fully validated at create time; invalid config fails fast with a line-referencing error and
   the agent is never created.
2. **Runtime config**: env vars + optional `$KERN_HOME/kern.toml`. Env:
   `KERN_HOME`, `KERN_API_ADDR`, `KERN_TOKEN`, `KERN_LOG`, `KERN_MAX_CONCURRENT_AGENTS`,
   `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `OLLAMA_BASE_URL` (+ optional provider base URLs).

Secrets are **environment-only** (D12): agent configs may reference `env:OPENAI_API_KEY`
indirectly (the runtime resolves), but keys are never stored in the database, checkpoints, or
events. The redaction layer (12.2) covers the log path.

---

## 15. Error taxonomy

All errors are typed and structured. Top-level kinds:

```
Config          Storage        Model       Tool
Permission      Sandbox        Timeout     NotFound
State           Serialization  Internal
```

- Each kind carries a stable `code` for API/CLI consumers (e.g. `MODEL_TIMEOUT`,
  `PERMISSION_DENIED`, `CHECKPOINT_FORMAT_UNSUPPORTED`).
- Failures are never swallowed: every `Err` path either retries (bounded), surfaces as a
  structured event, or transitions the agent to `failed` with the error attached.
- `Internal` errors panic in debug/test builds and surface as structured bugs in release.

`SPEC.md §Error codes` is the normative table.

---

## 16. Security model

Trust boundaries (untrusted → runtime):

```
model output ──┐
tool arguments ┤
agent config   ├─► PermissionEngine ─► Tool execution (sandboxed)
external data  ┘        │
                         ▼
                Deny / Ask / Allow
```

- The model can never execute anything directly; every action crosses the permission engine.
- Tool implementations additionally enforce their own constraints (path roots, host
  allowlists, output caps) — defense in depth.
- Shell commands are the highest-risk surface: disabled unless explicitly enabled, sandboxed
  when enabled, and `required` by default (fail closed).
- Secrets: env-only, redacted, never logged or persisted.
- Credential boundary: every tool subprocess starts from a scrubbed environment (a non-secret
  allowlist) — provider keys and `KERN_TOKEN` are never inherited by a tool process, on any
  sandbox backend (§22.5).
- Local API: loopback bind, optional bearer token, structured errors that do not leak internal
  details (stack traces never leave the daemon).

Threat model summary and per-OS sandbox limitations are documented in `SPEC.md §Security`.

---

## 17. Cross-platform matrix

| Capability | Linux | macOS | Windows |
|-----------|-------|-------|---------|
| Core runtime (lifecycle, engine, state, events, API, CLI) | ✅ | ✅ | ✅ |
| Sandbox backend | `bwrap` (namespaces) → `landlock` (kernel LSM, Linux ≥ 5.13) → rlimits; `required` fails closed when no tier is available | seatbelt (deprecated, documented) | none in v0.1 (`required` fails closed) |
| Shell tool | sandboxed (tier above) | sandboxed | in-tool containment only |
| Filesystem/http tools | enforced in-tool on all platforms | same | same |
| Tool process limits | CPU/FSIZE/NOFILE rlimits on every tier; `RLIMIT_AS` when `runtime.tool_memory_limit_mb` is set | n/a | n/a |
| Network isolation | bwrap only (netns) | seatbelt deny-network | none |
| Credential boundary | scrubbed env allowlist, every tier | same | same |

Platform-specific code lives behind `#[cfg(target_os)]` in `kern-core::sandbox`; everything
else is portable. Honest limitation notes ship in the README.

---

## 18. Known risks and mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| Rust toolchain not present | cannot build | toolchain bootstrap (rustup) is a prerequisite; documented in CONTRIBUTING.md |
| `bubblewrap` missing on some Linux hosts | shell tool unavailable | fail-closed with actionable error; `best-effort` opt-in with documented limits |
| Provider API drift | adapters break | raw-HTTP adapters with recorded-fixture tests; versioned, isolated code |
| SQLite single-writer | write contention under many agents | one writer task, batched event appends, WAL; v0.1 concurrency is modest by design |
| Model nondeterminism in tests | flaky tests | deterministic `mock` provider + fixture-driven adapter tests |
| Exactly-once not achievable | possible duplicate tool effect on crash | effectively-once semantics (D5) + idempotency contract for tools |
| Windows sandbox immaturity | weaker isolation on Windows | explicit documentation; in-tool enforcement always on |
| Unbounded history/events | disk/memory growth | bounded history (§7.3), event retention policy (configurable, default keep-all with size warning) |
| Blocking calls in async engine | runtime stall | all blocking work offloaded (`spawn_blocking` / writer task); enforced by review + stress test |
| Parallel batch ordering | model assumes ordering → wrong results | documented ordering caveat (§7.4); tools idempotent by `tool_call_id` |
| Cron/timezone drift | missed or double runs | UTC default, monotonic timer, recompute at startup, `skip_if_running` |
| Two daemons on one data dir | state corruption | exclusive `daemon.lock`, `STORAGE_LOCKED` refusal |

---

## 19. Non-goals for v0.1

- Distributed execution, remote workers, agent migration (v0.3 roadmap).
- Multi-user / multi-tenant / cloud control plane.
- Sub-agents / agents-as-tools (v0.2; the `Tool` trait is the hook).
- Token-level model streaming and mid-run user input (v0.2; `agent.thinking` covers reasoning
  visibility in v0.1).
- Memory providers beyond the durable KV store (provider trait exists; file/vector backends
  land in v0.2).
- A web UI (desktop/web clients are future application layers on the same API).
- Exactly-once tool execution (see §7.2).
- Background-service installation for the daemon (v0.1 runs foreground).

---

## 20. Design decisions

| ID | Decision |
|----|----------|
| D1 | Rust + tokio |
| D2 | Cargo workspace, 4 crates |
| D3 | SQLite WAL, single file |
| D4 | Versioned JSON payloads |
| D5 | Effectively-once tool semantics |
| D6 | Deny-by-default permission engine |
| D7 | Per-OS sandbox, fail-closed |
| D8 | HTTP/JSON + SSE local API, thin CLI |
| D9 | One task per agent, bounded concurrency |
| D10 | Raw HTTP provider adapters, no SDKs |
| D11 | tracing logs + persisted/broadcast events |
| D12 | Env-only secrets, redaction |
| D13 | Parallel tool-call batches (see §2 table) |
| D14 | Memory as policy-gated tools |
| D15 | Minimal scheduler in v0.1 |
| D16 | Checkpoint retention + DB-assisted dedup |
| D17 | Tool concurrency caps |
| D18 | Engine loop: per-agent registry build, policy gate before every tool call, bounded parallel batches, poll-based `ask` parking (crash-safe, no signal races), bounded history trim (§8.4) |
| D19 | Tool rows are recorded `requested` then updated to terminal (`completed`/`failed`) — the dedup contract |
| D20 | Checkpointing: pre/post-batch + waiting + interval + final checkpoints; §7 payloads with `pending_tool_calls`; checkpoint row, execution link, event, and retention prune commit in one transaction |
| D21 | Restore semantics: terminal tool rows replay (never re-run), `requested` rows re-execute, `ask` re-resolves by tool call id (decided applies, pending re-parks); recovered transcripts never duplicate already-fed results |
| D22 | Recovery: interrupted agents with pending permission decisions stay `recovering` (manual resume); an execution with no checkpoint resumes from its stored input; recovery failure → `failed` with error |
| D23 | Graceful shutdown is cooperative: runners checkpoint + pause at safe points on a watch signal (never force-abort mid-work); SIGKILL remains the crash test |
| — | `~/.kern` data layout, loopback API, foreground daemon |
| — | Shell tool in v0.1, disabled by default |
| — | `ask`-mode permission approvals in v0.1 API surface |
| D24 | CLI: thin `reqwest` client over the daemon API mirroring the daemon's address/token resolution; `kern run` validates locally via the config parser but never touches the store; `permissions`/`grant`/`deny` added as a terminal surface for the ask flow (SPEC §16 table omits them — documented extension); `GET /health` reports the strongest sandbox backend for `doctor`/`version` |

---

## 21. Document relationships

- `SPEC.md` — normative: state machine table, SQLite DDL, event catalog, API contract, config
  schemas, checkpoint format, error codes, acceptance criteria.
- `README.md` — project-facing overview; must reflect sandbox limitations honestly.
- `CONTRIBUTING.md` — contributor workflow, CI gates, and code review expectations.
- `TOOL_AUTHORING.md` — the `Tool` trait contract and how to add a builtin or custom tool.

---

## 22. Limits audit

This section audits every unbounded surface in the runtime and the redaction gap. It is
deliberately honest about what v0.1 does **not** bound.

### 22.1 Enforced limits (inventory)

| Surface | Limit | Where |
|---------|-------|-------|
| Event bus channel | 1024 in-flight events per channel (backpressure, not unbounded memory) | `event/mod.rs` `DEFAULT_CHANNEL_CAPACITY` |
| Tool concurrency | 4 per agent, 16 globally (D17) | `tool/executor.rs` |
| Model call | 60 s default timeout; exponential backoff 250 ms base → 4 s max | `model/gateway.rs` |
| HTTP tool | 30 s timeout; 1 MiB max response body | `tool/builtins/http.rs` |
| Shell/process output | 1 MiB per process captured | `tool/process.rs` `DEFAULT_OUTPUT_CAP` |
| Memory tool | 100 keys, 64 KiB per value, 200 list entries | `tool/builtins/memory.rs` |
| Agent steps | `runtime.max_steps` (default 100), terminal failure on exceed | `engine.rs`, `config/mod.rs` |
| Session history | `runtime.max_history_tokens` (default 16 384) with trim flag | `engine.rs` `trim_history` |
| Checkpoint retention | `runtime.checkpoint_retention` (default 50) pruned atomically in the commit transaction (D16/D20) | `store/mod.rs` |
| API pagination | events page ≤ 10 000 rows; SSE replay cursor ≤ 10 000 rows | `api/mod.rs`, `api/sse.rs` |
| Park/timer polling | 250 ms permission-ask poll, 5 s scheduler tick (no busy loops) | `engine.rs`, `scheduler.rs` |
| Execution wall-clock | `runtime.max_duration` (default 0 = unbounded), anchored at start, re-anchored on restore, checked before every turn and while parked | `engine.rs`, `config/mod.rs` |
| Tool-call volume | `runtime.max_tool_calls` (default 0 = unbounded), counter serialized into every checkpoint | `engine.rs`, `config/mod.rs` |
| Tool memory (address space) | `runtime.tool_memory_limit_mb` (default none) → `RLIMIT_AS` on every Linux sandbox tier | `sandbox.rs`, `tool/process.rs` |
| Durable sleep threshold | `runtime.durable_sleep_min` (default 10 s); at-or-above sleeps park the agent with a durable `wake_at` | `engine.rs`, `config/mod.rs` |
| Approval window | `runtime.ask_timeout` (default 300 s); overdue pending requests CAS to `expired` ≡ denied | `store/mod.rs`, `engine.rs` |
| Supervisor sweep | 15 s cadence, 60 s grace (daemon-wired); fails `starting\|running\|waiting` agents whose runner is gone | `engine.rs`, `daemon.rs` |

### 22.2 Known unbounded surfaces (documented, not hidden)

1. **Persisted events are not pruned.** The store tallies per-agent events and emits a warning
   at 100 000 (`WARN_EVENTS_PER_AGENT`), but v0.1 has no event-retention pruning. A very
   long-lived agent will grow the DB unboundedly. Mitigation: retention pruning is a small,
   isolated extension point in `store`; until it exists, operators can archive/trim the
   `events` table manually. Flagged for v0.2.
2. **SSE client connections are unbounded in count.** Each is a read-only stream; the loopback
   deployment model makes this acceptable, but a shared multi-tenant host should cap it.
3. **No seccomp filter.** The sandbox matrix (§17) already states this; rlimit fallback does
   not isolate the network.
4. **Tool-result JSON in DB rows** is bounded only indirectly by the 1 MiB per-tool caps;
   payloads are not size-capped at the store layer.

### 22.3 Redaction wiring

The `redact()` core is applied by every `tracing` layer:

- `telemetry.rs` runs every `tracing` field through `redact()` in both the console and
  JSON file layers (`logs/runtime.jsonl`), so no provider key ever reaches a log line.
- The daemon initializes the JSON file layer (SPEC §8: `logs/runtime.jsonl`).
- `crates/kern-cli/tests/redaction.rs` runs a real daemon with fake `OPENAI_API_KEY` /
  `ANTHROPIC_API_KEY` values and asserts the key material appears in **neither** the console
  logs, the `runtime.jsonl`, nor persisted `events` rows.

### 22.4 Benchmark targets

Dev-profile criterion benches in `crates/kern-core/benches/limits.rs` (see README
"Benchmarks" for the measured numbers and environment caveats): event-append throughput,
checkpoint commit cost, and one agent-loop turn. Criterion is a dev-dependency only; benches
run on the dev profile with HTML reports disabled to keep workspace disk bounded.

### 22.5 Security hardening

1. **Landlock sandbox tier (Linux).** No-`bwrap` boxes previously had rlimits only — no
   filesystem containment beyond in-tool string checks (TOCTOU documented). A new `landlock`
   backend (kernel LSM, no external binary, Linux ≥ 5.13) enforces kernel-level write
   containment in the child pre-exec: read/execute anywhere, writes only in the agent
   workspace, `/tmp`, and `/dev/null`, plus the rlimits. Tier order: `bwrap` → `landlock` →
   `rlimits` (best-effort) / fail-closed (required). The ABI mask is probed **empirically**,
   not from the version number — 6.8.0-87-generic claims ABI 4 but rejects `IOCTL_DEV`.
2. **Credential boundary.** Tool subprocesses never inherit the daemon env; `SandboxedRunner`
   scrubs to a non-secret allowlist (`PATH`, `HOME`, locale, `TERM`, `TZ`, user identity,
   `SHELL`, `EDITOR`, `PAGER`) for every backend. Redaction protected logs; the scrub
   protects the agent (provider keys, `KERN_TOKEN` unreachable by `sh`).
3. **Approval TTL (schema v2).** `permission_requests.expires_at`; `ask` requests expire at
   `requested_at + runtime.ask_timeout` (default 300 s); the engine poll CASes overdue
   requests `expired`; decisions on expired requests → `PERMISSION_REQUEST_EXPIRED` (409);
   `expired` is treated exactly like `denied` — an agent can never park forever on a stale
   prompt, and a late `grant` can never resurrect a closed window.
4. **Event retention (opt-in).** `KERN_EVENT_RETENTION` keeps the newest N events per agent
   (pruned at daemon start + every 6 h); unbounded remains the default. The one previously
   unbounded growth path is now a knob.
5. **Provider integration tests.** `kern-model/tests/real_provider.rs` runs against a
   live OpenAI-compatible endpoint when `AGENTROUTER_API_KEY` is set (self-skips
   otherwise); `scripts/provider-smoke.sh` drives it.

---

## 23. Design decisions (additions)

| ID | Decision |
|----|----------|
| D25 | `redact()` is wired into both tracing layers; the daemon writes `logs/runtime.jsonl`; redaction integration test; criterion benchmarks on the dev profile (HTML reports off); unbounded surfaces above documented rather than silently accepted |
| D26 | Engine passes `host[:port]` (default from scheme) so port-scoped network rules are enforceable (port-less rules still match any port); permission decisions are CAS'd on `pending` (conflicting replay → `PERMISSION_REQUEST_ALREADY_DECIDED` 409, same-decision replay idempotent); shell children spawn in a process group and are killed group-wide on drop/timeout (unix; Windows Job Objects deferred); bracketed IPv6 host rules are exact-host syntax, not globs; API preserves 413 for oversized bodies (`REQUEST_TOO_LARGE`) |
| D27 | Linux sandbox tier gains `landlock` (kernel LSM write containment, empirically probed ABI mask; no-`bwrap` `required` works on Linux ≥ 5.13); tool subprocess envs are scrubbed to a non-secret allowlist for every backend (credential boundary); approval TTL via schema v2 (`expires_at` + `PERMISSION_REQUEST_EXPIRED`, expired ≡ denied); opt-in event retention (`KERN_EVENT_RETENTION`); gated real-provider tests (`AGENTROUTER_API_KEY`, self-skipping). Deferred items (routing → app layer, no streaming in v0.1, MCP/agent-to-agent/SDK post-v0.1) are recorded in §19 and §25.3 |

## 24. Execution budgets and failure containment

### 24.1 Execution budget (D28)

The runtime bounds a single execution in wall time and tool-call volume:

- `runtime.max_duration` — the deadline is anchored when the execution starts,
  re-anchored to the *remaining* time when a run is restored, and checked before
  every model turn and while the agent is parked on an approval. A run cannot
  park past its budget even on a human.
- `runtime.max_tool_calls` — the issued-call counter lives in `SessionState`,
  which is serialized into **every** checkpoint, so a recovered run keeps its
  budget. Old checkpoints deserialize via serde defaults.

Failing a budget emits `RUN_DURATION_EXCEEDED` / `TOOL_CALL_LIMIT_EXCEEDED`
(structured, attached to `agent.last_error`) — never a silent stop.

### 24.2 Runner panic containment

`run_runner_safely` runs the runner body in an inner task. A panic is isolated
at the task boundary, extracted from the `JoinError`, and fails the execution
with `RUNNER_PANIC`. The `RunnerAbortGuard` makes aborting the outer task
(pause/terminate/shutdown) abort the inner runner — no orphaned runner survives,
no agent hangs `running` with a dead task.

### 24.3 Task-input durability (schema v3)

`executions.input` persists the task input at execution creation. The
no-checkpoint recovery path (an execution interrupted before its first
checkpoint) re-seeds the session with the stored input instead of resuming
with an empty task. Migrations are transactional and dual-bookkeep
`PRAGMA user_version` + `kern_meta.schema_version`.

### 24.4 Scheduler crash-loop backoff

A consecutive-failure streak (default threshold 3,
`schedule.backoff_after_failures`) defers the next run exponentially
(30s..30min), emitting a catalog `scheduler.backoff` event. Disable per agent
with a large threshold or `backoff: none`.

### 24.5 Streaming stays out

Token-level streaming cannot coexist safely with the step-based checkpoint
boundary: a checkpoint taken mid-stream is not a valid resume point, and
token-by-token persistence multiplies storage and event volume. Streaming
belongs in a future abstraction where the *stream itself* is the durable unit
(checkpointed cursors), not the v0.1 turn-based one.

---

## 25. Supervision

### 25.1 Runner-liveness supervision (stuck-execution detection)

The recovery sweep runs at daemon startup and restores every interrupted
agent. It cannot see the *in-daemon* failure modes: a spawn that never ran, a
runner task lost to an internal bug, or a hang that outlives panic
containment. A background supervisor — `Engine::supervisor_sweep` — runs on a
15s cadence in `kern daemon` (60s grace) and fails any agent
whose lifecycle says `starting|running|waiting` but whose runner task is gone:

- **`starting`** — the execution row exists (`pending`) but the runner died
  before `mark_started`; anchored on the last transition timestamp.
- **`running`** — the runner is gone mid-run; anchored on the persisted
  `executions.started_at`, so the grace window survives daemon restarts.
- **`waiting`** — included deliberately: the park poll that seals expired
  permission requests lives inside the runner, so an orphaned waiting agent
  could otherwise hang forever on an undecided request.

The failure is structured (`RUNNER_LOST`, §13), emitted as
`execution.failed` + `agent.failed` through the normal lifecycle path, and the
transition is CAS'd on the current lifecycle state — an agent that legitimately
ended between the sweep's read and the fail (completed/paused/terminated) is
never double-failed. This required one transition-table amendment: `waiting →
failed` (SPEC §3.2) — an unrecoverable failure while waiting on a human must
fail the agent, never leave an unobservable zombie.

### 25.2 Private store files

`Store::open` restricts `state.db`, `state.db-wal`, `state.db-shm`, and
`daemon.lock` to 0600 on Unix (SQLite otherwise creates them with the umask,
commonly group/world readable). The database holds agent configs, tool
results, memory, and event history; file permissions are the local mitigation
for the copied-database threat. Proven by `crates/kern-core/tests/store_perms.rs`.

### 25.3 Deferred from v0.1

Explicitly kept out of v0.1: per-tool
credential injection, model routing, MCP/SDKs, agent-to-agent tasks, token
accounting, event compaction, and CPU/memory/disk budgets (enforcement
belongs to the sandbox tier). The event system already carries global
sequence numbers and per-execution filtering; the transcript endpoint
reconstructs an execution in order — sufficient for v0.1 without an
event-sourcing layer.

## 26. Durable wake/sleep

### 26.1 The problem

An agent that sleeps for an hour held its runner task — and with it a
concurrency slot, a thread, and an idle model connection — for the whole
duration. Worse, the sleep lived only in-process: a daemon restart silently
lost it. Long-running agents need the runtime itself to own the sleep.

### 26.2 The design

A `sleep` tool call at or above `runtime.durable_sleep_min` (default 10s) is
NOT executed in-process. The batch handler:

1. Computes `wake_at = now + ms` and records the call **terminal** with its
   wake time (no tool execution, no sleeping task).
2. Checkpoints — the terminal row and its result message are in the session,
   so recovery replays the recorded result and never re-sleeps.
3. Persists `wake_at` on the execution row (schema v4) **before** the
   lifecycle transition, then parks: `running → sleeping`, runner unloaded,
   `agent.sleeping` event.

The crash-order argument is load-bearing: `wake_at` is durable before the
lifecycle transition, so a crash between the two leaves a `running` agent
with a wake time — recovery resumes it (a benign early wake), never a
sleeping agent with no wake time.

### 26.3 The wake path

- **Scheduler**: the timer loop’s `wake_due_once` scans `list_sleeping_due`
  (sleeping agents, past `wake_at`) and respawns each runner through the same
  `prepare_resume` + `spawn_resumed` machinery as crash recovery and manual
  resume — one resume path, three callers. `soonest_wake_at` feeds the timer
  cadence so the daemon sleeps exactly until the next wake instead of
  polling. Missed wakes (daemon was down) collapse: `reconcile_sleeping`
  fires them once at startup.
- **Clear-before-spawn**: `prepare_resume` clears `wake_at` before restore;
  if the spawn then fails, `sleeping → failed` (a new transition) keeps the
  agent observable.
- **Budget interaction**: the wall-clock deadline keeps counting while
  parked — a durable sleep is wall time, and the budget survives recovery.

### 26.4 Why this is safe

- The sleep is **recorded terminal, never executed** — replay idempotent by
  construction (the dedup machinery replays the transcript row).
- No in-process timer means no zombie task; a sleeping agent is a pure
  database row plus a lifecycle state.
- The supervisor deliberately excludes `sleeping` — a sleeping agent has no
  runner by design, and the scheduler owns its wake.

Proven by engine tests (park + wake replay), scheduler tests (due/future
wake filtering), a store test (due-scan + `soonest_wake_at`), and a
v3→v4 migration test that seeds a pre-v4 execution and verifies the upgrade
preserves data.

| D28 | Execution budget — `runtime.max_duration` (wall-clock deadline anchored at execution start, re-anchored on restore, enforced before every model turn and while parked on an approval) and `runtime.max_tool_calls` (counter serialized into every checkpoint via `SessionState`, backward-compatible with serde default) — both survive recovery; runner panic containment (inner task + abort guard: a panicking runner fails the execution with `RUNNER_PANIC` instead of leaving a dead `running` agent, and aborts propagate); task-input durability via schema v3 (`executions.input`, re-seeded on no-checkpoint resume); scheduler crash-loop backoff (`schedule.backoff_after_failures`, default 3, exponential 30s..30min, `scheduler.backoff` event). Criterion benches added (event_append ~54µs, checkpoint_create ~71µs, loop_overhead ~4.2ms — all ≪ targets) |
| D29 | Supervision: runner-liveness supervision — `Engine::supervisor_sweep` (15s cadence, 60s grace, daemon-wired) fails `starting\|running\|waiting` agents whose runner is gone with `RUNNER_LOST` instead of leaving them stuck forever; lifecycle amendment `waiting → failed` (SPEC §3.2); store files (db/wal/shm/lock) restricted to 0600 on Unix |
| D30 | Durable wake/sleep — schema v4 (`executions.wake_at`), `sleeping` lifecycle state with `running → sleeping` (park) and `sleeping → running/failed/terminated` transitions, `agent.sleeping` catalog event, `runtime.durable_sleep_min` config (default 10s), runner-unloaded sleeps whose terminal result is replayed on wake; the scheduler’s `wake_due_once`/`reconcile_sleeping` and the timer cadence consume `soonest_wake_at`; the wake path shares `prepare_resume`/`spawn_resumed` with recovery and manual resume; the supervisor excludes `sleeping` |
| D31 | Fault injection — `crate::fault` + `Store::open_with_faults` (test-only; production cost is one `Option` check per store write) make every recovery-relevant persisted write an injectable point, proven by the matrix test (`tests/fault_injection.rs`): no hang, valid lifecycle state, structured observable failure, monotonic events, exactly-once per recorded tool row, at-least-once only across the documented terminal-row-write window. The harness surfaced two recovery defects: a pre-start storage failure left the execution `pending` and locked the agent out (`EXECUTION_ALREADY_ACTIVE`) — `Lifecycle::fail`/`spawn_and_wait` now fail the pending row with events; and a fully-expired ask batch killed the run with an invalid `waiting → completed` transition — the park resolution unparks a still-`waiting` agent (CAS-guarded, `agent.resumed`), so expired ≡ denied per SPEC §8.1 and the run completes. `runtime.tool_memory_limit_mb` → `RLIMIT_AS` on every Linux sandbox tier (bwrap/landlock/rlimit) so the memory boundary does not depend on the installed backend |

---

## 27. Deterministic fault injection and tool memory cap

### 27.1 Why a white-box fault harness

The SIGKILL recovery proof (`crates/kern-cli/tests/recovery.rs`) is a black box:
`kill -9` lands wherever the scheduler happens to be, so it proves *that* recovery
works but cannot visit every persisted-write boundary. The deterministic harness
(`crate::fault`, `Store::open_with_faults`) is the white-box complement: each
recovery-relevant store operation is an injectable point that can be scripted to
fail at a chosen occurrence count, and the matrix test
(`crates/kern-core/tests/fault_injection.rs`) runs the same scripted agent against
every injection, asserting the durable-runtime invariants — the run never hangs;
the agent ends in a **valid** lifecycle state, never a split-brain row; failure is
structured and observable (`agent.last_error`, `execution.failed`); per-agent event
sequences stay strictly increasing; a tool call whose terminal row is durable is
**never re-executed** on recovery (exactly-once per recorded row); a row whose
terminal write failed is re-executed once — the documented at-least-once crash
window, asserted as such, not hidden.

The injector is deliberately not environment-driven (a process-global env var
would leak across parallel tests) and not reachable through the API or CLI: it is
constructed explicitly via the `#[doc(hidden)]` `Store::open_with_faults`, so the
production cost is one `Option` check per instrumented write.

### 27.2 The pre-start failure orphan

A storage failure in `lifecycle.start` (or a runner task that failed to spawn)
left the execution row `pending`. The partial `ux_executions_one_active` index
treats a `pending` row as an active execution, so every future run of the agent
failed with `EXECUTION_ALREADY_ACTIVE` — a permanent lockout, invisible to the
SIGKILL test (a real kill never produces this exact interleaving).

Fixed at both failure seams: `Lifecycle::fail` falls back to
`Store::fail_pending_execution` (pending → failed, with `execution.failed` +
`agent.failed`) when the agent row cannot transition (it never started), and
`spawn_and_wait` does the same when the spawn itself errors. A pre-start failure
leaves the agent `created` and immediately runnable again — the regression in
the matrix asserts the next run completes once the fault clears.

### 27.3 A fully-expired ask killed the run

When every request in an `ask` batch expired without an operator decision, the
runner continued from `waiting` and the next `complete` was an invalid
`waiting → completed` transition: the run died and the agent stayed `waiting` with
a dead runner — again unreachable by the SIGKILL test. The park resolution now
unparks a still-`waiting` agent (`Lifecycle::unpark`, CAS-guarded on `waiting`,
`agent.resumed`); a decision that landed between the poll and the unpark already
applied the transition and the CAS rejects the redundant unpark. SPEC §8.1's
"expired ≡ denied" is now actually implemented end-to-end: the tool row is
recorded terminal-failed with the expiry reason and the run completes.

### 27.4 Tool memory cap

`runtime.tool_memory_limit_mb` (per-agent, absent = unlimited, an explicit operator
choice) flows to `RunLimits.memory_limit_bytes` and is enforced as `RLIMIT_AS` by
every Linux sandbox tier — bwrap (`--rlimit AS`), the Landlock backend, and the
rlimit fallback — so the resource boundary does not depend on which backend is
installed. Proven by a real-process test: `ulimit -v` inside the sandbox equals the
configured cap. Network isolation still requires bwrap; per-execution cgroup
accounting remains post-v0.1.

### 27.5 CLI correctness

Two user-visible CLI defects are covered by regression tests:

1. **Doubled scheme in unreachable-daemon messages.** `ClientError::Unreachable`
   carries the full base URL (scheme included), but two display sites prepended
   `http://`, rendering `no daemon at http://http://127.0.0.1:8787`. Both fixed;
   a unit test pins the Display contract.
2. **`--home` did not drive API-token resolution.** Client commands resolved the
   token from `default_home()` (`$KERN_HOME`/`~/.kern`) and ignored `--home`, so a
   custom data dir produced 401s on every API call. `dispatch` now mirrors `--home`
   into `KERN_HOME`, and the full cycle (run → durable sleep → SIGKILL → restart →
   wake → complete, verified via the API transcript: `checkpoint.restored` in the
   middle of the ordered record) passes end-to-end with `kern --home`.
