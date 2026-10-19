//! Platform-neutral capture interface.
//!
//! Defines what a capture backend can report and do, independent of any OS
//! API. Platform crates (`capture-linux`, later Windows/macOS) implement
//! [`CaptureBackend`]; the engine only talks to the trait.

mod backend;
mod error;
mod stats;
mod types;

pub use backend::{CaptureBackend, UnsupportedCapture};
pub use error::{CaptureError, CaptureErrorKind};
pub use stats::{
    AudioStats, AudioStatsAccumulator, BufferKind, FrameObservation, IntervalStats, StreamPosition,
    VideoStats, VideoStatsAccumulator,
};
pub use types::{
    AudioCaptureCapability, AudioCaptureReport, AudioCaptureRequest, AudioFormatInfo, AudioInput,
    CaptureCapabilities, CursorMode, DEFAULT_PROBE_DURATION, MAX_PROBE_DURATION,
    MIN_PROBE_DURATION, Rational, ScreenCaptureCapability, ScreenCaptureRequest,
    VideoCaptureReport, VideoFormatInfo, VideoSourceInfo, VideoSourceKind,
};
