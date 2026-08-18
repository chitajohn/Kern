# Kern

**The open-source runtime for reliable, long-running AI agents.**

<p align="center">
  <img src="https://img.shields.io/badge/version-0.1.0-blue" alt="Version">
  <img src="https://img.shields.io/badge/license-Apache--2.0-green" alt="License">
  <img src="https://img.shields.io/badge/status-stable-brightgreen" alt="Status">
</p>

Kern executes AI agents as durable software processes. Execution, tools, state, memory,
permissions, isolation, checkpoints, recovery, scheduling, and observability around the
model. An agent that crashes, is killed, or whose machine restarts resumes from its last
checkpoint instead of starting over.

---

## Features

- **Durable Execution** -- Execution state, tool history, and checkpoints live in SQLite (WAL, crash-safe). Restarting Kern restores an agent from its last checkpoint and continues.

- **Model Abstraction** -- A clean provider trait with raw-HTTP adapters: OpenAI, Anthropic, Ollama, and a deterministic mock for tests and demos.

- **Tool System** -- First-class capabilities with name, description, input schema, permission requirements, and explicit results/errors. Builtins: filesystem, http, memory, sleep. Shell exists but is disabled by default for security.

- **Security by Default** -- The model is never the security boundary. Every tool call passes a policy check (`allow` / `deny` / `ask`). `ask` pauses the agent in `waiting` until a human grants or denies through `kern permissions` + `kern grant`.

- **Sandboxing** -- Platform-specific isolation, fail-closed. Linux: bubblewrap, landlock, rlimit fallback. macOS: sandbox-exec (seatbelt). Windows: none in v0.1.

- **Observability** -- Every meaningful action is a structured, persisted event, queryable via the API and `kern logs`, streamed over SSE.

- **Scheduling** -- Agents can run on an interval or cron schedule with crash-safe scheduling state.

---

## Quick Start

Requires Rust 1.90+ (stable, pinned in `rust-toolchain.toml`). Everything is local: no account, no cloud, no hosted database.

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

`kern init` scaffolds an agent that uses the **mock provider**, so the first run works with **zero API keys**. Switch the model provider in `agent.yaml` and set the matching key to use a real model:

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

## Agent Configuration

Agents are declarative. `agent.yaml` (schema v1, validated on load):

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

The same definition runs anywhere -- a laptop, a server, a future distributed Kern -- without rewriting the agent.

---

## CLI Reference

The CLI is a control interface to the runtime, not the runtime itself. It talks to the daemon's HTTP API and never touches the store directly.

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

## Configuration

### Runtime Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `KERN_HOME` | `~/.kern` | Data directory |
| `KERN_API_ADDR` | `127.0.0.1:8787` | API bind address |
| `KERN_TOKEN` | (token file) | API bearer token |
| `KERN_LOG` | `info` | Tracing level |
| `OPENAI_API_KEY` | -- | OpenAI provider |
| `ANTHROPIC_API_KEY` | -- | Anthropic provider |
| `OLLAMA_BASE_URL` | `http://127.0.0.1:11434` | Ollama provider |

### Provider Configuration

Set the appropriate API key for your provider:

```bash
export OPENAI_API_KEY=sk-...        # for OpenAI
export ANTHROPIC_API_KEY=sk-ant-...  # for Anthropic
# Ollama requires no key; ensure it's running locally
```

---

## Model Providers

| Provider | Environment Variable | Notes |
|----------|---------------------|-------|
| OpenAI | `OPENAI_API_KEY` | GPT-4o, GPT-4o-mini, etc. |
| Anthropic | `ANTHROPIC_API_KEY` | Claude models |
| Ollama | -- | Local models via `OLLAMA_BASE_URL` |
| Mock | -- | For tests and demos (no key required) |

---

## Tool System

### Built-in Tools

| Tool | Description | Default Permission |
|------|-------------|-------------------|
| `filesystem` | Read/write/list/stat files | Allow within configured roots |
| `http` | GET/POST requests | Allow to configured hosts |
| `memory` | Durable agent-scoped KV storage | Allow to configured keys |
| `sleep` | Pause execution (for testing) | No permission required |
| `shell` | Execute shell commands | Disabled by default (security) |

### Security Model

The model is never the security boundary. Every tool call passes through the permission engine:

- **allow** -- Execute immediately
- **deny** -- Reject, return error to model
- **ask** -- Pause agent, wait for human approval via `kern grant`

---

## Sandboxing

| Platform | Backend | Notes |
|----------|---------|-------|
| Linux (with `bwrap`) | bubblewrap | Full namespace isolation, read-only root, writable workspace |
| Linux (no `bwrap`) | landlock | Kernel-enforced write containment (Linux >= 5.13) |
| Linux (no `bwrap`, no landlock) | rlimit fallback | CPU/FSIZE/NOFILE limits only |
| macOS | sandbox-exec | Seatbelt profile (deprecated by Apple) |
| Windows | none | In-tool containment only (v0.1) |

---

## Durable Execution

Execution state, tool history, and checkpoints live in SQLite (WAL, crash-safe commits). Restarting Kern restores an agent from its last checkpoint and continues -- never re-runs already-finished tool calls.

### Recovery

```bash
kern run agent.yaml          # start an agent
kern checkpoint <agent>      # create a checkpoint
# kill -9 the daemon
kern daemon                  # restart
kern inspect <agent>         # agent is recovering
kern resume <agent>          # picks up from checkpoint
```

---

## Scheduling

Agents can run on an interval or cron schedule:

```yaml
schedule:
  every: 12h              # interval, OR
  cron: "0 3 * * *"       # cron expression, OR
  at: "2026-09-01T00:00:00Z"  # one-shot RFC3339
  timezone: "UTC"         # optional, default UTC
  skip_if_running: true   # optional, default true
```

---

## Project Layout

```
crates/
  kern-core/   runtime: store, events, lifecycle, engine, checkpoints, recovery,
               permissions, sandbox, scheduler, API, telemetry
  kern-model/  model gateway: provider trait + OpenAI/Anthropic/Ollama/mock adapters
  kern-tool/   tool system: trait, registry, executor, builtins
  kern-cli/    the `kern` binary: init, daemon, and the control CLI
```

---

## Development

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
cargo bench --bench limits      # optional smoke benchmarks
```

New contributors: read [`CONTRIBUTING.md`](CONTRIBUTING.md) first.

---

## Documentation

| Document | Description |
|----------|-------------|
| [README.md](README.md) | This file |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Development workflow and contribution guidelines |
| [Code of Conduct](CODE_OF_CONDUCT.md) | Community standards |
| [SECURITY.md](SECURITY.md) | Vulnerability reporting |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Design decisions and rationale |
| [SPEC.md](SPEC.md) | Normative contracts and schemas |
| [TOOL_AUTHORING.md](TOOL_AUTHORING.md) | Tool trait contract and how to add tools |
| [LICENSE](LICENSE) | Apache 2.0 license |

---

## License

Apache License, Version 2.0. See [LICENSE](LICENSE) for details.
