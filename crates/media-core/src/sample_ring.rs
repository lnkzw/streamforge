//! Lock-free ring of interleaved `f32` samples from one producer to one
//! consumer that must never wait: the feed of a playback device (the
//! monitor, ADR 0017), whose callback runs on the OS's real-time thread.
//!
//! [`AudioRing`](crate::AudioRing) takes a mutex, which such a callback must
//! not. Here neither side locks, waits or allocates after construction, and
//! there is no `unsafe`:
//!
//! - **Slots** are `AtomicU32`s holding `f32` bits; relaxed loads and stores
//!   of them are plain moves on the supported CPUs.
//! - **Positions** are two `u64` counters of samples ever written and read,
//!   each changed by one side only. The producer stores its samples, then
//!   publishes its position with `Release`; the consumer loads it with
//!   `Acquire` before it reads them, and the same way back for freed slots.
//!   At 96 000 samples per second a counter wraps after millions of years.
//! - **One producer, one consumer:** [`sample_ring`] returns one
//!   [`SampleWriter`] and one [`SampleReader`]; neither is `Clone`, and both
//!   take `&mut self`.
//!
//! Nothing is lost silently. The consumer always fills its buffer: stored
//! frames, then silence, counted as warm-up before the first data and as
//! underrun after it. Frames that do not fit are counted as overrun. Once
//! one side is dropped, the other stops counting: the ring has ended, and
//! whoever owns it reports why.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct SampleRingStats {
    pub capacity_frames: u64,
    pub written_frames: u64,
    pub read_frames: u64,
    /// Silence the consumer played before the first data arrived.
    pub warmup_frames: u64,
    /// Silence the consumer played after that, because the ring ran short.
    pub underrun_frames: u64,
    /// Frames the producer could not store because the ring was full.
    pub overrun_frames: u64,
    /// Largest single read; the producer sizes its buffering from it.
    pub max_read_frames: u64,
}

struct Shared {
    channels: usize,
    slots: Box<[AtomicU32]>,
    /// Samples ever written; changed by the producer only.
    written: AtomicU64,
    /// Samples ever read; changed by the consumer only.
    read: AtomicU64,
    warmup: AtomicU64,
    underrun: AtomicU64,
    overrun: AtomicU64,
    max_read: AtomicU64,
    /// Largest read since the writer last took it.
    recent_max_read: AtomicU64,
    writer_gone: AtomicBool,
    reader_gone: AtomicBool,
}

impl Shared {
    fn slot(&self, position: u64) -> &AtomicU32 {
        // The remainder is below the slot count, which is a usize.
        &self.slots[(position % self.slots.len() as u64) as usize]
    }

    fn frames(&self, samples: u64) -> u64 {
        samples / self.channels as u64
    }

    fn stats(&self) -> SampleRingStats {
        SampleRingStats {
            capacity_frames: self.frames(self.slots.len() as u64),
            written_frames: self.frames(self.written.load(Ordering::Acquire)),
            read_frames: self.frames(self.read.load(Ordering::Acquire)),
            warmup_frames: self.warmup.load(Ordering::Relaxed),
            underrun_frames: self.underrun.load(Ordering::Relaxed),
            overrun_frames: self.overrun.load(Ordering::Relaxed),
            max_read_frames: self.max_read.load(Ordering::Relaxed),
        }
    }

    /// Frames stored and not yet read.
    fn fill_frames(&self) -> usize {
        let written = self.written.load(Ordering::Acquire);
        let read = self.read.load(Ordering::Acquire);
        // At most the capacity, which is a usize.
        self.frames(written.saturating_sub(read)) as usize
    }
}

/// A ring holding up to `capacity_frames` frames of `channels` samples.
pub fn sample_ring(channels: u32, capacity_frames: usize) -> (SampleWriter, SampleReader) {
    let channels = channels.max(1) as usize;
    let shared = Arc::new(Shared {
        channels,
        slots: (0..capacity_frames.max(1) * channels)
            .map(|_| AtomicU32::new(0))
            .collect(),
        written: AtomicU64::new(0),
        read: AtomicU64::new(0),
        warmup: AtomicU64::new(0),
        underrun: AtomicU64::new(0),
        overrun: AtomicU64::new(0),
        max_read: AtomicU64::new(0),
        recent_max_read: AtomicU64::new(0),
        writer_gone: AtomicBool::new(false),
        reader_gone: AtomicBool::new(false),
    });
    (
        SampleWriter {
            shared: Arc::clone(&shared),
        },
        SampleReader {
            shared,
            primed: false,
        },
    )
}

/// The producer's end of a [`sample_ring`].
pub struct SampleWriter {
    shared: Arc<Shared>,
}

impl SampleWriter {
    /// Store as many whole frames of `samples` as fit (a trailing partial
    /// frame is ignored); returns the frames stored. The rest is counted as
    /// overrun, unless the reader is gone. Never waits or allocates.
    pub fn write(&mut self, samples: &[f32]) -> usize {
        let shared = &*self.shared;
        let channels = shared.channels;
        let frames = samples.len() / channels;
        if shared.reader_gone.load(Ordering::Acquire) {
            return 0;
        }
        let written = shared.written.load(Ordering::Relaxed);
        // The reader's loads of the slots it freed happen before this.
        let read = shared.read.load(Ordering::Acquire);
        let used = written - read;
        let free = (shared.slots.len() as u64 - used) as usize / channels;
        let stored = frames.min(free);
        for (position, sample) in (written..).zip(&samples[..stored * channels]) {
            shared
                .slot(position)
                .store(sample.to_bits(), Ordering::Relaxed);
        }
        shared
            .written
            .store(written + (stored * channels) as u64, Ordering::Release);
        if stored < frames {
            shared
                .overrun
                .fetch_add((frames - stored) as u64, Ordering::Relaxed);
        }
        stored
    }

    /// Frames stored and not yet read.
    pub fn fill_frames(&self) -> usize {
        self.shared.fill_frames()
    }

    pub fn capacity_frames(&self) -> usize {
        self.shared.slots.len() / self.shared.channels
    }

    pub fn stats(&self) -> SampleRingStats {
        self.shared.stats()
    }

    /// The largest read since the last call, in frames, so a burst of
    /// large reads raises the buffering for a while, not for good.
    pub fn take_read_peak(&mut self) -> usize {
        // At most a read buffer's length, which is a usize.
        self.shared.recent_max_read.swap(0, Ordering::Relaxed) as usize
    }

    /// Whether the reader is gone (e.g. its device stopped).
    pub fn is_closed(&self) -> bool {
        self.shared.reader_gone.load(Ordering::Acquire)
    }
}

impl Drop for SampleWriter {
    fn drop(&mut self) {
        self.shared.writer_gone.store(true, Ordering::Release);
    }
}

impl fmt::Debug for SampleWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SampleWriter")
            .field("stats", &self.stats())
            .finish()
    }
}

/// The consumer's end of a [`sample_ring`].
pub struct SampleReader {
    shared: Arc<Shared>,
    /// Whether data has arrived yet: silence before it is warm-up.
    primed: bool,
}

impl SampleReader {
    /// Fill `out`: the oldest stored whole frames, then silence (also for a
    /// trailing partial frame). Returns the frames taken from the ring.
    /// Never waits or allocates.
    pub fn read(&mut self, out: &mut [f32]) -> usize {
        let shared = &*self.shared;
        let channels = shared.channels;
        let frames = out.len() / channels;
        shared.max_read.fetch_max(frames as u64, Ordering::Relaxed);
        shared
            .recent_max_read
            .fetch_max(frames as u64, Ordering::Relaxed);
        // The writer's stores of these slots happen before this.
        let written = shared.written.load(Ordering::Acquire);
        let read = shared.read.load(Ordering::Relaxed);
        let available = ((written - read) as usize) / channels;
        let taken = frames.min(available);
        for (position, sample) in (read..).zip(&mut out[..taken * channels]) {
            *sample = f32::from_bits(shared.slot(position).load(Ordering::Relaxed));
        }
        shared
            .read
            .store(read + (taken * channels) as u64, Ordering::Release);
        out[taken * channels..].fill(0.0);
        let short = (frames - taken) as u64;
        if !self.primed {
            self.primed = taken > 0;
            shared.warmup.fetch_add(short, Ordering::Relaxed);
        } else if !shared.writer_gone.load(Ordering::Acquire) {
            shared.underrun.fetch_add(short, Ordering::Relaxed);
        }
        taken
    }

    /// Frames stored and not yet read.
    pub fn fill_frames(&self) -> usize {
        self.shared.fill_frames()
    }

    pub fn channels(&self) -> usize {
        self.shared.channels
    }

    pub fn stats(&self) -> SampleRingStats {
        self.shared.stats()
    }

    /// Whether the writer is gone (the feed ended).
    pub fn is_closed(&self) -> bool {
        self.shared.writer_gone.load(Ordering::Acquire)
    }
}

impl Drop for SampleReader {
    fn drop(&mut self) {
        self.shared.reader_gone.store(true, Ordering::Release);
    }
}

impl fmt::Debug for SampleReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SampleReader")
            .field("stats", &self.stats())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_come_out_in_order_across_the_wrap() {
        let (mut writer, mut reader) = sample_ring(2, 4);
        let mut out = [0.0; 6];
        for round in 0..5 {
            let base = round as f32 * 10.0;
            let frames = [
                base + 1.0,
                base + 2.0,
                base + 3.0,
                base + 4.0,
                base + 5.0,
                base + 6.0,
            ];
            // A trailing partial frame is ignored.
            assert_eq!(writer.write(&[frames.as_slice(), &[99.0]].concat()), 3);
            assert_eq!(writer.fill_frames(), 3);
            assert_eq!(reader.read(&mut out), 3);
            assert_eq!(out, frames);
        }
        let stats = reader.stats();
        assert_eq!((stats.written_frames, stats.read_frames), (15, 15));
        assert_eq!((stats.overrun_frames, stats.underrun_frames), (0, 0));
        assert_eq!(stats.capacity_frames, 4);
    }

    #[test]
    fn a_full_ring_counts_what_did_not_fit() {
        let (mut writer, mut reader) = sample_ring(1, 3);
        assert_eq!(writer.write(&[1.0, 2.0]), 2);
        assert_eq!(writer.write(&[3.0, 4.0, 5.0]), 1);
        assert_eq!(writer.stats().overrun_frames, 2);
        let mut out = [0.0; 3];
        assert_eq!(reader.read(&mut out), 3);
        assert_eq!(out, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn short_reads_pad_silence_counted_as_warmup_then_underrun() {
        let (mut writer, mut reader) = sample_ring(2, 8);
        let mut out = [7.0; 4];
        // Nothing yet: start-up silence.
        assert_eq!(reader.read(&mut out), 0);
        assert_eq!(out, [0.0; 4]);
        writer.write(&[1.0, 1.0]);
        // The first data arrives short: still start-up.
        assert_eq!(reader.read(&mut out), 1);
        assert_eq!(out, [1.0, 1.0, 0.0, 0.0]);
        writer.write(&[2.0, 2.0]);
        assert_eq!(reader.read(&mut out), 1);
        assert_eq!(out, [2.0, 2.0, 0.0, 0.0]);
        let stats = reader.stats();
        assert_eq!((stats.warmup_frames, stats.underrun_frames), (3, 1));
        assert_eq!(stats.max_read_frames, 2);
        assert_eq!(writer.take_read_peak(), 2);
        assert_eq!(writer.take_read_peak(), 0);
        reader.read(&mut out[..2]);
        assert_eq!(writer.take_read_peak(), 1);
    }

    #[test]
    fn a_gone_side_ends_the_counting() {
        let (mut writer, reader) = sample_ring(1, 2);
        assert!(!writer.is_closed());
        drop(reader);
        assert!(writer.is_closed());
        assert_eq!(writer.write(&[1.0, 2.0, 3.0]), 0);
        assert_eq!(writer.stats().overrun_frames, 0);

        let (mut writer, mut reader) = sample_ring(1, 2);
        writer.write(&[1.0]);
        drop(writer);
        assert!(reader.is_closed());
        let mut out = [0.0; 2];
        assert_eq!(reader.read(&mut out), 1);
        assert_eq!(reader.read(&mut out), 0);
        assert_eq!(reader.stats().underrun_frames, 0);
    }

    /// Two threads with uneven chunk sizes: every frame arrives once, in
    /// order, and nothing else arrives.
    #[test]
    fn two_threads_pass_every_frame_in_order() {
        const FRAMES: u64 = 400_000;
        let (mut writer, mut reader) = sample_ring(2, 97);
        let producer = std::thread::spawn(move || {
            let mut next = 0u64;
            let mut chunk = [0.0f32; 2 * 61];
            let mut size = 1;
            while next < FRAMES {
                size = size % 61 + 1;
                let frames = size.min((FRAMES - next) as usize);
                for (index, frame) in chunk[..frames * 2]
                    .as_chunks_mut::<2>()
                    .0
                    .iter_mut()
                    .enumerate()
                {
                    // Exact in f32: below 2^24, and never 0 (silence).
                    let value = ((next + index as u64) % (1 << 20) + 1) as f32;
                    *frame = [value, -value];
                }
                let stored = writer.write(&chunk[..frames * 2]);
                next += stored as u64;
                if stored < frames {
                    std::thread::yield_now();
                }
            }
            writer.stats().overrun_frames
        });
        let mut expected = 0u64;
        let mut out = [0.0f32; 2 * 53];
        let mut size = 1;
        while expected < FRAMES {
            size = size % 53 + 1;
            let taken = reader.read(&mut out[..size * 2]);
            for frame in out[..taken * 2].as_chunks::<2>().0 {
                let value = (expected % (1 << 20) + 1) as f32;
                assert_eq!(*frame, [value, -value], "frame {expected}");
                expected += 1;
            }
            assert!(out[taken * 2..size * 2].iter().all(|sample| *sample == 0.0));
            if taken < size {
                std::thread::yield_now();
            }
        }
        // Overruns count frames the producer then retried, so any number
        // is consistent; every frame still arrived once.
        producer.join().expect("producer");
        assert_eq!(reader.stats().read_frames, FRAMES);
        assert_eq!(reader.fill_frames(), 0);
    }
}
