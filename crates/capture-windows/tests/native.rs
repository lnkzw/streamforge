//! Windows.Graphics.Capture on the machine it runs on (the Windows CI
//! runner included). The runner has a desktop session, so a monitor is
//! expected to capture; if the API is unavailable there, the backend must
//! say so with a structured error, which the tests print.
#![cfg(target_os = "windows")]

use std::process::{Child, Command};
use std::time::{Duration, Instant};

use capture_api::{
    CaptureBackend, CaptureErrorKind, CaptureState, CursorMode, ScreenCaptureRequest,
    ScreenSelection, VideoSourceKind,
};
use capture_windows::WgcCapture;

fn selection(source: Option<&str>, cursor_mode: CursorMode) -> ScreenSelection {
    ScreenSelection {
        target_kinds: vec![VideoSourceKind::Monitor, VideoSourceKind::Window],
        cursor_mode,
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
        assert!(screen.target_list && !screen.picker);
        assert!(screen.target_kinds.contains(&VideoSourceKind::Monitor));
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
    assert_eq!(stream.state(), CaptureState::Running);
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
    let sources = capture.screen_sources().expect("sources");
    let mut error = |selection: ScreenSelection| {
        capture
            .start_screen_stream(&selection)
            .err()
            .expect("must fail")
    };
    let picker = error(selection(None, CursorMode::Embedded));
    assert_eq!(picker.kind, CaptureErrorKind::InvalidRequest, "{picker:?}");
    for id in ["monitor:no-such-display", "window:1:0x1", "printer:1"] {
        let missing = error(selection(Some(id), CursorMode::Embedded));
        assert_eq!(
            missing.kind,
            CaptureErrorKind::NotFound,
            "{id}: {missing:?}"
        );
    }
    // A listed window under another process id: a reused handle must not
    // resolve to a different window.
    if let Some(window) = sources
        .iter()
        .find(|source| source.kind == VideoSourceKind::Window)
    {
        let (process, handle) = window.id["window:".len()..]
            .split_once(':')
            .expect("window id");
        let process: u32 = process.parse().expect("process id");
        let other = format!("window:{}:{handle}", process.wrapping_add(1));
        let reused = error(selection(Some(&other), CursorMode::Embedded));
        assert_eq!(
            reused.kind,
            CaptureErrorKind::NotFound,
            "{other}: {reused:?}"
        );
    }
    // Without cursor_mode selection every mode is accepted (and reported ignored).
    if !screen.cursor_modes.is_empty() {
        let metadata = error(selection(Some("monitor:x"), CursorMode::Metadata));
        assert_eq!(metadata.kind, CaptureErrorKind::Unsupported, "{metadata:?}");
    }
}

/// Kills the process when the test ends, also when it fails.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Closing the captured window ends the stream with a reason: a PowerShell
/// form with a unique title is captured, then its process is killed.
#[test]
fn a_closed_window_ends_the_stream() {
    let mut capture = WgcCapture::new();
    if !capture.capabilities().screen.available {
        return;
    }
    let title = format!("streamforge-test-window-{}", std::process::id());
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms; \
         $form = New-Object System.Windows.Forms.Form; \
         $form.Text = '{title}'; \
         [void]$form.ShowDialog()"
    );
    let mut form = KillOnDrop(
        Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .spawn()
            .expect("start PowerShell"),
    );
    // PowerShell and Windows Forms can take tens of seconds to start on a
    // loaded runner.
    let deadline = Instant::now() + Duration::from_secs(60);
    let window = loop {
        let sources = capture.screen_sources().expect("sources");
        let windows = sources
            .iter()
            .filter(|source| source.kind == VideoSourceKind::Window)
            .count();
        if let Some(window) = sources
            .into_iter()
            .find(|source| source.kind == VideoSourceKind::Window && source.name == title)
        {
            break window;
        }
        if let Some(status) = form.0.try_wait().expect("PowerShell status") {
            panic!("PowerShell exited with {status} before its window was listed");
        }
        assert!(
            Instant::now() < deadline,
            "the test window was not listed within 60 s ({windows} other windows were)"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    let stream = capture
        .start_screen_stream(&selection(Some(&window.id), CursorMode::Hidden))
        .expect("capture the test window");
    assert_eq!(stream.state(), CaptureState::Running);

    form.0.kill().expect("close the test window");
    let _ = form.0.wait();
    let deadline = Instant::now() + Duration::from_secs(10);
    let state = loop {
        let state = stream.state();
        if state != CaptureState::Running || Instant::now() >= deadline {
            break state;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    println!("{state:?}");
    // Stopping an ended capture may report an error; it is not the subject.
    println!("{:#?}", stream.stop());
    assert!(matches!(state, CaptureState::Ended { .. }), "{state:?}");
}
