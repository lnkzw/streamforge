//! The media pipeline shared by all outputs: captures, compositor, mixer and
//! a fan-out to the attached outputs.
//!
//! Outputs attach and detach while the pipeline runs. Each attached output
//! has its own copy pool, queue and epoch, so a stalled output (a slow
//! network) only drops its own frames, never another output's.
//!
//! Timestamps: the producers report the nominal time of each frame/block on
//! the monotonic clock; the fan-out converts it to media time relative to
//! each output's epoch, so audio and video of an output share one timeline.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use audio_engine::{BlockSink, MixerConfig, MixerThread};
use capture_api::{AudioStream, CaptureBackend, VideoStream};
use command_api::{
    CommandError, ErrorCode, ErrorSummary, LiveInputSpec, MediaSources, PipelineReport,
};
use encoder_ffmpeg::OutputInput;
use media_core::{FramePool, MediaTimestamp, VideoFrame};
use video_compositor::{Compositor, FrameSink};

use crate::composite::reference_scene;
use crate::{command_error, mix};

/// Copies of rendered frames per output waiting for its encoder; beyond
/// this, that output's frames are dropped and counted.
const VIDEO_COPY_BUFFERS: usize = 8;
/// Recording and streaming.
const MAX_OUTPUTS: usize = 2;
/// Sizes the mixer's latency trend buffer (one entry per 10 s).
const EXPECTED_MAX_DURATION: Duration = Duration::from_secs(4 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputId {
    Recording,
    Streaming,
}

/// Field order is drop order: producers stop before the captures end.
pub(crate) struct Pipeline {
    compositor: Compositor,
    mixer: MixerThread,
    screen: Option<Box<dyn VideoStream>>,
    audio: Vec<Box<dyn AudioStream>>,
    specs: Vec<LiveInputSpec>,
    fanout: Arc<Fanout>,
}

impl Pipeline {
    pub fn start(
        capture: &mut dyn CaptureBackend,
        sources: &MediaSources,
    ) -> Result<Self, CommandError> {
        let screen = match &sources.screen {
            Some(selection) => Some(
                capture
                    .start_screen_stream(selection)
                    .map_err(command_error)?,
            ),
            None => None,
        };
        let audio = mix::start_live_inputs(capture, &sources.inputs)?;
        let fanout = Arc::new(Fanout::default());
        let mut scene = reference_scene(screen.as_deref()).map_err(internal)?;
        scene.sink = Some(Box::new(VideoTap(Arc::clone(&fanout))));
        let compositor = Compositor::spawn(scene).map_err(internal)?;
        let mixer = MixerThread::spawn(MixerConfig {
            inputs: mix::mixer_inputs(&sources.inputs, &audio, &sources.tones),
            master_gain_db: sources.master_gain_db,
            expected_duration: EXPECTED_MAX_DURATION,
            sink: Some(Box::new(AudioTap(Arc::clone(&fanout)))),
        })
        .map_err(internal)?;
        Ok(Self {
            compositor,
            mixer,
            screen,
            audio,
            specs: sources.inputs.clone(),
            fanout,
        })
    }

    /// `false` once the compositor or the mixer has stopped on its own.
    pub fn is_running(&self) -> bool {
        !self.compositor.is_finished() && !self.mixer.is_finished()
    }

    /// Feed `input` from now on; returns the output's epoch (media time 0).
    pub fn attach(&self, id: OutputId, input: OutputInput) -> Instant {
        let t0 = Instant::now();
        lock(&self.fanout.video).push(VideoOut {
            id,
            input: input.clone(),
            pool: FramePool::new(VIDEO_COPY_BUFFERS),
            t0,
        });
        lock(&self.fanout.audio).push(AudioOut { id, input, t0 });
        t0
    }

    pub fn detach(&self, id: OutputId) {
        lock(&self.fanout.video).retain(|out| out.id != id);
        lock(&self.fanout.audio).retain(|out| out.id != id);
    }

    /// Stop producers, then captures; report everything.
    pub fn stop(self) -> (PipelineReport, Vec<String>) {
        let Self {
            compositor,
            mixer,
            screen,
            audio,
            specs,
            fanout: _,
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
        warnings.extend(mix::capture_warnings(&audio_captures));
        if let Some(error) = &screen_capture_error {
            warnings.push(format!("screen capture failed: {}", error.message));
        }
        let report = PipelineReport {
            pipeline_stopped: true,
            render,
            mix,
            screen_capture,
            screen_capture_error,
            audio_captures,
        };
        (report, warnings)
    }
}

/// The attached outputs. The compositor and mixer threads lock their list
/// once per frame/block; the engine thread locks it only to attach or
/// detach (a push or removal, never while waiting on anything), so the
/// producers never wait on I/O. Capacity is reserved up front.
struct Fanout {
    video: Mutex<Vec<VideoOut>>,
    audio: Mutex<Vec<AudioOut>>,
}

impl Default for Fanout {
    fn default() -> Self {
        Self {
            video: Mutex::new(Vec::with_capacity(MAX_OUTPUTS)),
            audio: Mutex::new(Vec::with_capacity(MAX_OUTPUTS)),
        }
    }
}

struct VideoOut {
    id: OutputId,
    input: OutputInput,
    pool: FramePool,
    t0: Instant,
}

struct AudioOut {
    id: OutputId,
    input: OutputInput,
    t0: Instant,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicking producer leaves a consistent list behind.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Copies each rendered frame into a pooled buffer per output.
struct VideoTap(Arc<Fanout>);

impl FrameSink for VideoTap {
    fn frame(&mut self, frame: &VideoFrame, presented_at: Instant) {
        for out in lock(&self.0.video).iter_mut() {
            // Frames from before the output started are not part of it.
            let Some(pts) = since(out.t0, presented_at) else {
                continue;
            };
            let Some(mut buffer) = out.pool.acquire(frame.data().len()) else {
                out.input.count_dropped_video();
                continue;
            };
            buffer.copy_from_slice(frame.data());
            match VideoFrame::new(
                frame.width(),
                frame.height(),
                frame.stride(),
                frame.format(),
                None,
                buffer,
            ) {
                Ok(copy) => {
                    out.input.send_video(copy, pts);
                }
                Err(_) => out.input.count_dropped_video(),
            }
        }
    }
}

/// Forwards each mixed block to every output; does not allocate.
struct AudioTap(Arc<Fanout>);

impl BlockSink for AudioTap {
    fn block(&mut self, samples: &[f32], at: Instant) {
        for out in lock(&self.0.audio).iter() {
            if let Some(pts) = since(out.t0, at) {
                out.input.send_audio(samples, pts);
            }
        }
    }
}

/// Media time of `at` on the timeline starting at `t0`, in nanoseconds;
/// `None` before `t0`. Saturates after about 292 years.
pub(crate) fn since(t0: Instant, at: Instant) -> Option<MediaTimestamp> {
    let elapsed = at.checked_duration_since(t0)?;
    Some(MediaTimestamp {
        ticks: i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX),
        time_base_num: 1,
        time_base_den: 1_000_000_000,
    })
}

fn internal(error: impl std::fmt::Display) -> CommandError {
    CommandError::new(
        ErrorCode::Internal,
        format!("media pipeline failed: {error}"),
    )
}
