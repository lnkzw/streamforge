//! Live tests against the running desktop session.
//!
//! Ignored by default because they need PipeWire and xdg-desktop-portal, and
//! the audio probe opens a real input device. Playback plays into a null
//! sink the test creates, so nothing is audible. Run with:
//!
//! ```text
//! cargo test -p capture-linux --test live -- --ignored --test-threads=1
//! ```
//!
//! Screen capture needs a human to pick a source in the portal dialog, so it
//! is covered by the manual procedure in `docs/capture-linux.md` instead.
#![cfg(target_os = "linux")]

use std::f32::consts::TAU;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use capture_api::{
    AudioCaptureRequest, AudioDevices, AudioStreamRequest, CaptureErrorKind, CaptureState,
    PlaybackRequest, ScreenCapture,
};
use capture_linux::{PipeWireAudio, PortalScreen};
use media_core::sample_ring;
use pipewire as pw;

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
    let caps = PipeWireAudio::new().capability();
    assert!(caps.available, "{:?}", caps.unavailable_reason);
    assert!(caps.server_version.is_some());
    let screen = PortalScreen::new().capability();
    assert!(screen.available, "{:?}", screen.unavailable_reason);
    assert!(!screen.target_kinds.is_empty());
}

#[test]
#[ignore = "needs a running PipeWire daemon"]
fn audio_inputs_are_listed_with_names() {
    if !session_available() {
        return;
    }
    let inputs = PipeWireAudio::new().audio_inputs().expect("list inputs");
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
    let mut backend = PipeWireAudio::new();
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
    assert!(format.sample_rate_hz > 0 && format.channels > 0);
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
    let err = PipeWireAudio::new()
        .probe_audio(&AudioCaptureRequest {
            input: Some("streamforge.test.does-not-exist".into()),
            duration: Duration::from_millis(100),
        })
        .unwrap_err();
    assert_eq!(err.kind, capture_api::CaptureErrorKind::NotFound);
    assert!(err.details.is_some());
}

#[test]
#[ignore = "needs a running PipeWire daemon"]
fn audio_outputs_are_listed_with_their_monitors() {
    if !session_available() {
        return;
    }
    let mut backend = PipeWireAudio::new();
    let outputs = backend.audio_outputs().expect("list outputs");
    let inputs = backend.audio_inputs().expect("list inputs");
    for output in &outputs {
        assert!(!output.name.is_empty());
        assert!(output.media_class.starts_with("Audio/"), "{output:?}");
        if output.media_class.starts_with("Audio/Sink") {
            let monitor = format!("{}.monitor", output.name);
            assert!(
                inputs.iter().any(|input| input.name == monitor
                    && input.loopback_of.as_ref() == Some(&output.name)),
                "no monitor input for {output:?}"
            );
        }
    }
    eprintln!("found {} audio output(s)", outputs.len());
}

#[test]
#[ignore = "needs a running PipeWire daemon"]
fn unknown_audio_output_is_not_found() {
    if !session_available() {
        return;
    }
    let (writer, reader) = sample_ring(2, 4_800);
    let request = PlaybackRequest {
        output: Some("streamforge.test.does-not-exist".into()),
        rate: 48_000,
        channels: 2,
    };
    let Err(err) = PipeWireAudio::new().start_playback(&request, reader) else {
        panic!("played on an unknown output");
    };
    assert_eq!(err.kind, CaptureErrorKind::NotFound);
    assert!(err.details.is_some());
    assert!(writer.is_closed());
}

/// A PipeWire null sink nobody hears, for playback tests. It lives while
/// its client, a thread here, stays connected.
struct TestSink {
    name: String,
    stop: Option<pw::channel::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl TestSink {
    fn create() -> Self {
        let name = format!("streamforge-test-sink-{}", std::process::id());
        let (stop, stopped) = pw::channel::channel::<()>();
        let (ready, created) = mpsc::channel();
        let thread = thread::spawn({
            let name = name.clone();
            move || {
                pw::init();
                let main_loop = pw::main_loop::MainLoopRc::new(None).expect("main loop");
                let context = pw::context::ContextRc::new(&main_loop, None).expect("context");
                let core = context.connect_rc(None).expect("connect to PipeWire");
                let sink: pw::node::Node = core
                    .create_object(
                        "adapter",
                        &pw::properties::properties! {
                            "factory.name" => "support.null-audio-sink",
                            *pw::keys::NODE_NAME => name.as_str(),
                            *pw::keys::NODE_DESCRIPTION => "StreamForge test sink",
                            *pw::keys::MEDIA_CLASS => "Audio/Sink",
                            "audio.position" => "[ FL FR ]",
                        },
                    )
                    .expect("create the sink");
                let _stopped = stopped.attach(main_loop.loop_(), {
                    let main_loop = main_loop.clone();
                    move |()| main_loop.quit()
                });
                ready.send(()).expect("test waits");
                main_loop.run();
                drop(sink);
            }
        });
        created.recv().expect("the sink thread failed");
        Self {
            name,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    /// Remove the sink, as when a device is unplugged.
    fn remove(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().expect("sink thread");
        }
    }
}

impl Drop for TestSink {
    fn drop(&mut self) {
        self.remove();
    }
}

/// Poll `done` every 20 ms until it holds or `timeout` passes.
fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while !done() {
        if Instant::now() > deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
    true
}

#[test]
#[ignore = "creates a temporary PipeWire null sink (nothing is audible) and plays a tone into it"]
fn playback_plays_on_a_sink_and_ends_with_it() {
    if !session_available() {
        return;
    }
    let mut sink = TestSink::create();
    let mut backend = PipeWireAudio::new();
    assert!(
        wait_until(Duration::from_secs(3), || {
            let outputs = backend.audio_outputs().expect("list outputs");
            outputs.iter().any(|output| output.name == sink.name)
        }),
        "the test sink was not listed"
    );
    let monitor = backend
        .audio_inputs()
        .expect("list inputs")
        .into_iter()
        .find(|input| input.loopback_of.as_ref() == Some(&sink.name))
        .expect("the sink's monitor is an input");
    assert_eq!(monitor.name, format!("{}.monitor", sink.name));

    // Capture what the sink plays, then play a -20 dBFS tone into it.
    let capture = backend
        .start_audio_stream(&AudioStreamRequest {
            input: Some(monitor.name),
            rate: 48_000,
            channels: 2,
        })
        .expect("capture the monitor");
    let (mut writer, reader) = sample_ring(2, 24_000);
    let request = PlaybackRequest {
        output: Some(sink.name.clone()),
        rate: 48_000,
        channels: 2,
    };
    let playback = backend.start_playback(&request, reader).expect("play");
    assert_eq!(playback.state(), CaptureState::Running);
    let mut phase = 0.0_f32;
    let mut block = vec![0.0; 480 * 2];
    let mut captured = vec![0.0; 48_000 * 2];
    let mut peak = 0.0_f32;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(1) {
        while writer.fill_frames() < 4_800 {
            for frame in block.as_chunks_mut::<2>().0 {
                frame.fill(0.1 * (phase * TAU).sin());
                phase = (phase + 440.0 / 48_000.0).fract();
            }
            writer.write(&block);
        }
        let ring = capture.samples();
        loop {
            let frames = ring.pull(&mut captured);
            if frames == 0 {
                break;
            }
            peak = captured[..frames * 2]
                .iter()
                .fold(peak, |peak, sample| peak.max(sample.abs()));
        }
        thread::sleep(Duration::from_millis(5));
    }
    let stats = writer.stats();
    eprintln!("feed after 1 s: {stats:?}; captured peak {peak}");
    assert!((0.08..=0.12).contains(&peak), "captured peak {peak}");
    assert!(stats.read_frames >= 24_000, "{stats:?}");

    // Unplugged: the playback ends and closes its feed.
    sink.remove();
    assert!(
        wait_until(Duration::from_secs(3), || playback.state()
            != CaptureState::Running),
        "the playback did not end with its sink"
    );
    let state = playback.state();
    eprintln!("after the sink went away: {state:?}");
    // WirePlumber reports the missing target (`... is gone`), or the stream
    // just disconnects (`... went away`); either names the output.
    let CaptureState::Ended { reason } = state else {
        unreachable!()
    };
    assert!(reason.contains(&sink.name), "{reason}");
    assert!(writer.is_closed());
    playback.stop().expect("stop after the end");
    let _ = capture.stop();
}

#[test]
#[ignore = "creates and removes a temporary PipeWire null sink; nothing is audible"]
fn an_output_gone_since_the_listing_does_not_fall_back_to_the_default() {
    if !session_available() {
        return;
    }
    let mut sink = TestSink::create();
    let mut backend = PipeWireAudio::new();
    assert!(
        wait_until(Duration::from_secs(3), || {
            let outputs = backend.audio_outputs().expect("list outputs");
            outputs.iter().any(|output| output.name == sink.name)
        }),
        "the test sink was not listed"
    );
    // The listing is still fresh enough for the name check; PipeWire must
    // not play on the default sink instead.
    sink.remove();
    let (writer, reader) = sample_ring(2, 4_800);
    let request = PlaybackRequest {
        output: Some(sink.name.clone()),
        rate: 48_000,
        channels: 2,
    };
    let Err(err) = backend.start_playback(&request, reader) else {
        panic!("played although the output is gone");
    };
    eprintln!("{err:?}");
    assert_eq!(err.kind, CaptureErrorKind::NotFound, "{err:?}");
    assert!(writer.is_closed());
}
