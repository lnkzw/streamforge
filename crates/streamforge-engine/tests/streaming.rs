//! M6 acceptance through the real engine process: stream to a local RTMP
//! server, and keep recording when the stream fails.

mod common;

use std::thread;
use std::time::{Duration, Instant};

use common::{EngineProcess, TempDir};
use ffmpeg_next as ff;
use rtmp_test_server::{ServerOptions, TestServer};
use serde_json::{Value, json};

fn spawn(dir: &TempDir) -> EngineProcess {
    EngineProcess::spawn_with(&[std::ffi::OsStr::new("--output-dir"), dir.0.as_os_str()])
}

fn request(engine: &mut EngineProcess, id: u64, command: &str, params: Value) -> Value {
    let line = json!({"id": id, "command": command, "params": params}).to_string();
    engine.request(&line)
}

/// `start_streaming`, retrying while the test server is not listening yet;
/// also returns when the last attempt was sent.
fn start_streaming(engine: &mut EngineProcess, params: Value) -> (Value, Instant) {
    let deadline = Instant::now() + Duration::from_secs(10);
    for id in 100.. {
        let sent = Instant::now();
        let response = request(engine, id, "start_streaming", params.clone());
        let unavailable = response["error"]["code"] == "unavailable";
        if !unavailable || Instant::now() >= deadline {
            return (response, sent);
        }
        thread::sleep(Duration::from_millis(50));
    }
    unreachable!()
}

fn get_status(engine: &mut EngineProcess) -> Value {
    request(engine, 1, "get_status", json!({}))["result"].clone()
}

#[test]
fn streams_to_a_local_server() {
    let _media = common::media_lock();
    let dir = TempDir::new("stream");
    let server = TestServer::start(ServerOptions::default());
    let mut engine = spawn(&dir);
    let (started, begun) = start_streaming(
        &mut engine,
        json!({"url": server.url(), "stream_key": "acceptance-SECRET", "video_bitrate_kbps": 2500,
               "tones": [{"frequency_hz": 1000, "level_dbfs": -12}]}),
    );
    assert_eq!(started["ok"], true, "{started}");
    assert!(!started.to_string().contains("SECRET"));
    thread::sleep(Duration::from_secs(2));
    let status = get_status(&mut engine);
    assert_eq!(status["streaming_active"], true, "{status}");
    assert_eq!(status["streaming_state"], "connected", "{status}");
    let stopped = request(&mut engine, 2, "stop_streaming", json!({}));
    let wall = begun.elapsed().as_secs_f64();
    assert_eq!(stopped["ok"], true, "{stopped}");
    assert!(!stopped.to_string().contains("SECRET"));
    let report = &stopped["result"];
    assert_eq!(report["streaming"]["state"], "connected", "{report}");
    assert_eq!(report["streaming"]["connects"], 1);
    assert_eq!(report["pipeline_stopped"], true);
    assert!(engine.finish().success());

    let arrived = server.join();
    assert_eq!(arrived.error, None);
    assert_eq!((arrived.width, arrived.height), (1920, 1080));
    assert!(arrived.first_video_is_key);
    let video = arrived.video.expect("video");
    let audio = arrived.audio.expect("audio");
    assert_eq!(
        (video.codec.as_str(), audio.codec.as_str()),
        ("h264", "aac")
    );
    let span = video.end_s - video.first_s;
    let sent = &report["streaming"]["video"];
    println!("{sent}");
    // Frames the encoder did not keep up with are dropped and counted
    // (`frames_dropped`), and frames still in the encoder when the stop
    // flush runs out of time are never delivered (`frames_undelivered`);
    // trailing ones of either end the video early, never late. (Skipped
    // compositor ticks leave gaps; they do not shorten the video.)
    let fps = sent["fps"].as_f64().expect("fps");
    let short_frames = sent["frames_dropped"].as_u64().unwrap_or(0)
        + report["streaming"]["frames_undelivered"]
            .as_u64()
            .unwrap_or(0);
    let allowed_short = 0.15 + short_frames as f64 / fps;
    let video_short = audio.end_s - video.end_s;
    // The video covers the 2 s sleep, less what was dropped, and no more
    // than the time from the successful start to the stop reply: a loaded
    // runner can take seconds to answer `get_status` and stop.
    assert!(
        span + short_frames as f64 / fps > 1.5 && span < wall + 0.1,
        "video span {span} s in {wall:.3} s: {sent}"
    );
    assert!(
        video_short < allowed_short && -video_short < 0.15,
        "{video:?} {audio:?}; sent: {sent}"
    );
}

#[test]
fn recording_continues_when_the_stream_fails() {
    let _media = common::media_lock();
    let dir = TempDir::new("survive");
    let server = TestServer::start(ServerOptions {
        close_after: Some(Duration::from_millis(800)),
        ..ServerOptions::default()
    });
    let mut engine = spawn(&dir);
    let recording = request(
        &mut engine,
        1,
        "start_recording",
        json!({"file_name": "survivor.mkv", "tones": [{"frequency_hz": 440, "level_dbfs": -12}]}),
    );
    assert_eq!(recording["ok"], true, "{recording}");
    let recording_started = Instant::now();
    let (streaming, _) = start_streaming(
        &mut engine,
        json!({"url": server.url(), "reconnect_attempts": 1, "reconnect_delay_ms": 100,
               "video_bitrate_kbps": 2000}),
    );
    assert_eq!(streaming["ok"], true, "{streaming}");

    // The ingest drops the connection and never comes back.
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        let status = get_status(&mut engine);
        if status["streaming_state"] == "failed" {
            break status;
        }
        assert!(Instant::now() < deadline, "stream never failed: {status}");
        thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(status["streaming_active"], false);
    assert_eq!(status["recording_active"], true, "{status}");
    let stream = request(&mut engine, 2, "stop_streaming", json!({}));
    assert_eq!(stream["result"]["streaming"]["state"], "failed", "{stream}");
    assert_eq!(stream["result"]["pipeline_stopped"], false);

    thread::sleep(Duration::from_millis(500));
    let stopped = request(&mut engine, 3, "stop_recording", json!({}));
    let recorded_for = recording_started.elapsed().as_secs_f64();
    assert_eq!(stopped["ok"], true, "{stopped}");
    let report = &stopped["result"]["recording"];
    assert_eq!(stopped["result"]["pipeline_stopped"], true);
    assert_eq!(report["audio"]["blocks_dropped"], 0, "{report}");
    assert_eq!(report["audio"]["gap_samples"], 0, "{report}");
    assert!(engine.finish().success());
    let _ = server.join();

    // The file covers the whole recording, across the stream's failure.
    ff::init().expect("init");
    let input = ff::format::input(&dir.0.join("survivor.mkv")).expect("open recording");
    let seconds = input.duration() as f64 / f64::from(ff::ffi::AV_TIME_BASE);
    assert!(
        seconds > recorded_for - 0.5 && seconds <= recorded_for + 0.2,
        "file lasts {seconds:.2} s, recording ran {recorded_for:.2} s"
    );
}
