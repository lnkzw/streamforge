use std::fmt;

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
    /// The engine failed while executing the command.
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnknownCommand => "unknown_command",
            Self::InvalidParams => "invalid_params",
            Self::InvalidState => "invalid_state",
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
