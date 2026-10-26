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
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};

use audio_engine::{MAX_GAIN_DB, MAX_INPUTS, MIN_GAIN_DB};
use encoder_api::{EncoderPreference, ReconnectPolicy, StreamKey, StreamTarget};

use crate::{
    AddBusRequest, AddChannelRequest, AddSourceRequest, BUS_ID_PREFIX, BusRef, CHANNEL_ID_PREFIX,
    ChannelInput, ChannelRef, Command, CommandError, CompositeRequest, ErrorCode, LiveInputSpec,
    MAX_COORDINATE, MAX_EXTRA_TRACKS, MAX_ID_LEN, MAX_MIX_DURATION, MAX_NAME_CHARS, MixRequest,
    OutputKind, RecordingRequest, Rgba, SCENE_ID_PREFIX, SOURCE_ID_PREFIX, SaveProjectRequest,
    SceneRef, SendTap, SetChannelInputRequest, SetOutputProfileRequest, SetSendRequest, SourceKind,
    SourceRect, SourceRef, StreamingRequest, ToneSpec, UpdateBusRequest, UpdateChannelRequest,
    UpdateSourceRequest,
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

const GET_HEALTH: CommandDescriptor = CommandDescriptor {
    name: "get_health",
    summary: "Live counters while the pipeline runs: render (frames, skipped and late ticks, \
              render time, per-source frame age), mix (blocks, underruns, fill, drift), and per \
              output (frames encoded and dropped, samples, queue depth, stream state, \
              reconnects, encoder). Totals since start; poll it (cheap).",
    access: CommandAccess::ReadOnly,
    destructive: false,
    open_world: false,
};

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

const LIST_AUDIO_OUTPUTS: CommandDescriptor = CommandDescriptor {
    name: "list_audio_outputs",
    summary: "List audio output devices, for the monitor (set_monitor_device).",
    access: CommandAccess::ReadOnly,
    destructive: false,
    open_world: false,
};

const LIST_SCREEN_SOURCES: CommandDescriptor = CommandDescriptor {
    name: "list_screen_sources",
    summary: "List monitors and windows that can be captured by id without a picker (where the \
              platform allows it; see get_capture_capabilities). Window titles can reveal private \
              content.",
    access: CommandAccess::Sensitive,
    destructive: false,
    open_world: false,
};

const PROBE_SCREEN_CAPTURE: CommandDescriptor = CommandDescriptor {
    name: "probe_screen_capture",
    summary: "Capture a screen or window and report format, timestamps, FPS and drops. \
              Params: duration_ms (100..=60000, default 5000), source_id (from list_screen_sources; \
              without it the user picks in the system dialog), source_types ([\"monitor\",\"window\",\"virtual\"], \
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
    summary: "Record the media pipeline (the active scene at 1920x1080@60 and the configured \
              mixer: the recording profile's mix and tracks, see get_output_profiles) to H.264/AAC \
              in Matroska. Params: file_name (required, plain .mkv name in the engine's output \
              directory, never overwritten); video_encoder (auto: hardware if one works here, else \
              software; software; hardware; default: the profile's); optional screen layer \
              (screen, source_id, source_types, cursor) only while no preview or output runs. \
              Returns immediately with the encoder in use; recording continues until \
              stop_recording.",
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
              video_bitrate_kbps (300..50000) and video_encoder (auto, software, hardware), both by \
              default the stream profile's, \
              reconnect_attempts (default 5), reconnect_delay_ms (default 1000, doubling up to \
              30 s), and a screen layer as for start_recording. Returns once connected, with the \
              encoder in use; reconnects after errors; independent of recording.",
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
    summary: "List the scenes with their sources (each with a stable id), the active scene and \
              the canvas size. Commands that take a scene or source name also take its id; \
              given both, they must name the same one.",
    access: CommandAccess::ReadOnly,
    destructive: false,
    open_world: false,
};

const CREATE_SCENE: CommandDescriptor = CommandDescriptor {
    name: "create_scene",
    summary: "Create an empty scene. Params: name (1-64 characters, unique). Returns the scene \
              list, with the new scene's id.",
    access: CommandAccess::Control,
    destructive: false,
    open_world: false,
};

const DELETE_SCENE: CommandDescriptor = CommandDescriptor {
    name: "delete_scene",
    summary: "Delete a scene that is not active; irreversible. Params: scene_id or name, confirm \
              (must be true). Returns the scene list.",
    access: CommandAccess::Sensitive,
    destructive: true,
    open_world: false,
};

const SET_ACTIVE_SCENE: CommandDescriptor = CommandDescriptor {
    name: "set_active_scene",
    summary: "Render this scene from now on (applies live while preview or outputs run; its \
              screen sources start capturing, which opens the system picker). Params: scene_id \
              or name. Returns the scene list.",
    access: CommandAccess::Sensitive,
    destructive: true,
    open_world: false,
};

const ADD_SOURCE: CommandDescriptor = CommandDescriptor {
    name: "add_source",
    summary: "Add a source on top of a scene. Params: scene_id or scene, name, kind (color_bars, solid, \
              screen), color (#rrggbb[aa], solid), source_id, source_types and cursor (screen), x, \
              y, width, height (canvas pixels; all or none, default the whole canvas), opacity \
              (0-1). Screen sources start capturing when the scene is rendered: the source_id from \
              list_screen_sources, or else the one the user picks in the system dialog. Returns \
              the scene list.",
    access: CommandAccess::Sensitive,
    destructive: false,
    open_world: false,
};

const UPDATE_SOURCE: CommandDescriptor = CommandDescriptor {
    name: "update_source",
    summary: "Move, resize or fade a source. Params: the source's id, or scene (or scene_id) and \
              name; any of x, y, width, height, opacity. Returns the scene list.",
    access: CommandAccess::Control,
    destructive: true,
    open_world: false,
};

const REMOVE_SOURCE: CommandDescriptor = CommandDescriptor {
    name: "remove_source",
    summary: "Remove a source from a scene. Params: the source's id, or scene (or scene_id) and \
              name. Returns the scene list.",
    access: CommandAccess::Control,
    destructive: true,
    open_world: false,
};

const RESTART_SOURCE: CommandDescriptor = CommandDescriptor {
    name: "restart_source",
    summary: "Start a screen source's capture again and end the old one, e.g. after get_health \
              reports it ended; outputs keep running. Params: the source's id, or scene (or \
              scene_id) and name; optional source_id \
              (from list_screen_sources) to capture another source from now on: a closed \
              window's id does not come back, a display's does when it reconnects. A picker \
              source asks the user again, so it is refused while recording or streaming. \
              Returns the scene list.",
    access: CommandAccess::Sensitive,
    // It ends the running capture; a picker choice is lost.
    destructive: true,
    open_world: false,
};

const GET_MIXER: CommandDescriptor = CommandDescriptor {
    name: "get_mixer",
    summary: "Mixer channels and buses with their ids, settings and the peak level of the last \
              300 ms.",
    access: CommandAccess::ReadOnly,
    destructive: false,
    open_world: false,
};

const ADD_CHANNEL: CommandDescriptor = CommandDescriptor {
    name: "add_channel",
    summary: "Add a mixer channel, sending to the program bus, while the pipeline is idle. \
              Params: name (unique), input ({kind: device, device: a name from \
              list_audio_inputs, or null for the default input} or {kind: tone, frequency_hz, \
              level_dbfs -96..0, default -12}), gain_db (-96..24), mute, balance (-1..1). \
              Returns the mixer state.",
    access: CommandAccess::Sensitive,
    destructive: false,
    open_world: false,
};

const REMOVE_CHANNEL: CommandDescriptor = CommandDescriptor {
    name: "remove_channel",
    summary: "Remove a mixer channel while the pipeline is idle. Params: id, or name. Returns \
              the mixer state.",
    access: CommandAccess::Control,
    destructive: true,
    open_world: false,
};

const UPDATE_CHANNEL: CommandDescriptor = CommandDescriptor {
    name: "update_channel",
    summary: "Change a channel's fader, mute or balance (live if running). Params: id or name; \
              gain_db (-96..24), mute, balance (-1..1). Returns the mixer state.",
    access: CommandAccess::Control,
    destructive: true,
    open_world: false,
};

const SET_CHANNEL_INPUT: CommandDescriptor = CommandDescriptor {
    name: "set_channel_input",
    summary: "Choose what a channel plays while the pipeline is idle. Params: id or name; input \
              as for add_channel. Returns the mixer state.",
    access: CommandAccess::Sensitive,
    destructive: true,
    open_world: false,
};

const ADD_BUS: CommandDescriptor = CommandDescriptor {
    name: "add_bus",
    summary: "Add an aux bus while the pipeline is idle: a submix (with assign_db, its level \
              into the program bus) or a separate mix (assign_db null, e.g. for monitoring). \
              Params: name (unique among buses), gain_db (-96..24), mute, assign_db (-96..24 or \
              null). Every channel's send to it starts off. Returns the mixer state.",
    access: CommandAccess::Control,
    destructive: false,
    open_world: false,
};

const REMOVE_BUS: CommandDescriptor = CommandDescriptor {
    name: "remove_bus",
    summary: "Remove an aux bus and every send to it while the pipeline is idle; the program \
              bus cannot be removed. Params: id, or name. Returns the mixer state.",
    access: CommandAccess::Control,
    destructive: true,
    open_world: false,
};

const UPDATE_BUS: CommandDescriptor = CommandDescriptor {
    name: "update_bus",
    summary: "Change a bus's fader, mute or assignment (live if running). Params: id (e.g. \
              bus_program) or name (e.g. Program); gain_db (-96..24), mute, assign_db (aux \
              buses: -96..24, or null for a separate mix). Returns the mixer state.",
    access: CommandAccess::Control,
    destructive: true,
    open_world: false,
};

const SET_SEND: CommandDescriptor = CommandDescriptor {
    name: "set_send",
    summary: "Change a channel's send to a bus (live if running). Params: channel_id or \
              channel (name); bus_id or bus (name); enabled, level_db (-96..24), tap (pre_fader: \
              the channel fader does not move it; post_fader). Returns the mixer state.",
    access: CommandAccess::Control,
    destructive: true,
    open_world: false,
};

const GET_OUTPUT_PROFILES: CommandDescriptor = CommandDescriptor {
    name: "get_output_profiles",
    summary: "Every output's profile: the bus it takes as its mix and its encoder settings, which \
              start_recording and start_streaming use unless their parameters say otherwise; and \
              the monitor's bus, whether it is on, and its device.",
    access: CommandAccess::ReadOnly,
    destructive: false,
    open_world: false,
};

const SET_OUTPUT_PROFILE: CommandDescriptor = CommandDescriptor {
    name: "set_output_profile",
    summary: "Change an output's profile. Params: output (recording, stream or monitor); \
              mix_bus_id or mix_bus (name): the bus it takes as its mix (audio track 1, or what \
              the monitor plays); tracks (recording only: up to 7 buses, ids or names, recorded \
              as further audio tracks named after them); video_encoder (auto, software, hardware; \
              not for the monitor); video_bitrate_kbps (stream only); enabled (monitor only: \
              plays the bus on its device while the pipeline runs; off by default). A running \
              output fades to a new mix now, and the monitor switches on or off now; encoder \
              settings apply at the next start. Returns the profiles, which fields applied now or \
              wait for the next start, and warnings (e.g. a monitor device that did not open).",
    access: CommandAccess::Control,
    destructive: true,
    open_world: false,
};

const SET_MONITOR_DEVICE: CommandDescriptor = CommandDescriptor {
    name: "set_monitor_device",
    summary: "Choose the audio device the monitor plays on, while the pipeline is idle. Params: \
              device (a name from list_audio_outputs, or null or absent for the system default). \
              Returns the profiles.",
    access: CommandAccess::Sensitive,
    destructive: false,
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

const SET_SCREEN_SOURCE: CommandDescriptor = CommandDescriptor {
    name: "set_screen_source",
    summary: "Choose what a screen source captures from now on, while it has no running capture \
              (e.g. after load_project, which does not store it; for a running capture use \
              restart_source). Params: the source's id, or scene (or scene_id) and name; \
              source_id from list_screen_sources. Returns the scene list.",
    access: CommandAccess::Sensitive,
    destructive: false,
    open_world: false,
};

const LIST_PROJECTS: CommandDescriptor = CommandDescriptor {
    name: "list_projects",
    summary: "List the project files (.json) in the engine's project directory (--project-dir), \
              by name, with their size and when they were written; at most 256, the rest are \
              counted in omitted. The files are not read: validate_project checks one.",
    access: CommandAccess::ReadOnly,
    destructive: false,
    open_world: false,
};

const SAVE_PROJECT: CommandDescriptor = CommandDescriptor {
    name: "save_project",
    summary: "Write the scenes (with their ids), the active scene and the audio setup to a \
              project file. Params: file_name (plain .json name in the engine's project \
              directory, --project-dir), overwrite (replace an existing file atomically; \
              default false). Screen source ids and stream keys are not saved.",
    access: CommandAccess::Sensitive,
    // With `overwrite`, it replaces the previous file.
    destructive: true,
    open_world: false,
};

const LOAD_PROJECT: CommandDescriptor = CommandDescriptor {
    name: "load_project",
    summary: "Replace the scenes and the audio setup with a project file's, while no preview or \
              output runs. Params: file_name (plain .json name in the project directory). \
              Unsaved scenes are lost. Screen sources load without a capture target (choose one \
              with set_screen_source, or the picker asks). Returns the scenes and the mixer.",
    access: CommandAccess::Sensitive,
    destructive: true,
    open_world: false,
};

const VALIDATE_PROJECT: CommandDescriptor = CommandDescriptor {
    name: "validate_project",
    summary: "Check a project file without loading it. Params: file_name (plain .json name in the \
              project directory). Returns valid and every problem with its JSON path.",
    access: CommandAccess::ReadOnly,
    destructive: false,
    open_world: false,
};

const COMMANDS: &[CommandDescriptor] = &[
    GET_STATUS,
    GET_HEALTH,
    LIST_COMMANDS,
    GET_CAPTURE_CAPABILITIES,
    LIST_AUDIO_INPUTS,
    LIST_AUDIO_OUTPUTS,
    LIST_SCREEN_SOURCES,
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
    RESTART_SOURCE,
    SET_SCREEN_SOURCE,
    GET_MIXER,
    ADD_CHANNEL,
    REMOVE_CHANNEL,
    UPDATE_CHANNEL,
    SET_CHANNEL_INPUT,
    ADD_BUS,
    REMOVE_BUS,
    UPDATE_BUS,
    SET_SEND,
    GET_OUTPUT_PROFILES,
    SET_OUTPUT_PROFILE,
    SET_MONITOR_DEVICE,
    SET_PREVIEW,
    LIST_PROJECTS,
    SAVE_PROJECT,
    LOAD_PROJECT,
    VALIDATE_PROJECT,
];

const DEFAULT_SOURCE_KINDS: [VideoSourceKind; 2] =
    [VideoSourceKind::Monitor, VideoSourceKind::Window];
const DEFAULT_CURSOR: CursorMode = CursorMode::Embedded;
/// Longest accepted screen source id, in characters.
const MAX_SOURCE_ID_CHARS: usize = 256;

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
            "get_health" => expect_no_params(name, params).map(|()| Self::GetHealth),
            "list_audio_inputs" => expect_no_params(name, params).map(|()| Self::ListAudioInputs),
            "list_audio_outputs" => expect_no_params(name, params).map(|()| Self::ListAudioOutputs),
            "list_screen_sources" => {
                expect_no_params(name, params).map(|()| Self::ListScreenSources)
            }
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
            "delete_scene" => decode_delete_scene(name, params).map(Self::DeleteScene),
            "set_active_scene" => decode_set_active_scene(name, params).map(Self::SetActiveScene),
            "add_source" => decode_add_source(name, params).map(Self::AddSource),
            "update_source" => decode_update_source(name, params).map(Self::UpdateSource),
            "remove_source" => decode_remove_source(name, params).map(Self::RemoveSource),
            "restart_source" => decode_restart_source(name, params),
            "set_screen_source" => decode_set_screen_source(name, params),
            "list_projects" => expect_no_params(name, params).map(|()| Self::ListProjects),
            "save_project" => decode_save_project(name, params).map(Self::SaveProject),
            "load_project" => {
                decode_project_file(name, params).map(|file_name| Self::LoadProject { file_name })
            }
            "validate_project" => decode_project_file(name, params)
                .map(|file_name| Self::ValidateProject { file_name }),
            "get_mixer" => expect_no_params(name, params).map(|()| Self::GetMixer),
            "add_channel" => decode_add_channel(name, params).map(Self::AddChannel),
            "remove_channel" => decode_remove_channel(name, params).map(Self::RemoveChannel),
            "update_channel" => decode_update_channel(name, params).map(Self::UpdateChannel),
            "set_channel_input" => {
                decode_set_channel_input(name, params).map(Self::SetChannelInput)
            }
            "add_bus" => decode_add_bus(name, params).map(Self::AddBus),
            "remove_bus" => decode_remove_bus(name, params).map(Self::RemoveBus),
            "update_bus" => decode_update_bus(name, params).map(Self::UpdateBus),
            "set_send" => decode_set_send(name, params).map(Self::SetSend),
            "get_output_profiles" => {
                expect_no_params(name, params).map(|()| Self::GetOutputProfiles)
            }
            "set_output_profile" => {
                decode_set_output_profile(name, params).map(Self::SetOutputProfile)
            }
            "set_monitor_device" => decode_set_monitor_device(name, params)
                .map(|device| Self::SetMonitorDevice { device }),
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
            Self::GetHealth => &GET_HEALTH,
            Self::ListAudioInputs => &LIST_AUDIO_INPUTS,
            Self::ListAudioOutputs => &LIST_AUDIO_OUTPUTS,
            Self::ListScreenSources => &LIST_SCREEN_SOURCES,
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
            Self::RestartSource { .. } => &RESTART_SOURCE,
            Self::SetScreenSource { .. } => &SET_SCREEN_SOURCE,
            Self::GetMixer => &GET_MIXER,
            Self::AddChannel(_) => &ADD_CHANNEL,
            Self::RemoveChannel(_) => &REMOVE_CHANNEL,
            Self::UpdateChannel(_) => &UPDATE_CHANNEL,
            Self::SetChannelInput(_) => &SET_CHANNEL_INPUT,
            Self::AddBus(_) => &ADD_BUS,
            Self::RemoveBus(_) => &REMOVE_BUS,
            Self::UpdateBus(_) => &UPDATE_BUS,
            Self::SetSend(_) => &SET_SEND,
            Self::GetOutputProfiles => &GET_OUTPUT_PROFILES,
            Self::SetOutputProfile(_) => &SET_OUTPUT_PROFILE,
            Self::SetMonitorDevice { .. } => &SET_MONITOR_DEVICE,
            Self::SetPreview { .. } => &SET_PREVIEW,
            Self::ListProjects => &LIST_PROJECTS,
            Self::SaveProject(_) => &SAVE_PROJECT,
            Self::LoadProject { .. } => &LOAD_PROJECT,
            Self::ValidateProject { .. } => &VALIDATE_PROJECT,
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
    /// Screen source id from list_screen_sources: capture it without a picker.
    source_id: Option<String>,
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

/// A tone channel's level when `add_channel` or `set_channel_input` gives
/// none.
pub const DEFAULT_TONE_LEVEL_DBFS: f64 = -12.0;
/// The test tone outputs use when nothing else is configured: 1 kHz at
/// −20 dBFS, quiet but audibly alive.
pub const DEFAULT_TONE_HZ: f64 = 1000.0;
pub const DEFAULT_OUTPUT_TONE_DBFS: f64 = -20.0;

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
    let mut generated = Vec::with_capacity(tones.len());
    for tone in tones {
        generated.push(ToneSpec {
            frequency_hz: tone_frequency(command, tone.frequency_hz)?,
            level_dbfs: tone_level(command, tone.level_dbfs)?,
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

fn tone_frequency(command: &str, hz: f64) -> Result<f64, CommandError> {
    let nyquist = f64::from(audio_engine::RATE) / 2.0;
    if hz.is_finite() && hz > 0.0 && hz < nyquist {
        Ok(hz)
    } else {
        Err(invalid_params(
            command,
            format!("`frequency_hz` must be between 0 and {nyquist} (exclusive)"),
        ))
    }
}

/// A tone's level; `None` is the default level.
fn tone_level(command: &str, dbfs: Option<f64>) -> Result<f64, CommandError> {
    let level = dbfs.unwrap_or(DEFAULT_TONE_LEVEL_DBFS);
    if level.is_finite() && (f64::from(MIN_GAIN_DB)..=0.0).contains(&level) {
        Ok(level)
    } else {
        Err(invalid_params(
            command,
            format!("`level_dbfs` must be between {MIN_GAIN_DB} and 0"),
        ))
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StartRecordingParams {
    /// Plain .mkv file name, created in the engine's output directory; never overwrites.
    file_name: String,
    /// H.264 encoder: auto (hardware if one works here, else software), software or hardware.
    video_encoder: Option<EncoderPreference>,
    /// Add a live screen layer (opens the system picker).
    screen: Option<bool>,
    /// Screen source id from list_screen_sources: capture it without a picker.
    source_id: Option<String>,
    /// Screen source types to offer in the picker (monitor, window, virtual); default monitor and window.
    source_types: Option<Vec<VideoSourceKind>>,
    /// Cursor mode: hidden, embedded or metadata.
    cursor: Option<CursorMode>,
}

fn decode_start_recording(
    command: &str,
    params: Option<&Value>,
) -> Result<RecordingRequest, CommandError> {
    let params: StartRecordingParams = decode_params(command, params)?;
    Ok(RecordingRequest {
        file_name: output_file(command, "file_name", params.file_name, "mkv")?,
        video_encoder: params.video_encoder,
        screen: optional_screen(
            command,
            params.screen,
            params.source_id,
            params.source_types,
            params.cursor,
        )?,
    })
}

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
    /// Constant video bit rate in kbit/s, 300 to 50000 (default: the stream profile's).
    video_bitrate_kbps: Option<u32>,
    /// H.264 encoder: auto (hardware if one works here, else software), software or hardware (default: the stream profile's).
    video_encoder: Option<EncoderPreference>,
    /// Reconnect attempts after a lost connection, 0 to 100 (default 5).
    reconnect_attempts: Option<u32>,
    /// Wait before the first reconnect in ms, doubling up to 30 s (default 1000).
    reconnect_delay_ms: Option<u64>,
    /// Add a live screen layer (opens the system picker).
    screen: Option<bool>,
    /// Screen source id from list_screen_sources: capture it without a picker.
    source_id: Option<String>,
    /// Screen source types to offer in the picker (monitor, window, virtual); default monitor and window.
    source_types: Option<Vec<VideoSourceKind>>,
    /// Cursor mode: hidden, embedded or metadata.
    cursor: Option<CursorMode>,
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
        .map(|kbps| video_bitrate(command, kbps))
        .transpose()?;
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
        video_encoder: params.video_encoder,
        reconnect: ReconnectPolicy {
            max_attempts,
            initial_delay,
            max_delay: defaults.max_delay.max(initial_delay),
        },
        screen: optional_screen(
            command,
            params.screen,
            params.source_id,
            params.source_types,
            params.cursor,
        )?,
    })
}

/// The rule for scene and source names, for error messages.
pub const NAME_RULE: &str =
    "must be 1-64 characters without control characters or surrounding whitespace";

/// Scene and source names: 1-64 characters, no control characters, no
/// surrounding whitespace.
pub fn is_valid_name(name: &str) -> bool {
    let length = name.chars().count();
    length > 0
        && length <= MAX_NAME_CHARS
        && name.trim() == name
        && !name.chars().any(char::is_control)
}

fn valid_name(command: &str, field: &str, name: String) -> Result<String, CommandError> {
    if !is_valid_name(&name) {
        return Err(invalid_params(command, format!("`{field}` {NAME_RULE}")));
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

/// Scene and source ids: `prefix` (e.g. [`SCENE_ID_PREFIX`]), then
/// `a-z`, `0-9` or `_`, at most [`MAX_ID_LEN`] bytes in all.
pub fn is_valid_id(prefix: &str, id: &str) -> bool {
    id.len() > prefix.len()
        && id.len() <= MAX_ID_LEN
        && id.starts_with(prefix)
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn valid_id(command: &str, field: &str, prefix: &str, id: String) -> Result<String, CommandError> {
    if !is_valid_id(prefix, &id) {
        return Err(invalid_params(
            command,
            format!("`{field}` must be an id from list_scenes (`{prefix}…`)"),
        ));
    }
    Ok(id)
}

/// A scene named by `scene_id`, by `name_field`, or by both; `None` if
/// neither is given.
fn scene_ref(
    command: &str,
    id: Option<String>,
    name_field: &str,
    name: Option<String>,
) -> Result<Option<SceneRef>, CommandError> {
    let id = id
        .map(|id| valid_id(command, "scene_id", SCENE_ID_PREFIX, id))
        .transpose()?;
    let name = name
        .map(|name| valid_name(command, name_field, name))
        .transpose()?;
    Ok((id.is_some() || name.is_some()).then_some(SceneRef { id, name }))
}

fn required_scene(
    command: &str,
    id: Option<String>,
    name_field: &str,
    name: Option<String>,
) -> Result<SceneRef, CommandError> {
    scene_ref(command, id, name_field, name)?
        .ok_or_else(|| invalid_params(command, format!("give `scene_id` or `{name_field}`")))
}

/// A source named by its `id`, or by its scene and `name`.
fn source_ref(
    command: &str,
    id: Option<String>,
    scene_id: Option<String>,
    scene: Option<String>,
    name: Option<String>,
) -> Result<SourceRef, CommandError> {
    let id = id
        .map(|id| valid_id(command, "id", SOURCE_ID_PREFIX, id))
        .transpose()?;
    let scene = scene_ref(command, scene_id, "scene", scene)?;
    let name = name
        .map(|name| valid_name(command, "name", name))
        .transpose()?;
    if id.is_none() && (scene.is_none() || name.is_none()) {
        return Err(invalid_params(
            command,
            "give the source's `id`, or `scene` (or `scene_id`) and `name`",
        ));
    }
    Ok(SourceRef { id, scene, name })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetActiveSceneParams {
    /// Scene id from list_scenes (or give the name).
    scene_id: Option<String>,
    /// Scene name (or give scene_id).
    name: Option<String>,
}

fn decode_set_active_scene(
    command: &str,
    params: Option<&Value>,
) -> Result<SceneRef, CommandError> {
    let params: SetActiveSceneParams = decode_params(command, params)?;
    required_scene(command, params.scene_id, "name", params.name)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DeleteSceneParams {
    /// Scene id from list_scenes (or give the name).
    scene_id: Option<String>,
    /// Scene name (or give scene_id).
    name: Option<String>,
    /// Must be true: deleting a scene cannot be undone.
    confirm: Option<bool>,
}

fn decode_delete_scene(command: &str, params: Option<&Value>) -> Result<SceneRef, CommandError> {
    let params: DeleteSceneParams = decode_params(command, params)?;
    if params.confirm != Some(true) {
        return Err(invalid_params(
            command,
            "deleting a scene cannot be undone; pass `confirm: true`",
        ));
    }
    required_scene(command, params.scene_id, "name", params.name)
}

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
    /// Scene id from list_scenes (or give scene).
    scene_id: Option<String>,
    /// Scene name (or give scene_id).
    scene: Option<String>,
    /// Source name, unique within the scene.
    name: String,
    /// Source kind: color_bars, solid or screen.
    #[schemars(extend("enum" = ["color_bars", "solid", "screen"]))]
    kind: String,
    /// Solid sources: #rrggbb or #rrggbbaa.
    color: Option<String>,
    /// Screen source id from list_screen_sources: capture it without a picker.
    source_id: Option<String>,
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
            "`source_id`, `source_types` and `cursor` apply to screen sources only",
        )
    };
    let kind = match params.kind.as_str() {
        "color_bars" | "solid"
            if params.source_id.is_some()
                || params.source_types.is_some()
                || params.cursor.is_some() =>
        {
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
            let selection = screen_selection(
                command,
                params.source_id,
                params.source_types,
                params.cursor,
            )?;
            SourceKind::Screen {
                source_types: selection.source_kinds,
                cursor: selection.cursor,
                source_id: selection.source,
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
        scene: required_scene(command, params.scene_id, "scene", params.scene)?,
        name: valid_name(command, "name", params.name)?,
        kind,
        rect,
        opacity: opacity(command, params.opacity.unwrap_or(1.0))?,
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpdateSourceParams {
    /// Source id from list_scenes (or give scene and name).
    id: Option<String>,
    /// Scene id from list_scenes (or give scene).
    scene_id: Option<String>,
    /// Scene name.
    scene: Option<String>,
    /// Source name.
    name: Option<String>,
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
        source: source_ref(
            command,
            params.id,
            params.scene_id,
            params.scene,
            params.name,
        )?,
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
    /// Source id from list_scenes (or give scene and name).
    id: Option<String>,
    /// Scene id from list_scenes (or give scene).
    scene_id: Option<String>,
    /// Scene name.
    scene: Option<String>,
    /// Source name.
    name: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RestartSourceParams {
    /// Source id from list_scenes (or give scene and name).
    id: Option<String>,
    /// Scene id from list_scenes (or give scene).
    scene_id: Option<String>,
    /// Scene name.
    scene: Option<String>,
    /// Source name.
    name: Option<String>,
    /// Capture this source from list_screen_sources from now on (the scene
    /// keeps it), e.g. instead of a closed window.
    source_id: Option<String>,
}

fn decode_restart_source(command: &str, params: Option<&Value>) -> Result<Command, CommandError> {
    let params: RestartSourceParams = decode_params(command, params)?;
    Ok(Command::RestartSource {
        source: source_ref(
            command,
            params.id,
            params.scene_id,
            params.scene,
            params.name,
        )?,
        source_id: params
            .source_id
            .map(|id| valid_source_id(command, id))
            .transpose()?,
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetScreenSourceParams {
    /// Source id from list_scenes (or give scene and name).
    id: Option<String>,
    /// Scene id from list_scenes (or give scene).
    scene_id: Option<String>,
    /// Scene name.
    scene: Option<String>,
    /// Source name.
    name: Option<String>,
    /// Screen source id from list_screen_sources.
    source_id: String,
}

fn decode_set_screen_source(
    command: &str,
    params: Option<&Value>,
) -> Result<Command, CommandError> {
    let params: SetScreenSourceParams = decode_params(command, params)?;
    Ok(Command::SetScreenSource {
        source: source_ref(
            command,
            params.id,
            params.scene_id,
            params.scene,
            params.name,
        )?,
        source_id: valid_source_id(command, params.source_id)?,
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SaveProjectParams {
    /// Plain .json file name in the engine's project directory.
    file_name: String,
    /// Replace an existing file (atomically); default false.
    overwrite: Option<bool>,
}

fn decode_save_project(
    command: &str,
    params: Option<&Value>,
) -> Result<SaveProjectRequest, CommandError> {
    let params: SaveProjectParams = decode_params(command, params)?;
    Ok(SaveProjectRequest {
        file_name: project_file(command, params.file_name)?,
        overwrite: params.overwrite.unwrap_or(false),
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProjectFileParams {
    /// Plain .json file name in the engine's project directory.
    file_name: String,
}

fn decode_project_file(command: &str, params: Option<&Value>) -> Result<PathBuf, CommandError> {
    let params: ProjectFileParams = decode_params(command, params)?;
    project_file(command, params.file_name)
}

/// Check a project file name by the project commands' rule (a plain
/// `.json` name, not a Windows device) for `command`, which the error
/// names. A client checks a name as it is typed with it, and the engine
/// lists only names it would accept.
pub fn project_file_name(command: &str, file_name: &str) -> Result<PathBuf, CommandError> {
    project_file(command, file_name.to_owned())
}

fn project_file(command: &str, file_name: String) -> Result<PathBuf, CommandError> {
    plain_file(
        command,
        "file_name",
        file_name,
        "json",
        "it lives in the engine's project directory",
    )
}

fn decode_remove_source(command: &str, params: Option<&Value>) -> Result<SourceRef, CommandError> {
    let params: RemoveSourceParams = decode_params(command, params)?;
    source_ref(
        command,
        params.id,
        params.scene_id,
        params.scene,
        params.name,
    )
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

/// What a channel plays, as `add_channel` and `set_channel_input` take it.
#[derive(Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ChannelInputParams {
    /// A capture device.
    Device {
        /// Audio input name from list_audio_inputs; null or absent for the system default.
        #[serde(default)]
        device: Option<String>,
    },
    /// A test tone.
    Tone {
        /// Frequency in Hz (below 24000).
        frequency_hz: f64,
        /// Level in dBFS, -96 to 0 (default -12).
        level_dbfs: Option<f64>,
    },
}

fn channel_input(command: &str, input: ChannelInputParams) -> Result<ChannelInput, CommandError> {
    match input {
        ChannelInputParams::Device { device } => {
            if device.as_deref().is_some_and(str::is_empty) {
                return Err(invalid_params(command, "`device` must not be empty"));
            }
            Ok(ChannelInput::Device { device })
        }
        ChannelInputParams::Tone {
            frequency_hz,
            level_dbfs,
        } => Ok(ChannelInput::Tone {
            frequency_hz: tone_frequency(command, frequency_hz)?,
            level_dbfs: tone_level(command, level_dbfs)?,
        }),
    }
}

fn balance(command: &str, value: f32) -> Result<f32, CommandError> {
    if value.is_finite() && (-1.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(invalid_params(
            command,
            "`balance` must be between -1 and 1",
        ))
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddChannelParams {
    /// Channel name, unique among the channels.
    name: String,
    /// What the channel plays.
    input: ChannelInputParams,
    /// Fader in dB, -96 to 24 (default 0).
    gain_db: Option<f32>,
    /// Mute the channel in every mix (default false).
    mute: Option<bool>,
    /// Balance from -1 (left only) to 1 (right only); default 0.
    balance: Option<f32>,
}

fn decode_add_channel(
    command: &str,
    params: Option<&Value>,
) -> Result<AddChannelRequest, CommandError> {
    let params: AddChannelParams = decode_params(command, params)?;
    Ok(AddChannelRequest {
        name: valid_name(command, "name", params.name)?,
        input: channel_input(command, params.input)?,
        gain_db: gain_db(command, "gain_db", params.gain_db.unwrap_or(0.0))?,
        muted: params.mute.unwrap_or(false),
        balance: balance(command, params.balance.unwrap_or(0.0))?,
    })
}

/// Something named by its id (with `prefix`), by its name, or by both;
/// each with the parameter it came in.
fn named_ref(
    command: &str,
    what: &str,
    prefix: &str,
    (id_field, id): (&str, Option<String>),
    (name_field, name): (&str, Option<String>),
) -> Result<(Option<String>, Option<String>), CommandError> {
    let id = id
        .map(|id| valid_id(command, id_field, prefix, id))
        .transpose()?;
    let name = name
        .map(|name| valid_name(command, name_field, name))
        .transpose()?;
    if id.is_none() && name.is_none() {
        return Err(invalid_params(
            command,
            format!("give the {what}'s `{id_field}` or `{name_field}`"),
        ));
    }
    Ok((id, name))
}

fn channel_ref(
    command: &str,
    id: Option<String>,
    name: Option<String>,
) -> Result<ChannelRef, CommandError> {
    let (id, name) = named_ref(
        command,
        "channel",
        CHANNEL_ID_PREFIX,
        ("id", id),
        ("name", name),
    )?;
    Ok(ChannelRef { id, name })
}

fn bus_ref(
    command: &str,
    id: (&str, Option<String>),
    name: (&str, Option<String>),
) -> Result<BusRef, CommandError> {
    let (id, name) = named_ref(command, "bus", BUS_ID_PREFIX, id, name)?;
    Ok(BusRef { id, name })
}

/// For a field where `null` means something: absent is `None`, `null` is
/// `Some(None)`.
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ChannelParams {
    /// Channel id from get_mixer (or give name).
    id: Option<String>,
    /// Channel name.
    name: Option<String>,
}

fn decode_remove_channel(
    command: &str,
    params: Option<&Value>,
) -> Result<ChannelRef, CommandError> {
    let params: ChannelParams = decode_params(command, params)?;
    channel_ref(command, params.id, params.name)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpdateChannelParams {
    /// Channel id from get_mixer (or give name).
    id: Option<String>,
    /// Channel name.
    name: Option<String>,
    /// Fader in dB, -96 to 24.
    gain_db: Option<f32>,
    /// Mute the channel in every mix.
    mute: Option<bool>,
    /// Balance from -1 (left only) to 1 (right only).
    balance: Option<f32>,
}

fn decode_update_channel(
    command: &str,
    params: Option<&Value>,
) -> Result<UpdateChannelRequest, CommandError> {
    let params: UpdateChannelParams = decode_params(command, params)?;
    if params.gain_db.is_none() && params.mute.is_none() && params.balance.is_none() {
        return Err(invalid_params(
            command,
            "give `gain_db`, `mute` or `balance`",
        ));
    }
    Ok(UpdateChannelRequest {
        channel: channel_ref(command, params.id, params.name)?,
        gain_db: params
            .gain_db
            .map(|value| gain_db(command, "gain_db", value))
            .transpose()?,
        muted: params.mute,
        balance: params
            .balance
            .map(|value| balance(command, value))
            .transpose()?,
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetChannelInputParams {
    /// Channel id from get_mixer (or give name).
    id: Option<String>,
    /// Channel name.
    name: Option<String>,
    /// What the channel plays from now on.
    input: ChannelInputParams,
}

fn decode_set_channel_input(
    command: &str,
    params: Option<&Value>,
) -> Result<SetChannelInputRequest, CommandError> {
    let params: SetChannelInputParams = decode_params(command, params)?;
    Ok(SetChannelInputRequest {
        channel: channel_ref(command, params.id, params.name)?,
        input: channel_input(command, params.input)?,
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddBusParams {
    /// Bus name, unique among the buses (Program is taken).
    name: String,
    /// Fader in dB, -96 to 24 (default 0).
    gain_db: Option<f32>,
    /// Mute the bus (default false).
    mute: Option<bool>,
    /// Level into the program bus in dB, -96 to 24; null or absent: a separate mix.
    assign_db: Option<f32>,
}

fn decode_add_bus(command: &str, params: Option<&Value>) -> Result<AddBusRequest, CommandError> {
    let params: AddBusParams = decode_params(command, params)?;
    Ok(AddBusRequest {
        name: valid_name(command, "name", params.name)?,
        gain_db: gain_db(command, "gain_db", params.gain_db.unwrap_or(0.0))?,
        muted: params.mute.unwrap_or(false),
        assign_db: params
            .assign_db
            .map(|value| gain_db(command, "assign_db", value))
            .transpose()?,
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct BusParams {
    /// Bus id from get_mixer (or give name).
    id: Option<String>,
    /// Bus name.
    name: Option<String>,
}

fn decode_remove_bus(command: &str, params: Option<&Value>) -> Result<BusRef, CommandError> {
    let params: BusParams = decode_params(command, params)?;
    bus_ref(command, ("id", params.id), ("name", params.name))
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpdateBusParams {
    /// Bus id from get_mixer, e.g. bus_program (or give name).
    id: Option<String>,
    /// Bus name, e.g. Program.
    name: Option<String>,
    /// Fader in dB, -96 to 24.
    gain_db: Option<f32>,
    /// Mute the bus.
    mute: Option<bool>,
    /// An aux bus's level into the program bus in dB, -96 to 24; null: a separate mix.
    #[serde(default, deserialize_with = "present")]
    assign_db: Option<Option<f32>>,
}

fn decode_update_bus(
    command: &str,
    params: Option<&Value>,
) -> Result<UpdateBusRequest, CommandError> {
    let params: UpdateBusParams = decode_params(command, params)?;
    if params.gain_db.is_none() && params.mute.is_none() && params.assign_db.is_none() {
        return Err(invalid_params(
            command,
            "give `gain_db`, `mute` or `assign_db`",
        ));
    }
    Ok(UpdateBusRequest {
        bus: bus_ref(command, ("id", params.id), ("name", params.name))?,
        gain_db: params
            .gain_db
            .map(|value| gain_db(command, "gain_db", value))
            .transpose()?,
        muted: params.mute,
        assign_db: params
            .assign_db
            .map(|assign| {
                assign
                    .map(|value| gain_db(command, "assign_db", value))
                    .transpose()
            })
            .transpose()?,
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetSendParams {
    /// Channel id from get_mixer (or give channel).
    channel_id: Option<String>,
    /// Channel name.
    channel: Option<String>,
    /// Bus id from get_mixer (or give bus).
    bus_id: Option<String>,
    /// Bus name.
    bus: Option<String>,
    /// Whether the channel feeds the bus.
    enabled: Option<bool>,
    /// Send level in dB, -96 to 24.
    level_db: Option<f32>,
    /// Where the send takes the channel's signal: pre_fader or post_fader.
    tap: Option<SendTap>,
}

fn decode_set_send(command: &str, params: Option<&Value>) -> Result<SetSendRequest, CommandError> {
    let params: SetSendParams = decode_params(command, params)?;
    if params.enabled.is_none() && params.level_db.is_none() && params.tap.is_none() {
        return Err(invalid_params(
            command,
            "give `enabled`, `level_db` or `tap`",
        ));
    }
    let (id, name) = named_ref(
        command,
        "channel",
        CHANNEL_ID_PREFIX,
        ("channel_id", params.channel_id),
        ("channel", params.channel),
    )?;
    Ok(SetSendRequest {
        channel: ChannelRef { id, name },
        bus: bus_ref(command, ("bus_id", params.bus_id), ("bus", params.bus))?,
        enabled: params.enabled,
        level_db: params
            .level_db
            .map(|value| gain_db(command, "level_db", value))
            .transpose()?,
        tap: params.tap,
    })
}

fn video_bitrate(command: &str, kbps: u32) -> Result<u32, CommandError> {
    if VIDEO_BITRATE_KBPS.contains(&kbps) {
        Ok(kbps)
    } else {
        Err(invalid_params(
            command,
            format!(
                "`video_bitrate_kbps` must be between {} and {}",
                VIDEO_BITRATE_KBPS.start(),
                VIDEO_BITRATE_KBPS.end()
            ),
        ))
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetOutputProfileParams {
    /// The output: recording, stream or monitor.
    output: OutputKind,
    /// Id of the bus it takes as its mix (or give mix_bus).
    mix_bus_id: Option<String>,
    /// Name of the bus it takes as its mix.
    mix_bus: Option<String>,
    /// H.264 encoder: auto, software or hardware.
    video_encoder: Option<EncoderPreference>,
    /// Stream only: video bitrate in kbit/s.
    video_bitrate_kbps: Option<u32>,
    /// Recording only: the buses (ids or names) recorded as audio tracks 2 and on, in order.
    tracks: Option<Vec<String>>,
    /// Monitor only: play the bus on the monitor device (live, ramped).
    enabled: Option<bool>,
}

fn decode_set_output_profile(
    command: &str,
    params: Option<&Value>,
) -> Result<SetOutputProfileRequest, CommandError> {
    let params: SetOutputProfileParams = decode_params(command, params)?;
    let mix = (params.mix_bus_id.is_some() || params.mix_bus.is_some())
        .then(|| {
            bus_ref(
                command,
                ("mix_bus_id", params.mix_bus_id),
                ("mix_bus", params.mix_bus),
            )
        })
        .transpose()?;
    if mix.is_none()
        && params.video_encoder.is_none()
        && params.video_bitrate_kbps.is_none()
        && params.tracks.is_none()
        && params.enabled.is_none()
    {
        return Err(invalid_params(
            command,
            "give `mix_bus_id` or `mix_bus`, `tracks`, `video_encoder`, `video_bitrate_kbps` or \
             `enabled`",
        ));
    }
    if params.output != OutputKind::Recording && params.tracks.is_some() {
        return Err(invalid_params(
            command,
            "`tracks` is for the recording; a stream carries one audio track",
        ));
    }
    if params.output != OutputKind::Monitor && params.enabled.is_some() {
        return Err(invalid_params(
            command,
            "`enabled` is for the monitor; the recording and the stream start and stop",
        ));
    }
    if params.output == OutputKind::Monitor && params.video_encoder.is_some() {
        return Err(invalid_params(
            command,
            "`video_encoder` is for the recording and the stream; the monitor plays audio",
        ));
    }
    let tracks = params
        .tracks
        .map(|tracks| {
            if tracks.len() > MAX_EXTRA_TRACKS {
                return Err(invalid_params(
                    command,
                    format!("at most {MAX_EXTRA_TRACKS} `tracks` besides the mix"),
                ));
            }
            tracks
                .into_iter()
                .map(|bus| {
                    // An id is looked up as one; anything else is a name.
                    if is_valid_id(BUS_ID_PREFIX, &bus) {
                        Ok(BusRef {
                            id: Some(bus),
                            name: None,
                        })
                    } else {
                        valid_name(command, "tracks", bus).map(|name| BusRef {
                            id: None,
                            name: Some(name),
                        })
                    }
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    if params.output != OutputKind::Stream && params.video_bitrate_kbps.is_some() {
        return Err(invalid_params(
            command,
            "`video_bitrate_kbps` is for the stream",
        ));
    }
    Ok(SetOutputProfileRequest {
        output: params.output,
        mix,
        tracks,
        video_encoder: params.video_encoder,
        video_bitrate_kbps: params
            .video_bitrate_kbps
            .map(|kbps| video_bitrate(command, kbps))
            .transpose()?,
        enabled: params.enabled,
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetMonitorDeviceParams {
    /// Output device name from list_audio_outputs; null or absent: the system default.
    device: Option<String>,
}

fn decode_set_monitor_device(
    command: &str,
    params: Option<&Value>,
) -> Result<Option<String>, CommandError> {
    let params: SetMonitorDeviceParams = decode_params(command, params)?;
    if params.device.as_deref().is_some_and(str::is_empty) {
        return Err(invalid_params(command, "`device` must not be empty"));
    }
    Ok(params.device)
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
    /// Screen source id from list_screen_sources: capture it without a picker.
    source_id: Option<String>,
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
    let screen = optional_screen(
        command,
        params.screen,
        params.source_id,
        params.source_types,
        params.cursor,
    )?;
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
    source_id: Option<String>,
    source_types: Option<Vec<VideoSourceKind>>,
    cursor: Option<CursorMode>,
) -> Result<Option<ScreenSelection>, CommandError> {
    if screen.unwrap_or(false) {
        return screen_selection(command, source_id, source_types, cursor).map(Some);
    }
    if source_id.is_some() || source_types.is_some() || cursor.is_some() {
        return Err(invalid_params(
            command,
            "`source_id`, `source_types` and `cursor` require `screen: true`",
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
    plain_file(
        command,
        field,
        path,
        extension,
        "it is written to the engine's output directory",
    )
}

/// A plain file name (no directories) with `extension`; `place` says where
/// the file lives, for the error message.
fn plain_file(
    command: &str,
    field: &str,
    path: String,
    extension: &str,
    place: &str,
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
    // Windows opens a device for these names, whatever the extension (e.g.
    // `CON.json`); refused everywhere, so a name works on every system.
    let device = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(is_windows_device_name);
    if device {
        return Err(invalid_params(
            command,
            format!("`{field}` names a Windows device (CON, PRN, AUX, NUL, COM1-9, LPT1-9)"),
        ));
    }
    if !plain_name || !has_extension || path.file_stem().is_none() {
        return Err(invalid_params(
            command,
            format!(
                "`{field}` must be a plain file name ending in .{extension} \
                 (no directories); {place}"
            ),
        ));
    }
    Ok(path)
}

fn is_windows_device_name(stem: &str) -> bool {
    let stem = stem.to_ascii_uppercase();
    let numbered = |prefix: &str| {
        stem.strip_prefix(prefix).is_some_and(|digit| {
            digit.len() == 1 && ('1'..='9').contains(&digit.chars().next().unwrap_or('0'))
        })
    };
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || numbered("COM") || numbered("LPT")
}

fn decode_screen_probe(
    command: &str,
    params: Option<&Value>,
) -> Result<ScreenCaptureRequest, CommandError> {
    let params: ScreenProbeParams = decode_params(command, params)?;
    Ok(ScreenCaptureRequest {
        selection: screen_selection(
            command,
            params.source_id,
            params.source_types,
            params.cursor,
        )?,
        duration: probe_duration(command, params.duration_ms)?,
    })
}

/// Defaults, deduplication and validation shared by all screen commands.
fn valid_source_id(command: &str, id: String) -> Result<String, CommandError> {
    if id.is_empty() || id.chars().count() > MAX_SOURCE_ID_CHARS || id.chars().any(char::is_control)
    {
        return Err(invalid_params(
            command,
            format!(
                "`source_id` must be an id from list_screen_sources \
                 (1 to {MAX_SOURCE_ID_CHARS} characters)"
            ),
        ));
    }
    Ok(id)
}

fn screen_selection(
    command: &str,
    source_id: Option<String>,
    source_types: Option<Vec<VideoSourceKind>>,
    cursor: Option<CursorMode>,
) -> Result<ScreenSelection, CommandError> {
    let source_id = source_id
        .map(|id| valid_source_id(command, id))
        .transpose()?;
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
        source: source_id,
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
        "create_scene" => of::<SceneNameParams>(),
        "set_active_scene" => of::<SetActiveSceneParams>(),
        "delete_scene" => of::<DeleteSceneParams>(),
        "add_source" => of::<AddSourceParams>(),
        "update_source" => of::<UpdateSourceParams>(),
        "remove_source" => of::<RemoveSourceParams>(),
        "restart_source" => of::<RestartSourceParams>(),
        "set_screen_source" => of::<SetScreenSourceParams>(),
        "save_project" => of::<SaveProjectParams>(),
        "load_project" | "validate_project" => of::<ProjectFileParams>(),
        "add_channel" => of::<AddChannelParams>(),
        "remove_channel" => of::<ChannelParams>(),
        "update_channel" => of::<UpdateChannelParams>(),
        "set_channel_input" => of::<SetChannelInputParams>(),
        "add_bus" => of::<AddBusParams>(),
        "remove_bus" => of::<BusParams>(),
        "update_bus" => of::<UpdateBusParams>(),
        "set_send" => of::<SetSendParams>(),
        "set_output_profile" => of::<SetOutputProfileParams>(),
        "set_monitor_device" => of::<SetMonitorDeviceParams>(),
        "set_preview" => of::<PreviewParams>(),
        // Listed explicitly so a new command without a mapping has no schema
        // (and fails the registry test) instead of an empty one.
        "get_status"
        | "list_commands"
        | "get_capture_capabilities"
        | "get_health"
        | "list_audio_inputs"
        | "list_audio_outputs"
        | "list_screen_sources"
        | "stop_recording"
        | "stop_streaming"
        | "list_scenes"
        | "get_mixer"
        | "get_output_profiles"
        | "list_projects" => {
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
            "remove_source" | "restart_source" => Some(json!({"scene": "Scene", "name": "bars"})),
            "add_channel" => Some(json!({"name": "Mic", "input": {"kind": "device"}})),
            "remove_channel" => Some(json!({"name": "Mic"})),
            "update_channel" => Some(json!({"id": "ch_a1", "mute": true})),
            "set_channel_input" => Some(json!({"name": "Mic", "input": {"kind": "device",
                "device": "alsa_input.usb"}})),
            "add_bus" => Some(json!({"name": "Voice", "assign_db": 0})),
            "remove_bus" => Some(json!({"name": "Voice"})),
            "update_bus" => Some(json!({"id": "bus_program", "gain_db": -3.0})),
            "set_send" => Some(json!({"channel": "Mic", "bus": "Voice", "enabled": true})),
            "set_output_profile" => Some(json!({"output": "stream", "mix_bus": "Voice"})),
            "set_preview" => Some(json!({"enabled": false})),
            "set_screen_source" => Some(json!({"id": "src_a1", "source_id": "monitor:1"})),
            "save_project" | "load_project" | "validate_project" => {
                Some(json!({"file_name": "show.json"}))
            }
            _ => None,
        }
    }

    #[test]
    fn scenes_and_sources_are_named_by_id_or_name() {
        let decode = |command: &str, params: Value| {
            Command::decode(command, Some(&params)).unwrap_or_else(|err| panic!("{err}"))
        };
        assert_eq!(
            decode("set_active_scene", json!({"scene_id": "scene_k3j9x2"})),
            Command::SetActiveScene(SceneRef::id("scene_k3j9x2"))
        );
        assert_eq!(
            decode("delete_scene", json!({"name": "Main", "confirm": true})),
            Command::DeleteScene(SceneRef::name("Main"))
        );
        // Both may be given; the engine checks that they agree.
        assert_eq!(
            decode(
                "set_active_scene",
                json!({"scene_id": "scene_k3j9x2", "name": "Main"})
            ),
            Command::SetActiveScene(SceneRef {
                id: Some("scene_k3j9x2".to_owned()),
                name: Some("Main".to_owned()),
            })
        );
        assert_eq!(
            decode("remove_source", json!({"id": "src_a1"})),
            Command::RemoveSource(SourceRef::id("src_a1"))
        );
        assert_eq!(
            decode("remove_source", json!({"scene": "Main", "name": "bars"})),
            Command::RemoveSource(SourceRef::named("Main", "bars"))
        );
        assert_eq!(
            decode(
                "remove_source",
                json!({"scene_id": "scene_main", "name": "bars"})
            ),
            Command::RemoveSource(SourceRef {
                id: None,
                scene: Some(SceneRef::id("scene_main")),
                name: Some("bars".to_owned()),
            })
        );
        let Command::UpdateSource(update) =
            decode("update_source", json!({"id": "src_a1", "x": 4}))
        else {
            panic!("update_source");
        };
        assert_eq!(update.source, SourceRef::id("src_a1"));
        let Command::AddSource(add) = decode(
            "add_source",
            json!({"scene_id": "scene_main", "name": "a", "kind": "color_bars"}),
        ) else {
            panic!("add_source");
        };
        assert_eq!(add.scene, SceneRef::id("scene_main"));
        let Command::RestartSource { source, .. } =
            decode("restart_source", json!({"id": "src_a1"}))
        else {
            panic!("restart_source");
        };
        assert_eq!(source, SourceRef::id("src_a1"));
    }

    #[test]
    fn project_and_screen_source_params() {
        let decode = |command: &str, params: Value| Command::decode(command, Some(&params));
        assert_eq!(
            decode(
                "save_project",
                json!({"file_name": "show.json", "overwrite": true})
            ),
            Ok(Command::SaveProject(SaveProjectRequest {
                file_name: PathBuf::from("show.json"),
                overwrite: true,
            }))
        );
        assert_eq!(
            decode("save_project", json!({"file_name": "show.JSON"})),
            Ok(Command::SaveProject(SaveProjectRequest {
                file_name: PathBuf::from("show.JSON"),
                overwrite: false,
            }))
        );
        assert_eq!(
            decode("load_project", json!({"file_name": "show.json"})),
            Ok(Command::LoadProject {
                file_name: PathBuf::from("show.json")
            })
        );
        // Plain .json names only: the project directory is the engine's.
        for bad in [
            "CON.json",
            "com3.json",
            "../show.json",
            "/tmp/show.json",
            "projects/show.json",
            "show.mkv",
            ".json",
            "",
        ] {
            for command in ["save_project", "load_project", "validate_project"] {
                let error = decode(command, json!({"file_name": bad})).unwrap_err();
                assert_eq!(error.code, ErrorCode::InvalidParams, "{command} {bad}");
            }
        }
        assert_eq!(
            decode(
                "set_screen_source",
                json!({"id": "src_a1", "source_id": "window:7:0x2a"})
            ),
            Ok(Command::SetScreenSource {
                source: SourceRef::id("src_a1"),
                source_id: "window:7:0x2a".to_owned(),
            })
        );
        for bad in [
            json!({"id": "src_a1"}),
            json!({"id": "src_a1", "source_id": ""}),
            json!({"source_id": "monitor:1"}),
        ] {
            let error = decode("set_screen_source", bad.clone()).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidParams, "{bad}");
        }
        let access = |name: &str| {
            COMMANDS
                .iter()
                .find(|command| command.name == name)
                .map(|command| (command.access, command.destructive))
        };
        assert_eq!(
            access("save_project"),
            Some((CommandAccess::Sensitive, true))
        );
        assert_eq!(
            access("load_project"),
            Some((CommandAccess::Sensitive, true))
        );
        assert_eq!(
            access("validate_project"),
            Some((CommandAccess::ReadOnly, false))
        );
        assert_eq!(
            access("list_projects"),
            Some((CommandAccess::ReadOnly, false))
        );
        assert_eq!(
            decode("list_projects", json!({})),
            Ok(Command::ListProjects)
        );
        assert!(decode("list_projects", json!({"dir": "/tmp"})).is_err());
        assert!(project_file_name("save_project", "show.json").is_ok());
        let device = project_file_name("load_project", "NUL.json").unwrap_err();
        assert_eq!(
            device.details.as_ref().map(|d| &d["command"]),
            Some(&json!("load_project"))
        );
        assert_eq!(
            access("set_screen_source"),
            Some((CommandAccess::Sensitive, false))
        );
        assert!(NAME_RULE.contains(&MAX_NAME_CHARS.to_string()));
    }

    #[test]
    fn restart_source_params() {
        let decode = |params: Value| Command::decode("restart_source", Some(&params));
        assert!(matches!(
            decode(json!({"scene": "Scene", "name": "desk"})),
            Ok(Command::RestartSource {
                source_id: None,
                ..
            })
        ));
        let Ok(Command::RestartSource { source_id, .. }) =
            decode(json!({"scene": "Scene", "name": "desk", "source_id": "window:7:0x2a"}))
        else {
            panic!("decode");
        };
        assert_eq!(source_id.as_deref(), Some("window:7:0x2a"));
        for bad in [
            json!(""),
            json!("a\nb"),
            json!("x".repeat(MAX_SOURCE_ID_CHARS + 1)),
        ] {
            let err =
                decode(json!({"scene": "Scene", "name": "desk", "source_id": bad})).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidParams, "{bad}");
        }
        // It ends the running capture.
        let descriptor = COMMANDS
            .iter()
            .find(|command| command.name == "restart_source")
            .expect("registered");
        assert!(descriptor.destructive);
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
            // A source needs its id, or its scene and name.
            ("update_source", json!({"name": "a", "x": 1})),
            ("remove_source", json!({"scene": "Main"})),
            ("remove_source", json!({"scene_id": "scene_main"})),
            ("restart_source", json!({})),
            ("add_source", json!({"name": "a", "kind": "color_bars"})),
            ("set_active_scene", json!({})),
            // Ids come from list_scenes: the right prefix, a-z, 0-9 or _.
            ("remove_source", json!({"id": "scene_main"})),
            ("remove_source", json!({"id": "src_"})),
            ("remove_source", json!({"id": "src_ABC"})),
            (
                "remove_source",
                json!({"id": format!("src_{}", "a".repeat(61))}),
            ),
            ("set_active_scene", json!({"scene_id": "src_abc"})),
            ("set_active_scene", json!({"scene_id": 7})),
            ("set_preview", json!({})),
        ] {
            let err = Command::decode(command, Some(&params)).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidParams, "{command} {params}");
        }
    }

    #[test]
    fn audio_commands_decode_with_defaults_and_their_rules() {
        let Command::AddChannel(added) = Command::decode(
            "add_channel",
            Some(&json!({"name": "Music", "input": {"kind": "tone", "frequency_hz": 440}})),
        )
        .expect("decode") else {
            panic!("add_channel");
        };
        assert_eq!(
            added,
            AddChannelRequest {
                name: "Music".to_owned(),
                input: ChannelInput::Tone {
                    frequency_hz: 440.0,
                    level_dbfs: DEFAULT_TONE_LEVEL_DBFS,
                },
                gain_db: 0.0,
                muted: false,
                balance: 0.0,
            }
        );
        let Command::SetChannelInput(input) = Command::decode(
            "set_channel_input",
            Some(&json!({"id": "ch_a1", "input": {"kind": "device", "device": null}})),
        )
        .expect("decode") else {
            panic!("set_channel_input");
        };
        assert_eq!(input.channel, ChannelRef::id("ch_a1"));
        assert_eq!(input.input, ChannelInput::Device { device: None });
        let Command::UpdateBus(bus) = Command::decode(
            "update_bus",
            Some(&json!({"name": "Program", "mute": true})),
        )
        .expect("decode") else {
            panic!("update_bus");
        };
        assert_eq!(
            (bus.bus.name.as_deref(), bus.muted, bus.assign_db),
            (Some("Program"), Some(true), None)
        );
        // `assign_db`: absent changes nothing, null makes a separate mix.
        let Command::UpdateBus(separate) = Command::decode(
            "update_bus",
            Some(&json!({"name": "Voice", "assign_db": null})),
        )
        .expect("decode") else {
            panic!("update_bus");
        };
        assert_eq!(separate.assign_db, Some(None));
        let Command::AddBus(added) =
            Command::decode("add_bus", Some(&json!({"name": "Voice"}))).expect("decode")
        else {
            panic!("add_bus");
        };
        assert_eq!(
            added,
            AddBusRequest {
                name: "Voice".to_owned(),
                gain_db: 0.0,
                muted: false,
                assign_db: None,
            }
        );
        let Command::SetSend(send) = Command::decode(
            "set_send",
            Some(
                &json!({"channel_id": "ch_a1", "bus": "Voice", "level_db": -6,
                         "tap": "pre_fader"}),
            ),
        )
        .expect("decode") else {
            panic!("set_send");
        };
        assert_eq!(send.channel, ChannelRef::id("ch_a1"));
        assert_eq!(send.bus.name.as_deref(), Some("Voice"));
        assert_eq!(
            (send.enabled, send.level_db, send.tap),
            (None, Some(-6.0), Some(SendTap::PreFader))
        );
        for (command, params) in [
            ("add_channel", json!({"name": "Mic"})),
            (
                "add_channel",
                json!({"name": "", "input": {"kind": "device"}}),
            ),
            (
                "add_channel",
                json!({"name": "Mic", "input": {"kind": "file"}}),
            ),
            (
                "add_channel",
                json!({"name": "Mic", "input": {"kind": "device", "device": ""}}),
            ),
            (
                "add_channel",
                json!({"name": "Mic", "input": {"kind": "device", "frequency_hz": 1}}),
            ),
            (
                "add_channel",
                json!({"name": "T", "input": {"kind": "tone", "frequency_hz": 24000}}),
            ),
            (
                "add_channel",
                json!({"name": "T", "input": {"kind": "tone", "frequency_hz": 1,
                       "level_dbfs": 3}}),
            ),
            (
                "add_channel",
                json!({"name": "Mic", "input": {"kind": "device"}, "gain_db": 99}),
            ),
            (
                "add_channel",
                json!({"name": "Mic", "input": {"kind": "device"}, "balance": 1.5}),
            ),
            ("remove_channel", json!({})),
            ("remove_channel", json!({"id": "src_mic"})),
            ("update_channel", json!({"name": "Mic"})),
            ("update_channel", json!({"gain_db": 0})),
            ("update_channel", json!({"name": "Mic", "balance": -2})),
            ("set_channel_input", json!({"name": "Mic"})),
            ("update_bus", json!({"id": "bus_program"})),
            ("update_bus", json!({"gain_db": 0})),
            ("update_bus", json!({"id": "ch_program", "gain_db": 0})),
            ("update_bus", json!({"id": "bus_program", "gain_db": -200})),
            ("update_bus", json!({"name": "Voice", "assign_db": 30})),
            ("add_bus", json!({})),
            ("add_bus", json!({"name": "Voice", "assign_db": -200})),
            ("remove_bus", json!({})),
            ("remove_bus", json!({"id": "ch_voice"})),
            ("set_send", json!({"channel": "Mic", "bus": "Voice"})),
            ("set_send", json!({"bus": "Voice", "enabled": true})),
            ("set_send", json!({"channel": "Mic", "enabled": true})),
            (
                "set_send",
                json!({"channel": "Mic", "bus": "Voice", "tap": "sideways"}),
            ),
            (
                "set_send",
                json!({"channel": "Mic", "bus": "Voice", "level_db": 99}),
            ),
            (
                "set_send",
                json!({"channel_id": "bus_mic", "bus": "Voice", "enabled": true}),
            ),
        ] {
            let err = Command::decode(command, Some(&params)).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidParams, "{command} {params}");
        }
    }

    #[test]
    fn output_profiles_decode_with_their_rules() {
        let Command::SetOutputProfile(request) = Command::decode(
            "set_output_profile",
            Some(&json!({"output": "stream", "mix_bus_id": "bus_voice",
                         "video_bitrate_kbps": 4000})),
        )
        .expect("decode") else {
            panic!("set_output_profile");
        };
        assert_eq!(request.output, OutputKind::Stream);
        assert_eq!(
            request.mix.and_then(|bus| bus.id).as_deref(),
            Some("bus_voice")
        );
        assert_eq!(request.video_bitrate_kbps, Some(4000));
        // Track entries are ids when they look like one, names otherwise.
        let Command::SetOutputProfile(request) = Command::decode(
            "set_output_profile",
            Some(&json!({"output": "recording", "tracks": ["bus_voice", "Music"]})),
        )
        .expect("decode") else {
            panic!("set_output_profile");
        };
        let tracks = request.tracks.expect("tracks");
        assert_eq!(tracks[0].id.as_deref(), Some("bus_voice"));
        assert_eq!(tracks[1].name.as_deref(), Some("Music"));
        // The monitor: a bus and on or off.
        let Command::SetOutputProfile(request) = Command::decode(
            "set_output_profile",
            Some(&json!({"output": "monitor", "mix_bus": "Cue", "enabled": true})),
        )
        .expect("decode") else {
            panic!("set_output_profile");
        };
        assert_eq!(
            (request.output, request.enabled),
            (OutputKind::Monitor, Some(true))
        );
        for params in [
            json!({"output": "stream"}),
            json!({"mix_bus": "Voice"}),
            json!({"output": "speakers", "mix_bus": "Voice"}),
            json!({"output": "monitor", "video_encoder": "software"}),
            json!({"output": "monitor", "video_bitrate_kbps": 4000}),
            json!({"output": "monitor", "tracks": ["Voice"]}),
            json!({"output": "recording", "enabled": true}),
            json!({"output": "stream", "enabled": false}),
            json!({"output": "recording", "video_bitrate_kbps": 4000}),
            json!({"output": "stream", "video_bitrate_kbps": 1}),
            json!({"output": "stream", "mix_bus_id": "ch_voice"}),
            json!({"output": "stream", "video_encoder": "quantum"}),
            json!({"output": "stream", "tracks": ["Voice"]}),
            json!({"output": "recording", "tracks": ["", "Voice"]}),
            json!({"output": "recording", "tracks": vec!["Voice"; MAX_EXTRA_TRACKS + 1]}),
        ] {
            let err = Command::decode("set_output_profile", Some(&params)).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidParams, "{params}");
        }
    }

    #[test]
    fn the_monitor_device_is_a_name_or_the_default() {
        let device = |params: Value| match Command::decode("set_monitor_device", Some(&params)) {
            Ok(Command::SetMonitorDevice { device }) => Ok(device),
            Ok(other) => panic!("{other:?}"),
            Err(err) => Err(err.code),
        };
        assert_eq!(
            device(json!({"device": "alsa_output.usb"})),
            Ok(Some("alsa_output.usb".to_owned()))
        );
        assert_eq!(device(json!({"device": null})), Ok(None));
        assert_eq!(device(json!({})), Ok(None));
        assert_eq!(device(json!({"device": ""})), Err(ErrorCode::InvalidParams));
        assert_eq!(
            device(json!({"output": "x"})),
            Err(ErrorCode::InvalidParams)
        );
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
                    source: None,
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
                    source: None,
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
            json!({"source_id": ""}),
            json!({"source_id": "monitor:\n1"}),
            json!({"source_id": "m".repeat(257)}),
            json!({"source_id": 1}),
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
    fn named_screen_sources_are_decoded_everywhere() {
        assert_eq!(
            Command::decode("list_screen_sources", None),
            Ok(Command::ListScreenSources)
        );
        let probe = Command::decode(
            "probe_screen_capture",
            Some(&json!({"source_id": "monitor:1"})),
        );
        let Ok(Command::ProbeScreenCapture(request)) = probe else {
            panic!("{probe:?}");
        };
        assert_eq!(request.selection.source.as_deref(), Some("monitor:1"));

        let recording = Command::decode(
            "start_recording",
            Some(&json!({"file_name": "a.mkv", "screen": true, "source_id": "window:0x2a"})),
        );
        let Ok(Command::StartRecording(request)) = recording else {
            panic!("{recording:?}");
        };
        assert_eq!(
            request.screen.and_then(|screen| screen.source).as_deref(),
            Some("window:0x2a")
        );

        let add = Command::decode(
            "add_source",
            Some(
                &json!({"scene": "S", "name": "desk", "kind": "screen", "source_id": "monitor:2"}),
            ),
        );
        let Ok(Command::AddSource(request)) = add else {
            panic!("{add:?}");
        };
        let SourceKind::Screen { source_id, .. } = &request.kind else {
            panic!("{request:?}");
        };
        assert_eq!(source_id.as_deref(), Some("monitor:2"));
    }

    #[test]
    fn outputs_take_an_encoder_preference() {
        let recording = Command::decode("start_recording", Some(&json!({"file_name": "a.mkv"})));
        let Ok(Command::StartRecording(request)) = recording else {
            panic!("{recording:?}");
        };
        // None: the recording profile's.
        assert_eq!(request.video_encoder, None);
        let software = Command::decode(
            "start_recording",
            Some(&json!({"file_name": "a.mkv", "video_encoder": "software"})),
        );
        let Ok(Command::StartRecording(request)) = software else {
            panic!("{software:?}");
        };
        assert_eq!(request.video_encoder, Some(EncoderPreference::Software));
        let streaming = Command::decode(
            "start_streaming",
            Some(&json!({"url": "rtmp://example.com/app", "video_encoder": "hardware"})),
        );
        let Ok(Command::StartStreaming(request)) = streaming else {
            panic!("{streaming:?}");
        };
        assert_eq!(request.video_encoder, Some(EncoderPreference::Hardware));
        let err = Command::decode(
            "start_recording",
            Some(&json!({"file_name": "a.mkv", "video_encoder": "gpu"})),
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
    }

    #[test]
    fn source_ids_need_a_screen_source() {
        for (name, params) in [
            (
                "start_recording",
                json!({"file_name": "a.mkv", "source_id": "monitor:1"}),
            ),
            ("probe_composite", json!({"source_id": "monitor:1"})),
            (
                "add_source",
                json!({"scene": "S", "name": "b", "kind": "color_bars", "source_id": "monitor:1"}),
            ),
        ] {
            let err = Command::decode(name, Some(&params)).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidParams, "{name}: {params}");
            assert!(err.message.contains("source_id"), "{}", err.message);
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
                    source: None,
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
            Some(&json!({"file_name": "take1.MKV", "screen": true})),
        ) else {
            panic!("decode");
        };
        assert_eq!(request.file_name, PathBuf::from("take1.MKV"));
        assert!(request.screen.is_some());
        // No screen layer, also with `screen: false`: join the pipeline or
        // start it with the active scene.
        for params in [
            json!({"file_name": "a.mkv"}),
            json!({"file_name": "a.mkv", "screen": false}),
        ] {
            let Ok(Command::StartRecording(no_screen)) =
                Command::decode("start_recording", Some(&params))
            else {
                panic!("decode");
            };
            assert_eq!(no_screen.screen, None, "{params}");
        }
        // The audio is the configured mixer's (ADR 0017): per-run audio
        // parameters are unknown fields.
        for field in ["inputs", "tones", "master_gain_db"] {
            let mut params = json!({"file_name": "a.mkv"});
            params[field] = json!([]);
            let err = Command::decode("start_recording", Some(&params)).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidParams, "{field}");
            assert!(err.message.contains(field), "{}", err.message);
            let mut params = json!({"url": "rtmp://host/app"});
            params[field] = json!([]);
            let err = Command::decode("start_streaming", Some(&params)).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidParams, "{field}");
        }
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
        assert_eq!(request.video_bitrate_kbps, Some(4500));
        assert_eq!(request.reconnect.max_attempts, 0);
        assert_eq!(request.reconnect.initial_delay, Duration::from_millis(250));
        assert_eq!(request.screen, None);
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
        // None: the stream profile's.
        assert_eq!(defaults.video_bitrate_kbps, None);
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
        assert_eq!(add["required"], json!(["name", "kind"]));
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
        assert_eq!(request.video_bitrate_kbps, Some(3000));
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
        assert_eq!(access("get_health"), Some(CommandAccess::ReadOnly));
        // Restarting starts a capture again, like adding a screen source.
        assert_eq!(access("restart_source"), Some(CommandAccess::Sensitive));
        // Window titles can reveal private content.
        assert_eq!(
            access("list_screen_sources"),
            Some(CommandAccess::Sensitive)
        );
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
