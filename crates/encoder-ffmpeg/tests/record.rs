//! Record synthetic media and verify the file by demuxing and decoding it.
#![cfg(target_os = "linux")]

use std::path::PathBuf;

use encoder_ffmpeg::{AudioSettings, Recorder, RecorderConfig, RecorderErrorKind, VideoSettings};
use ffmpeg_next as ff;
use media_core::{FrameBuffer, PixelFormat, VideoFrame};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;
const FPS: u32 = 30;
const RATE: u32 = 48_000;
const SECONDS: u32 = 2;

/// Removes the file when the test ends, even on failure.
struct TempFile(PathBuf);

impl TempFile {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "streamforge-encoder-{}-{name}.mkv",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        Self(path)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Left half red, right half blue (BGRA).
fn picture() -> VideoFrame {
    let mut data = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
    for (i, px) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        *px = if (i as u32 % WIDTH) < WIDTH / 2 {
            [0, 0, 255, 255]
        } else {
            [255, 0, 0, 255]
        };
    }
    VideoFrame::new(
        WIDTH,
        HEIGHT,
        WIDTH as usize * 4,
        PixelFormat::Bgra8,
        None,
        FrameBuffer::unpooled(data),
    )
    .expect("frame")
}

fn config(path: &TempFile) -> RecorderConfig {
    RecorderConfig {
        path: path.0.clone(),
        video: VideoSettings::h264(WIDTH, HEIGHT, FPS),
        audio: AudioSettings::aac(RATE, 2),
    }
}

/// Record `SECONDS` of the picture and a 1 kHz tone at -12 dBFS.
fn record(path: &TempFile) -> encoder_ffmpeg::RecordingStats {
    let recorder = Recorder::start(config(path)).expect("start");
    let input = recorder.input();
    let frame_us = 1_000_000 / i64::from(FPS);
    let mut phase = 0.0f64;
    let amplitude = 10f64.powf(-12.0 / 20.0);
    let mut block = [0.0f32; 960];
    let total_blocks = SECONDS * 100;
    let total_frames = SECONDS * FPS;
    let (mut next_block, mut next_frame) = (0u32, 0u32);
    while next_block < total_blocks || next_frame < total_frames {
        // Interleave in timestamp order, as live producers would.
        let block_us = i64::from(next_block) * 10_000;
        let frame_pts = i64::from(next_frame) * frame_us;
        if next_frame < total_frames && (frame_pts <= block_us || next_block == total_blocks) {
            assert!(input.send_video_blocking(picture(), frame_pts));
            next_frame += 1;
        } else {
            for frame in block.as_chunks_mut::<2>().0 {
                let sample = (amplitude * (std::f64::consts::TAU * phase).sin()) as f32;
                *frame = [sample, sample];
                phase = (phase + 1000.0 / f64::from(RATE)).fract();
            }
            assert!(input.send_audio_blocking(&block, i64::from(next_block) * 480));
            next_block += 1;
        }
    }
    recorder.stop().expect("stop")
}

#[test]
fn recording_has_expected_streams_duration_and_sync() {
    let file = TempFile::new("sync");
    let stats = record(&file);
    assert_eq!(stats.video.frames_encoded, u64::from(SECONDS * FPS));
    assert_eq!(stats.video.frames_dropped, 0);
    assert_eq!(stats.audio.samples_encoded, u64::from(SECONDS * RATE));
    assert_eq!(stats.audio.blocks_dropped, 0);
    assert_eq!(stats.audio.gap_samples, 0);
    assert!(
        (stats.duration_ms - 2000.0).abs() < 1.0,
        "{}",
        stats.duration_ms
    );
    assert!(stats.file_bytes.is_some_and(|bytes| bytes > 1000));

    ff::init().expect("init");
    let input = ff::format::input(&file.0).expect("open recording");
    let video = input
        .streams()
        .best(ff::media::Type::Video)
        .expect("video stream");
    let audio = input
        .streams()
        .best(ff::media::Type::Audio)
        .expect("audio stream");
    assert_eq!(video.parameters().id(), ff::codec::Id::H264);
    assert_eq!(audio.parameters().id(), ff::codec::Id::AAC);
    let decoder = ff::codec::context::Context::from_parameters(video.parameters())
        .expect("ctx")
        .decoder()
        .video()
        .expect("video decoder");
    assert_eq!((decoder.width(), decoder.height()), (WIDTH, HEIGHT));
    let audio_decoder = ff::codec::context::Context::from_parameters(audio.parameters())
        .expect("ctx")
        .decoder()
        .audio()
        .expect("audio decoder");
    assert_eq!(audio_decoder.rate(), RATE);
    assert_eq!(audio_decoder.channel_layout().channels(), 2);

    let duration_s = input.duration() as f64 / f64::from(ff::ffi::AV_TIME_BASE);
    assert!(
        (duration_s - 2.0).abs() < 0.1,
        "container duration {duration_s}"
    );

    // Packet timestamps: both tracks start near 0 and end together.
    let (video_index, audio_index) = (video.index(), audio.index());
    let (video_tb, audio_tb) = (video.time_base(), audio.time_base());
    let mut ends = [f64::MIN; 2];
    let mut starts = [f64::MAX; 2];
    let mut input = input;
    for (stream, packet) in input.packets() {
        let Some(pts) = packet.pts() else { continue };
        let (slot, tb) = if stream.index() == video_index {
            (0, video_tb)
        } else if stream.index() == audio_index {
            (1, audio_tb)
        } else {
            continue;
        };
        let start = pts as f64 * f64::from(tb);
        let end = (pts + packet.duration()) as f64 * f64::from(tb);
        starts[slot] = starts[slot].min(start);
        ends[slot] = ends[slot].max(end);
    }
    assert!(
        starts[0].abs() < 0.05 && starts[1].abs() < 0.05,
        "starts {starts:?}"
    );
    let drift = (ends[0] - ends[1]).abs();
    assert!(drift < 0.05, "A/V end drift {drift} s, ends {ends:?}");
}

#[test]
fn decoded_content_matches_what_was_recorded() {
    let file = TempFile::new("content");
    record(&file);
    ff::init().expect("init");
    let mut input = ff::format::input(&file.0).expect("open");
    let video_index = input
        .streams()
        .best(ff::media::Type::Video)
        .expect("video")
        .index();
    let audio_index = input
        .streams()
        .best(ff::media::Type::Audio)
        .expect("audio")
        .index();
    let mut video = ff::codec::context::Context::from_parameters(
        input.stream(video_index).expect("stream").parameters(),
    )
    .expect("ctx")
    .decoder()
    .video()
    .expect("decoder");
    let mut audio = ff::codec::context::Context::from_parameters(
        input.stream(audio_index).expect("stream").parameters(),
    )
    .expect("ctx")
    .decoder()
    .audio()
    .expect("decoder");

    let mut rgb_checked = false;
    let (mut sum_squares, mut count) = (0f64, 0u64);
    let mut decoded = ff::frame::Video::empty();
    let mut samples = ff::frame::Audio::empty();
    for (stream, packet) in input.packets() {
        if stream.index() == video_index && !rgb_checked {
            video.send_packet(&packet).expect("send");
            if video.receive_frame(&mut decoded).is_ok() {
                let mut rgb = ff::frame::Video::empty();
                let mut scaler = ff::software::scaling::Context::get(
                    decoded.format(),
                    WIDTH,
                    HEIGHT,
                    ff::format::Pixel::RGB24,
                    WIDTH,
                    HEIGHT,
                    ff::software::scaling::Flags::BILINEAR,
                )
                .expect("scaler");
                scaler.run(&decoded, &mut rgb).expect("convert");
                let stride = rgb.stride(0);
                let data = rgb.data(0);
                let pixel = |x: usize, y: usize| {
                    let at = y * stride + x * 3;
                    [data[at], data[at + 1], data[at + 2]]
                };
                let left = pixel(WIDTH as usize / 4, HEIGHT as usize / 2);
                let right = pixel(3 * WIDTH as usize / 4, HEIGHT as usize / 2);
                assert!(
                    left[0] > 200 && left[1] < 60 && left[2] < 60,
                    "left {left:?}"
                );
                assert!(
                    right[0] < 60 && right[1] < 60 && right[2] > 200,
                    "right {right:?}"
                );
                rgb_checked = true;
            }
        } else if stream.index() == audio_index {
            audio.send_packet(&packet).expect("send");
            while audio.receive_frame(&mut samples).is_ok() {
                for plane in 0..samples.planes() {
                    for s in &samples.plane::<f32>(plane)[..samples.samples()] {
                        sum_squares += f64::from(*s) * f64::from(*s);
                        count += 1;
                    }
                }
            }
        }
    }
    assert!(rgb_checked, "no video frame decoded");
    let rms_db = 10.0 * (sum_squares / count as f64).log10();
    // Sine at -12 dBFS peak has an RMS of -15.05 dBFS; AAC is lossy.
    assert!((rms_db + 15.05).abs() < 0.5, "rms {rms_db}");
}

#[test]
fn unwritable_output_is_an_output_error() {
    let mut config = config(&TempFile::new("unused"));
    config.path = PathBuf::from("/nonexistent-directory/streamforge/out.mkv");
    let err = Recorder::start(config).err().expect("must fail");
    assert_eq!(err.kind, RecorderErrorKind::Output);
}
