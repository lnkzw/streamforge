//! Recording: H.264 video and AAC audio muxed into Matroska with FFmpeg.
//!
//! A [`Recorder`] owns a `recorder` thread that converts, encodes and muxes.
//! Producers (compositor, mixer) feed it through a cloneable
//! [`RecorderInput`] that never blocks. When the bounded queue is full, items
//! are dropped and counted. Timestamps are [`MediaTimestamp`]s with explicit
//! time bases on one monotonic timeline that starts at the recording start;
//! the recorder rescales them (checked) into the encoder time bases.
//!
//! FFmpeg is linked through `ffmpeg-next` on Linux. On other platforms
//! [`Recorder::start`] returns [`RecorderErrorKind::Unsupported`] (M9).
//! Binaries inherit the license of the FFmpeg build they link (often GPL).

mod audio_fifo;
#[cfg(target_os = "linux")]
mod ffmpeg;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::{self, JoinHandle};

use media_core::{MediaTimestamp, VideoFrame};

pub use audio_fifo::{AudioFifo, FifoStats, MAX_GAP_FILL};
pub use encoder_api::{
    AudioSettings, AudioTrackStats, RecorderConfig, RecorderError, RecorderErrorKind,
    RecordingStats, VideoSettings, VideoTrackStats,
};

/// Largest interleaved audio block accepted by [`RecorderInput::send_audio`].
pub const MAX_AUDIO_BLOCK_SAMPLES: usize = 2048;
/// About 1 s of 60 FPS video plus audio blocks; preallocated.
const QUEUE_CAPACITY: usize = 256;

// Audio blocks travel inline so the mixer thread never allocates; the
// bounded channel preallocates its slots once.
#[allow(clippy::large_enum_variant)]
// Only the Linux recorder reads the payloads.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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

/// State shared by the recorder thread, its owner and the producers.
#[derive(Debug)]
pub(crate) struct Shared {
    pub video_dropped: AtomicU64,
    pub audio_dropped: AtomicU64,
    /// Items stamped at or after this time (ns) are not recorded.
    pub end_ns: AtomicI64,
    /// Set by the recorder thread when encoding or writing failed.
    pub failed: AtomicBool,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            video_dropped: AtomicU64::new(0),
            audio_dropped: AtomicU64::new(0),
            end_ns: AtomicI64::new(i64::MAX),
            failed: AtomicBool::new(false),
        }
    }
}

/// Non-blocking producer handle; clone it for each producer.
#[derive(Clone)]
pub struct RecorderInput {
    tx: SyncSender<Item>,
    shared: Arc<Shared>,
}

impl RecorderInput {
    /// Queue a Bgra8/Bgrx8 frame with its presentation time. Returns `false`
    /// (and counts a drop) if the queue is full or the recorder has ended.
    pub fn send_video(&self, frame: VideoFrame, pts: MediaTimestamp) -> bool {
        let sent = self.tx.try_send(Item::Video { frame, pts }).is_ok();
        if !sent {
            self.count_dropped_video();
        }
        sent
    }

    /// Count a video frame the producer could not even prepare (e.g. no free
    /// copy buffer), so it shows up in `frames_dropped`.
    pub fn count_dropped_video(&self) {
        self.shared.video_dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Like [`Self::send_video`] but waits for queue space instead of
    /// dropping; for offline sources that may produce faster than real time.
    /// Returns `false` only if the recorder has stopped.
    pub fn send_video_blocking(&self, frame: VideoFrame, pts: MediaTimestamp) -> bool {
        self.tx.send(Item::Video { frame, pts }).is_ok()
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
        if !sent {
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
        self.tx
            .send(Item::Audio {
                samples: block,
                len: samples.len(),
                pts,
            })
            .is_ok()
    }
}

type RecorderResult = Result<RecordingStats, RecorderError>;

/// A running recording. Dropping it without [`Recorder::stop`] still
/// finalizes the file.
pub struct Recorder {
    input: RecorderInput,
    thread: Option<JoinHandle<RecorderResult>>,
}

impl Recorder {
    /// Open the output and the encoders on the `recorder` thread. Returns
    /// once the file header is written, or with the setup error.
    pub fn start(config: RecorderConfig) -> Result<Self, RecorderError> {
        let (tx, rx) = sync_channel(QUEUE_CAPACITY);
        let (ready_tx, ready_rx) = sync_channel(1);
        let shared = Arc::new(Shared::default());
        let thread = thread::Builder::new()
            .name("recorder".to_owned())
            .spawn({
                let shared = Arc::clone(&shared);
                move || run(config, rx, ready_tx, shared)
            })
            .map_err(|err| {
                RecorderError::new(
                    RecorderErrorKind::Internal,
                    format!("cannot start the recorder thread: {err}"),
                )
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                input: RecorderInput { tx, shared },
                thread: Some(thread),
            }),
            // Setup failed: the thread ends with that error.
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => Err(join(thread).err().unwrap_or_else(|| {
                RecorderError::new(RecorderErrorKind::Internal, "recorder ended during setup")
            })),
        }
    }

    pub fn input(&self) -> RecorderInput {
        self.input.clone()
    }

    /// Set the end of the recording: items stamped at or after `end` are not
    /// recorded (counted as `*_after_end`). Producers run on until they are
    /// stopped, so this gives both tracks the same end.
    pub fn end_at(&self, end: MediaTimestamp) {
        let end_ns = end
            .to_nanoseconds()
            .map_or(i64::MAX, |ns| i64::try_from(ns).unwrap_or(i64::MAX));
        self.input.shared.end_ns.store(end_ns, Ordering::Release);
    }

    /// `false` once encoding or writing has failed (the file is finalized as
    /// far as possible; [`Self::stop`] returns the error) or the thread ended.
    pub fn is_running(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
            && !self.input.shared.failed.load(Ordering::Acquire)
    }

    /// Encode everything queued, flush the encoders and finalize the file.
    pub fn stop(mut self) -> RecorderResult {
        self.finish().unwrap_or_else(|| {
            Err(RecorderError::new(
                RecorderErrorKind::Internal,
                "the recorder was already stopped",
            ))
        })
    }

    fn finish(&mut self) -> Option<RecorderResult> {
        let thread = self.thread.take()?;
        // Blocking send: the stop marker must arrive after queued items. It
        // fails only if the thread already ended; joining reports why.
        let _ = self.input.tx.send(Item::Stop);
        Some(join(thread))
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn join(thread: JoinHandle<RecorderResult>) -> RecorderResult {
    thread.join().unwrap_or_else(|_| {
        Err(RecorderError::new(
            RecorderErrorKind::Internal,
            "the recorder thread panicked",
        ))
    })
}

#[cfg(target_os = "linux")]
fn run(
    config: RecorderConfig,
    rx: Receiver<Item>,
    ready: SyncSender<Result<(), RecorderError>>,
    shared: Arc<Shared>,
) -> RecorderResult {
    ffmpeg::run(config, rx, ready, shared)
}

#[cfg(not(target_os = "linux"))]
fn run(
    _config: RecorderConfig,
    _rx: Receiver<Item>,
    ready: SyncSender<Result<(), RecorderError>>,
    _shared: Arc<Shared>,
) -> RecorderResult {
    let error = RecorderError::new(
        RecorderErrorKind::Unsupported,
        format!(
            "recording is not implemented on {} yet; Linux is supported, other platforms follow in M9",
            std::env::consts::OS
        ),
    );
    let _ = ready.send(Err(error.clone()));
    Err(error)
}
