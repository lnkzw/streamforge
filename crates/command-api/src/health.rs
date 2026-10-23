//! Live health of a running engine (`get_health`, ADR 0012): what the final
//! reports count, while it happens.

use audio_engine::LiveMixStats;
use capture_api::CaptureState;
use encoder_api::{LiveOutputStats, StreamState, VideoEncoderInfo};
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
    pub recording: Option<OutputHealth>,
    pub streaming: Option<OutputHealth>,
    /// Every running capture: screen sources of the scenes, the legacy
    /// screen layer and live audio inputs. An `ended` capture delivers
    /// nothing more. Scene screen sources (those with an `id`) can be
    /// started again with `restart_source`; the others cannot.
    pub captures: Vec<CaptureHealth>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
    Screen,
    Audio,
}

impl CaptureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Screen => "screen",
            Self::Audio => "audio",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CaptureHealth {
    pub kind: CaptureKind,
    /// The scene source's `id`, as in `list_scenes` and `render.sources`,
    /// or the channel's, as in `get_mixer` (`None` for the screen layer).
    pub id: Option<String>,
    /// The scene source's or channel's name, or `screen layer`. A channel's
    /// device is in `get_mixer`.
    pub name: String,
    /// `{"state": "running"}` or `{"state": "ended", "reason": ...}`.
    #[serde(flatten)]
    pub state: CaptureState,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutputHealth {
    /// File path (recording) or redacted target (stream); never a key.
    pub target: String,
    /// Time since the output started.
    pub elapsed_ms: u64,
    /// The output and the pipeline it needs are running; `false` once
    /// either failed (the stop report says why).
    pub running: bool,
    /// Streams only: `connecting`, `connected`, `reconnecting` or `failed`.
    pub state: Option<StreamState>,
    pub video_encoder: VideoEncoderInfo,
    /// `frames_encoded`, `frames_dropped`, `samples_encoded`,
    /// `blocks_dropped`, `queue_depth`, `connects`, `reconnect_attempts`.
    #[serde(flatten)]
    pub counters: LiveOutputStats,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn capture_kind_names_match_the_wire() {
        for kind in [CaptureKind::Screen, CaptureKind::Audio] {
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
            captures: vec![CaptureHealth {
                kind: CaptureKind::Screen,
                id: Some("src_desk".to_owned()),
                name: "desk".to_owned(),
                state: CaptureState::Ended {
                    reason: "the captured window was closed".to_owned(),
                },
            }],
            streaming: Some(OutputHealth {
                target: "rtmp://example.com/live/<stream key>".to_owned(),
                elapsed_ms: 1500,
                running: true,
                state: Some(StreamState::Reconnecting),
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
                    connects: 1,
                    reconnect_attempts: 1,
                },
            }),
        };
        let value = serde_json::to_value(&health).expect("serialize");
        assert_eq!(value["recording"], json!(null));
        let stream = &value["streaming"];
        assert_eq!(stream["state"], "reconnecting");
        assert_eq!(stream["frames_encoded"], 90);
        assert_eq!(stream["queue_depth"], 2);
        assert_eq!(stream["video_encoder"]["encoder"], "libx264");
        assert!(stream.get("counters").is_none(), "{stream}");
        assert_eq!(
            value["captures"][0],
            json!({"kind": "screen", "id": "src_desk", "name": "desk", "state": "ended",
                   "reason": "the captured window was closed"})
        );
    }
}
