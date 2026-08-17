//! Kern tool system (ARCHITECTURE.md §9).
//!
//! `Tool` is the unit of capability; `ToolRegistry` maps names to tools and
//! validates arguments; `ToolExecutor` applies timeout and concurrency policy;
//! `builtins` ships the v0.1 tools (`filesystem`, `http`, `memory.*`,
//! `noop`, `sleep` — `shell` arrives with the sandbox).
//!
//! `MemoryProvider` is the seam that keeps this crate free of the runtime's
//! store: `kern-core` implements it over the `memory` table and hands the
//! builtins back in.

pub mod builtins;
pub mod error;
pub mod executor;
pub mod path;
pub mod process;
pub mod registry;

pub use builtins::shell::{CommandRequest, CommandRunner, ShellTool, SystemRunner};
pub use error::ToolError;
pub use executor::{ToolExecutor, DEFAULT_GLOBAL_CAP, DEFAULT_PER_AGENT_CAP};
pub use process::{CommandOutput, RunLimits, DEFAULT_OUTPUT_CAP};
pub use registry::{MemoryEntry, MemoryProvider, Tool, ToolContext, ToolRegistry};
