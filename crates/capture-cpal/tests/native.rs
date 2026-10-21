//! The cpal backend on the machine it runs on (Windows and macOS CI runners
//! included). Runners usually have no microphone, so each test checks the
//! outcome for both cases: with an input, audio must arrive; without one,
//! the backend must say so with a structured error.
#![cfg(any(target_os = "windows", target_os = "macos"))]

use std::time::Duration;

use capture_api::{AudioCaptureRequest, AudioStreamRequest, CaptureBackend, CaptureErrorKind};
use capture_cpal::CpalCapture;

#[test]
fn capabilities_report_the_platform_and_what_is_missing() {
    let capabilities = CpalCapture::new().capabilities();
    assert_eq!(capabilities.platform, std::env::consts::OS);
    assert!(!capabilities.screen.available);
    assert!(
        capabilities
            .screen
            .unavailable_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("not part of the cpal backend"))
    );
    let backend = capabilities.audio.backend.as_deref().unwrap_or_default();
    assert!(backend.starts_with("cpal ("), "{backend}");
    assert_eq!(
        capabilities.audio.available,
        capabilities.audio.unavailable_reason.is_none()
    );
    println!("{capabilities:#?}");
}

#[test]
fn the_default_input_captures_or_reports_that_there_is_none() {
    let mut capture = CpalCapture::new();
    let inputs = capture.audio_inputs().expect("list inputs");
    println!("inputs: {inputs:#?}");
    match capture.probe_audio(&AudioCaptureRequest {
        input: None,
        duration: Duration::from_millis(500),
    }) {
        Ok(report) => {
            println!("{report:#?}");
            let format = report.format.as_ref().expect("format");
            assert!(format.rate > 0 && format.channels > 0);
            assert!(report.stats.frames > 0, "no audio arrived: {report:#?}");
        }
        Err(error) => {
            println!("{error:?}");
            assert!(
                matches!(
                    error.kind,
                    CaptureErrorKind::NotFound
                        | CaptureErrorKind::Unavailable
                        | CaptureErrorKind::PermissionDenied
                ),
                "{error:?}"
            );
            // Inputs without a default one are possible; then the error
            // says so instead of claiming there is no input at all.
            assert!(
                inputs.is_empty()
                    || error.kind != CaptureErrorKind::NotFound
                    || error.message.contains("no default audio input"),
                "{error:?}"
            );
        }
    }
}

#[test]
fn unknown_inputs_and_unsupported_layouts_are_errors() {
    let mut capture = CpalCapture::new();
    let missing = capture
        .start_audio_stream(&AudioStreamRequest {
            input: Some("no-such-input-device".to_owned()),
            rate: 48_000,
            channels: 2,
        })
        .err()
        .expect("must fail");
    assert_eq!(missing.kind, CaptureErrorKind::NotFound);
    let mono = capture
        .start_audio_stream(&AudioStreamRequest {
            input: None,
            rate: 48_000,
            channels: 1,
        })
        .err()
        .expect("must fail");
    assert_eq!(mono.kind, CaptureErrorKind::InvalidRequest);
    let no_rate = capture
        .start_audio_stream(&AudioStreamRequest {
            input: None,
            rate: 0,
            channels: 2,
        })
        .err()
        .expect("must fail");
    assert_eq!(no_rate.kind, CaptureErrorKind::InvalidRequest);
}
