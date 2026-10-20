//! Types of the output commands: `start_recording`, `stop_recording`,
//! `start_streaming` and `stop_streaming`.
//!
//! Outputs share one media pipeline (compositor, mixer, captures). The first
//! output to start brings it up with its `sources`; later outputs join it
//! and must not name sources. The pipeline stops with the last output.

use std::path::PathBuf;

use audio_engine::MixStats;
use capture_api::{ScreenSelection, VideoCaptureReport};
use encoder_api::{ReconnectPolicy, RecordingStats, StreamStats, StreamTarget};
use serde::Serialize;
use video_compositor::RenderStats;

use crate::{CaptureOutcome, ErrorSummary, LiveInputSpec, ToneSpec};

/// What the media pipeline renders and mixes.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaSources {
    pub screen: Option<ScreenSelection>,
    pub inputs: Vec<LiveInputSpec>,
    pub tones: Vec<ToneSpec>,
    pub master_gain_db: f32,
}

/// Start recording to a new file. Validated by `Command::decode`:
/// `file_name` is a plain `.mkv` name created in the engine's output
/// directory and never overwritten.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordingRequest {
    pub file_name: PathBuf,
    /// `None`: join the running pipeline, or start it with the default
    /// sources (one quiet test tone).
    pub sources: Option<MediaSources>,
}

/// Start a live stream. The key lives in `target` and is never echoed.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamingRequest {
    pub target: StreamTarget,
    pub video_bitrate_kbps: u32,
    pub reconnect: ReconnectPolicy,
    pub sources: Option<MediaSources>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StreamingStarted {
    /// Redacted target.
    pub target: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub video_bitrate_kbps: u32,
    pub sample_rate: u32,
    pub channels: u32,
}

/// The pipeline part of an output's report. Its stats are present only if
/// the pipeline stopped together with this output (`pipeline_stopped`);
/// otherwise another output keeps it running.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct PipelineReport {
    pub pipeline_stopped: bool,
    pub render: Option<RenderStats>,
    pub mix: Option<MixStats>,
    pub screen_capture: Option<VideoCaptureReport>,
    pub screen_capture_error: Option<ErrorSummary>,
    pub audio_captures: Vec<CaptureOutcome>,
    /// Screen captures of scene sources during this run.
    pub scene_captures: Vec<SceneCaptureOutcome>,
}

/// How the screen capture of one scene source ended.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SceneCaptureOutcome {
    pub source: String,
    pub report: Option<VideoCaptureReport>,
    pub error: Option<ErrorSummary>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordingReport {
    pub recording: RecordingStats,
    #[serde(flatten)]
    pub pipeline: PipelineReport,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StreamingReport {
    pub streaming: StreamStats,
    #[serde(flatten)]
    pub pipeline: PipelineReport,
    pub warnings: Vec<String>,
}
