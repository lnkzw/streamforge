//! Screen capture through ScreenCaptureKit (`screencapturekit`).
//!
//! Displays and windows come from `SCShareableContent` and are captured by
//! id; there is no picker yet. ScreenCaptureKit calls the output handler
//! on its own dispatch queue: the handler copies each complete BGRA frame
//! into a pooled buffer and publishes it to the stream's mailbox; when no
//! pooled buffer is free the frame is discarded and counted.
//!
//! Every query needs the screen-recording permission of the app that runs
//! StreamForge. Without it, listing and capturing fail with
//! `permission_denied` and the remedy. Waits on ScreenCaptureKit are
//! bounded by [`QUERY_TIMEOUT`].

use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use capture_api::{
    AudioCaptureCapability, AudioCaptureReport, AudioCaptureRequest, AudioInput, AudioStream,
    AudioStreamRequest, BufferKind, CaptureBackend, CaptureCapabilities, CaptureError,
    CaptureErrorKind, CursorMode, FrameObservation, Rational, ScreenCaptureCapability,
    ScreenCaptureRequest, ScreenSelection, ScreenSource, VideoCaptureReport, VideoFormatInfo,
    VideoSourceInfo, VideoSourceKind, VideoStatsAccumulator, VideoStream,
};
use media_core::{FrameMailbox, FramePool, MediaTimestamp, PixelFormat, VideoFrame};
use screencapturekit::cm::{CMSampleBufferExt, SCFrameStatus};
use screencapturekit::prelude::{
    CMSampleBuffer, CMSampleBufferSCExt, CMTime, PixelFormat as SckPixelFormat, SCContentFilter,
    SCError, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutputTrait,
    SCStreamOutputType,
};

use crate::ids::SourceRef;

pub(crate) const BACKEND: &str = "ScreenCaptureKit";
/// Frames in flight per stream (mailbox, compositor, one being filled).
const POOL_BUFFERS: usize = 4;
/// Longest wait for a ScreenCaptureKit query or for starting a stream.
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
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
/// prompt; capabilities therefore do not ask (`list_screen_sources` does).
fn screen_capability() -> ScreenCaptureCapability {
    ScreenCaptureCapability {
        backend: Some(BACKEND.to_owned()),
        available: true,
        unavailable_reason: None,
        portal_version: None,
        source_kinds: vec![VideoSourceKind::Monitor, VideoSourceKind::Window],
        cursor_modes: vec![CursorMode::Hidden, CursorMode::Embedded],
        picker: false,
        source_list: true,
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

/// The filter and frame size for a source id.
fn resolve(id: &str) -> Result<(SCContentFilter, VideoSourceInfo, (u32, u32)), CaptureError> {
    let not_found = || {
        CaptureError::new(
            CaptureErrorKind::NotFound,
            format!("no screen source `{id}`; list them with list_screen_sources"),
        )
    };
    let source = SourceRef::parse(id).ok_or_else(not_found)?;
    let content = shareable_content()?;
    let built = |result: Result<SCContentFilter, SCError>| {
        result.map_err(|err| capture_error("cannot select the source", &err))
    };
    match source {
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
            let (width, height) = (display.width(), display.height());
            let info = VideoSourceInfo {
                kind: Some(VideoSourceKind::Monitor),
                width: Some(width),
                height: Some(height),
            };
            Ok((filter, info, (width, height)))
        }
        SourceRef::Window(number) => {
            let window = content
                .windows()
                .into_iter()
                .find(|window| window.window_id() == number)
                .ok_or_else(not_found)?;
            let filter = built(SCContentFilter::create().with_window(&window).build())?;
            let frame = window.frame();
            let (width, height) = (size(frame.size.width), size(frame.size.height));
            let info = VideoSourceInfo {
                kind: Some(VideoSourceKind::Window),
                width,
                height,
            };
            Ok((filter, info, (width.unwrap_or(1280), height.unwrap_or(720))))
        }
    }
}

fn start(selection: &ScreenSelection, deliver: bool) -> Result<SckStream, CaptureError> {
    selection.check_supported(&screen_capability())?;
    let id = selection.source.as_deref().ok_or_else(|| {
        CaptureError::new(
            CaptureErrorKind::InvalidRequest,
            "name a source (`source_id`) from list_screen_sources",
        )
    })?;
    let (filter, source, (width, height)) = resolve(id)?;
    let configuration = SCStreamConfiguration::new()
        .with_width(width)
        .with_height(height)
        .with_pixel_format(SckPixelFormat::BGRA)
        .with_shows_cursor(selection.cursor != CursorMode::Hidden)
        // At most the canvas rate; ScreenCaptureKit sends fewer frames
        // while nothing changes.
        .with_minimum_frame_interval(&CMTime::new(1, 60))
        .with_queue_depth(QUEUE_DEPTH);
    let mailbox = Arc::new(FrameMailbox::new());
    let shared = Arc::new(Mutex::new(Shared::default()));
    let handler = Handler {
        mailbox: Arc::clone(&mailbox),
        shared: Arc::clone(&shared),
        pool: FramePool::new(POOL_BUFFERS),
        start: Instant::now(),
        deliver,
    };
    let mut stream = SCStream::new(&filter, &configuration)
        .map_err(|err| capture_error("cannot create the capture stream", &err))?;
    stream
        .add_output_handler(handler, SCStreamOutputType::Screen)
        .map_err(|err| capture_error("cannot attach the frame handler", &err))?;
    stream
        .start_capture()
        .map_err(|err| capture_error("cannot start capturing", &err))?;
    Ok(SckStream {
        stream: Some(stream),
        mailbox,
        source,
        shared,
        started: Instant::now(),
    })
}

/// State the dispatch queue updates and the stream handle reads.
#[derive(Default)]
struct Shared {
    stats: VideoStatsAccumulator,
    format: Option<VideoFormatInfo>,
    /// ScreenCaptureKit reported the stream as stopped (e.g. the window closed).
    stopped: bool,
    /// The first few frame errors.
    errors: Vec<String>,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Handler {
    mailbox: Arc<FrameMailbox>,
    shared: Arc<Mutex<Shared>>,
    pool: FramePool,
    start: Instant,
    /// Copy frames into the mailbox (streams) or only observe them (probes).
    deliver: bool,
}

impl Handler {
    /// Copy a complete frame into a pooled buffer and publish it.
    /// `Ok(None)` means no pooled buffer was free; `Ok(Some(size))` that the
    /// frame was published.
    fn deliver(
        &self,
        sample: &CMSampleBuffer,
        pts: Option<MediaTimestamp>,
    ) -> Result<Option<(u32, u32)>, String> {
        let pixels = sample
            .pixel_buffer()
            .ok_or("the sample carries no pixel buffer")?;
        let locked = pixels
            .lock_read_only()
            .map_err(|status| format!("cannot lock the pixel buffer (status {status})"))?;
        let width = u32::try_from(locked.width()).map_err(|err| err.to_string())?;
        let height = u32::try_from(locked.height()).map_err(|err| err.to_string())?;
        let stride = locked.bytes_per_row();
        let needed =
            VideoFrame::required_len(width, height, stride).map_err(|err| err.to_string())?;
        let data = crate::pixels::locked_bytes(&locked).ok_or("the pixel buffer is not BGRA")?;
        if data.len() < needed {
            return Err(format!(
                "the pixel buffer holds {} bytes, {width}x{height} needs {needed}",
                data.len()
            ));
        }
        let Some(mut pooled) = self.pool.acquire(needed) else {
            return Ok(None);
        };
        pooled.copy_from_slice(&data[..needed]);
        drop(locked);
        // Display content is opaque; window content outside the window's
        // shape is not meaningful for a layer, so frames count as opaque.
        let frame = VideoFrame::new(width, height, stride, PixelFormat::Bgrx8, pts, pooled)
            .map_err(|err| err.to_string())?;
        self.mailbox.publish(frame);
        Ok(Some((width, height)))
    }
}

/// Nanoseconds of a valid media time, if it fits.
fn nanos(time: CMTime) -> Option<i64> {
    if time.timescale <= 0 {
        return None;
    }
    let ns = i128::from(time.value) * 1_000_000_000 / i128::from(time.timescale);
    i64::try_from(ns).ok()
}

impl SCStreamOutputTrait for Handler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if !matches!(of_type, SCStreamOutputType::Screen) {
            return;
        }
        let arrival_ns = u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let status = sample.frame_status();
        let pts_ns = nanos(sample.presentation_timestamp());
        let complete = matches!(status, Some(SCFrameStatus::Complete));
        let delivered = (complete && self.deliver).then(|| {
            let pts = pts_ns.and_then(|ns| MediaTimestamp::new(ns, 1, 1_000_000_000).ok());
            self.deliver(&sample, pts)
        });
        let size = sample
            .pixel_buffer()
            .and_then(|pixels| {
                Some((
                    u32::try_from(pixels.width()).ok()?,
                    u32::try_from(pixels.height()).ok()?,
                ))
            })
            .filter(|_| complete);
        let mut shared = lock(&self.shared);
        if matches!(status, Some(SCFrameStatus::Stopped)) {
            shared.stopped = true;
        }
        shared.stats.observe(FrameObservation {
            arrival_ns,
            pts_ns: if complete { pts_ns } else { None },
            seq: None,
            kind: if complete {
                BufferKind::Frame
            } else {
                BufferKind::Empty
            },
        });
        if let Some((width, height)) = size {
            let changed = shared
                .format
                .as_ref()
                .is_none_or(|format| (format.width, format.height) != (width, height));
            if changed {
                shared.format = Some(VideoFormatInfo {
                    pixel_format: "BGRA".to_owned(),
                    width,
                    height,
                    // Frames arrive when the content changes, up to 60 FPS.
                    framerate: Rational { num: 0, den: 1 },
                });
            }
        }
        match delivered {
            Some(Ok(None)) => shared.stats.frame_discarded(),
            Some(Err(error)) => {
                shared.stats.frame_discarded();
                if shared.errors.len() < 8 {
                    shared.errors.push(error);
                }
            }
            Some(Ok(Some(_))) | None => {}
        }
    }
}

struct SckStream {
    stream: Option<SCStream>,
    mailbox: Arc<FrameMailbox>,
    source: VideoSourceInfo,
    shared: Arc<Mutex<Shared>>,
    started: Instant,
}

impl VideoStream for SckStream {
    fn frames(&self) -> Arc<FrameMailbox> {
        Arc::clone(&self.mailbox)
    }

    fn source(&self) -> &VideoSourceInfo {
        &self.source
    }

    fn stop(mut self: Box<Self>) -> Result<VideoCaptureReport, CaptureError> {
        let stopped = match self.stream.take() {
            Some(stream) => stream
                .stop_capture()
                .map_err(|err| capture_error("cannot stop capturing", &err)),
            None => Err(CaptureError::backend("the stream was already stopped")),
        };
        let elapsed = self.started.elapsed();
        let shared = lock(&self.shared);
        let mut warnings = Vec::new();
        match stopped {
            Ok(()) => {}
            // A stream ScreenCaptureKit already ended (e.g. its window
            // closed) cannot be stopped again; the report still counts.
            Err(_) if shared.stopped => {}
            Err(error) => return Err(error),
        }
        if shared.stopped {
            warnings.push("ScreenCaptureKit ended the capture (e.g. the window closed)".to_owned());
        }
        warnings.extend(
            shared
                .errors
                .iter()
                .map(|error| format!("frame not delivered: {error}")),
        );
        Ok(VideoCaptureReport {
            backend: BACKEND.to_owned(),
            source: self.source.clone(),
            format: shared.format.clone(),
            buffer_memory: Some("IOSurface (CVPixelBuffer)".to_owned()),
            stats: shared.stats.finish(elapsed),
            warnings,
        })
    }
}

impl Drop for SckStream {
    fn drop(&mut self) {
        if let Some(stream) = self.stream.take() {
            let _ = stream.stop_capture();
        }
    }
}
