//! The monitor (ADR 0017) with a fake playback device.

use std::sync::Mutex;
use std::thread::JoinHandle;

use capture_api::{CaptureEnd, CaptureState, PlaybackRequest, PlaybackStream};
use command_api::{OutputProfileSet, PROGRAM_BUS_ID, RecordingReport, RecordingState};
use media_core::SampleReader;

use super::*;

/// A fake device that pulls 256 frames at a time at exactly 48 kHz and
/// records what it plays: `speakers` (and the default) plays until stopped,
/// `flaky` is unplugged after 300 ms, anything else is not found.
pub(super) fn start_fake_playback(
    request: &PlaybackRequest,
    mut feed: SampleReader,
    played: &Arc<Mutex<Vec<f32>>>,
) -> Result<Box<dyn PlaybackStream>, CaptureError> {
    let lifetime = match request.output.as_deref() {
        None | Some("speakers") => None,
        Some("flaky") => Some(Duration::from_millis(300)),
        Some(other) => {
            return Err(CaptureError::new(
                CaptureErrorKind::NotFound,
                format!("audio output `{other}` not found"),
            ));
        }
    };
    let end = Arc::new(CaptureEnd::new());
    let stop = Arc::new(AtomicBool::new(false));
    let thread = std::thread::spawn({
        let (end, stop, played) = (Arc::clone(&end), Arc::clone(&stop), Arc::clone(played));
        move || {
            let start = Instant::now();
            let mut chunk = [0.0f32; 2 * 256];
            let mut consumed = 0u64;
            while !stop.load(Ordering::Acquire) {
                if lifetime.is_some_and(|lifetime| start.elapsed() >= lifetime) {
                    // The feed goes with the thread, which closes it.
                    end.end(|| "the device was unplugged".to_owned());
                    return;
                }
                let due = (start.elapsed().as_secs_f64() * 48_000.0) as u64;
                while consumed + 256 <= due {
                    feed.read(&mut chunk);
                    played.lock().expect("played").extend_from_slice(&chunk);
                    consumed += 256;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    });
    Ok(Box::new(FakePlayback {
        end,
        stop,
        thread: Some(thread),
    }))
}

struct FakePlayback {
    end: Arc<CaptureEnd>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl PlaybackStream for FakePlayback {
    fn state(&self) -> CaptureState {
        self.end.state()
    }

    fn stop(mut self: Box<Self>) -> Result<(), CaptureError> {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("device thread");
        }
        Ok(())
    }
}

fn engine_with_speakers(dir: &TempDir) -> (Engine, Arc<Mutex<Vec<f32>>>) {
    let played = Arc::default();
    let engine = Engine::with_capture(Capture {
        audio: Box::new(FakeAudio {
            played: Arc::clone(&played),
        }),
        ..Capture::unsupported("the monitor tests capture no screen")
    })
    .with_output_dir(&dir.0);
    (engine, played)
}

/// The peak of what was played from `from` (a sample index) on, after
/// `skip_ms` of it.
fn peak_since(played: &Mutex<Vec<f32>>, from: usize, skip_ms: usize) -> f32 {
    let played = played.lock().expect("played");
    let start = (from + skip_ms * 96).min(played.len());
    played[start..]
        .iter()
        .fold(0.0, |peak, sample| peak.max(sample.abs()))
}

fn played_len(played: &Mutex<Vec<f32>>) -> usize {
    played.lock().expect("played").len()
}

fn profile_set(engine: &mut Engine, params: serde_json::Value) -> OutputProfileSet {
    match run(engine, "set_output_profile", params) {
        CommandOutput::OutputProfileSet(set) => set,
        other => panic!("unexpected output: {other:?}"),
    }
}

fn monitor_health(engine: &mut Engine) -> command_api::MonitorHealth {
    health(engine).monitor.expect("monitor health")
}

fn stop_recording(engine: &mut Engine) -> RecordingReport {
    match engine.handle(Command::StopRecording).expect("stop") {
        CommandOutput::RecordingReport(report) => *report,
        other => panic!("unexpected output: {other:?}"),
    }
}

#[test]
fn the_monitor_plays_its_bus_live_beside_a_recording() {
    let _media = media_test_lock();
    let dir = TempDir::new("monitor-plays");
    let (mut engine, played) = engine_with_speakers(&dir);
    let CommandOutput::OutputProfiles(profiles) =
        run(&mut engine, "get_output_profiles", json!({}))
    else {
        panic!("profiles");
    };
    // Off by default, on the program bus and the default device.
    assert!(!profiles.monitor.enabled);
    assert_eq!(profiles.monitor.mix, PROGRAM_BUS_ID);
    assert_eq!(profiles.monitor.device, None);
    // A separate mix with the tone 20 dB lower: about -40 dBFS.
    run(
        &mut engine,
        "add_bus",
        json!({"name": "Cue", "assign_db": null}),
    );
    run(
        &mut engine,
        "set_send",
        json!({"channel_name": "Tone", "bus_name": "Cue", "enabled": true, "level_db": -20}),
    );
    let set = profile_set(
        &mut engine,
        json!({"output": "monitor", "bus_name": "Cue", "enabled": true}),
    );
    // Nothing runs yet: the profile waits for the pipeline.
    assert!(
        set.applied_now.is_empty() && set.warnings.is_empty(),
        "{set:?}"
    );
    let cue = set.profiles.monitor.mix.clone();

    run(
        &mut engine,
        "start_recording",
        json!({"file_name": "take.mkv"}),
    );
    std::thread::sleep(Duration::from_millis(600));
    let peak = peak_since(&played, 0, 300);
    assert!((peak - 0.01).abs() < 0.002, "monitor peak {peak}");
    let monitor = monitor_health(&mut engine);
    assert_eq!(monitor.state, CaptureState::Running);
    assert!(monitor.enabled);
    let latency = monitor.latency_ms.expect("latency");
    assert!(latency > 0.0 && latency < 100.0, "{monitor:?}");
    let CommandOutput::MixerState(mixer) = run(&mut engine, "get_mixer", json!({})) else {
        panic!("mixer");
    };
    let entry = &mixer.outputs[2];
    assert_eq!(entry.output, command_api::OutputKind::Monitor);
    assert!(entry.running);
    assert_eq!(entry.bus_id, cue);

    // Live, faded: the program bus at -20 dBFS.
    let mark = played_len(&played);
    let set = profile_set(
        &mut engine,
        json!({"output": "monitor", "bus_name": "Program"}),
    );
    assert_eq!(set.applied_now, ["mix"]);
    std::thread::sleep(Duration::from_millis(400));
    let peak = peak_since(&played, mark, 150);
    assert!((peak - 0.1).abs() < 0.01, "monitor peak {peak}");

    // Off: silence, with the device still open.
    let set = profile_set(&mut engine, json!({"output": "monitor", "enabled": false}));
    assert_eq!(set.applied_now, ["enabled"]);
    let mark = played_len(&played);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(peak_since(&played, mark, 150), 0.0);
    let monitor = monitor_health(&mut engine);
    assert!(!monitor.enabled);
    assert_eq!(monitor.state, CaptureState::Running);

    // The recording never noticed.
    assert_eq!(
        status(&mut engine).recording_state,
        Some(RecordingState::Running)
    );
    let report = stop_recording(&mut engine);
    assert!(
        report.recording.audio_tracks[0].samples_encoded > 0,
        "{report:?}"
    );
    let monitor = report.pipeline.monitor.expect("monitor in the report");
    assert!(monitor.feed.expect("feed").ring.read_frames > 0);
}

#[test]
fn a_monitor_device_that_does_not_open_only_warns() {
    let _media = media_test_lock();
    let dir = TempDir::new("monitor-gone");
    let (mut engine, _played) = engine_with_speakers(&dir);
    run(&mut engine, "set_monitor_device", json!({"device": "gone"}));
    profile_set(&mut engine, json!({"output": "monitor", "enabled": true}));
    let CommandOutput::RecordingStarted(started) = run(
        &mut engine,
        "start_recording",
        json!({"file_name": "take.mkv"}),
    ) else {
        panic!("started");
    };
    assert!(
        started
            .warnings
            .iter()
            .any(|warning| warning.contains("the monitor is silent: `gone` did not open")),
        "{:?}",
        started.warnings
    );
    let monitor = monitor_health(&mut engine);
    assert!(
        matches!(&monitor.state, CaptureState::Ended { reason } if reason.starts_with("could not start")),
        "{monitor:?}"
    );
    assert!(monitor.feed.is_none());
    let CommandOutput::MixerState(mixer) = run(&mut engine, "get_mixer", json!({})) else {
        panic!("mixer");
    };
    assert!(!mixer.outputs[2].running);
    assert!(
        mixer
            .warnings
            .iter()
            .any(|w| w.contains("monitor is silent"))
    );
    // The device changes only while idle.
    let busy = try_run(
        &mut engine,
        "set_monitor_device",
        json!({"device": "speakers"}),
    );
    assert_eq!(busy.unwrap_err().code, ErrorCode::InvalidState);
    // Switching on again tries again, and says so.
    let set = profile_set(&mut engine, json!({"output": "monitor", "enabled": true}));
    assert_eq!(set.applied_now, ["enabled"]);
    assert!(!set.warnings.is_empty(), "{set:?}");
    assert_eq!(
        status(&mut engine).recording_state,
        Some(RecordingState::Running)
    );
    stop_recording(&mut engine);

    // Idle: the default device works.
    let CommandOutput::OutputProfiles(profiles) =
        run(&mut engine, "set_monitor_device", json!({"device": null}))
    else {
        panic!("profiles");
    };
    assert_eq!(profiles.monitor.device, None);
}

#[test]
fn a_monitor_that_ends_leaves_the_recording_running() {
    let _media = media_test_lock();
    let dir = TempDir::new("monitor-flaky");
    let (mut engine, _played) = engine_with_speakers(&dir);
    run(
        &mut engine,
        "set_monitor_device",
        json!({"device": "flaky"}),
    );
    run(
        &mut engine,
        "start_recording",
        json!({"file_name": "take.mkv"}),
    );
    // Switched on while the recording runs.
    let set = profile_set(&mut engine, json!({"output": "monitor", "enabled": true}));
    assert_eq!(set.applied_now, ["enabled"]);
    assert!(set.warnings.is_empty(), "{set:?}");
    std::thread::sleep(Duration::from_millis(600));
    let monitor = monitor_health(&mut engine);
    assert_eq!(
        monitor.state,
        CaptureState::Ended {
            reason: "the device was unplugged".to_owned()
        }
    );
    assert_eq!(monitor.latency_ms, None);
    assert_eq!(
        status(&mut engine).recording_state,
        Some(RecordingState::Running)
    );
    // No device runs: a new bus waits for the monitor to be switched on.
    let set = profile_set(
        &mut engine,
        json!({"output": "monitor", "bus_name": "Program"}),
    );
    assert!(set.applied_now.is_empty(), "{set:?}");
    assert_eq!(set.next_start, ["mix"]);
    // Switching on again opens it anew.
    profile_set(&mut engine, json!({"output": "monitor", "enabled": true}));
    assert_eq!(monitor_health(&mut engine).state, CaptureState::Running);
    std::thread::sleep(Duration::from_millis(500));
    let report = stop_recording(&mut engine);
    assert!(report.recording.audio_tracks[0].samples_encoded > 0);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("the monitor ended")),
        "{:?}",
        report.warnings
    );
}

#[test]
fn a_channel_that_captures_the_monitor_device_is_a_feedback_warning() {
    let _media = media_test_lock();
    let dir = TempDir::new("monitor-loop");
    let (mut engine, _played) = engine_with_speakers(&dir);
    let CommandOutput::AudioOutputs(outputs) = run(&mut engine, "list_audio_outputs", json!({}))
    else {
        panic!("outputs");
    };
    assert_eq!(outputs.outputs[0].name, "speakers");
    run(
        &mut engine,
        "add_channel",
        json!({"name": "Loop", "input": {"kind": "device", "device": "speakers.monitor"}}),
    );
    run(
        &mut engine,
        "set_monitor_device",
        json!({"device": "speakers"}),
    );
    run(
        &mut engine,
        "start_recording",
        json!({"file_name": "take.mkv"}),
    );
    let set = profile_set(&mut engine, json!({"output": "monitor", "enabled": true}));
    assert!(
        set.warnings
            .iter()
            .any(|w| w.contains("channel `Loop` captures what the monitor plays")),
        "{set:?}"
    );
    // Off, the warning goes; on again, it is back.
    profile_set(&mut engine, json!({"output": "monitor", "enabled": false}));
    let CommandOutput::MixerState(mixer) = run(&mut engine, "get_mixer", json!({})) else {
        panic!("mixer");
    };
    assert!(
        !mixer.warnings.iter().any(|w| w.contains("Loop")),
        "{mixer:?}"
    );
    let set = profile_set(&mut engine, json!({"output": "monitor", "enabled": true}));
    assert!(
        set.warnings.iter().any(|w| w.contains("channel `Loop`")),
        "{set:?}"
    );
    stop_recording(&mut engine);
}
