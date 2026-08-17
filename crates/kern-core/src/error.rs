//! Structured error taxonomy (SPEC.md §13).
//!
//! Every failure in Kern is typed and carries a stable, machine-readable code.
//! These codes appear verbatim in API responses, event payloads, and logs.
//! A failure is never swallowed: every `Err` path either retries (bounded),
//! surfaces as a structured event, or transitions the agent to `failed`.

use serde::Serialize;

/// Stable, machine-readable error codes (SPEC.md §13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    ConfigInvalid,
    AgentNotFound,
    AgentNameTaken,
    InvalidTransition,
    ExecutionAlreadyActive,
    ExecutionNotFound,
    CheckpointNotFound,
    ModelTimeout,
    ModelUnavailable,
    ModelAuth,
    ModelRateLimited,
    ModelInvalidResponse,
    ModelBudgetExhausted,
    ToolInvalidArguments,
    ToolTimeout,
    ToolFailed,
    ToolUnavailable,
    StepLimitExceeded,
    RunDurationExceeded,
    ToolCallLimitExceeded,
    RunnerPanic,
    /// The supervisor sweep found an execution whose runner is gone
    /// (runner-liveness supervision, ARCHITECTURE.md §25).
    RunnerLost,
    PermissionDenied,
    PermissionRequestNotFound,
    PermissionRequestAlreadyDecided,
    PermissionRequestExpired,
    SandboxUnavailable,
    SandboxFailure,
    CheckpointFormatUnsupported,
    CheckpointCorrupt,
    StorageCorruption,
    StorageMigration,
    StorageLocked,
    StorageFailure,
    Internal,
}

impl ErrorCode {
    /// The canonical code string (e.g. `"AGENT_NOT_FOUND"`).
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::ConfigInvalid => "CONFIG_INVALID",
            ErrorCode::AgentNotFound => "AGENT_NOT_FOUND",
            ErrorCode::AgentNameTaken => "AGENT_NAME_TAKEN",
            ErrorCode::InvalidTransition => "INVALID_TRANSITION",
            ErrorCode::ExecutionAlreadyActive => "EXECUTION_ALREADY_ACTIVE",
            ErrorCode::ExecutionNotFound => "EXECUTION_NOT_FOUND",
            ErrorCode::CheckpointNotFound => "CHECKPOINT_NOT_FOUND",
            ErrorCode::ModelTimeout => "MODEL_TIMEOUT",
            ErrorCode::ModelUnavailable => "MODEL_UNAVAILABLE",
            ErrorCode::ModelAuth => "MODEL_AUTH",
            ErrorCode::ModelRateLimited => "MODEL_RATE_LIMITED",
            ErrorCode::ModelInvalidResponse => "MODEL_INVALID_RESPONSE",
            ErrorCode::ModelBudgetExhausted => "MODEL_BUDGET_EXHAUSTED",
            ErrorCode::ToolInvalidArguments => "TOOL_INVALID_ARGUMENTS",
            ErrorCode::ToolTimeout => "TOOL_TIMEOUT",
            ErrorCode::ToolFailed => "TOOL_FAILED",
            ErrorCode::ToolUnavailable => "TOOL_UNAVAILABLE",
            ErrorCode::StepLimitExceeded => "STEP_LIMIT_EXCEEDED",
            ErrorCode::RunDurationExceeded => "RUN_DURATION_EXCEEDED",
            ErrorCode::ToolCallLimitExceeded => "TOOL_CALL_LIMIT_EXCEEDED",
            ErrorCode::RunnerPanic => "RUNNER_PANIC",
            ErrorCode::RunnerLost => "RUNNER_LOST",
            ErrorCode::PermissionDenied => "PERMISSION_DENIED",
            ErrorCode::PermissionRequestNotFound => "PERMISSION_REQUEST_NOT_FOUND",
            ErrorCode::PermissionRequestAlreadyDecided => "PERMISSION_REQUEST_ALREADY_DECIDED",
            ErrorCode::PermissionRequestExpired => "PERMISSION_REQUEST_EXPIRED",
            ErrorCode::SandboxUnavailable => "SANDBOX_UNAVAILABLE",
            ErrorCode::SandboxFailure => "SANDBOX_FAILURE",
            ErrorCode::CheckpointFormatUnsupported => "CHECKPOINT_FORMAT_UNSUPPORTED",
            ErrorCode::CheckpointCorrupt => "CHECKPOINT_CORRUPT",
            ErrorCode::StorageCorruption => "STORAGE_CORRUPTION",
            ErrorCode::StorageMigration => "STORAGE_MIGRATION",
            ErrorCode::StorageLocked => "STORAGE_LOCKED",
            ErrorCode::StorageFailure => "STORAGE_FAILURE",
            ErrorCode::Internal => "INTERNAL",
        }
    }
}

/// A structured Kern error: code + human message + optional machine detail.
#[derive(Debug, Clone, Serialize)]
pub struct KernError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

impl KernError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }

    pub fn code(&self) -> ErrorCode {
        self.code
    }

    /// Convenience constructor for internal/unexpected failures.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }

    /// Convenience constructor for configuration failures.
    pub fn config(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::ConfigInvalid, message)
    }
}

impl std::fmt::Display for KernError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.detail {
            Some(detail) => {
                write!(f, "{}: {} ({detail})", self.code.as_str(), self.message)
            }
            None => write!(f, "{}: {}", self.code.as_str(), self.message),
        }
    }
}

impl std::error::Error for KernError {}

/// Result alias used across the runtime.
pub type Result<T> = std::result::Result<T, KernError>;

#[cfg(test)]
mod tests {
    use super::*;

    fn all_codes() -> Vec<ErrorCode> {
        vec![
            ErrorCode::ConfigInvalid,
            ErrorCode::AgentNotFound,
            ErrorCode::AgentNameTaken,
            ErrorCode::InvalidTransition,
            ErrorCode::ExecutionAlreadyActive,
            ErrorCode::ExecutionNotFound,
            ErrorCode::CheckpointNotFound,
            ErrorCode::ModelTimeout,
            ErrorCode::ModelUnavailable,
            ErrorCode::ModelAuth,
            ErrorCode::ModelRateLimited,
            ErrorCode::ModelInvalidResponse,
            ErrorCode::ModelBudgetExhausted,
            ErrorCode::ToolInvalidArguments,
            ErrorCode::ToolTimeout,
            ErrorCode::ToolFailed,
            ErrorCode::ToolUnavailable,
            ErrorCode::StepLimitExceeded,
            ErrorCode::RunDurationExceeded,
            ErrorCode::ToolCallLimitExceeded,
            ErrorCode::RunnerPanic,
            ErrorCode::RunnerLost,
            ErrorCode::PermissionDenied,
            ErrorCode::PermissionRequestNotFound,
            ErrorCode::PermissionRequestAlreadyDecided,
            ErrorCode::PermissionRequestExpired,
            ErrorCode::SandboxUnavailable,
            ErrorCode::SandboxFailure,
            ErrorCode::CheckpointFormatUnsupported,
            ErrorCode::CheckpointCorrupt,
            ErrorCode::StorageCorruption,
            ErrorCode::StorageMigration,
            ErrorCode::StorageLocked,
            ErrorCode::StorageFailure,
            ErrorCode::Internal,
        ]
    }

    #[test]
    fn serializes_to_spec_shape() {
        let err = KernError::new(ErrorCode::AgentNotFound, "no such agent")
            .with_detail(serde_json::json!({ "agent_id": "abc" }));
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["code"], "AGENT_NOT_FOUND");
        assert_eq!(json["message"], "no such agent");
        assert_eq!(json["detail"]["agent_id"], "abc");
    }

    #[test]
    fn omits_detail_when_absent() {
        let err = KernError::new(ErrorCode::ConfigInvalid, "bad config");
        let json = serde_json::to_value(&err).unwrap();
        assert!(json.get("detail").is_none());
    }

    #[test]
    fn serde_code_matches_as_str() {
        // Guards against drift between the serde rename and `as_str()`.
        for code in all_codes() {
            let serialized = serde_json::to_string(&code).unwrap();
            let without_quotes = serialized.trim_matches('"');
            assert_eq!(
                without_quotes,
                code.as_str(),
                "serde name and as_str() disagree for {code:?}"
            );
        }
    }

    #[test]
    fn code_strings_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for code in all_codes() {
            assert!(
                seen.insert(code.as_str()),
                "duplicate code: {}",
                code.as_str()
            );
        }
    }

    #[test]
    fn display_includes_code() {
        let err = KernError::new(ErrorCode::PermissionDenied, "not allowed");
        let s = err.to_string();
        assert!(s.contains("PERMISSION_DENIED"));
        assert!(s.contains("not allowed"));
    }

    #[test]
    fn is_std_error_compatible() {
        let err = KernError::new(ErrorCode::Internal, "boom");
        let boxed: Box<dyn std::error::Error> = Box::new(err);
        assert!(!boxed.to_string().is_empty());
    }
}
