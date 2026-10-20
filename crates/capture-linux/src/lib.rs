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

use std::time::{Duration, Instant};

use capture_api::{
    AudioCaptureCapability, AudioCaptureReport, AudioCaptureRequest, AudioInput, AudioStream,
    AudioStreamRequest, CaptureBackend, CaptureCapabilities, CaptureError, ScreenCaptureCapability,
    ScreenCaptureRequest, ScreenSelection, VideoCaptureReport, VideoStream,
};

/// Upper bound for each portal or PipeWire query. Screen and audio
/// capabilities are queried in parallel, so `capabilities()` (and the first
/// `get_status`) waits at most this long.
pub(crate) const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a listing of audio inputs is reused for name checks, so starting
/// several streams does not rescan the PipeWire registry each time.
const INPUT_CACHE_TTL: Duration = Duration::from_secs(2);

#[derive(Debug, Default)]
pub struct LinuxCapture {
    input_cache: Option<(Instant, Vec<AudioInput>)>,
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
                // The portal always asks the user; it cannot capture a named source.
                picker: true,
                source_list: false,
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
        let inputs = pw::audio_inputs(QUERY_TIMEOUT)?;
        self.input_cache = Some((Instant::now(), inputs.clone()));
        Ok(inputs)
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
        self.check_input(request.input.as_deref())?;
        audio::probe(request)
    }

    fn start_audio_stream(
        &mut self,
        request: &AudioStreamRequest,
    ) -> Result<Box<dyn AudioStream>, CaptureError> {
        self.check_input(request.input.as_deref())?;
        audio::start_stream(request)
    }
}

impl LinuxCapture {
    /// PipeWire falls back to the default input for an unknown target, so
    /// named inputs are checked against a (briefly cached) listing first.
    fn check_input(&mut self, name: Option<&str>) -> Result<(), CaptureError> {
        let Some(name) = name else {
            return Ok(());
        };
        let fresh = self
            .input_cache
            .as_ref()
            .is_some_and(|(at, _)| at.elapsed() < INPUT_CACHE_TTL);
        if !fresh {
            self.audio_inputs()?;
        }
        let inputs = self
            .input_cache
            .as_ref()
            .map_or(&[][..], |(_, inputs)| inputs);
        audio::verify_input(name, inputs)
    }
}
