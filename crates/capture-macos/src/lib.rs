//! Native capture on macOS: screens (ScreenCaptureKit) and cameras
//! (AVFoundation).
//!
//! Displays and windows are listed by
//! [`capture_api::ScreenCapture::screen_sources`] and captured by id, or
//! picked in the system picker (`SCContentSharingPicker`) where a main run
//! loop runs. Cameras are listed by
//! [`capture_api::CameraCapture::cameras`] and named by their device's
//! unique id (ADR 0019). Audio comes from `capture-cpal`
//! (`capture-platform` combines them). Only the id format and the camera
//! modes build on other platforms, so they are tested everywhere.
//! [`run_with_main_run_loop`] gives programs without AppKit a main run
//! loop (ADR 0020). The
//! `unsafe` code is isolated: reading locked pixel buffers in `pixels`,
//! every AVFoundation call in `avf`.

#[cfg(target_os = "macos")]
mod avf;
#[cfg(target_os = "macos")]
mod camera;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod camera_modes;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod ids;
#[cfg(target_os = "macos")]
mod picker;
#[cfg(target_os = "macos")]
mod pixels;
#[cfg(target_os = "macos")]
mod run_loop;
#[cfg(target_os = "macos")]
mod sck;

#[cfg(target_os = "macos")]
pub use camera::AvfCamera;
#[cfg(target_os = "macos")]
pub use run_loop::run_with_main_run_loop;
#[cfg(target_os = "macos")]
pub use sck::SckCapture;
