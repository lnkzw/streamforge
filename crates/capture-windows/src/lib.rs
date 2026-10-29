//! Native capture on Windows: screens (Windows.Graphics.Capture) and
//! cameras (Media Foundation).
//!
//! Monitors and top-level windows are listed by
//! [`capture_api::ScreenCapture::screen_sources`] and captured by id;
//! there is no system picker yet. Cameras are listed by
//! [`capture_api::CameraCapture::cameras`] and named by their device's
//! symbolic link (ADR 0019). Audio comes from `capture-cpal`
//! (`capture-platform` combines them). Only the id format and the camera
//! formats build on other platforms, so they are tested everywhere.

#[cfg(target_os = "windows")]
mod camera;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
mod camera_formats;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
mod ids;
#[cfg(target_os = "windows")]
mod mf;
#[cfg(target_os = "windows")]
mod wgc;

#[cfg(target_os = "windows")]
pub use camera::MfCamera;
#[cfg(target_os = "windows")]
pub use wgc::WgcCapture;
