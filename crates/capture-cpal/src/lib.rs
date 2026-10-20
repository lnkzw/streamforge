//! Audio capture through cpal: WASAPI on Windows, CoreAudio on macOS.
//!
//! [`CpalCapture`] implements the audio half of [`capture_api::CaptureBackend`]
//! (inputs, probes, continuous streams into a bounded `AudioRing`); screen
//! capture on these platforms comes with a later M9 step and reports
//! `unsupported` until then. The device format is converted to the
//! requested rate and stereo in the callback ([`Converter`]).

mod convert;

pub use convert::Converter;

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod backend;

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub use backend::CpalCapture;
