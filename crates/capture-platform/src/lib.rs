//! Selects the capture parts for the target platform.
//!
//! The single place that maps `cfg(target_os)` to a [`Capture`], so
//! every composition root (CLI, headless engine, UI, MCP server) uses the
//! same parts. See `docs/platforms.md` for what each backend offers.

use capture_api::Capture;

/// Capture parts for the platform this binary was built for.
pub fn capture() -> Capture {
    #[cfg(target_os = "linux")]
    {
        Capture {
            screen: Box::new(capture_linux::PortalScreen::new()),
            audio: Box::new(capture_linux::PipeWireAudio::new()),
            camera: Box::new(capture_linux::PipeWireCamera::new()),
        }
    }
    #[cfg(target_os = "windows")]
    {
        Capture {
            screen: Box::new(capture_windows::WgcCapture::new()),
            audio: Box::new(capture_cpal::CpalCapture::new()),
            camera: Box::new(capture_windows::MfCamera::new()),
        }
    }
    #[cfg(target_os = "macos")]
    {
        Capture {
            screen: Box::new(capture_macos::SckCapture::new()),
            audio: Box::new(capture_cpal::CpalCapture::new()),
            camera: Box::new(capture_macos::AvfCamera::new()),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        Capture::for_unsupported_platform()
    }
}
