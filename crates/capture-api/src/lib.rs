//! Platform-neutral capture interface.
//!
//! Defines what a capture backend can report and do, independent of any OS
//! API. Platform crates (`capture-linux`, `capture-cpal`, `capture-windows`) implement
//! [`CaptureBackend`]; the engine only talks to the trait.

mod backend;
mod error;
mod sink;
mod stats;
mod types;

pub use backend::{AudioStream, CaptureBackend, SplitCapture, UnsupportedCapture, VideoStream};
pub use error::{CaptureError, CaptureErrorKind};
pub use sink::{FrameSink, MappedFrame};
pub use stats::{
    AudioStats, AudioStatsAccumulator, BufferKind, FrameObservation, IntervalStats, StreamPosition,
    VideoStats, VideoStatsAccumulator,
};
pub use types::{
    AudioCaptureCapability, AudioCaptureReport, AudioCaptureRequest, AudioFormatInfo, AudioInput,
    AudioStreamRequest, CaptureCapabilities, CursorMode, DEFAULT_PROBE_DURATION,
    MAX_PROBE_DURATION, MIN_PROBE_DURATION, Rational, ScreenCaptureCapability,
    ScreenCaptureRequest, ScreenSelection, ScreenSource, VideoCaptureReport, VideoFormatInfo,
    VideoSourceInfo, VideoSourceKind,
};
