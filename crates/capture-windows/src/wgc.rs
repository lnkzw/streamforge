//! Screen capture through Windows.Graphics.Capture (`windows-capture`).
//!
//! Each stream runs on the crate's capture thread. The frame handler maps
//! each frame (a D3D11 staging copy made by `windows-capture`), copies it
//! into a pooled buffer and publishes it to the stream's mailbox; when no
//! pooled buffer is free the frame is discarded and counted. Windows draws
//! a border around captured content, its privacy indicator; it stays on.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Instant;

use capture_api::{
    AudioCaptureCapability, AudioCaptureReport, AudioCaptureRequest, AudioInput,
    AudioStreamRequest, BufferKind, CaptureBackend, CaptureCapabilities, CaptureError,
    CaptureErrorKind, CursorMode, FrameObservation, Rational, ScreenCaptureCapability,
    ScreenCaptureRequest, ScreenSelection, ScreenSource, VideoCaptureReport, VideoFormatInfo,
    VideoSourceInfo, VideoSourceKind, VideoStatsAccumulator, VideoStream,
};
use media_core::{FrameMailbox, FramePool, MediaTimestamp, PixelFormat, VideoFrame};
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    GraphicsCaptureItemType, MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use crate::ids::SourceRef;

pub(crate) const BACKEND: &str = "Windows.Graphics.Capture";
/// Frames in flight per stream (mailbox, compositor, one being filled).
const POOL_BUFFERS: usize = 4;

/// Screen capture of named monitors and windows. Audio is not handled
/// here (see `capture-cpal`).
#[derive(Debug, Default)]
pub struct WgcCapture;

impl WgcCapture {
    pub fn new() -> Self {
        Self
    }
}

fn supported() -> Result<(), CaptureError> {
    match GraphicsCaptureApi::is_supported() {
        Ok(true) => Ok(()),
        Ok(false) => Err(CaptureError::unsupported(
            "Windows.Graphics.Capture is not available on this system \
             (it needs Windows 10 version 1903 or newer with a desktop session)",
        )),
        Err(err) => Err(CaptureError::backend(format!(
            "cannot query Windows.Graphics.Capture support: {err}"
        ))),
    }
}

/// Whether this Windows build can turn the cursor on and off.
fn cursor_selectable() -> bool {
    GraphicsCaptureApi::is_cursor_settings_supported().unwrap_or(false)
}

fn screen_capability() -> ScreenCaptureCapability {
    match supported() {
        Ok(()) => ScreenCaptureCapability {
            backend: Some(BACKEND.to_owned()),
            available: true,
            unavailable_reason: None,
            portal_version: None,
            source_kinds: vec![VideoSourceKind::Monitor, VideoSourceKind::Window],
            cursor_modes: if cursor_selectable() {
                vec![CursorMode::Hidden, CursorMode::Embedded]
            } else {
                Vec::new()
            },
            picker: false,
            source_list: true,
        },
        Err(err) => ScreenCaptureCapability::unavailable(Some(BACKEND), err.message),
    }
}

fn audio_elsewhere() -> CaptureError {
    CaptureError::unsupported("audio capture is provided by the cpal backend, not by this one")
}

impl CaptureBackend for WgcCapture {
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
        supported()?;
        let mut sources = Vec::new();
        let monitors = Monitor::enumerate()
            .map_err(|err| CaptureError::backend(format!("cannot list monitors: {err}")))?;
        for monitor in monitors {
            let Ok(device) = monitor.device_name() else {
                continue;
            };
            sources.push(ScreenSource {
                id: SourceRef::Monitor(device.clone()).id(),
                kind: VideoSourceKind::Monitor,
                name: monitor.name().unwrap_or(device),
                width: monitor.width().ok(),
                height: monitor.height().ok(),
            });
        }
        let windows = Window::enumerate()
            .map_err(|err| CaptureError::backend(format!("cannot list windows: {err}")))?;
        for window in windows {
            let title = window.title().unwrap_or_default();
            let Ok(process) = window.process_id() else {
                continue;
            };
            if title.is_empty() {
                continue;
            }
            let handle = window.as_raw_hwnd().addr();
            sources.push(ScreenSource {
                id: SourceRef::Window { process, handle }.id(),
                kind: VideoSourceKind::Window,
                name: title,
                width: window.width().ok().and_then(|w| u32::try_from(w).ok()),
                height: window.height().ok().and_then(|h| u32::try_from(h).ok()),
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
    ) -> Result<Box<dyn capture_api::AudioStream>, CaptureError> {
        Err(audio_elsewhere())
    }
}

/// A capture source resolved from its id.
enum Item {
    Monitor(Monitor),
    Window(Window),
}

fn resolve(id: &str) -> Result<(Item, VideoSourceInfo), CaptureError> {
    let not_found = || {
        CaptureError::new(
            CaptureErrorKind::NotFound,
            format!("no screen source `{id}`; list them with list_screen_sources"),
        )
    };
    match SourceRef::parse(id).ok_or_else(not_found)? {
        SourceRef::Monitor(device) => {
            let monitor = Monitor::enumerate()
                .map_err(|err| CaptureError::backend(format!("cannot list monitors: {err}")))?
                .into_iter()
                .find(|monitor| monitor.device_name().is_ok_and(|name| name == device))
                .ok_or_else(not_found)?;
            let info = VideoSourceInfo {
                kind: Some(VideoSourceKind::Monitor),
                width: monitor.width().ok(),
                height: monitor.height().ok(),
            };
            Ok((Item::Monitor(monitor), info))
        }
        SourceRef::Window { process, handle } => {
            // A handle value, not a pointer: it carries no provenance.
            let window = Window::from_raw_hwnd(std::ptr::without_provenance_mut(handle));
            // A reused handle belongs to another process (see `ids`).
            if !window.is_valid() || window.process_id().ok() != Some(process) {
                return Err(not_found());
            }
            let info = VideoSourceInfo {
                kind: Some(VideoSourceKind::Window),
                width: window.width().ok().and_then(|w| u32::try_from(w).ok()),
                height: window.height().ok().and_then(|h| u32::try_from(h).ok()),
            };
            Ok((Item::Window(window), info))
        }
    }
}

fn start(selection: &ScreenSelection, deliver: bool) -> Result<WgcStream, CaptureError> {
    selection.check_supported(&screen_capability())?;
    let id = selection.source.as_deref().ok_or_else(|| {
        CaptureError::new(
            CaptureErrorKind::InvalidRequest,
            "name a source (`source_id`) from list_screen_sources",
        )
    })?;
    let (item, source) = resolve(id)?;
    let mut warnings = Vec::new();
    let cursor = if cursor_selectable() {
        match selection.cursor {
            CursorMode::Hidden => CursorCaptureSettings::WithoutCursor,
            // `Metadata` is not advertised, so `check_supported` rejected it.
            CursorMode::Embedded | CursorMode::Metadata => CursorCaptureSettings::WithCursor,
        }
    } else {
        warnings.push(format!(
            "this Windows version cannot select a cursor mode; `{}` was ignored",
            selection.cursor.as_str()
        ));
        CursorCaptureSettings::Default
    };
    let mailbox = Arc::new(FrameMailbox::new());
    let shared = Arc::new(Mutex::new(Shared::default()));
    let flags = Flags {
        mailbox: Arc::clone(&mailbox),
        shared: Arc::clone(&shared),
        start: Instant::now(),
        deliver,
    };
    let control = match item {
        Item::Monitor(monitor) => run(monitor, cursor, flags),
        Item::Window(window) => run(window, cursor, flags),
    }?;
    Ok(WgcStream {
        control: Some(control),
        mailbox,
        source,
        shared,
        started: Instant::now(),
        warnings,
    })
}

fn run<T>(
    item: T,
    cursor: CursorCaptureSettings,
    flags: Flags,
) -> Result<CaptureControl<Handler, String>, CaptureError>
where
    T: TryInto<GraphicsCaptureItemType> + Send + 'static,
{
    let settings = Settings::new(
        item,
        cursor,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        flags,
    );
    Handler::start_free_threaded(settings).map_err(|err| {
        CaptureError::new(
            CaptureErrorKind::Unavailable,
            format!("cannot start Windows.Graphics.Capture: {err}"),
        )
    })
}

/// State the capture thread updates and the stream handle reads.
#[derive(Default)]
struct Shared {
    stats: VideoStatsAccumulator,
    format: Option<VideoFormatInfo>,
    /// The captured window was closed.
    closed: bool,
    /// The first few frame errors.
    errors: Vec<String>,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Flags {
    mailbox: Arc<FrameMailbox>,
    shared: Arc<Mutex<Shared>>,
    start: Instant,
    /// Copy frames into the mailbox (streams) or only observe them (probes).
    deliver: bool,
}

struct Handler {
    flags: Flags,
    pool: FramePool,
}

impl Handler {
    /// Copy the mapped frame into a pooled buffer and publish it. `Ok(false)`
    /// means no pooled buffer was free. Allocates only while the pool warms
    /// up or when the size grows; `windows-capture` maps a staging texture.
    fn deliver(&mut self, frame: &mut Frame, pts: Option<MediaTimestamp>) -> Result<bool, String> {
        let mut buffer = frame.buffer().map_err(|err| err.to_string())?;
        let (width, height) = (buffer.width(), buffer.height());
        let stride = buffer.row_pitch() as usize;
        let needed =
            VideoFrame::required_len(width, height, stride).map_err(|err| err.to_string())?;
        let data = buffer.as_raw_buffer();
        if data.len() < needed {
            return Err(format!(
                "the mapped frame holds {} bytes, {width}x{height} needs {needed}",
                data.len()
            ));
        }
        let Some(mut pooled) = self.pool.acquire(needed) else {
            return Ok(false);
        };
        pooled.copy_from_slice(&data[..needed]);
        // BGRA from the compositor; the alpha of captured content is not
        // meaningful for a layer, so it is treated as opaque.
        let frame = VideoFrame::new(width, height, stride, PixelFormat::Bgrx8, pts, pooled)
            .map_err(|err| err.to_string())?;
        self.flags.mailbox.publish(frame);
        Ok(true)
    }
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = Flags;
    type Error = String;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            flags: ctx.flags,
            pool: FramePool::new(POOL_BUFFERS),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let arrival_ns = u64::try_from(self.flags.start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        // SystemRelativeTime in 100 ns units (the QPC clock).
        let pts_ns = frame
            .timestamp()
            .ok()
            .and_then(|time| time.Duration.checked_mul(100));
        let (width, height) = (frame.width(), frame.height());
        let delivered = if self.flags.deliver {
            let pts = pts_ns.and_then(|ns| MediaTimestamp::new(ns, 1, 1_000_000_000).ok());
            Some(self.deliver(frame, pts))
        } else {
            None
        };
        // Errors stay in the report; returning one would end the capture.
        let mut shared = lock(&self.flags.shared);
        shared.stats.observe(FrameObservation {
            arrival_ns,
            pts_ns,
            seq: None,
            kind: BufferKind::Frame,
        });
        let changed = shared
            .format
            .as_ref()
            .is_none_or(|format| (format.width, format.height) != (width, height));
        if changed {
            shared.format = Some(VideoFormatInfo {
                pixel_format: "BGRA8".to_owned(),
                width,
                height,
                // Frames arrive when the content changes.
                framerate: Rational { num: 0, den: 1 },
            });
        }
        match delivered {
            Some(Ok(false)) => shared.stats.frame_discarded(),
            Some(Err(error)) => {
                shared.stats.frame_discarded();
                if shared.errors.len() < 8 {
                    shared.errors.push(error);
                }
            }
            Some(Ok(true)) | None => {}
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        lock(&self.flags.shared).closed = true;
        Ok(())
    }
}

struct WgcStream {
    control: Option<CaptureControl<Handler, String>>,
    mailbox: Arc<FrameMailbox>,
    source: VideoSourceInfo,
    shared: Arc<Mutex<Shared>>,
    started: Instant,
    warnings: Vec<String>,
}

impl VideoStream for WgcStream {
    fn frames(&self) -> Arc<FrameMailbox> {
        Arc::clone(&self.mailbox)
    }

    fn source(&self) -> &VideoSourceInfo {
        &self.source
    }

    fn stop(mut self: Box<Self>) -> Result<VideoCaptureReport, CaptureError> {
        let stopped = match self.control.take() {
            Some(control) => control.stop().map_err(|err| {
                CaptureError::backend(format!(
                    "Windows.Graphics.Capture ended with an error: {err}"
                ))
            }),
            None => Err(CaptureError::backend("the stream was already stopped")),
        };
        let elapsed = self.started.elapsed();
        stopped?;
        let shared = lock(&self.shared);
        let mut warnings = std::mem::take(&mut self.warnings);
        if shared.closed {
            warnings.push("the captured window was closed".to_owned());
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
            buffer_memory: Some("d3d11 staging copy".to_owned()),
            stats: shared.stats.finish(elapsed),
            warnings,
        })
    }
}

impl Drop for WgcStream {
    fn drop(&mut self) {
        if let Some(control) = self.control.take() {
            let _ = control.stop();
        }
    }
}
