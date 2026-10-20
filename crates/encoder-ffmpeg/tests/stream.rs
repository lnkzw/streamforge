//! Stream to an in-process RTMP server and check what arrives, including
//! reconnects and giving up.
#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use encoder_ffmpeg::{
    AudioSettings, OutputInput, ReconnectPolicy, RecorderErrorKind, STOP_GRACE, StreamConfig,
    StreamKey, StreamState, StreamTarget, Streamer, VideoSettings,
};
use media_core::{FrameBuffer, MediaTimestamp, PixelFormat, VideoFrame};
use rtmp_test_server::{ServerOptions, TestServer, free_port};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;
const FPS: u32 = 30;
const RATE: u32 = 48_000;
const KEY: &str = "test-key-SECRET";

fn config(url: &str, reconnect: ReconnectPolicy) -> StreamConfig {
    StreamConfig {
        target: StreamTarget::new(url, StreamKey::new(KEY)).expect("target"),
        video: VideoSettings::h264_live(WIDTH, HEIGHT, FPS, 1_000),
        audio: AudioSettings::aac_live(RATE, 2),
        reconnect,
        io_timeout: Duration::from_secs(2),
    }
}

fn quick_retries(max_attempts: u32) -> ReconnectPolicy {
    ReconnectPolicy {
        max_attempts,
        initial_delay: Duration::from_millis(100),
        max_delay: Duration::from_millis(400),
    }
}

/// Connect, retrying while the test server is not listening yet (refused
/// connections leave no trace on the server).
fn start(config: &StreamConfig) -> Streamer {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match Streamer::start(config.clone()) {
            Ok(streamer) => return streamer,
            Err(err) if err.kind == RecorderErrorKind::Network && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => panic!("cannot start streaming: {err:?}"),
        }
    }
}

/// Feeds a gray picture and silence in real time, as live producers would.
struct Feeder {
    input: OutputInput,
    started: Instant,
    frames: i64,
    blocks: i64,
}

impl Feeder {
    fn new(input: OutputInput) -> Self {
        Self {
            input,
            started: Instant::now(),
            frames: 0,
            blocks: 0,
        }
    }

    fn picture() -> VideoFrame {
        let stride = WIDTH as usize * 4;
        let buffer = FrameBuffer::unpooled(vec![128u8; stride * HEIGHT as usize]);
        VideoFrame::new(WIDTH, HEIGHT, stride, PixelFormat::Bgra8, None, buffer).expect("frame")
    }

    fn tick(&mut self) {
        let now = self.started.elapsed().as_secs_f64();
        while self.blocks as f64 * 0.01 <= now {
            let pts = MediaTimestamp::new(self.blocks * 480, 1, RATE).expect("pts");
            self.input.send_audio(&[0.0; 960], pts);
            self.blocks += 1;
        }
        while self.frames as f64 / f64::from(FPS) <= now {
            let pts = MediaTimestamp::new(self.frames, 1, FPS).expect("pts");
            self.input.send_video(Self::picture(), pts);
            self.frames += 1;
        }
    }

    /// Feed until `done` holds or `max` elapsed; returns whether `done` held.
    fn run_until(&mut self, mut done: impl FnMut() -> bool, max: Duration) -> bool {
        let end = Instant::now() + max;
        while Instant::now() < end {
            if done() {
                return true;
            }
            self.tick();
            thread::sleep(Duration::from_millis(5));
        }
        done()
    }
}

#[test]
fn streams_to_a_local_server() {
    let server = TestServer::start(ServerOptions::default());
    let streamer = start(&config(&server.url(), quick_retries(0)));
    assert_eq!(streamer.state(), StreamState::Connected);
    assert!(!streamer.target().contains("SECRET"));
    let mut feeder = Feeder::new(streamer.input());
    feeder.run_until(|| false, Duration::from_secs(2));
    let stats = streamer.stop().expect("stop");
    assert_eq!(stats.state, StreamState::Connected, "{stats:?}");
    assert_eq!((stats.connects, stats.disconnects), (1, 0));
    assert_eq!(stats.video.frames_dropped, 0, "{stats:?}");
    assert!(stats.video.frames_encoded >= 55, "{stats:?}");
    assert!(!format!("{stats:?}").contains("SECRET"));

    let report = server.join();
    assert_eq!(report.error, None);
    assert_eq!((report.width, report.height), (WIDTH, HEIGHT));
    assert!(report.first_video_is_key);
    let video = report.video.expect("video arrived");
    let audio = report.audio.expect("audio arrived");
    assert_eq!(
        (video.codec.as_str(), audio.codec.as_str()),
        ("h264", "aac")
    );
    // B-frames delay the first presentation by up to two frames: FLV forbids
    // negative decode timestamps, so the muxer shifts all tracks equally.
    assert!(video.first_s.abs() < 0.1, "{video:?}");
    let span = video.end_s - video.first_s;
    assert!((span - 2.0).abs() < 0.3, "video span {span}");
    assert!(
        (video.end_s - audio.end_s).abs() < 0.1,
        "{video:?} {audio:?}"
    );
}

#[test]
fn reconnects_after_the_server_drops_the_connection() {
    let port = free_port();
    let first = TestServer::start_on(
        port,
        ServerOptions {
            close_after: Some(Duration::from_secs(1)),
            ..ServerOptions::default()
        },
    );
    let url = first.url();
    // A second ingest comes up on the same port once the first one is gone.
    let (second_tx, second_rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = first.join();
        let second = TestServer::start_on(port, ServerOptions::default());
        let _ = second_tx.send(second.join());
    });

    let streamer = start(&config(&url, quick_retries(30)));
    let mut feeder = Feeder::new(streamer.input());
    let mut seen_reconnecting = false;
    let reconnected = feeder.run_until(
        || {
            match streamer.state() {
                StreamState::Reconnecting => seen_reconnecting = true,
                StreamState::Connected if seen_reconnecting => return true,
                _ => {}
            }
            false
        },
        Duration::from_secs(15),
    );
    assert!(reconnected, "state {:?}", streamer.state());
    feeder.run_until(|| false, Duration::from_secs(1));
    let stats = streamer.stop().expect("stop");
    assert_eq!(stats.state, StreamState::Connected, "{stats:?}");
    assert_eq!((stats.connects, stats.disconnects), (2, 1), "{stats:?}");
    assert!(stats.reconnect_attempts >= 1);
    // Frames inside the encoder when the connection broke are counted.
    assert!(stats.frames_undelivered > 0, "{stats:?}");
    assert!(
        stats
            .last_error
            .as_deref()
            .is_some_and(|e| !e.contains("SECRET"))
    );

    let second = second_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("second server report");
    assert_eq!(second.error, None);
    // A fresh connection starts with a keyframe at time zero (plus the
    // B-frame delay, see above).
    assert!(second.first_video_is_key);
    let video = second.video.expect("video after reconnect");
    assert!(video.first_s.abs() < 0.1, "{video:?}");
    assert!(video.packets >= 15, "{video:?}");
}

#[test]
fn gives_up_after_the_reconnect_attempts() {
    let server = TestServer::start(ServerOptions {
        close_after: Some(Duration::from_millis(500)),
        ..ServerOptions::default()
    });
    let streamer = start(&config(&server.url(), quick_retries(2)));
    let mut feeder = Feeder::new(streamer.input());
    let failed = feeder.run_until(
        || streamer.state() == StreamState::Failed,
        Duration::from_secs(15),
    );
    assert!(failed, "state {:?}", streamer.state());
    feeder.run_until(|| false, Duration::from_millis(300));
    let stopping = Instant::now();
    let stats = streamer.stop().expect("stop");
    assert!(
        stopping.elapsed() < Duration::from_secs(1),
        "stop took {:?}",
        stopping.elapsed()
    );
    assert_eq!(stats.state, StreamState::Failed);
    assert_eq!((stats.connects, stats.disconnects), (1, 1));
    assert_eq!(stats.reconnect_attempts, 2);
    // Media after the failure is counted, not silently lost.
    assert!(stats.frames_dropped_offline > 0 && stats.blocks_dropped_offline > 0);
    assert!(stats.video.frames_dropped >= stats.frames_dropped_offline);
    let _ = server.join();
}

#[test]
fn unreachable_server_is_a_network_error_without_the_key() {
    let url = format!("rtmp://127.0.0.1:{}/live", free_port());
    let err = Streamer::start(config(&url, quick_retries(0)))
        .err()
        .expect("must fail");
    assert_eq!(err.kind, RecorderErrorKind::Network);
    assert!(!err.message.contains("SECRET"), "{}", err.message);
    assert!(err.message.contains("<stream key>"), "{}", err.message);
}

#[test]
fn silent_peer_times_out_instead_of_hanging() {
    // Accepts TCP but never speaks RTMP: the handshake must hit the I/O
    // timeout (2 s) rather than block forever.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("rtmp://{}/live", listener.local_addr().expect("addr"));
    let holder = thread::spawn(move || {
        let connection = listener.accept();
        thread::sleep(Duration::from_secs(4));
        drop(connection);
    });
    let started = Instant::now();
    let err = Streamer::start(config(&url, quick_retries(0)))
        .err()
        .expect("must fail");
    let waited = started.elapsed();
    assert_eq!(err.kind, RecorderErrorKind::Network, "{err:?}");
    assert!(
        waited >= Duration::from_millis(1500) && waited < Duration::from_secs(5),
        "gave up after {waited:?}"
    );
    let _ = holder.join();
}

#[test]
fn a_server_that_keeps_dropping_the_connection_ends_in_failed() {
    // Accepts every connection and drops it at once: each reconnect
    // succeeds, so only counting across short-lived connections ends it.
    let port = free_port();
    let first = TestServer::start_on(
        port,
        ServerOptions {
            close_after: Some(Duration::from_millis(200)),
            ..ServerOptions::default()
        },
    );
    let url = first.url();
    let stop = Arc::new(AtomicBool::new(false));
    let flapping = thread::spawn({
        let stop = Arc::clone(&stop);
        move || {
            let _ = first.join();
            while !stop.load(Ordering::Relaxed) {
                let next = TestServer::start_on(
                    port,
                    ServerOptions {
                        accept_timeout: Duration::from_secs(2),
                        close_after: Some(Duration::from_millis(200)),
                        ..ServerOptions::default()
                    },
                );
                let _ = next.join();
            }
        }
    });
    let streamer = start(&config(&url, quick_retries(3)));
    let mut feeder = Feeder::new(streamer.input());
    let failed = feeder.run_until(
        || streamer.state() == StreamState::Failed,
        Duration::from_secs(20),
    );
    stop.store(true, Ordering::Relaxed);
    assert!(failed, "state {:?}", streamer.state());
    let stats = streamer.stop().expect("stop");
    assert_eq!(stats.state, StreamState::Failed);
    assert!(stats.connects >= 2, "{stats:?}");
    assert_eq!(stats.connects, stats.disconnects, "{stats:?}");
    let _ = flapping.join();
}

#[test]
fn stop_is_bounded_while_a_reconnect_hangs() {
    // The ingest goes away; on its port a silent peer accepts TCP but never
    // answers, so the reconnect handshake would block for the full I/O
    // timeout (10 s here). Stopping must not wait for it.
    let port = free_port();
    let first = TestServer::start_on(
        port,
        ServerOptions {
            close_after: Some(Duration::from_millis(500)),
            ..ServerOptions::default()
        },
    );
    let mut config = config(&first.url(), quick_retries(5));
    config.io_timeout = Duration::from_secs(10);
    let (bound_tx, bound_rx) = mpsc::channel();
    let done = Arc::new(AtomicBool::new(false));
    let silent = thread::spawn({
        let done = Arc::clone(&done);
        move || {
            let _ = first.join();
            let listener = std::net::TcpListener::bind(("127.0.0.1", port)).expect("rebind");
            let _ = bound_tx.send(());
            listener.set_nonblocking(true).expect("nonblocking");
            let mut held = Vec::new();
            while !done.load(Ordering::Relaxed) {
                if let Ok((connection, _)) = listener.accept() {
                    held.push(connection);
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    });
    let streamer = start(&config);
    let mut feeder = Feeder::new(streamer.input());
    let lost = feeder.run_until(
        || streamer.state() == StreamState::Reconnecting,
        Duration::from_secs(10),
    );
    assert!(lost, "state {:?}", streamer.state());
    bound_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("silent peer listening");
    // Let the reconnect attempt start and block in the handshake.
    feeder.run_until(|| false, Duration::from_millis(700));
    let stopping = Instant::now();
    let stats = streamer.stop().expect("stop");
    let took = stopping.elapsed();
    assert!(
        took < STOP_GRACE + Duration::from_secs(1),
        "stop took {took:?}"
    );
    done.store(true, Ordering::Relaxed);
    assert_eq!(stats.disconnects, 1, "{stats:?}");
    let _ = silent.join();
}
