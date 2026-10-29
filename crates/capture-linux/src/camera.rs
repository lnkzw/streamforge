//! Cameras: the `Video/Source` nodes that WirePlumber creates for V4L2 and
//! libcamera devices (ADR 0019), listed through the registry with the modes
//! of their `EnumFormat` params, and captured on a `camera-capture` thread.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use capture_api::{
    Camera, CameraRequest, CameraStream, CaptureEnd, CaptureError, CaptureErrorKind, CaptureState,
    VideoCaptureReport, VideoFormatInfo, VideoSourceInfo, VideoStream,
};
use media_core::{FrameMailbox, FramePool};
use pipewire as pw;
use pw::registry::GlobalObject;
use pw::spa::param::ParamType;
use pw::types::ObjectType;

use crate::modes::{self, ModeOffer};
use crate::pw::{BACKEND, Connection, RunUntil};
use crate::video::{self, Delivery, Target, VideoRun};

/// How long a camera may take to start streaming (opening a V4L2 device
/// can take a second).
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// In flight in the capture callback, waiting in the mailbox, held by the
/// consumer, plus one spare.
const STREAM_BUFFERS: usize = 4;

/// A camera node as the registry announces it, with what it can deliver.
#[derive(Debug, Clone)]
pub(crate) struct CameraNode {
    /// `node.name`: the camera's id.
    pub name: String,
    pub description: Option<String>,
    /// Every mode StreamForge can convert.
    pub offers: Vec<ModeOffer>,
}

impl CameraNode {
    pub fn listing(&self) -> Camera {
        Camera {
            camera_id: self.name.clone(),
            name: self
                .description
                .clone()
                .unwrap_or_else(|| self.name.clone()),
            modes: capture_api::listed_modes(&self.offers),
        }
    }
}

/// A video source is a camera when it comes from a camera device API or
/// has the camera role (virtual cameras); screen-cast nodes have neither.
fn is_camera(device_api: Option<&str>, media_role: Option<&str>) -> bool {
    media_role == Some("Camera") || matches!(device_api, Some("v4l2" | "libcamera"))
}

/// The properties that tell a camera apart, from a bound node's info:
/// registry globals carry `media.role` but not `device.api`.
#[derive(Debug, Default)]
struct NodeKind {
    device_api: Option<String>,
    media_role: Option<String>,
}

/// List the cameras, sorted by id. Both roundtrips share `timeout`.
pub(crate) fn camera_nodes(timeout: Duration) -> Result<Vec<CameraNode>, CaptureError> {
    let deadline = Instant::now() + timeout;
    let connection = Connection::connect()?;
    let registry = connection
        .core
        .get_registry()
        .map_err(|err| CaptureError::backend(format!("cannot get PipeWire registry: {err}")))?;
    // Every video source; which are cameras shows once they are bound.
    let globals = Rc::new(RefCell::new(Vec::new()));
    let _listener = registry
        .add_listener_local()
        .global({
            let globals = Rc::clone(&globals);
            move |global| {
                if global.type_ != ObjectType::Node {
                    return;
                }
                let Some(props) = global.props else {
                    return;
                };
                let Some(name) = props.get(*pw::keys::NODE_NAME) else {
                    return;
                };
                if props.get(*pw::keys::MEDIA_CLASS) == Some("Video/Source") {
                    let description = props
                        .get(*pw::keys::NODE_DESCRIPTION)
                        .or_else(|| props.get(*pw::keys::NODE_NICK))
                        .map(str::to_owned);
                    globals
                        .borrow_mut()
                        .push((global.to_owned(), name.to_owned(), description));
                }
            }
        })
        .register();
    connection.roundtrip(timeout)?;
    let globals: Vec<(GlobalObject<_>, String, Option<String>)> = globals.take();

    // Each node's info and EnumFormat params; nodes run in the daemon, so
    // they answer before the next roundtrip completes.
    let kinds = Rc::new(RefCell::new(
        std::iter::repeat_with(NodeKind::default)
            .take(globals.len())
            .collect::<Vec<_>>(),
    ));
    let params = Rc::new(RefCell::new(vec![Vec::new(); globals.len()]));
    let mut bound = Vec::with_capacity(globals.len());
    for (index, (global, name, _)) in globals.iter().enumerate() {
        let node: pw::node::Node = registry.bind(global).map_err(|err| {
            CaptureError::backend(format!("cannot bind video node `{name}`: {err}"))
        })?;
        let listener = node
            .add_listener_local()
            .info({
                let kinds = Rc::clone(&kinds);
                move |info| {
                    let Some(props) = info.props() else {
                        return;
                    };
                    let get = |key: &str| props.get(key).map(str::to_owned);
                    kinds.borrow_mut()[index] = NodeKind {
                        device_api: get(*pw::keys::DEVICE_API),
                        media_role: get(*pw::keys::MEDIA_ROLE),
                    };
                }
            })
            .param({
                let params = Rc::clone(&params);
                move |_seq, id, _index, _next, param| {
                    if let (ParamType::EnumFormat, Some(param)) = (id, param) {
                        params.borrow_mut()[index].push(param.as_bytes().to_vec());
                    }
                }
            })
            .register();
        node.enum_params(0, Some(ParamType::EnumFormat), 0, u32::MAX);
        bound.push((node, listener));
    }
    if !bound.is_empty() {
        connection.roundtrip(deadline.saturating_duration_since(Instant::now()))?;
    }
    drop(bound);
    let (kinds, params) = (kinds.take(), params.take());

    let mut nodes: Vec<CameraNode> = globals
        .into_iter()
        .zip(kinds)
        .zip(params)
        .filter(|((_, kind), _)| is_camera(kind.device_api.as_deref(), kind.media_role.as_deref()))
        .map(|(((_, name, description), _), params)| CameraNode {
            name,
            description,
            offers: modes::camera_offers(params.iter().map(Vec::as_slice)),
        })
        .collect();
    nodes.sort_by(|a, b| a.name.cmp(&b.name));
    // Two nodes with one name cannot be told apart by id; keep the first.
    nodes.dedup_by(|later, kept| later.name == kept.name);
    Ok(nodes)
}

/// `NotFound` for a camera id that is not listed, with the ids that are.
pub(crate) fn not_found(camera_id: &str, nodes: &[CameraNode]) -> CaptureError {
    CaptureError::new(
        CaptureErrorKind::NotFound,
        format!("camera `{camera_id}` not found; see list_cameras"),
    )
    .with_details(serde_json::json!({
        "camera_id": camera_id,
        "available": nodes.iter().map(|node| node.name.as_str()).collect::<Vec<_>>(),
    }))
}

/// Start capturing `node` on a dedicated thread, in the mode chosen for
/// `request`. Returns once the stream runs, or with why it did not start
/// (the camera is busy, gone or refused the formats).
pub(crate) fn start_stream(
    request: &CameraRequest,
    node: &CameraNode,
) -> Result<CameraStream, CaptureError> {
    let Some(choice) = capture_api::choose_mode(&node.offers, request) else {
        return Err(CaptureError::unsupported(format!(
            "camera `{}` offers no format StreamForge converts \
             (packed RGB, YUY2, UYVY, NV12, I420 or MJPEG)",
            node.name
        )));
    };
    let formats = modes::connect_pods(&node.offers, choice.offer).map_err(CaptureError::backend)?;
    let mailbox = Arc::new(FrameMailbox::new());
    let delivery = Delivery {
        mailbox: Arc::clone(&mailbox),
        pool: FramePool::new(STREAM_BUFFERS),
    };
    let (stop, stop_receiver) = pw::channel::channel();
    let (started, started_receiver) = mpsc::sync_channel(1);
    let ended = Arc::new(CaptureEnd::new());
    let node_name = node.name.clone();
    let thread = thread::Builder::new()
        .name("camera-capture".to_owned())
        .spawn({
            let ended = Arc::clone(&ended);
            move || {
                let run = video::capture(
                    Target::Camera {
                        node_name: node_name.clone(),
                        formats,
                    },
                    RunUntil::Stopped(stop_receiver),
                    Some(delivery),
                    Some(started),
                )
                .map_err(|error| camera_error(&node_name, error));
                // Before a stop, this is why the capture ended on its own.
                ended.end(|| match &run {
                    Ok(_) => format!("camera `{node_name}` went away"),
                    Err(error) => error.message.clone(),
                });
                run
            }
        })
        .map_err(|err| CaptureError::backend(format!("cannot start the capture thread: {err}")))?;
    let joined = |thread: JoinHandle<Result<VideoRun, CaptureError>>| {
        thread
            .join()
            .unwrap_or_else(|_| Err(CaptureError::backend("the camera capture thread panicked")))
    };
    let negotiated = match started_receiver.recv_timeout(START_TIMEOUT) {
        Ok(negotiated) => negotiated,
        Err(RecvTimeoutError::Disconnected) => {
            // The thread ended before streaming; its result says why.
            return Err(joined(thread).err().unwrap_or_else(|| {
                CaptureError::backend("the camera capture ended before it started")
            }));
        }
        Err(RecvTimeoutError::Timeout) => {
            let _ = stop.send(());
            let message = format!(
                "camera `{}` did not start within {} ms; another application may hold it",
                node.name,
                START_TIMEOUT.as_millis()
            );
            let error = CaptureError::timeout(message);
            return Err(match joined(thread) {
                Err(run) => error.with_detail("capture_error", run.message),
                Ok(_) => error,
            });
        }
    };
    let mut warnings = choice.warnings;
    warnings.extend(other_mode(&choice.offer, negotiated.as_ref()));
    let stream = Box::new(LinuxCameraStream {
        source: VideoSourceInfo {
            kind: None,
            width: negotiated.as_ref().map(|format| format.width),
            height: negotiated.as_ref().map(|format| format.height),
        },
        mailbox,
        ended,
        running: Some(Running {
            stop,
            thread,
            warnings: warnings.clone(),
        }),
    });
    Ok(CameraStream {
        stream,
        format: negotiated,
        warnings,
    })
}

/// WirePlumber reports a camera that is gone, at the start or later, as a
/// missing target.
fn camera_error(name: &str, error: CaptureError) -> CaptureError {
    if error.message.contains("target not found") {
        CaptureError::new(
            CaptureErrorKind::NotFound,
            format!("camera `{name}` is gone; see list_cameras"),
        )
        .with_detail("pipewire_error", error.message)
    } else {
        error
    }
}

/// A warning when the camera runs in another mode than the chosen one,
/// e.g. because another application already uses it.
fn other_mode(chosen: &ModeOffer, negotiated: Option<&VideoFormatInfo>) -> Option<String> {
    let negotiated = negotiated?;
    let text = |width: u32, height: u32, fps, format: &str| {
        format!(
            "{width}x{height} at {} fps in {format}",
            capture_api::rate_text(fps)
        )
    };
    let got_fps = capture_api::reduce_rate(negotiated.framerate);
    // A camera that does not state its rates runs at whatever it reports.
    let wanted_fps = if chosen.fps == capture_api::UNSTATED_RATE {
        got_fps
    } else {
        chosen.fps
    };
    let wanted = text(
        chosen.width,
        chosen.height,
        wanted_fps,
        &chosen.format.name(),
    );
    let got = text(
        negotiated.width,
        negotiated.height,
        got_fps,
        &negotiated.pixel_format,
    );
    (wanted != got).then(|| {
        format!("the camera runs at {got}, not the chosen {wanted}; another application may use it")
    })
}

struct Running {
    stop: pw::channel::Sender<()>,
    thread: JoinHandle<Result<VideoRun, CaptureError>>,
    warnings: Vec<String>,
}

struct LinuxCameraStream {
    source: VideoSourceInfo,
    mailbox: Arc<FrameMailbox>,
    /// Set when the capture thread ends.
    ended: Arc<CaptureEnd>,
    running: Option<Running>,
}

impl LinuxCameraStream {
    fn shutdown(&mut self) -> Option<Result<VideoCaptureReport, CaptureError>> {
        let running = self.running.take()?;
        // Fails only if the capture thread already ended (and dropped the
        // receiver); joining below reports why.
        let _ = running.stop.send(());
        let run = running
            .thread
            .join()
            .unwrap_or_else(|_| Err(CaptureError::backend("the camera capture thread panicked")));
        Some(run.map(|mut run| {
            let mut warnings = running.warnings;
            warnings.append(&mut run.warnings);
            VideoCaptureReport {
                backend: BACKEND.to_owned(),
                source: self.source.clone(),
                format: run.format,
                buffer_memory: run.buffer_memory.map(str::to_owned),
                stats: run.stats,
                warnings,
            }
        }))
    }
}

impl VideoStream for LinuxCameraStream {
    fn frames(&self) -> Arc<FrameMailbox> {
        Arc::clone(&self.mailbox)
    }

    fn source(&self) -> &VideoSourceInfo {
        &self.source
    }

    fn state(&self) -> CaptureState {
        let panicked = self
            .running
            .as_ref()
            .is_some_and(|running| running.thread.is_finished());
        // A thread that ends normally sets the reason first.
        if panicked {
            self.ended
                .end(|| "the camera capture thread panicked".to_owned());
        }
        self.ended.state()
    }

    fn stop(mut self: Box<Self>) -> Result<VideoCaptureReport, CaptureError> {
        self.shutdown()
            .unwrap_or_else(|| Err(CaptureError::backend("the stream was already stopped")))
    }
}

impl Drop for LinuxCameraStream {
    fn drop(&mut self) {
        // Join the thread even if the owner never called `stop`; the report
        // is not needed then.
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capture_api::Rational;

    use crate::modes::CameraFormat;

    #[test]
    fn cameras_are_told_apart_from_screen_casts() {
        assert!(is_camera(Some("v4l2"), None));
        assert!(is_camera(Some("libcamera"), None));
        assert!(is_camera(None, Some("Camera")));
        // A portal's screen cast, and other video sources.
        assert!(!is_camera(None, None));
        assert!(!is_camera(None, Some("Screen")));
        assert!(!is_camera(Some("videotestsrc"), None));
    }

    #[test]
    fn a_differing_negotiated_mode_is_reported() {
        let chosen = ModeOffer {
            width: 1280,
            height: 720,
            fps: Rational::integer(30),
            format: CameraFormat::Mjpeg,
        };
        let format = |width, pixel_format: &str| VideoFormatInfo {
            pixel_format: pixel_format.to_owned(),
            width,
            height: 720,
            framerate: Rational::integer(30),
        };
        assert_eq!(other_mode(&chosen, Some(&format(1280, "MJPG"))), None);
        // An equal rate written differently is the same mode.
        let doubled = VideoFormatInfo {
            framerate: Rational { num: 60, den: 2 },
            ..format(1280, "MJPG")
        };
        assert_eq!(other_mode(&chosen, Some(&doubled)), None);
        assert_eq!(other_mode(&chosen, None), None);
        assert_eq!(
            other_mode(&chosen, Some(&format(1280, "YUY2"))).as_deref(),
            Some(
                "the camera runs at 1280x720 at 30 fps in YUY2, not the chosen 1280x720 at 30 \
                 fps in MJPG; another application may use it"
            )
        );
    }

    #[test]
    fn a_missing_target_means_the_camera_is_gone() {
        let error = camera_error(
            "cam",
            CaptureError::backend("PipeWire error on object 2: target not found (-2)"),
        );
        assert_eq!(error.kind, CaptureErrorKind::NotFound);
        assert_eq!(error.message, "camera `cam` is gone; see list_cameras");
        let other = camera_error("cam", CaptureError::backend("boom"));
        assert_eq!(
            (other.kind, other.message.as_str()),
            (CaptureErrorKind::Backend, "boom")
        );
    }

    #[test]
    fn unknown_cameras_name_the_known_ones() {
        let node = CameraNode {
            name: "v4l2_input.usb-cam".into(),
            description: Some("USB Camera".into()),
            offers: Vec::new(),
        };
        let error = not_found("gone", std::slice::from_ref(&node));
        assert_eq!(error.kind, CaptureErrorKind::NotFound);
        assert_eq!(error.message, "camera `gone` not found; see list_cameras");
        let details = error.details.expect("details");
        assert_eq!(
            details["available"],
            serde_json::json!(["v4l2_input.usb-cam"])
        );
        assert_eq!(node.listing().name, "USB Camera");
    }
}
