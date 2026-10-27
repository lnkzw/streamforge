//! The parts of the engine's results the soak test reads, as a client sees
//! them on the wire (`docs/protocol.md`). Missing fields are errors, so a
//! protocol change shows up here; fields the test does not use are ignored.
//! The health parts are also written to the timeline as they are.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Health {
    pub render: Option<Render>,
    pub mix: Option<Mix>,
    pub recording: Option<OutputHealth>,
    pub streaming: Option<OutputHealth>,
    pub captures: Vec<Capture>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Render {
    pub frames_rendered: u64,
    pub skipped_ticks: u64,
    pub late_frames: u64,
    pub render_time: Option<Latency>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Latency {
    pub mean_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Mix {
    pub blocks_mixed: u64,
    pub late_blocks: u64,
    pub skipped_blocks: u64,
    pub inputs: Vec<MixInput>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct MixInput {
    pub name: String,
    /// `null` until the input has delivered enough to measure.
    pub fill_ms: Option<f64>,
    pub drift_ppm: Option<f64>,
    pub underrun_frames: u64,
    /// `null` for inputs without a ring (tones).
    pub overrun_frames: Option<u64>,
    pub trimmed_frames: u64,
}

/// `recording` or `streaming` in `get_health`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct OutputHealth {
    pub state: String,
    pub running: bool,
    pub frames_encoded: u64,
    pub frames_dropped: u64,
    pub blocks_dropped: u64,
    pub queue_depth: u64,
    /// Streams only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connects: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconnect_attempts: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Capture {
    pub kind: String,
    pub id: String,
    pub name: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The result of `stop_recording`.
#[derive(Debug, Clone, Deserialize)]
pub struct RecordingStop {
    pub recording: RecordingFile,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RecordingFile {
    pub video: VideoTrack,
    pub audio_tracks: Vec<AudioTrack>,
}

/// The result of `stop_streaming`.
#[derive(Debug, Clone, Deserialize)]
pub struct StreamingStop {
    pub streaming: StreamReport,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamReport {
    pub state: String,
    pub connects: u64,
    pub disconnects: u64,
    pub reconnect_attempts: u64,
    pub frames_dropped_offline: u64,
    pub blocks_dropped_offline: u64,
    pub frames_undelivered: u64,
    pub samples_undelivered: u64,
    pub last_error: Option<String>,
    pub video: VideoTrack,
    pub audio_tracks: Vec<AudioTrack>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct VideoTrack {
    pub encoder: String,
    pub hardware: bool,
    pub fps: Fraction,
    pub frames_encoded: u64,
    pub frames_dropped: u64,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct Fraction {
    pub num: u32,
    pub den: u32,
}

impl Fraction {
    pub fn value(self) -> f64 {
        f64::from(self.num) / f64::from(self.den.max(1))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AudioTrack {
    pub samples_encoded: u64,
    pub blocks_dropped: u64,
    pub gap_samples: u64,
    /// Samples that arrived again for a time already written, and were
    /// dropped.
    pub overlap_samples: u64,
    pub discontinuities: u64,
}
