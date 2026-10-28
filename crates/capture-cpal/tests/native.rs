//! The cpal backend on the machine it runs on (Windows and macOS CI runners
//! included). Runners usually have no microphone or speakers, so each test
//! checks the outcome for both cases: with a device, audio must arrive (or
//! be pulled); without one, the backend must say so with a structured
//! error.
#![cfg(any(target_os = "windows", target_os = "macos"))]

use std::f32::consts::TAU;
use std::thread;
use std::time::{Duration, Instant};

use capture_api::{
    AudioCaptureRequest, AudioStreamRequest, CaptureBackend, CaptureErrorKind, CaptureState,
    PlaybackRequest,
};
use capture_cpal::CpalCapture;
use media_core::sample_ring;

fn playback(output: Option<&str>, channels: u32) -> PlaybackRequest {
    PlaybackRequest {
        output: output.map(str::to_owned),
        rate: 48_000,
        channels,
    }
}

fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    done()
}

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
            assert!(format.sample_rate_hz > 0 && format.channels > 0);
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
fn outputs_are_listed_as_monitor_inputs() {
    let mut capture = CpalCapture::new();
    let inputs = capture.audio_inputs().expect("list inputs");
    let outputs = capture.audio_outputs().expect("list outputs");
    let monitors: Vec<_> = inputs
        .iter()
        .filter(|input| input.loopback_of.is_some())
        .collect();
    println!("monitors: {monitors:#?}");
    for monitor in &monitors {
        let output = monitor.loopback_of.as_deref().expect("filtered");
        assert_eq!(monitor.name, format!("{output}.monitor"));
        assert!(
            outputs.iter().any(|listed| listed.name == output),
            "{monitor:?} is not of a listed output: {outputs:#?}"
        );
    }
    // WASAPI endpoints are never inputs too, so every output has one.
    if cfg!(target_os = "windows") {
        assert_eq!(monitors.len(), outputs.len(), "{inputs:#?} {outputs:#?}");
    }
}

/// On macOS, the first process tap asks for the permission to record
/// system audio, which a CI runner cannot grant.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "asks for the permission to record system audio"
)]
fn a_monitor_captures_while_nothing_plays() {
    let mut capture = CpalCapture::new();
    let inputs = capture.audio_inputs().expect("list inputs");
    let Some(monitor) = inputs.iter().find(|input| input.loopback_of.is_some()) else {
        println!("no output to capture: {inputs:#?}");
        return;
    };
    let report = capture
        .probe_audio(&AudioCaptureRequest {
            input: Some(monitor.name.clone()),
            duration: Duration::from_millis(500),
        })
        .expect("capture the monitor");
    println!("{report:#?}");
    // On Windows the capture plays silence on the output, so the loopback
    // delivers even when nothing else plays.
    assert!(report.stats.frames > 0, "no audio arrived: {report:#?}");
    let format = report.format.as_ref().expect("format");
    assert!(format.sample_rate_hz > 0 && format.channels > 0);
}

#[test]
#[ignore = "plays a quiet tone (-30 dBFS, 440 Hz) on the default output for 1 s and captures it"]
fn the_default_outputs_monitor_captures_what_it_plays() {
    use cpal::traits::{DeviceTrait, HostTrait};
    let device = cpal::default_host()
        .default_output_device()
        .expect("a default output");
    let id = device.id().expect("its id").id().to_owned();
    let mut capture = CpalCapture::new();
    let monitor = format!("{id}.monitor");
    let inputs = capture.audio_inputs().expect("list inputs");
    if !inputs.iter().any(|input| input.name == monitor) {
        // On macOS, an output that is an input too has no monitor.
        println!("SKIPPED: the default output `{id}` has no monitor: {inputs:#?}");
        return;
    }
    let stream = capture
        .start_audio_stream(&AudioStreamRequest {
            input: Some(monitor.clone()),
            rate: 48_000,
            channels: 2,
        })
        .unwrap_or_else(|error| panic!("capture `{monitor}`: {error:?}"));
    let (mut writer, reader) = sample_ring(2, 24_000);
    let playback = capture
        .start_playback(&playback(Some(&id), 2), reader)
        .expect("play on the default output");
    let mut phase = 0.0_f32;
    let mut block = vec![0.0; 480 * 2];
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(1) {
        while writer.fill_frames() < 4_800 {
            for frame in block.as_chunks_mut::<2>().0 {
                frame.fill(0.0316 * (phase * TAU).sin());
                phase = (phase + 440.0 / 48_000.0).fract();
            }
            writer.write(&block);
        }
        thread::sleep(Duration::from_millis(5));
    }
    playback.stop().expect("stop playing");
    let report = stream.stop().expect("stop capturing");
    println!("{report:#?}");
    // The output's volume may apply before the loopback; any level shows
    // that the tone arrived.
    let peak = report.stats.peak_dbfs.expect("the tone was captured");
    assert!(peak > -60.0, "{report:#?}");
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
    let missing_monitor = capture
        .start_audio_stream(&AudioStreamRequest {
            input: Some("no-such-output-device.monitor".to_owned()),
            rate: 48_000,
            channels: 2,
        })
        .err()
        .expect("must fail");
    assert_eq!(missing_monitor.kind, CaptureErrorKind::NotFound);
    assert!(missing_monitor.details.expect("details")["available"].is_array());
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

#[test]
fn the_default_output_plays_or_reports_that_there_is_none() {
    let mut capture = CpalCapture::new();
    let outputs = capture.audio_outputs().expect("list outputs");
    println!("outputs: {outputs:#?}");
    assert!(
        outputs
            .iter()
            .all(|output| !output.name.is_empty() && output.media_class == "Audio/Sink"),
        "{outputs:#?}"
    );
    // Nothing is written, so the device plays silence.
    let (writer, reader) = sample_ring(2, 24_000);
    match capture.start_playback(&playback(None, 2), reader) {
        Ok(playback) => {
            assert_eq!(playback.state(), CaptureState::Running);
            assert!(
                wait_until(Duration::from_secs(3), || writer.stats().max_read_frames
                    > 0),
                "the device never pulled: {:?}",
                writer.stats()
            );
            println!("feed: {:?}", writer.stats());
            playback.stop().expect("stop");
            // The stopped device let go of its feed.
            assert!(writer.is_closed());
        }
        Err(error) => {
            println!("{error:?}");
            // A CI VM's audio device may also not open in time (seen on
            // macOS runners): that is a structured report too.
            assert!(
                matches!(
                    error.kind,
                    CaptureErrorKind::NotFound
                        | CaptureErrorKind::Unavailable
                        | CaptureErrorKind::PermissionDenied
                        | CaptureErrorKind::Timeout
                ),
                "{error:?}"
            );
            assert!(
                outputs.is_empty()
                    || error.kind != CaptureErrorKind::NotFound
                    || error.message.contains("no default audio output"),
                "{error:?}"
            );
            assert!(writer.is_closed());
        }
    }
}

#[test]
fn unknown_outputs_and_unplayable_requests_are_errors() {
    let mut capture = CpalCapture::new();
    let (writer, reader) = sample_ring(2, 64);
    let missing = capture
        .start_playback(&playback(Some("no-such-output-device"), 2), reader)
        .err()
        .expect("must fail");
    assert_eq!(missing.kind, CaptureErrorKind::NotFound, "{missing:?}");
    let details = missing.details.expect("details");
    assert_eq!(details["output"], "no-such-output-device");
    assert!(details["available"].is_array());
    // A refused feed is closed.
    assert!(writer.is_closed());

    let (_writer, reader) = sample_ring(2, 64);
    let surround = capture
        .start_playback(&playback(None, 6), reader)
        .err()
        .expect("must fail");
    assert_eq!(surround.kind, CaptureErrorKind::Unsupported);
    let (_writer, reader) = sample_ring(2, 64);
    let mismatch = capture
        .start_playback(&playback(None, 1), reader)
        .err()
        .expect("must fail");
    assert_eq!(mismatch.kind, CaptureErrorKind::InvalidRequest);
}

#[test]
#[ignore = "plays a quiet tone (-30 dBFS, 440 Hz) on the default output for 2 s"]
fn a_tone_plays_on_the_default_output_without_gaps() {
    use cpal::traits::{DeviceTrait, HostTrait};
    let device = cpal::default_host().default_output_device();
    let config = device.as_ref().map(DeviceTrait::default_output_config);
    println!("default output config: {config:?}");

    let (mut writer, reader) = sample_ring(2, 24_000);
    let playback = CpalCapture::new()
        .start_playback(&playback(None, 2), reader)
        .expect("play on the default output");
    // As the engine's feed does: keep about 100 ms queued, in 10 ms blocks.
    let mut phase = 0.0_f32;
    let mut block = vec![0.0; 480 * 2];
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(2) {
        while writer.fill_frames() < 4_800 {
            for frame in block.as_chunks_mut::<2>().0 {
                frame.fill(0.0316 * (phase * TAU).sin());
                phase = (phase + 440.0 / 48_000.0).fract();
            }
            writer.write(&block);
        }
        assert_eq!(playback.state(), CaptureState::Running);
        thread::sleep(Duration::from_millis(5));
    }
    let stats = writer.stats();
    println!("feed after 2 s: {stats:?}");
    playback.stop().expect("stop");
    assert!(writer.is_closed());
    // About 2 s pulled (less the queue and the start), without a gap.
    assert!(stats.read_frames >= 80_000, "{stats:?}");
    assert_eq!(stats.underrun_frames, 0, "{stats:?}");
    assert_eq!(stats.overrun_frames, 0, "{stats:?}");
}
