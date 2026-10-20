//! Selects the capture backend for the target platform.
//!
//! The single place that maps `cfg(target_os)` to a [`CaptureBackend`], so
//! every composition root (CLI, headless engine, UI, MCP server) uses the
//! same one. See `docs/platforms.md` for what each backend offers.

use capture_api::CaptureBackend;

/// Capture backend for the platform this binary was built for.
pub fn backend() -> Box<dyn CaptureBackend> {
    #[cfg(target_os = "linux")]
    {
        Box::new(capture_linux::LinuxCapture::new())
    }
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        Box::new(capture_cpal::CpalCapture::new())
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        Box::new(capture_api::UnsupportedCapture::for_current_platform())
    }
}
