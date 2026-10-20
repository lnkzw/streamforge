//! M5 acceptance: record through the real engine process, then verify the
//! file's streams, duration, A/V timestamp drift and decoded content.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::EngineProcess;
use ffmpeg_next as ff;
use serde_json::Value;

const RECORD_FOR: Duration = Duration::from_millis(2_000);

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "streamforge-recording-{}-{name}",
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

/// Send requests one by one (waiting, bounded, for each response) to a fresh
/// engine, then close stdin and wait for a clean exit.
fn session(output_dir: &Path, steps: &[(&str, Option<Duration>)]) -> Vec<Value> {
    let mut engine =
        EngineProcess::spawn_with(&[std::ffi::OsStr::new("--output-dir"), output_dir.as_os_str()]);
    let mut responses = Vec::new();
    for (request, wait_after) in steps {
        responses.push(engine.request(request));
        if let Some(wait) = wait_after {
            std::thread::sleep(*wait);
        }
    }
    assert!(engine.finish().success());
    responses
}

#[test]
fn recorded_file_has_synchronized_streams_and_expected_content() {
    let dir = TempDir::new("acceptance");
    let responses = session(
        &dir.0,
        &[
            (
                r#"{"id":1,"command":"start_recording","params":{"file_name":"accept.mkv","tones":[{"frequency_hz":1000,"level_dbfs":-12}]}}"#,
                Some(RECORD_FOR),
            ),
            (r#"{"id":2,"command":"get_status"}"#, None),
            (r#"{"id":3,"command":"stop_recording"}"#, None),
        ],
    );
    assert_eq!(responses[0]["ok"], true, "{}", responses[0]);
    assert_eq!(responses[1]["result"]["recording_active"], true);
    assert_eq!(responses[2]["ok"], true, "{}", responses[2]);
    let report = &responses[2]["result"]["recording"];
    // Shared CI runners may not sustain 1080p60 x264, so drops are allowed,
    // but none may be silent: every tick before the end was encoded, counted
    // as dropped, or skipped (and counted) by the compositor.
    let count = |value: &Value| value.as_u64().unwrap_or_else(|| panic!("{report}"));
    let encoded = count(&report["video"]["frames_encoded"]);
    let dropped = count(&report["video"]["frames_dropped"]);
    let skipped = count(&responses[2]["result"]["render"]["skipped_ticks"]);
    // Ticks from the output's first frame (its epoch starts at attach, so
    // the first frame may be a tick later) to the cut-off, which the audio
    // end marks (audio has no drops; trailing video frames may be dropped).
    // Skipped ticks cover the whole pipeline run (they can exceed the
    // output's share), so the check is one-sided: nothing silently lost,
    // nothing invented.
    let first_ms = report["video"]["first_pts_ms"]
        .as_f64()
        .expect("video start");
    let end_ms = report["audio"]["end_ms"].as_f64().expect("audio end");
    let ticks = ((end_ms - first_ms) * 60.0 / 1000.0).round() as u64;
    assert!(
        encoded + dropped + skipped + 2 >= ticks,
        "{encoded} encoded + {dropped} dropped + {skipped} skipped < {ticks} ticks: {report}"
    );
    assert!(
        encoded + dropped <= ticks + 1,
        "more frames than ticks: {report}"
    );
    // A throughput floor: three quarters of the ticks. The 3-core macOS CI
    // VM skips about a third of the compositor ticks at 1080p60 (counted
    // above), so there it only guards against near-total failure;
    // throughput is measured separately (docs/testing.md).
    let (num, den) = if cfg!(target_os = "macos") {
        (1, 3)
    } else {
        (3, 4)
    };
    assert!(
        encoded * den >= ticks * num,
        "fewer than {num}/{den} of the frames encoded: {report}"
    );
    assert_eq!(report["audio"]["blocks_dropped"], 0, "{report}");
    assert_eq!(report["audio"]["gap_samples"], 0, "{report}");

    let path = dir.0.join("accept.mkv");
    ff::init().expect("init");
    let mut input = ff::format::input(&path).expect("open recording");

    // Stream metadata.
    let video_stream = input.streams().best(ff::media::Type::Video).expect("video");
    let audio_stream = input.streams().best(ff::media::Type::Audio).expect("audio");
    assert_eq!(video_stream.parameters().id(), ff::codec::Id::H264);
    assert_eq!(audio_stream.parameters().id(), ff::codec::Id::AAC);
    // Real base frame rate; Matroska's millisecond timestamps make the
    // estimated average frame rate inexact (e.g. 125/2).
    assert_eq!(video_stream.rate(), ff::Rational::new(60, 1));
    let (video_index, audio_index) = (video_stream.index(), audio_stream.index());
    let (video_tb, audio_tb) = (video_stream.time_base(), audio_stream.time_base());
    let mut video = ff::codec::context::Context::from_parameters(video_stream.parameters())
        .expect("ctx")
        .decoder()
        .video()
        .expect("decoder");
    let mut audio = ff::codec::context::Context::from_parameters(audio_stream.parameters())
        .expect("ctx")
        .decoder()
        .audio()
        .expect("decoder");
    assert_eq!((video.width(), video.height()), (1920, 1080));
    assert_eq!(video.color_space(), ff::util::color::Space::SMPTE170M);
    assert_eq!(video.color_range(), ff::util::color::Range::MPEG);
    assert_eq!(audio.rate(), 48_000);
    assert_eq!(audio.channel_layout().channels(), 2);

    // Duration: the container covers what was recorded, at least the
    // requested time and up to the recorder's own cut-off. (On a loaded
    // runner `stop_recording` can arrive half a second late, which makes
    // the file longer than `RECORD_FOR`; the cut-off accounts for that.)
    let seconds = input.duration() as f64 / f64::from(ff::ffi::AV_TIME_BASE);
    let cut_off = end_ms / 1000.0;
    assert!(
        seconds > RECORD_FOR.as_secs_f64() - 0.3,
        "duration {seconds} s"
    );
    assert!(
        (seconds - cut_off).abs() < 0.15,
        "duration {seconds} s, cut-off at {cut_off} s"
    );

    // Timestamps, A/V drift and decoded content.
    let mut ends = [f64::MIN; 2];
    let mut starts = [f64::MAX; 2];
    let mut first_picture_checked = false;
    let (mut sum_squares, mut samples_seen) = (0f64, 0u64);
    let mut picture = ff::frame::Video::empty();
    let mut sound = ff::frame::Audio::empty();
    for (stream, packet) in input.packets() {
        let Some(pts) = packet.pts() else { continue };
        if stream.index() == video_index {
            starts[0] = starts[0].min(pts as f64 * f64::from(video_tb));
            ends[0] = ends[0].max((pts + packet.duration()) as f64 * f64::from(video_tb));
            if !first_picture_checked {
                video.send_packet(&packet).expect("send");
                if video.receive_frame(&mut picture).is_ok() {
                    check_color_bars(&picture);
                    first_picture_checked = true;
                }
            }
        } else if stream.index() == audio_index {
            starts[1] = starts[1].min(pts as f64 * f64::from(audio_tb));
            ends[1] = ends[1].max((pts + packet.duration()) as f64 * f64::from(audio_tb));
            audio.send_packet(&packet).expect("send");
            while audio.receive_frame(&mut sound).is_ok() {
                for plane in 0..sound.planes() {
                    for s in &sound.plane::<f32>(plane)[..sound.samples()] {
                        sum_squares += f64::from(*s) * f64::from(*s);
                        samples_seen += 1;
                    }
                }
            }
        }
    }
    assert!(first_picture_checked, "no video frame decoded");
    // Dropped trailing frames shorten the video track; allow exactly them.
    let drift = (ends[0] - ends[1]).abs();
    let allowed = 0.05 + dropped as f64 / 60.0;
    assert!(
        drift < allowed,
        "A/V end drift {drift:.3} s > {allowed:.3} s (video {:.3}, audio {:.3})",
        ends[0],
        ends[1]
    );
    // Starts are not affected by drops at the end, but a loaded runner's
    // compositor may skip (and count) the first ticks, so video may start
    // up to those ticks late; audio has no such excuse.
    let video_late = starts[0] - starts[1];
    let allowed_late = 0.05 + skipped as f64 / 60.0;
    assert!(
        video_late < allowed_late && -video_late < 0.05,
        "A/V start drift {video_late:.3} s (video minus audio; {skipped} ticks skipped)"
    );
    let rms_db = 10.0 * (sum_squares / samples_seen as f64).log10();
    assert!((rms_db + 15.05).abs() < 0.6, "audio rms {rms_db:.2} dBFS");
}

/// The reference scene starts with eight color bars; sample their centers.
fn check_color_bars(picture: &ff::frame::Video) {
    let mut rgb = ff::frame::Video::empty();
    ff::software::scaling::Context::get(
        picture.format(),
        picture.width(),
        picture.height(),
        ff::format::Pixel::RGB24,
        picture.width(),
        picture.height(),
        ff::software::scaling::Flags::BILINEAR,
    )
    .expect("scaler")
    .run(picture, &mut rgb)
    .expect("convert");
    let stride = rgb.stride(0);
    let data = rgb.data(0);
    // White, yellow, cyan, green, magenta, red, blue, black (RGB).
    let expected: [[u8; 3]; 8] = [
        [255, 255, 255],
        [255, 255, 0],
        [0, 255, 255],
        [0, 255, 0],
        [255, 0, 255],
        [255, 0, 0],
        [0, 0, 255],
        [0, 0, 0],
    ];
    let y = 600; // below the moving overlay's path
    for (bar, want) in expected.iter().enumerate() {
        let x = bar * 240 + 120;
        let at = y * stride + x * 3;
        let got = [data[at], data[at + 1], data[at + 2]];
        for (g, w) in got.iter().zip(want) {
            // Limited-range YUV 4:2:0 round trip.
            assert!(
                g.abs_diff(*w) <= 24,
                "bar {bar}: got {got:?}, want {want:?}"
            );
        }
    }
}
