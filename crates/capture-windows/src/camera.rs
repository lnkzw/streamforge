//! Cameras through Media Foundation's source reader (ADR 0019).
//!
//! Every Media Foundation call runs on a thread of StreamForge's own, in
//! the multithreaded apartment ([`mf::Session`]): the listing on a short
//! thread per call, each stream on its camera thread. The caller's thread
//! (the engine's, where cpal keeps its single-threaded apartment) never
//! enters COM for a camera.
//!
//! A stream's reader is asynchronous: its callback forwards each sample to
//! the camera thread, which locks the RGB32 frame, hands it to a
//! `capture_api::FrameSink` and asks for the next one. Stopping flushes the
//! reader and waits for that within a bound, so a camera that stopped
//! delivering cannot hold the caller.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use capture_api::{
    Camera, CameraCapture, CameraCaptureCapability, CameraMode, CameraRequest, CameraStream,
    CaptureError, CaptureErrorKind, CaptureState, FrameSink, MappedFrame, ModeOffer,
    VideoCaptureReport, VideoFormatInfo, VideoSourceInfo, VideoStream, choose_mode, listed_modes,
};
use media_core::FrameMailbox;
use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, IMFSourceReader, MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR,
};

use crate::camera_formats::CameraFormat;
use crate::mf::{self, Layout, ReaderEvent, Sample, Session, Source};

pub(crate) const BACKEND: &str = "Media Foundation";

/// Bound on listing the cameras, opening each new one to read its modes.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on a camera start: opening it and its first frame.
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on the reader's flush when stopping.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
/// Bound on a stop: the flush and releasing the camera.
const STOP_TIMEOUT: Duration = Duration::from_secs(4);
/// Reader events queued at most: one sample (one is asked for at a time),
/// one flush and the stop request, with room to spare.
const EVENT_QUEUE: usize = 8;

/// Cameras listed and captured through Media Foundation. The modes of a
/// camera are read once (opening it) and kept for the process: a device's
/// modes do not change, and a camera that runs cannot always be opened a
/// second time to read them.
#[derive(Debug, Default)]
pub struct MfCamera {
    modes: HashMap<String, Vec<CameraMode>>,
}

impl MfCamera {
    pub fn new() -> Self {
        Self::default()
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
            format!(
                "Media Foundation did not answer within {} s",
                timeout.as_secs()
            ),
        )),
        Err(RecvTimeoutError::Disconnected) => Err(CaptureError::backend(
            "the camera thread ended without an answer",
        )),
    }
}

struct Listed {
    camera_id: String,
    name: String,
    /// `None` when the modes are already known or the camera did not open.
    modes: Option<Vec<CameraMode>>,
}

/// The cameras, reading the modes of those not in `known`.
fn list(known: &HashSet<String>) -> Result<Vec<Listed>, CaptureError> {
    let session = Session::start()?;
    let mut listed = Vec::new();
    for device in session.cameras()? {
        let modes = if known.contains(&device.link) {
            None
        } else {
            // A camera that does not open now (another application holds
            // it) is listed without modes and read again next time.
            modes_of(&session, &device).ok()
        };
        listed.push(Listed {
            camera_id: device.link,
            name: device.name,
            modes,
        });
    }
    Ok(listed)
}

fn modes_of(session: &Session, device: &mf::Device) -> Result<Vec<CameraMode>, CaptureError> {
    let source = device.open()?;
    let reader = session.reader(source.get(), None)?;
    let offers: Vec<_> = mf::native_offers(&reader)?
        .into_iter()
        .map(|(_, offer)| offer)
        .collect();
    drop(reader);
    drop(source);
    Ok(listed_modes(&offers))
}

impl CameraCapture for MfCamera {
    fn capability(&mut self) -> CameraCaptureCapability {
        match on_own_thread("camera-capability", LIST_TIMEOUT, || {
            Session::start().map(drop)
        }) {
            Ok(()) => CameraCaptureCapability {
                backend: Some(BACKEND.to_owned()),
                available: true,
                unavailable_reason: None,
            },
            Err(err) => CameraCaptureCapability::unavailable(Some(BACKEND), err.message),
        }
    }

    fn cameras(&mut self) -> Result<Vec<Camera>, CaptureError> {
        let known: HashSet<String> = self.modes.keys().cloned().collect();
        let listed = on_own_thread("camera-list", LIST_TIMEOUT, move || list(&known))?;
        Ok(listed
            .into_iter()
            .map(|listed| {
                if let Some(modes) = listed.modes {
                    self.modes.insert(listed.camera_id.clone(), modes);
                }
                Camera {
                    modes: self
                        .modes
                        .get(&listed.camera_id)
                        .cloned()
                        .unwrap_or_default(),
                    camera_id: listed.camera_id,
                    name: listed.name,
                }
            })
            .collect())
    }

    fn start_camera_stream(
        &mut self,
        request: &CameraRequest,
    ) -> Result<CameraStream, CaptureError> {
        let (stream, modes) = start(request)?;
        // A running camera may not open a second time for the listing.
        self.modes.insert(request.camera_id.clone(), modes);
        Ok(stream)
    }
}

/// What a camera thread reports once its camera delivered the first frame.
struct Started {
    format: VideoFormatInfo,
    warnings: Vec<String>,
    /// The camera's modes, as `list_cameras` gives them.
    modes: Vec<CameraMode>,
}

fn start(request: &CameraRequest) -> Result<(CameraStream, Vec<CameraMode>), CaptureError> {
    let (events, received) = mpsc::sync_channel(EVENT_QUEUE);
    let (started, start_answer) = mpsc::sync_channel(1);
    let (stopped, stop_answer) = mpsc::sync_channel(1);
    let sink = Arc::new(FrameSink::new(true, "BGRx"));
    let thread = thread::Builder::new()
        .name("camera".to_owned())
        .spawn({
            let request = request.clone();
            let sink = Arc::clone(&sink);
            let events = events.clone();
            move || {
                let warnings = run(&request, &sink, events, &received, started);
                let _ = stopped.send(warnings);
            }
        })
        .map_err(|err| CaptureError::backend(format!("cannot start the camera thread: {err}")))?;
    match start_answer.recv_timeout(START_TIMEOUT) {
        Ok(Ok(Started {
            format,
            warnings,
            modes,
        })) => Ok((
            CameraStream {
                stream: Box::new(MfCameraStream {
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
            },
            modes,
        )),
        Ok(Err(err)) => Err(err),
        Err(RecvTimeoutError::Timeout) => {
            // The thread closes the camera once its open returns.
            let _ = events.try_send(ReaderEvent::Stop);
            Err(CaptureError::new(
                CaptureErrorKind::Timeout,
                format!(
                    "camera `{}` did not deliver a frame within {} s",
                    request.camera_id,
                    START_TIMEOUT.as_secs()
                ),
            ))
        }
        Err(RecvTimeoutError::Disconnected) => Err(CaptureError::backend(
            "the camera thread ended without an answer",
        )),
    }
}

/// An open camera: its source and asynchronous reader, released (reader
/// first) before the thread's session.
struct Open {
    reader: IMFSourceReader,
    _source: Source,
    name: String,
    layout: Layout,
    format: VideoFormatInfo,
    warnings: Vec<String>,
    modes: Vec<CameraMode>,
}

fn open(
    session: &Session,
    request: &CameraRequest,
    events: SyncSender<ReaderEvent>,
) -> Result<Open, CaptureError> {
    let device = session
        .cameras()?
        .into_iter()
        .find(|device| device.link == request.camera_id)
        .ok_or_else(|| {
            CaptureError::new(
                CaptureErrorKind::NotFound,
                format!(
                    "camera `{}` is not connected; see list_cameras",
                    request.camera_id
                ),
            )
        })?;
    let source = device.open()?;
    let reader = session.reader(source.get(), Some(events))?;
    let natives = mf::native_offers(&reader)?;
    let offers: Vec<ModeOffer<CameraFormat>> = natives.iter().map(|(_, offer)| *offer).collect();
    let choice = choose_mode(&offers, request).ok_or_else(|| {
        CaptureError::unsupported(format!(
            "camera `{}` offers no format StreamForge reads \
             (RGB, YUY2, UYVY, NV12, I420 or MJPEG)",
            device.name
        ))
    })?;
    let (layout, chosen, mut warnings) = select(session, &reader, &natives, choice.offer)?;
    warnings.splice(0..0, choice.warnings);
    Ok(Open {
        reader,
        _source: source,
        format: VideoFormatInfo {
            pixel_format: chosen.format.name().to_owned(),
            width: chosen.width,
            height: chosen.height,
            framerate: chosen.fps,
        },
        name: device.name,
        layout,
        warnings,
        modes: listed_modes(&offers),
    })
}

/// Select the chosen mode, or another format of it if Media Foundation
/// cannot convert that one, with a warning then.
fn select(
    session: &Session,
    reader: &IMFSourceReader,
    natives: &[(IMFMediaType, ModeOffer<CameraFormat>)],
    chosen: ModeOffer<CameraFormat>,
) -> Result<(Layout, ModeOffer<CameraFormat>, Vec<String>), CaptureError> {
    let mut candidates: Vec<&(IMFMediaType, ModeOffer<CameraFormat>)> = natives
        .iter()
        .filter(|(_, offer)| offer.size() == chosen.size() && offer.fps == chosen.fps)
        .collect();
    candidates.sort_by_key(|(_, offer)| {
        (
            offer.format != chosen.format,
            capture_api::OfferFormat::rank(offer.format),
        )
    });
    candidates.dedup_by_key(|(_, offer)| offer.format);
    let mut first_error = None;
    for (native, offer) in candidates {
        match session.select(reader, native) {
            Ok(layout) => {
                let warnings = first_error
                    .map(|err: CaptureError| {
                        vec![format!(
                            "the camera's {} frames could not be converted ({}); using {}",
                            chosen.format.name(),
                            err.message,
                            offer.format.name()
                        )]
                    })
                    .unwrap_or_default();
                return Ok((layout, *offer, warnings));
            }
            Err(err) => {
                first_error.get_or_insert(err);
            }
        }
    }
    Err(first_error.unwrap_or_else(|| CaptureError::backend("the chosen mode has no media type")))
}

/// The camera thread: open, deliver frames until stopped, close. Returns
/// the warnings of the stop.
fn run(
    request: &CameraRequest,
    sink: &FrameSink,
    events: SyncSender<ReaderEvent>,
    received: &Receiver<ReaderEvent>,
    started: SyncSender<Result<Started, CaptureError>>,
) -> Vec<String> {
    // Declared first: dropped last, after every Media Foundation object.
    let session = match Session::start() {
        Ok(session) => session,
        Err(err) => {
            let _ = started.send(Err(err));
            return Vec::new();
        }
    };
    let open = match open(&session, request, events) {
        Ok(open) => open,
        Err(err) => {
            let _ = started.send(Err(err));
            return Vec::new();
        }
    };
    let mut camera = Running {
        open,
        started: Some(started),
        start_failed: false,
        reading: false,
    };
    camera.read(sink);
    // The reader's callback holds a sender too, so the queue does not
    // close by itself: leave on the stop request, or when the start failed
    // (its caller is gone then).
    while !camera.start_failed
        && let Ok(event) = received.recv()
    {
        match event {
            ReaderEvent::Stop => break,
            ReaderEvent::Flushed => {}
            ReaderEvent::Sample {
                status,
                flags,
                timestamp,
                sample,
            } => {
                camera.reading = false;
                if camera.on_sample(sink, status, flags, timestamp, sample.as_ref()) {
                    camera.read(sink);
                }
            }
        }
    }
    let warnings = camera.close(received);
    drop(session);
    warnings
}

struct Running {
    open: Open,
    /// Answered once: with the first frame, or with the error before it.
    started: Option<SyncSender<Result<Started, CaptureError>>>,
    /// The camera ended before its first frame; the start failed.
    start_failed: bool,
    /// A sample was asked for and has not arrived.
    reading: bool,
}

impl Running {
    fn read(&mut self, sink: &FrameSink) {
        match mf::request_sample(&self.open.reader) {
            Ok(()) => self.reading = true,
            Err(err) => self.fail(sink, err),
        }
    }

    /// The camera ended: fail the start, or end the running capture.
    fn fail(&mut self, sink: &FrameSink, err: CaptureError) {
        let reason = err.message.clone();
        if let Some(started) = self.started.take() {
            let _ = started.send(Err(err));
            self.start_failed = true;
        }
        sink.end(reason);
    }

    /// Handle one sample; whether to ask for the next.
    fn on_sample(
        &mut self,
        sink: &FrameSink,
        status: windows::core::HRESULT,
        flags: u32,
        timestamp: i64,
        sample: Option<&Sample>,
    ) -> bool {
        let arrival_ns = sink.elapsed_ns();
        let flags = flags as i32;
        if let Err(err) = status.ok() {
            let context = format!("camera `{}` stopped delivering", self.open.name);
            self.fail(sink, mf::error(context, &err));
            return false;
        }
        if flags & MF_SOURCE_READERF_ERROR.0 != 0 {
            let err =
                CaptureError::backend(format!("camera `{}` reported an error", self.open.name));
            self.fail(sink, err);
            return false;
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 != 0 {
            let err = CaptureError::new(
                CaptureErrorKind::NotFound,
                format!(
                    "camera `{}` ended its stream (disconnected?)",
                    self.open.name
                ),
            );
            self.fail(sink, err);
            return false;
        }
        if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 != 0 {
            match mf::current_layout(&self.open.reader) {
                Ok(layout) => self.open.layout = layout,
                Err(err) => {
                    self.fail(sink, err);
                    return false;
                }
            }
        }
        let Some(sample) = sample else {
            // A gap in the stream (a stream tick), no picture.
            sink.observe_empty(arrival_ns);
            return true;
        };
        let pts_ns = timestamp.checked_mul(100);
        let layout = self.open.layout;
        let delivered = mf::with_rows(sample, layout, |rows| {
            let frame = MappedFrame {
                width: layout.width,
                height: layout.height,
                stride: rows.span.stride,
                data: rows.data,
            };
            if rows.span.bottom_up {
                sink.publish_bottom_up(frame, pts_ns)
            } else {
                sink.publish(frame, pts_ns)
            }
        })
        .and_then(|delivered| delivered);
        sink.observe_frame(
            arrival_ns,
            layout.width,
            layout.height,
            pts_ns,
            Some(delivered),
        );
        if let Some(started) = self.started.take() {
            let _ = started.send(Ok(Started {
                format: self.open.format.clone(),
                warnings: self.open.warnings.clone(),
                modes: std::mem::take(&mut self.open.modes),
            }));
        }
        true
    }

    /// Flush a pending read (bounded), then release the camera.
    fn close(self, received: &Receiver<ReaderEvent>) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.reading {
            match mf::flush(&self.open.reader) {
                Ok(()) => {
                    let deadline = Instant::now() + FLUSH_TIMEOUT;
                    loop {
                        let left = deadline.saturating_duration_since(Instant::now());
                        match received.recv_timeout(left) {
                            Ok(ReaderEvent::Flushed) => break,
                            Ok(_) => {}
                            Err(_) => {
                                warnings.push(format!(
                                    "the camera did not flush within {} s",
                                    FLUSH_TIMEOUT.as_secs()
                                ));
                                break;
                            }
                        }
                    }
                }
                Err(err) => warnings.push(err.message),
            }
        }
        drop(self.open);
        warnings
    }
}

struct MfCameraStream {
    sink: Arc<FrameSink>,
    events: SyncSender<ReaderEvent>,
    stopped: Receiver<Vec<String>>,
    thread: Option<JoinHandle<()>>,
    source: VideoSourceInfo,
    format: VideoFormatInfo,
    warnings: Vec<String>,
    started: Instant,
}

impl VideoStream for MfCameraStream {
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
        let _ = self.events.try_send(ReaderEvent::Stop);
        let mut warnings = std::mem::take(&mut self.warnings);
        match self.stopped.recv_timeout(STOP_TIMEOUT) {
            Ok(stop_warnings) => {
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
                warnings.extend(stop_warnings);
            }
            Err(_) => warnings.push(format!(
                "the camera did not close within {} s; it closes on its own thread",
                STOP_TIMEOUT.as_secs()
            )),
        }
        let mut report = self.sink.report(
            BACKEND,
            self.source.clone(),
            "system memory",
            warnings,
            self.started.elapsed(),
        );
        // The camera's own format and rate, not the converted frames'.
        report.format = Some(self.format.clone());
        Ok(report)
    }
}

impl Drop for MfCameraStream {
    fn drop(&mut self) {
        // Without a stop: the thread closes the camera on its own.
        let _ = self.events.try_send(ReaderEvent::Stop);
    }
}
