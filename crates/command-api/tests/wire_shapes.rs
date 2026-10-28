//! The freeze guard (ADR 0018, point 8): the field names and JSON types of
//! every command's result, pinned in `wire_shapes.txt`.
//!
//! Results serialize types from `audio-engine`, `encoder-api`,
//! `capture-api` and `video-compositor`, so renaming an internal field
//! changes the protocol. Each sample below fills every field (options with
//! a value, lists with an element), so its serialized paths are the whole
//! shape. A change to the pinned file is a protocol change: from 0.1.0 on,
//! v1 only gains fields (`docs/protocol.md`, Versioning policy).
//!
//! After an intended change, rewrite the file and review its diff:
//! `UPDATE_WIRE_SHAPES=1 cargo test -p command-api --test wire_shapes`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use audio_engine::{
    BufferLatency, BusStats, InputStats, LiveBusStats, LiveInputStats, LiveMixStats, MeterReading,
    MixStats, PlaybackFeedStats,
};
use capture_api::{
    AudioCaptureCapability, AudioCaptureReport, AudioFormatInfo, AudioInput, AudioOutput,
    AudioStats, Camera, CameraCaptureCapability, CameraMode, CaptureCapabilities, CaptureError,
    CaptureErrorKind, CaptureState, CursorMode, IntervalStats, Rational, ScreenCaptureCapability,
    ScreenSource, VideoCaptureReport, VideoFormatInfo, VideoSourceInfo, VideoSourceKind,
    VideoStats,
};
use command_api::{
    AudioInputList, AudioOutputList, BusState, CameraList, CameraModeSpec, CaptureHealth,
    CaptureKind, CaptureOutcome, CaptureTargetList, ChannelInput, ChannelSpec, ChannelState,
    CommandAccess, CommandList, CommandOutput, CompositeReport, EngineStatus, ErrorCode,
    ErrorSummary, HealthReport, ImageFileInfo, ImageFileList, MixReport, MixerState, MonitorHealth,
    MonitorProfile, OutputHealth, OutputKind, OutputMix, OutputProfileSet, OutputProfiles,
    PipelineReport, PreviewState, ProjectFileInfo, ProjectFileList, ProjectLoaded, ProjectProblem,
    ProjectSaved, ProjectValidation, RecordingProfile, RecordingReport, RecordingStarted,
    RecordingState, Rgba, SceneCaptureOutcome, SceneList, SceneSpec, SendSpec, SendTap,
    SnapshotResult, SourceKind, SourceRect, SourceSpec, StopFailure, StreamProfile,
    StreamingReport, StreamingStarted, registry,
};
use encoder_api::{
    AudioTrackStats, EncoderPreference, LiveConnectionStats, LiveOutputStats, RecordingStats,
    SkippedEncoder, StreamState, StreamStats, VideoEncoderInfo, VideoTrackStats,
};
use media_core::{AudioRingStats, LatencySummary, MailboxStats, MediaTimestamp, SampleRingStats};
use serde::Serialize;
use serde_json::{Value, json};
use video_compositor::{LiveRenderStats, LiveSourceStats, RenderStats, SourceId, SourceStats};

/// Error details that carry results, pinned like the results.
const RECORDING_FAILURE: &str = "stop_recording: error details";
const STREAM_FAILURE: &str = "stop_streaming: error details";
/// The wire names of the enums in results.
const ENUMS: &str = "enum values";

#[test]
fn every_result_keeps_its_wire_shape() {
    let mut covered = BTreeSet::new();
    let mut sections = BTreeMap::new();
    for (commands, sample) in samples() {
        for command in commands {
            assert!(covered.insert(*command), "`{command}` has two samples");
        }
        sections.insert(commands.join(", "), render(&sample));
    }
    let commands: BTreeSet<&str> = registry().iter().map(|command| command.name).collect();
    let missing: Vec<_> = commands.difference(&covered).collect();
    let unknown: Vec<_> = covered.difference(&commands).collect();
    assert!(missing.is_empty(), "commands without a sample: {missing:?}");
    assert!(
        unknown.is_empty(),
        "samples for unknown commands: {unknown:?}"
    );
    sections.insert(
        RECORDING_FAILURE.to_owned(),
        render(&stop_failure(Some(recording_stats()))),
    );
    sections.insert(STREAM_FAILURE.to_owned(), render(&stop_failure(None)));
    sections.insert(ENUMS.to_owned(), enum_values());

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wire_shapes.txt");
    if std::env::var_os("UPDATE_WIRE_SHAPES").is_some() {
        std::fs::write(&path, file_text(&sections)).expect("write the pinned shapes");
        return;
    }
    let pinned = parse(&std::fs::read_to_string(&path).expect("read the pinned shapes"));
    let mut changes = Vec::new();
    for title in sections
        .keys()
        .chain(pinned.keys())
        .collect::<BTreeSet<_>>()
    {
        let now = sections.get(title).cloned().unwrap_or_default();
        let then = pinned.get(title).cloned().unwrap_or_default();
        for line in then.difference(&now) {
            changes.push(format!("{title}: - {line}"));
        }
        for line in now.difference(&then) {
            changes.push(format!("{title}: + {line}"));
        }
    }
    assert!(
        changes.is_empty(),
        "the wire shapes changed (a protocol change; see this file's docs):\n{}",
        changes.join("\n")
    );
}

/// Every path of `value` with the JSON types found there, one per line:
/// `a.b: number`, `a[]` for the elements of a list (merged over all of
/// them), `a: null|string` where elements differ, and `absent` for a path
/// that some elements of a list lack (another variant, a skipped field).
/// Whether a field may be `null` is not pinned (samples fill every
/// option); `docs/protocol.md` says so per field.
fn render(value: &impl Serialize) -> BTreeSet<String> {
    let mut paths = BTreeMap::new();
    walk(
        &serde_json::to_value(value).expect("serialize"),
        String::new(),
        &mut paths,
    );
    paths
        .into_iter()
        .map(|(path, kinds): (String, BTreeSet<&str>)| {
            format!(
                "{path}: {}",
                kinds.into_iter().collect::<Vec<_>>().join("|")
            )
        })
        .collect()
}

fn walk(value: &Value, path: String, paths: &mut BTreeMap<String, BTreeSet<&'static str>>) {
    let kind = match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    if !path.is_empty() {
        paths.entry(path.clone()).or_default().insert(kind);
    }
    match value {
        Value::Array(items) => {
            assert!(
                !items.is_empty(),
                "`{path}` is empty in its sample; give it an element so its shape is pinned"
            );
            let shapes: Vec<_> = items
                .iter()
                .map(|item| {
                    let mut own = BTreeMap::new();
                    walk(item, format!("{path}[]"), &mut own);
                    own
                })
                .collect();
            let all: BTreeSet<String> = shapes.iter().flat_map(|own| own.keys().cloned()).collect();
            for own in shapes {
                for missing in all.iter().filter(|path| !own.contains_key(*path)) {
                    paths.entry(missing.clone()).or_default().insert("absent");
                }
                for (path, kinds) in own {
                    paths.entry(path).or_default().extend(kinds);
                }
            }
        }
        Value::Object(fields) => {
            for (key, item) in fields {
                let path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                walk(item, path, paths);
            }
        }
        _ => {}
    }
}

const HEADER: &str = "\
# The wire shape of every result (ADR 0018, point 8), checked by
# wire_shapes.rs. Generated: UPDATE_WIRE_SHAPES=1 cargo test -p command-api
# --test wire_shapes. A change here changes protocol v1.
";

fn file_text(sections: &BTreeMap<String, BTreeSet<String>>) -> String {
    let mut text = HEADER.to_owned();
    for (title, lines) in sections {
        text.push_str(&format!("\n== {title}\n"));
        // By path, so a field comes before its children.
        let mut lines: Vec<_> = lines.iter().collect();
        lines.sort_by_key(|line| line.split_once(": ").map(|(path, _)| path));
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
    }
    text
}

fn parse(text: &str) -> BTreeMap<String, BTreeSet<String>> {
    let mut sections: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut title = None;
    // `lines` also takes a checkout's `\r\n`.
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("== ") {
            title = Some(name.to_owned());
            sections.entry(name.to_owned()).or_default();
        } else if !line.is_empty() && !line.starts_with('#') {
            let title = title.as_ref().expect("a line before the first section");
            sections
                .get_mut(title)
                .expect("section")
                .insert(line.to_owned());
        }
    }
    sections
}

fn samples() -> Vec<(&'static [&'static str], Value)> {
    let output = |output: CommandOutput| serde_json::to_value(output).expect("serialize");
    vec![
        (
            &["get_status"],
            output(CommandOutput::Status(EngineStatus {
                application: "StreamForge".to_owned(),
                version: "0.1.0".to_owned(),
                pipeline_running: true,
                capture_available: true,
                recording_state: Some(RecordingState::Running),
                streaming_state: Some(StreamState::Connected),
                preview_active: true,
            })),
        ),
        (
            &["get_health"],
            output(CommandOutput::Health(Box::new(health()))),
        ),
        (
            &["list_commands"],
            output(CommandOutput::Commands(CommandList::all())),
        ),
        (
            &["get_capture_capabilities"],
            output(CommandOutput::CaptureCapabilities(capabilities())),
        ),
        (
            &["list_audio_inputs"],
            output(CommandOutput::AudioInputs(AudioInputList {
                inputs: vec![AudioInput {
                    name: "alsa_input.usb".to_owned(),
                    description: Some("USB microphone".to_owned()),
                    media_class: "Audio/Source".to_owned(),
                    loopback_of: Some("alsa_output.usb".to_owned()),
                }],
            })),
        ),
        (
            &["list_audio_outputs"],
            output(CommandOutput::AudioOutputs(AudioOutputList {
                outputs: vec![AudioOutput {
                    name: "alsa_output.usb".to_owned(),
                    description: Some("USB headphones".to_owned()),
                    media_class: "Audio/Sink".to_owned(),
                }],
            })),
        ),
        (
            &["list_capture_targets"],
            output(CommandOutput::CaptureTargets(CaptureTargetList {
                targets: vec![ScreenSource {
                    id: "monitor:1".to_owned(),
                    kind: VideoSourceKind::Monitor,
                    name: "DP-1".to_owned(),
                    width: Some(1920),
                    height: Some(1080),
                }],
            })),
        ),
        (
            &["probe_screen_capture"],
            output(CommandOutput::VideoCaptureReport(video_capture())),
        ),
        (
            &["probe_audio_capture"],
            output(CommandOutput::AudioCaptureReport(audio_capture())),
        ),
        (
            &["probe_composite"],
            output(CommandOutput::CompositeReport(Box::new(CompositeReport {
                render: render_stats(),
                screen_capture: Some(video_capture()),
                screen_capture_error: Some(error()),
                snapshot: Some(SnapshotResult {
                    path: "/videos/last.ppm".to_owned(),
                    written: false,
                    error: Some(error()),
                }),
                warnings: warnings(),
            }))),
        ),
        (
            &["probe_audio_mix"],
            output(CommandOutput::MixReport(Box::new(MixReport {
                mix: mix_stats(),
                captures: vec![capture_outcome()],
                warnings: warnings(),
            }))),
        ),
        (
            &["start_recording"],
            output(CommandOutput::RecordingStarted(RecordingStarted {
                path: "/videos/take.mkv".to_owned(),
                width: 1920,
                height: 1080,
                fps: Rational::integer(60),
                sample_rate_hz: 48_000,
                channels: 2,
                video_encoder: video_encoder(),
                warnings: warnings(),
            })),
        ),
        (
            &["stop_recording"],
            output(CommandOutput::RecordingReport(Box::new(RecordingReport {
                recording: recording_stats(),
                pipeline: pipeline(),
                warnings: warnings(),
            }))),
        ),
        (
            &["start_streaming"],
            output(CommandOutput::StreamingStarted(StreamingStarted {
                target: "rtmp://example.com/live/<stream key>".to_owned(),
                width: 1920,
                height: 1080,
                fps: Rational::integer(60),
                video_bitrate_kbps: 6_000,
                sample_rate_hz: 48_000,
                channels: 2,
                video_encoder: video_encoder(),
                warnings: warnings(),
            })),
        ),
        (
            &["stop_streaming"],
            output(CommandOutput::StreamingReport(Box::new(StreamingReport {
                streaming: stream_stats(),
                pipeline: pipeline(),
                warnings: warnings(),
            }))),
        ),
        (
            &[
                "list_scenes",
                "create_scene",
                "delete_scene",
                "set_active_scene",
                "add_source",
                "update_source",
                "move_source",
                "remove_source",
                "restart_source",
                "set_capture_target",
            ],
            output(CommandOutput::SceneList(scene_list())),
        ),
        (
            &[
                "get_mixer",
                "add_channel",
                "remove_channel",
                "update_channel",
                "set_channel_input",
                "add_bus",
                "remove_bus",
                "update_bus",
                "set_send",
            ],
            output(CommandOutput::MixerState(mixer())),
        ),
        (
            &["get_output_profiles", "set_monitor_device"],
            output(CommandOutput::OutputProfiles(profiles())),
        ),
        (
            &["set_output_profile"],
            output(CommandOutput::OutputProfileSet(OutputProfileSet {
                profiles: profiles(),
                applied_now: vec!["mix"],
                next_start: vec!["video_encoder"],
                warnings: warnings(),
            })),
        ),
        (
            &["set_preview"],
            output(CommandOutput::PreviewState(Box::new(PreviewState {
                enabled: false,
                width: 480,
                height: 270,
                fps: Rational::integer(30),
                frames_published: Some(900),
                frames_dropped: Some(1),
                pipeline: Some(pipeline()),
                warnings: warnings(),
            }))),
        ),
        (
            &["list_projects"],
            output(CommandOutput::ProjectFiles(ProjectFileList {
                files: vec![ProjectFileInfo {
                    file_name: "show.json".to_owned(),
                    bytes: 2_048,
                    modified_unix_ms: Some(1_761_500_000_000),
                }],
                omitted: 0,
            })),
        ),
        (
            &["list_cameras"],
            output(CommandOutput::Cameras(CameraList {
                cameras: vec![Camera {
                    camera_id: "v4l2_input.pci-0000_00_14.0-usb-0_1_1.0".to_owned(),
                    name: "USB Camera".to_owned(),
                    modes: vec![CameraMode {
                        width: 1280,
                        height: 720,
                        fps: Rational { num: 30, den: 1 },
                    }],
                }],
            })),
        ),
        (
            &["list_images"],
            output(CommandOutput::ImageFiles(ImageFileList {
                files: vec![ImageFileInfo {
                    file_name: "logo.png".to_owned(),
                    bytes: 4_096,
                    modified_unix_ms: Some(1_761_500_000_000),
                    width: Some(320),
                    height: Some(200),
                }],
                omitted: 0,
            })),
        ),
        (
            &["save_project"],
            output(CommandOutput::ProjectSaved(ProjectSaved {
                file_name: "show.json".to_owned(),
                path: "/projects/show.json".to_owned(),
                replaced: true,
                scenes: 1,
                bytes: 2_048,
            })),
        ),
        (
            &["load_project"],
            output(CommandOutput::ProjectLoaded(Box::new(ProjectLoaded {
                file_name: "show.json".to_owned(),
                scenes: scene_list(),
                mixer: mixer(),
                warnings: warnings(),
            }))),
        ),
        (
            &["validate_project"],
            output(CommandOutput::ProjectValidation(ProjectValidation {
                file_name: "show.json".to_owned(),
                valid: false,
                schema_version: Some(3),
                migrates: false,
                problems: vec![ProjectProblem {
                    path: "scenes[0].name".to_owned(),
                    message: "must not be empty".to_owned(),
                }],
            })),
        ),
    ]
}

/// The wire names of every enum in results, one line per enum: a renamed
/// variant changes the protocol without changing a path. The matches fail
/// to compile when a variant is added, so its list grows with it.
fn enum_values() -> BTreeSet<String> {
    fn line<T: Serialize>(name: &str, tag: Option<&str>, variants: &[T]) -> String {
        let values: Vec<String> = variants
            .iter()
            .map(|variant| {
                let value = serde_json::to_value(variant).expect("serialize");
                let value = tag.map_or(&value, |tag| &value[tag]);
                value.as_str().expect("a name").to_owned()
            })
            .collect();
        format!("{name}: {}", values.join("|"))
    }
    use RecordingState as R;
    let _ = |state: R| match state {
        R::Running | R::Failed => {}
    };
    use StreamState as S;
    let _ = |state: S| match state {
        S::Connecting | S::Connected | S::Reconnecting | S::Failed | S::Stopped => {}
    };
    let _ = |state: CaptureState| match state {
        CaptureState::Running | CaptureState::Ended { .. } => {}
    };
    let _ = |kind: CaptureKind| match kind {
        CaptureKind::Screen | CaptureKind::Audio | CaptureKind::Camera => {}
    };
    use OutputKind as O;
    let _ = |output: O| match output {
        O::Recording | O::Stream | O::Monitor => {}
    };
    use EncoderPreference as E;
    let _ = |preference: E| match preference {
        E::Auto | E::Software | E::Hardware => {}
    };
    let _ = |tap: SendTap| match tap {
        SendTap::PreFader | SendTap::PostFader => {}
    };
    let _ = |kind: SourceKind| match kind {
        SourceKind::ColorBars
        | SourceKind::Solid { .. }
        | SourceKind::Screen { .. }
        | SourceKind::Image { .. }
        | SourceKind::Camera { .. } => {}
    };
    let _ = |input: ChannelInput| match input {
        ChannelInput::Device { .. } | ChannelInput::Tone { .. } => {}
    };
    use VideoSourceKind as V;
    let _ = |kind: V| match kind {
        V::Monitor | V::Window | V::Virtual => {}
    };
    use CursorMode as C;
    let _ = |mode: C| match mode {
        C::Hidden | C::Embedded | C::Metadata => {}
    };
    use CommandAccess as A;
    let _ = |access: A| match access {
        A::ReadOnly | A::Control | A::Sensitive => {}
    };
    // `ErrorCode::ALL` is kept complete by `command-api`'s own tests.
    let codes: Vec<ErrorSummary> = ErrorCode::ALL
        .iter()
        .map(|code| ErrorSummary {
            code: *code,
            message: String::new(),
            details: None,
        })
        .collect();
    [
        line("RecordingState", None, &[R::Running, R::Failed]),
        line(
            "StreamState",
            None,
            &[
                S::Connecting,
                S::Connected,
                S::Reconnecting,
                S::Failed,
                S::Stopped,
            ],
        ),
        line(
            "CaptureState",
            Some("state"),
            &[
                CaptureState::Running,
                CaptureState::Ended {
                    reason: String::new(),
                },
            ],
        ),
        line(
            "CaptureKind",
            None,
            &[CaptureKind::Screen, CaptureKind::Audio, CaptureKind::Camera],
        ),
        line("OutputKind", None, &[O::Recording, O::Stream, O::Monitor]),
        line(
            "EncoderPreference",
            None,
            &[E::Auto, E::Software, E::Hardware],
        ),
        line("SendTap", None, &[SendTap::PreFader, SendTap::PostFader]),
        line(
            "SourceKind",
            Some("kind"),
            &[
                SourceKind::ColorBars,
                SourceKind::Solid {
                    color: Rgba([0; 4]),
                },
                SourceKind::Screen {
                    target_kinds: Vec::new(),
                    cursor_mode: C::Hidden,
                    capture_target_id: None,
                },
                SourceKind::Image {
                    file: String::new(),
                },
                SourceKind::Camera {
                    camera_id: String::new(),
                    mode: None,
                },
            ],
        ),
        line(
            "ChannelInput",
            Some("kind"),
            &[
                ChannelInput::Device { device: None },
                ChannelInput::Tone {
                    frequency_hz: 1_000.0,
                    level_dbfs: -20.0,
                },
            ],
        ),
        line(
            "VideoSourceKind",
            None,
            &[V::Monitor, V::Window, V::Virtual],
        ),
        line("CursorMode", None, &[C::Hidden, C::Embedded, C::Metadata]),
        line(
            "CommandAccess",
            None,
            &[A::ReadOnly, A::Control, A::Sensitive],
        ),
        line("ErrorCode", Some("code"), &codes),
    ]
    .into_iter()
    .collect()
}

fn stop_failure(recording: Option<RecordingStats>) -> StopFailure {
    StopFailure {
        recording,
        pipeline: pipeline(),
        warnings: warnings(),
    }
}

fn warnings() -> Vec<String> {
    vec!["a warning".to_owned()]
}

/// `details` is free-form per error; an empty object stands for it.
fn error() -> ErrorSummary {
    CaptureError::new(CaptureErrorKind::NotFound, "the window is gone")
        .with_details(json!({}))
        .into()
}

fn latency() -> LatencySummary {
    LatencySummary {
        min_ms: 1.0,
        mean_ms: 2.0,
        p95_ms: 3.0,
        p99_ms: 4.0,
        max_ms: 5.0,
    }
}

fn meter() -> MeterReading {
    MeterReading {
        peak_dbfs: Some(-6.0),
        rms_dbfs: Some(-12.0),
    }
}

fn timestamp() -> MediaTimestamp {
    MediaTimestamp {
        ticks: 48_000,
        time_base_num: 1,
        time_base_den: 48_000,
    }
}

fn video_encoder() -> VideoEncoderInfo {
    VideoEncoderInfo {
        encoder: "h264_vaapi".to_owned(),
        hardware: true,
        skipped: vec![SkippedEncoder {
            encoder: "h264_nvenc".to_owned(),
            reason: "no device".to_owned(),
        }],
    }
}

fn video_track() -> VideoTrackStats {
    VideoTrackStats {
        encoder: video_encoder(),
        width: 1920,
        height: 1080,
        fps: Rational::integer(60),
        frames_encoded: 600,
        frames_dropped: 0,
        frames_after_end: 1,
        first_pts_ms: Some(0.0),
        end_ms: Some(10_000.0),
    }
}

fn audio_track(name: &str) -> AudioTrackStats {
    AudioTrackStats {
        name: Some(name.to_owned()),
        encoder: "aac".to_owned(),
        sample_rate_hz: 48_000,
        channels: 2,
        bitrate_bps: 160_000,
        samples_encoded: 480_000,
        blocks_dropped: 0,
        blocks_after_end: 1,
        gap_samples: 0,
        overlap_samples: 0,
        discontinuities: 0,
        padding_samples: 512,
        first_pts_ms: Some(0.0),
        end_ms: Some(10_000.0),
    }
}

fn recording_stats() -> RecordingStats {
    RecordingStats {
        path: "/videos/take.mkv".to_owned(),
        container: "matroska".to_owned(),
        duration_ms: 10_000.0,
        file_bytes: Some(12_000_000),
        video: video_track(),
        audio_tracks: vec![audio_track("Program"), audio_track("Voice")],
    }
}

fn stream_stats() -> StreamStats {
    StreamStats {
        target: "rtmp://example.com/live/<stream key>".to_owned(),
        container: "flv".to_owned(),
        state: StreamState::Connected,
        connects: 1,
        disconnects: 0,
        reconnect_attempts: 0,
        connected_ms: 10_000.0,
        last_error: Some("connection reset".to_owned()),
        video: video_track(),
        audio_tracks: [audio_track("Program")],
        frames_dropped_offline: 0,
        blocks_dropped_offline: 0,
        frames_undelivered: 0,
        samples_undelivered: 0,
    }
}

fn render_stats() -> RenderStats {
    RenderStats {
        renderer: "cpu",
        threads: 4,
        width: 1920,
        height: 1080,
        target_fps: Rational::integer(60),
        elapsed_ms: 10_000,
        frames_rendered: 600,
        skipped_ticks: 0,
        late_frames: 0,
        achieved_fps: Some(60.0),
        render_time: Some(latency()),
        scene_updates: 1,
        retired_sources: 0,
        sources: vec![SourceStats {
            id: SourceId::from("src_desk"),
            name: "desk".to_owned(),
            kind: "live",
            frames_consumed: 600,
            stale_ticks: 0,
            missing_ticks: 1,
            mailbox: Some(MailboxStats::default()),
        }],
    }
}

fn mix_stats() -> MixStats {
    MixStats {
        sample_rate_hz: 48_000,
        channels: 2,
        block_frames: 480,
        elapsed_ms: 10_000,
        blocks_mixed: 1_000,
        late_blocks: 0,
        skipped_blocks: 0,
        mix_time: Some(latency()),
        program: meter(),
        clipped_samples: 0,
        invalid_samples: 0,
        inputs: vec![InputStats {
            name: "Mic".to_owned(),
            kind: "live",
            gain_db: 0.0,
            muted: false,
            meter: meter(),
            warmup_frames: 480,
            underrun_frames: 0,
            trimmed_frames: 0,
            drift_ppm: Some(12.5),
            ring: Some(AudioRingStats::default()),
            buffer: Some(BufferLatency {
                target_ms: 20.0,
                min_ms: 10.0,
                mean_ms: 20.0,
                max_ms: 30.0,
                windows_ms: vec![20.0],
            }),
        }],
        aux_buses: vec![BusStats {
            name: "Voice".to_owned(),
            meter: meter(),
            clipped_samples: 0,
            invalid_samples: 0,
        }],
    }
}

fn video_capture() -> VideoCaptureReport {
    VideoCaptureReport {
        backend: "pipewire".to_owned(),
        source: VideoSourceInfo {
            kind: Some(VideoSourceKind::Monitor),
            width: Some(1920),
            height: Some(1080),
        },
        format: Some(VideoFormatInfo {
            pixel_format: "BGRx".to_owned(),
            width: 1920,
            height: 1080,
            framerate: Rational::integer(60),
        }),
        buffer_memory: Some("dmabuf".to_owned()),
        stats: VideoStats {
            elapsed_ms: 5_000,
            frames: 300,
            empty_buffers: 0,
            corrupted_buffers: 0,
            dropped_frames: 0,
            buffer_starvation: 0,
            frames_discarded: 0,
            measured_fps: Some(60.0),
            frame_interval: Some(IntervalStats {
                min_ms: 16.0,
                mean_ms: 16.7,
                max_ms: 17.0,
            }),
            first_pts: Some(timestamp()),
            last_pts: Some(timestamp()),
            non_monotonic_pts: 0,
            frames_without_pts: 0,
        },
        warnings: warnings(),
    }
}

fn audio_capture() -> AudioCaptureReport {
    AudioCaptureReport {
        backend: "pipewire".to_owned(),
        input: Some("alsa_input.usb".to_owned()),
        format: Some(AudioFormatInfo {
            sample_format: "f32le".to_owned(),
            sample_rate_hz: 48_000,
            channels: 2,
        }),
        stats: AudioStats {
            elapsed_ms: 5_000,
            buffers: 500,
            frames: 240_000,
            expected_frames: Some(240_000),
            frame_deficit: Some(0),
            empty_buffers: 0,
            dropped_buffers: 0,
            buffer_starvation: 0,
            invalid_samples: 0,
            clipped_samples: 0,
            peak_dbfs: Some(-6.0),
            rms_dbfs: Some(-12.0),
            first_position: Some(timestamp()),
            last_position: Some(timestamp()),
        },
        warnings: warnings(),
    }
}

fn capture_outcome() -> CaptureOutcome {
    CaptureOutcome {
        input: Some("alsa_input.usb".to_owned()),
        report: Some(audio_capture()),
        error: Some(error()),
    }
}

fn monitor() -> MonitorHealth {
    MonitorHealth {
        device: Some("alsa_output.usb".to_owned()),
        enabled: true,
        state: CaptureState::Running,
        latency_ms: Some(30.0),
        feed: Some(PlaybackFeedStats {
            ring: SampleRingStats::default(),
            fill_frames: 960,
            pull_frames: 480,
            target_frames: 960,
            drift_ppm: 3.0,
            padded_frames: 0,
            trimmed_frames: 0,
        }),
    }
}

fn pipeline() -> PipelineReport {
    PipelineReport {
        stopped: true,
        render: Some(render_stats()),
        mix: Some(mix_stats()),
        audio_captures: vec![capture_outcome()],
        scene_captures: vec![SceneCaptureOutcome {
            id: "src_desk".to_owned(),
            name: "desk".to_owned(),
            kind: CaptureKind::Screen,
            report: Some(video_capture()),
            error: Some(error()),
        }],
        scene_captures_omitted: 0,
        monitor: Some(monitor()),
    }
}

fn health() -> HealthReport {
    HealthReport {
        pipeline_running: true,
        render: Some(LiveRenderStats {
            elapsed_ms: 10_000,
            frames_rendered: 600,
            skipped_ticks: 0,
            late_frames: 0,
            render_time: Some(latency()),
            sources: vec![LiveSourceStats {
                id: SourceId::from("src_desk"),
                name: "desk".to_owned(),
                kind: "live",
                frames_consumed: 600,
                stale_ticks: 0,
                missing_ticks: 1,
                last_frame_age_ms: Some(16),
            }],
        }),
        mix: Some(LiveMixStats {
            elapsed_ms: 10_000,
            blocks_mixed: 1_000,
            late_blocks: 0,
            skipped_blocks: 0,
            clipped_samples: 0,
            invalid_samples: 0,
            mix_time: Some(latency()),
            inputs: vec![LiveInputStats {
                name: "Mic".to_owned(),
                kind: "live",
                warmup_frames: 480,
                underrun_frames: 0,
                trimmed_frames: 0,
                fill_ms: Some(20.0),
                drift_ppm: Some(12.5),
                overrun_frames: Some(0),
            }],
            aux_buses: vec![LiveBusStats {
                name: "Voice".to_owned(),
                clipped_samples: 0,
                invalid_samples: 0,
            }],
        }),
        recording: Some(OutputHealth {
            target: "/videos/take.mkv".to_owned(),
            elapsed_ms: 10_000,
            running: true,
            state: RecordingState::Running,
            video_encoder: video_encoder(),
            counters: LiveOutputStats::default(),
            connection: None,
        }),
        streaming: Some(OutputHealth {
            target: "rtmp://example.com/live/<stream key>".to_owned(),
            elapsed_ms: 10_000,
            running: true,
            state: StreamState::Connected,
            video_encoder: video_encoder(),
            counters: LiveOutputStats::default(),
            connection: Some(LiveConnectionStats::default()),
        }),
        monitor: Some(monitor()),
        captures: vec![
            CaptureHealth {
                kind: CaptureKind::Screen,
                id: "src_desk".to_owned(),
                name: "desk".to_owned(),
                state: CaptureState::Running,
                format: None,
            },
            CaptureHealth {
                kind: CaptureKind::Camera,
                id: "src_cam".to_owned(),
                name: "cam".to_owned(),
                state: CaptureState::Running,
                format: Some(VideoFormatInfo {
                    pixel_format: "YUY2".to_owned(),
                    width: 1280,
                    height: 720,
                    framerate: Rational::integer(30),
                }),
            },
            CaptureHealth {
                kind: CaptureKind::Audio,
                id: "ch_mic".to_owned(),
                name: "Mic".to_owned(),
                state: CaptureState::Ended {
                    reason: "the device was removed".to_owned(),
                },
                format: None,
            },
        ],
    }
}

fn capabilities() -> CaptureCapabilities {
    CaptureCapabilities {
        platform: "linux".to_owned(),
        desktop: Some("KDE".to_owned()),
        session_type: Some("wayland".to_owned()),
        screen: ScreenCaptureCapability {
            backend: Some("pipewire".to_owned()),
            available: true,
            unavailable_reason: Some("no portal".to_owned()),
            portal_version: Some(5),
            target_kinds: vec![VideoSourceKind::Monitor],
            cursor_modes: vec![CursorMode::Embedded],
            picker: true,
            target_list: false,
        },
        audio: AudioCaptureCapability {
            backend: Some("pipewire".to_owned()),
            available: true,
            unavailable_reason: Some("no audio server".to_owned()),
            server_version: Some("1.4.0".to_owned()),
        },
        camera: CameraCaptureCapability {
            backend: Some("pipewire".to_owned()),
            available: true,
            unavailable_reason: Some("no camera service".to_owned()),
        },
    }
}

fn scene_list() -> SceneList {
    let rect = SourceRect {
        x: 0.0,
        y: 0.0,
        width: 1920.0,
        height: 1080.0,
    };
    let source = |id: &str, kind| SourceSpec {
        id: id.to_owned(),
        name: id.trim_start_matches("src_").to_owned(),
        kind,
        rect,
        opacity: 1.0,
        visible: true,
    };
    SceneList {
        canvas_width: 1920,
        canvas_height: 1080,
        active_scene_name: "Show".to_owned(),
        active_scene_id: "scene_show".to_owned(),
        scenes: vec![SceneSpec {
            id: "scene_show".to_owned(),
            name: "Show".to_owned(),
            sources: vec![
                source("src_bars", SourceKind::ColorBars),
                source(
                    "src_red",
                    SourceKind::Solid {
                        color: Rgba([255, 0, 0, 128]),
                    },
                ),
                source(
                    "src_desk",
                    SourceKind::Screen {
                        target_kinds: vec![VideoSourceKind::Monitor],
                        cursor_mode: CursorMode::Embedded,
                        capture_target_id: Some("monitor:1".to_owned()),
                    },
                ),
                // Without a capture target, which is then left out.
                source(
                    "src_picked",
                    SourceKind::Screen {
                        target_kinds: vec![VideoSourceKind::Window],
                        cursor_mode: CursorMode::Hidden,
                        capture_target_id: None,
                    },
                ),
                source(
                    "src_logo",
                    SourceKind::Image {
                        file: "logo.png".to_owned(),
                    },
                ),
                source(
                    "src_cam",
                    SourceKind::Camera {
                        camera_id: "v4l2_input.usb-cam".to_owned(),
                        mode: Some(CameraModeSpec {
                            width: Some(1280),
                            height: Some(720),
                            fps: Some(Rational::integer(30)),
                        }),
                    },
                ),
                // The engine chooses the mode, which is then left out.
                source(
                    "src_auto",
                    SourceKind::Camera {
                        camera_id: "v4l2_input.usb-cam".to_owned(),
                        mode: None,
                    },
                ),
            ],
        }],
        warnings: vec!["camera `v4l2_input.usb-cam` has no 1280x720 mode".to_owned()],
    }
}

fn mixer() -> MixerState {
    let channel = |id: &str, input| ChannelState {
        spec: ChannelSpec {
            id: id.to_owned(),
            name: id.trim_start_matches("ch_").to_owned(),
            input,
            gain_db: 0.0,
            muted: false,
            balance: 0.0,
            sends: vec![SendSpec {
                bus: "bus_program".to_owned(),
                enabled: true,
                level_db: 0.0,
                tap: SendTap::PostFader,
            }],
        },
        peak_dbfs: Some(-6.0),
    };
    MixerState {
        running: true,
        channels: vec![
            channel(
                "ch_mic",
                ChannelInput::Device {
                    device: Some("alsa_input.usb".to_owned()),
                },
            ),
            channel(
                "ch_tone",
                ChannelInput::Tone {
                    frequency_hz: 1_000.0,
                    level_dbfs: -20.0,
                },
            ),
        ],
        buses: vec![BusState {
            id: "bus_voice".to_owned(),
            name: "Voice".to_owned(),
            kind: "aux",
            gain_db: 0.0,
            muted: false,
            assign_db: Some(0.0),
            peak_dbfs: Some(-6.0),
        }],
        outputs: vec![OutputMix {
            output: OutputKind::Recording,
            bus_id: "bus_program".to_owned(),
            tracks: vec!["bus_voice".to_owned()],
            running: true,
        }],
        warnings: warnings(),
    }
}

fn profiles() -> OutputProfiles {
    OutputProfiles {
        recording: RecordingProfile {
            mix: "bus_program".to_owned(),
            tracks: vec!["bus_voice".to_owned()],
            video_encoder: EncoderPreference::Auto,
        },
        stream: StreamProfile {
            mix: "bus_program".to_owned(),
            video_bitrate_kbps: 6_000,
            video_encoder: EncoderPreference::Hardware,
        },
        monitor: MonitorProfile {
            mix: "bus_voice".to_owned(),
            enabled: true,
            device: Some("alsa_output.usb".to_owned()),
        },
    }
}
