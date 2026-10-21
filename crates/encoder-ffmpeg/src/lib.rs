//! Outputs: recording (H.264 + AAC in Matroska) and live streaming (H.264 +
//! AAC in FLV over RTMP) with FFmpeg.
//!
//! Each output ([`Recorder`], [`Streamer`]) owns one thread with its own
//! encoders, so outputs never share a failure. Producers (compositor, mixer)
//! feed an output through a cloneable [`OutputInput`] that never blocks.
//! When the bounded queue is full, items are dropped and counted. Timestamps
//! are [`MediaTimestamp`]s with explicit time bases on one monotonic
//! timeline that starts at the output's start; the output rescales them
//! (checked) into the encoder time bases.
//!
//! FFmpeg is linked through `ffmpeg-next` on Linux, Windows and macOS (see
//! `docs/platforms.md` for how each gets its libraries). Binaries inherit
//! the license of the FFmpeg build they link (often GPL).

mod audio_fifo;
mod ffmpeg;

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use media_core::{MediaTimestamp, VideoFrame};

pub use audio_fifo::{AudioFifo, FifoStats, MAX_GAP_FILL};
pub use encoder_api::{
    AudioSettings, AudioTrackStats, EncoderPreference, LiveOutputStats, RateControl,
    ReconnectPolicy, RecorderConfig, RecorderError, RecorderErrorKind, RecordingStats,
    SkippedEncoder, StreamConfig, StreamKey, StreamState, StreamStats, StreamTarget,
    VideoEncoderInfo, VideoSettings, VideoTrackStats,
};

/// Largest interleaved audio block accepted by [`OutputInput::send_audio`].
pub const MAX_AUDIO_BLOCK_SAMPLES: usize = 2048;
/// About 1 s of 60 FPS video plus audio blocks; preallocated.
const QUEUE_CAPACITY: usize = 256;
/// How long a stopping stream may spend flushing and closing a healthy
/// connection before blocked network I/O is interrupted.
pub const STOP_GRACE: Duration = Duration::from_secs(2);

/// Nanoseconds on a process-wide monotonic clock (for deadlines in atomics).
pub(crate) fn monotonic_ns() -> i64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let epoch = *EPOCH.get_or_init(Instant::now);
    i64::try_from(epoch.elapsed().as_nanos()).unwrap_or(i64::MAX)
}

// Audio blocks travel inline so the mixer thread never allocates; the
// bounded channel preallocates its slots once.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Item {
    Video {
        frame: VideoFrame,
        pts: MediaTimestamp,
    },
    Audio {
        samples: [f32; MAX_AUDIO_BLOCK_SAMPLES],
        len: usize,
        /// Timestamp of the first frame.
        pts: MediaTimestamp,
    },
    Stop,
}

/// State shared by an output thread, its owner and the producers.
#[derive(Debug)]
pub(crate) struct Shared {
    pub video_dropped: AtomicU64,
    pub audio_dropped: AtomicU64,
    /// Items stamped at or after this time (ns) are not encoded.
    pub end_ns: AtomicI64,
    /// Set by the output thread when it can no longer deliver media.
    pub failed: AtomicBool,
    /// [`StreamState`] of a streaming output.
    pub state: AtomicU8,
    /// Deadline ([`monotonic_ns`]) after which blocked FFmpeg I/O is
    /// interrupted; `i64::MAX` while not stopping.
    pub abort_after_ns: AtomicI64,
    /// Totals the output thread publishes after each item (see
    /// [`LiveOutputStats`]); drops here are the output thread's own.
    pub frames_encoded: AtomicU64,
    pub frames_dropped: AtomicU64,
    pub samples_encoded: AtomicU64,
    pub blocks_dropped: AtomicU64,
    /// Sent minus received media items; may dip below zero for a moment
    /// (a producer counts after sending), so readers clamp it.
    pub queued: AtomicI64,
    pub connects: AtomicU32,
    pub reconnect_attempts: AtomicU32,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            video_dropped: AtomicU64::new(0),
            audio_dropped: AtomicU64::new(0),
            end_ns: AtomicI64::new(i64::MAX),
            failed: AtomicBool::new(false),
            state: AtomicU8::new(StreamState::Connecting.as_u8()),
            abort_after_ns: AtomicI64::new(i64::MAX),
            frames_encoded: AtomicU64::new(0),
            frames_dropped: AtomicU64::new(0),
            samples_encoded: AtomicU64::new(0),
            blocks_dropped: AtomicU64::new(0),
            queued: AtomicI64::new(0),
            connects: AtomicU32::new(0),
            reconnect_attempts: AtomicU32::new(0),
        }
    }
}

/// Totals of the output thread's own work, published as a whole.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Counts {
    pub frames_encoded: u64,
    pub frames_dropped: u64,
    pub samples_encoded: u64,
    pub blocks_dropped: u64,
}

impl Shared {
    pub(crate) fn publish(&self, counts: Counts) {
        self.frames_encoded
            .store(counts.frames_encoded, Ordering::Relaxed);
        self.frames_dropped
            .store(counts.frames_dropped, Ordering::Relaxed);
        self.samples_encoded
            .store(counts.samples_encoded, Ordering::Relaxed);
        self.blocks_dropped
            .store(counts.blocks_dropped, Ordering::Relaxed);
    }

    /// The output thread took a media item off the queue.
    pub(crate) fn received(&self, item: &Item) {
        if !matches!(item, Item::Stop) {
            self.queued.fetch_sub(1, Ordering::Relaxed);
        }
    }

    fn sent(&self) {
        self.queued.fetch_add(1, Ordering::Relaxed);
    }

    fn live(&self) -> LiveOutputStats {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        LiveOutputStats {
            frames_encoded: load(&self.frames_encoded),
            frames_dropped: load(&self.frames_dropped) + load(&self.video_dropped),
            samples_encoded: load(&self.samples_encoded),
            blocks_dropped: load(&self.blocks_dropped) + load(&self.audio_dropped),
            queue_depth: u64::try_from(self.queued.load(Ordering::Relaxed)).unwrap_or(0),
            connects: self.connects.load(Ordering::Relaxed),
            reconnect_attempts: self.reconnect_attempts.load(Ordering::Relaxed),
        }
    }
}

/// Non-blocking producer handle; clone it for each producer.
#[derive(Clone)]
pub struct OutputInput {
    tx: SyncSender<Item>,
    shared: Arc<Shared>,
}

impl OutputInput {
    /// Queue a Bgra8/Bgrx8 frame with its presentation time. Returns `false`
    /// (and counts a drop) if the queue is full or the output has ended.
    pub fn send_video(&self, frame: VideoFrame, pts: MediaTimestamp) -> bool {
        let sent = self.tx.try_send(Item::Video { frame, pts }).is_ok();
        if sent {
            self.shared.sent();
        } else {
            self.count_dropped_video();
        }
        sent
    }

    /// Count a video frame the producer could not even prepare (e.g. no free
    /// copy buffer), so it shows up in `frames_dropped`.
    pub fn count_dropped_video(&self) {
        self.count_dropped(1, 0);
    }

    /// Count frames and audio blocks that never reached this output (e.g. a
    /// producer that did not deliver before the output ended).
    pub fn count_dropped(&self, video: u64, audio: u64) {
        self.shared
            .video_dropped
            .fetch_add(video, Ordering::Relaxed);
        self.shared
            .audio_dropped
            .fetch_add(audio, Ordering::Relaxed);
    }

    /// Like [`Self::send_video`] but waits for queue space instead of
    /// dropping; for offline sources that may produce faster than real time.
    /// Returns `false` only if the output has stopped.
    pub fn send_video_blocking(&self, frame: VideoFrame, pts: MediaTimestamp) -> bool {
        let sent = self.tx.send(Item::Video { frame, pts }).is_ok();
        if sent {
            self.shared.sent();
        }
        sent
    }

    /// Queue interleaved samples whose first frame has timestamp `pts`.
    /// Copies into a fixed-size message; does not allocate. Blocks larger
    /// than [`MAX_AUDIO_BLOCK_SAMPLES`] are dropped and counted.
    pub fn send_audio(&self, samples: &[f32], pts: MediaTimestamp) -> bool {
        let sent = samples.len() <= MAX_AUDIO_BLOCK_SAMPLES && {
            let mut block = [0.0; MAX_AUDIO_BLOCK_SAMPLES];
            block[..samples.len()].copy_from_slice(samples);
            self.tx
                .try_send(Item::Audio {
                    samples: block,
                    len: samples.len(),
                    pts,
                })
                .is_ok()
        };
        if sent {
            self.shared.sent();
        } else {
            self.shared.audio_dropped.fetch_add(1, Ordering::Relaxed);
        }
        sent
    }

    /// Like [`Self::send_audio`] but waits for queue space; see
    /// [`Self::send_video_blocking`].
    pub fn send_audio_blocking(&self, samples: &[f32], pts: MediaTimestamp) -> bool {
        if samples.len() > MAX_AUDIO_BLOCK_SAMPLES {
            self.shared.audio_dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let mut block = [0.0; MAX_AUDIO_BLOCK_SAMPLES];
        block[..samples.len()].copy_from_slice(samples);
        let sent = self
            .tx
            .send(Item::Audio {
                samples: block,
                len: samples.len(),
                pts,
            })
            .is_ok();
        if sent {
            self.shared.sent();
        }
        sent
    }
}

/// The output thread reports its opened video encoder, or why it failed.
type Ready = SyncSender<Result<VideoEncoderInfo, RecorderError>>;

/// An output thread and the input that feeds it.
struct Worker<T> {
    input: OutputInput,
    encoder: VideoEncoderInfo,
    thread: Option<JoinHandle<Result<T, RecorderError>>>,
}

impl<T: Send + 'static> Worker<T> {
    /// Spawn `run` on a thread named `name` and wait until it reports that
    /// the output is open (or why it is not).
    fn start<F>(name: &str, run: F) -> Result<Self, RecorderError>
    where
        F: FnOnce(Receiver<Item>, Ready, Arc<Shared>) -> Result<T, RecorderError> + Send + 'static,
    {
        let (tx, rx) = sync_channel(QUEUE_CAPACITY);
        let (ready_tx, ready_rx) = sync_channel(1);
        let shared = Arc::new(Shared::default());
        let thread = thread::Builder::new()
            .name(name.to_owned())
            .spawn({
                let shared = Arc::clone(&shared);
                move || run(rx, ready_tx, shared)
            })
            .map_err(|err| {
                RecorderError::new(
                    RecorderErrorKind::Internal,
                    format!("cannot start the {name} thread: {err}"),
                )
            })?;
        match ready_rx.recv() {
            Ok(Ok(encoder)) => Ok(Self {
                input: OutputInput { tx, shared },
                encoder,
                thread: Some(thread),
            }),
            // Setup failed: the thread ends with that error.
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => Err(join(thread).err().unwrap_or_else(|| {
                RecorderError::new(
                    RecorderErrorKind::Internal,
                    format!("the {name} thread ended during setup"),
                )
            })),
        }
    }
}

impl<T> Worker<T> {
    fn end_at(&self, end: MediaTimestamp) {
        let end_ns = end
            .to_nanoseconds()
            .map_or(i64::MAX, |ns| i64::try_from(ns).unwrap_or(i64::MAX));
        self.input.shared.end_ns.store(end_ns, Ordering::Release);
    }

    fn thread_alive(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
    }

    fn is_running(&self) -> bool {
        self.thread_alive() && !self.input.shared.failed.load(Ordering::Acquire)
    }

    fn finish(&mut self) -> Option<Result<T, RecorderError>> {
        let thread = self.thread.take()?;
        // Blocking send: the stop marker must arrive after queued items. It
        // fails only if the thread already ended; joining reports why.
        let _ = self.input.tx.send(Item::Stop);
        Some(join(thread))
    }

    fn stop(mut self) -> Result<T, RecorderError> {
        self.finish().unwrap_or_else(|| {
            Err(RecorderError::new(
                RecorderErrorKind::Internal,
                "the output was already stopped",
            ))
        })
    }
}

impl<T> Drop for Worker<T> {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn join<T>(thread: JoinHandle<Result<T, RecorderError>>) -> Result<T, RecorderError> {
    thread.join().unwrap_or_else(|_| {
        Err(RecorderError::new(
            RecorderErrorKind::Internal,
            "the output thread panicked",
        ))
    })
}

/// A running recording. Dropping it without [`Recorder::stop`] still
/// finalizes the file.
pub struct Recorder {
    worker: Worker<RecordingStats>,
}

impl Recorder {
    /// Open the output and the encoders on the `recorder` thread. Returns
    /// once the file header is written, or with the setup error.
    pub fn start(config: RecorderConfig) -> Result<Self, RecorderError> {
        Worker::start("recorder", move |rx, ready, shared| {
            run_recorder(config, rx, ready, shared)
        })
        .map(|worker| Self { worker })
    }

    pub fn input(&self) -> OutputInput {
        self.worker.input.clone()
    }

    /// The video encoder in use (hardware where available, ADR 0011).
    pub fn video_encoder(&self) -> &VideoEncoderInfo {
        &self.worker.encoder
    }

    /// Counters so far; never waits for the recorder thread.
    pub fn live(&self) -> LiveOutputStats {
        self.worker.input.shared.live()
    }

    /// Set the end of the recording: items stamped at or after `end` are not
    /// recorded (counted as `*_after_end`). Producers run on until they are
    /// stopped, so this gives both tracks the same end.
    pub fn end_at(&self, end: MediaTimestamp) {
        self.worker.end_at(end);
    }

    /// `false` once encoding or writing has failed (the file is finalized as
    /// far as possible; [`Self::stop`] returns the error) or the thread ended.
    pub fn is_running(&self) -> bool {
        self.worker.is_running()
    }

    /// Encode everything queued, flush the encoders and finalize the file.
    pub fn stop(self) -> Result<RecordingStats, RecorderError> {
        self.worker.stop()
    }
}

/// A running live stream. It reconnects after network or encoder errors as
/// its [`ReconnectPolicy`] allows; dropping it disconnects cleanly.
pub struct Streamer {
    worker: Worker<StreamStats>,
    target: String,
}

impl Streamer {
    /// Connect on the `streamer` thread and start publishing. Returns once
    /// the first connection is established, or with its error (no retry).
    pub fn start(config: StreamConfig) -> Result<Self, RecorderError> {
        let target = config.target.redacted();
        Worker::start("streamer", move |rx, ready, shared| {
            run_streamer(config, rx, ready, shared)
        })
        .map(|worker| Self { worker, target })
    }

    pub fn input(&self) -> OutputInput {
        self.worker.input.clone()
    }

    /// The video encoder of the first connection; reconnects keep it.
    pub fn video_encoder(&self) -> &VideoEncoderInfo {
        &self.worker.encoder
    }

    /// Totals over all connections so far; never waits for the streamer.
    pub fn live(&self) -> LiveOutputStats {
        self.worker.input.shared.live()
    }

    /// Redacted target, safe to show.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// See [`Recorder::end_at`].
    pub fn end_at(&self, end: MediaTimestamp) {
        self.worker.end_at(end);
    }

    /// Current state; a streamer thread that ended on its own (e.g. a
    /// panic) reads as [`StreamState::Failed`].
    pub fn state(&self) -> StreamState {
        let state = StreamState::from_u8(self.worker.input.shared.state.load(Ordering::Acquire));
        if self.worker.thread_alive() || state == StreamState::Stopped {
            state
        } else {
            StreamState::Failed
        }
    }

    /// Flush, disconnect and report. A healthy connection gets
    /// [`STOP_GRACE`] to flush and close; after that, blocked network I/O is
    /// interrupted, so stopping is bounded even for a stalled peer. A stream
    /// that ended in [`StreamState::Failed`] still returns its statistics.
    pub fn stop(mut self) -> Result<StreamStats, RecorderError> {
        self.start_stop_grace();
        self.worker.finish().unwrap_or_else(|| {
            Err(RecorderError::new(
                RecorderErrorKind::Internal,
                "the output was already stopped",
            ))
        })
    }

    fn start_stop_grace(&self) {
        let deadline =
            monotonic_ns().saturating_add(i64::try_from(STOP_GRACE.as_nanos()).unwrap_or(i64::MAX));
        self.worker
            .input
            .shared
            .abort_after_ns
            .store(deadline, Ordering::Release);
    }
}

impl Drop for Streamer {
    /// Dropping (e.g. at engine shutdown) is bounded like [`Streamer::stop`];
    /// the worker's own drop then disconnects.
    fn drop(&mut self) {
        self.start_stop_grace();
    }
}

fn run_recorder(
    config: RecorderConfig,
    rx: Receiver<Item>,
    ready: Ready,
    shared: Arc<Shared>,
) -> Result<RecordingStats, RecorderError> {
    ffmpeg::record::run(config, rx, ready, shared)
}

fn run_streamer(
    config: StreamConfig,
    rx: Receiver<Item>,
    ready: Ready,
    shared: Arc<Shared>,
) -> Result<StreamStats, RecorderError> {
    ffmpeg::stream::run(config, rx, ready, shared)
}
