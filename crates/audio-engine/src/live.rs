//! Live input buffering with adaptive resampling.
//!
//! A live input is produced on its own clock (device or graph) and consumed
//! on the mixer's block clock. Two clocks always drift apart. Instead of
//! padding or trimming, which is audible, each input is resampled by a tiny
//! ratio that keeps its buffer fill at a target:
//!
//! - **Fill:** frames in the ring plus the local read-ahead, smoothed with an
//!   exponential moving average (time constant about 2 s), which averages out
//!   the producer's quantum sawtooth.
//! - **Controller:** proportional. The ratio is `1 + KP * (fill - target)`,
//!   clamped to ±[`MAX_CORRECTION`]. At equilibrium the ratio equals the clock
//!   offset, reported as `drift_ppm`, with a fill error of `drift / KP`
//!   frames (about 50 frames, or 1 ms, per 50 ppm).
//! - **Interpolation:** linear between consecutive source frames, continuous
//!   across blocks. At ratio 1 the input passes through unchanged, delayed by
//!   one frame.
//!
//! Real stalls are still handled explicitly and counted:
//! - **Warm-up:** silence until the fill reaches the target.
//! - **Underrun:** when the producer stops delivering, the rest of the block is
//!   padded with silence and the input warms up again.
//! - **Trim:** after a burst far above target (e.g. the mixer was paused), the
//!   excess is trimmed.
//!
//! The target is the larger of two blocks and the largest recent producer push
//! plus one block. "Recent" means the current or previous 1 s window, so a
//! single burst does not inflate latency for good. The target is also capped
//! below the ring capacity, so warm-up always finishes.
//!
//! `read_block` does not allocate.

use std::sync::Arc;

use media_core::AudioRing;

/// Interleaved channels; the mixer is stereo.
const CHANNELS: usize = 2;
/// Source frames pulled from the ring at once into the local read-ahead.
const READ_AHEAD_FRAMES: usize = 128;
/// EMA weight of a new fill sample per block (time constant about 2 s at 10 ms).
pub(crate) const FILL_SMOOTHING: f64 = 0.005;
/// Ratio change per frame of fill error.
pub(crate) const KP: f64 = 1e-6;
/// Largest resampling correction (±2000 ppm, about 3.5 cents).
pub const MAX_CORRECTION: f64 = 0.002;
/// Blocks per window for the recent push peak (1 s at 10 ms).
const PEAK_WINDOW_BLOCKS: u32 = 100;
/// Trim when the fill exceeds target + this many blocks + the push peak.
pub(crate) const TRIM_MARGIN_BLOCKS: usize = 4;

/// Frames produced by one `read_block` call, by category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct BlockOutcome {
    pub warmup: u64,
    pub underrun: u64,
    pub trimmed: u64,
}

pub(crate) struct LiveInput {
    ring: Arc<AudioRing>,
    capacity: usize,
    local: [f32; READ_AHEAD_FRAMES * CHANNELS],
    local_read: usize,
    local_len: usize,
    prev: [f32; CHANNELS],
    next: [f32; CHANNELS],
    /// Position between `prev` (0) and `next` (1).
    frac: f64,
    /// Source frames consumed per output frame.
    ratio: f64,
    fill_ema: f64,
    warming_up: bool,
    target: usize,
    peak_current: usize,
    peak_previous: usize,
    peak_blocks: u32,
}

impl LiveInput {
    pub fn new(ring: Arc<AudioRing>) -> Self {
        let capacity = ring.capacity_frames();
        Self {
            ring,
            capacity,
            local: [0.0; READ_AHEAD_FRAMES * CHANNELS],
            local_read: 0,
            local_len: 0,
            prev: [0.0; CHANNELS],
            next: [0.0; CHANNELS],
            frac: 0.0,
            ratio: 1.0,
            fill_ema: 0.0,
            warming_up: true,
            target: 0,
            peak_current: 0,
            peak_previous: 0,
            peak_blocks: 0,
        }
    }

    pub fn ring(&self) -> &AudioRing {
        &self.ring
    }

    /// Current target fill in frames.
    pub fn target(&self) -> usize {
        self.target
    }

    /// Estimated producer clock offset against the mixer clock.
    pub fn drift_ppm(&self) -> f64 {
        (self.ratio - 1.0) * 1e6
    }

    /// Frames buffered ahead of the output: ring plus local read-ahead.
    pub fn fill(&self) -> usize {
        self.ring.fill_frames() + (self.local_len - self.local_read) / CHANNELS
    }

    /// Write one block of interleaved stereo into `out`. Returns the fill
    /// before reading (for latency statistics) and what happened.
    pub fn read_block(&mut self, out: &mut [f32]) -> (Option<usize>, BlockOutcome) {
        let block = out.len() / CHANNELS;
        self.update_target(block);
        let mut outcome = BlockOutcome::default();
        let mut fill = self.fill();

        if self.warming_up {
            if fill < self.target {
                out.fill(0.0);
                outcome.warmup = block as u64;
                return (None, outcome);
            }
            if !self.prime() {
                out.fill(0.0);
                outcome.warmup = block as u64;
                return (None, outcome);
            }
            self.warming_up = false;
            fill = self.fill();
            self.fill_ema = fill as f64;
        }

        let trim_above = self.target + TRIM_MARGIN_BLOCKS * block + self.recent_push_peak();
        if fill > trim_above {
            let local = (self.local_len - self.local_read) / CHANNELS;
            let excess = fill - self.target;
            outcome.trimmed = self.ring.discard(excess.saturating_sub(local)) as u64;
            fill = self.fill();
            self.fill_ema = fill as f64;
        }

        self.fill_ema += FILL_SMOOTHING * (fill as f64 - self.fill_ema);
        let error = self.fill_ema - self.target as f64;
        self.ratio = 1.0 + (KP * error).clamp(-MAX_CORRECTION, MAX_CORRECTION);

        for (index, frame) in out.as_chunks_mut::<CHANNELS>().0.iter_mut().enumerate() {
            while self.frac >= 1.0 {
                match self.pop() {
                    Some(source) => {
                        self.prev = self.next;
                        self.next = source;
                        self.frac -= 1.0;
                    }
                    None => {
                        // Producer stalled: pad the rest and rebuild the cushion.
                        out[index * CHANNELS..].fill(0.0);
                        outcome.underrun = (block - index) as u64;
                        self.warming_up = true;
                        return (Some(fill), outcome);
                    }
                }
            }
            let t = self.frac as f32;
            for (channel, sample) in frame.iter_mut().enumerate() {
                let (a, b) = (self.prev[channel], self.next[channel]);
                *sample = a + (b - a) * t;
            }
            self.frac += self.ratio;
        }
        (Some(fill), outcome)
    }

    /// Load the first two source frames after warm-up.
    fn prime(&mut self) -> bool {
        match (self.pop(), self.pop()) {
            (Some(first), Some(second)) => {
                self.prev = first;
                self.next = second;
                self.frac = 0.0;
                true
            }
            _ => false,
        }
    }

    fn pop(&mut self) -> Option<[f32; CHANNELS]> {
        if self.local_read == self.local_len {
            let frames = self.ring.pull(&mut self.local);
            if frames == 0 {
                return None;
            }
            self.local_read = 0;
            self.local_len = frames * CHANNELS;
        }
        let frame = [self.local[self.local_read], self.local[self.local_read + 1]];
        self.local_read += CHANNELS;
        Some(frame)
    }

    fn recent_push_peak(&self) -> usize {
        self.peak_current.max(self.peak_previous)
    }

    fn update_target(&mut self, block: usize) {
        self.peak_current = self.peak_current.max(self.ring.take_push_peak());
        self.peak_blocks += 1;
        if self.peak_blocks == PEAK_WINDOW_BLOCKS {
            self.peak_previous = self.peak_current;
            self.peak_current = 0;
            self.peak_blocks = 0;
        }
        let wanted = (2 * block).max(self.recent_push_peak().saturating_add(block));
        // Warm-up must be able to finish even after an oversized push.
        let ceiling = self.capacity.saturating_sub(block).max(block);
        self.target = wanted.min(ceiling);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: usize = 480;
    const RAMP_PERIOD: u64 = 1 << 16;

    struct Simulation {
        ring: Arc<AudioRing>,
        input: LiveInput,
        produced: u64,
        next_push: f64,
        producer_rate: f64,
        quantum: usize,
        last_output: Option<f32>,
        max_step_error: f32,
        outcome: BlockOutcome,
        fills: Vec<usize>,
    }

    impl Simulation {
        fn new(drift_ppm: f64, quantum: usize) -> Self {
            let ring = Arc::new(AudioRing::new(2, 48_000));
            Self {
                input: LiveInput::new(Arc::clone(&ring)),
                ring,
                produced: 0,
                next_push: 0.0,
                producer_rate: 48_000.0 * (1.0 + drift_ppm * 1e-6),
                quantum,
                last_output: None,
                max_step_error: 0.0,
                outcome: BlockOutcome::default(),
                fills: Vec::new(),
            }
        }

        /// Run `blocks` mixer blocks; the producer pushes a ramp (left =
        /// frame index modulo 2^16, right = its negation) in `quantum`-sized
        /// chunks on its clock. 2^16 keeps f32 resolution below 0.01.
        fn run(&mut self, blocks: u64, stall: Option<(f64, f64)>) {
            let mut out = vec![0.0; BLOCK * 2];
            let mut chunk = vec![0.0; self.quantum * 2];
            for block in 0..blocks {
                let now = block as f64 * 0.01;
                while self.next_push <= now {
                    let stalled =
                        stall.is_some_and(|(from, to)| (from..to).contains(&self.next_push));
                    if !stalled {
                        for (i, frame) in chunk.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                            let value = ((self.produced + i as u64) % RAMP_PERIOD) as f32;
                            frame[0] = value;
                            frame[1] = -value;
                        }
                        self.ring.push(chunk.iter().copied());
                    }
                    self.produced += self.quantum as u64;
                    self.next_push += self.quantum as f64 / self.producer_rate;
                }
                let (fill, outcome) = self.input.read_block(&mut out);
                self.outcome.warmup += outcome.warmup;
                self.outcome.underrun += outcome.underrun;
                self.outcome.trimmed += outcome.trimmed;
                if let Some(fill) = fill {
                    self.fills.push(fill);
                }
                if outcome == BlockOutcome::default() {
                    // Continuity: a linearly interpolated ramp advances by the
                    // ratio (about 1) per frame, with no jumps.
                    for frame in out.as_chunks::<2>().0 {
                        assert_eq!(frame[1], -frame[0]);
                        // Skip the wrap-around, where interpolation mixes ends.
                        let inside = |v: f32| (1_000.0..64_000.0).contains(&v);
                        if let Some(last) = self.last_output
                            && inside(last)
                            && inside(frame[0])
                        {
                            let step = frame[0] - last;
                            self.max_step_error = self.max_step_error.max((step - 1.0).abs());
                        }
                        self.last_output = Some(frame[0]);
                    }
                } else {
                    self.last_output = None;
                }
            }
        }
    }

    #[test]
    fn slow_device_is_tracked_without_gaps() {
        // Five minutes of mixer blocks.
        let mut sim = Simulation::new(-46.0, 1024);
        sim.run(30_000, None);
        assert_eq!(sim.outcome.underrun, 0, "{:?}", sim.outcome);
        assert_eq!(sim.outcome.trimmed, 0);
        let drift = sim.input.drift_ppm();
        assert!((drift + 46.0).abs() < 5.0, "drift {drift}");
        // Step error = |ratio - 1| plus f32 rounding of large ramp values.
        assert!(sim.max_step_error < 0.01, "{}", sim.max_step_error);
        let late = &sim.fills[sim.fills.len() - 6_000..];
        let target = sim.input.target();
        let mean = late.iter().sum::<usize>() as f64 / late.len() as f64;
        assert!(
            (mean - target as f64).abs() < 100.0,
            "mean {mean} target {target}"
        );
    }

    #[test]
    fn fast_device_is_tracked_without_trimming() {
        let mut sim = Simulation::new(200.0, 1024);
        sim.run(30_000, None);
        assert_eq!(sim.outcome.underrun, 0);
        assert_eq!(sim.outcome.trimmed, 0);
        let drift = sim.input.drift_ppm();
        assert!((drift - 200.0).abs() < 10.0, "drift {drift}");
        assert!(sim.max_step_error < 0.01);
    }

    #[test]
    fn stall_underruns_once_then_recovers() {
        let mut sim = Simulation::new(0.0, 1024);
        // The producer delivers nothing between 10 s and 10.3 s.
        sim.run(3_000, Some((10.0, 10.3)));
        assert!(sim.outcome.underrun > 0);
        assert!(sim.outcome.warmup > 0);
        // After recovery the input plays continuously again.
        assert!(sim.last_output.is_some());
        assert_eq!(sim.outcome.trimmed, 0);
    }

    #[test]
    fn target_follows_recent_pushes_only() {
        let ring = Arc::new(AudioRing::new(2, 48_000));
        let mut input = LiveInput::new(Arc::clone(&ring));
        let mut out = vec![0.0; BLOCK * 2];
        ring.push(std::iter::repeat_n(0.0, 8_192 * 2));
        input.read_block(&mut out);
        assert_eq!(input.target(), 8_192 + BLOCK);
        // Two quiet windows later the burst no longer counts.
        for _ in 0..2 * PEAK_WINDOW_BLOCKS {
            ring.push(std::iter::repeat_n(0.0, 480 * 2));
            input.read_block(&mut out);
        }
        assert_eq!(input.target(), 2 * BLOCK);
    }

    #[test]
    fn oversized_push_cannot_block_warm_up() {
        let ring = Arc::new(AudioRing::new(2, 4_800));
        let mut input = LiveInput::new(Arc::clone(&ring));
        let mut out = vec![0.0; BLOCK * 2];
        // One push larger than the whole ring.
        ring.push(std::iter::repeat_n(0.25, 10_000 * 2));
        let (_, outcome) = input.read_block(&mut out);
        assert!(input.target() <= 4_800 - BLOCK);
        assert_eq!(outcome.warmup, 0, "warm-up must finish");
        assert!(out.iter().all(|s| *s == 0.25));
    }
}
