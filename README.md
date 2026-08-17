# Kern

**The open-source runtime for reliable, long-running AI agents.**

<p align="center">
  <img src="https://img.shields.io/badge/status-v0.1.0--pre--release-orange" alt="Status">
  <img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="License">
  <img src="https://img.shields.io/badge/runtime-local--first-111111" alt="Local First">
  <img src="https://img.shields.io/badge/AI-model--agnostic-111111" alt="Model Agnostic">
</p>

<p align="center">
  <strong>Agents should run like software, not like prompts.</strong>
</p>

                         K E R N
              ───────────────────────────

                    ┌─────────────┐
                    │    AGENT    │
                    └──────┬──────┘
                           │
              ┌────────────▼────────────┐
              │       KERN RUNTIME      │
              │                         │
              │  Execution   Scheduling │
              │  State       Checkpoint │
              │  Memory      Recovery   │
              │  Tools       Security   │
              │  Events      Networking │
              └────────────┬────────────┘
                           │
             ┌─────────────┼─────────────┐
             ▼             ▼             ▼
          Models         Tools         System
       ┌──────────┐   ┌──────────┐   ┌──────────┐
       │ OpenAI   │   │ Files    │   │ Memory   │
       │ Anthropic│   │ Network  │   │ Processes│
       │ Ollama   │   │ HTTP     │   │ Sandbox  │
       │ Mock     │   │ Shell*   │   │ Events   │
       └──────────┘   └──────────┘   └──────────┘

          One runtime. Durable execution.
        (* shell tool disabled by default)

Kern executes AI agents as durable software processes: execution, tools, state, memory,
permissions, isolation, checkpoints, recovery, scheduling, and observability around the
model. An agent that crashes, is killed, or whose machine restarts resumes from its last
checkpoint instead of starting over.

Today's agents are usually a single loop in an application process:

    prompt → model → tool → model → tool → done

That works for demos. It falls apart when an agent must run for hours, survive crashes,
execute untrusted code, respect permissions, and report progress to another application.
Kern treats those as **runtime problems**, not application problems.

---

## Status

Kern v0.1.0 is implemented and tested. Nothing below is claimed that is not exercised by
the test suite (currently **318 tests**, including a SIGKILL recovery proof, a
deterministic fault-injection matrix over every persisted-write boundary, a CLI
integration suite that drives the real daemon, a redaction audit, state-corruption tests,
seeded fuzz-lite tests, and real-provider smoke tests against a live OpenAI-compatible
endpoint). Known limitations — no seccomp; landlock-only write containment where `bwrap`
is absent (no network isolation; memory is capped only when
`runtime.tool_memory_limit_mb` is set); no Anthropic/Ollama live coverage; unbounded
event growth unless `KERN_EVENT_RETENTION` is set — are documented in
`ARCHITECTURE.md §22`/§27.

| Capability | Status | Where |
|-----------|--------|-------|
| Durable SQLite state (WAL, single file, schema v4) | ✅ shipped | `SPEC.md §5`, `ARCHITECTURE.md §3` |
| Event system (catalog-pinned kinds, broadcast + persisted replay) | ✅ shipped | `SPEC.md §7`, `ARCHITECTURE.md §12` |
| Declarative agent config (`agent.yaml`, schema v1, validated) | ✅ shipped | `SPEC.md §9` |
| Agent lifecycle + engine loop (parallel tool batches, bounded history, execution budgets) | ✅ shipped | `SPEC.md §6`, `ARCHITECTURE.md §6–7, §24` |
| Model gateway: OpenAI, Anthropic, Ollama, mock | ✅ shipped | `SPEC.md §10`, `ARCHITECTURE.md §8` |
| Tool system + builtins (filesystem, http, shell-off-by-default, memory, sleep) | ✅ shipped | `SPEC.md §11`, `ARCHITECTURE.md §9` |
| Permission engine (allow / deny / ask, approval TTL) | ✅ shipped | `ARCHITECTURE.md §10, §22.5` |
| Sandboxing (bubblewrap, **landlock**, rlimit fallback, sandbox-exec; see matrix) | ✅ shipped | `SPEC.md §12`, `ARCHITECTURE.md §10.2, §17` |
| Tool resource caps (timeout, output, **memory via `RLIMIT_AS`**) | ✅ shipped | `ARCHITECTURE.md §22.1, §27.4` |
| Checkpointing + crash recovery (SIGKILL test + deterministic fault-injection matrix) | ✅ shipped | `SPEC.md §13`, `ARCHITECTURE.md §11, §27` |
| Durable wake/sleep (runner-unloaded sleeps that survive restart) | ✅ shipped | `ARCHITECTURE.md §26` |
| Supervisor (stuck/lost-runner detection, `RUNNER_LOST`) | ✅ shipped | `ARCHITECTURE.md §25.1` |
| Scheduler (interval + cron + crash-loop backoff) | ✅ shipped | `SPEC.md §14`, `ARCHITECTURE.md §13, §24.4` |
| Local HTTP API + SSE event streaming | ✅ shipped | `SPEC.md §15`, `ARCHITECTURE.md §12.3` |
| CLI control interface (`kern ps`, `logs`, `inspect`, `doctor`, …) | ✅ shipped | `SPEC.md §16`, `ARCHITECTURE.md §12.4` |
| Secret redaction in all log layers + scrubbed tool subprocess envs | ✅ shipped | `ARCHITECTURE.md §22.3, §22.5` |
| Benchmarks (smoke targets) | ✅ shipped | README "Benchmarks" below |

**Known limitations (honest, not hidden):** no seccomp filter yet; the Linux landlock/rlimit
backends do not isolate the network (namespaces/bwrap do); macOS `sandbox-exec` is
deprecated by Apple; Windows has no OS sandbox in v0.1 (fail-closed when sandbox is
required); persisted event history is unbounded unless `KERN_EVENT_RETENTION` is set
(warning at 100k events per agent — `ARCHITECTURE.md §22.2`).

---

## Quickstart

Requires Rust 1.90+ (stable, pinned in `rust-toolchain.toml`). Everything is local: no
account, no cloud, no hosted database.

```bash
cargo build --release
export PATH="$PWD/target/release:$PATH"

kern init            # creates ~/.kern, API token, and a commented agent.yaml
kern daemon          # starts the runtime (keep this terminal open)
```

In a second terminal:

```bash
kern run agent.yaml --wait
```

`kern init` scaffolds an agent that uses the **mock provider**, so the first run works with
**zero API keys**. Switch the model provider in `agent.yaml` and set the matching key to use
a real model:

```bash
export OPENAI_API_KEY=sk-...
# or: export ANTHROPIC_API_KEY=...
# or: run a local Ollama and use provider: ollama
```

The durable-recovery proof is one `kill -9` away:

```bash
kern run agent.yaml          # in one terminal, note the agent name
kern checkpoint <agent>      # in another, then: kill -9 the daemon
kern daemon                  # restart the runtime
kern inspect <agent>         # agent is recovering
kern resume <agent>          # picks up from the checkpoint
kern logs <agent>            # full event history survived
```

---

## A Kern agent

Agents are declarative. `agent.yaml` (schema v1, validated on load — see `SPEC.md §9`):

```yaml
version: 1
name: researcher
description: "A research agent with a bounded workspace."
model:
  provider: openai          # openai | anthropic | ollama | mock
  model: gpt-4o-mini
tools:
  - filesystem
  - http
  - memory
permissions:
  filesystem:
    read:  { allow: [./workspace] }
    write: { allow: [./workspace] }
  network:
    allow: [api.github.com]
runtime:
  checkpoint_interval: 60s
  max_steps: 100
  checkpoint_retention: 50
  schedule:                # optional: run on an interval or cron
    every: 1h
```

The same definition runs anywhere — a laptop, a server, a future distributed Kern — without
rewriting the agent.

---

## CLI

The CLI is a control interface to the runtime, not the runtime itself. It talks to the
daemon's HTTP API and never touches the store directly.

| Command | Purpose |
|---------|---------|
| `kern init` | First-run scaffolding: `~/.kern`, API token, `agent.yaml` |
| `kern daemon` | Run the runtime (foreground, logs to `~/.kern/logs/runtime.jsonl`) |
| `kern run agent.yaml [--wait]` | Validate, create, and start an agent |
| `kern ps` | List agents and their lifecycle state |
| `kern logs <agent> [-f]` | Durable event replay (or follow live) |
| `kern inspect <agent>` | Spec summary, state, latest checkpoint |
| `kern pause` / `resume` / `checkpoint` / `terminate` | Lifecycle control |
| `kern permissions` / `grant` / `deny` | Resolve `waiting` agents' `ask` requests |
| `kern schedule <agent>` | Show next scheduled runs |
| `kern tools` / `kern models` | Runtime capability inventory |
| `kern doctor` | Environment health: DB integrity, provider keys, sandbox backend |
| `kern version` | Version, schema version, sandbox backend |

---

## What Kern provides

**Durable execution.** Execution state, tool history, and checkpoints live in SQLite (WAL,
crash-safe commits). Restarting Kern restores an agent from its last checkpoint and
continues — never re-runs already-finished tool calls.

**Model abstraction.** A clean provider trait with raw-HTTP adapters (no SDKs): OpenAI,
Anthropic, Ollama, and a deterministic mock for tests and demos.

**Tools.** First-class capabilities with name, description, input schema (JSON-Schema
validated), permission requirements, and explicit results/errors. Builtins: filesystem,
http, memory; shell exists but is **disabled by default** for security.

**Security by default.** The model is never the security boundary. Every tool call passes a
policy check (`allow` / `deny` / `ask`). `ask` pauses the agent in `waiting` until a human
grants or denies through `kern permissions` + `kern grant`. Secrets come from the
environment only and are redacted from every log layer (audited by integration test).

**Sandboxing.** Platform-specific isolation, fail-closed:

| Platform | Backend | `sandbox: required` | `sandbox: best-effort` |
|----------|---------|---------------------|------------------------|
| Linux (with `bwrap`) | bubblewrap | namespaces, read-only root, writable workspace, rlimits, dropped caps, `--die-with-parent` | same |
| Linux (no `bwrap`) | **landlock** (kernel LSM, Linux ≥ 5.13, no external binary) | read-everywhere, writes only in workspace + `/tmp`; rlimits; no network/memory isolation | same |
| Linux (no `bwrap`, no landlock) | rlimit fallback | agent **fails to start** | CPU/FSIZE/NOFILE limits; no network/memory isolation |
| macOS | `sandbox-exec` (seatbelt) | deny-default profile, no network | same; deprecated by Apple |
| Windows | none in v0.1 | agent **fails to start** | no OS isolation (in-tool containment only) |

No seccomp filter in v0.1; the filesystem and http tools enforce their own path/host
constraints on every platform (defense in depth).

**Observability.** Every meaningful action is a structured, persisted event — `agent.started`,
`tool.requested`, `tool.completed`, `checkpoint.created`, `agent.paused`, `agent.completed` —
queryable via the API and `kern logs`, streamed over SSE.

**Scheduling.** Agents can run on an interval or cron schedule with crash-safe scheduling
state.

---

## Benchmarks

Benchmarks in `crates/kern-core/benches/limits.rs` (dev profile, informational — not CI
gates):

| Target | Measured (dev profile) |
|--------|------------------------|
| Event append (persist + broadcast) | ~53 µs / event |
| Checkpoint commit (full transaction) | ~67 µs |
| One agent-loop turn (mock model, no tools) | ~4 ms |

```bash
cargo bench --bench limits
```

These are order-of-magnitude regression checks, not marketing numbers. Re-measure on
release builds and real hardware before quoting them anywhere.

---

## Project layout

```
crates/
  kern-core/   runtime: store, events, lifecycle, engine, checkpoints, recovery,
               permissions, sandbox, scheduler, API, telemetry
  kern-model/  model gateway: provider trait + OpenAI/Anthropic/Ollama/mock adapters
  kern-tool/   tool system: trait, registry, executor, builtins
  kern-cli/    the `kern` binary: init, daemon, and the control CLI
ARCHITECTURE.md    why: design, decisions, limits audit
SPEC.md            what (normative): DDL, event catalog, API contract, schemas
```

## Development

```bash
cargo build --workspace
cargo test --workspace          # unit + integration, incl. SIGKILL recovery proof
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
cargo bench --bench limits      # optional smoke benchmarks
```

New contributors: read [`CONTRIBUTING.md`](CONTRIBUTING.md) first (workflow, CI gates,
review expectations). Tool authors: [`TOOL_AUTHORING.md`](TOOL_AUTHORING.md) has the `Tool`
trait contract.

## License

Apache-2.0. See [LICENSE](LICENSE).
