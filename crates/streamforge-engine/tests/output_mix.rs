//! ADR 0017 step 3: an output records the bus its profile names, and a
//! new mix applies live, through the real engine process and a decoded file.

mod common;

use std::time::Duration;

use common::{EngineProcess, TempDir};
use ffmpeg_next as ff;
use serde_json::Value;

/// How long each mix is recorded.
const SPAN: Duration = Duration::from_millis(1_200);

fn ok(response: Value) -> Value {
    assert_eq!(response["ok"], true, "{response}");
    response["result"].clone()
}

#[test]
fn a_recording_takes_its_profiles_mix_and_switches_live() {
    let _media = common::media_lock();
    let dir = TempDir::new("output-mix");
    let mut engine =
        EngineProcess::spawn_with(&[std::ffi::OsStr::new("--output-dir"), dir.0.as_os_str()]);
    // A separate mix that gets the default tone (-20 dBFS) 20 dB lower.
    ok(engine
        .request(r#"{"id":1,"command":"add_bus","params":{"name":"Quiet","assign_db":null}}"#));
    ok(engine.request(
        r#"{"id":2,"command":"set_send","params":{"channel":"Tone","bus":"Quiet","enabled":true,"level_db":-20}}"#,
    ));
    let set = ok(engine.request(
        r#"{"id":3,"command":"set_output_profile","params":{"output":"recording","mix_bus":"Quiet"}}"#,
    ));
    assert_eq!(set["applied_now"], serde_json::json!([]), "{set}");
    let quiet = set["recording"]["mix"].as_str().expect("bus id").to_owned();
    // A profile's bus cannot be removed.
    let removed = engine.request(r#"{"id":4,"command":"remove_bus","params":{"name":"Quiet"}}"#);
    assert_eq!(removed["error"]["code"], "invalid_state", "{removed}");

    ok(engine.request(r#"{"id":5,"command":"start_recording","params":{"file_name":"mix.mkv"}}"#));
    let mixer = ok(engine.request(r#"{"id":6,"command":"get_mixer"}"#));
    assert_eq!(mixer["outputs"][0]["bus"], quiet.as_str(), "{mixer}");
    assert_eq!(mixer["outputs"][0]["running"], true);
    std::thread::sleep(SPAN);
    // The new mix applies now; the encoder would wait for the next start.
    let set = ok(engine.request(
        r#"{"id":7,"command":"set_output_profile","params":{"output":"recording","mix_bus":"Program","video_encoder":"software"}}"#,
    ));
    assert_eq!(set["applied_now"], serde_json::json!(["mix"]), "{set}");
    assert_eq!(
        set["next_start"],
        serde_json::json!(["video_encoder"]),
        "{set}"
    );
    let mixer = ok(engine.request(r#"{"id":8,"command":"get_mixer"}"#));
    assert_eq!(mixer["outputs"][0]["bus"], "bus_program", "{mixer}");
    std::thread::sleep(SPAN);
    ok(engine.request(r#"{"id":9,"command":"stop_recording"}"#));
    assert!(engine.finish().success());

    // Peaks per 50 ms of the left channel: about -40 dBFS, then -20 from
    // the switch on (found in the data, as runners differ in timing).
    let peaks = peaks_dbfs(&dir.0.join("mix.mkv"), 2_400);
    let switch = peaks
        .iter()
        .position(|peak| *peak > -30.0)
        .unwrap_or_else(|| panic!("never switched: {peaks:?}"));
    // About one span in: well after the start, well before the end.
    assert!(
        (16..peaks.len().saturating_sub(10)).contains(&switch),
        "switch at window {switch}: {peaks:?}"
    );
    let quiet_part = &peaks[2..switch - 1];
    let program_part = &peaks[switch + 1..peaks.len() - 2];
    for (part, dbfs) in [(quiet_part, -40.0), (program_part, -20.0)] {
        assert!(
            part.iter().all(|peak| (peak - dbfs).abs() < 1.5),
            "expected {dbfs} dBFS: {part:?} (all: {peaks:?})"
        );
    }
    // The switch fades: nothing louder than the program level on the way.
    assert!(
        peaks.iter().all(|peak| *peak < -19.0),
        "a jump past the mixes: {peaks:?}"
    );
}

/// The decoded audio's peak per `window` samples of its first channel.
fn peaks_dbfs(path: &std::path::Path, window: usize) -> Vec<f64> {
    ff::init().expect("init");
    let mut input = ff::format::input(path).expect("open recording");
    let stream = input.streams().best(ff::media::Type::Audio).expect("audio");
    let index = stream.index();
    let mut decoder = ff::codec::context::Context::from_parameters(stream.parameters())
        .expect("ctx")
        .decoder()
        .audio()
        .expect("decoder");
    let mut samples = Vec::new();
    let mut frame = ff::frame::Audio::empty();
    for (stream, packet) in input.packets() {
        if stream.index() != index {
            continue;
        }
        decoder.send_packet(&packet).expect("send");
        while decoder.receive_frame(&mut frame).is_ok() {
            samples.extend_from_slice(&frame.plane::<f32>(0)[..frame.samples()]);
        }
    }
    samples
        .chunks(window)
        .filter(|chunk| chunk.len() == window)
        .map(|chunk| {
            let peak = chunk
                .iter()
                .fold(0f32, |peak, sample| peak.max(sample.abs()));
            20.0 * f64::from(peak).log10()
        })
        .collect()
}

#[test]
fn a_recording_carries_its_tracks_named_after_their_buses() {
    let _media = common::media_lock();
    let dir = TempDir::new("tracks");
    let mut engine =
        EngineProcess::spawn_with(&[std::ffi::OsStr::new("--output-dir"), dir.0.as_os_str()]);
    ok(engine
        .request(r#"{"id":1,"command":"add_bus","params":{"name":"Quiet","assign_db":null}}"#));
    ok(engine.request(
        r#"{"id":2,"command":"set_send","params":{"channel":"Tone","bus":"Quiet","enabled":true,"level_db":-20}}"#,
    ));
    let set = ok(engine.request(
        r#"{"id":3,"command":"set_output_profile","params":{"output":"recording","tracks":["Quiet"]}}"#,
    ));
    let quiet = set["recording"]["tracks"][0]
        .as_str()
        .expect("id")
        .to_owned();
    // A track's bus cannot be removed.
    let removed = engine.request(r#"{"id":4,"command":"remove_bus","params":{"name":"Quiet"}}"#);
    assert_eq!(removed["error"]["code"], "invalid_state", "{removed}");
    ok(engine
        .request(r#"{"id":5,"command":"start_recording","params":{"file_name":"tracks.mkv"}}"#));
    let mixer = ok(engine.request(r#"{"id":6,"command":"get_mixer"}"#));
    assert_eq!(
        mixer["outputs"][0]["tracks"],
        serde_json::json!([quiet]),
        "{mixer}"
    );
    // The track list is fixed while it records.
    let set = ok(engine.request(
        r#"{"id":7,"command":"set_output_profile","params":{"output":"recording","tracks":[]}}"#,
    ));
    assert_eq!(set["next_start"], serde_json::json!(["tracks"]), "{set}");
    // Its running track cannot become its mix as well, although the next
    // profile no longer has it as a track.
    let refused = engine.request(
        r#"{"id":8,"command":"set_output_profile","params":{"output":"recording","mix_bus":"Quiet"}}"#,
    );
    assert_eq!(refused["error"]["code"], "invalid_state", "{refused}");
    assert_eq!(
        refused["error"]["details"]["bus"],
        quiet.as_str(),
        "{refused}"
    );
    std::thread::sleep(SPAN);
    let stopped = ok(engine.request(r#"{"id":9,"command":"stop_recording"}"#));
    assert!(engine.finish().success());
    let recording = &stopped["recording"];
    assert_eq!(recording["audio"]["name"], "Program", "{recording}");
    assert_eq!(recording["extra_audio"][0]["name"], "Quiet", "{recording}");
    assert_eq!(
        recording["extra_audio"][0]["samples_encoded"], recording["audio"]["samples_encoded"],
        "{recording}"
    );

    // Two AAC streams, named after their buses, at their buses' levels.
    let found = track_peaks(&dir.0.join("tracks.mkv"));
    let names: Vec<_> = found.iter().map(|(name, _)| name.as_deref()).collect();
    assert_eq!(names, [Some("Program"), Some("Quiet")]);
    for ((_, peak), dbfs) in found.iter().zip([-20.0, -40.0]) {
        assert!((peak - dbfs).abs() < 1.5, "{found:?}");
    }
}

/// Each audio stream's name and its peak (dBFS) after the first 100 ms.
fn track_peaks(path: &std::path::Path) -> Vec<(Option<String>, f64)> {
    ff::init().expect("init");
    let mut input = ff::format::input(path).expect("open recording");
    let mut tracks: Vec<(usize, Option<String>, ff::decoder::Audio, Vec<f32>)> = input
        .streams()
        .filter(|stream| stream.parameters().medium() == ff::media::Type::Audio)
        .map(|stream| {
            let decoder = ff::codec::context::Context::from_parameters(stream.parameters())
                .expect("ctx")
                .decoder()
                .audio()
                .expect("decoder");
            let name = stream.metadata().get("title").map(str::to_owned);
            (stream.index(), name, decoder, Vec::new())
        })
        .collect();
    let mut frame = ff::frame::Audio::empty();
    for (stream, packet) in input.packets() {
        let Some(track) = tracks.iter_mut().find(|track| track.0 == stream.index()) else {
            continue;
        };
        track.2.send_packet(&packet).expect("send");
        while track.2.receive_frame(&mut frame).is_ok() {
            track
                .3
                .extend_from_slice(&frame.plane::<f32>(0)[..frame.samples()]);
        }
    }
    tracks
        .into_iter()
        .map(|(_, name, _, samples)| {
            let peak = samples
                .iter()
                .skip(4_800)
                .fold(0f32, |peak, sample| peak.max(sample.abs()));
            (name, 20.0 * f64::from(peak).log10())
        })
        .collect()
}
