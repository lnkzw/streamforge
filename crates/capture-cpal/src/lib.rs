//! Audio capture and playback through cpal: WASAPI on Windows, CoreAudio
//! on macOS.
//!
//! [`CpalCapture`] implements the audio half of [`capture_api::CaptureBackend`]
//! (inputs, probes, continuous streams into a bounded `AudioRing`; outputs
//! and playback of the monitor's feed). Screen capture comes from
//! `capture-windows` and `capture-macos`; `capture-platform` combines them
//! with this backend. The device format is converted in the callback: to
//! the requested rate and stereo for capture ([`Converter`]), from the
//! feed's for playback ([`PlaybackConverter`]).

mod convert;
// Used by the backend (Windows, macOS); tested everywhere.
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
mod liveness;

pub use convert::{Converter, PlaybackConverter};

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod backend;
#[cfg(any(target_os = "windows", target_os = "macos"))]
mod playback;

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub use backend::CpalCapture;
