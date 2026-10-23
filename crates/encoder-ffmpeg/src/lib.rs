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
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use media_core::{MediaTimestamp, VideoFrame};

pub use audio_fifo::{AudioFifo, FifoStats, MAX_GAP_FILL};
pub use encoder_api::{
    AudioSettings, AudioTrackStats, EncoderPreference, LiveOutputStats, MAX_AUDIO_TRACKS,
    RateControl, ReconnectPolicy, RecorderConfig, RecorderError, RecorderErrorKind, RecordingStats,
    SkippedEncoder, StreamConfig, StreamKey, StreamState, StreamStats, StreamTarget,
    VideoEncoderInfo, VideoSettings, VideoTrackStats,
};

/// Largest interleaved audio block accepted by [`OutputInput::send_audio`].
pub const MAX_AUDIO_BLOCK_SAMPLES: usize = 2048;
/// About 1 s of 60 FPS video plus audio blocks; preallocated.
const QUEUE_CAPACITY: usize = 256;
/// Blocks for the tracks after the first: as many audio items as the queue
/// holds at its usual mix of video and audio (about 1.6 s).
const TRACK_POOL_BLOCKS: usize = 160;
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
        /// Track 1.
        samples: [f32; MAX_AUDIO_BLOCK_SAMPLES],
        len: usize,
        /// Timestamp of the first frame.
        pts: MediaTimestamp,
        /// The other tracks' samples, if the output has more than one track.
        extra: Option<TrackBlock>,
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

/// Preallocated buffers for an output's audio tracks after the first
/// (which travels inline in the item). The producer takes a buffer, fills
/// it and sends it in the item as a [`TrackBlock`]; dropping the block,
/// wherever that happens, returns the buffer. So queue slots stay small,
/// nothing is allocated after start, no lock is held while encoding, and
/// no buffer can leak.
pub(crate) struct TrackPool {
    /// Free buffers; each holds the extra tracks one after the other. Its
    /// lock is held only to push or pop one.
    free: Mutex<Vec<Vec<f32>>>,
    tracks: usize,
}

impl TrackPool {
    fn new(tracks: usize, blocks: usize) -> Self {
        Self {
            free: Mutex::new(
                (0..blocks)
                    .map(|_| vec![0.0; tracks * MAX_AUDIO_BLOCK_SAMPLES])
                    .collect(),
            ),
            tracks,
        }
    }

    fn free(&self) -> std::sync::MutexGuard<'_, Vec<Vec<f32>>> {
        self.free
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A free buffer, if any; never waits for one.
    fn take(self: &Arc<Self>) -> Option<TrackBlock> {
        let buffer = self.free().pop()?;
        Some(TrackBlock {
            buffer,
            len: 0,
            pool: Arc::clone(self),
        })
    }
}

/// One block of an output's audio tracks after the first, in a pooled
/// buffer that goes back to its pool on drop.
pub(crate) struct TrackBlock {
    buffer: Vec<f32>,
    len: usize,
    pool: Arc<TrackPool>,
}

impl TrackBlock {
    /// Each extra track's samples, in order.
    pub(crate) fn tracks(&self) -> impl Iterator<Item = &[f32]> {
        self.buffer
            .chunks(MAX_AUDIO_BLOCK_SAMPLES)
            .map(|chunk| &chunk[..self.len])
    }
}

impl Drop for TrackBlock {
    fn drop(&mut self) {
        // The free list holds every buffer at most once, within the
        // capacity it started with: this does not allocate.
        let buffer = std::mem::take(&mut self.buffer);
        self.pool.free().push(buffer);
    }
}

/// Non-blocking producer handle; clone it for each producer.
#[derive(Clone)]
pub struct OutputInput {
    tx: SyncSender<Item>,
    shared: Arc<Shared>,
    /// For an output with more than one audio track.
    pool: Option<Arc<TrackPool>>,
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
        self.send_audio_tracks(&[samples], pts)
    }

    /// Queue one block of every audio track (track 1 first; each the same
    /// length, at most [`MAX_AUDIO_BLOCK_SAMPLES`]) as one item, so the
    /// tracks share the queue's depth and are dropped together. Does not
    /// allocate. A block whose track count does not match the output's, or
    /// that finds the queue or the track pool full, is dropped and counted.
    pub fn send_audio_tracks(&self, tracks: &[&[f32]], pts: MediaTimestamp) -> bool {
        let sent = self.queue_audio(tracks, pts, false);
        if sent {
            self.shared.sent();
        } else {
            self.shared.audio_dropped.fetch_add(1, Ordering::Relaxed);
        }
        sent
    }

    /// With `wait`, wait for a free track block and queue space instead of
    /// giving up (offline producers); no lock is held while waiting.
    fn queue_audio(&self, tracks: &[&[f32]], pts: MediaTimestamp, wait: bool) -> bool {
        let Some((first, rest)) = tracks.split_first() else {
            return false;
        };
        let len = first.len();
        let extra_tracks = self.pool.as_ref().map_or(0, |pool| pool.tracks);
        if len > MAX_AUDIO_BLOCK_SAMPLES
            || rest.len() != extra_tracks
            || rest.iter().any(|track| track.len() != len)
        {
            return false;
        }
        let extra = match &self.pool {
            Some(pool) if !rest.is_empty() => {
                let mut block = loop {
                    match pool.take() {
                        Some(block) => break block,
                        // Buffers come back as the output thread encodes,
                        // or when its queue is dropped.
                        None if wait => thread::sleep(Duration::from_millis(1)),
                        None => return false,
                    }
                };
                for (chunk, track) in block.buffer.chunks_mut(MAX_AUDIO_BLOCK_SAMPLES).zip(rest) {
                    chunk[..len].copy_from_slice(track);
                }
                block.len = len;
                Some(block)
            }
            _ => None,
        };
        let mut block = [0.0; MAX_AUDIO_BLOCK_SAMPLES];
        block[..len].copy_from_slice(first);
        let item = Item::Audio {
            samples: block,
            len,
            pts,
            extra,
        };
        // A refused item is dropped here, which returns its track buffer.
        if wait {
            self.tx.send(item).is_ok()
        } else {
            self.tx.try_send(item).is_ok()
        }
    }

    /// Like [`Self::send_audio`] but waits for queue space; see
    /// [`Self::send_video_blocking`].
    pub fn send_audio_blocking(&self, samples: &[f32], pts: MediaTimestamp) -> bool {
        self.send_audio_tracks_blocking(&[samples], pts)
    }

    /// Like [`Self::send_audio_tracks`] but waits for room; see
    /// [`Self::send_video_blocking`].
    pub fn send_audio_tracks_blocking(&self, tracks: &[&[f32]], pts: MediaTimestamp) -> bool {
        let sent = self.queue_audio(tracks, pts, true);
        if sent {
            self.shared.sent();
        } else {
            self.shared.audio_dropped.fetch_add(1, Ordering::Relaxed);
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
    /// `extra_tracks` audio tracks after the first get a [`TrackPool`].
    fn start<F>(name: &str, extra_tracks: usize, run: F) -> Result<Self, RecorderError>
    where
        F: FnOnce(Receiver<Item>, Ready, Arc<Shared>) -> Result<T, RecorderError> + Send + 'static,
    {
        let (tx, rx) = sync_channel(QUEUE_CAPACITY);
        let (ready_tx, ready_rx) = sync_channel(1);
        let shared = Arc::new(Shared::default());
        let pool =
            (extra_tracks > 0).then(|| Arc::new(TrackPool::new(extra_tracks, TRACK_POOL_BLOCKS)));
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
                input: OutputInput { tx, shared, pool },
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
        if config.audio_tracks.len() > MAX_AUDIO_TRACKS {
            return Err(RecorderError::new(
                RecorderErrorKind::Unsupported,
                format!(
                    "at most {MAX_AUDIO_TRACKS} audio tracks, got {}",
                    config.audio_tracks.len()
                ),
            ));
        }
        let extra_tracks = config.audio_tracks.len().saturating_sub(1);
        Worker::start("recorder", extra_tracks, move |rx, ready, shared| {
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
        Worker::start("streamer", 0, move |rx, ready, shared| {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_buffers_go_back_to_their_pool_when_dropped() {
        let pool = Arc::new(TrackPool::new(2, 3));
        let taken: Vec<TrackBlock> = (0..3).map(|_| pool.take().expect("free")).collect();
        assert!(pool.take().is_none(), "three buffers, all taken");
        assert!(
            taken
                .iter()
                .all(|block| block.buffer.len() == 2 * MAX_AUDIO_BLOCK_SAMPLES)
        );
        drop(taken);
        // All back, none duplicated: three again, then none.
        let again: Vec<TrackBlock> = (0..3).map(|_| pool.take().expect("free")).collect();
        assert!(pool.take().is_none());
        drop(again);
        assert_eq!(pool.free().len(), 3);
    }

    #[test]
    fn a_refused_item_returns_its_buffer() {
        let (tx, rx) = sync_channel(1);
        let pool = Arc::new(TrackPool::new(1, 2));
        let input = OutputInput {
            tx,
            shared: Arc::new(Shared::default()),
            pool: Some(Arc::clone(&pool)),
        };
        let pts = MediaTimestamp::new(0, 1, 48_000).expect("pts");
        let block = [0.25f32; 960];
        assert!(input.send_audio_tracks(&[&block, &block], pts));
        // The queue is full: refused, counted, and the buffer is back.
        assert!(!input.send_audio_tracks(&[&block, &block], pts));
        assert_eq!(input.shared.audio_dropped.load(Ordering::Relaxed), 1);
        assert_eq!(pool.free().len(), 1);
        // The queued item carries the second track; dropping it returns it.
        let Ok(Item::Audio {
            extra: Some(extra),
            len,
            ..
        }) = rx.try_recv()
        else {
            panic!("an audio item with its track block");
        };
        assert_eq!(len, 960);
        assert!(extra.tracks().all(|track| track == block));
        drop(extra);
        assert_eq!(pool.free().len(), 2);
        // A block without its second track does not fit this output.
        assert!(!input.send_audio(&block, pts));
    }
}
