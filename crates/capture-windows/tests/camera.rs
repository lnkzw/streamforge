//! Media Foundation cameras on the machine the tests run on. CI runners
//! have no camera (and Windows Server may lack Media Foundation), so there
//! the tests check the structured errors. With a camera they capture it,
//! which is the manual hardware test (`docs/testing.md`); a camera another
//! application holds fails the capture tests.
#![cfg(target_os = "windows")]

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use capture_api::{Camera, CameraCapture, CameraRequest, CaptureErrorKind, CaptureState};
use capture_windows::MfCamera;

static DEVICE_TEST: Mutex<()> = Mutex::new(());

fn request(camera_id: &str, size: Option<(u32, u32)>) -> CameraRequest {
    CameraRequest {
        camera_id: camera_id.to_owned(),
        size,
        fps: None,
    }
}

/// The listed cameras, or `None` (printed) where Media Foundation is
/// unavailable, which the capability must then say.
fn cameras(capture: &mut MfCamera) -> Option<Vec<Camera>> {
    match capture.cameras() {
        Ok(cameras) => {
            println!("{cameras:#?}");
            Some(cameras)
        }
        Err(error) => {
            println!("no camera list: {error:?}");
            assert!(!capture.capability().available, "{error:?}");
            None
        }
    }
}

#[test]
fn the_camera_capability_is_available_or_says_why() {
    let _device = DEVICE_TEST.lock().unwrap();
    let camera = MfCamera::new().capability();
    println!("{camera:#?}");
    assert_eq!(camera.backend.as_deref(), Some("Media Foundation"));
    assert_eq!(camera.available, camera.unavailable_reason.is_none());
}

#[test]
fn listed_cameras_have_ids_names_and_unique_modes() {
    let _device = DEVICE_TEST.lock().unwrap();
    let mut capture = MfCamera::new();
    let Some(cameras) = cameras(&mut capture) else {
        return;
    };
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
    // The second listing reads the modes from the cache; they stay.
    assert_eq!(capture.cameras().expect("second listing"), cameras);
}

#[test]
fn an_unknown_camera_is_not_found() {
    let _device = DEVICE_TEST.lock().unwrap();
    let mut capture = MfCamera::new();
    let available = capture.capability().available;
    let error = capture
        .start_camera_stream(&request(
            r"\\?\usb#vid_0000&pid_0000#streamforge-test#{e5323777-f976-4f5b-9b55-b94699c46e44}\global",
            None,
        ))
        .err()
        .expect("an unknown camera must not start");
    println!("{error:?}");
    let expected = if available {
        CaptureErrorKind::NotFound
    } else {
        CaptureErrorKind::Unavailable
    };
    assert_eq!(error.kind, expected, "{error:?}");
}

/// Start `request`, check three frames against the reported format and
/// stop; returns the frame size.
fn capture_once(capture: &mut MfCamera, request: &CameraRequest) -> (u32, u32) {
    let started = capture
        .start_camera_stream(request)
        .unwrap_or_else(|error| panic!("start {request:?}: {error:?}"));
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
    size
}

#[test]
fn each_listed_camera_delivers_frames_in_its_chosen_mode() {
    let _device = DEVICE_TEST.lock().unwrap();
    let mut capture = MfCamera::new();
    let Some(cameras) = cameras(&mut capture) else {
        return;
    };
    if cameras.is_empty() {
        println!("no camera on this machine");
        return;
    }
    for camera in &cameras {
        assert!(
            !camera.modes.is_empty(),
            "camera {} has no modes; it may be in use or camera privacy may be off",
            camera.name
        );
        println!("camera {} ({})", camera.name, camera.camera_id);
        capture_once(&mut capture, &request(&camera.camera_id, None));
        // Drop must release the device before the next start too.
        drop(
            capture
                .start_camera_stream(&request(&camera.camera_id, None))
                .expect("start before drop")
                .stream,
        );
        // Again in its smallest mode: the camera was released, and a mode
        // other than the default one is selected.
        let smallest = camera.modes.last().expect("modes");
        let size = capture_once(
            &mut capture,
            &request(&camera.camera_id, Some((smallest.width, smallest.height))),
        );
        assert_eq!(size, (smallest.width, smallest.height));
    }
}
