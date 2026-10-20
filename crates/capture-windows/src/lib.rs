//! Native screen capture on Windows (Windows.Graphics.Capture).
//!
//! Monitors and top-level windows are listed by
//! [`capture_api::CaptureBackend::screen_sources`] and captured by id;
//! there is no system picker yet. Audio comes from `capture-cpal`
//! (`capture-platform` combines the two). Only the id format builds on
//! other platforms, so it is tested everywhere.

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
mod ids;
#[cfg(target_os = "windows")]
mod wgc;

#[cfg(target_os = "windows")]
pub use wgc::WgcCapture;
