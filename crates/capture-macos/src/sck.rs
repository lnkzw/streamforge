//! Screen capture through ScreenCaptureKit (`screencapturekit`).
//!
//! Displays and windows come from `SCShareableContent` and are captured by
//! id; there is no picker yet. ScreenCaptureKit calls the output handler
//! on its own dispatch queue: the handler maps each complete BGRA frame and
//! hands it to a `capture_api::FrameSink`, which copies it into a pooled
//! buffer and publishes it, or discards and counts it when no buffer is
//! free. A delegate records when ScreenCaptureKit stops a stream itself
//! (window closed, display gone, permission revoked).
//!
//! Sizes are in pixels: ScreenCaptureKit reports displays and windows in
//! points, so the stream is configured from the filter's pixel size
//! (`SCShareableContentInfo`, macOS 14+).
//!
//! Every query needs the screen-recording permission of the app that runs
//! StreamForge. Without it, listing and capturing fail with
//! `permission_denied` and the remedy. Waits on ScreenCaptureKit are
//! bounded by [`QUERY_TIMEOUT`].

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use capture_api::{
    AudioCaptureCapability, AudioCaptureReport, AudioCaptureRequest, AudioInput, AudioStream,
    AudioStreamRequest, CaptureBackend, CaptureCapabilities, CaptureError, CaptureErrorKind,
    CaptureState, CursorMode, FrameSink, MappedFrame, ScreenCaptureCapability,
    ScreenCaptureRequest, ScreenSelection, ScreenSource, VideoCaptureReport, VideoSourceInfo,
    VideoSourceKind, VideoStream,
};
use media_core::FrameMailbox;
use screencapturekit::cm::{CMSampleBufferExt, SCFrameStatus};
use screencapturekit::cv::CVPixelBuffer;
use screencapturekit::prelude::{
    CMSampleBuffer, CMSampleBufferSCExt, CMTime, PixelFormat as SckPixelFormat, SCContentFilter,
    SCError, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegateTrait,
    SCStreamOutputTrait, SCStreamOutputType,
};
use screencapturekit::shareable_content::SCShareableContentInfo;

use crate::ids::SourceRef;

pub(crate) const BACKEND: &str = "ScreenCaptureKit";
/// Longest wait for a ScreenCaptureKit query, or for starting or stopping
/// a stream. Starting a named source during a recording waits on the
/// engine thread, so this stays short.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
/// Frames ScreenCaptureKit keeps queued for the handler.
const QUEUE_DEPTH: u32 = 5;

const PERMISSION_REMEDY: &str = "allow screen recording for the app that runs StreamForge \
     (System Settings, Privacy & Security, Screen & System Audio Recording), then restart it";

/// Screen capture of named displays and windows. Audio is not handled
/// here (see `capture-cpal`).
#[derive(Debug, Default)]
pub struct SckCapture;

impl SckCapture {
    pub fn new() -> Self {
        Self
    }
}

fn audio_elsewhere() -> CaptureError {
    CaptureError::unsupported("audio capture is provided by the cpal backend, not by this one")
}

/// Run `query` on a helper thread and wait at most [`QUERY_TIMEOUT`]. A
/// query that never answers leaves its thread blocked; the caller gets a
/// `timeout` error instead of hanging.
fn bounded<T: Send + 'static>(
    what: &str,
    query: impl FnOnce() -> Result<T, SCError> + Send + 'static,
) -> Result<T, CaptureError> {
    let (tx, rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("sck-query".to_owned())
        .spawn(move || {
            let _ = tx.send(query());
        })
        .map_err(|err| CaptureError::backend(format!("cannot start a query thread: {err}")))?;
    match rx.recv_timeout(QUERY_TIMEOUT) {
        Ok(result) => result.map_err(|err| capture_error(what, &err)),
        Err(_) => Err(CaptureError::new(
            CaptureErrorKind::Timeout,
            format!("ScreenCaptureKit did not answer within {QUERY_TIMEOUT:?} ({what})"),
        )),
    }
}

/// Map ScreenCaptureKit errors; a declined permission gets its remedy.
fn capture_error(what: &str, error: &SCError) -> CaptureError {
    let message = error.to_string();
    let lower = message.to_lowercase();
    let declined = matches!(error, SCError::PermissionDenied(_))
        || lower.contains("declined")
        || lower.contains("tcc")
        || lower.contains("not authorized")
        || lower.contains("permission");
    if declined {
        CaptureError::new(
            CaptureErrorKind::PermissionDenied,
            format!("{what}: screen recording is not permitted ({message}); {PERMISSION_REMEDY}"),
        )
    } else {
        CaptureError::new(CaptureErrorKind::Unavailable, format!("{what}: {message}"))
    }
}

fn shareable_content() -> Result<SCShareableContent, CaptureError> {
    bounded("cannot list screen content", || {
        SCShareableContent::create()
            .with_exclude_desktop_windows(true)
            .with_on_screen_windows_only(true)
            .get()
    })
}

/// ScreenCaptureKit is linked, so capture is offered. Whether the app may
/// use it is only known by asking, which can show the system's permission
/// prompt; capabilities therefore do not ask (`list_capture_targets` does).
fn screen_capability() -> ScreenCaptureCapability {
    ScreenCaptureCapability {
        backend: Some(BACKEND.to_owned()),
        available: true,
        unavailable_reason: None,
        portal_version: None,
        target_kinds: vec![VideoSourceKind::Monitor, VideoSourceKind::Window],
        cursor_modes: vec![CursorMode::Hidden, CursorMode::Embedded],
        picker: false,
        target_list: true,
    }
}

impl CaptureBackend for SckCapture {
    fn capabilities(&mut self) -> CaptureCapabilities {
        CaptureCapabilities {
            platform: std::env::consts::OS.to_owned(),
            desktop: None,
            session_type: None,
            screen: screen_capability(),
            audio: AudioCaptureCapability::unavailable(None, audio_elsewhere().message),
        }
    }

    fn audio_inputs(&mut self) -> Result<Vec<AudioInput>, CaptureError> {
        Err(audio_elsewhere())
    }

    fn screen_sources(&mut self) -> Result<Vec<ScreenSource>, CaptureError> {
        let content = shareable_content()?;
        let mut sources: Vec<ScreenSource> = content
            .displays()
            .iter()
            .map(|display| ScreenSource {
                id: SourceRef::Display(display.display_id()).id(),
                kind: VideoSourceKind::Monitor,
                name: format!("Display {}", display.display_id()),
                width: Some(display.width()),
                height: Some(display.height()),
            })
            .collect();
        for window in content.windows() {
            let title = window.title().unwrap_or_default();
            // Layer 0 holds normal app windows; menus, docks and overlays
            // live on other layers.
            if title.is_empty() || window.window_layer() != 0 {
                continue;
            }
            let app = window
                .owning_application()
                .map(|app| app.application_name())
                .unwrap_or_default();
            let frame = window.frame();
            sources.push(ScreenSource {
                id: SourceRef::Window(window.window_id()).id(),
                kind: VideoSourceKind::Window,
                name: if app.is_empty() {
                    title
                } else {
                    format!("{title} ({app})")
                },
                width: size(frame.size.width),
                height: size(frame.size.height),
            });
        }
        Ok(sources)
    }

    fn probe_screen(
        &mut self,
        request: &ScreenCaptureRequest,
    ) -> Result<VideoCaptureReport, CaptureError> {
        let stream = start(&request.selection, false)?;
        thread::sleep(request.duration);
        Box::new(stream).stop()
    }

    fn probe_audio(
        &mut self,
        _request: &AudioCaptureRequest,
    ) -> Result<AudioCaptureReport, CaptureError> {
        Err(audio_elsewhere())
    }

    fn start_screen_stream(
        &mut self,
        selection: &ScreenSelection,
    ) -> Result<Box<dyn VideoStream>, CaptureError> {
        Ok(Box::new(start(selection, true)?))
    }

    fn start_audio_stream(
        &mut self,
        _request: &AudioStreamRequest,
    ) -> Result<Box<dyn AudioStream>, CaptureError> {
        Err(audio_elsewhere())
    }
}

/// Points (or pixels) as a positive whole number, if it is one.
fn size(value: f64) -> Option<u32> {
    (value.is_finite() && value >= 1.0 && value <= f64::from(u32::MAX))
        .then(|| value.round() as u32)
}

/// The filter and its pixel size for a source id.
fn resolve(id: &str) -> Result<(SCContentFilter, VideoSourceInfo), CaptureError> {
    let not_found = || {
        CaptureError::new(
            CaptureErrorKind::NotFound,
            format!("no screen source `{id}`; list them with list_capture_targets"),
        )
    };
    let source = SourceRef::parse(id).ok_or_else(not_found)?;
    let content = shareable_content()?;
    let built = |result: Result<SCContentFilter, SCError>| {
        result.map_err(|err| capture_error("cannot select the source", &err))
    };
    let (filter, kind, points) = match source {
        SourceRef::Display(number) => {
            let display = content
                .displays()
                .into_iter()
                .find(|display| display.display_id() == number)
                .ok_or_else(not_found)?;
            let filter = built(
                SCContentFilter::create()
                    .with_display(&display)
                    .with_excluding_windows(&[])
                    .build(),
            )?;
            let points = (Some(display.width()), Some(display.height()));
            (filter, VideoSourceKind::Monitor, points)
        }
        SourceRef::Window(number) => {
            let window = content
                .windows()
                .into_iter()
                .find(|window| window.window_id() == number)
                .ok_or_else(not_found)?;
            let filter = built(SCContentFilter::create().with_window(&window).build())?;
            let frame = window.frame();
            let points = (size(frame.size.width), size(frame.size.height));
            (filter, VideoSourceKind::Window, points)
        }
    };
    // Pixels (e.g. twice the points on a Retina display), else points.
    let (width, height) = SCShareableContentInfo::for_filter(&filter)
        .map(|info| info.pixel_size())
        .filter(|&(width, height)| width > 0 && height > 0)
        .map_or(points, |(width, height)| (Some(width), Some(height)));
    let info = VideoSourceInfo {
        kind: Some(kind),
        width,
        height,
    };
    Ok((filter, info))
}

fn start(selection: &ScreenSelection, deliver: bool) -> Result<SckStream, CaptureError> {
    selection.check_supported(&screen_capability())?;
    let id = selection.capture_target_id.as_deref().ok_or_else(|| {
        CaptureError::new(
            CaptureErrorKind::InvalidRequest,
            "name a source (`capture_target_id`) from list_capture_targets",
        )
    })?;
    let (filter, source) = resolve(id)?;
    let configuration = SCStreamConfiguration::new()
        .with_width(source.width.unwrap_or(1280))
        .with_height(source.height.unwrap_or(720))
        .with_pixel_format(SckPixelFormat::BGRA)
        .with_shows_cursor(selection.cursor_mode != CursorMode::Hidden)
        // At most the canvas rate; ScreenCaptureKit sends fewer frames
        // while nothing changes.
        .with_minimum_frame_interval(&CMTime::new(1, 60))
        .with_queue_depth(QUEUE_DEPTH);
    let sink = Arc::new(FrameSink::new(deliver, "BGRA"));
    let mut stream = SCStream::new_with_delegate(
        &filter,
        &configuration,
        Stopped {
            sink: Arc::clone(&sink),
        },
    )
    .map_err(|err| capture_error("cannot create the capture stream", &err))?;
    stream
        .add_output_handler(
            Handler {
                sink: Arc::clone(&sink),
            },
            SCStreamOutputType::Screen,
        )
        .map_err(|err| capture_error("cannot attach the frame handler", &err))?;
    let stream = bounded("cannot start capturing", move || {
        stream.start_capture().map(|()| stream)
    })?;
    Ok(SckStream {
        stream: Some(stream),
        sink,
        source,
        started: Instant::now(),
    })
}

/// Notes a stream that ScreenCaptureKit stopped on its own.
struct Stopped {
    sink: Arc<FrameSink>,
}

impl SCStreamDelegateTrait for Stopped {
    fn did_stop_with_error(&self, error: SCError) {
        self.sink
            .end(format!("ScreenCaptureKit stopped the capture: {error}"));
    }
}

struct Handler {
    sink: Arc<FrameSink>,
}

/// Nanoseconds of a valid media time, if it fits.
fn nanos(time: CMTime) -> Option<i64> {
    if time.timescale <= 0 {
        return None;
    }
    let ns = i128::from(time.value) * 1_000_000_000 / i128::from(time.timescale);
    i64::try_from(ns).ok()
}

/// Lock the pixel buffer and hand its bytes to the sink.
fn publish(sink: &FrameSink, pixels: &CVPixelBuffer, pts_ns: Option<i64>) -> Result<bool, String> {
    let locked = pixels
        .lock_read_only()
        .map_err(|status| format!("cannot lock the pixel buffer (status {status})"))?;
    let width = u32::try_from(locked.width()).map_err(|err| err.to_string())?;
    let height = u32::try_from(locked.height()).map_err(|err| err.to_string())?;
    let stride = locked.bytes_per_row();
    let data = crate::pixels::locked_bytes(&locked).ok_or("the pixel buffer is not BGRA")?;
    sink.publish(
        MappedFrame {
            width,
            height,
            stride,
            data,
        },
        pts_ns,
    )
}

impl SCStreamOutputTrait for Handler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if !matches!(of_type, SCStreamOutputType::Screen) {
            return;
        }
        let arrival_ns = self.sink.elapsed_ns();
        let pixels = match sample.frame_status() {
            Some(SCFrameStatus::Complete) => sample.pixel_buffer(),
            Some(SCFrameStatus::Stopped) => {
                self.sink.end("ScreenCaptureKit stopped the capture");
                None
            }
            // Idle, blank, suspended or started: no new image.
            _ => None,
        };
        let Some(pixels) = pixels else {
            self.sink.observe_empty(arrival_ns);
            return;
        };
        let pts_ns = nanos(sample.presentation_timestamp());
        let (width, height) = (
            u32::try_from(pixels.width()).unwrap_or(0),
            u32::try_from(pixels.height()).unwrap_or(0),
        );
        let delivered = self
            .sink
            .delivers()
            .then(|| publish(&self.sink, &pixels, pts_ns));
        self.sink
            .observe_frame(arrival_ns, width, height, pts_ns, delivered);
    }
}

struct SckStream {
    stream: Option<SCStream>,
    sink: Arc<FrameSink>,
    source: VideoSourceInfo,
    started: Instant,
}

impl VideoStream for SckStream {
    fn frames(&self) -> Arc<FrameMailbox> {
        self.sink.mailbox()
    }

    fn source(&self) -> &VideoSourceInfo {
        &self.source
    }

    fn state(&self) -> CaptureState {
        self.sink.state()
    }

    fn stop(mut self: Box<Self>) -> Result<VideoCaptureReport, CaptureError> {
        let stopped = match self.stream.take() {
            Some(stream) => bounded("cannot stop capturing", move || stream.stop_capture()),
            None => Err(CaptureError::backend("the stream was already stopped")),
        };
        let elapsed = self.started.elapsed();
        match stopped {
            Ok(()) => {}
            // A stream ScreenCaptureKit already ended cannot be stopped
            // again; the report says why it ended.
            Err(_) if self.sink.ended() => {}
            Err(error) => return Err(error),
        }
        Ok(self.sink.report(
            BACKEND,
            self.source.clone(),
            "IOSurface (CVPixelBuffer)",
            Vec::new(),
            elapsed,
        ))
    }
}

impl Drop for SckStream {
    fn drop(&mut self) {
        if let Some(stream) = self.stream.take() {
            let _ = bounded("cannot stop capturing", move || stream.stop_capture());
        }
    }
}
