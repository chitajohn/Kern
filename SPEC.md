# Kern Specification — v0.1.0

**Status:** Draft for v0.1.0 (pre-release)
**Normative:** yes. This document defines the contracts the implementation must satisfy.
Non-normative explanation and rationale live in `ARCHITECTURE.md`.

Conformance keywords: **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, **MAY** (RFC 2119).

---

## 1. Scope

Kern v0.1.0 is a local-first runtime that executes AI agents as durable software processes. It
provides: agent lifecycle management, an execution engine, a model abstraction, a tool system,
persistent state, checkpointing, crash recovery, a permission engine, sandboxed execution, a
structured event stream, a local HTTP API, and a CLI.

Out of scope for v0.1.0 (see `ARCHITECTURE.md §19`): distributed execution, multi-tenancy,
scheduled starts, web UI, background-service daemon installation, exactly-once tool execution.

---

## 2. Terminology

| Term | Definition |
|------|------------|
| **Agent** | A named, validated configuration (model, tools, memory, permissions, runtime knobs) plus its durable lifecycle state. |
| **Execution** | One run of an agent from start to a terminal state. At most one execution is active per agent at a time. |
| **Session** | In-memory execution context (history, step, variables, pending tool call), reconstructable from checkpoints. |
| **Checkpoint** | Durable, versioned snapshot of a session plus metadata. |
| **Event** | Immutable, sequentially numbered, persisted record of a runtime action. |
| **Tool call** | A durable record of a requested tool invocation, its status, and its result/error. |
| **Permission request** | A pending grant/deny decision created when policy evaluates to `ask`. |
| **Durable sleep** | A `sleep` tool call at or above `runtime.durable_sleep_min` that parks the agent: the runner unloads, the wake time is persisted on the execution, and the scheduler wakes the agent later. The sleep survives daemon and machine restarts. |
| **Policy** | The agent's permission configuration evaluated by the permission engine. |
| **Runtime** | The daemon process exposing the API and executing agents. |
| **Data dir** | `$KERN_HOME` (default `~/.kern`); holds `state.db`, `logs/`, `token`. |

---

## 3. Agent lifecycle (normative)

### 3.1 States

```
created  starting  running  paused  waiting  sleeping  recovering  completed  failed  terminated
```

### 3.2 Valid transitions

| From | Trigger | To | Required side effects |
|------|---------|-----|-----------------------|
| created | `start` | starting | spawn runner task |
| completed \| failed \| terminated | `start` (new run) | starting | new execution, spawn runner task, reset `last_error` |
| starting | runner ready | running | emit `execution.started`, `agent.started` |
| starting | init error | failed | attach structured error |
| running | `pause` | paused | checkpoint, suspend runner |
| running | policy = `ask` | waiting | create permission request, emit `permission.asked` + `agent.waiting`, checkpoint |
| running | loop finished | completed | final checkpoint, emit `execution.completed`, `agent.completed` |
| running | unrecoverable error | failed | emit `execution.failed`, `agent.failed` |
| starting \| running | `terminate` | terminated | abort runner, emit `agent.terminated` |
| waiting | grant + resume | running | emit `permission.granted`, `agent.resumed` |
| waiting | deny + resume | running | feed denial to model, emit `permission.denied`, `agent.resumed` |
| waiting | `pause` | paused | |
| waiting | `terminate` | terminated | |
| waiting | unrecoverable error (runner lost) | failed | attach structured error (§25 supervisor) |
| paused | `resume` | running | restore checkpoint, emit `agent.resumed` |
| running | durable sleep (sleep ≥ `durable_sleep_min`) | sleeping | persist `wake_at` on the execution, record the sleep result terminal, unload the runner, emit `agent.sleeping` |
| sleeping | wake_at due (scheduler) | running | clear `wake_at`, restore checkpoint, respawn runner, emit `agent.resumed` (missed wakes collapse) |
| sleeping | manual `resume` | running | clear `wake_at`, restore checkpoint, emit `agent.resumed` |
| sleeping | `terminate` | terminated | |
| sleeping | wake failed (unrecoverable) | failed | attach structured error |
| paused | `terminate` | terminated | |
| recovering | recovery success | running | emit `checkpoint.restored`, `agent.resumed` |
| recovering | recovery failure | failed | attach error |
| recovering | `terminate` | terminated | |
| starting \| running \| waiting | daemon restart (interrupted) | recovering | startup reconciliation (§11.3); recovery resumes the run |

**Rules**

- Any transition not in this table is a programming error: the `Lifecycle` component MUST
  reject it with `StateError::InvalidTransition`.
- Terminal states (`completed`, `failed`, `terminated`) MUST NOT have outgoing transitions
  except `start` (a new run): scheduled agents and repeated `kern run` invocations must be able
  to start again, and the `executions` history is retained per run.
- A transition, its persisted state update, and its event append MUST commit atomically in one
  transaction.
- `pause` from `running`/`waiting` MUST checkpoint first.
- `resume` from `paused` MUST restore the latest checkpoint before resuming the loop.
- `sleeping → running` MUST clear the persisted `wake_at` BEFORE respawning the runner (a
  crash between clearing and spawn leaves a `running` agent that recovery resumes — a benign
  early wake, never a sleeping agent with no wake time).
- `waiting → failed` exists for the runner-liveness supervisor (§25): an agent
  waiting on a permission decision whose runner is gone must fail, never hang forever.

---

## 4. Domain model

### 4.1 Agent

```json
{
  "id": "uuid",
  "name": "researcher",
  "spec_version": 1,
  "config_json": { "version": 1, "name": "researcher", "model": { ... }, "tools": [...], "permissions": {...}, "runtime": {...} },
  "lifecycle_state": "created",
  "created_at": "2026-08-14T12:00:00Z",
  "updated_at": "2026-08-14T12:00:01Z",
  "last_error": null,
  "auto_recover": true
}
```

- `name` MUST be a unique slug (`[a-z0-9][a-z0-9-_]*`).
- `config_json` is the fully validated agent spec (see §9); the runtime never re-parses an
  unvalidated spec.

### 4.2 Execution

```json
{
  "id": "uuid",
  "agent_id": "uuid",
  "status": "running",
  "started_at": "...",
  "finished_at": null,
  "latest_checkpoint_id": "uuid",
  "wake_at": "..."
}
```

- `status` ∈ `pending | running | completed | failed | interrupted`.
- `wake_at` (schema v4) is set only while the agent is parked on a durable sleep; the
  scheduler's due-scan and `soonest_wake_at` consult it. Cleared on wake.
- At most one execution with status `pending|running` MAY exist per agent (partial unique
  index, see §5).

### 4.3 Tool call

```json
{
  "id": "tool_call_id",
  "agent_id": "uuid",
  "execution_id": "uuid",
  "tool_name": "filesystem",
  "args_json": "{...}",
  "status": "completed",
  "result_json": "{...}",
  "error_json": null,
  "started_at": "...",
  "finished_at": "..."
}
```

- `id` is the model-supplied tool-call id where the provider supplies one; otherwise the
  runtime generates one. It is the **dedup key** for tool execution (§11.2).
- `status` ∈ `requested | running | completed | failed`.

### 4.4 Permission request

```json
{
  "id": "uuid",
  "agent_id": "uuid",
  "tool_call_id": "...",
  "resource": "filesystem:write",
  "action": "write ./workspace/out.txt",
  "status": "pending",
  "requested_at": "...",
  "decided_at": null
}
```

- `status` ∈ `pending | granted | denied | expired`.
- Only one pending request per agent MAY exist at a time.

---

## 5. Storage schema (normative DDL)

Database: `$KERN_HOME/state.db` (SQLite, WAL mode, `foreign_keys=ON`).
Current schema version: **4** (migrations are transactional; `PRAGMA user_version` and
`kern_meta.schema_version` are updated atomically).

```sql
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE kern_meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);  -- keys: schema_version, runtime_version, instance_id

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
  input                TEXT,          -- durable task input (v3); re-seeded on no-checkpoint resume
  wake_at              TEXT,          -- durable wake time (v4); set while the agent sleeps
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
```

**Rules**

- Timestamps are RFC 3339 UTC text.
- `events.seq` is monotonic, never reused, and the replay cursor.
- Schema migrations are forward-only. `PRAGMA user_version` plus `kern_meta.schema_version`
  MUST agree after any open. Migration to a *lower* version is rejected.
- A corrupted database MUST be detected (integrity check on open) and surfaced as a structured
  `StorageError::Corruption` — the runtime MUST NOT silently re-create or overwrite it. A
  `.corrupt-<ts>` quarantine copy MAY be made before any repair, and only with explicit user
  action.
- The data dir is single-owner: the daemon holds an exclusive lock file (`daemon.lock`); a
  second daemon MUST refuse to start with `STORAGE_LOCKED`.
- Checkpoint retention: on checkpoint create, older checkpoints beyond
  `runtime.checkpoint_retention` (default 50) per agent are pruned in the same transaction
  (the latest is never pruned).

---

## 6. Event catalog (normative)

Envelope: `{ "seq": 12, "ts": "…", "kind": "tool.completed", "agent_id": "…", "execution_id": "…", "payload": {…} }`

| kind | payload (normative keys, extensible) |
|------|---------------------------------------|
| `runtime.started` | `{ "instance_id", "schema_version", "runtime_version" }` |
| `runtime.shutting_down` | `{}` |
| `agent.created` | `{ "agent_id", "name" }` |
| `agent.started` | `{ "agent_id", "execution_id" }` |
| `agent.paused` | `{ "agent_id", "checkpoint_id" }` |
| `agent.sleeping` | `{ "agent_id", "wake_at" }` |
| `agent.resumed` | `{ "agent_id", "checkpoint_id" | null }` |
| `agent.thinking` | `{ "agent_id", "step", "text" }` |
| `agent.waiting` | `{ "agent_id", "permission_request_id", "resource", "action" }` |
| `agent.completed` | `{ "agent_id", "execution_id", "final_text" }` |
| `agent.failed` | `{ "agent_id", "execution_id", "error": {…} }` |
| `agent.terminated` | `{ "agent_id" }` |
| `execution.started` | `{ "execution_id", "agent_id" }` |
| `execution.completed` | `{ "execution_id", "steps", "checkpoints" }` |
| `execution.failed` | `{ "execution_id", "error": {…} }` |
| `execution.restored` | `{ "execution_id", "checkpoint_id" }` |
| `model.requested` | `{ "provider", "model", "step" }` |
| `model.completed` | `{ "provider", "model", "kind": "finish" | "tool_call", "latency_ms" }` |
| `model.failed` | `{ "error": {…} }` |
| `tool.requested` | `{ "tool_call_id", "tool_name", "args": {…} }` (args only when `log_tool_args` enabled) |
| `tool.started` | `{ "tool_call_id", "tool_name" }` |
| `tool.completed` | `{ "tool_call_id", "tool_name", "latency_ms", "result_size" }` |
| `tool.failed` | `{ "tool_call_id", "tool_name", "error": {…} }` |
| `checkpoint.created` | `{ "checkpoint_id", "execution_id", "seq" }` |
| `checkpoint.restored` | `{ "checkpoint_id", "execution_id" }` |
| `checkpoint.failed` | `{ "error": {…} }` |
| `permission.asked` | `{ "permission_request_id", "resource", "action" }` |
| `permission.granted` | `{ "permission_request_id", "resource" }` |
| `permission.denied` | `{ "permission_request_id", "resource", "reason" }` |
| `scheduler.recovered_agent` | `{ "agent_id", "execution_id", "checkpoint_id" }` |
| `scheduler.run_due` | `{ "agent_id", "scheduled_for" }` |
| `scheduler.backoff` | `{ "agent_id", "consecutive_failures", "backoff" }` — emitted when a crash-looping agent's next run is deferred |

Errors are structured per §13 and appear in payloads as `{ "code", "message", "detail" }`.

---

## 7. Checkpoint format (normative, format v1)

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
  "messages": [ { "role": "user", "content": "…" } ],
  "pending_tool_calls": [],
  "tool_calls": [ { "id": "…", "tool_name": "…", "args": {…}, "status": "completed", "result": {…}, "error": null } ],
  "variables": { "key": "value" },
  "memory_refs": [],
  "runtime_meta": { "provider": "openai", "model": "gpt-4o-mini" }
}
```

**Rules**

- `format_version` MUST be present. A reader MUST accept versions `<= 1` and MUST reject `> 1`
  with `CheckpointError::UnsupportedVersion` (never silently upgrade).
- `tool_calls` is the dedup source for tool execution (§11.2). It MUST be complete for the
  execution (all requested calls with their terminal status).
- `messages` MAY be truncated by the bounded-history policy (§8.4); truncation MUST be marked
  with `"history_trimmed": true` and the dropped count.
- `pending_tool_calls` carries a batch (0..N) of requested-but-undecided calls (parallel
  batch semantics, §8.1).
- Checkpoint creation and its `checkpoint.created` event MUST commit in the same transaction.

---

## 8. Execution engine

### 8.1 Loop

Normative behavior (order matters):

1. Load session (fresh or restored).
2. Build completion request: system prompt (+ optional memory digest) + bounded history + tool
   specs (only tools the agent is configured with) + durable variables.
3. Emit `model.requested`. Call gateway with timeout/retries. **Model calls are at-least-once**
   (no side effects besides cost); a lost response is re-requested after restore.
4. On `Finish`: append final message, emit `model.completed`, create final checkpoint, emit
   `execution.completed` + `agent.completed`, transition to `completed`.
5. On `Thinking(text)`: emit `agent.thinking`, continue (no state change, no checkpoint).
6. On `ToolCalls(batch)` (1..N):
   a. Validate every call's args against its schema; record all as `requested` in one
      transaction; checkpoint **before the batch**.
   b. Classify each call via policy (§10): `deny` → `permission.denied` + denial result;
      `ask` → permission request, `permission.asked`, transition to `waiting`, checkpoint,
      suspend until decisions; granted → proceed, denied → denial result.
   c. Execute `allow` calls concurrently (bounded by `max_concurrent_tools` and the global
      tool semaphore), each with timeout + sandbox; record each result/error in one
      transaction with `tool.completed`/`tool.failed` events.
   d. Feed every result and denial back to the model in a single follow-up turn; checkpoint
      **after the batch**.
   e. **Ordering caveat:** batch members run concurrently; models MUST NOT assume ordering or
      exclusivity between batch members.
7. Enforce `max_steps` (agent fails with `STEP_LIMIT_EXCEEDED`), `checkpoint_interval`, and
   bounded history.
8. Enforce the execution budget: `runtime.max_duration` (agent fails with
   `RUN_DURATION_EXCEEDED`; the wall-clock deadline is anchored at execution start, re-anchored
   to the remaining time on restore, and checked before every model turn **and while parked on
   an approval**) and `runtime.max_tool_calls` (agent fails with `TOOL_CALL_LIMIT_EXCEEDED`;
   the issued-call counter is serialized into every checkpoint so a recovered run keeps its
   budget). A durable sleep counts against the wall-clock budget: the clock keeps running while
   the agent is parked.
9. Durable wake/sleep: a `sleep` call at or above `runtime.durable_sleep_min` is NOT executed
   in-process. It is recorded terminal with its wake time, checkpointed, and the agent parks
   (`sleeping`, runner unloaded, `wake_at` persisted). The scheduler wakes the agent when the
   wake time passes; a daemon that was down past the wake time wakes it on startup (missed
   wakes collapse). On wake the checkpoint is restored and the recorded result is replayed —
   the sleep is never re-executed.

### 8.2 Timeouts and retries

- Model call: `model.timeout` (default 60s). Retried up to `model_retries` (default 2) with
  exponential backoff for transient `ModelError` kinds only (`rate_limited`, `transport`,
  `unavailable`). Permanent errors (`auth`, `invalid_response`, `timeout` after budget) fail
  the execution.
- Tool call: `tool_timeout` (default 30s). A timeout produces `ToolError::Timeout` and is fed
  to the model; the agent MAY continue per policy.

### 8.3 Failure visibility

- Every failure path emits a structured event. No failure is swallowed.
- `max_steps`, `max_duration`, `max_tool_calls`, model budget exhaustion, and unrecoverable
  tool failure transition the agent to `failed` with the error attached (`agent.last_error`).
- A panic inside the runner task (a Kern bug or provider-adapter bug) is contained at the task
  boundary: the execution fails with `RUNNER_PANIC` (never a hung `running` agent), and aborting
  the runner propagates to the inner task so no orphaned runner survives.

### 8.4 Bounded history

- `runtime.max_history_tokens` (default 16k) bounds `messages` using a character-based token
  approximation (~4 chars ≈ 1 token). On exceed: drop oldest messages to fit and set
  `history_trimmed` in the next checkpoint payload (no event kind in v0.1 catalog).
  Per-provider tokenizers are a v0.2 concern.

---

## 9. Agent configuration (normative, schema v1)

`agent.yaml`. Parsed with `deny_unknown_fields: true`. Unknown keys, bad types, unknown
provider/tool names, and invalid permissions MUST fail with a line-referencing error at create
time; the agent is not created.

```yaml
version: 1                     # required, integer

name: researcher               # required, unique slug [a-z0-9][a-z0-9-_]*
description: "Research assistant"  # optional, free text

model:
  provider: openai             # required: openai | anthropic | ollama | mock
  model: gpt-4o-mini           # required: provider-specific model id
  temperature: 0.2             # optional, float 0..2
  max_tokens: 2048             # optional, positive int
  timeout: 60s                 # optional, duration
  base_url: null               # optional: override provider base URL

tools:                         # required, non-empty
  - filesystem                 # each: name of a builtin or custom registered tool
  - http
  # shell is NOT available here unless explicitly enabled in permissions

memory:
  enabled: false               # optional; exposes memory.read/write/list tools
  inject_digest: false         # optional; prepend memory digest to system prompt each step
  max_keys: 100                # optional; per-agent key cap
  max_value_bytes: 65536       # optional; per-key value cap

permissions:                  # optional; absence of a class = no access (default deny)
  filesystem:                  # optional; absence = no filesystem access
    read:                      # each rule list: allow | ask | deny (deny > ask > allow)
      allow: [./workspace]     # paths relative to data dir workspace; globs allowed
      ask:    [./shared/**]
      deny:   [./workspace/secret/**]
    write:
      allow: [./workspace]
  network:                     # optional; absence = no network access
    allow: [api.github.com]    # hosts, optionally host:port (e.g. api.github.com:443);
                               # a port-less rule matches ANY port on the host; a
                               # port rule matches only that exact port (default port
                               # is filled from the URL scheme). IPv6 literals use
                               # bracket form: "[2001:db8::1]:8080"
    # ask:  [...]              # optional
    # deny: [...]              # optional
  memory:                      # optional; absence = memory tools unavailable
    read:
      allow: ["*"]            # glob-matched keys
    write:
      allow: ["*"]
  shell:                       # optional; absent/disabled = shell tool unavailable
    enabled: false             # required to enable the shell tool
    sandbox: required          # required | best-effort | off  (see §12)

schedule:                      # optional; absent = manual start only
  every: 12h                   # interval, OR
  cron: "0 3 * * *"            # cron expression, OR
  at: "2026-09-01T00:00:00Z"   # one-shot RFC3339
  timezone: "UTC"              # optional, default UTC
  skip_if_running: true        # optional, default true
  backoff_after_failures: 3    # optional; consecutive failures before the scheduler
                               # backs off the next run (exponential, 30s..30min)

runtime:
  checkpoint_interval: 30s     # optional, default 30s
  max_steps: 100               # optional, default 100
  max_history_tokens: 16384    # optional, default 16384
  max_concurrent_tools: 4      # optional, per-agent tool parallelism
  checkpoint_retention: 50     # optional, checkpoints kept per agent
  auto_recover: true           # optional, default true
  model_retries: 2             # optional, default 2
  tool_timeout: 30s            # optional, default 30s
  ask_timeout: 300s            # optional, approval window for ask requests (S10.1)
  log_tool_args: false         # optional, default false (redaction, see §14.3)
  max_duration: 1h             # optional; execution wall-clock budget, survives recovery
  max_tool_calls: 500          # optional; tool-call budget, survives recovery
  tool_memory_limit_mb: 1024   # optional; address-space cap for every tool process,
                               # enforced as RLIMIT_AS by the active sandbox backend
  durable_sleep_min: 10s       # optional, default 10s; `sleep` calls at or above this
                               # park the agent (durable, runner unloaded); shorter sleeps
                               # run inside the runner
```

**Rules**

- `schedule.cron` accepts the standard 5-field form (`minute hour day-of-month month
  day-of-week`, e.g. `"0 3 * * *"`). 6/7-field seconds-first forms (with optional year) and
  `@daily`-style shorthands are also accepted; a 5-field expression is normalized to
  seconds=0 before validation (the underlying parser requires a seconds field).
- The `shell` tool MUST NOT be exposed to the model unless `permissions.shell.enabled: true`.
- `filesystem` roots are relative to the agent workspace root (`$KERN_HOME/workspace/<name>`);
  absolute roots are allowed but MUST be canonicalized; rules outside any root are denied.
- If `permissions.shell.enabled: true` and `permissions.shell.sandbox: required` but no sandbox
  backend is available on the host, the agent MUST fail to start with
  `SandboxError::Unavailable` (fail closed).

---

## 10. Permission engine (normative)

Evaluation for a request `(resource_class, resource, action)`:

1. Select rules for `resource_class` (`filesystem`, `network`, `memory`, `shell`).
2. Match most-specific rule (longest canonical path prefix / exact host).
3. Precedence within a match: `deny` > `ask` > `allow`.
4. No match ⇒ `deny` (default deny).
5. `filesystem`: canonicalize path, resolve symlinks, verify containment within an allowed
   root; escape ⇒ deny. Write rules also cover create/delete/rename.
6. `network`: host normalized (lowercase, trailing dot stripped, IDN→punycode); allowlist
   match; IP-literal normalization (IPv6 brackets). Port semantics: a rule without a port
   matches any port on that host; a rule with a port (`host:port`) matches only that exact
   port — the runtime evaluates the URL's host with its port (default port filled from the
   scheme), so port-scoped rules are enforceable. No match ⇒ deny.
7. `memory`: glob-match keys against read/write rules; no match ⇒ deny.
8. Result is one of `Allow { reason } | Deny { reason } | Ask`.

A `Deny` result is returned to the model as a tool error with code `PERMISSION_DENIED` and the
reason text. The runtime MUST NOT execute the tool.

### 10.1 Ask requests expire (approval TTL)

An `Ask` request is created with `expires_at = requested_at + runtime.ask_timeout` (default
`300s`, per agent). The engine's poll CASes overdue pending requests to `expired` — a waiting
agent can never park forever on a stale prompt. Semantics (normative):

- A decision inside the window is recorded once (CAS on `pending`, §10.2).
- A decision on an expired request is rejected with `PERMISSION_REQUEST_EXPIRED` (HTTP 409)
  and the request is sealed `expired` — a late `grant` can never resurrect it.
- The engine treats `expired` exactly like `denied`: the tool call fails with
  `PERMISSION_DENIED` ("permission request expired") and the agent continues.
- `GET /permissions/pending` carries `expires_at`, so clients can show the deadline.
- Pre-v2 rows (no `expires_at`) have no deadline and stay decidable (migration v2 is
  backward-safe).

---

## 11. Tool system (normative)

### 11.1 Tool contract

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> serde_json::Value;   // JSON Schema draft 2020-12
    fn permission(&self) -> PermissionRequirement;
    async fn execute(&self, args: Value, ctx: ToolContext) -> Result<Value, ToolError>;
}
```

- `args` are validated against `input_schema` before execution; failure ⇒
  `ToolError::InvalidArguments` (fed to model, never crashes the runtime).
- `ToolContext` MUST carry the `tool_call_id`, sandbox, workdir, and timeout; tools SHOULD use
  the id to implement their own idempotency on top of the engine's dedup.
- Tool output and error output MUST be size-capped (default 1 MiB) and time-capped
  (`tool_timeout`).
- **Idempotency contract:** tools with side effects MUST tolerate duplicate invocation; a crash
  between spawn and result-record MAY re-execute a tool (§11.2). This contract is documented in
  tool authoring docs and surfaced in `kern tools`.

### 11.2 Tool execution and dedup (effectively-once)

1. Model returns a batch of `ToolCall{ id, name, args }`. Runtime records rows
   `tool_calls(execution_id, id)` status `requested` (one transaction).
2. Checkpoint before the batch.
3. Classify via policy (§10); execute `allow` calls concurrently (bounded); record each
   terminal status `completed`/`failed` with result/error (one transaction).
4. Checkpoint after the batch.
5. On restore, for each `requested` call in the latest checkpoint:
   a. Look up `tool_calls(execution_id, id)`.
   b. Terminal row exists → replay the recorded result; construct the session's tool message
      from the row. **Never re-execute.**
   c. Row missing or `running` → re-execute (documented at-least-once window; the tool may
      have partially run — idempotency contract §11.1).
6. No recorded result is ever executed twice.

### 11.3 Builtins (v0.1)

| Tool | Input schema (summary) | Permission class | Notes |
|------|------------------------|------------------|-------|
| `filesystem` | `{ action: read\|write\|list\|stat, path, content? }` | `filesystem:read\|write` | canonicalized; symlink-escape prevented |
| `http` | `{ method: get\|post, url, headers?, body?, timeout? }` | `network:<host>` | host allowlist enforced; TLS verified; response cap |
| `memory.read` / `memory.write` / `memory.list` | `{ key }` / `{ key, value, description? }` / `{ prefix? }` | `memory:read\|write` | durable agent-scoped KV; glob policy; size caps |
| `shell` | `{ command: string, cwd? }` | `shell` | only when enabled; sandboxed; output cap |
| `noop` / `sleep` | `{}` / `{ ms: int }` | none | test & recovery-suite fixtures |

**Behavioral details (normative):**

- `filesystem`: a relative `path` resolves against the **first** configured root (the agent's
  primary workspace), never the daemon's cwd. Targets are canonicalized (symlinks followed,
  `.`/`..` resolved) and must remain inside a root; a symlink inside a root pointing outside
  is denied. Text files only in v0.1.
- `http`: redirects are NOT followed (a `3xx` is returned to the model), so an allowlisted
  host cannot bounce the request to an unvetted one. TLS is always verified. Bodies larger
  than the response cap fail with `TOOL_FAILED` (never silently truncated). An empty host
  allowlist denies all requests.
- `sleep`: capped at 60 s (fixture for timeout/concurrency tests).

---

## 12. Sandbox (normative, per platform)

| Platform | Backend | Enforced when `sandbox: required` | Notes |
|----------|---------|-----------------------------------|-------|
| Linux | `bubblewrap` (`bwrap`) | namespaces (net, pid, mount, ipc, uts), read-only root, writable workspace only, rlimits, dropped capabilities, `--die-with-parent` | Requires `bwrap` on PATH; no seccomp filter in v0.1 (documented limitation) |
| Linux (no `bwrap`) | `landlock` (kernel LSM, Linux ≥ 5.13, no external binary) | kernel-enforced write containment: read/execute anywhere, writes only in the agent workspace, `/tmp`, and `/dev/null`; plus the rlimits below | Landlock has no network domain — namespaces (bwrap) remain the only network isolation; mask probed empirically (some kernels report an ABI whose bits they reject); `required` ⇒ agent fails to start if neither bwrap nor landlock is available |
| Linux (no `bwrap`, no `landlock`) | rlimits fallback | — | `required` ⇒ agent fails to start (`SandboxError::Unavailable`); `best-effort` ⇒ CPU/FSIZE/NOFILE rlimits only + limitation logged (no network isolation) |

Tool processes on every Linux tier (and the rlimit fallback) additionally carry `RLIMIT_AS` when
`runtime.tool_memory_limit_mb` is configured — an explicit per-agent address-space cap. Without
that knob there is no memory cap (the default); per-execution cgroup accounting remains post-v0.1.
| macOS | `sandbox-exec` (seatbelt) | seatbelt profile: no network, read-only root except workspace | `sandbox-exec` is deprecated by Apple and untested on CI; documented as such |
| Windows | none in v0.1 | — | `required` ⇒ agent fails to start; `best-effort` ⇒ no OS isolation + limitation logged; job objects + restricted tokens deferred |

- `filesystem` and `http` tools enforce their own path/host constraints on ALL platforms
  (defense in depth), independent of the OS backend.
- The exact capabilities of each backend MUST be documented in the README; we do not claim
  stronger boundaries than enforced.

---

## 13. Errors (normative codes)

| Code | Kind | Meaning |
|------|------|---------|
| `CONFIG_INVALID` | Config | agent/runtime config invalid (detail carries field/line) |
| `AGENT_NOT_FOUND` | NotFound | unknown agent |
| `AGENT_NAME_TAKEN` | State | duplicate agent name |
| `INVALID_TRANSITION` | State | lifecycle transition not allowed |
| `EXECUTION_ALREADY_ACTIVE` | State | an execution is already active for this agent |
| `EXECUTION_NOT_FOUND` | NotFound | unknown execution |
| `CHECKPOINT_NOT_FOUND` | NotFound | unknown checkpoint |
| `MODEL_TIMEOUT` | Model | model call exceeded timeout |
| `MODEL_UNAVAILABLE` | Model | provider unreachable |
| `MODEL_AUTH` | Model | provider authentication failed |
| `MODEL_RATE_LIMITED` | Model | provider rate limit |
| `MODEL_INVALID_RESPONSE` | Model | malformed provider response |
| `MODEL_BUDGET_EXHAUSTED` | Model | retries exhausted |
| `RUN_DURATION_EXCEEDED` | State | execution exceeded `runtime.max_duration` (budget survives recovery) |
| `TOOL_CALL_LIMIT_EXCEEDED` | State | execution exceeded `runtime.max_tool_calls` (budget survives recovery) |
| `RUNNER_PANIC` | Internal | runner task panicked; execution failed cleanly instead of hanging |
| `RUNNER_LOST` | Internal | supervisor sweep found an execution with no live runner (`starting\|running\|waiting` past the grace window); failed so it cannot stay stuck forever |
| `TOOL_INVALID_ARGUMENTS` | Tool | args failed schema validation |
| `TOOL_TIMEOUT` | Tool | tool exceeded `tool_timeout` |
| `TOOL_FAILED` | Tool | tool execution error |
| `TOOL_UNAVAILABLE` | Tool | tool not registered/disabled |
| `STEP_LIMIT_EXCEEDED` | State | agent exceeded `runtime.max_steps` (§8.1) |
| `PERMISSION_DENIED` | Permission | policy denied |
| `PERMISSION_REQUEST_NOT_FOUND` | NotFound | unknown permission request |
| `PERMISSION_REQUEST_ALREADY_DECIDED` | Conflict (409) | a permission request was decided more than once with a conflicting decision; the original decision stands (replaying the *same* decision is idempotent) |
| `SANDBOX_UNAVAILABLE` | Sandbox | required backend missing |
| `SANDBOX_FAILURE` | Sandbox | backend failed at runtime |
| `CHECKPOINT_FORMAT_UNSUPPORTED` | Serialization | checkpoint version too new |
| `CHECKPOINT_CORRUPT` | Serialization | payload failed validation |
| `STORAGE_CORRUPTION` | Storage | DB integrity failure |
| `STORAGE_MIGRATION` | Storage | migration failure/conflict |
| `STORAGE_LOCKED` | Storage | another daemon owns the data dir |
| `STORAGE_FAILURE` | Storage | generic storage failure (I/O, busy, …) |
| `INTERNAL` | Internal | unexpected bug (never leaks stack traces in API responses) |

Errors serialize as `{ "code": "…", "message": "…", "detail": {…} }`.

---

## 14. Security (normative requirements)

1. **Model output is untrusted.** Every tool execution crosses the permission engine (§10).
   The runtime, not the model, authorizes.
2. **Default deny.** Absence of a rule denies.
3. **Secrets env-only.** API keys MUST be read from environment variables only
   (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`); MUST NOT be persisted in `state.db`, checkpoints,
   events, or agent configs; MUST NOT appear in logs.
4. **Redaction.** Logging guard redacts env values, bearer tokens, and `Authorization` headers.
   Tool args are logged only under `log_tool_args: true`.
4b. **Credential boundary.** Tool subprocesses NEVER inherit the daemon's environment
   wholesale. Every `shell` spawn starts from a scrubbed allowlist (`PATH`, `HOME`, locale,
   `TERM`, `TZ`, user identity, `SHELL`, `EDITOR`, `PAGER`) applied backend-independently
   (bwrap's `--clearenv`, landlock/rlimits/none alike). Provider keys and `KERN_TOKEN` are
   never visible to a tool process — redaction protects logs; the scrub protects the agent.
5. **API surface.** Bound to loopback by default (`127.0.0.1`); optional bearer token from
   `$KERN_HOME/token` (generated by `kern init`). When the token exists, the API MUST reject
   unauthenticated requests with `401`.
6. **Fail closed.** `shell` + `sandbox: required` with no backend ⇒ agent does not start.
7. **Defense in depth.** Tools enforce their own constraints in addition to engine policy.
8. **Corruption.** Never silently overwrite durable state (§5 rules).
9. Structured errors must not leak internal paths/stack traces to API clients.

---

## 15. API contract (normative)

Base path: `/api/v1`. Content type: `application/json`. Errors: §13 shape.

### 15.1 Endpoints

| Method & path | Request | Success | Notes |
|---|---|---|---|
| `POST /agents` | `{ "name", "spec": {…yaml-json…} }` | `201` agent | validates spec; creates only |
| `GET /agents` | — | `200` `[agent]` | |
| `GET /agents/{id}` | — | `200` agent | includes `last_error`, summary counts |
| `POST /agents/{id}/start` | — | `202` `{ "execution_id" }` | |
| `POST /agents/{id}/pause` | — | `202` | |
| `POST /agents/{id}/resume` | — | `202` | |
| `POST /agents/{id}/terminate` | — | `202` | |
| `POST /agents/{id}/checkpoint` | — | `201` checkpoint | creates checkpoint now |
| `GET /agents/{id}/checkpoints` | — | `200` `[checkpoint-meta]` | |
| `POST /agents/{id}/checkpoints/{cid}/restore` | — | `202` | restores into a new execution? **No**: restores the session; agent resumes from it on next start |
| `GET /agents/{id}/events?after={seq}&limit={n}` | — | `200` `[event]` | replay |
| `GET /events/stream?after={seq}` | — | `200` `text/event-stream` | SSE: `event: <kind>` + `data: {envelope}`; `after` replays then live |
| `GET /executions/{id}` | — | `200` execution + tool-call summary | |
| `GET /executions/{id}/transcript` | — | `200` `[{seq, kind, role?, content?, tool?}]` | full ordered record of model turns, tool calls, results |
| `GET /agents/{id}/executions` | — | `200` `[execution]` | execution history |
| `GET /tools` | — | `200` `[{name, description, input_schema, permission}]` | |
| `GET /models` | — | `200` `[{provider, models: [...], configured: bool}]` | configured = key present |
| `GET /permissions/pending` | — | `200` `[permission_request]` | |
| `POST /permissions/{id}/grant` | — | `200` | resumes agent if waiting |
| `POST /permissions/{id}/deny` | — | `200` | resumes agent with denial |
| `GET /health` | — | `200` `{ "status": "ok", "version": "…" }` | |

### 15.2 SSE format

```
event: tool.completed
data: {"seq":12,"ts":"…","kind":"tool.completed","agent_id":"…","execution_id":"…","payload":{…}}

event: agent.completed
data: {…}
```

- One event per SSE event; `: keepalive` comment every 15s of idle.
- A `?after=` cursor replays from `seq+1` then switches to live. Resumable: a dropped
  connection reconnects with the last received `seq`.

### 15.3 State-change semantics

- Lifecycle endpoints are idempotent in effect: `pause` on `paused` returns `202` with the
  current state (no error); `resume` on `running` likewise. `start` on `running` is a no-op
  `202`. `terminate` on a terminal state returns the current state.
- Invalid transitions (e.g. `start` on `completed` with an active execution) return
  `409 INVALID_TRANSITION` (the caller must create a new execution by re-running).

---

## 16. CLI contract (normative)

`kern` talks to the daemon's API only (never the database).

| Command | Behavior |
|---|---|
| `kern init` | scaffold `agent.yaml`, create `$KERN_HOME`, generate token |
| `kern daemon` | run runtime foreground; print API address; graceful shutdown on SIGINT/SIGTERM (pause + checkpoint running agents) |
| `kern run agent.yaml` | create + start agent; print `agent_id`; `--wait` tails to completion, exits 0 on completed / 1 on failed |
| `kern doctor` | environment health: DB integrity, schema version, sandbox backend, provider keys present, API reachable; exit 0/1 |
| `kern schedule <name\|id>` | show schedule and next run time |
| `kern ps` | table of agents: name, id, state, updated_at |
| `kern logs <name\|id>` | tail events (follow with `-f`) |
| `kern inspect <name\|id>` | agent detail + latest checkpoint summary + last error |
| `kern pause\|resume\|checkpoint\|terminate <name\|id>` | lifecycle control |
| `kern tools` / `kern models` | capability discovery |
| `kern version` | version + API address |

Exit codes: `0` success, `1` runtime/client error, `2` usage error.

---

## 17. Runtime configuration (normative)

Env vars (runtime config may also be set in `$KERN_HOME/kern.toml`; env wins):

| Var | Default | Meaning |
|---|---|---|
| `KERN_HOME` | `~/.kern` | data dir |
| `KERN_API_ADDR` | `127.0.0.1:8787` | API bind address |
| `KERN_TOKEN` | (token file) | API bearer token |
| `KERN_LOG` | `info` | tracing level |
| `KERN_MAX_CONCURRENT_AGENTS` | `8` | concurrency cap |
| `KERN_MAX_CONCURRENT_TOOLS` | `16` | global tool process cap |
| `KERN_EVENT_RETENTION` | unset (unbounded) | keep the newest N events per agent; pruned at daemon start and periodically (opt-in) |
| `OPENAI_API_KEY` | — | openai provider |
| `ANTHROPIC_API_KEY` | — | anthropic provider |
| `OLLAMA_BASE_URL` | `http://127.0.0.1:11434` | ollama provider |
| `OPENAI_BASE_URL` / `ANTHROPIC_BASE_URL` | provider defaults | base URL override |

Data dir layout: `state.db`, `logs/runtime.jsonl`, `token`, `workspace/<agent-name>/`.

---

## 18. Acceptance criteria (v0.1 definition of done)

The following automated tests MUST pass:

1. **Crash recovery (the core proof).** Spawn real `kern daemon` → create agent (mock model
   with scripted multi-step work incl. a filesystem tool call) → let it run → **SIGKILL** the
   daemon mid-execution → restart daemon → assert agent recovered (`checkpoint.restored`),
   continued, and reached `completed`. No manual intervention.
2. **Lifecycle.** Every valid transition in §3.2 executes atomically and emits the correct
   events; every invalid transition is rejected.
3. **Permission enforcement.** A deny rule prevents tool execution (no side effect, event
   recorded); `ask` suspends to `waiting` and grant/deny via API resumes correctly; default
   deny blocks unlisted resources.
4. **Security.** Shell escape attempts (e.g. `../` paths, commands touching disallowed paths)
   fail; sandbox `required` without backend fails to start; API rejects requests without token
   when token configured; logs contain no secrets.
5. **Tool behavior.** Invalid args fail schema validation; timeouts produce
   `TOOL_TIMEOUT`; dedup: restoring with a recorded result never re-executes the tool.
6. **Model gateway.** Mock provider drives a full agent run; openai/anthropic adapters pass
   fixture-based contract tests (recorded responses) without live keys.
7. **Events/API/CLI.** Full lifecycle observable via replay + SSE; every CLI command works
   against a running daemon in integration tests.
8. **State integrity.** Schema migration 0→current on fresh DB; downgrade rejected; corrupted
   DB surfaced, never silently overwritten.
9. **Parallel tool calls.** A scripted batch of ≥2 calls executes concurrently (bounded), all
   recorded, results returned in one follow-up turn; restore never re-executes recorded calls.
10. **Memory.** An agent writes durable memory; a later execution reads the same value after a
    daemon restart.
11. **Scheduler.** A `every: 2s` agent runs repeatedly; `skip_if_running` is honored;
    `next_run_at` advances correctly.
12. **Transcript.** `GET /executions/{id}/transcript` returns the complete ordered record of a
    finished run.

---

## 19. Non-normative notes

- Performance targets (informational): agent loop overhead < 50 ms/step excluding model/tool
  latency; checkpoint create < 20 ms on typical local hardware; SQLite event append < 5 ms.
  Benchmarked, not gated.
- The `mock` provider and `noop`/`sleep` tools exist for tests/demos and are documented as
  such; they are not production capabilities.
