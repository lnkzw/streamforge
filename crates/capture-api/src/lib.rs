//! Platform-neutral capture interface.
//!
//! Defines what a capture backend can report and do, independent of any OS
//! API: capture, and audio playback for the monitor. Platform crates
//! (`capture-linux`, `capture-cpal`, `capture-windows`, `capture-macos`)
//! implement [`ScreenCapture`], [`AudioDevices`] and [`CameraCapture`]
//! (Linux only so far), and `capture-platform` groups one of each into a
//! [`Capture`], the only thing the engine talks to.

mod backend;
mod error;
mod sink;
mod stats;
mod types;

pub use backend::{
    AudioDevices, AudioStream, CameraCapture, CameraStream, Capture, PlaybackStream, ScreenCapture,
    UnsupportedCapture, VideoStream,
};
pub use error::{CaptureError, CaptureErrorKind};
pub use sink::{FrameSink, MappedFrame};
pub use stats::{
    AudioStats, AudioStatsAccumulator, BufferKind, FrameObservation, IntervalStats, StreamPosition,
    VideoStats, VideoStatsAccumulator,
};
pub use types::{
    AudioCaptureCapability, AudioCaptureReport, AudioCaptureRequest, AudioFormatInfo, AudioInput,
    AudioOutput, AudioStreamRequest, Camera, CameraCaptureCapability, CameraMode, CameraRequest,
    CaptureCapabilities, CaptureEnd, CaptureState, CursorMode, DEFAULT_PROBE_DURATION,
    MAX_PROBE_DURATION, MIN_PROBE_DURATION, PlaybackRequest, Rational, ScreenCaptureCapability,
    ScreenCaptureRequest, ScreenSelection, ScreenSource, VideoCaptureReport, VideoFormatInfo,
    VideoSourceInfo, VideoSourceKind,
};
