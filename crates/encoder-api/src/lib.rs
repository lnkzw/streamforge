//! Output vocabulary: recording and streaming settings, statistics, errors.
//!
//! Free of FFmpeg so command and adapter crates can use these types without
//! linking an encoder; `encoder-ffmpeg` implements the outputs.

mod stream;

use std::path::PathBuf;

pub use stream::{
    ReconnectPolicy, StreamConfig, StreamKey, StreamState, StreamStats, StreamTarget,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecorderConfig {
    /// Output file; the container is chosen from the extension (`.mkv` → Matroska).
    pub path: PathBuf,
    pub video: VideoSettings,
    pub audio: AudioSettings,
}

/// Which H.264 encoders an output tries (ADR 0011).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum EncoderPreference {
    /// A hardware encoder if one opens on this machine, else software.
    #[default]
    Auto,
    /// Software only (libx264).
    Software,
    /// Hardware only; the output fails if no hardware encoder opens.
    Hardware,
}

impl EncoderPreference {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Software => "software",
            Self::Hardware => "hardware",
        }
    }
}

/// What the video encoder aims for; each encoder maps it to its own options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateControl {
    /// Constant quality for files (libx264 CRF 20 or the hardware
    /// encoder's comparable constant-quality mode).
    Quality,
    /// Constant bit rate with a keyframe every `keyframe_interval_s`
    /// seconds, as live ingests expect.
    ConstantBitrate { kbps: u32, keyframe_interval_s: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub encoders: EncoderPreference,
    pub rate: RateControl,
}

impl VideoSettings {
    /// H.264 at constant quality for recordings, hardware where available.
    pub fn h264(width: u32, height: u32, fps: u32) -> Self {
        Self {
            width,
            height,
            fps,
            encoders: EncoderPreference::Auto,
            rate: RateControl::Quality,
        }
    }

    pub fn with_encoders(mut self, encoders: EncoderPreference) -> Self {
        self.encoders = encoders;
        self
    }
}

/// The video encoder an output opened, known once it has started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VideoEncoderInfo {
    pub encoder: String,
    /// The encoder runs on a GPU or media engine.
    pub hardware: bool,
    /// Encoders tried before `encoder`, in order (empty when the first one
    /// worked).
    pub skipped: Vec<SkippedEncoder>,
}

/// An encoder that was tried and not used, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedEncoder {
    pub encoder: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSettings {
    pub rate: u32,
    pub channels: u32,
    pub bitrate: u64,
}

impl AudioSettings {
    /// AAC at 160 kbit/s.
    pub fn aac(rate: u32, channels: u32) -> Self {
        Self {
            rate,
            channels,
            bitrate: 160_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VideoTrackStats {
    /// The encoder of the (last) session and the candidates tried before
    /// it; serialized as `encoder`, `hardware` and `skipped`.
    #[serde(flatten)]
    pub encoder: VideoEncoderInfo,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub frames_encoded: u64,
    /// Frames not recorded: no free copy buffer, the input queue was full,
    /// the recorder had failed, or the timestamp was invalid or did not
    /// advance.
    pub frames_dropped: u64,
    /// Frames stamped at or after the requested end; not part of the file.
    pub frames_after_end: u64,
    /// Start of the first frame.
    pub first_pts_ms: Option<f64>,
    /// End of the last frame (its timestamp plus one frame duration).
    pub end_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AudioTrackStats {
    pub encoder: String,
    pub rate: u32,
    pub channels: u32,
    pub bitrate: u64,
    pub samples_encoded: u64,
    /// Blocks not recorded: the input queue was full, the recorder had
    /// failed, or the block was invalid (too large, bad timestamp).
    pub blocks_dropped: u64,
    /// Blocks stamped at or after the requested end; not part of the file.
    pub blocks_after_end: u64,
    /// Silence inserted to keep the audio timeline continuous.
    pub gap_samples: u64,
    /// Repeated samples dropped from overlapping blocks.
    pub overlap_samples: u64,
    /// Gaps too long to fill; the timeline restarted.
    pub discontinuities: u64,
    /// Silence appended to complete the last encoder frame before the end
    /// and before each discontinuity.
    pub padding_samples: u64,
    /// Start of the first sample.
    pub first_pts_ms: Option<f64>,
    /// End of the last recorded sample (padding excluded).
    pub end_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordingStats {
    pub path: String,
    pub container: String,
    /// End of the last encoded frame or sample.
    pub duration_ms: f64,
    pub file_bytes: Option<u64>,
    pub video: VideoTrackStats,
    pub audio: AudioTrackStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderErrorKind {
    /// Recording is not available on this platform or no encoder exists.
    Unsupported,
    /// The output file or container could not be created or finalized.
    Output,
    /// A streaming server could not be reached or the connection failed.
    Network,
    /// An encoder failed.
    Encoder,
    /// The recorder thread failed unexpectedly.
    Internal,
}

#[derive(Debug, Clone, PartialEq, Error)]
#[error("{message}")]
pub struct RecorderError {
    pub kind: RecorderErrorKind,
    pub message: String,
    /// Statistics up to the failure; the file was still finalized when present.
    pub partial: Option<Box<RecordingStats>>,
}

impl RecorderError {
    pub fn new(kind: RecorderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            partial: None,
        }
    }
}
