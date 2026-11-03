# Changelog

All notable changes to Kern. The format follows [Keep a Changelog](https://keepachangelog.com/),
and the project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- **`kern tui`** — a full-screen dashboard over the same local API every other command
  uses: the agent list with live lifecycle glyphs, an event stream that follows the selected
  agent, pending-approval review with grant/deny, agent detail, a tool catalog, and usage
  totals in the header. It renders the Kern logo in the sidebar and the help overlay, filters
  agents by name/id/state, and sends messages and lifecycle actions (start, pause, resume,
  checkpoint, terminate-with-confirmation) without leaving the terminal. Connection failures
  flip the header to `offline` and keep the last snapshot with a visible warning instead of
  presenting stale data as live. Needs an interactive terminal; the plain-text commands stay
  available for scripts.

## [0.2.0] — 2026-11-03

Every item deferred from 0.1 is now either shipped or an explicit boundary (see
`ARCHITECTURE.md §25.3`).

### Added

- **Token accounting** (`ARCHITECTURE.md §31`). Model turns carry `Usage` with provenance —
  `reported` by the provider or `estimated` from a documented per-family estimate. Totals are
  persisted per execution (schema v6: `executions.input_tokens` / `output_tokens`) before any
  state change, so a crash cannot lose billed tokens. `model.completed` carries the same
  numbers; `GET /usage` and `kern usage [--agent] [--json]` aggregate them.
- **Model routing** (`§32`). `model.routing[]` selects a provider/model per call by step count
  and/or cumulative tokens; the first matching rule wins.
- **Transcript compaction** (`§7.5`). `runtime.compact_above_tokens` folds older turns into a
  deterministic digest (task message and recent tail preserved, no model call, checkpointed
  immediately) instead of silently trimming them.
- **Per-tool credential injection** (`§33`). `credentials[]` grants a secret to specific tools
  (`shell`, `http`, `mcp`), resolved at agent start and failing closed when unset. Values are
  redacted from `Debug`, never logged or evented; an HTTP grant overrides any model-supplied
  header of the same name.
- **File-backed memory** (`§34`). `memory.backend: file` stores an agent's memory in
  `<workspace>/.kern/memory/memory.json` (temp-file + rename, one file per agent) behind the
  unchanged `memory.*` tools; the digest reads through the same provider.
- **Sub-agents / agents-as-tools** (`§35`). `subagents[]` plus `agent.<name>` in `tools` runs a
  real child execution — own workspace, checkpoints, events — with the parent's permissions and
  a validated subset of its tools. Nesting is rejected; a dedicated semaphore sized at
  `global_tool_cap / 2` prevents the obvious deadlock.
- **Kern as an MCP server** (`§36`). `kern mcp serve agent.yaml` serves stateless tools over
  JSON-RPC stdio (protocol `2024-11-05`) under the same policy classifier the engine uses; `ask`
  fails closed because this surface has no operator channel.
- **Event compaction** (`§37`). `kern compact --per-agent N` / `POST /events/compact`.
- Shared `policy::classify`, used by both the engine and the MCP surface, so call
  classification cannot drift between them.

### Changed

- `ModelProvider` gained `complete_counting` / `stream_counting` (default implementations wrap
  the existing methods), and `ModelGateway::complete` / `complete_streaming` now return
  `Completion { response, usage }`.
- `CommandRequest`-level tool plumbing is unchanged, but `SandboxedRunner` carries explicitly
  granted credentials and applies them after the environment scrub.
- Storage schema v6; `kern-core::tools::build_registry` takes the engine (needed for sub-agent
  tools).

### Still out of scope

Durable token streaming (a mid-stream checkpoint is not a resume point), CPU/memory/disk
budgets beyond the per-process address-space cap, vector memory backends, and exact
per-provider tokenizers.

## [0.1.0] — 2026-11-02

First published release: durable execution with checkpoints and crash recovery, lifecycle
state machine, policy engine with `ask` + approval TTL, per-OS fail-closed sandbox, model
gateway with OpenAI/Anthropic/Ollama adapters, tool system with MCP client support, durable
wake/sleep, scheduler, structured event log with SSE, local HTTP API, and the `kern` CLI.
