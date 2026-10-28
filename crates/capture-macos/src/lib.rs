//! Native screen capture on macOS (ScreenCaptureKit).
//!
//! Displays and windows are listed by
//! [`capture_api::ScreenCapture::screen_sources`] and captured by id;
//! there is no system picker yet. Audio comes from `capture-cpal`
//! (`capture-platform` combines the two). Only the id format builds on
//! other platforms, so it is tested everywhere. The single `unsafe` call
//! (reading a locked pixel buffer) is isolated in `pixels`.

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod ids;
#[cfg(target_os = "macos")]
mod pixels;
#[cfg(target_os = "macos")]
mod sck;

#[cfg(target_os = "macos")]
pub use sck::SckCapture;
