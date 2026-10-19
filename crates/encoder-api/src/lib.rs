//! Recording vocabulary: settings, statistics and errors.
//!
//! Free of FFmpeg so command and adapter crates can use these types without
//! linking an encoder; `encoder-ffmpeg` implements the recorder.

use std::path::PathBuf;

use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecorderConfig {
    /// Output file; the container is chosen from the extension (`.mkv` → Matroska).
    pub path: PathBuf,
    pub video: VideoSettings,
    pub audio: AudioSettings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Preferred encoder; any H.264 encoder is used if it is unavailable.
    pub encoder: String,
    /// Encoder options, e.g. `("preset", "veryfast")`.
    pub options: Vec<(String, String)>,
}

impl VideoSettings {
    /// libx264, `preset=veryfast`, `crf=20`: real-time at 1080p60 on a
    /// desktop CPU with good quality.
    pub fn h264(width: u32, height: u32, fps: u32) -> Self {
        Self {
            width,
            height,
            fps,
            encoder: "libx264".to_owned(),
            options: vec![
                ("preset".to_owned(), "veryfast".to_owned()),
                ("crf".to_owned(), "20".to_owned()),
            ],
        }
    }
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
    pub encoder: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub frames_encoded: u64,
    /// Frames not recorded: the input queue was full or the timestamp did
    /// not advance.
    pub frames_dropped: u64,
    pub first_pts_ms: Option<f64>,
    pub last_pts_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AudioTrackStats {
    pub encoder: String,
    pub rate: u32,
    pub channels: u32,
    pub bitrate: u64,
    pub samples_encoded: u64,
    /// Blocks not recorded because the input queue was full.
    pub blocks_dropped: u64,
    /// Silence inserted to keep the audio timeline continuous.
    pub gap_samples: u64,
    /// Repeated samples dropped from overlapping blocks.
    pub overlap_samples: u64,
    /// Gaps too long to fill; the timeline restarted.
    pub discontinuities: u64,
    pub first_pts_ms: Option<f64>,
    pub last_pts_ms: Option<f64>,
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
