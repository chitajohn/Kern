//! Builtin tools (SPEC.md §11.3).
//!
//! `shell` lands with the sandbox — it must never be constructed
//! without a runtime-enforced isolation layer to run through (the gate lives
//! in `kern-core`, which injects the sandboxed `CommandRunner`).

pub mod filesystem;
pub mod http;
pub mod memory;
pub mod noop;
pub mod shell;
