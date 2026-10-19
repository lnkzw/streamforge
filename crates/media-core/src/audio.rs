//! Bounded ring of interleaved `f32` samples: the graph edge between an
//! audio producer (e.g. a capture callback) and the mixer.
//!
//! Capacity is fixed at construction; pushing never allocates. When full,
//! the oldest frames are overwritten and counted as overrun. The lock is
//! held only while copying samples; producers are PipeWire main-loop
//! callbacks (not RT threads), so this is acceptable for now.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct AudioRingStats {
    pub capacity_frames: u64,
    pub pushed_frames: u64,
    pub pulled_frames: u64,
    /// Frames overwritten because the consumer did not keep up.
    pub overrun_frames: u64,
    /// Frames the consumer dropped on purpose (e.g. to bound latency).
    pub discarded_frames: u64,
    /// Largest single push; the consumer sizes its buffering from it.
    pub max_push_frames: u64,
}

#[derive(Debug)]
struct Inner {
    samples: Box<[f32]>,
    /// Sample index of the oldest stored sample.
    read: usize,
    /// Stored samples (always whole frames).
    len: usize,
}

#[derive(Debug)]
pub struct AudioRing {
    channels: usize,
    inner: Mutex<Inner>,
    pushed: AtomicU64,
    pulled: AtomicU64,
    overrun: AtomicU64,
    discarded: AtomicU64,
    max_push: AtomicU64,
}

impl AudioRing {
    /// A ring holding up to `capacity_frames` frames of `channels` samples.
    pub fn new(channels: u32, capacity_frames: usize) -> Self {
        let channels = channels.max(1) as usize;
        Self {
            channels,
            inner: Mutex::new(Inner {
                samples: vec![0.0; capacity_frames.max(1) * channels].into_boxed_slice(),
                read: 0,
                len: 0,
            }),
            pushed: AtomicU64::new(0),
            pulled: AtomicU64::new(0),
            overrun: AtomicU64::new(0),
            discarded: AtomicU64::new(0),
            max_push: AtomicU64::new(0),
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn capacity_frames(&self) -> usize {
        self.lock().samples.len() / self.channels
    }

    /// Append the whole frames in `samples` (a trailing partial frame is
    /// ignored). Returns the number of frames stored.
    pub fn push<I>(&self, samples: I) -> usize
    where
        I: IntoIterator<Item = f32>,
        I::IntoIter: ExactSizeIterator,
    {
        let mut samples = samples.into_iter();
        let frames = samples.len() / self.channels;
        if frames == 0 {
            return 0;
        }
        let mut inner = self.lock();
        let capacity = inner.samples.len();
        let mut incoming = frames * self.channels;
        let mut overrun = 0;
        // More than fits: keep only the newest frames.
        if incoming > capacity {
            let skip = incoming - capacity;
            for _ in 0..skip {
                samples.next();
            }
            overrun += skip;
            incoming = capacity;
        }
        // Make room by dropping the oldest frames.
        let excess = (inner.len + incoming).saturating_sub(capacity);
        if excess > 0 {
            inner.read = (inner.read + excess) % capacity;
            inner.len -= excess;
            overrun += excess;
        }
        let mut write = (inner.read + inner.len) % capacity;
        for sample in samples.take(incoming) {
            inner.samples[write] = sample;
            write = (write + 1) % capacity;
        }
        inner.len += incoming;
        drop(inner);

        self.pushed.fetch_add(frames as u64, Ordering::Relaxed);
        self.overrun
            .fetch_add((overrun / self.channels) as u64, Ordering::Relaxed);
        self.max_push.fetch_max(frames as u64, Ordering::Relaxed);
        incoming / self.channels
    }

    /// Move up to `out.len() / channels` frames into `out`; returns frames copied.
    pub fn pull(&self, out: &mut [f32]) -> usize {
        let mut inner = self.lock();
        let capacity = inner.samples.len();
        let count = (out.len() / self.channels).min(inner.len / self.channels) * self.channels;
        let first = count.min(capacity - inner.read);
        let read = inner.read;
        out[..first].copy_from_slice(&inner.samples[read..read + first]);
        out[first..count].copy_from_slice(&inner.samples[..count - first]);
        inner.read = (inner.read + count) % capacity;
        inner.len -= count;
        drop(inner);
        let frames = count / self.channels;
        self.pulled.fetch_add(frames as u64, Ordering::Relaxed);
        frames
    }

    /// Drop up to `frames` of the oldest frames; returns frames dropped.
    pub fn discard(&self, frames: usize) -> usize {
        let mut inner = self.lock();
        let capacity = inner.samples.len();
        let count = (frames * self.channels).min(inner.len);
        inner.read = (inner.read + count) % capacity;
        inner.len -= count;
        drop(inner);
        let frames = count / self.channels;
        self.discarded.fetch_add(frames as u64, Ordering::Relaxed);
        frames
    }

    pub fn fill_frames(&self) -> usize {
        self.lock().len / self.channels
    }

    pub fn stats(&self) -> AudioRingStats {
        AudioRingStats {
            capacity_frames: self.capacity_frames() as u64,
            pushed_frames: self.pushed.load(Ordering::Relaxed),
            pulled_frames: self.pulled.load(Ordering::Relaxed),
            overrun_frames: self.overrun.load(Ordering::Relaxed),
            discarded_frames: self.discarded.load(Ordering::Relaxed),
            max_push_frames: self.max_push.load(Ordering::Relaxed),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock only means another holder panicked mid-copy; the
        // indices are still consistent because they are updated after copying.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(values: &[f32]) -> Vec<f32> {
        // Stereo: each value becomes (v, -v).
        values.iter().flat_map(|&v| [v, -v]).collect()
    }

    #[test]
    fn push_and_pull_preserve_order_across_wrap_around() {
        let ring = AudioRing::new(2, 4);
        assert_eq!(ring.push(frames(&[1.0, 2.0, 3.0])), 3);
        let mut out = [0.0; 4];
        assert_eq!(ring.pull(&mut out), 2);
        assert_eq!(out, [1.0, -1.0, 2.0, -2.0]);
        // Wraps: slots 3, 0, 1.
        assert_eq!(ring.push(frames(&[4.0, 5.0, 6.0])), 3);
        assert_eq!(ring.fill_frames(), 4);
        let mut out = [0.0; 8];
        assert_eq!(ring.pull(&mut out), 4);
        assert_eq!(out, [3.0, -3.0, 4.0, -4.0, 5.0, -5.0, 6.0, -6.0]);
        assert_eq!(ring.fill_frames(), 0);
    }

    #[test]
    fn overrun_drops_the_oldest_frames_and_counts_them() {
        let ring = AudioRing::new(2, 3);
        ring.push(frames(&[1.0, 2.0]));
        ring.push(frames(&[3.0, 4.0]));
        let mut out = [0.0; 6];
        assert_eq!(ring.pull(&mut out), 3);
        assert_eq!(out, [2.0, -2.0, 3.0, -3.0, 4.0, -4.0]);
        assert_eq!(ring.stats().overrun_frames, 1);
    }

    #[test]
    fn oversized_push_keeps_the_newest_frames() {
        let ring = AudioRing::new(2, 2);
        assert_eq!(ring.push(frames(&[1.0, 2.0, 3.0, 4.0, 5.0])), 2);
        let mut out = [0.0; 4];
        ring.pull(&mut out);
        assert_eq!(out, [4.0, -4.0, 5.0, -5.0]);
        let stats = ring.stats();
        assert_eq!(stats.overrun_frames, 3);
        assert_eq!(stats.pushed_frames, 5);
        assert_eq!(stats.max_push_frames, 5);
    }

    #[test]
    fn partial_frames_are_ignored_and_pull_is_bounded() {
        let ring = AudioRing::new(2, 4);
        assert_eq!(ring.push([1.0, 2.0, 3.0]), 1);
        let mut out = [9.0; 6];
        assert_eq!(ring.pull(&mut out), 1);
        assert_eq!(out, [1.0, 2.0, 9.0, 9.0, 9.0, 9.0]);
    }

    #[test]
    fn discard_drops_oldest_and_is_counted() {
        let ring = AudioRing::new(1, 8);
        ring.push([1.0, 2.0, 3.0, 4.0]);
        assert_eq!(ring.discard(3), 3);
        assert_eq!(ring.discard(5), 1);
        assert_eq!(ring.fill_frames(), 0);
        let stats = ring.stats();
        assert_eq!(stats.discarded_frames, 4);
        assert_eq!(stats.pulled_frames, 0);
        assert_eq!(stats.capacity_frames, 8);
    }
}
