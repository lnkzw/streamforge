//! Screen capture through Windows.Graphics.Capture (`windows-capture`).
//!
//! Each stream runs on the crate's capture thread. The frame handler maps
//! each frame (a D3D11 staging copy made by `windows-capture`) and hands it
//! to a `capture_api::FrameSink`, which copies it into a pooled buffer and
//! publishes it, or discards and counts it when no buffer is free. Windows
//! draws a border around captured content, its privacy indicator; it stays
//! on.

use std::sync::Arc;
use std::thread;
use std::time::Instant;

use capture_api::{
    AudioCaptureCapability, AudioCaptureReport, AudioCaptureRequest, AudioInput,
    AudioStreamRequest, CaptureBackend, CaptureCapabilities, CaptureError, CaptureErrorKind,
    CaptureState, CursorMode, FrameSink, MappedFrame, ScreenCaptureCapability,
    ScreenCaptureRequest, ScreenSelection, ScreenSource, VideoCaptureReport, VideoSourceInfo,
    VideoSourceKind, VideoStream,
};
use media_core::FrameMailbox;
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
    let sink = Arc::new(FrameSink::new(deliver, "BGRA8"));
    let control = match item {
        Item::Monitor(monitor) => run(
            monitor,
            cursor,
            (
                Arc::clone(&sink),
                "the captured display went away (disconnected or turned off)",
            ),
        ),
        Item::Window(window) => run(
            window,
            cursor,
            (Arc::clone(&sink), "the captured window was closed"),
        ),
    }?;
    Ok(WgcStream {
        control: Some(control),
        sink,
        source,
        started: Instant::now(),
        warnings,
    })
}

fn run<T>(
    item: T,
    cursor: CursorCaptureSettings,
    flags: (Arc<FrameSink>, &'static str),
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

struct Handler {
    sink: Arc<FrameSink>,
    /// Why the capture ended when the system closes the item.
    closed: &'static str,
}

impl Handler {
    /// Map the frame (`windows-capture` creates and maps a staging texture)
    /// and hand it to the sink.
    fn publish(frame: &mut Frame, sink: &FrameSink, pts_ns: Option<i64>) -> Result<bool, String> {
        let mut buffer = frame.buffer().map_err(|err| err.to_string())?;
        let (width, height) = (buffer.width(), buffer.height());
        let stride = buffer.row_pitch() as usize;
        let data = buffer.as_raw_buffer();
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
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = (Arc<FrameSink>, &'static str);
    type Error = String;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let (sink, closed) = ctx.flags;
        Ok(Self { sink, closed })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let arrival_ns = self.sink.elapsed_ns();
        // SystemRelativeTime in 100 ns units (the QPC clock).
        let pts_ns = frame
            .timestamp()
            .ok()
            .and_then(|time| time.Duration.checked_mul(100));
        let (width, height) = (frame.width(), frame.height());
        let delivered = self
            .sink
            .delivers()
            .then(|| Self::publish(frame, &self.sink, pts_ns));
        // Errors stay in the report; returning one would end the capture.
        self.sink
            .observe_frame(arrival_ns, width, height, pts_ns, delivered);
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        self.sink.end(self.closed);
        Ok(())
    }
}

struct WgcStream {
    control: Option<CaptureControl<Handler, String>>,
    sink: Arc<FrameSink>,
    source: VideoSourceInfo,
    started: Instant,
    warnings: Vec<String>,
}

impl VideoStream for WgcStream {
    fn frames(&self) -> Arc<FrameMailbox> {
        self.sink.mailbox()
    }

    fn source(&self) -> &VideoSourceInfo {
        &self.source
    }

    fn state(&self) -> CaptureState {
        match self.sink.state() {
            CaptureState::Running
                if self
                    .control
                    .as_ref()
                    .is_some_and(CaptureControl::is_finished) =>
            {
                // Its error comes with the join, in the report of the stop.
                CaptureState::Ended {
                    reason: "the Windows.Graphics.Capture thread ended; the source's capture \
                             report says why"
                        .to_owned(),
                }
            }
            state => state,
        }
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
        Ok(self.sink.report(
            BACKEND,
            self.source.clone(),
            "d3d11 staging copy",
            std::mem::take(&mut self.warnings),
            elapsed,
        ))
    }
}

impl Drop for WgcStream {
    fn drop(&mut self) {
        if let Some(control) = self.control.take() {
            let _ = control.stop();
        }
    }
}
