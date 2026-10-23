//! Typed command vocabulary shared by the CLI, the desktop UI, the JSON-lines
//! protocol and the MCP server.
//!
//! Adapters decode their input into a [`Command`], pass it to a
//! [`CommandHandler`], and encode the returned [`CommandOutput`] or
//! [`CommandError`]. Business logic lives behind the handler, never in an
//! adapter. This crate knows nothing about transports or engine internals.

mod access;
mod audio;
mod composite;
mod error;
mod health;
mod mix;
mod outputs;
mod project;
mod registry;
mod scenes;

pub use access::{AccessPolicy, Gated, label as access_label};
pub use audio::{
    AddBusRequest, AddChannelRequest, AudioGraphSpec, BUS_ID_PREFIX, BusRef, BusSpec, BusState,
    CHANNEL_ID_PREFIX, ChannelInput, ChannelRef, ChannelSpec, ChannelState, MAX_AUX_BUSES,
    MAX_CHANNELS, MixerState, PROGRAM_BUS_ID, PROGRAM_BUS_NAME, ProgramBusSpec, SendSpec, SendTap,
    SetChannelInputRequest, SetSendRequest, UpdateBusRequest, UpdateChannelRequest,
};
pub use composite::{CompositeReport, CompositeRequest, ErrorSummary, SnapshotResult};
pub use error::{CommandError, ErrorCode};
pub use health::{CaptureHealth, CaptureKind, HealthReport, OutputHealth};
pub use mix::{CaptureOutcome, LiveInputSpec, MAX_MIX_DURATION, MixReport, MixRequest, ToneSpec};
pub use outputs::{
    MediaSources, PipelineReport, RecordingReport, RecordingRequest, RecordingStarted,
    SceneCaptureOutcome, StreamingReport, StreamingRequest, StreamingStarted,
};
pub use project::{
    ProjectLoaded, ProjectProblem, ProjectSaved, ProjectValidation, SaveProjectRequest,
};
pub use registry::{
    CommandAccess, CommandDescriptor, CommandList, DEFAULT_OUTPUT_TONE_DBFS, DEFAULT_TONE_HZ,
    NAME_RULE, decode_streaming_to, is_valid_id, is_valid_name, params_schema, registry,
};
pub use scenes::{
    AddSourceRequest, MAX_ID_LEN, MAX_NAME_CHARS, MAX_SCENE_SOURCES, MAX_SCENES, PreviewState,
    Rgba, SCENE_ID_PREFIX, SOURCE_ID_PREFIX, SceneList, SceneRef, SceneSpec, SourceKind,
    SourceRect, SourceRef, SourceSpec, UpdateSourceRequest,
};

/// Live mix snapshot types in [`HealthReport`], for adapters that do not
/// use `audio-engine`.
pub use audio_engine::{LiveInputStats, LiveMixStats};
use std::path::PathBuf;

use capture_api::{
    AudioCaptureReport, AudioCaptureRequest, AudioInput, CaptureCapabilities, ScreenCaptureRequest,
    VideoCaptureReport,
};
/// Capture types in results, for adapters that do not use `capture-api`.
pub use capture_api::{CaptureState, ScreenSource};
/// Output encoder types, for adapters that do not use `encoder-api`.
pub use encoder_api::{
    EncoderPreference, LiveOutputStats, SkippedEncoder, StreamState, VideoEncoderInfo,
};
use serde::{Deserialize, Serialize};
/// Live render snapshot types in [`HealthReport`], for adapters that do not
/// use `video-compositor` (`LatencySummary` is media-core's).
pub use video_compositor::{LatencySummary, LiveRenderStats, LiveSourceStats, SourceId};

/// A typed engine command.
///
/// Wire names and parameters are defined by the [`registry`]; use
/// [`Command::decode`] to build a command from untyped input.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Report engine status.
    GetStatus,
    /// List all commands supported by this engine.
    ListCommands,
    /// Query screen and audio capture capabilities of the running system.
    GetCaptureCapabilities,
    /// Live counters of the pipeline and the open outputs.
    GetHealth,
    /// List audio input devices.
    ListAudioInputs,
    /// List screen sources that can be captured by id (no picker).
    ListScreenSources,
    /// Capture a screen source (named or user-picked) briefly and report diagnostics.
    ProbeScreenCapture(ScreenCaptureRequest),
    /// Capture an audio input briefly and report diagnostics; no audio is stored.
    ProbeAudioCapture(AudioCaptureRequest),
    /// Run the compositor on the reference scene and report render timing.
    ProbeComposite(CompositeRequest),
    /// Mix live inputs and test tones and report meters and buffering.
    ProbeAudioMix(MixRequest),
    /// Start recording to a new file; returns immediately.
    StartRecording(RecordingRequest),
    /// Stop the running recording and finalize its file.
    StopRecording,
    /// Start a live stream; returns once connected.
    StartStreaming(StreamingRequest),
    /// Stop the live stream and report its connection history.
    StopStreaming,
    /// List scenes and the active one.
    ListScenes,
    CreateScene {
        name: String,
    },
    /// Delete a scene that is not active (requires `confirm: true`).
    DeleteScene(SceneRef),
    /// Make a scene the one the pipeline renders (live if running).
    SetActiveScene(SceneRef),
    AddSource(AddSourceRequest),
    UpdateSource(UpdateSourceRequest),
    RemoveSource(SourceRef),
    /// Restart a screen source's capture while outputs keep running.
    RestartSource {
        source: SourceRef,
        /// Capture this screen source from now on (the scene keeps it).
        source_id: Option<String>,
    },
    /// Choose what a screen source captures while it has no running
    /// capture (e.g. after loading a project, which does not store it).
    SetScreenSource {
        source: SourceRef,
        source_id: String,
    },
    /// Write the scenes and audio setup to a project file.
    SaveProject(SaveProjectRequest),
    /// Replace the scenes and audio setup with a project file's (only while
    /// the pipeline is idle).
    LoadProject {
        file_name: PathBuf,
    },
    /// Check a project file without loading it.
    ValidateProject {
        file_name: PathBuf,
    },
    /// Channels and buses with their settings and peak levels.
    GetMixer,
    /// Add a channel (only while the pipeline is idle).
    AddChannel(AddChannelRequest),
    /// Remove a channel (only while the pipeline is idle).
    RemoveChannel(ChannelRef),
    /// Change a channel's fader, mute or balance (live if running).
    UpdateChannel(UpdateChannelRequest),
    /// Choose what a channel plays (only while the pipeline is idle).
    SetChannelInput(SetChannelInputRequest),
    /// Add an aux bus (only while the pipeline is idle).
    AddBus(AddBusRequest),
    /// Remove an aux bus and the sends to it (only while the pipeline is
    /// idle).
    RemoveBus(BusRef),
    /// Change a bus's fader, mute or assignment (live if running).
    UpdateBus(UpdateBusRequest),
    /// Change a channel's send to a bus (live if running).
    SetSend(SetSendRequest),
    /// Run the pipeline for a preview (frames go to the in-process
    /// preview mailbox) or stop doing so.
    SetPreview {
        enabled: bool,
    },
}

/// Structured result of a successfully handled [`Command`].
///
/// Serializes as the inner value, so JSON adapters emit the result object
/// directly.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum CommandOutput {
    Status(EngineStatus),
    Health(Box<HealthReport>),
    Commands(CommandList),
    CaptureCapabilities(CaptureCapabilities),
    AudioInputs(AudioInputList),
    ScreenSources(ScreenSourceList),
    VideoCaptureReport(VideoCaptureReport),
    AudioCaptureReport(AudioCaptureReport),
    CompositeReport(Box<CompositeReport>),
    MixReport(Box<MixReport>),
    RecordingStarted(RecordingStarted),
    RecordingReport(Box<RecordingReport>),
    StreamingStarted(StreamingStarted),
    StreamingReport(Box<StreamingReport>),
    SceneList(SceneList),
    MixerState(MixerState),
    PreviewState(Box<PreviewState>),
    ProjectSaved(ProjectSaved),
    ProjectLoaded(Box<ProjectLoaded>),
    ProjectValidation(ProjectValidation),
}

/// Result of `list_audio_inputs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioInputList {
    pub inputs: Vec<AudioInput>,
}

/// Result of `list_screen_sources`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScreenSourceList {
    pub sources: Vec<ScreenSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EngineStatus {
    pub application: String,
    pub version: String,
    /// The media engine (compositor, mixer, encoders) is built in and can
    /// start outputs. Always true since M5; kept for protocol version 1.
    pub media_engine_ready: bool,
    /// A media pipeline runs (for outputs or the preview).
    pub pipeline_running: bool,
    pub capture_available: bool,
    /// A stream is open and connected, connecting or reconnecting.
    pub streaming_active: bool,
    /// State of the open stream, if any (`failed`: reconnects exhausted;
    /// `stop_streaming` reports why).
    pub streaming_state: Option<StreamState>,
    /// A recording is open and its workers are running.
    pub recording_active: bool,
    /// A recording is open but stopped unexpectedly (encoder, compositor or
    /// mixer failure); `stop_recording` finalizes it and reports the error.
    pub recording_failed: bool,
    /// The preview keeps the pipeline running (`set_preview`).
    pub preview_active: bool,
}

/// Executes typed commands.
///
/// Implemented by the engine controller and called by every adapter, so CLI,
/// protocol server, UI, and MCP share one implementation of each command.
pub trait CommandHandler {
    fn handle(&mut self, command: Command) -> Result<CommandOutput, CommandError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_serializes_as_structured_json() {
        let status = EngineStatus {
            application: "StreamForge".to_owned(),
            version: "0.1.0".to_owned(),
            media_engine_ready: true,
            pipeline_running: false,
            capture_available: false,
            streaming_active: false,
            streaming_state: None,
            recording_active: false,
            recording_failed: false,
            preview_active: false,
        };
        let value = serde_json::to_value(CommandOutput::Status(status)).expect("serialize status");
        assert_eq!(
            value,
            serde_json::json!({
                "application": "StreamForge",
                "version": "0.1.0",
                "media_engine_ready": true,
                "pipeline_running": false,
                "capture_available": false,
                "streaming_active": false,
                "streaming_state": null,
                "recording_active": false,
                "recording_failed": false,
                "preview_active": false,
            })
        );
    }
}
