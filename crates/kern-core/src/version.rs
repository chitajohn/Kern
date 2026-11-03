//! Version constants (SPEC.md §5, §7).

/// Kern runtime version, from Cargo metadata.
pub const KERN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Storage schema version currently supported (SPEC.md §5).
/// v2: `permission_requests.expires_at` — approval TTL.
/// v3: `executions.input` — durable pre-start task input.
/// v4: `executions.wake_at` — durable wake/sleep (ARCHITECTURE.md §26).
/// v5: `agent_messages` — durable mid-run operator input (ARCHITECTURE.md §30).
/// v6: `executions.input_tokens/output_tokens` — durable token accounting
///     (ARCHITECTURE.md §31).
pub const STORAGE_SCHEMA_VERSION: u32 = 6;

/// Checkpoint format version currently supported (SPEC.md §7).
pub const CHECKPOINT_FORMAT_VERSION: u32 = 1;
