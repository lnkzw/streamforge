//! Types of the `probe_composite` command.

use std::path::PathBuf;
use std::time::Duration;

use capture_api::{ScreenSelection, VideoCaptureReport};
use serde::Serialize;
use video_compositor::RenderStats;

use crate::ErrorSummary;

/// Composite the reference scene for `duration` and report render
/// diagnostics. `duration` is validated by `Command::decode`; a
/// `snapshot_file_name` is a plain `.ppm` file name inside the engine's output
/// directory and is never overwritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositeRequest {
    pub duration: Duration,
    /// Include a live screen capture layer (opens the system picker).
    pub screen: Option<ScreenSelection>,
    pub snapshot_file_name: Option<PathBuf>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapshotResult {
    /// Full path of the file in the engine's output directory.
    pub path: String,
    pub written: bool,
    /// Why it was not written: the file exists (`invalid_params`), the
    /// directory is missing (`not_found`) or not writable
    /// (`permission_denied`), or writing failed (`backend_error`).
    pub error: Option<ErrorSummary>,
}
