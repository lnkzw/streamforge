//! Types of the `start_recording` and `stop_recording` commands.

use std::path::PathBuf;

use audio_engine::MixStats;
use capture_api::{ScreenSelection, VideoCaptureReport};
use encoder_api::RecordingStats;
use serde::Serialize;
use video_compositor::RenderStats;

use crate::{CaptureOutcome, ErrorSummary, LiveInputSpec, ToneSpec};

/// Start recording the reference scene (and optional live sources) to a new
/// file. Validated by `Command::decode`: `file_name` is a plain `.mkv` name
/// created in the engine's output directory and never overwritten.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordingRequest {
    pub file_name: PathBuf,
    pub screen: Option<ScreenSelection>,
    pub inputs: Vec<LiveInputSpec>,
    pub tones: Vec<ToneSpec>,
    pub master_gain_db: f32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordingStarted {
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub sample_rate: u32,
    pub channels: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordingReport {
    pub recording: RecordingStats,
    pub render: Option<RenderStats>,
    pub mix: Option<MixStats>,
    pub screen_capture: Option<VideoCaptureReport>,
    pub screen_capture_error: Option<ErrorSummary>,
    pub audio_captures: Vec<CaptureOutcome>,
    pub warnings: Vec<String>,
}
