//! Windows.Graphics.Capture on the machine it runs on (the Windows CI
//! runner included). The runner has a desktop session, so a monitor is
//! expected to capture; if the API is unavailable there, the backend must
//! say so with a structured error, which the tests print.
#![cfg(target_os = "windows")]

use std::time::{Duration, Instant};

use capture_api::{
    CaptureBackend, CaptureErrorKind, CursorMode, ScreenCaptureRequest, ScreenSelection,
    VideoSourceKind,
};
use capture_windows::WgcCapture;

fn selection(source: Option<&str>, cursor: CursorMode) -> ScreenSelection {
    ScreenSelection {
        source_kinds: vec![VideoSourceKind::Monitor, VideoSourceKind::Window],
        cursor,
        source: source.map(str::to_owned),
    }
}

#[test]
fn capabilities_describe_named_sources_without_a_picker() {
    let capabilities = WgcCapture::new().capabilities();
    println!("{capabilities:#?}");
    assert_eq!(capabilities.platform, "windows");
    let screen = &capabilities.screen;
    assert_eq!(screen.available, screen.unavailable_reason.is_none());
    if screen.available {
        assert!(screen.source_list && !screen.picker);
        assert!(screen.source_kinds.contains(&VideoSourceKind::Monitor));
    }
    assert!(!capabilities.audio.available, "audio comes from cpal");
}

#[test]
fn a_listed_monitor_delivers_frames_of_its_size() {
    let mut capture = WgcCapture::new();
    let sources = match capture.screen_sources() {
        Ok(sources) => sources,
        Err(error) => {
            println!("no source list: {error:?}");
            assert!(!capture.capabilities().screen.available, "{error:?}");
            return;
        }
    };
    println!("{sources:#?}");
    let monitor = sources
        .iter()
        .find(|source| source.kind == VideoSourceKind::Monitor)
        .expect("a desktop session has a monitor");
    let stream = capture
        .start_screen_stream(&selection(Some(&monitor.id), CursorMode::Embedded))
        .expect("capture the monitor");
    let mailbox = stream.frames();
    let deadline = Instant::now() + Duration::from_secs(5);
    let frame = loop {
        if let Some(frame) = mailbox.take() {
            break frame;
        }
        assert!(Instant::now() < deadline, "no frame within 5 s");
        std::thread::sleep(Duration::from_millis(10));
    };
    println!("frame {}x{}", frame.width(), frame.height());
    assert!(frame.width() > 0 && frame.height() > 0);
    if let (Some(width), Some(height)) = (monitor.width, monitor.height) {
        assert_eq!((frame.width(), frame.height()), (width, height));
    }
    let report = stream.stop().expect("stop");
    println!("{report:#?}");
    assert!(report.stats.frames >= 1, "{report:#?}");
    let format = report.format.expect("format");
    assert_eq!(
        (format.width, format.height),
        (frame.width(), frame.height())
    );
}

#[test]
fn a_monitor_probe_reports_frames() {
    let mut capture = WgcCapture::new();
    let Ok(sources) = capture.screen_sources() else {
        return;
    };
    let Some(monitor) = sources
        .iter()
        .find(|source| source.kind == VideoSourceKind::Monitor)
    else {
        panic!("no monitor in {sources:#?}");
    };
    let report = capture
        .probe_screen(&ScreenCaptureRequest {
            selection: selection(Some(&monitor.id), CursorMode::Hidden),
            duration: Duration::from_millis(500),
        })
        .expect("probe");
    println!("{report:#?}");
    assert!(report.stats.frames >= 1, "{report:#?}");
    assert_eq!(report.source.kind, Some(VideoSourceKind::Monitor));
}

#[test]
fn bad_selections_are_structured_errors() {
    let mut capture = WgcCapture::new();
    let screen = capture.capabilities().screen;
    if !screen.available {
        return;
    }
    let mut error = |selection: ScreenSelection| {
        capture
            .start_screen_stream(&selection)
            .err()
            .expect("must fail")
    };
    let picker = error(selection(None, CursorMode::Embedded));
    assert_eq!(picker.kind, CaptureErrorKind::InvalidRequest, "{picker:?}");
    for id in ["monitor:no-such-display", "window:0x1", "printer:1"] {
        let missing = error(selection(Some(id), CursorMode::Embedded));
        assert_eq!(
            missing.kind,
            CaptureErrorKind::NotFound,
            "{id}: {missing:?}"
        );
    }
    // Without cursor selection every mode is accepted (and reported ignored).
    if !screen.cursor_modes.is_empty() {
        let metadata = error(selection(Some("monitor:x"), CursorMode::Metadata));
        assert_eq!(metadata.kind, CaptureErrorKind::Unsupported, "{metadata:?}");
    }
}
