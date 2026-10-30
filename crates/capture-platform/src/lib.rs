//! Selects the capture parts for the target platform.
//!
//! The single place that maps `cfg(target_os)` to a [`Capture`], so
//! every composition root (CLI, headless engine, UI, MCP server) uses the
//! same parts, and that gives the programs without a UI framework what
//! capture needs from their main thread ([`run_with_main_run_loop`]).
//! See `docs/platforms.md` for what each backend offers.

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

/// Runs a program's work, called from `main` by the programs without a
/// UI framework of their own (the CLI, the engine process, the MCP
/// server), and returns its result. On macOS the work runs on a thread
/// of its own while the main thread runs the main run loop, which
/// ScreenCaptureKit's picker needs (ADR 0020); a panic in the work
/// continues on the main thread. Elsewhere it runs the work directly.
pub fn run_with_main_run_loop<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    #[cfg(target_os = "macos")]
    {
        capture_macos::run_with_main_run_loop(work)
    }
    #[cfg(not(target_os = "macos"))]
    {
        work()
    }
}
