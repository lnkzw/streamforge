//! Recording: H.264 video and AAC audio muxed into Matroska with FFmpeg.
//!
//! A [`Recorder`] owns a `recorder` thread that converts, encodes and muxes.
//! Producers (compositor, mixer) feed it through a cloneable
//! [`RecorderInput`] that never blocks. When the bounded queue is full, items
//! are dropped and counted. Timestamps come from one monotonic clock: video in
//! microseconds and audio in sample frames, both relative to the recording
//! start.
//!
//! FFmpeg is linked through `ffmpeg-next` on Linux. On other platforms
//! [`Recorder::start`] returns [`RecorderErrorKind::Unsupported`] (M9).
//! Binaries inherit the license of the FFmpeg build they link (often GPL).

mod audio_fifo;
#[cfg(target_os = "linux")]
mod ffmpeg;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::{self, JoinHandle};

use media_core::VideoFrame;

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
        pts_us: i64,
    },
    Audio {
        samples: [f32; MAX_AUDIO_BLOCK_SAMPLES],
        len: usize,
        /// Timestamp of the first frame, in sample frames.
        pts: i64,
    },
    Stop,
}

#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub video_dropped: AtomicU64,
    pub audio_dropped: AtomicU64,
}

/// Non-blocking producer handle; clone it for each producer.
#[derive(Clone)]
pub struct RecorderInput {
    tx: SyncSender<Item>,
    counters: Arc<Counters>,
}

impl RecorderInput {
    /// Queue a Bgra8/Bgrx8 frame with its presentation time in microseconds.
    /// Returns `false` (and counts a drop) if the queue is full or closed.
    pub fn send_video(&self, frame: VideoFrame, pts_us: i64) -> bool {
        let sent = self.tx.try_send(Item::Video { frame, pts_us }).is_ok();
        if !sent {
            self.counters.video_dropped.fetch_add(1, Ordering::Relaxed);
        }
        sent
    }

    /// Count a video frame the producer could not even prepare (e.g. no free
    /// copy buffer), so it shows up in `frames_dropped`.
    pub fn count_dropped_video(&self) {
        self.counters.video_dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Like [`Self::send_video`] but waits for queue space instead of
    /// dropping; for offline sources that may produce faster than real time.
    /// Returns `false` only if the recorder has stopped.
    pub fn send_video_blocking(&self, frame: VideoFrame, pts_us: i64) -> bool {
        self.tx.send(Item::Video { frame, pts_us }).is_ok()
    }

    /// Like [`Self::send_audio`] but waits for queue space; see
    /// [`Self::send_video_blocking`].
    pub fn send_audio_blocking(&self, samples: &[f32], pts: i64) -> bool {
        if samples.len() > MAX_AUDIO_BLOCK_SAMPLES {
            self.counters.audio_dropped.fetch_add(1, Ordering::Relaxed);
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

    /// Queue interleaved samples whose first frame has timestamp `pts`
    /// (in sample frames). Copies into a fixed-size message; does not
    /// allocate. Blocks larger than [`MAX_AUDIO_BLOCK_SAMPLES`] are dropped.
    pub fn send_audio(&self, samples: &[f32], pts: i64) -> bool {
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
            self.counters.audio_dropped.fetch_add(1, Ordering::Relaxed);
        }
        sent
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
        let counters = Arc::new(Counters::default());
        let thread = thread::Builder::new()
            .name("recorder".to_owned())
            .spawn({
                let counters = Arc::clone(&counters);
                move || run(config, rx, ready_tx, counters)
            })
            .map_err(|err| {
                RecorderError::new(
                    RecorderErrorKind::Internal,
                    format!("cannot start the recorder thread: {err}"),
                )
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                input: RecorderInput { tx, counters },
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
    counters: Arc<Counters>,
) -> RecorderResult {
    ffmpeg::run(config, rx, ready, counters)
}

#[cfg(not(target_os = "linux"))]
fn run(
    _config: RecorderConfig,
    _rx: Receiver<Item>,
    ready: SyncSender<Result<(), RecorderError>>,
    _counters: Arc<Counters>,
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
