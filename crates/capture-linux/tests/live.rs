//! Live tests against the running desktop session.
//!
//! Ignored by default because they need PipeWire and xdg-desktop-portal, and
//! the audio probe opens a real input device. Run with:
//!
//! ```text
//! cargo test -p capture-linux --test live -- --ignored --test-threads=1
//! ```
//!
//! Screen capture needs a human to pick a source in the portal dialog, so it
//! is covered by the manual procedure in `docs/capture-linux.md` instead.
#![cfg(target_os = "linux")]

use std::time::Duration;

use capture_api::{AudioCaptureRequest, CaptureBackend};
use capture_linux::LinuxCapture;

fn session_available() -> bool {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR");
    let available = runtime_dir
        .as_ref()
        .is_some_and(|dir| std::path::Path::new(dir).join("pipewire-0").exists());
    if !available {
        eprintln!("skipping: no PipeWire socket in $XDG_RUNTIME_DIR (no desktop session)");
    }
    available
}

#[test]
#[ignore = "needs a desktop session with PipeWire and xdg-desktop-portal"]
fn capabilities_report_running_services() {
    if !session_available() {
        return;
    }
    let caps = LinuxCapture::new().capabilities();
    assert_eq!(caps.platform, "linux");
    assert!(caps.audio.available, "{:?}", caps.audio.unavailable_reason);
    assert!(caps.audio.server_version.is_some());
    assert!(
        caps.screen.available,
        "{:?}",
        caps.screen.unavailable_reason
    );
    assert!(!caps.screen.source_kinds.is_empty());
}

#[test]
#[ignore = "needs a running PipeWire daemon"]
fn audio_inputs_are_listed_with_names() {
    if !session_available() {
        return;
    }
    let inputs = LinuxCapture::new().audio_inputs().expect("list inputs");
    for input in &inputs {
        assert!(!input.name.is_empty());
        assert!(input.media_class.starts_with("Audio/"), "{input:?}");
    }
    eprintln!("found {} audio input(s)", inputs.len());
}

#[test]
#[ignore = "opens a real audio input for 0.5 s (statistics only, nothing is stored)"]
fn audio_probe_reports_format_and_positions() {
    if !session_available() {
        return;
    }
    let mut backend = LinuxCapture::new();
    let Some(input) = backend
        .audio_inputs()
        .expect("list inputs")
        .into_iter()
        .next()
    else {
        eprintln!("skipping: no audio input device present");
        return;
    };
    let report = backend
        .probe_audio(&AudioCaptureRequest {
            input: Some(input.name.clone()),
            duration: Duration::from_millis(500),
        })
        .expect("probe");
    let format = report.format.expect("negotiated format");
    assert_eq!(format.sample_format, "f32le");
    assert!(format.rate > 0 && format.channels > 0);
    assert!(report.stats.frames > 0, "{:?}", report.stats);
    let first = report.stats.first_position.expect("first position");
    let last = report.stats.last_position.expect("last position");
    assert!(last.ticks >= first.ticks);
    assert_eq!(report.stats.buffer_starvation, 0);
}

#[test]
#[ignore = "needs a running PipeWire daemon"]
fn unknown_audio_input_is_not_found() {
    if !session_available() {
        return;
    }
    let err = LinuxCapture::new()
        .probe_audio(&AudioCaptureRequest {
            input: Some("streamforge.test.does-not-exist".into()),
            duration: Duration::from_millis(100),
        })
        .unwrap_err();
    assert_eq!(err.kind, capture_api::CaptureErrorKind::NotFound);
    assert!(err.details.is_some());
}
