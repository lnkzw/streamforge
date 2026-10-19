//! Typed command vocabulary shared by CLI, UI, JSON-RPC, and future MCP adapters.
//!
//! Adapters decode their input into a [`Command`], pass it to a
//! [`CommandHandler`], and encode the returned [`CommandOutput`] or
//! [`CommandError`]. Business logic lives behind the handler, never in an
//! adapter. This crate knows nothing about transports or engine internals.

mod error;
mod registry;

pub use error::{CommandError, ErrorCode};
pub use registry::{CommandAccess, CommandDescriptor, CommandList, registry};

use capture_api::{
    AudioCaptureReport, AudioCaptureRequest, AudioInput, CaptureCapabilities, ScreenCaptureRequest,
    VideoCaptureReport,
};
use serde::{Deserialize, Serialize};

/// A typed engine command.
///
/// Wire names and parameters are defined by the [`registry`]; use
/// [`Command::decode`] to build a command from untyped input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Report engine status.
    GetStatus,
    /// List all commands supported by this engine.
    ListCommands,
    /// Query screen and audio capture capabilities of the running system.
    GetCaptureCapabilities,
    /// List audio input devices.
    ListAudioInputs,
    /// Capture a user-selected screen source briefly and report diagnostics.
    ProbeScreenCapture(ScreenCaptureRequest),
    /// Capture an audio input briefly and report diagnostics; no audio is stored.
    ProbeAudioCapture(AudioCaptureRequest),
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
    VideoCaptureReport(VideoCaptureReport),
    AudioCaptureReport(AudioCaptureReport),
}

/// Result of `list_audio_inputs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioInputList {
    pub inputs: Vec<AudioInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EngineStatus {
    pub application: String,
    pub version: String,
    pub media_engine_ready: bool,
    pub capture_available: bool,
    pub streaming_active: bool,
    pub recording_active: bool,
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
            recording_active: false,
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
                "recording_active": false,
            })
        );
    }
}
