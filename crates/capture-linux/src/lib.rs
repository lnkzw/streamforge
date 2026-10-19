//! Linux capture backend: XDG desktop portal ScreenCast + PipeWire.
//!
//! - Screen: the portal shows the compositor's picker and hands out a PipeWire
//!   node; frames arrive as shared-memory buffers with optional header metadata.
//! - Audio: PipeWire source nodes are enumerated through the registry and
//!   captured as interleaved F32 at their native rate and channel count.
//!
//! Probes run a PipeWire main loop on the calling thread for a bounded time;
//! callers must not invoke them from a UI thread. Screen streams run their
//! loop on a dedicated `screen-capture` thread. See
//! `docs/capture-linux.md` for system requirements (Hyprland in particular).
#![cfg(target_os = "linux")]

mod audio;
mod formats;
mod portal;
mod pw;
mod screen;
mod video;

use std::time::Duration;

use capture_api::{
    AudioCaptureCapability, AudioCaptureReport, AudioCaptureRequest, AudioInput, CaptureBackend,
    CaptureCapabilities, CaptureError, CaptureErrorKind, ScreenCaptureCapability,
    ScreenCaptureRequest, ScreenSelection, VideoCaptureReport, VideoStream,
};
use serde_json::json;

/// Upper bound for each capability query. Screen and audio are queried in
/// parallel, so `capabilities()` (and the first `get_status`) waits at most
/// this long.
const QUERY_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Default)]
pub struct LinuxCapture {
    _private: (),
}

impl LinuxCapture {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CaptureBackend for LinuxCapture {
    fn capabilities(&mut self) -> CaptureCapabilities {
        let (portal, audio) = std::thread::scope(|scope| {
            let portal = scope.spawn(|| portal::query(QUERY_TIMEOUT));
            // PipeWire objects are !Send; they stay on this thread.
            let audio = pw::server_version(QUERY_TIMEOUT);
            let portal = portal.join().unwrap_or_else(|_| {
                Err(CaptureError::backend(
                    "the portal capability query panicked",
                ))
            });
            (portal, audio)
        });
        let screen = match portal {
            Ok(info) => ScreenCaptureCapability {
                backend: Some(screen::BACKEND.to_owned()),
                available: !info.source_kinds.is_empty(),
                unavailable_reason: info
                    .source_kinds
                    .is_empty()
                    .then(|| "the ScreenCast portal advertises no source types".to_owned()),
                portal_version: Some(info.version),
                source_kinds: info.source_kinds,
                cursor_modes: info.cursor_modes,
            },
            Err(err) => ScreenCaptureCapability::unavailable(Some(screen::BACKEND), err.message),
        };
        let audio = match audio {
            Ok(version) => AudioCaptureCapability {
                backend: Some(pw::BACKEND.to_owned()),
                available: true,
                unavailable_reason: None,
                server_version: Some(version),
            },
            Err(err) => AudioCaptureCapability::unavailable(Some(pw::BACKEND), err.message),
        };
        CaptureCapabilities {
            platform: std::env::consts::OS.to_owned(),
            desktop: std::env::var("XDG_CURRENT_DESKTOP").ok(),
            session_type: std::env::var("XDG_SESSION_TYPE").ok(),
            screen,
            audio,
        }
    }

    fn audio_inputs(&mut self) -> Result<Vec<AudioInput>, CaptureError> {
        pw::audio_inputs(QUERY_TIMEOUT)
    }

    fn probe_screen(
        &mut self,
        request: &ScreenCaptureRequest,
    ) -> Result<VideoCaptureReport, CaptureError> {
        screen::probe(&request.selection, request.duration)
    }

    fn start_screen_stream(
        &mut self,
        selection: &ScreenSelection,
    ) -> Result<Box<dyn VideoStream>, CaptureError> {
        screen::start_stream(selection)
    }

    fn probe_audio(
        &mut self,
        request: &AudioCaptureRequest,
    ) -> Result<AudioCaptureReport, CaptureError> {
        if let Some(name) = request.input.as_deref() {
            // Check first: PipeWire would silently fall back to the default
            // input for an unknown target.
            let inputs = pw::audio_inputs(QUERY_TIMEOUT)?;
            if !inputs.iter().any(|input| input.name == name) {
                return Err(CaptureError::new(
                    CaptureErrorKind::NotFound,
                    format!("audio input `{name}` not found; see list_audio_inputs"),
                )
                .with_details(json!({
                    "input": name,
                    "available": inputs.iter().map(|input| &input.name).collect::<Vec<_>>(),
                })));
            }
        }
        let run = audio::capture(request.input.as_deref(), request.duration)?;
        Ok(AudioCaptureReport {
            backend: pw::BACKEND.to_owned(),
            input: request.input.clone(),
            format: run.format,
            stats: run.stats,
            warnings: run.warnings,
        })
    }
}
