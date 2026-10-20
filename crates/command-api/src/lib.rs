//! Typed command vocabulary shared by CLI, UI, JSON-RPC, and future MCP adapters.
//!
//! Adapters decode their input into a [`Command`], pass it to a
//! [`CommandHandler`], and encode the returned [`CommandOutput`] or
//! [`CommandError`]. Business logic lives behind the handler, never in an
//! adapter. This crate knows nothing about transports or engine internals.

mod access;
mod composite;
mod error;
mod mix;
mod outputs;
mod registry;
mod scenes;

pub use access::{AccessPolicy, Gated, label as access_label};
pub use composite::{CompositeReport, CompositeRequest, ErrorSummary, SnapshotResult};
pub use error::{CommandError, ErrorCode};
pub use mix::{CaptureOutcome, LiveInputSpec, MAX_MIX_DURATION, MixReport, MixRequest, ToneSpec};
pub use outputs::{
    MediaSources, PipelineReport, RecordingReport, RecordingRequest, RecordingStarted,
    SceneCaptureOutcome, StreamingReport, StreamingRequest, StreamingStarted,
};
pub use registry::{
    CommandAccess, CommandDescriptor, CommandList, decode_streaming_to, default_sources,
    params_schema, registry,
};
pub use scenes::{
    AddSourceRequest, AudioSetup, MAX_NAME_CHARS, MAX_SCENE_SOURCES, MAX_SCENES, MixerState,
    PreviewState, Rgba, SceneList, SceneSpec, SourceKind, SourceRect, SourceSpec, StripChange,
    StripState, UpdateSourceRequest,
};

/// Entries of [`ScreenSourceList`], for adapters that do not use `capture-api`.
pub use capture_api::ScreenSource;
use capture_api::{
    AudioCaptureReport, AudioCaptureRequest, AudioInput, CaptureCapabilities, ScreenCaptureRequest,
    VideoCaptureReport,
};
use encoder_api::StreamState;
use serde::{Deserialize, Serialize};

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
    DeleteScene {
        name: String,
    },
    /// Make a scene the one the pipeline renders (live if running).
    SetActiveScene {
        name: String,
    },
    AddSource(AddSourceRequest),
    UpdateSource(UpdateSourceRequest),
    RemoveSource {
        scene: String,
        name: String,
    },
    /// Mixer strips, gains and peak levels.
    GetMixer,
    SetMixerStrip(StripChange),
    SetMasterGain {
        gain_db: f32,
    },
    /// Configure audio inputs and tones (only while the pipeline is idle).
    SetAudioSources(AudioSetup),
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
    pub media_engine_ready: bool,
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
            media_engine_ready: false,
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
                "media_engine_ready": false,
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
