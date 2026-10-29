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
    CaptureError, CaptureErrorKind, CaptureState, FrameSink, ModeOffer, VideoCaptureReport,
    VideoFormatInfo, VideoSourceInfo, VideoStream, choose_mode, listed_modes,
};
use media_core::FrameMailbox;
use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, IMFSourceReader, MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR,
    MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED,
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

enum ListingEvent {
    Devices(Vec<Listed>),
    Modes {
        index: usize,
        modes: Vec<CameraMode>,
    },
    Finished(Result<(), CaptureError>),
}

/// Send the devices before opening any, then each camera's modes as read.
fn list(known: &HashSet<String>, answer: &SyncSender<ListingEvent>) -> Result<(), CaptureError> {
    let session = Session::start()?;
    let devices = session.cameras()?;
    let listed = devices
        .iter()
        .map(|device| Listed {
            camera_id: device.link.clone(),
            name: device.name.clone(),
            modes: None,
        })
        .collect();
    if answer.send(ListingEvent::Devices(listed)).is_err() {
        return Ok(());
    }
    for (index, device) in devices.iter().enumerate() {
        if known.contains(&device.link) {
            continue;
        }
        match modes_of(&session, device) {
            Ok(modes) => {
                if answer.send(ListingEvent::Modes { index, modes }).is_err() {
                    break;
                }
            }
            // Privacy is global: preserve the error and its Settings hint.
            Err(err) if err.kind == CaptureErrorKind::PermissionDenied => return Err(err),
            // In-use or otherwise unreadable cameras remain listed without
            // modes and are retried next time (docs/platforms.md).
            Err(_) => {}
        }
    }
    Ok(())
}

fn receive_listing(
    answered: &Receiver<ListingEvent>,
    timeout: Duration,
) -> Result<Vec<Listed>, CaptureError> {
    let deadline = Instant::now() + timeout;
    let mut listed = Vec::new();
    loop {
        match answered.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(ListingEvent::Devices(devices)) => listed = devices,
            Ok(ListingEvent::Modes { index, modes }) => listed[index].modes = Some(modes),
            Ok(ListingEvent::Finished(result)) => {
                result?;
                return Ok(listed);
            }
            Err(RecvTimeoutError::Timeout) => return Ok(listed),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(CaptureError::backend(
                    "the camera listing thread ended without an answer",
                ));
            }
        }
    }
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
        let (answer, answered) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("camera-list".to_owned())
            .spawn(move || {
                let result = list(&known, &answer);
                let _ = answer.send(ListingEvent::Finished(result));
            })
            .map_err(|err| CaptureError::backend(format!("cannot start a camera thread: {err}")))?;
        let listed = receive_listing(&answered, LIST_TIMEOUT)?;
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
                let warnings = run(&request, &sink, events, received, started);
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
        Ok(Err(err)) => {
            // The error is sent only after the camera/session teardown.
            let _ = thread.join();
            Err(err)
        }
        Err(RecvTimeoutError::Timeout) => {
            let _ = stop_thread(&events, &stop_answer, &mut Some(thread));
            Err(CaptureError::new(
                CaptureErrorKind::Timeout,
                format!(
                    "camera `{}` did not deliver a frame within {} s",
                    request.camera_id,
                    START_TIMEOUT.as_secs()
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

/// Stop and join only after the worker confirms all native teardown is done.
/// A driver stuck in a native call can exceed the bound; its handle detaches.
fn stop_thread(
    events: &SyncSender<ReaderEvent>,
    stopped: &Receiver<Vec<String>>,
    thread: &mut Option<JoinHandle<()>>,
) -> Result<Vec<String>, RecvTimeoutError> {
    let Some(thread) = thread.take() else {
        return Ok(Vec::new());
    };
    let _ = events.try_send(ReaderEvent::Stop);
    let warnings = stopped.recv_timeout(STOP_TIMEOUT)?;
    let _ = thread.join();
    Ok(warnings)
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
    received: Receiver<ReaderEvent>,
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
            drain_events(received);
            drop(session);
            let _ = started.send(Err(err));
            return Vec::new();
        }
    };
    let mut camera = Running {
        open,
        started: Some(started),
        start_error: None,
        reading: false,
    };
    camera.read(sink);
    // The reader's callback holds a sender too, so the queue does not
    // close by itself: leave on the stop request or a failed start.
    while camera.start_error.is_none()
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
    // Hold a failed start's answer until the device and session are free.
    let failed_start = camera
        .start_error
        .take()
        .map(|err| (camera.started.take().expect("unanswered start"), err));
    let warnings = camera.close(&received);
    // close drops reader/source first, then every queued sample must go
    // before MFShutdown/CoUninitialize. run owns the receiver itself.
    drain_events(received);
    drop(session);
    if let Some((started, err)) = failed_start {
        let _ = started.send(Err(err));
    }
    warnings
}

fn drain_events(received: Receiver<ReaderEvent>) {
    while let Ok(event) = received.try_recv() {
        drop(event);
    }
    drop(received);
}

struct Running {
    open: Open,
    /// Answered once: with the first frame, or with the error before it.
    started: Option<SyncSender<Result<Started, CaptureError>>>,
    /// The camera ended before its first frame; the start failed.
    start_error: Option<CaptureError>,
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
        if self.started.is_some() {
            self.start_error = Some(err);
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
        if flags
            & (MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0
                | MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED.0)
            != 0
        {
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
            sink.publish_rows(layout.width, layout.height, rows, pts_ns)
        })
        .and_then(|delivered| delivered);
        let first_delivery_error = delivered.as_ref().err().cloned();
        let published = matches!(delivered, Ok(true));
        sink.observe_frame(
            arrival_ns,
            layout.width,
            layout.height,
            pts_ns,
            Some(delivered),
        );
        if self.started.is_some()
            && let Some(err) = first_delivery_error
        {
            self.fail(sink, CaptureError::backend(err));
            return false;
        }
        if published && let Some(started) = self.started.take() {
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
        let mut warnings = std::mem::take(&mut self.warnings);
        match stop_thread(&self.events, &self.stopped, &mut self.thread) {
            Ok(stop_warnings) => warnings.extend(stop_warnings),
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

impl Drop for MfCameraStream {
    fn drop(&mut self) {
        let _ = stop_thread(&self.events, &self.stopped, &mut self.thread);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_stream() -> (Box<MfCameraStream>, Arc<std::sync::atomic::AtomicBool>) {
        let (events, received) = mpsc::sync_channel(1);
        let (stopped, answer) = mpsc::sync_channel(1);
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_released = Arc::clone(&released);
        let thread = thread::spawn(move || {
            assert!(matches!(received.recv().unwrap(), ReaderEvent::Stop));
            worker_released.store(true, std::sync::atomic::Ordering::SeqCst);
            stopped.send(Vec::new()).unwrap();
        });
        (
            Box::new(MfCameraStream {
                sink: Arc::new(FrameSink::new(true, "BGRx")),
                events,
                stopped: answer,
                thread: Some(thread),
                source: VideoSourceInfo {
                    kind: None,
                    width: Some(640),
                    height: Some(480),
                },
                format: VideoFormatInfo {
                    pixel_format: "MJPG".to_owned(),
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
        assert!(released.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn stop_reports_observed_size_but_keeps_native_format_and_rate() {
        let (stream, released) = fake_stream();
        stream
            .sink
            .observe_frame(0, 1280, 720, Some(0), Some(Ok(true)));
        let report = stream.stop().expect("stop");
        assert!(released.load(std::sync::atomic::Ordering::SeqCst));
        let format = report.format.expect("format");
        assert_eq!((format.width, format.height), (1280, 720));
        assert_eq!(format.pixel_format, "MJPG");
        assert_eq!(format.framerate, capture_api::Rational::integer(30));
        assert_eq!(
            report.warnings,
            ["the camera changed its size from 640x480 to 1280x720 while running"]
        );
    }

    fn listed(id: &str) -> Listed {
        Listed {
            camera_id: id.to_owned(),
            name: id.to_owned(),
            modes: None,
        }
    }

    #[test]
    fn listing_timeout_keeps_devices_and_completed_modes() {
        let (answer, answered) = mpsc::sync_channel(4);
        answer
            .send(ListingEvent::Devices(vec![listed("fast"), listed("slow")]))
            .unwrap();
        let modes = vec![CameraMode {
            width: 640,
            height: 480,
            fps: capture_api::Rational::integer(30),
        }];
        answer
            .send(ListingEvent::Modes {
                index: 0,
                modes: modes.clone(),
            })
            .unwrap();
        let result = receive_listing(&answered, Duration::from_millis(1)).expect("partial listing");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].modes, Some(modes));
        assert_eq!(result[1].camera_id, "slow");
        assert!(result[1].modes.is_none());
    }

    #[test]
    fn listing_preserves_permission_denial_and_its_hint() {
        let (answer, answered) = mpsc::sync_channel(2);
        answer
            .send(ListingEvent::Devices(vec![listed("denied")]))
            .unwrap();
        let message = "allow desktop apps in Settings > Privacy & security > Camera";
        answer
            .send(ListingEvent::Finished(Err(CaptureError::new(
                CaptureErrorKind::PermissionDenied,
                message,
            ))))
            .unwrap();
        let err = receive_listing(&answered, Duration::from_secs(1))
            .err()
            .expect("privacy error");
        assert_eq!(err.kind, CaptureErrorKind::PermissionDenied);
        assert_eq!(err.message, message);
    }

    #[test]
    fn stop_waits_for_teardown_and_joins_the_worker() {
        let (events, received) = mpsc::sync_channel(1);
        let (stopped, answer) = mpsc::sync_channel(1);
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_released = Arc::clone(&released);
        let mut thread = Some(thread::spawn(move || {
            assert!(matches!(received.recv().unwrap(), ReaderEvent::Stop));
            worker_released.store(true, std::sync::atomic::Ordering::SeqCst);
            stopped.send(vec!["closed".to_owned()]).unwrap();
        }));
        assert_eq!(
            stop_thread(&events, &answer, &mut thread).unwrap(),
            ["closed"]
        );
        assert!(thread.is_none());
        assert!(released.load(std::sync::atomic::Ordering::SeqCst));
        // A later Drop after stop must neither send nor wait a second time.
        assert!(
            stop_thread(&events, &answer, &mut thread)
                .unwrap()
                .is_empty()
        );
    }
}
