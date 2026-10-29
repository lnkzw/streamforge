//! AVFoundation cameras on the machine the tests run on. CI runners have no
//! camera, so there the tests check the listing and the structured errors.
//! With a camera they capture it, which is the manual hardware test
//! (`docs/testing.md`); without camera access for the app that runs the
//! tests (the terminal), the capture tests check `permission_denied`
//! instead. The first run asks for that access.
#![cfg(target_os = "macos")]

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use capture_api::{CameraCapture, CameraRequest, CaptureErrorKind, CaptureState, VideoFormatInfo};
use capture_macos::AvfCamera;

static DEVICE_TEST: Mutex<()> = Mutex::new(());

fn request(camera_id: &str, size: Option<(u32, u32)>) -> CameraRequest {
    CameraRequest {
        camera_id: camera_id.to_owned(),
        size,
        fps: None,
    }
}

/// Capture `request` (with a rate) for two seconds; the measured rate must
/// follow the requested one, so the frame duration was really set. `None`
/// without camera access.
fn capture_at_rate(capture: &mut AvfCamera, request: &CameraRequest) -> Option<f64> {
    let fps = request.fps.expect("a rate");
    let started = match capture.start_camera_stream(request) {
        Ok(started) => started,
        Err(error) if error.kind == CaptureErrorKind::PermissionDenied => return None,
        Err(error) => panic!("start {request:?}: {error:?}"),
    };
    println!(
        "format {:?}, warnings {:?}",
        started.format, started.warnings
    );
    assert_eq!(started.format.as_ref().map(|f| f.framerate), Some(fps));
    let mailbox = started.stream.frames();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        // Keep the pool free, as the compositor would.
        let _ = mailbox.take();
        std::thread::sleep(Duration::from_millis(5));
    }
    let report = started.stream.stop().expect("stop");
    let measured = report.stats.measured_fps.expect("a measured rate");
    let wanted = f64::from(fps.num) / f64::from(fps.den);
    println!("requested {wanted} fps, measured {measured:.2}");
    assert!(
        measured < wanted * 1.25 && measured > wanted * 0.5,
        "requested {wanted} fps, measured {measured:.2}: {report:#?}"
    );
    Some(measured)
}

#[test]
fn the_camera_capability_is_available_or_says_why() {
    let _device = DEVICE_TEST.lock().unwrap();
    let camera = AvfCamera::new().capability();
    println!("{camera:#?}");
    assert_eq!(camera.backend.as_deref(), Some("AVFoundation"));
    assert_eq!(camera.available, camera.unavailable_reason.is_none());
    if let Some(reason) = &camera.unavailable_reason {
        assert!(reason.contains("Privacy & Security") || reason.contains("restricted"));
    }
}

#[test]
fn listed_cameras_have_ids_names_and_unique_modes() {
    let _device = DEVICE_TEST.lock().unwrap();
    let mut capture = AvfCamera::new();
    let cameras = capture.cameras().expect("listing needs no camera access");
    println!("{cameras:#?}");
    let ids: HashSet<&str> = cameras.iter().map(|c| c.camera_id.as_str()).collect();
    assert_eq!(ids.len(), cameras.len(), "ids repeat");
    for camera in &cameras {
        assert!(
            !camera.camera_id.is_empty() && !camera.name.is_empty(),
            "{camera:?}"
        );
        let modes: HashSet<_> = camera
            .modes
            .iter()
            .map(|m| (m.width, m.height, m.fps.num, m.fps.den))
            .collect();
        assert_eq!(modes.len(), camera.modes.len(), "{camera:?}");
        let area = |i: usize| u64::from(camera.modes[i].width) * u64::from(camera.modes[i].height);
        assert!(
            (1..camera.modes.len()).all(|i| area(i - 1) >= area(i)),
            "largest first: {camera:?}"
        );
    }
}

#[test]
fn an_unknown_camera_is_not_found() {
    let _device = DEVICE_TEST.lock().unwrap();
    let error = AvfCamera::new()
        .start_camera_stream(&request("0x0000000000000000-streamforge-test", None))
        .err()
        .expect("an unknown camera must not start");
    println!("{error:?}");
    // Not found comes before camera access, so this never asks.
    assert_eq!(error.kind, CaptureErrorKind::NotFound, "{error:?}");
    assert!(error.message.contains("list_cameras"), "{error:?}");
}

/// Start `request`, check three frames against the reported format and
/// stop; returns the format, or `None` without camera access.
fn capture_once(capture: &mut AvfCamera, request: &CameraRequest) -> Option<VideoFormatInfo> {
    let started = match capture.start_camera_stream(request) {
        Ok(started) => started,
        Err(error) if error.kind == CaptureErrorKind::PermissionDenied => {
            println!("no camera access: {error:?}");
            assert!(
                error.message.contains("Privacy & Security")
                    || error.message.contains("asking")
                    || error.message.contains("restricted"),
                "{error:?}"
            );
            return None;
        }
        Err(error) => panic!("start {request:?}: {error:?}"),
    };
    println!(
        "format {:?}, warnings {:?}",
        started.format, started.warnings
    );
    let format = started.format.clone().expect("a camera reports its format");
    let stream = started.stream;
    let mailbox = stream.frames();
    let deadline = Instant::now() + Duration::from_secs(5);
    let size = (format.width, format.height);
    let mut previous_pts = None;
    for _ in 0..3 {
        let frame = loop {
            if let Some(frame) = mailbox.take() {
                break frame;
            }
            assert!(Instant::now() < deadline, "fewer than 3 frames within 5 s");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!((frame.width(), frame.height()), size);
        let pts = frame.pts().expect("camera frame PTS");
        if let Some(previous) = previous_pts {
            assert!(pts.ticks > previous, "camera PTS must increase");
        }
        previous_pts = Some(pts.ticks);
    }
    println!("frame {}x{}", size.0, size.1);
    assert_eq!(stream.state(), CaptureState::Running);
    let report = stream.stop().expect("stop");
    println!("{report:#?}");
    assert!(report.stats.frames >= 3, "{report:#?}");
    assert_eq!(report.stats.frames_discarded, 0, "{report:#?}");
    assert!(
        report
            .warnings
            .iter()
            .all(|warning| !warning.contains("frame not delivered")),
        "{report:#?}"
    );
    assert_eq!(report.format.as_ref(), Some(&format));
    Some(format)
}

#[test]
fn each_listed_camera_delivers_frames_in_its_chosen_mode() {
    let _device = DEVICE_TEST.lock().unwrap();
    let mut capture = AvfCamera::new();
    let cameras = capture.cameras().expect("listing");
    if cameras.is_empty() {
        println!("no camera on this machine");
        return;
    }
    for camera in &cameras {
        println!("camera {} ({})", camera.name, camera.camera_id);
        if camera.modes.is_empty() {
            // Only formats StreamForge does not read (docs/platforms.md).
            let error = capture
                .start_camera_stream(&request(&camera.camera_id, None))
                .err()
                .expect("a camera without modes must not start");
            println!("no mode StreamForge reads: {error:?}");
            assert!(
                matches!(
                    error.kind,
                    CaptureErrorKind::Unsupported | CaptureErrorKind::PermissionDenied
                ),
                "{error:?}"
            );
            continue;
        }
        let Some(default) = capture_once(&mut capture, &request(&camera.camera_id, None)) else {
            return;
        };
        // Drop must stop the session before the next start too.
        drop(
            capture
                .start_camera_stream(&request(&camera.camera_id, None))
                .expect("start before drop")
                .stream,
        );
        // Again in its smallest mode: a mode other than the default one is
        // selected.
        let smallest = camera.modes.last().expect("modes");
        let size = capture_once(
            &mut capture,
            &request(&camera.camera_id, Some((smallest.width, smallest.height))),
        )
        .map(|format| (format.width, format.height));
        assert_eq!(size, Some((smallest.width, smallest.height)));
        // A slower rate of the default size, where the camera lists one.
        let slower = camera.modes.iter().find(|mode| {
            (mode.width, mode.height) == (default.width, default.height)
                && mode.fps.num > 0
                && u64::from(mode.fps.num) * u64::from(default.framerate.den)
                    < u64::from(default.framerate.num) * u64::from(mode.fps.den)
        });
        if let Some(slower) = slower {
            let mut at_rate = request(&camera.camera_id, Some((slower.width, slower.height)));
            at_rate.fps = Some(slower.fps);
            capture_at_rate(&mut capture, &at_rate);
        } else {
            println!(
                "camera {} lists no slower rate at its default size",
                camera.name
            );
        }
    }
}
