//! Audio capture through cpal: WASAPI on Windows, CoreAudio on macOS.
//!
//! [`CpalCapture`] implements the audio half of [`capture_api::CaptureBackend`]
//! (inputs, probes, continuous streams into a bounded `AudioRing`). Screen
//! capture comes from `capture-windows` and `capture-macos`;
//! `capture-platform` combines them with this backend. The device format is
//! converted to the requested rate and stereo in the callback
//! ([`Converter`]).

mod convert;
// Used by the backend (Windows, macOS); tested everywhere.
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
mod liveness;

pub use convert::Converter;

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod backend;

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub use backend::CpalCapture;
