//! Selects the capture backend for the target platform.
//!
//! The single place that maps `cfg(target_os)` to a [`CaptureBackend`], so
//! every composition root (CLI, headless engine, future UI) uses the same one.

use capture_api::CaptureBackend;

/// Capture backend for the platform this binary was built for.
pub fn backend() -> Box<dyn CaptureBackend> {
    #[cfg(target_os = "linux")]
    {
        Box::new(capture_linux::LinuxCapture::new())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Box::new(capture_api::UnsupportedCapture::for_current_platform())
    }
}
