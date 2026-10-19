use serde::Serialize;
use serde_json::{Map, Value};
use thiserror::Error;

/// Category of a [`CaptureError`]; adapters map it to their own error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureErrorKind {
    /// The platform or backend does not offer the requested capability.
    Unsupported,
    /// A required system service (portal, PipeWire, ...) is missing or not running.
    Unavailable,
    /// The user dismissed the selection dialog.
    Cancelled,
    /// The system refused access.
    PermissionDenied,
    /// The requested device or source does not exist.
    NotFound,
    /// A system service did not answer in time.
    Timeout,
    /// The request is malformed.
    InvalidRequest,
    /// The backend failed while capturing.
    Backend,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct CaptureError {
    pub kind: CaptureErrorKind,
    /// Human-readable description including a remedy where one is known.
    pub message: String,
    /// Optional machine-readable context, e.g. the available alternatives.
    pub details: Option<Value>,
}

impl CaptureError {
    pub fn new(kind: CaptureErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// Add one entry to the details object, keeping existing details.
    /// Non-object details are preserved under `"details"`.
    pub fn with_detail(mut self, key: &str, value: impl Into<Value>) -> Self {
        let mut map = match self.details.take() {
            Some(Value::Object(map)) => map,
            Some(other) => Map::from_iter([("details".to_owned(), other)]),
            None => Map::new(),
        };
        map.insert(key.to_owned(), value.into());
        self.details = Some(Value::Object(map));
        self
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(CaptureErrorKind::Unsupported, message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(CaptureErrorKind::Unavailable, message)
    }

    pub fn backend(message: impl Into<String>) -> Self {
        Self::new(CaptureErrorKind::Backend, message)
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(CaptureErrorKind::Timeout, message)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn with_detail_merges_into_existing_details() {
        let err = CaptureError::backend("x")
            .with_details(json!({"a": 1}))
            .with_detail("b", 2);
        assert_eq!(err.details, Some(json!({"a": 1, "b": 2})));
    }

    #[test]
    fn with_detail_keeps_non_object_details() {
        let err = CaptureError::backend("x")
            .with_details(json!([1]))
            .with_detail("warnings", vec!["w".to_owned()]);
        assert_eq!(
            err.details,
            Some(json!({"details": [1], "warnings": ["w"]}))
        );
        let fresh = CaptureError::backend("y").with_detail("k", "v");
        assert_eq!(fresh.details, Some(json!({"k": "v"})));
    }
}
