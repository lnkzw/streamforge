//! Camera sources (ADR 0019) against a fake camera: they start with their
//! scene (also on air) and run until the pipeline stops, a missing camera
//! warns instead of failing, a busy one fails, and a change of camera or
//! mode opens it again.

use std::collections::HashSet;

use capture_api::{
    Camera, CameraCapture, CameraCaptureCapability, CameraMode, CameraRequest, CameraStream,
    CaptureEnd, CaptureState, VideoFormatInfo,
};
use command_api::{CameraModeSpec, CaptureKind, HealthReport, SceneList, SourceKind};

use super::*;

const MODES: [(u32, u32); 2] = [(1280, 720), (640, 480)];

/// What the fake camera saw, shared with the test.
#[derive(Clone, Default)]
struct CameraLog {
    /// Every start request, with how many of its streams were open then.
    starts: Arc<Mutex<Vec<(CameraRequest, usize)>>>,
    /// Streams started and not stopped yet.
    open: Arc<AtomicUsize>,
    /// One end switch per started stream, in start order.
    ends: Arc<Mutex<Vec<Arc<CaptureEnd>>>>,
    /// Cameras that are not plugged in.
    unplugged: Arc<Mutex<HashSet<String>>>,
    /// Cameras another application holds.
    busy: Arc<Mutex<HashSet<String>>>,
}

impl CameraLog {
    fn starts(&self) -> Vec<(CameraRequest, usize)> {
        self.starts.lock().expect("starts").clone()
    }

    fn open(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }

    fn plug(&self, camera_id: &str, plugged: bool) {
        set(&self.unplugged, camera_id, !plugged);
    }

    fn hold(&self, camera_id: &str, held: bool) {
        set(&self.busy, camera_id, held);
    }
}

fn set(ids: &Mutex<HashSet<String>>, id: &str, member: bool) {
    let mut ids = ids.lock().expect("ids");
    if member {
        ids.insert(id.to_owned());
    } else {
        ids.remove(id);
    }
}

/// The colour of the frame the stream of start `index` (0 for the first)
/// shows, as the preview has it (BGRA).
fn color(index: usize) -> [u8; 4] {
    [255, 20 * (index as u8 + 1), 0, 255]
}

/// Where a camera added by [`add_camera`] shows in the 480x270 preview.
const CAMERA_PIXEL: (u32, u32) = (40, 40);

/// Wait for the preview to show the stream of start `index`.
fn shows(engine: &Engine, index: usize) {
    wait_for_preview_pixel(engine, CAMERA_PIXEL.0, CAMERA_PIXEL.1, color(index));
}

/// The preview a while from now does not show any camera stream.
fn shows_no_camera(engine: &Engine) {
    let frames = engine.preview_frames();
    std::thread::sleep(Duration::from_millis(200));
    let _ = frames.take();
    let deadline = Instant::now() + Duration::from_secs(2);
    let frame = loop {
        if let Some(frame) = frames.take() {
            break frame;
        }
        assert!(Instant::now() < deadline, "no preview frame");
        std::thread::sleep(Duration::from_millis(10));
    };
    let pixel = frame.pixel(CAMERA_PIXEL.0, CAMERA_PIXEL.1);
    assert!(
        !(pixel[0] == 255 && pixel[2] == 0 && pixel[1] % 20 == 0 && pixel[1] > 0),
        "a camera still shows: {pixel:?}"
    );
}

/// `cam:1` and `cam:2` run in 1280x720 or 640x480 at 30 fps, each stream
/// in its own colour ([`color`]); `cam:busy` (and a held one) does not
/// start in time, as one another application holds; every other camera
/// (and an unplugged one) is not found.
struct FakeCamera(CameraLog);

impl CameraCapture for FakeCamera {
    fn capability(&mut self) -> CameraCaptureCapability {
        CameraCaptureCapability {
            backend: Some("fake".into()),
            available: true,
            unavailable_reason: None,
        }
    }

    fn cameras(&mut self) -> Result<Vec<Camera>, CaptureError> {
        Ok(["cam:1", "cam:2"]
            .into_iter()
            .map(|id| Camera {
                camera_id: id.to_owned(),
                name: id.to_owned(),
                modes: MODES
                    .iter()
                    .map(|&(width, height)| CameraMode {
                        width,
                        height,
                        fps: Rational::integer(30),
                    })
                    .collect(),
            })
            .collect())
    }

    fn start_camera_stream(
        &mut self,
        request: &CameraRequest,
    ) -> Result<CameraStream, CaptureError> {
        let log = &self.0;
        let index = {
            let mut starts = log.starts.lock().expect("starts");
            starts.push((request.clone(), log.open()));
            starts.len() - 1
        };
        let id = request.camera_id.as_str();
        if id == "cam:busy" || log.busy.lock().expect("busy").contains(id) {
            return Err(CaptureError::timeout(format!(
                "camera `{id}` did not start within 5000 ms; another application may hold it"
            ))
            .with_detail("capture_error", "device busy"));
        }
        if !["cam:1", "cam:2"].contains(&id)
            || log.unplugged.lock().expect("unplugged").contains(id)
        {
            return Err(CaptureError::new(
                CaptureErrorKind::NotFound,
                format!("camera `{id}` not found; see list_cameras"),
            ));
        }
        let mut warnings = Vec::new();
        let (width, height) = match request.size {
            Some(size) if MODES.contains(&size) => size,
            Some((width, height)) => {
                warnings.push(format!(
                    "the camera has no {width}x{height} mode; using 1280x720"
                ));
                MODES[0]
            }
            None => MODES[0],
        };
        let mailbox = Arc::new(FrameMailbox::new());
        // One small frame in this stream's colour.
        let [b, g, r, _] = color(index);
        mailbox.publish(
            VideoFrame::new(
                16,
                9,
                16 * 4,
                PixelFormat::Bgrx8,
                None,
                FrameBuffer::unpooled([b, g, r, 0].repeat(16 * 9)),
            )
            .expect("frame"),
        );
        let ended = Arc::new(CaptureEnd::new());
        log.ends.lock().expect("ends").push(Arc::clone(&ended));
        log.open.fetch_add(1, Ordering::SeqCst);
        Ok(CameraStream {
            stream: Box::new(FakeCameraStream {
                mailbox,
                ended,
                open: Some(Arc::clone(&log.open)),
                source: VideoSourceInfo {
                    kind: None,
                    width: Some(width),
                    height: Some(height),
                },
            }),
            format: Some(VideoFormatInfo {
                pixel_format: "YUY2".to_owned(),
                width,
                height,
                framerate: Rational::integer(30),
            }),
            warnings,
        })
    }
}

struct FakeCameraStream {
    mailbox: Arc<FrameMailbox>,
    ended: Arc<CaptureEnd>,
    /// Counted down once, when stopped or dropped.
    open: Option<Arc<AtomicUsize>>,
    source: VideoSourceInfo,
}

impl Drop for FakeCameraStream {
    fn drop(&mut self) {
        if let Some(open) = self.open.take() {
            open.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl VideoStream for FakeCameraStream {
    fn frames(&self) -> Arc<FrameMailbox> {
        Arc::clone(&self.mailbox)
    }

    fn source(&self) -> &VideoSourceInfo {
        &self.source
    }

    fn state(&self) -> CaptureState {
        self.ended.state()
    }

    fn stop(self: Box<Self>) -> Result<VideoCaptureReport, CaptureError> {
        Ok(VideoCaptureReport {
            backend: "fake".into(),
            source: self.source.clone(),
            format: None,
            buffer_memory: None,
            stats: VideoStatsAccumulator::new().finish(Duration::from_millis(1)),
            warnings: Vec::new(),
        })
    }
}

fn engine_with_cameras() -> (Engine, CameraLog) {
    let log = CameraLog::default();
    let engine = Engine::with_capture(Capture {
        screen: Box::new(FakeScreen {
            capability_queries: Arc::new(AtomicUsize::new(0)),
            screen_starts: Arc::new(AtomicUsize::new(0)),
            screen_ends: Arc::default(),
        }),
        audio: Box::new(FakeAudio {
            played: Arc::default(),
        }),
        camera: Box::new(FakeCamera(log.clone())),
    });
    (engine, log)
}

fn scenes(output: CommandOutput) -> SceneList {
    match output {
        CommandOutput::SceneList(list) => list,
        other => panic!("unexpected output: {other:?}"),
    }
}

fn add_camera(
    engine: &mut Engine,
    scene: &str,
    name: &str,
    camera_id: &str,
) -> Result<SceneList, CommandError> {
    try_run(
        engine,
        "add_source",
        json!({"scene_name": scene, "name": name, "kind": "camera", "camera_id": camera_id,
               "x": 0, "y": 0, "width": 640, "height": 360}),
    )
    .map(scenes)
}

fn camera_health(health: &HealthReport, name: &str) -> command_api::CaptureHealth {
    health
        .captures
        .iter()
        .find(|capture| capture.name == name)
        .cloned()
        .unwrap_or_else(|| panic!("no capture `{name}`: {:?}", health.captures))
}

fn source_kind(engine: &mut Engine, scene: &str, name: &str) -> SourceKind {
    let list = scene_list(engine);
    list.scenes
        .iter()
        .find(|s| s.name == scene)
        .and_then(|s| s.sources.iter().find(|source| source.name == name))
        .map(|source| source.kind.clone())
        .unwrap_or_else(|| panic!("no source `{name}`"))
}

fn update(engine: &mut Engine, params: serde_json::Value) -> Result<SceneList, CommandError> {
    try_run(engine, "update_source", params).map(scenes)
}

#[test]
fn a_camera_starts_with_its_scene_also_on_air_and_runs_until_the_pipeline_stops() {
    let _media = media_test_lock();
    let dir = TempDir::new("camera-on-air");
    let (engine, log) = engine_with_cameras();
    let mut engine = engine.with_output_dir(&dir.0);
    run(&mut engine, "create_scene", json!({"name": "Cam"}));
    add_camera(&mut engine, "Cam", "cam", "cam:1").expect("add");
    run(&mut engine, "set_preview", json!({"enabled": true}));
    start(&mut engine, json!({"file_name": "cam.mkv"})).expect("start");
    // Not rendered yet: the camera stays off.
    assert!(log.starts().is_empty());

    let switched = scenes(run(
        &mut engine,
        "set_active_scene",
        json!({"scene_name": "Cam"}),
    ));
    assert!(switched.warnings.is_empty(), "{:?}", switched.warnings);
    assert_eq!(log.open(), 1);
    let (request, _) = &log.starts()[0];
    assert_eq!(
        (request.camera_id.as_str(), request.size, request.fps),
        ("cam:1", None, None)
    );
    let cam = camera_health(&health(&mut engine), "cam");
    assert_eq!(cam.kind, CaptureKind::Camera);
    assert_eq!(cam.state, CaptureState::Running);
    let format = cam.format.expect("negotiated format");
    assert_eq!(
        (format.pixel_format.as_str(), format.width, format.height),
        ("YUY2", 1280, 720)
    );
    shows(&engine, 0);

    // Switching away and back does not reopen it; it keeps running.
    run(
        &mut engine,
        "set_active_scene",
        json!({"scene_name": "Test pattern"}),
    );
    assert_eq!(log.open(), 1);
    run(
        &mut engine,
        "set_active_scene",
        json!({"scene_name": "Cam"}),
    );
    assert_eq!((log.starts().len(), log.open()), (1, 1));
    // A kept camera of a scene that is not rendered restarts alone.
    run(
        &mut engine,
        "set_active_scene",
        json!({"scene_name": "Test pattern"}),
    );
    run(
        &mut engine,
        "restart_source",
        json!({"scene_name": "Cam", "source_name": "cam"}),
    );
    assert_eq!((log.starts().len(), log.open()), (2, 1));
    // Back on program, the scene shows the new stream.
    run(
        &mut engine,
        "set_active_scene",
        json!({"scene_name": "Cam"}),
    );
    shows(&engine, 1);

    engine.handle(Command::StopRecording).expect("stop");
    // The preview still runs: so does the camera.
    assert_eq!(log.open(), 1);
    let CommandOutput::PreviewState(preview) =
        run(&mut engine, "set_preview", json!({"enabled": false}))
    else {
        panic!("unexpected output");
    };
    assert_eq!(log.open(), 0);
    let report = preview.pipeline.expect("pipeline report");
    let cams: Vec<_> = report
        .scene_captures
        .iter()
        .filter(|outcome| outcome.name == "cam")
        .collect();
    assert_eq!(cams.len(), 2, "{:?}", report.scene_captures);
    assert!(
        cams.iter()
            .all(|outcome| outcome.kind == CaptureKind::Camera)
    );
}

#[test]
fn removing_a_camera_stops_it_and_restart_reopens_an_ended_one() {
    let _media = media_test_lock();
    let (mut engine, log) = engine_with_cameras();
    run(&mut engine, "set_preview", json!({"enabled": true}));
    add_camera(&mut engine, "Test pattern", "cam", "cam:1").expect("add");
    assert_eq!(log.open(), 1);
    shows(&engine, 0);

    // Unplugged: it ends like a screen capture, with the reason.
    log.ends.lock().expect("ends")[0].end(|| "camera `cam:1` went away".to_owned());
    assert_eq!(
        camera_health(&health(&mut engine), "cam").state,
        CaptureState::Ended {
            reason: "camera `cam:1` went away".to_owned()
        }
    );
    // Plugged in again: restart_source reopens it, the old one stopped
    // first.
    run(
        &mut engine,
        "restart_source",
        json!({"scene_name": "Test pattern", "source_name": "cam"}),
    );
    let starts = log.starts();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[1].1, 0, "the old camera was still open");
    assert_eq!(log.open(), 1);
    assert_eq!(
        camera_health(&health(&mut engine), "cam").state,
        CaptureState::Running
    );
    shows(&engine, 1);

    // Held by another application: the restart fails, and the source shows
    // nothing (not the closed camera's last frame) until it is restarted.
    log.hold("cam:1", true);
    let busy = json!({"scene_name": "Test pattern", "source_name": "cam"});
    let error = try_run(&mut engine, "restart_source", busy.clone()).unwrap_err();
    assert_eq!(error.code, ErrorCode::Timeout);
    // The backend's details stay, next to the source.
    assert_eq!(
        error.details,
        Some(json!({"capture_error": "device busy", "source": "cam"}))
    );
    assert!(matches!(
        camera_health(&health(&mut engine), "cam").state,
        CaptureState::Ended { reason } if reason.starts_with("could not start: camera `cam:1` did not start")
    ));
    shows_no_camera(&engine);
    log.hold("cam:1", false);
    run(&mut engine, "restart_source", busy);
    shows(&engine, 3);
    // A camera has no capture target.
    let error = try_run(
        &mut engine,
        "restart_source",
        json!({"scene_name": "Test pattern", "source_name": "cam", "capture_target_id": "x"}),
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidParams);
    assert!(error.message.contains("camera source"), "{error}");

    run(
        &mut engine,
        "remove_source",
        json!({"scene_name": "Test pattern", "source_name": "cam"}),
    );
    assert_eq!(log.open(), 0);
    assert!(
        !health(&mut engine)
            .captures
            .iter()
            .any(|capture| capture.name == "cam")
    );
    let CommandOutput::PreviewState(preview) =
        run(&mut engine, "set_preview", json!({"enabled": false}))
    else {
        panic!("unexpected output");
    };
    // Every stream that ran, but not the restart that failed: that error
    // was returned already.
    let outcomes = preview.pipeline.expect("pipeline report").scene_captures;
    assert_eq!(outcomes.len(), 3, "{outcomes:?}");
    assert!(
        outcomes.iter().all(|outcome| outcome.report.is_some()),
        "{outcomes:?}"
    );
}

#[test]
fn a_camera_that_is_not_found_shows_nothing_and_warns() {
    let _media = media_test_lock();
    let (mut engine, log) = engine_with_cameras();
    log.plug("cam:1", false);
    // Added while idle: nothing opens until the pipeline starts.
    add_camera(&mut engine, "Test pattern", "cam", "cam:1").expect("add");
    assert!(log.starts().is_empty());
    let CommandOutput::PreviewState(preview) =
        run(&mut engine, "set_preview", json!({"enabled": true}))
    else {
        panic!("unexpected output");
    };
    assert!(
        preview
            .warnings
            .iter()
            .any(|warning| warning.contains("`cam` shows nothing")
                && warning.contains("not found")),
        "{:?}",
        preview.warnings
    );
    let cam = camera_health(&health(&mut engine), "cam");
    assert_eq!(cam.kind, CaptureKind::Camera);
    assert!(
        matches!(&cam.state, CaptureState::Ended { reason } if reason.starts_with("could not start: camera `cam:1` not found")),
        "{cam:?}"
    );
    assert_eq!(cam.format, None);

    // A scene command that starts one says so too.
    let added = add_camera(&mut engine, "Test pattern", "other", "cam:none").expect("add");
    assert!(
        added
            .warnings
            .iter()
            .any(|warning| warning.contains("`other` shows nothing")),
        "{:?}",
        added.warnings
    );
    // Only the colour bars and the overlay are drawn.
    shows_no_camera(&engine);
    let render = health(&mut engine).render.expect("render");
    assert!(
        !render
            .sources
            .iter()
            .any(|source| ["cam", "other"].contains(&source.name.as_str())),
        "{render:?}"
    );

    // Plugged in: restart_source opens it.
    log.plug("cam:1", true);
    let restarted = scenes(run(
        &mut engine,
        "restart_source",
        json!({"scene_name": "Test pattern", "source_name": "cam"}),
    ));
    assert!(restarted.warnings.is_empty(), "{:?}", restarted.warnings);
    assert_eq!(
        camera_health(&health(&mut engine), "cam").state,
        CaptureState::Running
    );
    shows(&engine, 2);
    let CommandOutput::PreviewState(preview) =
        run(&mut engine, "set_preview", json!({"enabled": false}))
    else {
        panic!("unexpected output");
    };
    let pipeline = preview.pipeline.expect("pipeline report");
    // The camera that was not found is reported with why.
    assert!(
        pipeline
            .scene_captures
            .iter()
            .any(|outcome| outcome.name == "other"
                && outcome
                    .error
                    .as_ref()
                    .is_some_and(|e| e.code == ErrorCode::NotFound)),
        "{:?}",
        pipeline.scene_captures
    );
}

#[test]
fn a_busy_camera_fails_the_command_that_starts_it() {
    let _media = media_test_lock();
    let (mut engine, log) = engine_with_cameras();
    add_camera(&mut engine, "Test pattern", "cam", "cam:busy").expect("added while idle");
    let error = try_run(&mut engine, "set_preview", json!({"enabled": true})).unwrap_err();
    assert_eq!(error.code, ErrorCode::Timeout);
    assert!(!health(&mut engine).pipeline_running);

    run(
        &mut engine,
        "remove_source",
        json!({"scene_name": "Test pattern", "source_name": "cam"}),
    );
    run(&mut engine, "set_preview", json!({"enabled": true}));
    let error = add_camera(&mut engine, "Test pattern", "cam", "cam:busy").unwrap_err();
    assert_eq!(error.code, ErrorCode::Timeout);
    // Nothing changed.
    let list = scene_list(&mut engine);
    assert!(
        !list.scenes[0]
            .sources
            .iter()
            .any(|source| source.name == "cam")
    );
    assert_eq!(log.open(), 0);
    run(&mut engine, "set_preview", json!({"enabled": false}));
}

#[test]
fn update_source_switches_the_camera_or_mode_and_undoes_a_failed_switch() {
    let _media = media_test_lock();
    let (mut engine, log) = engine_with_cameras();
    add_camera(&mut engine, "Test pattern", "cam", "cam:1").expect("add");
    let cam = json!({"scene_name": "Test pattern", "source_name": "cam"});
    let with = |extra: serde_json::Value| {
        let mut params = cam.clone();
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        params
    };
    // While idle, only the scene changes.
    update(
        &mut engine,
        with(json!({"mode": {"width": 640, "height": 480}})),
    )
    .expect("idle");
    assert!(log.starts().is_empty());
    assert_eq!(
        source_kind(&mut engine, "Test pattern", "cam"),
        SourceKind::Camera {
            camera_id: "cam:1".to_owned(),
            mode: Some(CameraModeSpec {
                width: Some(640),
                height: Some(480),
                fps: None
            }),
        }
    );
    run(&mut engine, "set_preview", json!({"enabled": true}));
    assert_eq!(log.starts()[0].0.size, Some((640, 480)));
    shows(&engine, 0);

    // Another mode: the camera closes, then opens in it.
    let changed = update(
        &mut engine,
        with(json!({"mode": {"width": 800, "height": 600}})),
    )
    .expect("mode");
    let starts = log.starts();
    assert_eq!(starts.len(), 2);
    assert_eq!(
        (starts[1].0.size, starts[1].1),
        (Some((800, 600)), 0),
        "opened while the old one was open"
    );
    // The fake lacks it: the closest one, with a warning.
    assert!(
        changed
            .warnings
            .iter()
            .any(|warning| warning.contains("camera source `cam`: the camera has no 800x600")),
        "{:?}",
        changed.warnings
    );
    assert_eq!(
        camera_health(&health(&mut engine), "cam")
            .format
            .map(|f| (f.width, f.height)),
        Some((1280, 720))
    );
    shows(&engine, 1);
    // The same camera and mode again, with a move: nothing reopens.
    update(
        &mut engine,
        with(json!({"camera_id": "cam:1", "mode": {"width": 800, "height": 600}, "x": 10})),
    )
    .expect("same");
    assert_eq!(log.starts().len(), 2);
    // A rate is the same however it is written.
    update(
        &mut engine,
        with(json!({"mode": {"width": 800, "height": 600, "fps": {"num": 30, "den": 1}}})),
    )
    .expect("rate");
    assert_eq!(log.starts().len(), 3);
    update(
        &mut engine,
        with(json!({"mode": {"width": 800, "height": 600, "fps": {"num": 60, "den": 2}}})),
    )
    .expect("same rate");
    assert_eq!(log.starts().len(), 3);

    // A busy camera: the switch fails, the source keeps its camera and
    // mode, and that camera opens again.
    let error = update(&mut engine, with(json!({"camera_id": "cam:busy", "x": 20}))).unwrap_err();
    assert_eq!(error.code, ErrorCode::Timeout);
    // It runs in its closest mode again, as before.
    assert!(
        error.message.contains(
            "the camera it had opened again, but: camera source `cam`: the camera has no 800x600"
        ),
        "{error}"
    );
    let starts = log.starts();
    assert_eq!(starts.len(), 5);
    assert_eq!(starts[3].0.camera_id, "cam:busy");
    assert_eq!(
        (starts[4].0.camera_id.as_str(), starts[4].0.size),
        ("cam:1", Some((800, 600)))
    );
    shows(&engine, 4);
    let list = scene_list(&mut engine);
    let source = list.scenes[0]
        .sources
        .iter()
        .find(|source| source.name == "cam")
        .expect("cam");
    assert_eq!(source.rect.x, 10.0);
    assert!(matches!(&source.kind, SourceKind::Camera { camera_id, .. } if camera_id == "cam:1"));
    assert_eq!(
        camera_health(&health(&mut engine), "cam").state,
        CaptureState::Running
    );
    assert_eq!(log.open(), 1);

    // Another camera, and an empty mode lets the engine choose again.
    update(&mut engine, with(json!({"camera_id": "cam:2", "mode": {}}))).expect("switch");
    let (last, _) = log.starts().pop().expect("start");
    assert_eq!((last.camera_id.as_str(), last.size), ("cam:2", None));
    shows(&engine, 5);
    assert_eq!(
        source_kind(&mut engine, "Test pattern", "cam"),
        SourceKind::Camera {
            camera_id: "cam:2".to_owned(),
            mode: None,
        }
    );
    assert_eq!(log.open(), 1);

    // A failed switch whose old camera is gone by now says so.
    log.plug("cam:2", false);
    let error = update(&mut engine, with(json!({"camera_id": "cam:busy"}))).unwrap_err();
    assert_eq!(error.code, ErrorCode::Timeout);
    assert!(
        error
            .message
            .contains("the camera it had opened again, but")
            && error.message.contains("shows nothing"),
        "{error}"
    );
    assert!(matches!(
        camera_health(&health(&mut engine), "cam").state,
        CaptureState::Ended { .. }
    ));
    shows_no_camera(&engine);
    assert_eq!(log.open(), 0);

    // Only camera sources have a camera, and a refused update changes
    // nothing.
    let error = update(
        &mut engine,
        json!({"scene_name": "Test pattern", "source_name": "color_bars", "camera_id": "cam:1",
               "x": 99}),
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidParams);
    let list = scene_list(&mut engine);
    assert_eq!(list.scenes[0].sources[0].rect.x, 0.0);
    run(&mut engine, "set_preview", json!({"enabled": false}));
}

#[test]
fn a_project_keeps_its_cameras() {
    let dir = TempDir::new("camera-project");
    let (engine, log) = engine_with_cameras();
    let mut engine = engine.with_project_dir(&dir.0);
    add_camera(&mut engine, "Test pattern", "cam", "cam:1").expect("add");
    update(
        &mut engine,
        json!({"scene_name": "Test pattern", "source_name": "cam",
               "mode": {"width": 640, "height": 480, "fps": {"num": 30, "den": 1}}}),
    )
    .expect("mode");
    let saved = source_kind(&mut engine, "Test pattern", "cam");
    run(
        &mut engine,
        "save_project",
        json!({"file_name": "show.json"}),
    );
    run(
        &mut engine,
        "remove_source",
        json!({"scene_name": "Test pattern", "source_name": "cam"}),
    );
    run(
        &mut engine,
        "load_project",
        json!({"file_name": "show.json"}),
    );
    assert_eq!(source_kind(&mut engine, "Test pattern", "cam"), saved);
    // Loading opens no camera; the pipeline does.
    assert!(log.starts().is_empty());
}
