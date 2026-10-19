//! Recording session: compositor and mixer feed the recorder until stopped.
//!
//! Timestamps: both producers report the nominal time of each frame/block
//! on the monotonic clock; the taps convert it to media time relative to
//! the session epoch `t0`, so audio and video share one timeline.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use audio_engine::{BlockSink, CHANNELS, MixerConfig, MixerThread, RATE};
use capture_api::{AudioStream, CaptureBackend, VideoStream};
use command_api::{
    CommandError, ErrorCode, ErrorSummary, LiveInputSpec, RecordingReport, RecordingRequest,
    RecordingStarted,
};
use encoder_ffmpeg::{
    AudioSettings, Recorder, RecorderConfig, RecorderError, RecorderErrorKind, RecorderInput,
    VideoSettings,
};
use media_core::{FramePool, VideoFrame};
use serde_json::{Map, Value, json};
use video_compositor::{Compositor, FrameSink};

use crate::composite::{CANVAS_HEIGHT, CANVAS_WIDTH, FPS, reference_scene};
use crate::{command_error, mix};

/// Copies of rendered frames waiting for the encoder; beyond this, frames
/// are dropped and counted instead of growing memory.
const VIDEO_COPY_BUFFERS: usize = 8;
/// Sizes the mixer's latency trend buffer (one entry per 10 s).
const EXPECTED_MAX_DURATION: Duration = Duration::from_secs(4 * 60 * 60);

/// A running recording. Field order is drop order: producers stop first,
/// then the recorder finalizes the file, then captures end.
pub(crate) struct RecordingSession {
    compositor: Compositor,
    mixer: MixerThread,
    recorder: Recorder,
    screen: Option<Box<dyn VideoStream>>,
    audio: Vec<Box<dyn AudioStream>>,
    specs: Vec<LiveInputSpec>,
    path: PathBuf,
}

impl RecordingSession {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn start(
        capture: &mut dyn CaptureBackend,
        output_dir: &Path,
        request: &RecordingRequest,
    ) -> Result<(Self, RecordingStarted), CommandError> {
        let path = output_dir.join(&request.file_name);
        // Reserve the name; fails if it exists, so nothing is overwritten.
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|err| {
                CommandError::new(
                    ErrorCode::InvalidParams,
                    format!(
                        "cannot create {}: {err}; choose another file_name",
                        path.display()
                    ),
                )
                .with_details(json!({ "path": path.display().to_string() }))
            })?;
        let started = Self::start_reserved(capture, path.clone(), request);
        if started.is_err() {
            // Only our just-created (empty or header-only) file is removed.
            let _ = fs::remove_file(&path);
        }
        started
    }

    fn start_reserved(
        capture: &mut dyn CaptureBackend,
        path: PathBuf,
        request: &RecordingRequest,
    ) -> Result<(Self, RecordingStarted), CommandError> {
        let screen = match &request.screen {
            Some(selection) => Some(
                capture
                    .start_screen_stream(selection)
                    .map_err(command_error)?,
            ),
            None => None,
        };
        let audio = mix::start_live_inputs(capture, &request.inputs)?;
        let recorder = Recorder::start(RecorderConfig {
            path: path.clone(),
            video: VideoSettings::h264(CANVAS_WIDTH, CANVAS_HEIGHT, FPS),
            audio: AudioSettings::aac(RATE, CHANNELS),
        })
        .map_err(recorder_error)?;

        let t0 = Instant::now();
        let mut scene = reference_scene(screen.as_deref()).map_err(internal)?;
        scene.sink = Some(Box::new(VideoTap {
            input: recorder.input(),
            pool: FramePool::new(VIDEO_COPY_BUFFERS),
            t0,
        }));
        let compositor = Compositor::spawn(scene).map_err(internal)?;
        let mixer = MixerThread::spawn(MixerConfig {
            inputs: mix::mixer_inputs(&request.inputs, &audio, &request.tones),
            master_gain_db: request.master_gain_db,
            expected_duration: EXPECTED_MAX_DURATION,
            sink: Some(Box::new(AudioTap {
                input: recorder.input(),
                t0,
            })),
        })
        .map_err(internal)?;

        let started = RecordingStarted {
            path: display_path(&path),
            width: CANVAS_WIDTH,
            height: CANVAS_HEIGHT,
            fps: FPS,
            sample_rate: RATE,
            channels: CHANNELS,
        };
        let session = Self {
            compositor,
            mixer,
            recorder,
            screen,
            audio,
            specs: request.inputs.clone(),
            path,
        };
        Ok((session, started))
    }

    /// Stop producers, finalize the file, end captures; report everything.
    pub fn stop(self) -> Result<RecordingReport, CommandError> {
        let Self {
            compositor,
            mixer,
            recorder,
            screen,
            audio,
            specs,
            path: _,
        } = self;
        let mut warnings = Vec::new();
        let render = compositor
            .stop()
            .map(|run| run.stats)
            .map_err(|err| warnings.push(format!("compositor failed: {err}")))
            .ok();
        let mix = mixer
            .stop()
            .map_err(|err| warnings.push(format!("mixer failed: {err}")))
            .ok();
        let recording = recorder.stop();
        let (screen_capture, screen_capture_error) = match screen.map(|stream| stream.stop()) {
            Some(Ok(report)) => (Some(report), None),
            Some(Err(error)) => (None, Some(ErrorSummary::from(error))),
            None => (None, None),
        };
        let audio_captures = mix::stop_captures(&specs, audio);

        if let Some(render) = &render
            && (render.late_frames > 0 || render.skipped_ticks > 0)
        {
            warnings.push(format!(
                "compositor missed its deadline: {} late frame(s), {} skipped tick(s)",
                render.late_frames, render.skipped_ticks
            ));
        }
        if let Some(mix) = &mix {
            warnings.extend(mix::mix_warnings(mix));
        }
        let recording = match recording {
            Ok(stats) => stats,
            Err(error) => {
                let mut details = Map::new();
                if let Some(partial) = &error.partial {
                    details.insert("recording".into(), to_value(partial));
                }
                details.insert("audio_captures".into(), to_value(&audio_captures));
                if let Some(report) = &screen_capture {
                    details.insert("screen_capture".into(), to_value(report));
                }
                if let Some(summary) = &screen_capture_error {
                    details.insert("screen_capture_error".into(), to_value(summary));
                }
                details.insert("warnings".into(), to_value(&warnings));
                return Err(recorder_error(error).with_details(Value::Object(details)));
            }
        };
        if recording.video.frames_dropped > 0 || recording.audio.blocks_dropped > 0 {
            warnings.push(format!(
                "recorder dropped {} video frame(s) and {} audio block(s); the encoder fell behind",
                recording.video.frames_dropped, recording.audio.blocks_dropped
            ));
        }
        if recording.audio.gap_samples > 0 || recording.audio.discontinuities > 0 {
            warnings.push(format!(
                "audio timeline: {} sample(s) of silence inserted, {} discontinuities",
                recording.audio.gap_samples, recording.audio.discontinuities
            ));
        }
        Ok(RecordingReport {
            recording,
            render,
            mix,
            screen_capture,
            screen_capture_error,
            audio_captures,
            warnings,
        })
    }
}

/// Copies each rendered frame into a pooled buffer for the recorder.
struct VideoTap {
    input: RecorderInput,
    pool: FramePool,
    t0: Instant,
}

impl FrameSink for VideoTap {
    fn frame(&mut self, frame: &VideoFrame, presented_at: Instant) {
        let Some(mut buffer) = self.pool.acquire(frame.data().len()) else {
            self.input.count_dropped_video();
            return;
        };
        buffer.copy_from_slice(frame.data());
        let copy = VideoFrame::new(
            frame.width(),
            frame.height(),
            frame.stride(),
            frame.format(),
            None,
            buffer,
        );
        match copy {
            Ok(copy) => {
                let pts_us =
                    i64::try_from(presented_at.saturating_duration_since(self.t0).as_micros())
                        .unwrap_or(i64::MAX);
                self.input.send_video(copy, pts_us);
            }
            Err(_) => self.input.count_dropped_video(),
        }
    }
}

/// Forwards each mixed block with its sample-frame timestamp.
struct AudioTap {
    input: RecorderInput,
    t0: Instant,
}

impl BlockSink for AudioTap {
    fn block(&mut self, samples: &[f32], at: Instant) {
        let nanos = at.saturating_duration_since(self.t0).as_nanos();
        let pts = (nanos * u128::from(RATE) + 500_000_000) / 1_000_000_000;
        self.input
            .send_audio(samples, i64::try_from(pts).unwrap_or(i64::MAX));
    }
}

fn recorder_error(error: RecorderError) -> CommandError {
    let code = match error.kind {
        RecorderErrorKind::Unsupported => ErrorCode::Unsupported,
        RecorderErrorKind::Output | RecorderErrorKind::Encoder => ErrorCode::BackendError,
        RecorderErrorKind::Internal => ErrorCode::Internal,
    };
    CommandError::new(code, format!("recording failed: {}", error.message))
}

fn display_path(path: &Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_owned())
        .display()
        .to_string()
}

fn to_value(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or_else(|err| Value::String(err.to_string()))
}

fn internal(error: impl std::fmt::Display) -> CommandError {
    CommandError::new(ErrorCode::Internal, format!("recording failed: {error}"))
}
