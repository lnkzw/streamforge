//! M6 acceptance through the real engine process: stream to a local RTMP
//! server, and keep recording when the stream fails.
#![cfg(target_os = "linux")]

mod common;

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use common::EngineProcess;
use ffmpeg_next as ff;
use rtmp_test_server::{ServerOptions, TestServer};
use serde_json::{Value, json};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "streamforge-streaming-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn spawn(dir: &TempDir) -> EngineProcess {
    EngineProcess::spawn_with(&[std::ffi::OsStr::new("--output-dir"), dir.0.as_os_str()])
}

fn request(engine: &mut EngineProcess, id: u64, command: &str, params: Value) -> Value {
    let line = json!({"id": id, "command": command, "params": params}).to_string();
    engine.request(&line)
}

/// `start_streaming`, retrying while the test server is not listening yet.
fn start_streaming(engine: &mut EngineProcess, params: Value) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    for id in 100.. {
        let response = request(engine, id, "start_streaming", params.clone());
        let unavailable = response["error"]["code"] == "unavailable";
        if !unavailable || Instant::now() >= deadline {
            return response;
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
    let dir = TempDir::new("stream");
    let server = TestServer::start(ServerOptions::default());
    let mut engine = spawn(&dir);
    let started = start_streaming(
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
    assert!((span - 2.0).abs() < 0.5, "video span {span} s");
    assert!(
        (video.end_s - audio.end_s).abs() < 0.15,
        "{video:?} {audio:?}"
    );
}

#[test]
fn recording_continues_when_the_stream_fails() {
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
    let streaming = start_streaming(
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
