//! Command registry: wire names, metadata, and parameter decoding.

use std::path::PathBuf;
use std::time::Duration;

use capture_api::{
    AudioCaptureRequest, CursorMode, DEFAULT_PROBE_DURATION, MAX_PROBE_DURATION,
    MIN_PROBE_DURATION, ScreenCaptureRequest, ScreenSelection, VideoSourceKind,
};
use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use audio_engine::{MAX_GAIN_DB, MAX_INPUTS, MIN_GAIN_DB};
use encoder_api::{ReconnectPolicy, StreamKey, StreamTarget};

use crate::{
    AddSourceRequest, AudioSetup, Command, CommandError, CompositeRequest, ErrorCode,
    LiveInputSpec, MAX_MIX_DURATION, MAX_NAME_CHARS, MediaSources, MixRequest, RecordingRequest,
    Rgba, SourceKind, SourceRect, StreamingRequest, StripChange, ToneSpec, UpdateSourceRequest,
};

/// Permission class of a command.
///
/// Adapters must require explicit authorization for anything other than
/// [`CommandAccess::ReadOnly`] (see [`crate::AccessPolicy`]). Levels are
/// ordered: read-only < control < sensitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandAccess {
    /// Inspects state without changing it.
    ReadOnly,
    /// Changes engine state, e.g. starts or stops an output.
    Control,
    /// Destructive, credential-related, or privacy-sensitive (e.g. microphone access).
    Sensitive,
}

impl std::str::FromStr for CommandAccess {
    type Err = String;

    /// `read-only` (or `read_only`), `control` or `sensitive`.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "read-only" | "read_only" => Ok(Self::ReadOnly),
            "control" => Ok(Self::Control),
            "sensitive" => Ok(Self::Sensitive),
            _ => Err(format!(
                "`{text}` is not an access level; use read-only, control or sensitive"
            )),
        }
    }
}

/// Static metadata for one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CommandDescriptor {
    /// Stable `snake_case` wire name.
    pub name: &'static str,
    pub summary: &'static str,
    pub access: CommandAccess,
    /// May overwrite, remove or end something (a scene, a gain, a running
    /// output), as opposed to only adding or inspecting.
    pub destructive: bool,
    /// Talks to systems outside this machine (e.g. a streaming server).
    pub open_world: bool,
}

impl CommandDescriptor {
    /// The lowest access any call of this command can need (see
    /// [`Command::required_access`]): `add_source` of color bars or a solid
    /// color needs only control.
    pub fn least_access(&self) -> CommandAccess {
        if self.name == "add_source" {
            CommandAccess::Control
        } else {
            self.access
        }
    }
}

const GET_STATUS: CommandDescriptor = CommandDescriptor {
    name: "get_status",
    summary: "Report engine status.",
    access: CommandAccess::ReadOnly,
    destructive: false,
    open_world: false,
};

const LIST_COMMANDS: CommandDescriptor = CommandDescriptor {
    name: "list_commands",
    summary: "List commands supported by this engine.",
    access: CommandAccess::ReadOnly,
    destructive: false,
    open_world: false,
};

const GET_CAPTURE_CAPABILITIES: CommandDescriptor = CommandDescriptor {
    name: "get_capture_capabilities",
    summary: "Query screen and audio capture capabilities of the running system.",
    access: CommandAccess::ReadOnly,
    destructive: false,
    open_world: false,
};

const LIST_AUDIO_INPUTS: CommandDescriptor = CommandDescriptor {
    name: "list_audio_inputs",
    summary: "List audio input devices.",
    access: CommandAccess::ReadOnly,
    destructive: false,
    open_world: false,
};

const PROBE_SCREEN_CAPTURE: CommandDescriptor = CommandDescriptor {
    name: "probe_screen_capture",
    summary: "Capture a user-selected screen or window and report format, timestamps, FPS and drops. \
              Params: duration_ms (100..=60000, default 5000), source_types ([\"monitor\",\"window\",\"virtual\"], \
              default [\"monitor\",\"window\"]), cursor (\"hidden\"|\"embedded\"|\"metadata\", default \"embedded\").",
    access: CommandAccess::Sensitive,
    destructive: false,
    open_world: false,
};

const PROBE_AUDIO_CAPTURE: CommandDescriptor = CommandDescriptor {
    name: "probe_audio_capture",
    summary: "Capture an audio input and report format, levels, timestamps and drops; no audio is stored. \
              Params: input (name from list_audio_inputs, default: system default), duration_ms (100..=60000, default 5000).",
    access: CommandAccess::Sensitive,
    destructive: false,
    open_world: false,
};

const PROBE_COMPOSITE: CommandDescriptor = CommandDescriptor {
    name: "probe_composite",
    summary: "Composite a 1920x1080@60 test scene (color bars, moving overlay, optional live screen) \
              and report render time, late frames and source drops. Params: duration_ms (100..=60000, \
              default 5000), screen (bool, default false; opens the system picker), source_types and \
              cursor (only with screen), snapshot_path (plain .ppm file name, written to the engine's output \
              directory, never overwritten).",
    access: CommandAccess::Sensitive,
    destructive: false,
    open_world: false,
};

const PROBE_AUDIO_MIX: CommandDescriptor = CommandDescriptor {
    name: "probe_audio_mix",
    summary: "Mix live audio inputs and test tones at 48 kHz stereo and report meters, buffer \
              latency over time, underruns and clipping; no audio is stored. Params: duration_ms \
              (100..=900000, default 5000), inputs ([{input, gain_db, mute}]), tones \
              ([{frequency_hz, level_dbfs, gain_db, mute}]), master_gain_db; up to 8 sources, \
              default one 1 kHz test tone.",
    access: CommandAccess::Sensitive,
    destructive: false,
    open_world: false,
};

const START_RECORDING: CommandDescriptor = CommandDescriptor {
    name: "start_recording",
    summary: "Record the media pipeline (the active scene at 1920x1080@60 and the audio mix) to \
              H.264/AAC in Matroska. Params: file_name (required, plain .mkv name in the engine's \
              output directory, never overwritten); optional sources (screen, source_types, \
              cursor, inputs, tones, master_gain_db) only while no preview or output runs: they \
              replace the configured audio (set_audio_sources, which may include microphones) \
              for this run and add a screen layer. Returns immediately; recording continues \
              until stop_recording.",
    access: CommandAccess::Sensitive,
    destructive: false,
    open_world: false,
};

const STOP_RECORDING: CommandDescriptor = CommandDescriptor {
    name: "stop_recording",
    summary: "Stop the running recording, finalize the file and report its statistics.",
    access: CommandAccess::Control,
    destructive: true,
    open_world: false,
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
    destructive: false,
    open_world: true,
};

const STOP_STREAMING: CommandDescriptor = CommandDescriptor {
    name: "stop_streaming",
    summary: "Stop the live stream, disconnect and report its connection history.",
    access: CommandAccess::Control,
    destructive: true,
    open_world: false,
};

const LIST_SCENES: CommandDescriptor = CommandDescriptor {
    name: "list_scenes",
    summary: "List the scenes with their sources, the active scene and the canvas size.",
    access: CommandAccess::ReadOnly,
    destructive: false,
    open_world: false,
};

const CREATE_SCENE: CommandDescriptor = CommandDescriptor {
    name: "create_scene",
    summary: "Create an empty scene. Params: name (1-64 characters, unique). Returns the scene list.",
    access: CommandAccess::Control,
    destructive: false,
    open_world: false,
};

const DELETE_SCENE: CommandDescriptor = CommandDescriptor {
    name: "delete_scene",
    summary: "Delete a scene that is not active; irreversible. Params: name, confirm (must be \
              true). Returns the scene list.",
    access: CommandAccess::Sensitive,
    destructive: true,
    open_world: false,
};

const SET_ACTIVE_SCENE: CommandDescriptor = CommandDescriptor {
    name: "set_active_scene",
    summary: "Render this scene from now on (applies live while preview or outputs run; its \
              screen sources start capturing, which opens the system picker). Params: name. \
              Returns the scene list.",
    access: CommandAccess::Sensitive,
    destructive: true,
    open_world: false,
};

const ADD_SOURCE: CommandDescriptor = CommandDescriptor {
    name: "add_source",
    summary: "Add a source on top of a scene. Params: scene, name, kind (color_bars, solid, \
              screen), color (#rrggbb[aa], solid), source_types and cursor (screen), x, y, width, \
              height (canvas pixels; all or none, default the whole canvas), opacity (0-1). Screen \
              sources start capturing (system picker) when the scene is rendered. Returns the \
              scene list.",
    access: CommandAccess::Sensitive,
    destructive: false,
    open_world: false,
};

const UPDATE_SOURCE: CommandDescriptor = CommandDescriptor {
    name: "update_source",
    summary: "Move, resize or fade a source. Params: scene, name, and any of x, y, width, height, \
              opacity. Returns the scene list.",
    access: CommandAccess::Control,
    destructive: true,
    open_world: false,
};

const REMOVE_SOURCE: CommandDescriptor = CommandDescriptor {
    name: "remove_source",
    summary: "Remove a source from a scene. Params: scene, name. Returns the scene list.",
    access: CommandAccess::Control,
    destructive: true,
    open_world: false,
};

const GET_MIXER: CommandDescriptor = CommandDescriptor {
    name: "get_mixer",
    summary: "Mixer strips with gain, mute and the peak level since the previous call.",
    access: CommandAccess::ReadOnly,
    destructive: false,
    open_world: false,
};

const SET_MIXER_STRIP: CommandDescriptor = CommandDescriptor {
    name: "set_mixer_strip",
    summary: "Change one strip (live if running). Params: index, gain_db (-96..24), mute. Returns \
              the mixer state.",
    access: CommandAccess::Control,
    destructive: true,
    open_world: false,
};

const SET_MASTER_GAIN: CommandDescriptor = CommandDescriptor {
    name: "set_master_gain",
    summary: "Change the master gain (live if running). Params: gain_db (-96..24). Returns the \
              mixer state.",
    access: CommandAccess::Control,
    destructive: true,
    open_world: false,
};

const SET_AUDIO_SOURCES: CommandDescriptor = CommandDescriptor {
    name: "set_audio_sources",
    summary: "Configure the pipeline's audio while it is idle. Params: inputs, tones, \
              master_gain_db (as for probe_audio_mix; one -20 dBFS tone if neither inputs nor \
              tones are given). Returns the mixer state.",
    access: CommandAccess::Sensitive,
    destructive: true,
    open_world: false,
};

const SET_PREVIEW: CommandDescriptor = CommandDescriptor {
    name: "set_preview",
    summary: "Run the pipeline for an in-process preview (quarter resolution, 30 FPS) or stop. \
              Params: enabled.",
    access: CommandAccess::Sensitive,
    destructive: false,
    open_world: false,
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
    LIST_SCENES,
    CREATE_SCENE,
    DELETE_SCENE,
    SET_ACTIVE_SCENE,
    ADD_SOURCE,
    UPDATE_SOURCE,
    REMOVE_SOURCE,
    GET_MIXER,
    SET_MIXER_STRIP,
    SET_MASTER_GAIN,
    SET_AUDIO_SOURCES,
    SET_PREVIEW,
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
            "list_scenes" => expect_no_params(name, params).map(|()| Self::ListScenes),
            "create_scene" => {
                decode_scene_name(name, params).map(|name| Self::CreateScene { name })
            }
            "delete_scene" => {
                decode_delete_scene(name, params).map(|name| Self::DeleteScene { name })
            }
            "set_active_scene" => {
                decode_scene_name(name, params).map(|name| Self::SetActiveScene { name })
            }
            "add_source" => decode_add_source(name, params).map(Self::AddSource),
            "update_source" => decode_update_source(name, params).map(Self::UpdateSource),
            "remove_source" => decode_remove_source(name, params)
                .map(|(scene, name)| Self::RemoveSource { scene, name }),
            "get_mixer" => expect_no_params(name, params).map(|()| Self::GetMixer),
            "set_mixer_strip" => decode_mixer_strip(name, params).map(Self::SetMixerStrip),
            "set_master_gain" => {
                decode_master_gain(name, params).map(|gain_db| Self::SetMasterGain { gain_db })
            }
            "set_audio_sources" => decode_audio_sources(name, params).map(Self::SetAudioSources),
            "set_preview" => {
                decode_preview(name, params).map(|enabled| Self::SetPreview { enabled })
            }
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

    /// Access this particular command needs: its descriptor's class, except
    /// `add_source` of a non-capturing source (color bars, solid), which is
    /// `control` (only screen sources capture private content).
    pub fn required_access(&self) -> CommandAccess {
        match self {
            // Listed explicitly: a new source kind needs the descriptor's
            // (sensitive) access until it is known to capture nothing.
            Self::AddSource(request)
                if matches!(
                    request.kind,
                    SourceKind::ColorBars | SourceKind::Solid { .. }
                ) =>
            {
                CommandAccess::Control
            }
            _ => self.descriptor().access,
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
            Self::ListScenes => &LIST_SCENES,
            Self::CreateScene { .. } => &CREATE_SCENE,
            Self::DeleteScene { .. } => &DELETE_SCENE,
            Self::SetActiveScene { .. } => &SET_ACTIVE_SCENE,
            Self::AddSource(_) => &ADD_SOURCE,
            Self::UpdateSource(_) => &UPDATE_SOURCE,
            Self::RemoveSource { .. } => &REMOVE_SOURCE,
            Self::GetMixer => &GET_MIXER,
            Self::SetMixerStrip(_) => &SET_MIXER_STRIP,
            Self::SetMasterGain { .. } => &SET_MASTER_GAIN,
            Self::SetAudioSources(_) => &SET_AUDIO_SOURCES,
            Self::SetPreview { .. } => &SET_PREVIEW,
        }
    }

    pub fn name(&self) -> &'static str {
        self.descriptor().name
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScreenProbeParams {
    /// Run time in milliseconds.
    duration_ms: Option<u64>,
    /// Screen source types to offer in the picker (monitor, window, virtual); default monitor and window.
    source_types: Option<Vec<VideoSourceKind>>,
    /// Cursor mode: hidden, embedded or metadata.
    cursor: Option<CursorMode>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AudioProbeParams {
    /// Audio input name from list_audio_inputs; null for the system default.
    input: Option<String>,
    /// Run time in milliseconds.
    duration_ms: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MixProbeParams {
    /// Run time in milliseconds.
    duration_ms: Option<u64>,
    /// Live audio inputs (names from list_audio_inputs).
    #[serde(default)]
    inputs: Vec<LiveInputParams>,
    /// Test tones.
    #[serde(default)]
    tones: Vec<ToneParams>,
    /// Master gain in dB, -96 to 24.
    master_gain_db: Option<f32>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct LiveInputParams {
    /// Audio input name from list_audio_inputs; null for the system default.
    input: Option<String>,
    /// Gain in dB, -96 to 24.
    gain_db: Option<f32>,
    /// Mute this strip.
    mute: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ToneParams {
    /// Tone frequency in Hz (below 24000).
    frequency_hz: f64,
    /// Tone level in dBFS, -96 to 0 (default -12).
    level_dbfs: Option<f64>,
    /// Gain in dB, -96 to 24.
    gain_db: Option<f32>,
    /// Mute this strip.
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

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StartRecordingParams {
    /// Plain .mkv file name, created in the engine's output directory; never overwrites.
    file_name: String,
    /// Add a live screen layer (opens the system picker).
    screen: Option<bool>,
    /// Screen source types to offer in the picker (monitor, window, virtual); default monitor and window.
    source_types: Option<Vec<VideoSourceKind>>,
    /// Cursor mode: hidden, embedded or metadata.
    cursor: Option<CursorMode>,
    /// Live audio inputs (names from list_audio_inputs).
    inputs: Option<Vec<LiveInputParams>>,
    /// Test tones.
    tones: Option<Vec<ToneParams>>,
    /// Master gain in dB, -96 to 24.
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

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StartStreamingParams {
    /// rtmp:// or rtmps:// server URL with host and application, without credentials or key.
    url: String,
    // Raw JSON so a wrongly typed key is rejected without serde quoting
    // its value in the error message.
    /// Secret stream key, appended to the URL when connecting; never echoed.
    #[schemars(with = "Option<String>")]
    stream_key: Option<Value>,
    /// Constant video bit rate in kbit/s, 300 to 50000 (default 6000).
    video_bitrate_kbps: Option<u32>,
    /// Reconnect attempts after a lost connection, 0 to 100 (default 5).
    reconnect_attempts: Option<u32>,
    /// Wait before the first reconnect in ms, doubling up to 30 s (default 1000).
    reconnect_delay_ms: Option<u64>,
    /// Add a live screen layer (opens the system picker).
    screen: Option<bool>,
    /// Screen source types to offer in the picker (monitor, window, virtual); default monitor and window.
    source_types: Option<Vec<VideoSourceKind>>,
    /// Cursor mode: hidden, embedded or metadata.
    cursor: Option<CursorMode>,
    /// Live audio inputs (names from list_audio_inputs).
    inputs: Option<Vec<LiveInputParams>>,
    /// Test tones.
    tones: Option<Vec<ToneParams>>,
    /// Master gain in dB, -96 to 24.
    master_gain_db: Option<f32>,
}

/// Stands in for an adapter's fixed target while its parameters decode.
const PLACEHOLDER_URL: &str = "rtmp://configured-target/app";

/// `start_streaming` to a target the adapter fixed (its configuration),
/// not one the client chooses: `url` and `stream_key` in `params` are
/// refused (`permission_denied`); everything else decodes and validates as
/// usual. Lets adapters keep credentials away from less trusted clients.
pub fn decode_streaming_to(
    target: StreamTarget,
    params: Option<&Value>,
) -> Result<Command, CommandError> {
    const COMMAND: &str = "start_streaming";
    let mut object = match params {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(object)) => object.clone(),
        Some(other) => return Err(not_an_object(COMMAND, other)),
    };
    if let Some(field) = ["url", "stream_key"]
        .into_iter()
        .find(|field| object.contains_key(*field))
    {
        return Err(CommandError::new(
            ErrorCode::PermissionDenied,
            format!("`{field}` is fixed by the configuration; streaming goes only to its target"),
        )
        .with_details(json!({ "command": COMMAND, "field": field })));
    }
    object.insert("url".into(), PLACEHOLDER_URL.into());
    let mut request = decode_start_streaming(COMMAND, Some(&Value::Object(object)))?;
    request.target = target;
    Ok(Command::StartStreaming(request))
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

/// Scene and source names: 1-64 characters, no control characters, no
/// surrounding whitespace.
fn valid_name(command: &str, field: &str, name: String) -> Result<String, CommandError> {
    let length = name.chars().count();
    if length == 0
        || length > MAX_NAME_CHARS
        || name.trim() != name
        || name.chars().any(char::is_control)
    {
        return Err(invalid_params(
            command,
            format!(
                "`{field}` must be 1-{MAX_NAME_CHARS} characters without control characters or \
                 surrounding whitespace"
            ),
        ));
    }
    Ok(name)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SceneNameParams {
    /// Scene name.
    name: String,
}

fn decode_scene_name(command: &str, params: Option<&Value>) -> Result<String, CommandError> {
    let params: SceneNameParams = decode_params(command, params)?;
    valid_name(command, "name", params.name)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DeleteSceneParams {
    /// Scene name.
    name: String,
    /// Must be true: deleting a scene cannot be undone.
    confirm: Option<bool>,
}

fn decode_delete_scene(command: &str, params: Option<&Value>) -> Result<String, CommandError> {
    let params: DeleteSceneParams = decode_params(command, params)?;
    if params.confirm != Some(true) {
        return Err(invalid_params(
            command,
            "deleting a scene cannot be undone; pass `confirm: true`",
        ));
    }
    valid_name(command, "name", params.name)
}

/// Positions and sizes in canvas pixels stay within this magnitude.
const MAX_COORDINATE: f32 = 16_384.0;

fn coordinate(command: &str, field: &str, value: f32, size: bool) -> Result<f32, CommandError> {
    let valid = value.is_finite()
        && if size {
            value > 0.0 && value <= MAX_COORDINATE
        } else {
            value.abs() <= MAX_COORDINATE
        };
    if !valid {
        let range = if size {
            format!("greater than 0 and at most {MAX_COORDINATE}")
        } else {
            format!("between -{MAX_COORDINATE} and {MAX_COORDINATE}")
        };
        return Err(invalid_params(
            command,
            format!("`{field}` must be {range}"),
        ));
    }
    Ok(value)
}

fn opacity(command: &str, value: f32) -> Result<f32, CommandError> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(invalid_params(command, "`opacity` must be between 0 and 1"))
    }
}

fn parse_color(command: &str, text: &str) -> Result<Rgba, CommandError> {
    let invalid = || invalid_params(command, "`color` must be #rrggbb or #rrggbbaa");
    let hex = text.strip_prefix('#').ok_or_else(invalid)?;
    if !(hex.len() == 6 || hex.len() == 8) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| invalid());
    let alpha = if hex.len() == 8 { byte(6)? } else { 255 };
    Ok(Rgba([byte(0)?, byte(2)?, byte(4)?, alpha]))
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddSourceParams {
    /// Scene name.
    scene: String,
    /// Source name, unique within the scene.
    name: String,
    /// Source kind: color_bars, solid or screen.
    #[schemars(extend("enum" = ["color_bars", "solid", "screen"]))]
    kind: String,
    /// Solid sources: #rrggbb or #rrggbbaa.
    color: Option<String>,
    /// Screen source types to offer in the picker (monitor, window, virtual); default monitor and window.
    source_types: Option<Vec<VideoSourceKind>>,
    /// Cursor mode: hidden, embedded or metadata.
    cursor: Option<CursorMode>,
    /// Left edge in canvas pixels.
    x: Option<f32>,
    /// Top edge in canvas pixels.
    y: Option<f32>,
    /// Width in canvas pixels.
    width: Option<f32>,
    /// Height in canvas pixels.
    height: Option<f32>,
    /// Opacity from 0 to 1.
    opacity: Option<f32>,
}

fn decode_add_source(
    command: &str,
    params: Option<&Value>,
) -> Result<AddSourceRequest, CommandError> {
    let params: AddSourceParams = decode_params(command, params)?;
    let screen_only = || {
        invalid_params(
            command,
            "`source_types` and `cursor` apply to screen sources only",
        )
    };
    let kind = match params.kind.as_str() {
        "color_bars" | "solid" if params.source_types.is_some() || params.cursor.is_some() => {
            return Err(screen_only());
        }
        "color_bars" if params.color.is_some() => {
            return Err(invalid_params(
                command,
                "`color` applies to solid sources only",
            ));
        }
        "color_bars" => SourceKind::ColorBars,
        "solid" => SourceKind::Solid {
            color: parse_color(
                command,
                params
                    .color
                    .as_deref()
                    .ok_or_else(|| invalid_params(command, "solid sources need a `color`"))?,
            )?,
        },
        "screen" if params.color.is_some() => {
            return Err(invalid_params(
                command,
                "`color` applies to solid sources only",
            ));
        }
        "screen" => {
            let selection = screen_selection(command, params.source_types, params.cursor)?;
            SourceKind::Screen {
                source_types: selection.source_kinds,
                cursor: selection.cursor,
            }
        }
        _ => {
            return Err(invalid_params(
                command,
                "`kind` must be color_bars, solid or screen",
            ));
        }
    };
    let rect = match (params.x, params.y, params.width, params.height) {
        (None, None, None, None) => None,
        (Some(x), Some(y), Some(width), Some(height)) => Some(SourceRect {
            x: coordinate(command, "x", x, false)?,
            y: coordinate(command, "y", y, false)?,
            width: coordinate(command, "width", width, true)?,
            height: coordinate(command, "height", height, true)?,
        }),
        _ => {
            return Err(invalid_params(
                command,
                "give all of x, y, width and height, or none (the whole canvas)",
            ));
        }
    };
    Ok(AddSourceRequest {
        scene: valid_name(command, "scene", params.scene)?,
        name: valid_name(command, "name", params.name)?,
        kind,
        rect,
        opacity: opacity(command, params.opacity.unwrap_or(1.0))?,
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpdateSourceParams {
    /// Scene name.
    scene: String,
    /// Source name.
    name: String,
    /// Left edge in canvas pixels.
    x: Option<f32>,
    /// Top edge in canvas pixels.
    y: Option<f32>,
    /// Width in canvas pixels.
    width: Option<f32>,
    /// Height in canvas pixels.
    height: Option<f32>,
    /// Opacity from 0 to 1.
    opacity: Option<f32>,
}

fn decode_update_source(
    command: &str,
    params: Option<&Value>,
) -> Result<UpdateSourceRequest, CommandError> {
    let params: UpdateSourceParams = decode_params(command, params)?;
    if [
        params.x,
        params.y,
        params.width,
        params.height,
        params.opacity,
    ]
    .iter()
    .all(Option::is_none)
    {
        return Err(invalid_params(
            command,
            "give at least one of x, y, width, height or opacity",
        ));
    }
    let check = |field: &str, value: Option<f32>, size: bool| {
        value
            .map(|value| coordinate(command, field, value, size))
            .transpose()
    };
    Ok(UpdateSourceRequest {
        scene: valid_name(command, "scene", params.scene)?,
        name: valid_name(command, "name", params.name)?,
        x: check("x", params.x, false)?,
        y: check("y", params.y, false)?,
        width: check("width", params.width, true)?,
        height: check("height", params.height, true)?,
        opacity: params.opacity.map(|o| opacity(command, o)).transpose()?,
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoveSourceParams {
    /// Scene name.
    scene: String,
    /// Source name.
    name: String,
}

fn decode_remove_source(
    command: &str,
    params: Option<&Value>,
) -> Result<(String, String), CommandError> {
    let params: RemoveSourceParams = decode_params(command, params)?;
    Ok((
        valid_name(command, "scene", params.scene)?,
        valid_name(command, "name", params.name)?,
    ))
}

fn gain_db(command: &str, field: &str, value: f32) -> Result<f32, CommandError> {
    if value.is_finite() && (MIN_GAIN_DB..=MAX_GAIN_DB).contains(&value) {
        Ok(value)
    } else {
        Err(invalid_params(
            command,
            format!("`{field}` must be between {MIN_GAIN_DB} and {MAX_GAIN_DB} dB"),
        ))
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MixerStripParams {
    /// Mixer strip index (from get_mixer).
    index: usize,
    /// Gain in dB, -96 to 24.
    gain_db: Option<f32>,
    /// Mute this strip.
    mute: Option<bool>,
}

fn decode_mixer_strip(command: &str, params: Option<&Value>) -> Result<StripChange, CommandError> {
    let params: MixerStripParams = decode_params(command, params)?;
    if params.gain_db.is_none() && params.mute.is_none() {
        return Err(invalid_params(command, "give `gain_db`, `mute` or both"));
    }
    Ok(StripChange {
        index: params.index,
        gain_db: params
            .gain_db
            .map(|value| gain_db(command, "gain_db", value))
            .transpose()?,
        muted: params.mute,
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MasterGainParams {
    /// Gain in dB, -96 to 24.
    gain_db: f32,
}

fn decode_master_gain(command: &str, params: Option<&Value>) -> Result<f32, CommandError> {
    let params: MasterGainParams = decode_params(command, params)?;
    gain_db(command, "gain_db", params.gain_db)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AudioSourcesParams {
    /// Live audio inputs (names from list_audio_inputs).
    #[serde(default)]
    inputs: Vec<LiveInputParams>,
    /// Test tones.
    tones: Option<Vec<ToneParams>>,
    /// Master gain in dB, -96 to 24.
    master_gain_db: Option<f32>,
}

fn decode_audio_sources(command: &str, params: Option<&Value>) -> Result<AudioSetup, CommandError> {
    let params: AudioSourcesParams = decode_params(command, params)?;
    let tones = match params.tones {
        Some(tones) => tones,
        None if params.inputs.is_empty() => vec![default_tone()],
        None => Vec::new(),
    };
    let sources = mix_sources(command, params.inputs, tones, params.master_gain_db)?;
    Ok(AudioSetup {
        inputs: sources.inputs,
        tones: sources.tones,
        master_gain_db: sources.master_gain_db,
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PreviewParams {
    /// Whether the preview keeps the pipeline running.
    enabled: bool,
}

fn decode_preview(command: &str, params: Option<&Value>) -> Result<bool, CommandError> {
    let params: PreviewParams = decode_params(command, params)?;
    Ok(params.enabled)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CompositeProbeParams {
    /// Run time in milliseconds.
    duration_ms: Option<u64>,
    /// Add a live screen layer (opens the system picker).
    screen: Option<bool>,
    /// Screen source types to offer in the picker (monitor, window, virtual); default monitor and window.
    source_types: Option<Vec<VideoSourceKind>>,
    /// Cursor mode: hidden, embedded or metadata.
    cursor: Option<CursorMode>,
    /// Plain .ppm file name for a snapshot of the last frame.
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

/// JSON Schema (draft 2020-12, subschemas inlined) of a command's
/// parameters, for adapters that describe commands to clients (MCP tools).
/// Generated from the same structs `Command::decode` deserializes, so it
/// cannot drift from validation. `None` for an unknown command.
pub fn params_schema(name: &str) -> Option<Value> {
    fn of<T: JsonSchema>() -> Value {
        let generator = SchemaSettings::draft2020_12()
            .with(|settings| settings.inline_subschemas = true)
            .into_generator();
        let mut schema = Value::from(generator.into_root_schema_for::<T>());
        if let Some(object) = schema.as_object_mut() {
            // Metadata of the Rust type, not of the command.
            object.remove("$schema");
            object.remove("title");
        }
        schema
    }
    let schema = match name {
        "probe_screen_capture" => of::<ScreenProbeParams>(),
        "probe_audio_capture" => of::<AudioProbeParams>(),
        "probe_composite" => of::<CompositeProbeParams>(),
        "probe_audio_mix" => of::<MixProbeParams>(),
        "start_recording" => of::<StartRecordingParams>(),
        "start_streaming" => of::<StartStreamingParams>(),
        "create_scene" | "set_active_scene" => of::<SceneNameParams>(),
        "delete_scene" => of::<DeleteSceneParams>(),
        "add_source" => of::<AddSourceParams>(),
        "update_source" => of::<UpdateSourceParams>(),
        "remove_source" => of::<RemoveSourceParams>(),
        "set_mixer_strip" => of::<MixerStripParams>(),
        "set_master_gain" => of::<MasterGainParams>(),
        "set_audio_sources" => of::<AudioSourcesParams>(),
        "set_preview" => of::<PreviewParams>(),
        // Listed explicitly so a new command without a mapping has no schema
        // (and fails the registry test) instead of an empty one.
        "get_status"
        | "list_commands"
        | "get_capture_capabilities"
        | "list_audio_inputs"
        | "stop_recording"
        | "stop_streaming"
        | "list_scenes"
        | "get_mixer" => {
            json!({"type": "object", "properties": {}, "additionalProperties": false})
        }
        _ => return None,
    };
    Some(schema)
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
            "create_scene" | "set_active_scene" => Some(json!({"name": "Scene"})),
            "delete_scene" => Some(json!({"name": "Scene", "confirm": true})),
            "add_source" => Some(json!({"scene": "Scene", "name": "bars", "kind": "color_bars"})),
            "update_source" => Some(json!({"scene": "Scene", "name": "bars", "opacity": 0.5})),
            "remove_source" => Some(json!({"scene": "Scene", "name": "bars"})),
            "set_mixer_strip" => Some(json!({"index": 0, "mute": true})),
            "set_master_gain" => Some(json!({"gain_db": -3.0})),
            "set_preview" => Some(json!({"enabled": false})),
            _ => None,
        }
    }

    #[test]
    fn scene_and_source_params() {
        let Ok(Command::AddSource(solid)) = Command::decode(
            "add_source",
            Some(
                &json!({"scene": "Main", "name": "logo", "kind": "solid", "color": "#ff8000",
                         "x": 10, "y": 20, "width": 300, "height": 100, "opacity": 0.5}),
            ),
        ) else {
            panic!("decode");
        };
        assert_eq!(
            solid.kind,
            SourceKind::Solid {
                color: Rgba([255, 128, 0, 255])
            }
        );
        assert_eq!(
            solid.rect,
            Some(SourceRect {
                x: 10.0,
                y: 20.0,
                width: 300.0,
                height: 100.0
            })
        );
        let Ok(Command::AddSource(screen)) = Command::decode(
            "add_source",
            Some(&json!({"scene": "Main", "name": "desk", "kind": "screen"})),
        ) else {
            panic!("decode");
        };
        assert!(matches!(screen.kind, SourceKind::Screen { .. }));
        assert_eq!((screen.rect, screen.opacity), (None, 1.0));
        assert_eq!(
            serde_json::to_value(Rgba([1, 2, 3, 4])).expect("json"),
            json!("#01020304")
        );
    }

    #[test]
    fn invalid_scene_params_are_rejected() {
        for (command, params) in [
            ("create_scene", json!({"name": ""})),
            ("create_scene", json!({"name": " padded "})),
            ("create_scene", json!({"name": "x".repeat(65)})),
            ("create_scene", json!({"name": "tab\there"})),
            ("delete_scene", json!({"name": "Main"})),
            ("delete_scene", json!({"name": "Main", "confirm": false})),
            (
                "add_source",
                json!({"scene": "Main", "name": "a", "kind": "video"}),
            ),
            (
                "add_source",
                json!({"scene": "Main", "name": "a", "kind": "solid"}),
            ),
            (
                "add_source",
                json!({"scene": "Main", "name": "a", "kind": "solid", "color": "red"}),
            ),
            (
                "add_source",
                json!({"scene": "Main", "name": "a", "kind": "color_bars", "x": 1}),
            ),
            (
                "add_source",
                json!({"scene": "Main", "name": "a", "kind": "color_bars", "cursor": "hidden"}),
            ),
            (
                "add_source",
                json!({"scene": "Main", "name": "a", "kind": "color_bars",
                                  "x": 0, "y": 0, "width": 0, "height": 10}),
            ),
            (
                "add_source",
                json!({"scene": "Main", "name": "a", "kind": "color_bars", "opacity": 2}),
            ),
            ("update_source", json!({"scene": "Main", "name": "a"})),
            ("set_mixer_strip", json!({"index": 0})),
            ("set_mixer_strip", json!({"index": 0, "gain_db": 99})),
            ("set_master_gain", json!({"gain_db": -200})),
            ("set_preview", json!({})),
        ] {
            let err = Command::decode(command, Some(&params)).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidParams, "{command} {params}");
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
    fn every_command_has_an_object_params_schema() {
        for descriptor in registry() {
            let schema = params_schema(descriptor.name)
                .unwrap_or_else(|| panic!("{} has no schema", descriptor.name));
            assert_eq!(schema["type"], "object", "{}: {schema}", descriptor.name);
            assert_eq!(schema["additionalProperties"], false, "{}", descriptor.name);
            // Inlined: no references a client would have to resolve.
            assert!(!schema.to_string().contains("$ref"), "{}", descriptor.name);
        }
        assert_eq!(params_schema("nope"), None);
        let add = params_schema("add_source").expect("schema");
        assert_eq!(
            add["properties"]["kind"]["enum"],
            json!(["color_bars", "solid", "screen"])
        );
        assert_eq!(add["required"], json!(["scene", "name", "kind"]));
        // The advertised kinds are exactly the ones the decoder accepts.
        for kind in add["properties"]["kind"]["enum"].as_array().expect("kinds") {
            let mut params = json!({"scene": "Main", "name": "s", "kind": kind});
            if kind == "solid" {
                params["color"] = json!("#000000");
            }
            assert!(
                Command::decode("add_source", Some(&params)).is_ok(),
                "{kind}"
            );
        }
        assert!(
            add["properties"]["opacity"]["description"]
                .as_str()
                .is_some_and(|text| text.contains("0 to 1"))
        );
        // Enums keep their wire names (in whatever form schemars emits).
        let probe = params_schema("probe_screen_capture").expect("schema");
        let kinds = probe["properties"]["source_types"]["items"].to_string();
        for kind in ["monitor", "window", "virtual"] {
            assert!(kinds.contains(&format!("\"{kind}\"")), "{kinds}");
        }
    }

    #[test]
    fn streaming_to_a_fixed_target_refuses_url_and_key() {
        let target = StreamTarget::new("rtmp://ingest.example/app", StreamKey::new("k-SECRET"))
            .expect("target");
        let Ok(Command::StartStreaming(request)) =
            decode_streaming_to(target.clone(), Some(&json!({"video_bitrate_kbps": 3000})))
        else {
            panic!("decode");
        };
        assert_eq!(request.target, target);
        assert_eq!(request.video_bitrate_kbps, 3000);
        for params in [
            json!({"url": "rtmp://evil/app"}),
            json!({"stream_key": "x"}),
        ] {
            let err = decode_streaming_to(target.clone(), Some(&params)).unwrap_err();
            assert_eq!(err.code, ErrorCode::PermissionDenied, "{params}");
        }
        let invalid = decode_streaming_to(target, Some(&json!({"video_bitrate_kbps": 1})));
        assert_eq!(invalid.unwrap_err().code, ErrorCode::InvalidParams);
    }

    #[test]
    fn descriptors_flag_effects_and_least_access() {
        let get = |name: &str| *registry().iter().find(|d| d.name == name).expect(name);
        assert!(get("delete_scene").destructive && get("stop_streaming").destructive);
        assert!(!get("create_scene").destructive && !get("get_status").destructive);
        assert!(get("start_streaming").open_world);
        assert_eq!(
            registry().iter().filter(|d| d.open_world).count(),
            1,
            "only streaming leaves the machine"
        );
        for descriptor in registry() {
            assert!(descriptor.least_access() <= descriptor.access);
            assert!(descriptor.access != CommandAccess::ReadOnly || !descriptor.destructive);
        }
        assert_eq!(get("add_source").least_access(), CommandAccess::Control);
        assert_eq!("read-only".parse(), Ok(CommandAccess::ReadOnly));
        assert_eq!("sensitive".parse(), Ok(CommandAccess::Sensitive));
        assert!("everything".parse::<CommandAccess>().is_err());
    }

    #[test]
    fn only_screen_sources_need_sensitive_access() {
        let add = |params: Value| {
            Command::decode("add_source", Some(&params))
                .expect("decode")
                .required_access()
        };
        let bars = add(json!({"scene": "Main", "name": "s", "kind": "color_bars"}));
        let solid = add(json!({"scene": "Main", "name": "s", "kind": "solid", "color": "#ffffff"}));
        let screen = add(json!({"scene": "Main", "name": "s", "kind": "screen"}));
        assert_eq!(
            (bars, solid, screen),
            (
                CommandAccess::Control,
                CommandAccess::Control,
                CommandAccess::Sensitive
            )
        );
        assert!(CommandAccess::ReadOnly < CommandAccess::Control);
        assert!(CommandAccess::Control < CommandAccess::Sensitive);
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
                "destructive": false,
                "open_world": false,
            })
        );
    }
}
