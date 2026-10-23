//! Types of the output commands: `start_recording`, `stop_recording`,
//! `start_streaming` and `stop_streaming`.
//!
//! Outputs share one media pipeline (compositor, mixer, captures). The first
//! output to start brings it up with its `sources`; later outputs join it
//! and must not name sources. The pipeline stops with the last output.
//!
//! Each output has a profile (ADR 0017): the bus it takes as its mix and its
//! encoder settings. A start uses its profile; the start's own parameters
//! override it for that run.

use std::path::PathBuf;

use audio_engine::MixStats;
use capture_api::{ScreenSelection, VideoCaptureReport};
use encoder_api::{
    EncoderPreference, ReconnectPolicy, RecordingStats, StreamStats, StreamTarget, VideoEncoderInfo,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use video_compositor::RenderStats;

use crate::{BusRef, CaptureOutcome, ErrorSummary, LiveInputSpec, PROGRAM_BUS_ID, ToneSpec};

/// A stream's video bitrate when nothing else says.
pub const DEFAULT_VIDEO_BITRATE_KBPS: u32 = 6_000;

/// An output that has a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    Recording,
    Stream,
}

impl OutputKind {
    pub const ALL: [Self; 2] = [Self::Recording, Self::Stream];

    /// Its name in commands and files.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recording => "recording",
            Self::Stream => "stream",
        }
    }
}

/// Most audio tracks after the first (the mix) in a recording.
pub const MAX_EXTRA_TRACKS: usize = encoder_api::MAX_AUDIO_TRACKS - 1;

/// How the recording starts.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordingProfile {
    /// The id of the bus the recording takes as its mix: audio track 1.
    pub mix: String,
    /// The ids of the buses recorded as further audio tracks, in order;
    /// each track is named after its bus. Fixed when a recording starts.
    pub tracks: Vec<String>,
    pub video_encoder: EncoderPreference,
}

/// How the stream starts. Its target is not part of it (ADR 0017): the URL
/// is a `start_streaming` parameter, the key never stored.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StreamProfile {
    /// The id of the bus the stream takes as its mix.
    pub mix: String,
    pub video_bitrate_kbps: u32,
    pub video_encoder: EncoderPreference,
}

/// Every output's profile. Result of `get_output_profiles`, and a project
/// file's `outputs`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutputProfiles {
    pub recording: RecordingProfile,
    pub stream: StreamProfile,
}

impl Default for OutputProfiles {
    /// Both outputs take the program bus, with the default encoders.
    fn default() -> Self {
        Self {
            recording: RecordingProfile {
                mix: PROGRAM_BUS_ID.to_owned(),
                tracks: Vec::new(),
                video_encoder: EncoderPreference::default(),
            },
            stream: StreamProfile {
                mix: PROGRAM_BUS_ID.to_owned(),
                video_bitrate_kbps: DEFAULT_VIDEO_BITRATE_KBPS,
                video_encoder: EncoderPreference::default(),
            },
        }
    }
}

impl OutputProfiles {
    /// The bus id `output` takes as its mix.
    pub fn mix(&self, output: OutputKind) -> &str {
        match output {
            OutputKind::Recording => &self.recording.mix,
            OutputKind::Stream => &self.stream.mix,
        }
    }

    pub fn mix_mut(&mut self, output: OutputKind) -> &mut String {
        match output {
            OutputKind::Recording => &mut self.recording.mix,
            OutputKind::Stream => &mut self.stream.mix,
        }
    }

    pub fn video_encoder_mut(&mut self, output: OutputKind) -> &mut EncoderPreference {
        match output {
            OutputKind::Recording => &mut self.recording.video_encoder,
            OutputKind::Stream => &mut self.stream.video_encoder,
        }
    }
}

/// Change an output's profile; at least one field is given, and the
/// bitrate only for the stream.
#[derive(Debug, Clone, PartialEq)]
pub struct SetOutputProfileRequest {
    pub output: OutputKind,
    /// The bus to take as the mix.
    pub mix: Option<BusRef>,
    /// The recording's further audio tracks (replacing the list).
    pub tracks: Option<Vec<BusRef>>,
    pub video_encoder: Option<EncoderPreference>,
    pub video_bitrate_kbps: Option<u32>,
}

/// Result of `set_output_profile`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutputProfileSet {
    #[serde(flatten)]
    pub profiles: OutputProfiles,
    /// The fields that changed the running output now (the mix: the output
    /// fades over to the new bus).
    pub applied_now: Vec<&'static str>,
    /// The fields the running output takes at its next start (encoder
    /// settings).
    pub next_start: Vec<&'static str>,
}

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
    /// Which H.264 encoders to try (ADR 0011); `None`: the profile's.
    pub video_encoder: Option<EncoderPreference>,
    /// `None`: join the running pipeline, or start it with the default
    /// sources (one quiet test tone).
    pub sources: Option<MediaSources>,
}

/// Start a live stream. The key lives in `target` and is never echoed.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamingRequest {
    pub target: StreamTarget,
    /// `None`: the profile's.
    pub video_bitrate_kbps: Option<u32>,
    pub video_encoder: Option<EncoderPreference>,
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
    pub video_encoder: VideoEncoderInfo,
    /// E.g. a channel that is silent because its device did not open.
    pub warnings: Vec<String>,
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
    pub video_encoder: VideoEncoderInfo,
    /// E.g. a channel that is silent because its device did not open.
    pub warnings: Vec<String>,
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
    /// Screen captures of scene sources during this run (the most recent
    /// 64; `scene_captures_omitted` counts older ones).
    pub scene_captures: Vec<SceneCaptureOutcome>,
    pub scene_captures_omitted: u64,
}

/// How the screen capture of one scene source ended.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SceneCaptureOutcome {
    /// The scene source's id (as in `list_scenes`) and name.
    pub id: String,
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
