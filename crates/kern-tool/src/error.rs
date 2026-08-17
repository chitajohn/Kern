//! Tool error taxonomy (SPEC.md §13 codes).
//!
//! `code()` maps each variant to the normative error code the engine attaches
//! to `tool.failed` events and feeds back to the model. `InvalidArguments` and
//! `PermissionDenied` are *caller* errors (the model can fix them and retry);
//! `Timeout`, `Failed`, and `Unavailable` are *environment* errors (the model
//! MAY continue per SPEC §8.2, but the agent can also fail on them).

use std::time::Duration;

use serde_json::Value;

/// Structured tool error.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("tool timed out after {0:?}")]
    Timeout(Duration),
    #[error("tool failed: {0}")]
    Failed(String),
    #[error("tool unavailable: {0}")]
    Unavailable(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
}

impl ToolError {
    /// The normative error code (SPEC.md §13).
    pub fn code(&self) -> &'static str {
        match self {
            ToolError::InvalidArguments(_) => "TOOL_INVALID_ARGUMENTS",
            ToolError::Timeout(_) => "TOOL_TIMEOUT",
            ToolError::Failed(_) => "TOOL_FAILED",
            ToolError::Unavailable(_) => "TOOL_UNAVAILABLE",
            ToolError::PermissionDenied(_) => "PERMISSION_DENIED",
        }
    }

    /// The structured `{ code, message }` shape for events and model feedback.
    pub fn to_json(&self) -> Value {
        serde_json::json!({ "code": self.code(), "message": self.to_string() })
    }

    /// A short human-readable label for logs.
    pub fn kind(&self) -> &'static str {
        match self {
            ToolError::InvalidArguments(_) => "invalid_arguments",
            ToolError::Timeout(_) => "timeout",
            ToolError::Failed(_) => "failed",
            ToolError::Unavailable(_) => "unavailable",
            ToolError::PermissionDenied(_) => "permission_denied",
        }
    }
}
