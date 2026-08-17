# Authoring tools for Kern

Tools are first-class runtime capabilities. This guide covers the contract a tool must
honor. For the surrounding system, see `ARCHITECTURE.md §9` (tool system), `SPEC.md §11`
(normative tool contract), and the builtins in `crates/kern-tool/src/builtins/` as working
examples.

## The contract

A tool is an `Arc<dyn Tool>` (defined in `crates/kern-tool/src/registry.rs`):

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> &Value;          // JSON Schema for `run` args
    async fn run(&self, args: &Value, ctx: &ToolContext<'_>)
        -> Result<Value, ToolError>;
}
```

`ToolContext` carries the `agent_id`, `execution_id`, and `tool_call_id` — the dedup key
(`SPEC.md §4.3`). Side-effecting tools use it for tool-side idempotency so a re-executed call
(after restore) does not double-apply.

## Rules

1. **Stateless across calls.** All per-call context arrives in `ToolContext`. Do not stash
   state in the tool object; the runtime owns execution state.
2. **Never enforce gateway/engine policy.** Timeouts, concurrency caps, and permission
   checks belong to the `ToolExecutor` and the permission engine. Your tool must not block
   on its own permission logic beyond its own in-tool containment (e.g. the `http` builtin
   enforcing its host allow-list, the `filesystem` builtin enforcing its path roots).
3. **Never panic; return promptly.** `run` must return `Err(ToolError)` instead of panicking,
   and SHOULD respect short request-level deadlines so cancellation is prompt. The executor
   enforces timeouts; a tool that ignores cancellation makes timeouts slow.
4. **Validate inputs by schema.** The registry compiles your `input_schema` once at
   registration and rejects invalid args before `run` is ever called. Make the schema precise
   (types, enums, bounds) — it is also what the model sees when choosing arguments.
5. **Return structured JSON.** Results are `serde_json::Value` and are persisted into the
   transcript and session history. Keep them bounded and useful.
6. **Errors are structured.** Use `ToolError` variants (`InvalidArgs`, `Denied`,
   `Timeout`, `Failed`, … — see `crates/kern-tool/src/error.rs`). A structured error is
   surfaced to the model; a panic or opaque string is a bug.
7. **Idempotent where it matters.** If `run` has side effects keyed by a call id, make it
   safe to run twice for the same `tool_call_id` (the engine already dedups recorded
   results; tool-side idempotency is defense in depth — `SPEC.md §11.1`).

## Wiring a new builtin

1. Implement `Tool` in `crates/kern-tool/src/builtins/<name>.rs`.
2. Register it in `crates/kern-tool/src/builtins/mod.rs` so the runtime's builtin factory
   exposes it, and add it to the config schema's `tools` enum in `kern-core`'s config module.
3. Decide its default permission posture: tools that touch the system (shell, network)
   default to **disabled or deny** — see the shell builtin for the pattern.
4. Add tests: schema validation, success path, error path, and (where relevant) an
   idempotency test under `tool_call_id` replay.
5. Update `SPEC.md §11` (builtins table) and this document if the tool changes the contract.

## Adding a custom (out-of-tree) tool

Any crate can construct a `Tool` and register it with a `ToolRegistry`. The engine builds
its registry per agent from the config's `tools` list plus runtime-sourced tools, so
integration is the same: implement the trait, register, configure. Nothing about the tool
system requires modifying `kern-core`.
