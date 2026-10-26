use std::fmt;

use capture_api::{CaptureError, CaptureErrorKind};
use serde::{Serialize, Serializer};
use serde_json::Value;
use thiserror::Error;

/// Machine-readable category of a [`CommandError`].
///
/// Adapters map these to their own wire codes; this enum is not serialized
/// directly so that renaming a variant cannot silently change a protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// No command with the requested name exists.
    UnknownCommand,
    /// The command exists but its parameters are malformed.
    InvalidParams,
    /// The command is valid but not allowed in the current engine state.
    InvalidState,
    /// The platform or backend does not offer the requested capability.
    Unsupported,
    /// A required system service is missing or not running.
    Unavailable,
    /// The user cancelled an interactive step, e.g. a selection dialog.
    Cancelled,
    /// The system refused access.
    PermissionDenied,
    /// A referenced device or object does not exist.
    NotFound,
    /// A system service did not answer in time.
    Timeout,
    /// A media backend (capture, encoder, ...) failed.
    BackendError,
    /// The engine failed while executing the command.
    Internal,
}

impl ErrorCode {
    pub const ALL: [Self; 11] = [
        Self::UnknownCommand,
        Self::InvalidParams,
        Self::InvalidState,
        Self::Unsupported,
        Self::Unavailable,
        Self::Cancelled,
        Self::PermissionDenied,
        Self::NotFound,
        Self::Timeout,
        Self::BackendError,
        Self::Internal,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnknownCommand => "unknown_command",
            Self::InvalidParams => "invalid_params",
            Self::InvalidState => "invalid_state",
            Self::Unsupported => "unsupported",
            Self::Unavailable => "unavailable",
            Self::Cancelled => "cancelled",
            Self::PermissionDenied => "permission_denied",
            Self::NotFound => "not_found",
            Self::Timeout => "timeout",
            Self::BackendError => "backend_error",
            Self::Internal => "internal",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Structured error returned by command decoding and command handlers.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{code}: {message}")]
pub struct CommandError {
    pub code: ErrorCode,
    /// Human-readable description; not intended for matching.
    pub message: String,
    /// Optional machine-readable context, e.g. offending parameter names.
    pub details: Option<Value>,
}

impl CommandError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

impl From<CaptureError> for CommandError {
    fn from(error: CaptureError) -> Self {
        let code = match error.kind {
            CaptureErrorKind::Unsupported => ErrorCode::Unsupported,
            CaptureErrorKind::Unavailable => ErrorCode::Unavailable,
            CaptureErrorKind::Cancelled => ErrorCode::Cancelled,
            CaptureErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
            CaptureErrorKind::NotFound => ErrorCode::NotFound,
            CaptureErrorKind::Timeout => ErrorCode::Timeout,
            CaptureErrorKind::InvalidRequest => ErrorCode::InvalidParams,
            CaptureErrorKind::Backend => ErrorCode::BackendError,
        };
        Self {
            code,
            message: error.message,
            details: error.details,
        }
    }
}

/// An error inside a report (a capture that failed, a snapshot that was not
/// written), in a command error's shape: `{code, message, details}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErrorSummary {
    #[serde(serialize_with = "code_name")]
    pub code: ErrorCode,
    pub message: String,
    pub details: Option<Value>,
}

impl From<CommandError> for ErrorSummary {
    fn from(error: CommandError) -> Self {
        Self {
            code: error.code,
            message: error.message,
            details: error.details,
        }
    }
}

impl From<CaptureError> for ErrorSummary {
    fn from(error: CaptureError) -> Self {
        CommandError::from(error).into()
    }
}

/// Codes are written by their wire name (see [`ErrorCode`]).
fn code_name<S: Serializer>(code: &ErrorCode, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(code.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn capture_error_kinds_map_to_distinct_codes() {
        let kinds = [
            (CaptureErrorKind::Unsupported, ErrorCode::Unsupported),
            (CaptureErrorKind::Unavailable, ErrorCode::Unavailable),
            (CaptureErrorKind::Cancelled, ErrorCode::Cancelled),
            (
                CaptureErrorKind::PermissionDenied,
                ErrorCode::PermissionDenied,
            ),
            (CaptureErrorKind::NotFound, ErrorCode::NotFound),
            (CaptureErrorKind::Timeout, ErrorCode::Timeout),
            (CaptureErrorKind::InvalidRequest, ErrorCode::InvalidParams),
            (CaptureErrorKind::Backend, ErrorCode::BackendError),
        ];
        for (kind, code) in kinds {
            assert_eq!(CommandError::from(CaptureError::new(kind, "x")).code, code);
        }
    }

    #[test]
    fn errors_in_reports_have_the_command_error_shape() {
        let error = CaptureError::new(CaptureErrorKind::NotFound, "the window is gone")
            .with_details(json!({"capture_target_id": "window:7"}));
        assert_eq!(
            json!(ErrorSummary::from(error)),
            json!({"code": "not_found", "message": "the window is gone",
                   "details": {"capture_target_id": "window:7"}})
        );
    }
}
