//! The system picker (`SCContentSharingPicker`) as the programs show it:
//! from the work of `run_with_main_run_loop`, while the main thread runs
//! the main run loop. The default test harness runs tests off the main
//! thread with no run loop, so this test has its own `main`
//! (`harness = false`). Nobody picks on a CI runner, so every wait ends
//! after a short timeout, or the picker reports that it cannot be shown
//! there; a watchdog fails the test instead of hanging. That the picker
//! appears, and what a pick captures, needs a human (hardware round).

#[cfg(target_os = "macos")]
fn main() {
    use std::process;
    use std::thread;
    use std::time::{Duration, Instant};

    use capture_api::{
        CaptureErrorKind, CursorMode, ScreenCapture, ScreenCaptureRequest, ScreenSelection,
        VideoSourceKind,
    };
    use capture_macos::{SckCapture, run_with_main_run_loop};

    const PICK: Duration = Duration::from_secs(3);
    const DEADLINE: Duration = Duration::from_secs(60);

    thread::spawn(|| {
        thread::sleep(DEADLINE);
        eprintln!("picker: timed out (a pick did not end?)");
        process::exit(1);
    });
    // Asked off the main thread, where a pick could wait: only while a
    // main run loop runs.
    let offered = || {
        thread::spawn(|| SckCapture::new().capability().picker)
            .join()
            .expect("capability")
    };
    assert!(!offered(), "before the main run loop");
    assert!(!SckCapture::new().capability().picker, "on the main thread");
    run_with_main_run_loop(|| {
        let mut capture = SckCapture::new().with_pick_timeout(PICK);
        let screen = capture.capability();
        println!("{screen:#?}");
        assert!(screen.picker && screen.target_list, "{screen:?}");
        let selection = |target_kinds| ScreenSelection {
            target_kinds,
            cursor_mode: CursorMode::Embedded,
            capture_target_id: None,
        };
        let ends = |what: &str, started: Instant, error: Option<capture_api::CaptureError>| {
            let error = error.unwrap_or_else(|| panic!("{what}: nobody picks, yet it started"));
            println!("{what}: {error:?} after {:?}", started.elapsed());
            assert!(
                matches!(
                    error.kind,
                    CaptureErrorKind::Timeout | CaptureErrorKind::Unavailable
                ),
                "{what}: {error:?}"
            );
            let elapsed = started.elapsed();
            assert!(elapsed < PICK + Duration::from_secs(10), "{what}");
            if error.kind == CaptureErrorKind::Timeout {
                assert!(elapsed >= PICK, "{what}: timed out early");
            }
        };
        // Twice in a row, so the first pick leaves nothing behind that
        // keeps the second from showing or ending; then a probe, for one
        // kind only.
        for attempt in ["first pick", "second pick"] {
            let started = Instant::now();
            let error = capture
                .start_screen_stream(&selection(vec![
                    VideoSourceKind::Monitor,
                    VideoSourceKind::Window,
                ]))
                .err();
            ends(attempt, started, error);
        }
        let started = Instant::now();
        let error = capture
            .probe_screen(&ScreenCaptureRequest {
                selection: selection(vec![VideoSourceKind::Window]),
                duration: Duration::from_millis(200),
            })
            .err();
        ends("probe", started, error);
    });
    assert!(!offered(), "after the main run loop");
    println!("picker: ok");
}

#[cfg(not(target_os = "macos"))]
fn main() {}
