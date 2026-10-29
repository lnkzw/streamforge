//! Cameras through AVFoundation (ADR 0019).
//!
//! Listing runs on a short thread per call and opens nothing: a device
//! states its formats without being opened. Each stream runs on a camera
//! thread of its own, which owns the capture session (see `avf`): it
//! answers the start once the first frame is published, then watches the
//! camera (unplugged, stopped by a runtime error) until it is stopped.
//!
//! The session asks AVFoundation for BGRA frames, which it converts from
//! the camera's format (YUV, or MJPEG decoded) on its own; the delegate
//! copies each frame on AVFoundation's queue into a pooled one
//! (`capture_api::FrameSink`). Frames AVFoundation drops as late are
//! counted as dropped.
//!
//! Camera access belongs to the app that runs StreamForge (the terminal,
//! for the CLI). A start asks for it if it is not decided yet, which shows
//! macOS's prompt; listing needs no access.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use capture_api::{
    Camera, CameraCapture, CameraCaptureCapability, CameraRequest, CameraStream, CaptureError,
    CaptureErrorKind, CaptureState, FrameSink, ModeOffer, VideoCaptureReport, VideoFormatInfo,
    VideoSourceInfo, VideoStream, choose_mode, listed_modes,
};
use media_core::FrameMailbox;
use objc2_core_video::{CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetWidth};

use crate::avf::{self, Access, PERMISSION_REMEDY, Running};
use crate::camera_modes::{CameraFormat, NativeMode, fourcc_text, listed_rate, native_modes};
use crate::pixels::LockedPixels;

pub(crate) const BACKEND: &str = "AVFoundation";

/// Bound on listing the cameras.
const LIST_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on a camera start once its access is decided: opening it and its
/// first frame. The camera thread keeps it and answers after teardown.
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a start waits for the answer to macOS's camera prompt, on top
/// of `START_TIMEOUT`.
const ACCESS_WAIT: Duration = Duration::from_secs(3);
/// Room for the camera thread's teardown after its own start bound: the
/// stop bound, so a failed start is answered once the camera is free.
const ANSWER_MARGIN: Duration = STOP_TIMEOUT;
/// Bound on a stop: stopping the session and its last callback.
const STOP_TIMEOUT: Duration = Duration::from_secs(4);
/// How often a running camera thread checks whether its camera ended.
const WATCH_INTERVAL: Duration = Duration::from_millis(200);
/// Room for every event a camera thread can get: the delegate announces
/// the first frame or its failure once, and the stream asks to stop once.
const EVENT_QUEUE: usize = 2;

/// Cameras listed and captured through AVFoundation.
#[derive(Debug, Default)]
pub struct AvfCamera;

impl AvfCamera {
    pub fn new() -> Self {
        Self
    }
}

/// Run `work` on a short thread of its own (see the module docs), bounded
/// by `timeout`; a thread that does not answer in time is left to finish.
fn on_own_thread<T: Send + 'static>(
    name: &str,
    timeout: Duration,
    work: impl FnOnce() -> Result<T, CaptureError> + Send + 'static,
) -> Result<T, CaptureError> {
    let (answer, answered) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let _ = answer.send(work());
        })
        .map_err(|err| CaptureError::backend(format!("cannot start a camera thread: {err}")))?;
    match answered.recv_timeout(timeout) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(CaptureError::new(
            CaptureErrorKind::Timeout,
            format!("AVFoundation did not answer within {} s", timeout.as_secs()),
        )),
        Err(RecvTimeoutError::Disconnected) => Err(CaptureError::backend(
            "the camera thread ended without an answer",
        )),
    }
}

fn offers(modes: &[NativeMode]) -> Vec<ModeOffer<CameraFormat>> {
    modes.iter().map(|mode| mode.offer).collect()
}

impl CameraCapture for AvfCamera {
    /// AVFoundation is linked, so cameras are offered unless this app's
    /// camera access is denied. Asking for it is left to a start, since
    /// asking shows the system's prompt.
    fn capability(&mut self) -> CameraCaptureCapability {
        let access = on_own_thread("camera-capability", LIST_TIMEOUT, avf::camera_access);
        match access.map(|access| access_error(&access)) {
            Ok(None) => CameraCaptureCapability {
                backend: Some(BACKEND.to_owned()),
                available: true,
                unavailable_reason: None,
            },
            Ok(Some(err)) | Err(err) => {
                CameraCaptureCapability::unavailable(Some(BACKEND), err.message)
            }
        }
    }

    fn cameras(&mut self) -> Result<Vec<Camera>, CaptureError> {
        on_own_thread("camera-list", LIST_TIMEOUT, || {
            Ok(avf::cameras()?
                .iter()
                .map(|device| Camera {
                    camera_id: device.id(),
                    name: device.name(),
                    modes: listed_modes(&offers(&native_modes(&device.formats()))),
                })
                .collect())
        })
    }

    fn start_camera_stream(
        &mut self,
        request: &CameraRequest,
    ) -> Result<CameraStream, CaptureError> {
        start(request)
    }
}

/// What the camera thread hears: from the delegate about the first frame,
/// from the stream handle to stop.
pub(crate) enum Event {
    Delivered,
    Failed(String),
    Stop,
}

/// The delegate's state on AVFoundation's queue (see `avf`).
pub(crate) struct Output {
    pub(crate) sink: Arc<FrameSink>,
    events: SyncSender<Event>,
    /// The first frame (or its failure) was announced.
    announced: AtomicBool,
}

impl Output {
    pub(crate) fn new(sink: Arc<FrameSink>, events: SyncSender<Event>) -> Self {
        Self {
            sink,
            events,
            announced: AtomicBool::new(false),
        }
    }

    /// One captured frame; `None` is a sample without an image.
    pub(crate) fn frame(
        &self,
        arrival_ns: u64,
        pixels: Option<&CVPixelBuffer>,
        pts_ns: Option<i64>,
    ) {
        let Some(pixels) = pixels else {
            self.sink.observe_empty(arrival_ns);
            return;
        };
        // The size needs no lock, so a frame that fails to lock still
        // counts at its own size.
        let (width, height) = (
            u32::try_from(CVPixelBufferGetWidth(pixels)).unwrap_or(0),
            u32::try_from(CVPixelBufferGetHeight(pixels)).unwrap_or(0),
        );
        let delivered =
            LockedPixels::lock(pixels).and_then(|locked| publish(&self.sink, &locked, pts_ns));
        let announcement = match &delivered {
            Ok(true) => Some(Event::Delivered),
            Err(err) => Some(Event::Failed(err.clone())),
            Ok(false) => None,
        };
        self.sink
            .observe_frame(arrival_ns, width, height, pts_ns, Some(delivered));
        if let Some(event) = announcement
            && !self.announced.swap(true, Ordering::AcqRel)
        {
            // Never blocks the queue; the thread reads at most this one.
            let _ = self.events.try_send(event);
        }
    }

    /// AVFoundation dropped a late frame.
    pub(crate) fn dropped(&self) {
        self.sink.observe_dropped();
    }
}

fn publish(
    sink: &FrameSink,
    locked: &LockedPixels<'_>,
    pts_ns: Option<i64>,
) -> Result<bool, String> {
    let (width, height, rows) = locked.bgra_rows()?;
    sink.publish_rows(width, height, rows, pts_ns)
}

/// Why cameras cannot be used with `access`, if they cannot.
fn access_error(access: &Access) -> Option<CaptureError> {
    let message = match access {
        Access::Granted | Access::Undecided => return None,
        Access::Denied => format!("camera access is denied; {PERMISSION_REMEDY}"),
        Access::Restricted => "camera access is restricted on this Mac (by a \
                               device-management profile or Screen Time)"
            .to_owned(),
    };
    Some(CaptureError::new(
        CaptureErrorKind::PermissionDenied,
        message,
    ))
}

/// What a camera thread reports once its camera delivered the first frame.
struct Started {
    format: VideoFormatInfo,
    warnings: Vec<String>,
}

fn start(request: &CameraRequest) -> Result<CameraStream, CaptureError> {
    let (events, received) = mpsc::sync_channel(EVENT_QUEUE);
    let (started, start_answer) = mpsc::sync_channel(1);
    let (stopped, stop_answer) = mpsc::sync_channel(1);
    let sink = Arc::new(FrameSink::new(true, "BGRA"));
    let thread = thread::Builder::new()
        .name("camera".to_owned())
        .spawn({
            let request = request.clone();
            let sink = Arc::clone(&sink);
            let events = events.clone();
            move || {
                run(&request, &sink, events, &received, started);
                let _ = stopped.send(());
            }
        })
        .map_err(|err| CaptureError::backend(format!("cannot start the camera thread: {err}")))?;
    // The thread bounds the start itself; this only catches a thread stuck
    // in a system call.
    match start_answer.recv_timeout(ACCESS_WAIT + START_TIMEOUT + ANSWER_MARGIN) {
        Ok(Ok(Started { format, warnings })) => Ok(CameraStream {
            stream: Box::new(AvfCameraStream {
                sink,
                events,
                stopped: stop_answer,
                thread: Some(thread),
                source: VideoSourceInfo {
                    kind: None,
                    width: Some(format.width),
                    height: Some(format.height),
                },
                format: format.clone(),
                warnings: warnings.clone(),
                started: Instant::now(),
            }),
            format: Some(format),
            warnings,
        }),
        Ok(Err(err)) => {
            // The error is sent only after the session's teardown.
            let _ = thread.join();
            Err(err)
        }
        Err(RecvTimeoutError::Timeout) => {
            // The thread is stuck in a system call: ask it to stop and leave
            // it, as waiting for it would not end sooner.
            let _ = events.try_send(Event::Stop);
            drop(thread);
            Err(CaptureError::new(
                CaptureErrorKind::Timeout,
                format!(
                    "camera `{}` did not start: AVFoundation did not answer within {} s",
                    request.camera_id,
                    (ACCESS_WAIT + START_TIMEOUT + ANSWER_MARGIN).as_secs()
                ),
            ))
        }
        Err(RecvTimeoutError::Disconnected) => {
            let _ = stop_thread(&events, &stop_answer, &mut Some(thread));
            Err(CaptureError::backend(
                "the camera thread ended without an answer",
            ))
        }
    }
}

/// Stop and join only after the worker confirms the session is torn down.
/// A camera stuck in a system call can exceed the bound; its handle
/// detaches.
fn stop_thread(
    events: &SyncSender<Event>,
    stopped: &Receiver<()>,
    thread: &mut Option<JoinHandle<()>>,
) -> Result<(), RecvTimeoutError> {
    let Some(thread) = thread.take() else {
        return Ok(());
    };
    // Never full (see `EVENT_QUEUE`); a thread that ended has no queue.
    let _ = events.try_send(Event::Stop);
    stopped.recv_timeout(STOP_TIMEOUT)?;
    let _ = thread.join();
    Ok(())
}

/// A running camera and its name.
struct Open {
    running: Running,
    name: String,
    /// When its access was decided; the start bound counts from here.
    decided: Instant,
}

fn open(
    request: &CameraRequest,
    sink: &Arc<FrameSink>,
    events: SyncSender<Event>,
) -> Result<(Open, Started), CaptureError> {
    let device = avf::cameras()?
        .into_iter()
        .find(|device| device.id() == request.camera_id)
        .ok_or_else(|| {
            CaptureError::new(
                CaptureErrorKind::NotFound,
                format!(
                    "camera `{}` is not connected; see list_cameras",
                    request.camera_id
                ),
            )
        })?;
    let name = device.name();
    // Not found comes first, so a missing camera never shows the prompt.
    let access = match avf::camera_access()? {
        Access::Undecided => match avf::request_camera_access(ACCESS_WAIT)? {
            Some(true) => Access::Granted,
            Some(false) => Access::Denied,
            None => {
                return Err(CaptureError::new(
                    CaptureErrorKind::PermissionDenied,
                    "macOS is asking whether the app that runs StreamForge may use the \
                     camera; allow it, then restart the source",
                ));
            }
        },
        access => access,
    };
    if let Some(err) = access_error(&access) {
        return Err(err);
    }
    let decided = Instant::now();
    let modes = native_modes(&device.formats());
    let choice = choose_mode(&offers(&modes), request).ok_or_else(|| {
        CaptureError::unsupported(format!(
            "camera `{name}` offers no format StreamForge reads \
             (BGRA, YUY2, UYVY, NV12, I420 or MJPEG)"
        ))
    })?;
    let mode = modes
        .iter()
        .find(|mode| mode.offer == choice.offer)
        .ok_or_else(|| CaptureError::backend("the chosen mode has no camera format"))?;
    let running = avf::start(&device, mode, Output::new(Arc::clone(sink), events))?;
    let mut warnings = choice.warnings;
    let format = running_format(&running, mode, &mut warnings);
    Ok((
        Open {
            running,
            name,
            decided,
        },
        Started { format, warnings },
    ))
}

/// The format the camera runs in, read back; a warning when macOS did not
/// keep the chosen one.
fn running_format(
    running: &Running,
    mode: &NativeMode,
    warnings: &mut Vec<String>,
) -> VideoFormatInfo {
    let chosen = mode.offer;
    let active = running.active();
    let fps = active.duration.and_then(listed_rate).unwrap_or(chosen.fps);
    let pixel_format = CameraFormat::from_subtype(active.format.subtype).map_or_else(
        || fourcc_text(active.format.subtype),
        |f| f.name().to_owned(),
    );
    let (width, height) = (
        u32::try_from(active.format.width).unwrap_or(chosen.width),
        u32::try_from(active.format.height).unwrap_or(chosen.height),
    );
    if (width, height) != chosen.size() || fps != chosen.fps || pixel_format != chosen.format.name()
    {
        warnings.push(format!(
            "macOS runs the camera in {width}x{height} at {} fps {pixel_format}, not the chosen \
             {}x{} at {} fps {}",
            capture_api::rate_text(fps),
            chosen.width,
            chosen.height,
            capture_api::rate_text(chosen.fps),
            chosen.format.name()
        ));
    }
    VideoFormatInfo {
        pixel_format,
        width,
        height,
        framerate: fps,
    }
}

/// Why a running camera ended, if it did: unplugged, or stopped by
/// AVFoundation (with the runtime error's reason when it sent one).
fn ended(open: &Open, sink: &FrameSink) -> Option<CaptureError> {
    if !open.running.is_connected() {
        return Some(CaptureError::new(
            CaptureErrorKind::NotFound,
            format!("camera `{}` was disconnected", open.name),
        ));
    }
    if let CaptureState::Ended { reason } = sink.state() {
        return Some(CaptureError::new(CaptureErrorKind::Unavailable, reason));
    }
    (!open.running.is_running()).then(|| {
        CaptureError::new(
            CaptureErrorKind::Unavailable,
            format!("macOS stopped camera `{}`", open.name),
        )
    })
}

/// The camera thread: open, watch until stopped, close. A start fails when
/// no frame came within `START_TIMEOUT` of its access decision; a frame
/// that came while `startRunning` still blocked counts, as the camera then
/// runs.
fn run(
    request: &CameraRequest,
    sink: &Arc<FrameSink>,
    events: SyncSender<Event>,
    received: &Receiver<Event>,
    started: SyncSender<Result<Started, CaptureError>>,
) {
    let (open, mut info) = match open(request, sink, events) {
        Ok(opened) => opened,
        Err(err) => {
            // `open` dropped its session already: the camera is free.
            let _ = started.send(Err(err));
            return;
        }
    };
    let deadline = open.decided + START_TIMEOUT;
    let mut pending = Some(started);
    let mut start_error = None;
    let mut watching = true;
    loop {
        let wait = match pending {
            Some(_) => WATCH_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
            None => WATCH_INTERVAL,
        };
        match received.recv_timeout(wait) {
            Ok(Event::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Event::Delivered) => {
                if let Some(answer) = pending.take() {
                    let _ = answer.send(Ok(Started {
                        format: info.format.clone(),
                        warnings: std::mem::take(&mut info.warnings),
                    }));
                }
            }
            Ok(Event::Failed(reason)) => {
                if pending.is_some() {
                    start_error = Some(CaptureError::backend(reason));
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        if watching && let Some(err) = ended(&open, sink) {
            watching = false;
            sink.end(err.message.clone());
            if pending.is_some() {
                start_error = Some(err);
                break;
            }
        }
        if pending.is_some() && Instant::now() >= deadline {
            start_error = Some(CaptureError::new(
                CaptureErrorKind::Timeout,
                format!(
                    "camera `{}` did not deliver a frame within {} s",
                    open.name,
                    START_TIMEOUT.as_secs()
                ),
            ));
            break;
        }
    }
    // Answer a failed start only once the camera is free.
    drop(open);
    if let (Some(answer), Some(err)) = (pending, start_error) {
        let _ = answer.send(Err(err));
    }
}

struct AvfCameraStream {
    sink: Arc<FrameSink>,
    events: SyncSender<Event>,
    stopped: Receiver<()>,
    thread: Option<JoinHandle<()>>,
    source: VideoSourceInfo,
    format: VideoFormatInfo,
    warnings: Vec<String>,
    started: Instant,
}

impl VideoStream for AvfCameraStream {
    fn frames(&self) -> Arc<FrameMailbox> {
        self.sink.mailbox()
    }

    fn source(&self) -> &VideoSourceInfo {
        &self.source
    }

    fn state(&self) -> CaptureState {
        match self.sink.state() {
            CaptureState::Running if self.thread.as_ref().is_some_and(JoinHandle::is_finished) => {
                CaptureState::Ended {
                    reason: "the camera thread ended".to_owned(),
                }
            }
            state => state,
        }
    }

    fn stop(mut self: Box<Self>) -> Result<VideoCaptureReport, CaptureError> {
        let mut warnings = std::mem::take(&mut self.warnings);
        if stop_thread(&self.events, &self.stopped, &mut self.thread).is_err() {
            warnings.push(format!(
                "the camera did not close within {} s; it closes on its own thread",
                STOP_TIMEOUT.as_secs()
            ));
        }
        let mut report = self.sink.report(
            BACKEND,
            self.source.clone(),
            "system memory (CVPixelBuffer)",
            warnings,
            self.started.elapsed(),
        );
        // The camera's own format and rate, not the converted frames'.
        let mut format = self.format.clone();
        if let Some(observed) = &report.format
            && (observed.width, observed.height) != (format.width, format.height)
        {
            report.warnings.push(format!(
                "the camera changed its size from {}x{} to {}x{} while running",
                format.width, format.height, observed.width, observed.height
            ));
            format.width = observed.width;
            format.height = observed.height;
        }
        report.format = Some(format);
        Ok(report)
    }
}

impl Drop for AvfCameraStream {
    fn drop(&mut self) {
        let _ = stop_thread(&self.events, &self.stopped, &mut self.thread);
    }
}

#[cfg(test)]
mod tests {
    use objc2_core_video::kCVPixelFormatType_32BGRA;

    use super::*;
    use crate::pixels::test_buffer;

    fn fake_stream() -> (Box<AvfCameraStream>, Arc<AtomicBool>) {
        let (events, received) = mpsc::sync_channel(1);
        let (stopped, answer) = mpsc::sync_channel(1);
        let released = Arc::new(AtomicBool::new(false));
        let worker_released = Arc::clone(&released);
        let thread = thread::spawn(move || {
            assert!(matches!(received.recv().unwrap(), Event::Stop));
            worker_released.store(true, Ordering::SeqCst);
            stopped.send(()).unwrap();
        });
        (
            Box::new(AvfCameraStream {
                sink: Arc::new(FrameSink::new(true, "BGRA")),
                events,
                stopped: answer,
                thread: Some(thread),
                source: VideoSourceInfo {
                    kind: None,
                    width: Some(640),
                    height: Some(480),
                },
                format: VideoFormatInfo {
                    pixel_format: "NV12".to_owned(),
                    width: 640,
                    height: 480,
                    framerate: capture_api::Rational::integer(30),
                },
                warnings: Vec::new(),
                started: Instant::now(),
            }),
            released,
        )
    }

    #[test]
    fn drop_releases_the_worker_before_returning() {
        let (stream, released) = fake_stream();
        drop(stream);
        assert!(released.load(Ordering::SeqCst));
    }

    #[test]
    fn stop_reports_observed_size_but_keeps_native_format_and_rate() {
        let (stream, released) = fake_stream();
        stream
            .sink
            .observe_frame(0, 1280, 720, Some(0), Some(Ok(true)));
        let report = stream.stop().expect("stop");
        assert!(released.load(Ordering::SeqCst));
        let format = report.format.expect("format");
        assert_eq!((format.width, format.height), (1280, 720));
        assert_eq!(format.pixel_format, "NV12");
        assert_eq!(format.framerate, capture_api::Rational::integer(30));
        assert_eq!(
            report.warnings,
            ["the camera changed its size from 640x480 to 1280x720 while running"]
        );
    }

    #[test]
    fn the_stop_follows_an_unread_first_frame_event() {
        // The queue holds both events a camera thread can get unread.
        let (events, _received) = mpsc::sync_channel(EVENT_QUEUE);
        events.try_send(Event::Delivered).unwrap();
        events.try_send(Event::Stop).unwrap();
        let (events, received) = mpsc::sync_channel(EVENT_QUEUE);
        let (stopped, answer) = mpsc::sync_channel(1);
        events.try_send(Event::Delivered).unwrap();
        let mut thread = Some(thread::spawn(move || {
            assert!(matches!(received.recv().unwrap(), Event::Delivered));
            assert!(matches!(received.recv().unwrap(), Event::Stop));
            stopped.send(()).unwrap();
        }));
        stop_thread(&events, &answer, &mut thread).expect("stopped");
        assert!(thread.is_none());
        // A later Drop after stop must neither send nor wait a second time.
        stop_thread(&events, &answer, &mut thread).expect("nothing to stop");
    }

    #[test]
    fn a_frame_is_published_without_padding_and_announced_once() {
        let (buffer, stride) = test_buffer(3, 2, kCVPixelFormatType_32BGRA);
        assert!(stride >= 12, "stride {stride}");
        let (events, received) = mpsc::sync_channel(EVENT_QUEUE);
        let output = Output::new(Arc::new(FrameSink::new(true, "BGRA")), events);
        output.frame(0, Some(&*buffer), Some(1_000));
        output.frame(10, Some(&*buffer), Some(2_000));
        output.frame(20, None, None);
        output.dropped();
        let frame = output.sink.mailbox().take().expect("frame");
        assert_eq!((frame.width(), frame.height(), frame.stride()), (3, 2, 12));
        let expected: Vec<u8> = (0..12).map(|i| i as u8).collect();
        assert_eq!(frame.row(0), &expected[..]);
        let expected: Vec<u8> = (stride..stride + 12).map(|i| i as u8).collect();
        assert_eq!(frame.row(1), &expected[..]);
        assert!(matches!(received.try_recv(), Ok(Event::Delivered)));
        assert!(received.try_recv().is_err(), "announced once");
        let report = output.sink.report(
            BACKEND,
            VideoSourceInfo {
                kind: None,
                width: None,
                height: None,
            },
            "test",
            Vec::new(),
            Duration::from_secs(1),
        );
        assert_eq!(report.stats.frames, 2);
        assert_eq!(report.stats.empty_buffers, 1);
        assert_eq!(report.stats.dropped_frames, 1);
        assert_eq!(report.stats.frames_discarded, 0);
    }

    #[test]
    fn a_frame_in_another_format_fails_the_start() {
        let (buffer, _) = test_buffer(4, 4, u32::from_be_bytes(*b"420v"));
        let (events, received) = mpsc::sync_channel(EVENT_QUEUE);
        let output = Output::new(Arc::new(FrameSink::new(true, "BGRA")), events);
        output.frame(0, Some(&*buffer), None);
        match received.try_recv() {
            Ok(Event::Failed(reason)) => assert!(reason.contains("420v"), "{reason}"),
            _ => panic!("expected a failure"),
        }
        assert!(output.sink.mailbox().take().is_none());
    }
}
