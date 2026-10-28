//! Live health of a running engine (`get_health`, ADR 0012): what the final
//! reports count, while it happens.

use audio_engine::{LiveMixStats, PlaybackFeedStats};
use capture_api::{CaptureState, VideoFormatInfo};
use encoder_api::{LiveConnectionStats, LiveOutputStats, StreamState, VideoEncoderInfo};

use crate::RecordingState;
use serde::Serialize;
use video_compositor::LiveRenderStats;

/// Result of `get_health`. Counters are totals since the pipeline or the
/// output started; clients compute rates from two readings.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HealthReport {
    /// A media pipeline runs (for outputs or the preview).
    pub pipeline_running: bool,
    /// Compositor counters, refreshed every 250 ms; `None` without a
    /// pipeline. `sources[].last_frame_age_ms` shows stalled captures.
    pub render: Option<LiveRenderStats>,
    /// Mixer counters, refreshed every 250 ms; `None` without a pipeline.
    pub mix: Option<LiveMixStats>,
    pub recording: Option<OutputHealth<RecordingState>>,
    pub streaming: Option<OutputHealth<StreamState>>,
    /// The monitor, once it was switched on in this run.
    pub monitor: Option<MonitorHealth>,
    /// Every running capture: screen and camera sources of the scenes and
    /// live audio inputs. An `ended` capture delivers nothing more. Screen
    /// and camera sources can be started again with `restart_source`; audio
    /// inputs cannot.
    pub captures: Vec<CaptureHealth>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
    Screen,
    Audio,
    Camera,
}

impl CaptureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Screen => "screen",
            Self::Audio => "audio",
            Self::Camera => "camera",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CaptureHealth {
    pub kind: CaptureKind,
    /// The scene source's `id`, as in `list_scenes` and `render.sources`,
    /// or the channel's, as in `get_mixer`.
    pub id: String,
    /// The scene source's or channel's name. A channel's device is in
    /// `get_mixer`.
    pub name: String,
    /// `{"state": "running"}` or `{"state": "ended", "reason": ...}`.
    #[serde(flatten)]
    pub state: CaptureState,
    /// A camera's negotiated size, rate and pixel format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<VideoFormatInfo>,
}

/// The monitor's playback (ADR 0017).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MonitorHealth {
    /// The output device; `None`: the system default.
    pub device: Option<String>,
    /// Whether it plays its bus now; off, it plays silence.
    pub enabled: bool,
    /// `{"state": "running"}`, or `{"state": "ended", "reason": ...}` when
    /// the device ended or did not open (`could not start: ...`).
    #[serde(flatten)]
    pub state: CaptureState,
    /// Buffered audio plus the device's largest pull, the delay the feed
    /// adds; `None` if the device did not open.
    pub latency_ms: Option<f64>,
    /// The feed's buffering (`fill_frames`, `target_frames`, `drift_ppm`,
    /// `padded_frames`, `trimmed_frames`) and its ring's counters
    /// (`ring.underrun_frames`, `ring.overrun_frames`, ...), at 48 kHz;
    /// `None` if the device did not open.
    pub feed: Option<PlaybackFeedStats>,
}

/// A running output's health; `S` is its state: [`RecordingState`] or
/// [`StreamState`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutputHealth<S> {
    /// File path (recording) or redacted target (stream); never a key.
    pub target: String,
    /// Time since the output started.
    pub elapsed_ms: u64,
    /// The output and the pipeline it needs are running; `false` once
    /// either failed (the stop report says why).
    pub running: bool,
    pub state: S,
    pub video_encoder: VideoEncoderInfo,
    /// `frames_encoded`, `frames_dropped`, `samples_encoded`,
    /// `blocks_dropped`, `queue_depth`.
    #[serde(flatten)]
    pub counters: LiveOutputStats,
    /// Streams only: `connects`, `reconnect_attempts`.
    #[serde(flatten)]
    pub connection: Option<LiveConnectionStats>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn capture_kind_names_match_the_wire() {
        for kind in [CaptureKind::Screen, CaptureKind::Audio, CaptureKind::Camera] {
            assert_eq!(json!(kind), json!(kind.as_str()));
        }
    }

    #[test]
    fn output_counters_are_flat_fields_of_the_output() {
        let health = HealthReport {
            pipeline_running: true,
            render: None,
            mix: None,
            recording: None,
            monitor: None,
            captures: vec![CaptureHealth {
                kind: CaptureKind::Screen,
                id: "src_desk".to_owned(),
                name: "desk".to_owned(),
                state: CaptureState::Ended {
                    reason: "the captured window was closed".to_owned(),
                },
                format: None,
            }],
            streaming: Some(OutputHealth {
                target: "rtmp://example.com/live/<stream key>".to_owned(),
                elapsed_ms: 1500,
                running: true,
                state: StreamState::Reconnecting,
                video_encoder: VideoEncoderInfo {
                    encoder: "libx264".to_owned(),
                    hardware: false,
                    skipped: Vec::new(),
                },
                counters: LiveOutputStats {
                    frames_encoded: 90,
                    frames_dropped: 3,
                    samples_encoded: 72_704,
                    blocks_dropped: 1,
                    queue_depth: 2,
                },
                connection: Some(LiveConnectionStats {
                    connects: 1,
                    reconnect_attempts: 1,
                }),
            }),
        };
        let value = serde_json::to_value(&health).expect("serialize");
        assert_eq!(value["recording"], json!(null));
        let stream = &value["streaming"];
        assert_eq!(stream["state"], "reconnecting");
        assert_eq!(stream["frames_encoded"], 90);
        assert_eq!(stream["queue_depth"], 2);
        assert_eq!(stream["reconnect_attempts"], 1);
        assert_eq!(stream["video_encoder"]["encoder"], "libx264");
        assert!(stream.get("counters").is_none(), "{stream}");
        assert_eq!(
            value["captures"][0],
            json!({"kind": "screen", "id": "src_desk", "name": "desk", "state": "ended",
                   "reason": "the captured window was closed"})
        );
    }
}
