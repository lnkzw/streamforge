//! Command registry: wire names, metadata, and parameter decoding.

use std::path::PathBuf;
use std::time::Duration;

use capture_api::{
    AudioCaptureRequest, CursorMode, DEFAULT_PROBE_DURATION, MAX_PROBE_DURATION,
    MIN_PROBE_DURATION, ScreenCaptureRequest, ScreenSelection, VideoSourceKind,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use audio_engine::{MAX_GAIN_DB, MAX_INPUTS, MIN_GAIN_DB};
use encoder_api::{ReconnectPolicy, StreamKey, StreamTarget};

use crate::{
    Command, CommandError, CompositeRequest, ErrorCode, LiveInputSpec, MAX_MIX_DURATION,
    MediaSources, MixRequest, RecordingRequest, StreamingRequest, ToneSpec,
};

/// Permission class of a command.
///
/// Adapters must require explicit authorization for anything other than
/// [`CommandAccess::ReadOnly`]; enforcement arrives with agent integration (M8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandAccess {
    /// Inspects state without changing it.
    ReadOnly,
    /// Changes engine state, e.g. starts or stops an output.
    Control,
    /// Destructive, credential-related, or privacy-sensitive (e.g. microphone access).
    Sensitive,
}

/// Static metadata for one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CommandDescriptor {
    /// Stable `snake_case` wire name.
    pub name: &'static str,
    pub summary: &'static str,
    pub access: CommandAccess,
}

const GET_STATUS: CommandDescriptor = CommandDescriptor {
    name: "get_status",
    summary: "Report engine status.",
    access: CommandAccess::ReadOnly,
};

const LIST_COMMANDS: CommandDescriptor = CommandDescriptor {
    name: "list_commands",
    summary: "List commands supported by this engine.",
    access: CommandAccess::ReadOnly,
};

const GET_CAPTURE_CAPABILITIES: CommandDescriptor = CommandDescriptor {
    name: "get_capture_capabilities",
    summary: "Query screen and audio capture capabilities of the running system.",
    access: CommandAccess::ReadOnly,
};

const LIST_AUDIO_INPUTS: CommandDescriptor = CommandDescriptor {
    name: "list_audio_inputs",
    summary: "List audio input devices.",
    access: CommandAccess::ReadOnly,
};

const PROBE_SCREEN_CAPTURE: CommandDescriptor = CommandDescriptor {
    name: "probe_screen_capture",
    summary: "Capture a user-selected screen or window and report format, timestamps, FPS and drops. \
              Params: duration_ms (100..=60000, default 5000), source_types ([\"monitor\",\"window\",\"virtual\"], \
              default [\"monitor\",\"window\"]), cursor (\"hidden\"|\"embedded\"|\"metadata\", default \"embedded\").",
    access: CommandAccess::Sensitive,
};

const PROBE_AUDIO_CAPTURE: CommandDescriptor = CommandDescriptor {
    name: "probe_audio_capture",
    summary: "Capture an audio input and report format, levels, timestamps and drops; no audio is stored. \
              Params: input (name from list_audio_inputs, default: system default), duration_ms (100..=60000, default 5000).",
    access: CommandAccess::Sensitive,
};

const PROBE_COMPOSITE: CommandDescriptor = CommandDescriptor {
    name: "probe_composite",
    summary: "Composite a 1920x1080@60 test scene (color bars, moving overlay, optional live screen) \
              and report render time, late frames and source drops. Params: duration_ms (100..=60000, \
              default 5000), screen (bool, default false; opens the system picker), source_types and \
              cursor (only with screen), snapshot_path (plain .ppm file name, written to the engine's output \
              directory, never overwritten).",
    access: CommandAccess::Sensitive,
};

const PROBE_AUDIO_MIX: CommandDescriptor = CommandDescriptor {
    name: "probe_audio_mix",
    summary: "Mix live audio inputs and test tones at 48 kHz stereo and report meters, buffer \
              latency over time, underruns and clipping; no audio is stored. Params: duration_ms \
              (100..=900000, default 5000), inputs ([{input, gain_db, mute}]), tones \
              ([{frequency_hz, level_dbfs, gain_db, mute}]), master_gain_db; up to 8 sources, \
              default one 1 kHz test tone.",
    access: CommandAccess::Sensitive,
};

const START_RECORDING: CommandDescriptor = CommandDescriptor {
    name: "start_recording",
    summary: "Record the media pipeline (1920x1080@60 reference scene, optional live screen layer, \
              audio mix) to H.264/AAC in Matroska. Params: file_name (required, plain .mkv name in \
              the engine's output directory, never overwritten); sources (screen, source_types, \
              cursor, inputs, tones, master_gain_db) only if no other output is running, since \
              outputs share one pipeline; without sources and without a running pipeline, one \
              -20 dBFS 1 kHz tone. Returns immediately; recording continues until stop_recording.",
    access: CommandAccess::Sensitive,
};

const STOP_RECORDING: CommandDescriptor = CommandDescriptor {
    name: "stop_recording",
    summary: "Stop the running recording, finalize the file and report its statistics.",
    access: CommandAccess::Control,
};

const START_STREAMING: CommandDescriptor = CommandDescriptor {
    name: "start_streaming",
    summary: "Publish the media pipeline over RTMP (H.264 CBR + AAC in FLV). Params: url \
              (required, rtmp:// or rtmps://, no credentials), stream_key (secret, never echoed), \
              video_bitrate_kbps (default 6000), reconnect_attempts (default 5), \
              reconnect_delay_ms (default 1000, doubling up to 30 s), and sources as for \
              start_recording. Returns once connected; reconnects after errors; independent of \
              recording.",
    access: CommandAccess::Sensitive,
};

const STOP_STREAMING: CommandDescriptor = CommandDescriptor {
    name: "stop_streaming",
    summary: "Stop the live stream, disconnect and report its connection history.",
    access: CommandAccess::Control,
};

const COMMANDS: &[CommandDescriptor] = &[
    GET_STATUS,
    LIST_COMMANDS,
    GET_CAPTURE_CAPABILITIES,
    LIST_AUDIO_INPUTS,
    PROBE_SCREEN_CAPTURE,
    PROBE_AUDIO_CAPTURE,
    PROBE_COMPOSITE,
    PROBE_AUDIO_MIX,
    START_RECORDING,
    STOP_RECORDING,
    START_STREAMING,
    STOP_STREAMING,
];

const DEFAULT_SOURCE_KINDS: [VideoSourceKind; 2] =
    [VideoSourceKind::Monitor, VideoSourceKind::Window];
const DEFAULT_CURSOR: CursorMode = CursorMode::Embedded;

/// All commands known to this build, in a stable order.
pub fn registry() -> &'static [CommandDescriptor] {
    COMMANDS
}

/// Result of `list_commands`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommandList {
    pub commands: Vec<CommandDescriptor>,
}

impl CommandList {
    pub fn all() -> Self {
        Self {
            commands: COMMANDS.to_vec(),
        }
    }
}

impl Command {
    /// Decode a command from its wire name and optional JSON parameters.
    ///
    /// Absent, `null`, and empty-object parameters are equivalent and select
    /// each parameter's default.
    pub fn decode(name: &str, params: Option<&Value>) -> Result<Self, CommandError> {
        match name {
            "get_status" => expect_no_params(name, params).map(|()| Self::GetStatus),
            "list_commands" => expect_no_params(name, params).map(|()| Self::ListCommands),
            "get_capture_capabilities" => {
                expect_no_params(name, params).map(|()| Self::GetCaptureCapabilities)
            }
            "list_audio_inputs" => expect_no_params(name, params).map(|()| Self::ListAudioInputs),
            "probe_screen_capture" => {
                decode_screen_probe(name, params).map(Self::ProbeScreenCapture)
            }
            "probe_audio_capture" => decode_audio_probe(name, params).map(Self::ProbeAudioCapture),
            "probe_composite" => decode_composite_probe(name, params).map(Self::ProbeComposite),
            "probe_audio_mix" => decode_mix_probe(name, params).map(Self::ProbeAudioMix),
            "start_recording" => decode_start_recording(name, params).map(Self::StartRecording),
            "stop_recording" => expect_no_params(name, params).map(|()| Self::StopRecording),
            "start_streaming" => decode_start_streaming(name, params).map(Self::StartStreaming),
            "stop_streaming" => expect_no_params(name, params).map(|()| Self::StopStreaming),
            _ => Err(CommandError::new(
                ErrorCode::UnknownCommand,
                format!("unknown command `{name}`"),
            )
            .with_details(json!({
                "command": name,
                "available": COMMANDS.iter().map(|d| d.name).collect::<Vec<_>>(),
            }))),
        }
    }

    pub fn descriptor(&self) -> &'static CommandDescriptor {
        match self {
            Self::GetStatus => &GET_STATUS,
            Self::ListCommands => &LIST_COMMANDS,
            Self::GetCaptureCapabilities => &GET_CAPTURE_CAPABILITIES,
            Self::ListAudioInputs => &LIST_AUDIO_INPUTS,
            Self::ProbeScreenCapture(_) => &PROBE_SCREEN_CAPTURE,
            Self::ProbeAudioCapture(_) => &PROBE_AUDIO_CAPTURE,
            Self::ProbeComposite(_) => &PROBE_COMPOSITE,
            Self::ProbeAudioMix(_) => &PROBE_AUDIO_MIX,
            Self::StartRecording(_) => &START_RECORDING,
            Self::StopRecording => &STOP_RECORDING,
            Self::StartStreaming(_) => &START_STREAMING,
            Self::StopStreaming => &STOP_STREAMING,
        }
    }

    pub fn name(&self) -> &'static str {
        self.descriptor().name
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScreenProbeParams {
    duration_ms: Option<u64>,
    source_types: Option<Vec<VideoSourceKind>>,
    cursor: Option<CursorMode>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioProbeParams {
    input: Option<String>,
    duration_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MixProbeParams {
    duration_ms: Option<u64>,
    #[serde(default)]
    inputs: Vec<LiveInputParams>,
    #[serde(default)]
    tones: Vec<ToneParams>,
    master_gain_db: Option<f32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveInputParams {
    input: Option<String>,
    gain_db: Option<f32>,
    mute: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToneParams {
    frequency_hz: f64,
    level_dbfs: Option<f64>,
    gain_db: Option<f32>,
    mute: Option<bool>,
}

const DEFAULT_TONE_LEVEL_DBFS: f64 = -12.0;
const DEFAULT_TONE_HZ: f64 = 1000.0;

fn decode_mix_probe(command: &str, params: Option<&Value>) -> Result<MixRequest, CommandError> {
    let mut params: MixProbeParams = decode_params(command, params)?;
    if params.inputs.is_empty() && params.tones.is_empty() {
        // Zero-config diagnostic: one test tone, no device access.
        params.tones.push(ToneParams {
            frequency_hz: DEFAULT_TONE_HZ,
            level_dbfs: None,
            gain_db: None,
            mute: None,
        });
    }
    let sources = mix_sources(command, params.inputs, params.tones, params.master_gain_db)?;
    Ok(MixRequest {
        duration: duration_within(command, params.duration_ms, MAX_MIX_DURATION)?,
        inputs: sources.inputs,
        tones: sources.tones,
        master_gain_db: sources.master_gain_db,
    })
}

struct MixSources {
    inputs: Vec<LiveInputSpec>,
    tones: Vec<ToneSpec>,
    master_gain_db: f32,
}

/// Validate live inputs, tones and gains shared by mixing commands.
fn mix_sources(
    command: &str,
    inputs: Vec<LiveInputParams>,
    tones: Vec<ToneParams>,
    master_gain_db: Option<f32>,
) -> Result<MixSources, CommandError> {
    let sources = inputs.len() + tones.len();
    if sources > MAX_INPUTS {
        return Err(invalid_params(
            command,
            format!("at most {MAX_INPUTS} sources in `inputs` and `tones`, got {sources}"),
        ));
    }
    let gain = |name: &str, db: Option<f32>| {
        let db = db.unwrap_or(0.0);
        if db.is_finite() && (MIN_GAIN_DB..=MAX_GAIN_DB).contains(&db) {
            Ok(db)
        } else {
            Err(invalid_params(
                command,
                format!("`{name}` must be between {MIN_GAIN_DB} and {MAX_GAIN_DB} dB"),
            ))
        }
    };
    let mut live = Vec::with_capacity(inputs.len());
    for input in inputs {
        if input.input.as_deref().is_some_and(str::is_empty) {
            return Err(invalid_params(command, "`input` must not be empty"));
        }
        live.push(LiveInputSpec {
            input: input.input,
            gain_db: gain("gain_db", input.gain_db)?,
            muted: input.mute.unwrap_or(false),
        });
    }
    let nyquist = f64::from(audio_engine::RATE) / 2.0;
    let mut generated = Vec::with_capacity(tones.len());
    for tone in tones {
        if !(tone.frequency_hz.is_finite()
            && tone.frequency_hz > 0.0
            && tone.frequency_hz < nyquist)
        {
            return Err(invalid_params(
                command,
                format!("`frequency_hz` must be between 0 and {nyquist} (exclusive)"),
            ));
        }
        let level = tone.level_dbfs.unwrap_or(DEFAULT_TONE_LEVEL_DBFS);
        if !(level.is_finite() && (f64::from(MIN_GAIN_DB)..=0.0).contains(&level)) {
            return Err(invalid_params(
                command,
                format!("`level_dbfs` must be between {MIN_GAIN_DB} and 0"),
            ));
        }
        generated.push(ToneSpec {
            frequency_hz: tone.frequency_hz,
            level_dbfs: level,
            gain_db: gain("gain_db", tone.gain_db)?,
            muted: tone.mute.unwrap_or(false),
        });
    }
    Ok(MixSources {
        inputs: live,
        tones: generated,
        master_gain_db: gain("master_gain_db", master_gain_db)?,
    })
}

/// Pipeline source fields shared by the output commands.
struct SourceParams {
    screen: Option<bool>,
    source_types: Option<Vec<VideoSourceKind>>,
    cursor: Option<CursorMode>,
    inputs: Option<Vec<LiveInputParams>>,
    /// Absent: one quiet test tone if there are no `inputs`; `[]`: no tones.
    tones: Option<Vec<ToneParams>>,
    master_gain_db: Option<f32>,
}

const DEFAULT_OUTPUT_TONE_DBFS: f64 = -20.0;

/// `None` if no source field is present: the output joins the running
/// pipeline, or the engine starts one with [`MediaSources::default`]-like
/// defaults (see `default_sources`).
fn output_sources(
    command: &str,
    params: SourceParams,
) -> Result<Option<MediaSources>, CommandError> {
    let given = params.screen.is_some()
        || params.source_types.is_some()
        || params.cursor.is_some()
        || params.inputs.is_some()
        || params.tones.is_some()
        || params.master_gain_db.is_some();
    if !given {
        return Ok(None);
    }
    let inputs = params.inputs.unwrap_or_default();
    // Without any source, a quiet tone keeps the audio track audibly alive;
    // with live inputs, only they are mixed.
    let tones = match params.tones {
        Some(tones) => tones,
        None if inputs.is_empty() => vec![default_tone()],
        None => Vec::new(),
    };
    let sources = mix_sources(command, inputs, tones, params.master_gain_db)?;
    Ok(Some(MediaSources {
        screen: optional_screen(command, params.screen, params.source_types, params.cursor)?,
        inputs: sources.inputs,
        tones: sources.tones,
        master_gain_db: sources.master_gain_db,
    }))
}

fn default_tone() -> ToneParams {
    ToneParams {
        frequency_hz: DEFAULT_TONE_HZ,
        level_dbfs: Some(DEFAULT_OUTPUT_TONE_DBFS),
        gain_db: None,
        mute: None,
    }
}

/// Sources of an output started without any: one quiet test tone.
pub fn default_sources() -> MediaSources {
    MediaSources {
        screen: None,
        inputs: Vec::new(),
        tones: vec![ToneSpec {
            frequency_hz: DEFAULT_TONE_HZ,
            level_dbfs: DEFAULT_OUTPUT_TONE_DBFS,
            gain_db: 0.0,
            muted: false,
        }],
        master_gain_db: 0.0,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartRecordingParams {
    file_name: String,
    screen: Option<bool>,
    source_types: Option<Vec<VideoSourceKind>>,
    cursor: Option<CursorMode>,
    inputs: Option<Vec<LiveInputParams>>,
    tones: Option<Vec<ToneParams>>,
    master_gain_db: Option<f32>,
}

fn decode_start_recording(
    command: &str,
    params: Option<&Value>,
) -> Result<RecordingRequest, CommandError> {
    let params: StartRecordingParams = decode_params(command, params)?;
    Ok(RecordingRequest {
        file_name: output_file(command, "file_name", params.file_name, "mkv")?,
        sources: output_sources(
            command,
            SourceParams {
                screen: params.screen,
                source_types: params.source_types,
                cursor: params.cursor,
                inputs: params.inputs,
                tones: params.tones,
                master_gain_db: params.master_gain_db,
            },
        )?,
    })
}

const DEFAULT_VIDEO_BITRATE_KBPS: u32 = 6_000;
const VIDEO_BITRATE_KBPS: std::ops::RangeInclusive<u32> = 300..=50_000;
const MAX_RECONNECT_ATTEMPTS: u32 = 100;
const RECONNECT_DELAY_MS: std::ops::RangeInclusive<u64> = 100..=60_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartStreamingParams {
    url: String,
    /// Raw JSON so a wrongly typed key is rejected without serde quoting
    /// its value in the error message.
    stream_key: Option<Value>,
    video_bitrate_kbps: Option<u32>,
    reconnect_attempts: Option<u32>,
    reconnect_delay_ms: Option<u64>,
    screen: Option<bool>,
    source_types: Option<Vec<VideoSourceKind>>,
    cursor: Option<CursorMode>,
    inputs: Option<Vec<LiveInputParams>>,
    tones: Option<Vec<ToneParams>>,
    master_gain_db: Option<f32>,
}

fn decode_start_streaming(
    command: &str,
    params: Option<&Value>,
) -> Result<StreamingRequest, CommandError> {
    let params: StartStreamingParams = decode_params(command, params)?;
    // The key moves into a redacting type right away; validation messages
    // never include it.
    let key = match params.stream_key {
        None | Some(Value::Null) => StreamKey::default(),
        Some(Value::String(key)) => StreamKey::new(key),
        Some(_) => return Err(invalid_params(command, "`stream_key` must be a string")),
    };
    let target =
        StreamTarget::new(&params.url, key).map_err(|message| invalid_params(command, message))?;
    let video_bitrate_kbps = params
        .video_bitrate_kbps
        .unwrap_or(DEFAULT_VIDEO_BITRATE_KBPS);
    if !VIDEO_BITRATE_KBPS.contains(&video_bitrate_kbps) {
        return Err(invalid_params(
            command,
            format!(
                "`video_bitrate_kbps` must be between {} and {}",
                VIDEO_BITRATE_KBPS.start(),
                VIDEO_BITRATE_KBPS.end()
            ),
        ));
    }
    let defaults = ReconnectPolicy::default();
    let max_attempts = params.reconnect_attempts.unwrap_or(defaults.max_attempts);
    if max_attempts > MAX_RECONNECT_ATTEMPTS {
        return Err(invalid_params(
            command,
            format!("`reconnect_attempts` must be at most {MAX_RECONNECT_ATTEMPTS}"),
        ));
    }
    let initial_delay = match params.reconnect_delay_ms {
        None => defaults.initial_delay,
        Some(ms) if RECONNECT_DELAY_MS.contains(&ms) => Duration::from_millis(ms),
        Some(_) => {
            return Err(invalid_params(
                command,
                format!(
                    "`reconnect_delay_ms` must be between {} and {}",
                    RECONNECT_DELAY_MS.start(),
                    RECONNECT_DELAY_MS.end()
                ),
            ));
        }
    };
    Ok(StreamingRequest {
        target,
        video_bitrate_kbps,
        reconnect: ReconnectPolicy {
            max_attempts,
            initial_delay,
            max_delay: defaults.max_delay.max(initial_delay),
        },
        sources: output_sources(
            command,
            SourceParams {
                screen: params.screen,
                source_types: params.source_types,
                cursor: params.cursor,
                inputs: params.inputs,
                tones: params.tones,
                master_gain_db: params.master_gain_db,
            },
        )?,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompositeProbeParams {
    duration_ms: Option<u64>,
    screen: Option<bool>,
    source_types: Option<Vec<VideoSourceKind>>,
    cursor: Option<CursorMode>,
    snapshot_path: Option<String>,
}

fn decode_composite_probe(
    command: &str,
    params: Option<&Value>,
) -> Result<CompositeRequest, CommandError> {
    let params: CompositeProbeParams = decode_params(command, params)?;
    let screen = optional_screen(command, params.screen, params.source_types, params.cursor)?;
    Ok(CompositeRequest {
        duration: probe_duration(command, params.duration_ms)?,
        screen,
        snapshot_path: params
            .snapshot_path
            .map(|path| output_file(command, "snapshot_path", path, "ppm"))
            .transpose()?,
    })
}

/// `Some(selection)` with `screen: true`; selection fields without it are an error.
fn optional_screen(
    command: &str,
    screen: Option<bool>,
    source_types: Option<Vec<VideoSourceKind>>,
    cursor: Option<CursorMode>,
) -> Result<Option<ScreenSelection>, CommandError> {
    if screen.unwrap_or(false) {
        return screen_selection(command, source_types, cursor).map(Some);
    }
    if source_types.is_some() || cursor.is_some() {
        return Err(invalid_params(
            command,
            "`source_types` and `cursor` require `screen: true`",
        ));
    }
    Ok(None)
}

/// Files a command creates are plain file names with a fixed extension. The
/// engine writes them into its own output directory and never overwrites, so
/// a remote client cannot choose where files land.
fn output_file(
    command: &str,
    field: &str,
    path: String,
    extension: &str,
) -> Result<PathBuf, CommandError> {
    let path = PathBuf::from(path);
    let mut components = path.components();
    let plain_name = matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    );
    let has_extension = path
        .extension()
        .is_some_and(|found| found.eq_ignore_ascii_case(extension));
    if !plain_name || !has_extension || path.file_stem().is_none() {
        return Err(invalid_params(
            command,
            format!(
                "`{field}` must be a plain file name ending in .{extension} \
                 (no directories); it is written to the engine's output directory"
            ),
        ));
    }
    Ok(path)
}

fn decode_screen_probe(
    command: &str,
    params: Option<&Value>,
) -> Result<ScreenCaptureRequest, CommandError> {
    let params: ScreenProbeParams = decode_params(command, params)?;
    Ok(ScreenCaptureRequest {
        selection: screen_selection(command, params.source_types, params.cursor)?,
        duration: probe_duration(command, params.duration_ms)?,
    })
}

/// Defaults, deduplication and validation shared by all screen commands.
fn screen_selection(
    command: &str,
    source_types: Option<Vec<VideoSourceKind>>,
    cursor: Option<CursorMode>,
) -> Result<ScreenSelection, CommandError> {
    let mut source_kinds = Vec::new();
    for kind in source_types.unwrap_or_else(|| DEFAULT_SOURCE_KINDS.to_vec()) {
        if !source_kinds.contains(&kind) {
            source_kinds.push(kind);
        }
    }
    if source_kinds.is_empty() {
        return Err(invalid_params(command, "`source_types` must not be empty"));
    }
    Ok(ScreenSelection {
        source_kinds,
        cursor: cursor.unwrap_or(DEFAULT_CURSOR),
    })
}

fn decode_audio_probe(
    command: &str,
    params: Option<&Value>,
) -> Result<AudioCaptureRequest, CommandError> {
    let params: AudioProbeParams = decode_params(command, params)?;
    if params.input.as_deref().is_some_and(str::is_empty) {
        return Err(invalid_params(command, "`input` must not be empty"));
    }
    Ok(AudioCaptureRequest {
        input: params.input,
        duration: probe_duration(command, params.duration_ms)?,
    })
}

fn probe_duration(command: &str, duration_ms: Option<u64>) -> Result<Duration, CommandError> {
    duration_within(command, duration_ms, MAX_PROBE_DURATION)
}

fn duration_within(
    command: &str,
    duration_ms: Option<u64>,
    max: Duration,
) -> Result<Duration, CommandError> {
    let duration = duration_ms.map_or(DEFAULT_PROBE_DURATION, Duration::from_millis);
    if (MIN_PROBE_DURATION..=max).contains(&duration) {
        return Ok(duration);
    }
    Err(CommandError::new(
        ErrorCode::InvalidParams,
        format!(
            "`duration_ms` must be between {} and {}",
            MIN_PROBE_DURATION.as_millis(),
            MAX_PROBE_DURATION.as_millis()
        ),
    )
    .with_details(json!({
        "command": command,
        "min_ms": MIN_PROBE_DURATION.as_millis(),
        "max_ms": MAX_PROBE_DURATION.as_millis(),
    })))
}

fn decode_params<T: DeserializeOwned>(
    command: &str,
    params: Option<&Value>,
) -> Result<T, CommandError> {
    let object = match params {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(object @ Value::Object(_)) => object.clone(),
        Some(other) => return Err(not_an_object(command, other)),
    };
    serde_json::from_value(object)
        .map_err(|err| invalid_params(command, format!("invalid parameters: {err}")))
}

fn expect_no_params(command: &str, params: Option<&Value>) -> Result<(), CommandError> {
    match params {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Object(map)) if map.is_empty() => Ok(()),
        Some(Value::Object(map)) => Err(CommandError::new(
            ErrorCode::InvalidParams,
            format!("`{command}` takes no parameters"),
        )
        .with_details(json!({
            "command": command,
            "unexpected": map.keys().collect::<Vec<_>>(),
        }))),
        Some(other) => Err(not_an_object(command, other)),
    }
}

fn invalid_params(command: &str, message: impl Into<String>) -> CommandError {
    CommandError::new(ErrorCode::InvalidParams, message).with_details(json!({ "command": command }))
}

fn not_an_object(command: &str, value: &Value) -> CommandError {
    invalid_params(
        command,
        format!("`params` must be an object, got {}", json_type(value)),
    )
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn registry_names_are_unique_snake_case() {
        let mut seen = HashSet::new();
        for descriptor in registry() {
            assert!(
                seen.insert(descriptor.name),
                "duplicate {}",
                descriptor.name
            );
            assert!(
                descriptor
                    .name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_'),
                "{} is not snake_case",
                descriptor.name
            );
        }
    }

    /// Minimal valid params for commands with required parameters.
    pub(crate) fn sample_params(name: &str) -> Option<Value> {
        match name {
            "start_recording" => Some(json!({"file_name": "sample.mkv"})),
            "start_streaming" => Some(json!({"url": "rtmp://127.0.0.1:1/live"})),
            _ => None,
        }
    }

    #[test]
    fn every_registered_command_decodes_to_its_descriptor() {
        for descriptor in registry() {
            let command = Command::decode(descriptor.name, sample_params(descriptor.name).as_ref())
                .unwrap_or_else(|err| panic!("{} does not decode: {err}", descriptor.name));
            assert_eq!(command.descriptor(), descriptor);
        }
    }

    #[test]
    fn unknown_command_lists_available_commands() {
        let err = Command::decode("start_teleport", None).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnknownCommand);
        let details = err.details.expect("details");
        assert_eq!(details["command"], "start_teleport");
        let available = details["available"].as_array().expect("array");
        assert_eq!(available.len(), registry().len());
        assert!(available.contains(&json!("get_status")));
    }

    #[test]
    fn absent_null_and_empty_params_are_equivalent() {
        for params in [None, Some(&Value::Null), Some(&json!({}))] {
            assert_eq!(
                Command::decode("get_status", params),
                Ok(Command::GetStatus)
            );
        }
    }

    #[test]
    fn unexpected_params_are_rejected() {
        let err = Command::decode("get_status", Some(&json!({"verbose": true}))).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
        assert_eq!(
            err.details.expect("details")["unexpected"],
            json!(["verbose"])
        );
    }

    #[test]
    fn non_object_params_are_rejected() {
        let err = Command::decode("list_commands", Some(&json!([1]))).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
        assert!(err.message.contains("array"), "{}", err.message);

        let err = Command::decode("probe_screen_capture", Some(&json!("x"))).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
        assert!(err.message.contains("string"), "{}", err.message);
    }

    #[test]
    fn screen_probe_defaults() {
        assert_eq!(
            Command::decode("probe_screen_capture", None),
            Ok(Command::ProbeScreenCapture(ScreenCaptureRequest {
                selection: ScreenSelection {
                    source_kinds: vec![VideoSourceKind::Monitor, VideoSourceKind::Window],
                    cursor: CursorMode::Embedded,
                },
                duration: Duration::from_secs(5),
            }))
        );
    }

    #[test]
    fn screen_probe_params_are_decoded_and_deduplicated() {
        let params = json!({
            "duration_ms": 1500,
            "source_types": ["window", "window", "virtual"],
            "cursor": "hidden",
        });
        assert_eq!(
            Command::decode("probe_screen_capture", Some(&params)),
            Ok(Command::ProbeScreenCapture(ScreenCaptureRequest {
                selection: ScreenSelection {
                    source_kinds: vec![VideoSourceKind::Window, VideoSourceKind::Virtual],
                    cursor: CursorMode::Hidden,
                },
                duration: Duration::from_millis(1500),
            }))
        );
    }

    #[test]
    fn invalid_screen_probe_params_are_rejected() {
        for params in [
            json!({"duration_ms": 50}),
            json!({"duration_ms": 60_001}),
            json!({"duration_ms": -1}),
            json!({"source_types": []}),
            json!({"source_types": ["printer"]}),
            json!({"cursor": "sparkly"}),
            json!({"seconds": 5}),
        ] {
            let err = Command::decode("probe_screen_capture", Some(&params)).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidParams, "{params}");
            assert_eq!(
                err.details.expect("details")["command"],
                "probe_screen_capture"
            );
        }
    }

    #[test]
    fn duration_error_reports_bounds() {
        let err =
            Command::decode("probe_audio_capture", Some(&json!({"duration_ms": 1}))).unwrap_err();
        let details = err.details.expect("details");
        assert_eq!(details["min_ms"], 100);
        assert_eq!(details["max_ms"], 60_000);
    }

    #[test]
    fn audio_probe_params() {
        assert_eq!(
            Command::decode("probe_audio_capture", None),
            Ok(Command::ProbeAudioCapture(AudioCaptureRequest {
                input: None,
                duration: Duration::from_secs(5),
            }))
        );
        assert_eq!(
            Command::decode(
                "probe_audio_capture",
                Some(&json!({"input": "alsa_input.usb", "duration_ms": 100}))
            ),
            Ok(Command::ProbeAudioCapture(AudioCaptureRequest {
                input: Some("alsa_input.usb".into()),
                duration: Duration::from_millis(100),
            }))
        );
        let err = Command::decode("probe_audio_capture", Some(&json!({"input": ""}))).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
    }

    #[test]
    fn composite_probe_defaults_to_test_pattern_only() {
        assert_eq!(
            Command::decode("probe_composite", None),
            Ok(Command::ProbeComposite(CompositeRequest {
                duration: Duration::from_secs(5),
                screen: None,
                snapshot_path: None,
            }))
        );
    }

    #[test]
    fn composite_probe_with_screen_and_snapshot() {
        let params = json!({
            "duration_ms": 2000,
            "screen": true,
            "source_types": ["window"],
            "snapshot_path": "frame.PPM",
        });
        assert_eq!(
            Command::decode("probe_composite", Some(&params)),
            Ok(Command::ProbeComposite(CompositeRequest {
                duration: Duration::from_secs(2),
                screen: Some(ScreenSelection {
                    source_kinds: vec![VideoSourceKind::Window],
                    cursor: CursorMode::Embedded,
                }),
                snapshot_path: Some(PathBuf::from("frame.PPM")),
            }))
        );
    }

    #[test]
    fn invalid_composite_params_are_rejected() {
        for params in [
            json!({"source_types": ["monitor"]}),
            json!({"cursor": "hidden"}),
            json!({"snapshot_path": "frame.png"}),
            json!({"snapshot_path": "/tmp/frame.ppm"}),
            json!({"snapshot_path": "../frame.ppm"}),
            json!({"snapshot_path": "out/frame.ppm"}),
            json!({"snapshot_path": "./frame.ppm"}),
            json!({"snapshot_path": ".ppm"}),
            json!({"snapshot_path": ""}),
            json!({"screen": "yes"}),
            json!({"duration_ms": 10}),
        ] {
            let err = Command::decode("probe_composite", Some(&params)).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidParams, "{params}");
        }
    }

    #[test]
    fn mix_probe_params_are_decoded_with_defaults() {
        let params = json!({
            "duration_ms": 600_000,
            "inputs": [{"input": "mic", "gain_db": -6.0}, {}],
            "tones": [{"frequency_hz": 440.0}],
        });
        assert_eq!(
            Command::decode("probe_audio_mix", Some(&params)),
            Ok(Command::ProbeAudioMix(MixRequest {
                duration: Duration::from_secs(600),
                inputs: vec![
                    LiveInputSpec {
                        input: Some("mic".into()),
                        gain_db: -6.0,
                        muted: false,
                    },
                    LiveInputSpec {
                        input: None,
                        gain_db: 0.0,
                        muted: false,
                    },
                ],
                tones: vec![ToneSpec {
                    frequency_hz: 440.0,
                    level_dbfs: -12.0,
                    gain_db: 0.0,
                    muted: false,
                }],
                master_gain_db: 0.0,
            }))
        );
    }

    #[test]
    fn mix_probe_without_sources_uses_one_test_tone() {
        let Ok(Command::ProbeAudioMix(request)) = Command::decode("probe_audio_mix", None) else {
            panic!("decode");
        };
        assert!(request.inputs.is_empty());
        assert_eq!(request.tones.len(), 1);
        assert_eq!(request.tones[0].frequency_hz, 1000.0);
    }

    #[test]
    fn invalid_mix_params_are_rejected() {
        let nine_tones: Vec<_> = (0..9).map(|_| json!({"frequency_hz": 100.0})).collect();
        for params in [
            json!({"tones": nine_tones}),
            json!({"tones": [{"frequency_hz": 0.0}]}),
            json!({"tones": [{"frequency_hz": 24_000.0}]}),
            json!({"tones": [{"frequency_hz": 440.0, "level_dbfs": 3.0}]}),
            json!({"tones": [{"frequency_hz": 440.0}], "master_gain_db": 30.0}),
            json!({"inputs": [{"input": "", "gain_db": 0.0}]}),
            json!({"inputs": [{"gain_db": -200.0}]}),
            json!({"inputs": [{"volume": 1}]}),
            json!({"tones": [{"frequency_hz": 440.0}], "duration_ms": 900_001}),
        ] {
            let err = Command::decode("probe_audio_mix", Some(&params)).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidParams, "{params}");
        }
    }

    #[test]
    fn start_recording_params() {
        let Ok(Command::StartRecording(request)) = Command::decode(
            "start_recording",
            Some(&json!({"file_name": "take1.MKV", "inputs": [{"input": "mic"}]})),
        ) else {
            panic!("decode");
        };
        assert_eq!(request.file_name, PathBuf::from("take1.MKV"));
        let sources = request.sources.expect("sources given");
        assert_eq!(sources.screen, None);
        assert_eq!(sources.inputs.len(), 1);
        // With live inputs there is no default tone.
        assert!(sources.tones.is_empty());
        // No source field at all: join the pipeline or use the defaults.
        let Ok(Command::StartRecording(no_sources)) =
            Command::decode("start_recording", Some(&json!({"file_name": "a.mkv"})))
        else {
            panic!("decode");
        };
        assert_eq!(no_sources.sources, None);
        let defaults = default_sources();
        assert_eq!(defaults.tones.len(), 1);
        assert_eq!(defaults.tones[0].level_dbfs, -20.0);
        // Any source field counts: here only the master gain, so one tone.
        let Ok(Command::StartRecording(gain_only)) = Command::decode(
            "start_recording",
            Some(&json!({"file_name": "a.mkv", "master_gain_db": -3.0})),
        ) else {
            panic!("decode");
        };
        let gain_only = gain_only.sources.expect("sources given");
        assert_eq!((gain_only.tones.len(), gain_only.master_gain_db), (1, -3.0));
        // Eight live inputs fit; the default tone does not count against them.
        let eight: Vec<_> = (0..8).map(|i| json!({"input": format!("in{i}")})).collect();
        assert!(
            Command::decode(
                "start_recording",
                Some(&json!({"file_name": "a.mkv", "inputs": eight}))
            )
            .is_ok()
        );
        let Ok(Command::StartRecording(silent)) = Command::decode(
            "start_recording",
            Some(&json!({"file_name": "a.mkv", "tones": []})),
        ) else {
            panic!("decode");
        };
        let silent = silent.sources.expect("sources given");
        assert!(silent.tones.is_empty() && silent.inputs.is_empty());
    }

    #[test]
    fn start_streaming_params() {
        let Ok(Command::StartStreaming(request)) = Command::decode(
            "start_streaming",
            Some(&json!({
                "url": "rtmp://live.example.com/app",
                "stream_key": "sk_SECRET",
                "video_bitrate_kbps": 4500,
                "reconnect_attempts": 0,
                "reconnect_delay_ms": 250,
            })),
        ) else {
            panic!("decode");
        };
        assert_eq!(request.video_bitrate_kbps, 4500);
        assert_eq!(request.reconnect.max_attempts, 0);
        assert_eq!(request.reconnect.initial_delay, Duration::from_millis(250));
        assert_eq!(request.sources, None);
        assert_eq!(
            request.target.redacted(),
            "rtmp://live.example.com/app/<stream key>"
        );
        // The key never shows in debug output of the command.
        let command = Command::StartStreaming(request);
        assert!(!format!("{command:?}").contains("SECRET"));
        let Ok(Command::StartStreaming(defaults)) =
            Command::decode("start_streaming", Some(&json!({"url": "rtmp://host/app"})))
        else {
            panic!("decode");
        };
        assert_eq!(defaults.video_bitrate_kbps, 6000);
        assert_eq!(defaults.reconnect, ReconnectPolicy::default());
    }

    #[test]
    fn invalid_start_streaming_params_never_echo_the_key() {
        for params in [
            json!({}),
            json!({"url": "http://host/app", "stream_key": "SECRET"}),
            json!({"url": "rtmp://user:pw@host/app", "stream_key": "SECRET"}),
            json!({"url": "rtmp://host/app", "stream_key": "bad/SECRET"}),
            json!({"url": "rtmp://host/app", "video_bitrate_kbps": 10}),
            json!({"url": "rtmp://host/app", "reconnect_attempts": 1000}),
            json!({"url": "rtmp://host/app", "reconnect_delay_ms": 5}),
            json!({"url": "rtmp://host/app", "cursor": "hidden"}),
            json!({"url": "rtmp://host/app", "key": "SECRET"}),
            json!({"url": "rtmp://host/app", "stream_key": 12345678}),
            json!({"url": "rtmp://host/app", "stream_key": ["SECRET"]}),
        ] {
            let err = Command::decode("start_streaming", Some(&params)).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidParams, "{params}");
            let shown = format!("{err} {:?}", err.details);
            assert!(
                !shown.contains("SECRET") && !shown.contains("12345678"),
                "{shown}"
            );
        }
        assert!(Command::decode("stop_streaming", Some(&json!({"now": true}))).is_err());
    }

    #[test]
    fn invalid_start_recording_params_are_rejected() {
        for params in [
            json!({}),
            json!({"file_name": "a.mp4"}),
            json!({"file_name": "../a.mkv"}),
            json!({"file_name": "/tmp/a.mkv"}),
            json!({"file_name": "a.mkv", "cursor": "hidden"}),
            json!({"file_name": "a.mkv", "tones": [{"frequency_hz": 0.0}]}),
        ] {
            let err = Command::decode("start_recording", Some(&params)).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidParams, "{params}");
        }
        assert!(Command::decode("stop_recording", Some(&json!({"now": true}))).is_err());
    }

    #[test]
    fn access_classes_match_risk() {
        let access = |name| registry().iter().find(|d| d.name == name).map(|d| d.access);
        assert_eq!(
            access("get_capture_capabilities"),
            Some(CommandAccess::ReadOnly)
        );
        assert_eq!(access("list_audio_inputs"), Some(CommandAccess::ReadOnly));
        assert_eq!(
            access("probe_screen_capture"),
            Some(CommandAccess::Sensitive)
        );
        assert_eq!(
            access("probe_audio_capture"),
            Some(CommandAccess::Sensitive)
        );
        assert_eq!(access("probe_composite"), Some(CommandAccess::Sensitive));
        assert_eq!(access("probe_audio_mix"), Some(CommandAccess::Sensitive));
        assert_eq!(access("start_recording"), Some(CommandAccess::Sensitive));
        assert_eq!(access("stop_recording"), Some(CommandAccess::Control));
        assert_eq!(access("start_streaming"), Some(CommandAccess::Sensitive));
        assert_eq!(access("stop_streaming"), Some(CommandAccess::Control));
    }

    #[test]
    fn command_list_serializes_with_access_class() {
        let value = serde_json::to_value(CommandList::all()).expect("serialize");
        assert_eq!(
            value["commands"][0],
            json!({
                "name": "get_status",
                "summary": "Report engine status.",
                "access": "read_only",
            })
        );
    }
}
