//! Kern runtime core.
//!
//! This crate implements the Kern runtime: agent lifecycle, the execution engine,
//! durable state, events, checkpoints, recovery, permissions, sandboxing,
//! scheduling, configuration, and the local API.
//!
//! The structured error taxonomy, telemetry foundation, the durable store, the
//! catalog-pinned event system, validated agent/runtime configuration, the
//! lifecycle state machine, runner task management, the model gateway
//! (`kern-model`), the tool system (`kern-tool`), the permission engine, and
//! the fail-closed sandbox backends.

pub mod api;
pub mod checkpoint;
pub mod config;
pub mod engine;
pub mod error;
pub mod event;
pub mod fault;
pub mod lifecycle;
pub mod permissions;
pub mod recovery;
pub mod sandbox;
pub mod schedule;
pub mod scheduler;
pub mod store;
pub mod tasks;
pub mod telemetry;
pub mod tools;
pub mod version;

pub use error::{ErrorCode, KernError, Result};
