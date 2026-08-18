# Contributing to Kern

Kern is infrastructure. We optimize for a system that actually works, not for the appearance
of progress. Read `ARCHITECTURE.md` (why) and `SPEC.md` (what, normative) before changing
anything substantial.

## Ground Rules

- **Runtime over framework.** Every abstraction must earn its place with a clear
  responsibility. Fewer, sharper primitives beat clever ones.

- **Security by default.** Model output, tool arguments, and config are never trusted input.
  Kern enforces policy -- the model never does. Never weaken a security boundary; if you find
  one is weaker than documented, fix the code and the docs, honestly.

- **No fake completeness.** A feature is done when implementation + tests + failure handling
  + documentation exist -- not when it compiles or a happy path passes.

- **Never log secrets.** Provider keys are read from the environment and redacted at every
  log layer. The redaction audit tests must stay green.

- **Keep dependencies intentional.** No new dependency without a documented justification
  and a check of maturity, maintenance, license, and security.

- **Cross-platform awareness.** Linux, macOS, and Windows matter. Platform-specific code is
  isolated; limitations are documented, never papered over.

## CI Gates

Every merge must pass:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
```

The test suite includes a SIGKILL crash-recovery proof, a CLI integration suite that drives
the real daemon binary, and a redaction audit. If your change touches any of those surfaces,
the corresponding tests must pass -- not just compile.

## Workflow

1. **File an issue or open a discussion first** for anything architectural. Major design
   decisions are documented in prose in `ARCHITECTURE.md` -- a one-line "add this" does
   not fit that bar.

2. **Small, focused commits.** One logical change per commit, `feat(...)`/`fix(...)`/
   `test(...)` style prefixes. Never commit secrets.

3. **Write the test with the code.** Failure paths ship in the same change as the happy
   path.

4. **Update the docs that describe what you changed.** `ARCHITECTURE.md` (design decisions),
   `SPEC.md` (normative contracts), or `README.md` (user-facing claims). Never mark
   something complete in docs before it is implemented and tested.

5. **Run the full gate locally** (above) before pushing. Be especially careful when
   touching the store, checkpointing, or the engine loop.

## Review Expectations

- Reviewers check behavior against `SPEC.md`, not just style. A change that silently alters
  a normative contract (an event kind, a state transition, an endpoint shape) is a blocker
  until the SPEC and the design documentation are updated.

- Performance-sensitive paths (engine loop, checkpoint transaction, event append) should
  not regress the smoke benchmarks (`cargo bench --bench limits`) without explanation.

- Anything that touches security, checkpointing, recovery, or the event catalog gets extra
  scrutiny and an explicit sign-off line in the PR description.

## Areas That Need Care

- **Store migrations:** Schema v1 is versioned. New migrations must be additive and tested
  (0 -> current on a fresh DB; downgrade rejected; corruption surfaced, never silently
  overwritten).

- **Checkpoint format:** Versioned JSON. Changing the payload shape requires a format
  version bump and a recovery test.

- **Event catalog:** Kinds are catalog-pinned in `SPEC.md`. Adding a kind is fine;
  changing the meaning of an existing kind is a breaking change.

- **The engine loop:** Effectively-once tool semantics and restore behavior are
  load-bearing. Read them before editing `engine.rs`.

## Getting Help

- `ARCHITECTURE.md` -- design, decisions, limits audit
- `SPEC.md` -- normative contracts
- `TOOL_AUTHORING.md` -- the `Tool` trait contract and how to add tools
