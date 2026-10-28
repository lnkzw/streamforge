//! Linux screen, camera and audio capture: XDG desktop portal ScreenCast + PipeWire.
//!
//! - `PortalScreen`: the portal shows the compositor's picker and hands out a PipeWire
//!   node; frames arrive as shared-memory buffers with optional header metadata.
//! - `PipeWireCamera`: the `Video/Source` nodes of V4L2 and libcamera devices
//!   (ADR 0019), with their modes from the nodes' `EnumFormat` params;
//!   YUV and MJPEG frames are converted to BGRX by `frame-decode`.
//! - `PipeWireAudio`: PipeWire source nodes, and the monitor of each sink, are
//!   enumerated through the registry and captured as interleaved F32 at their
//!   native rate and channel count.
//! - Playback (the monitor, ADR 0017): an output stream to a sink, whose
//!   process callback runs on PipeWire's real-time thread.
//!
//! Probes run a PipeWire main loop on the calling thread for a bounded time;
//! callers must not invoke them from a UI thread. Screen and camera streams
//! run their loop on a dedicated `screen-capture` or `camera-capture`
//! thread. See
//! `docs/capture-linux.md` for system requirements (Hyprland in particular).
#![cfg(target_os = "linux")]

mod audio;
mod camera;
mod formats;
mod modes;
mod playback;
mod portal;
mod pw;
mod screen;
mod video;

use std::time::{Duration, Instant};

use capture_api::{
    AudioCaptureCapability, AudioCaptureReport, AudioCaptureRequest, AudioDevices, AudioInput,
    AudioOutput, AudioStream, AudioStreamRequest, Camera, CameraCapture, CameraCaptureCapability,
    CameraRequest, CameraStream, CaptureError, PlaybackRequest, PlaybackStream, ScreenCapture,
    ScreenCaptureCapability, ScreenCaptureRequest, ScreenSelection, ScreenSource,
    VideoCaptureReport, VideoStream,
};
use media_core::SampleReader;

/// Upper bound for each portal or PipeWire query. Screen, camera and audio
/// capabilities are queried in parallel by `Capture::capabilities` (in
/// `capture-api`), so it and the first `get_status` wait at most this long.
pub(crate) const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a listing of audio devices or cameras is reused for name checks, so
/// starting several streams does not rescan the PipeWire registry each time.
const NODE_CACHE_TTL: Duration = Duration::from_secs(2);

/// Screen capture through the XDG ScreenCast portal.
#[derive(Debug, Default)]
pub struct PortalScreen;

impl PortalScreen {
    pub fn new() -> Self {
        Self
    }
}

/// Audio devices and playback through PipeWire.
#[derive(Debug, Default)]
pub struct PipeWireAudio {
    /// The last listing of audio devices, and when it was made.
    nodes: pw::AudioNodes,
    scanned_at: Option<Instant>,
}

impl PipeWireAudio {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Cameras through PipeWire.
#[derive(Debug, Default)]
pub struct PipeWireCamera {
    /// The last listing of cameras, and when it was made.
    nodes: Vec<camera::CameraNode>,
    scanned_at: Option<Instant>,
}

impl PipeWireCamera {
    pub fn new() -> Self {
        Self::default()
    }

    fn scan_nodes(&mut self) -> Result<&[camera::CameraNode], CaptureError> {
        self.nodes = camera::camera_nodes(QUERY_TIMEOUT)?;
        self.scanned_at = Some(Instant::now());
        Ok(&self.nodes)
    }
}

impl CameraCapture for PipeWireCamera {
    fn capability(&mut self) -> CameraCaptureCapability {
        match pw::server_version(QUERY_TIMEOUT) {
            Ok(_) => CameraCaptureCapability {
                backend: Some(pw::BACKEND.to_owned()),
                available: true,
                unavailable_reason: None,
            },
            Err(err) => CameraCaptureCapability::unavailable(Some(pw::BACKEND), err.message),
        }
    }

    fn cameras(&mut self) -> Result<Vec<Camera>, CaptureError> {
        Ok(self
            .scan_nodes()?
            .iter()
            .map(camera::CameraNode::listing)
            .collect())
    }

    fn start_camera_stream(
        &mut self,
        request: &CameraRequest,
    ) -> Result<CameraStream, CaptureError> {
        let recent = self
            .scanned_at
            .is_some_and(|at| at.elapsed() < NODE_CACHE_TTL);
        let find = |nodes: &[camera::CameraNode]| {
            nodes
                .iter()
                .find(|node| node.name == request.camera_id)
                .cloned()
        };
        // A camera plugged in since the last listing is found by a rescan.
        let node = match recent.then(|| find(&self.nodes)).flatten() {
            Some(node) => node,
            None => {
                let nodes = self.scan_nodes()?;
                find(nodes).ok_or_else(|| camera::not_found(&request.camera_id, nodes))?
            }
        };
        camera::start_stream(request, &node)
    }
}

impl ScreenCapture for PortalScreen {
    fn capability(&mut self) -> ScreenCaptureCapability {
        match portal::query(QUERY_TIMEOUT) {
            Ok(info) => ScreenCaptureCapability {
                backend: Some(screen::BACKEND.to_owned()),
                available: !info.target_kinds.is_empty(),
                unavailable_reason: info
                    .target_kinds
                    .is_empty()
                    .then(|| "the ScreenCast portal advertises no source types".to_owned()),
                portal_version: Some(info.version),
                target_kinds: info.target_kinds,
                cursor_modes: info.cursor_modes,
                // The portal always asks the user; it cannot capture a named source.
                picker: true,
                target_list: false,
            },
            Err(err) => ScreenCaptureCapability::unavailable(Some(screen::BACKEND), err.message),
        }
    }

    fn screen_sources(&mut self) -> Result<Vec<ScreenSource>, CaptureError> {
        Err(screen::no_target_list())
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
}

impl AudioDevices for PipeWireAudio {
    fn capability(&mut self) -> AudioCaptureCapability {
        match pw::server_version(QUERY_TIMEOUT) {
            Ok(version) => AudioCaptureCapability {
                backend: Some(pw::BACKEND.to_owned()),
                available: true,
                unavailable_reason: None,
                server_version: Some(version),
            },
            Err(err) => AudioCaptureCapability::unavailable(Some(pw::BACKEND), err.message),
        }
    }

    fn audio_inputs(&mut self) -> Result<Vec<AudioInput>, CaptureError> {
        Ok(self.scan_nodes()?.inputs.clone())
    }

    fn audio_outputs(&mut self) -> Result<Vec<AudioOutput>, CaptureError> {
        Ok(self.scan_nodes()?.outputs.clone())
    }

    fn probe_audio(
        &mut self,
        request: &AudioCaptureRequest,
    ) -> Result<AudioCaptureReport, CaptureError> {
        let target = self.check_input(request.input.as_deref())?;
        audio::probe(request, target)
    }

    fn start_audio_stream(
        &mut self,
        request: &AudioStreamRequest,
    ) -> Result<Box<dyn AudioStream>, CaptureError> {
        let target = self.check_input(request.input.as_deref())?;
        audio::start_stream(request, target)
    }

    fn start_playback(
        &mut self,
        request: &PlaybackRequest,
        feed: SampleReader,
    ) -> Result<Box<dyn PlaybackStream>, CaptureError> {
        request.check(&feed)?;
        if let Some(name) = request.output.as_deref() {
            playback::verify_output(name, &self.recent_nodes()?.outputs)?;
        }
        playback::start(request, feed)
    }
}

impl PipeWireAudio {
    fn scan_nodes(&mut self) -> Result<&pw::AudioNodes, CaptureError> {
        self.nodes = pw::audio_nodes(QUERY_TIMEOUT)?;
        self.scanned_at = Some(Instant::now());
        Ok(&self.nodes)
    }

    /// The last listing if it is recent, else a new one.
    fn recent_nodes(&mut self) -> Result<&pw::AudioNodes, CaptureError> {
        let recent = self
            .scanned_at
            .is_some_and(|at| at.elapsed() < NODE_CACHE_TTL);
        if !recent {
            self.scan_nodes()?;
        }
        Ok(&self.nodes)
    }

    /// PipeWire falls back to the default device for an unknown target, so
    /// named inputs are checked against a (briefly cached) listing first.
    /// Returns what a named input captures: a source, or a sink's monitor.
    fn check_input(&mut self, name: Option<&str>) -> Result<Option<audio::Target>, CaptureError> {
        let Some(name) = name else {
            return Ok(None);
        };
        audio::resolve_input(name, &self.recent_nodes()?.inputs).map(Some)
    }
}
