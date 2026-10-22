//! Types of the `probe_audio_mix` command.

use std::time::Duration;

use audio_engine::MixStats;
use capture_api::AudioCaptureReport;
use serde::Serialize;

use crate::ErrorSummary;

/// Longest accepted mix probe; long enough for the 10-minute latency test.
pub const MAX_MIX_DURATION: Duration = Duration::from_secs(15 * 60);

/// Mix live inputs and test tones for `duration` and report meters,
/// buffering and timing. Validated by `Command::decode`: 1 to
/// `audio_engine::MAX_INPUTS` sources (a 1 kHz test tone when none are
/// given), gains within the mixer limits.
#[derive(Debug, Clone, PartialEq)]
pub struct MixRequest {
    pub duration: Duration,
    pub inputs: Vec<LiveInputSpec>,
    pub tones: Vec<ToneSpec>,
    pub master_gain_db: f32,
}

/// Serialized as the `inputs` entries of the mixing commands' parameters.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LiveInputSpec {
    /// Input name; `None` uses the system default input.
    pub input: Option<String>,
    pub gain_db: f32,
    #[serde(rename = "mute")]
    pub muted: bool,
}

/// Serialized as the `tones` entries of the mixing commands' parameters.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToneSpec {
    pub frequency_hz: f64,
    pub level_dbfs: f64,
    pub gain_db: f32,
    #[serde(rename = "mute")]
    pub muted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MixReport {
    pub mix: MixStats,
    /// One entry per live input, in request order.
    pub captures: Vec<CaptureOutcome>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CaptureOutcome {
    pub input: Option<String>,
    pub report: Option<AudioCaptureReport>,
    pub error: Option<ErrorSummary>,
}
