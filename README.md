# Kern

**The open-source runtime for reliable, long-running AI agents.**

<p align="center">
  <img src="https://img.shields.io/badge/version-0.3.0-blue" alt="Version">
  <img src="https://img.shields.io/badge/license-Apache--2.0-green" alt="License">
  <img src="https://img.shields.io/badge/status-stable-brightgreen" alt="Status">
  <img src="https://img.shields.io/badge/rust-1.90%2B-orange" alt="MSRV">
</p>

Kern executes AI agents as durable software processes. It owns execution, tools, state,
memory, permissions, isolation, checkpoints, recovery, scheduling and observability around the
model. An agent that crashes, is killed, or whose machine restarts resumes from its last
checkpoint instead of starting over.

Everything is local: one daemon, one SQLite file, one bearer token, no account and no hosted
database.

---

## Contents

- [Highlights](#highlights)
- [Install](#install)
- [Quick start](#quick-start)
- [Terminal dashboard](#terminal-dashboard)
- [Agent configuration](#agent-configuration)
- [CLI reference](#cli-reference)
- [HTTP API](#http-api)
- [Tools](#tools)
- [Permissions and credentials](#permissions-and-credentials)
- [Sandboxing](#sandboxing)
- [Durable execution and recovery](#durable-execution-and-recovery)
- [Observability and cost](#observability-and-cost)
- [Scheduling](#scheduling)
- [Environment variables](#environment-variables)
- [Project layout](#project-layout)
- [Development](#development)
- [Documentation](#documentation)
- [License](#license)

---

## Highlights

- **Durable execution.** Execution state, tool history and checkpoints live in SQLite (WAL,
  crash-safe). After a crash the agent is restored from its last checkpoint and continues — it
  never re-runs a tool call it already finished.
- **Model abstraction.** A small provider trait with raw-HTTP adapters: OpenAI, Anthropic,
  Ollama, and a deterministic mock that needs no key.
- **Tool system.** Capabilities with name, description, input schema and permission class.
  Builtins: filesystem, http, memory, sleep, shell (off by default). MCP servers plug in as
  `<server>.<tool>` behind the same policy gate.
- **Security by default.** The model is never the security boundary: every tool call is
  `allow`/`deny`/`ask`, and `ask` parks the agent in `waiting` until a human decides.
- **Sandboxing.** Platform-specific and fail-closed: bubblewrap/landlock on Linux, seatbelt on
  macOS, job objects on Windows.
- **Live steering.** `kern message <agent> "..."` queues input the runner applies at its next
  safe point — durable, delivered exactly once, no interruption of the call in flight.
- **Sub-agents.** Declare `subagents[]` and the model gets an `agent.<name>` tool that runs a
  real child execution with a validated subset of the parent's tools.
- **Terminal dashboard.** `kern tui` shows agents, live events, approvals, usage and tools, and
  drives lifecycle actions without leaving the terminal.
- **Token accounting.** Every turn's usage is recorded with provenance — `reported` by the
  provider or `estimated` from a documented per-family estimate — and persisted per execution
  before any state change, so a crash cannot lose billed tokens.
- **Model routing.** `model.routing[]` swaps provider/model mid-run based on step count or
  cumulative tokens.
- **Context compaction.** Past `runtime.compact_above_tokens`, older turns fold into a
  deterministic digest instead of being silently dropped. No summarizer call.
- **Scoped credentials.** `credentials[]` injects secrets into exactly the tools you name;
  values are never logged, evented, or shown to the model.
- **MCP in both directions.** Kern consumes MCP servers' tools *and* serves an agent's
  stateless tools over MCP stdio (`kern mcp serve`).
- **Scheduling.** Interval, cron, or one-shot `at`, with crash-safe scheduling state.

---

## Install

Prebuilt binaries are self-contained — no clone, no Rust toolchain:

**macOS / Linux**

```bash
curl -fsSL https://raw.githubusercontent.com/chitajohn/Kern/main/scripts/install.sh | sh
```

**Windows** (PowerShell)

```powershell
irm https://raw.githubusercontent.com/chitajohn/Kern/main/scripts/install.ps1 | iex
```

Targets: x86_64 and aarch64 Linux, x86_64 and aarch64 macOS, x86_64 Windows. The installers
verify the published SHA-256, install into a per-user directory (`~/.local/bin` on Unix,
`%LOCALAPPDATA%\Kern\bin` on Windows) and add it to `PATH`. Override with `KERN_VERSION` /
`KERN_INSTALL_DIR` (`-Version` / `-InstallDir` on Windows).

**From crates.io**

```bash
cargo install kern-runtime        # installs the `kern` binary
```

**From source** (Rust 1.90+, pinned in `rust-toolchain.toml`)

```bash
cargo install --git https://github.com/chitajohn/Kern kern-runtime
```

---

## Quick start

Two terminals, no API key required:

```bash
kern init          # ~/.kern, API token, and a commented agent.yaml (mock provider)
kern daemon        # the runtime; leave it running
```

```bash
kern run agent.yaml --wait      # validate, create, start, follow to completion
kern tui                        # or watch everything in the dashboard
```

`kern init` scaffolds an agent on the **mock provider**, so the first run works offline. To use
a real model, change `agent.yaml` and set the matching key:

```bash
export OPENAI_API_KEY=sk-...        # provider: openai
export ANTHROPIC_API_KEY=sk-ant-... # provider: anthropic
# ollama needs no key: run it locally and use provider: ollama
```

The recovery proof is one `kill -9` away:

```bash
kern run agent.yaml          # note the agent name
kern checkpoint <agent>      # force a checkpoint
# kill -9 the daemon
kern daemon                  # restart
kern inspect <agent>         # state: recovering, with the checkpoint summary
kern resume <agent>          # continues from the checkpoint
kern logs <agent>            # the full event history survived
```

---

## Terminal dashboard

`kern tui` is a full-screen client over the same API as every other command. It never touches
the database, adds no state, and changes nothing until you press a key.

```
 KERN  v0.3.0  ● online  127.0.0.1:8787   agents 2   tokens 1.2k in / 340 out   schema v6
┌──────────────┬───────────────────────────────────────────────────────────────┐
│    ◤ logo ◢  │  Overview │ Events │ Permissions │ Tools                     │
│  runtime for │  state      running                                          │
├ agents (2) ──┤  model      openai / gpt-4o-mini                              │
│ ● researcher │  tools      filesystem, http                                  │
│ ◐ writer     │  tokens     1200 in / 340 out across 2 run(s)                 │
└──────────────┴───────────────────────────────────────────────────────────────┘
 [↑↓] select  [1-4] tab  [m]essage  [s]tart  [p]ause  [u]resume  [c]heckpoint  [t]erminate
```

| Key | Action |
|-----|--------|
| `↑`/`↓`, `j`/`k` | Move the selection (agents, events, approvals) |
| `1`–`4`, `Tab` | Overview · Events · Permissions · Tools |
| `h` / `l`, `Enter` | Focus the agent list / the main panel |
| `m` | Message the selected agent (delivered at its next safe point) |
| `s` `p` `u` `c` | Start · pause · resume · checkpoint |
| `t` | Terminate (asks for confirmation) |
| `y` / `n` | Grant / deny the selected approval |
| `/` `x` | Filter agents · clear the filter |
| `f`, `g`/`G` | Follow the event stream · jump to top/bottom |
| `r` | Refresh now |
| `?` | Help overlay (logo + full keymap) |
| `q` | Quit (`Ctrl-C` too) |

The **Events** pane appends incrementally (`?after=<seq>`) and shows live `model.delta` text
as it arrives when `runtime.stream` is on. If a poll fails, the header flips to `offline`, the
last snapshot stays visible with a warning, and the TUI keeps retrying — it never disguises
stale data as live.

Non-interactive stdout is refused (`exit 1`), so scripts should use `kern ps`, `kern logs`,
`kern inspect`, `kern usage` and the other plain-text commands.

---

## Agent configuration

Agents are declarative. `agent.yaml` (schema v1, validated on load, with field paths in
errors):

```yaml
version: 1
name: researcher
description: "A research agent with a bounded workspace."
model:
  provider: openai            # openai | anthropic | ollama | mock
  model: gpt-4o-mini
  temperature: 0.2
  routing:                    # optional: switch model mid-run
    - when: { min_tokens: 20000 }
      provider: anthropic
      model: claude-sonnet-4
tools: [filesystem, http, memory, github.create_issue]
memory:
  enabled: true
  backend: file               # store | file (one JSON file per agent)
permissions:
  filesystem:
    read:  { allow: [./workspace] }
    write: { allow: [./workspace] }
  network:
    allow: [api.github.com]
  mcp:
    allow: ["github.create_issue"]
runtime:
  max_steps: 100
  checkpoint_interval: 60s
  checkpoint_retention: 50
  compact_above_tokens: 60000 # optional: fold old turns into a digest
  compact_keep_recent: 12
  stream: true                # broadcast live model text as `model.delta`
  schedule:
    every: 1h                 # or cron: "0 3 * * *", or at: "2026-09-01T00:00:00Z"
subagents:                    # optional: expose child agents as tools
  - name: helper
    description: "Searches the workspace"
    model: { provider: mock, model: test }
    tools: [filesystem]
credentials:                  # optional: scoped secret injection
  - name: github
    source: env:GITHUB_TOKEN
    target: http              # shell | http | mcp
    header: Authorization
    prefix: "Bearer "
```

Unknown fields are rejected, so a typo in a key is an error rather than a silent default.

---

## CLI reference

The CLI is a control interface to the runtime, not the runtime itself: it speaks the daemon's
HTTP API and never opens the store.

| Command | Purpose |
|---------|---------|
| `kern init` | First-run scaffolding: `~/.kern`, API token, `agent.yaml` |
| `kern daemon` | Run the runtime in the foreground (logs to `~/.kern/logs/runtime.jsonl`) |
| `kern run agent.yaml [--wait]` | Validate, create, and start an agent |
| `kern ps` | Table of agents: name, id, state, updated |
| `kern tui [--refresh-ms N]` | Full-screen dashboard (see above) |
| `kern logs <agent> [-f] [--limit N]` | Durable event replay, or follow live |
| `kern inspect <agent>` | Spec summary, state, latest checkpoint, last error |
| `kern pause` / `resume` / `checkpoint` / `terminate` | Lifecycle control |
| `kern message <agent> "text"` | Queue input for the next safe point |
| `kern permissions` / `grant <id>` / `deny <id>` | Resolve `waiting` agents' `ask` requests |
| `kern schedule <agent>` | Show schedule and next run |
| `kern tools` / `kern models` | Capability inventory |
| `kern usage [--agent <agent>] [--json]` | Token totals per agent and runtime-wide |
| `kern compact [--per-agent N]` | Prune the event log, keeping the newest N per agent |
| `kern mcp serve agent.yaml [--workspace DIR]` | Serve an agent's stateless tools over MCP stdio |
| `kern doctor` | Environment health: store integrity, daemon, sandbox, provider keys |
| `kern version` | Version, API address, live daemon info |

Exit codes: `0` success, `1` runtime/client error, `2` usage error.

---

## HTTP API

Loopback only, bearer-token authenticated (`KERN_TOKEN` or `$KERN_HOME/token`), errors shaped
`{code, message, detail?}`.

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `/health` | Version, schema, sandbox backend |
| `GET`/`POST` | `/agents` | List agents · create from a spec |
| `GET` | `/agents/{id}` | Agent detail |
| `POST` | `/agents/{id}/start\|pause\|resume\|checkpoint\|terminate` | Lifecycle |
| `POST` | `/agents/{id}/messages` | Queue operator input |
| `GET` | `/agents/{id}/checkpoints` | Checkpoint summaries |
| `GET` | `/agents/{id}/events?after=&limit=` | Event history |
| `GET` | `/events/stream?after=` | SSE: durable events + live `model.delta` |
| `POST` | `/events/compact` | Prune the event log |
| `GET` | `/permissions/pending` | Approvals awaiting a human |
| `POST` | `/permissions/{id}/grant\|deny` | Decide |
| `GET` | `/usage` | Token totals |
| `GET` | `/tools`, `/models` | Capability discovery |

---

## Tools

| Tool | Description | Default permission |
|------|-------------|--------------------|
| `filesystem` | read / write / list / stat | Per-root `read`/`write` allow-lists |
| `http` | GET / POST | Per-host allow-list |
| `memory` | Durable agent-scoped key/value | Key allow-list |
| `sleep` | Pause (used in tests and demos) | No permission required |
| `shell` | Execute commands | Denied unless allowed |
| `agent.<name>` | Delegate to a declared sub-agent | Allowed only if declared |
| `<server>.<tool>` | Tools from an MCP server | `permissions.mcp` allow-list |

Shell commands run with a scrubbed environment; `credentials[].target: shell` is the only way a
secret reaches them.

### MCP servers (Kern as a client)

```yaml
mcp:
  servers:
    - name: fs
      command: npx
      args: ["-y", "@modelcontextprotocol/server-filesystem", "./workspace"]
      timeout: 30s

tools: [filesystem, fs.read_file, fs.list_directory]
permissions:
  mcp:
    allow: ["fs.read_file", "fs.list_directory"]
```

Tools are namespaced `<server>.<tool>` and enumerated at agent start. Only servers you name are
launched; they get a scrubbed environment (the `env:` map is the only way in), are killed with
their whole process tree, and are denied until `permissions.mcp` allows them.

### Kern as an MCP server

`kern mcp serve agent.yaml` speaks MCP 2024-11-05 over stdio and serves the spec's *stateless*
tools (filesystem, http, shell, noop, sleep) through the same classifier the engine uses. Tools
that need the daemon's store or engine (memory, sub-agents, proxied MCP servers) are rejected at
start rather than served against the wrong state. An `ask` rule fails closed there, because that
surface has no operator channel.

---

## Permissions and credentials

Every tool call resolves to `allow`, `deny`, or `ask`:

- **allow** — execute immediately.
- **deny** — return an error to the model.
- **ask** — park the agent in `waiting`; a human decides with `kern grant`/`kern deny` or the
  TUI's Permissions tab. The decision is durable, so a crash while waiting does not lose it.

`credentials[]` keeps secrets out of the model's reach. A grant is scoped to a named tool
(`shell`, `http`, `mcp`); shell injections happen *after* the environment scrub, HTTP grants
override any header the model supplies, and values are redacted from debug output and never
evented. A credential whose `env:` source is unset is a start-time error, not a silent empty
string.

---

## Sandboxing

| Platform | Backend | Notes |
|----------|---------|-------|
| Linux with `bwrap` | bubblewrap | Full namespace isolation, read-only root, writable workspace |
| Linux without `bwrap` | landlock | Kernel write containment (Linux ≥ 5.13) |
| Linux, neither | rlimit fallback | CPU/FILE/NOFILE limits only |
| macOS | sandbox-exec | Seatbelt profile (deprecated by Apple, still enforced) |
| Windows | job objects | Whole-tree kill on timeout/cancel, memory cap; no filesystem or network isolation, so `required` fails closed |

---

## Durable execution and recovery

Execution state, tool history and checkpoints live in SQLite with WAL and crash-safe commits.
A restart restores the agent from its last checkpoint and continues; finished tool calls are
never re-run, and a tool call interrupted mid-flight is retried as a whole.

Events are the audit trail and are never pruned on a schedule — compaction is explicit
(`kern compact --per-agent N`). `seq` stays monotonic, so a replay cursor pointing at a pruned
event simply sees the gap.

---

## Observability and cost

- **Events.** Every meaningful action is a persisted, structured event, queryable over the API
  and `kern logs`, streamed over SSE.
- **Live text.** With `runtime.stream: true`, model text streams as transient `model.delta`
  events (never persisted — the turn is still recorded from the final response). Every adapter
  streams its native protocol: OpenAI SSE, Anthropic Messages SSE, Ollama NDJSON.
- **Usage.** `Completion { response, usage }` carries provenance; missing halves are filled from
  the estimator and tagged `estimated`. Totals land in `executions.input_tokens` /
  `output_tokens` (schema v6) and surface in `GET /usage`, `kern usage`, and the TUI header.
- **Routing.** `model.routing[]` rules are evaluated per call on step count and cumulative
  tokens; the first match wins and the switch is evented.
- **Compaction.** Transcript digests are recorded as events, so a compacted run is still
  auditable.

---

## Scheduling

```yaml
schedule:
  every: 12h                  # interval
  # cron: "0 3 * * *"         # cron expression
  # at: "2026-09-01T00:00:00Z" # one-shot
  timezone: "UTC"
  skip_if_running: true
```

Scheduling state is durable: a missed tick after downtime is reported, not silently swallowed,
and `kern schedule <agent>` shows the next run.

---

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `KERN_HOME` | `~/.kern` | Data directory (`--home` overrides) |
| `KERN_API_ADDR` | `127.0.0.1:8787` | API bind address |
| `KERN_TOKEN` | `$KERN_HOME/token` | API bearer token |
| `KERN_LOG` | `info` | Tracing level |
| `OPENAI_API_KEY` | — | OpenAI provider |
| `ANTHROPIC_API_KEY` | — | Anthropic provider |
| `OLLAMA_BASE_URL` | `http://127.0.0.1:11434` | Ollama provider |

Data directory layout: `kern.db` (SQLite + WAL), `token`, `logs/runtime.jsonl`, `workspaces/`.

---

## Model providers

| Provider | Key | Notes |
|----------|-----|-------|
| OpenAI | `OPENAI_API_KEY` | GPT-4o family and compatible endpoints |
| Anthropic | `ANTHROPIC_API_KEY` | Claude models |
| Ollama | — | Local models via `OLLAMA_BASE_URL` |
| Mock | — | Deterministic; tests and demos, no key |

---

## Project layout

```
crates/
  kern-core/   runtime: store, events, lifecycle, engine, checkpoints, recovery,
               permissions, policy, sandbox, memory, sub-agents, MCP server,
               scheduler, API/SSE, telemetry
  kern-model/  provider trait, raw-HTTP adapters, token ledger, usage estimation
  kern-tool/   tool trait, registry, executor, builtins (filesystem, http, memory, sleep, shell)
  kern-cli/    the `kern` binary: init, daemon, control CLI, TUI
```

---

## Development

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

New contributors: read [`CONTRIBUTING.md`](CONTRIBUTING.md) first.

---

## Documentation

| Document | Description |
|----------|-------------|
| [README.md](README.md) | This file |
| [CHANGELOG.md](CHANGELOG.md) | Release history |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Design decisions and rationale (§1–§38) |
| [SPEC.md](SPEC.md) | Normative contracts: schemas, lifecycle, API, CLI |
| [TOOL_AUTHORING.md](TOOL_AUTHORING.md) | Tool trait contract and how to add tools |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Development workflow |
| [SECURITY.md](SECURITY.md) | Vulnerability reporting |
| [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) | Community standards |
| [LICENSE](LICENSE) | Apache 2.0 |

---

## License

Apache License, Version 2.0. See [LICENSE](LICENSE) for details.
