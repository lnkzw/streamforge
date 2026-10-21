//! ScreenCaptureKit on the machine it runs on (the macOS CI runner
//! included). Screen recording needs a permission that a CI runner may not
//! have; without it, every query must fail with `permission_denied` and the
//! remedy, which the tests check and print.
#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use capture_api::{
    CaptureBackend, CaptureErrorKind, CaptureState, CursorMode, ScreenCaptureRequest,
    ScreenSelection, ScreenSource, VideoSourceKind,
};
use capture_macos::SckCapture;

fn selection(source: Option<&str>, cursor: CursorMode) -> ScreenSelection {
    ScreenSelection {
        source_kinds: vec![VideoSourceKind::Monitor, VideoSourceKind::Window],
        cursor,
        source: source.map(str::to_owned),
    }
}

/// The listed sources, or `None` (printed) where screen recording is not
/// permitted.
fn sources(capture: &mut SckCapture) -> Option<Vec<ScreenSource>> {
    match capture.screen_sources() {
        Ok(sources) => {
            println!("{sources:#?}");
            Some(sources)
        }
        Err(error) => {
            println!("no source list: {error:?}");
            assert_eq!(error.kind, CaptureErrorKind::PermissionDenied, "{error:?}");
            assert!(error.message.contains("System Settings"), "{error:?}");
            None
        }
    }
}

#[test]
fn capabilities_describe_named_sources_without_a_picker() {
    let capabilities = SckCapture::new().capabilities();
    println!("{capabilities:#?}");
    assert_eq!(capabilities.platform, "macos");
    let screen = &capabilities.screen;
    assert!(screen.available && screen.source_list && !screen.picker);
    assert!(!capabilities.audio.available, "audio comes from cpal");
}

#[test]
fn a_listed_display_delivers_frames_or_permission_is_denied() {
    let mut capture = SckCapture::new();
    let Some(sources) = sources(&mut capture) else {
        return;
    };
    let display = sources
        .iter()
        .find(|source| source.kind == VideoSourceKind::Monitor)
        .expect("a display");
    let stream =
        match capture.start_screen_stream(&selection(Some(&display.id), CursorMode::Embedded)) {
            Ok(stream) => stream,
            Err(error) => {
                // Listing can work while capturing is still refused.
                assert_eq!(error.kind, CaptureErrorKind::PermissionDenied, "{error:?}");
                return;
            }
        };
    let mailbox = stream.frames();
    let deadline = Instant::now() + Duration::from_secs(5);
    let frame = loop {
        if let Some(frame) = mailbox.take() {
            break Some(frame);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let state = stream.state();
    let report = stream.stop().expect("stop");
    println!("{report:#?}");
    let frame = frame.expect("no frame within 5 s");
    assert_eq!(state, CaptureState::Running);
    println!("frame {}x{}", frame.width(), frame.height());
    assert!(report.stats.frames >= 1, "{report:#?}");
    let format = report.format.expect("format");
    assert_eq!(
        (format.width, format.height),
        (frame.width(), frame.height())
    );
}

#[test]
fn a_display_probe_reports_frames_or_permission_is_denied() {
    let mut capture = SckCapture::new();
    let Some(sources) = sources(&mut capture) else {
        return;
    };
    let Some(display) = sources
        .iter()
        .find(|source| source.kind == VideoSourceKind::Monitor)
    else {
        panic!("no display in {sources:#?}");
    };
    match capture.probe_screen(&ScreenCaptureRequest {
        selection: selection(Some(&display.id), CursorMode::Hidden),
        duration: Duration::from_millis(500),
    }) {
        Ok(report) => {
            println!("{report:#?}");
            assert_eq!(report.source.kind, Some(VideoSourceKind::Monitor));
        }
        Err(error) => assert_eq!(error.kind, CaptureErrorKind::PermissionDenied, "{error:?}"),
    }
}

#[test]
fn bad_selections_are_structured_errors() {
    let mut capture = SckCapture::new();
    let picker = capture
        .start_screen_stream(&selection(None, CursorMode::Embedded))
        .err()
        .expect("must fail");
    assert_eq!(picker.kind, CaptureErrorKind::InvalidRequest, "{picker:?}");
    let metadata = capture
        .start_screen_stream(&selection(Some("display:1"), CursorMode::Metadata))
        .err()
        .expect("must fail");
    assert_eq!(metadata.kind, CaptureErrorKind::Unsupported, "{metadata:?}");
    let malformed = capture
        .start_screen_stream(&selection(Some("printer:1"), CursorMode::Embedded))
        .err()
        .expect("must fail");
    assert_eq!(malformed.kind, CaptureErrorKind::NotFound, "{malformed:?}");
    if sources(&mut capture).is_some() {
        let missing = capture
            .start_screen_stream(&selection(Some("window:4294967295"), CursorMode::Embedded))
            .err()
            .expect("must fail");
        assert_eq!(missing.kind, CaptureErrorKind::NotFound, "{missing:?}");
    }
}
