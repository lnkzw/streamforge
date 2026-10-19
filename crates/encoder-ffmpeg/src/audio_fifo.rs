//! Regroups timestamped interleaved audio blocks into fixed-size encoder
//! frames, keeping sample timestamps continuous.
//!
//! - A block that starts later than expected (the producer skipped blocks)
//!   is preceded by silence up to [`MAX_GAP_FILL`] samples (`gap_samples`).
//!   Longer gaps restart the timeline at the block's timestamp
//!   (`discontinuities`).
//! - A block that overlaps already queued samples has its overlapping head
//!   dropped (`overlap_samples`).

/// Longest gap filled with silence: 1 s at 48 kHz.
pub const MAX_GAP_FILL: u64 = 48_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FifoStats {
    pub gap_samples: u64,
    pub overlap_samples: u64,
    pub discontinuities: u64,
}

#[derive(Debug)]
pub struct AudioFifo {
    channels: usize,
    /// Interleaved samples waiting to be encoded.
    samples: Vec<f32>,
    /// Timestamp (in sample frames) of `samples[0]`.
    start: Option<i64>,
    stats: FifoStats,
}

impl AudioFifo {
    pub fn new(channels: usize, capacity_frames: usize) -> Self {
        Self {
            channels: channels.max(1),
            samples: Vec::with_capacity(capacity_frames * channels.max(1)),
            start: None,
            stats: FifoStats::default(),
        }
    }

    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels
    }

    pub fn stats(&self) -> FifoStats {
        self.stats
    }

    /// Queue an interleaved block whose first frame has timestamp `pts`.
    /// Returns `true` if the timeline restarted, so the caller can flush
    /// queued samples first (they no longer connect to this block).
    pub fn push(&mut self, block: &[f32], pts: i64) -> bool {
        let block = &block[..block.len() - block.len() % self.channels];
        let Some(start) = self.start else {
            self.start = Some(pts);
            self.samples.extend_from_slice(block);
            return false;
        };
        let expected = start + self.frames() as i64;
        let mut block = block;
        if pts > expected {
            let gap = (pts - expected) as u64;
            if gap > MAX_GAP_FILL {
                self.stats.discontinuities += 1;
                return true;
            }
            self.samples
                .resize(self.samples.len() + gap as usize * self.channels, 0.0);
            self.stats.gap_samples += gap;
        } else if pts < expected {
            let overlap = ((expected - pts) as usize).min(block.len() / self.channels);
            block = &block[overlap * self.channels..];
            self.stats.overlap_samples += overlap as u64;
        }
        self.samples.extend_from_slice(block);
        false
    }

    /// After `push` returned `true` and the caller flushed: start a new
    /// timeline with `block` at `pts`.
    pub fn restart(&mut self, block: &[f32], pts: i64) {
        self.samples.clear();
        self.start = Some(pts);
        self.samples
            .extend_from_slice(&block[..block.len() - block.len() % self.channels]);
    }

    /// Remove up to `frames` frames from the front, de-interleaving them into
    /// `planes` (one slice per channel). Returns the timestamp of the first
    /// removed frame and the number of frames removed.
    pub fn pop_planar(&mut self, frames: usize, planes: &mut [&mut [f32]]) -> Option<(i64, usize)> {
        let start = self.start?;
        let count = frames.min(self.frames());
        if count == 0 {
            return None;
        }
        for (index, frame) in self.samples[..count * self.channels]
            .chunks_exact(self.channels)
            .enumerate()
        {
            for (channel, plane) in planes.iter_mut().enumerate() {
                plane[index] = frame[channel];
            }
        }
        self.samples.drain(..count * self.channels);
        self.start = Some(start + count as i64);
        Some((start, count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(values: &[f32]) -> Vec<f32> {
        values.iter().flat_map(|&v| [v, -v]).collect()
    }

    fn pop(fifo: &mut AudioFifo, frames: usize) -> Option<(i64, Vec<f32>, Vec<f32>)> {
        let (mut left, mut right) = (vec![0.0; frames], vec![0.0; frames]);
        let (pts, count) = fifo.pop_planar(frames, &mut [&mut left, &mut right])?;
        left.truncate(count);
        right.truncate(count);
        Some((pts, left, right))
    }

    #[test]
    fn contiguous_blocks_regroup_with_continuous_timestamps() {
        let mut fifo = AudioFifo::new(2, 16);
        fifo.push(&block(&[1.0, 2.0, 3.0]), 100);
        fifo.push(&block(&[4.0, 5.0]), 103);
        let (pts, left, right) = pop(&mut fifo, 4).expect("frame");
        assert_eq!(pts, 100);
        assert_eq!(left, vec![1.0, 2.0, 3.0, 4.0]);
        assert_eq!(right, vec![-1.0, -2.0, -3.0, -4.0]);
        let (pts, left, _) = pop(&mut fifo, 4).expect("partial");
        assert_eq!((pts, left), (104, vec![5.0]));
        assert!(pop(&mut fifo, 4).is_none());
        assert_eq!(fifo.stats(), FifoStats::default());
    }

    #[test]
    fn gaps_are_filled_with_silence_and_counted() {
        let mut fifo = AudioFifo::new(2, 16);
        fifo.push(&block(&[1.0]), 0);
        assert!(!fifo.push(&block(&[2.0]), 3));
        let (_, left, _) = pop(&mut fifo, 8).expect("frame");
        assert_eq!(left, vec![1.0, 0.0, 0.0, 2.0]);
        assert_eq!(fifo.stats().gap_samples, 2);
    }

    #[test]
    fn overlaps_drop_the_repeated_head() {
        let mut fifo = AudioFifo::new(2, 16);
        fifo.push(&block(&[1.0, 2.0]), 10);
        fifo.push(&block(&[9.0, 3.0]), 11);
        let (_, left, _) = pop(&mut fifo, 8).expect("frame");
        assert_eq!(left, vec![1.0, 2.0, 3.0]);
        assert_eq!(fifo.stats().overlap_samples, 1);
    }

    #[test]
    fn huge_gaps_restart_the_timeline() {
        let mut fifo = AudioFifo::new(2, 16);
        fifo.push(&block(&[1.0]), 0);
        assert!(fifo.push(&block(&[2.0]), MAX_GAP_FILL as i64 + 10));
        assert_eq!(fifo.stats().discontinuities, 1);
        // The caller flushes the old samples, then restarts.
        let (pts, left, _) = pop(&mut fifo, 8).expect("old");
        assert_eq!((pts, left), (0, vec![1.0]));
        fifo.restart(&block(&[2.0]), MAX_GAP_FILL as i64 + 10);
        let (pts, left, _) = pop(&mut fifo, 8).expect("new");
        assert_eq!((pts, left), (MAX_GAP_FILL as i64 + 10, vec![2.0]));
    }
}
