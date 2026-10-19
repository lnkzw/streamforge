//! Types of the `probe_composite` command.

use std::path::PathBuf;
use std::time::Duration;

use capture_api::{CaptureError, CaptureErrorKind, ScreenSelection, VideoCaptureReport};
use serde::Serialize;
use serde_json::Value;
use video_compositor::RenderStats;

/// Composite the reference scene for `duration` and report render
/// diagnostics. `duration` is validated by `Command::decode`; a
/// `snapshot_path` ends in `.ppm` and is never overwritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositeRequest {
    pub duration: Duration,
    /// Include a live screen capture layer (opens the system picker).
    pub screen: Option<ScreenSelection>,
    pub snapshot_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CompositeReport {
    pub render: RenderStats,
    pub screen_capture: Option<VideoCaptureReport>,
    /// Set when the screen layer was requested but capturing failed midway.
    pub screen_capture_error: Option<ErrorSummary>,
    pub snapshot: Option<SnapshotResult>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ErrorSummary {
    pub kind: CaptureErrorKind,
    pub message: String,
    pub details: Option<Value>,
}

impl From<CaptureError> for ErrorSummary {
    fn from(error: CaptureError) -> Self {
        Self {
            kind: error.kind,
            message: error.message,
            details: error.details,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapshotResult {
    pub path: String,
    pub written: bool,
    pub error: Option<String>,
}
