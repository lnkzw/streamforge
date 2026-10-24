//! Feeding a playback device (the monitor, ADR 0017) from the mixer.
//!
//! The device runs on its own clock and pulls from a [`SampleReader`] on its
//! real-time thread; the mixer writes each block through a [`PlaybackFeed`]
//! into the matching [`SampleWriter`]. It is the mirror of a live input
//! (`live`): there the mixer consumes, here it produces, so the same
//! controller holds the ring at a target fill from this side.
//!
//! - **Target:** the larger of two blocks and the device's largest recent
//!   pull plus one block ("recent" as for live inputs: the current or
//!   previous 1 s window), capped below the ring capacity.
//! - **Drift:** each block is resampled by a ratio of `1 + KP * (fill -
//!   target)`, clamped to ±[`MAX_CORRECTION`], with the fill smoothed as for
//!   live inputs; linear interpolation, continuous across blocks, one frame
//!   of delay at ratio 1. `drift_ppm` is the device clock's offset against
//!   the mixer clock (positive: the device runs fast).
//! - **Padding:** before the first block, after the device ran short (an
//!   underrun, counted by the ring), and when the target grows (a larger
//!   device quantum), the ring is topped up with silence to the target, so
//!   the next pulls find enough. Counted as `padded_frames`.
//! - **Trim:** far above the target (the device stalled), the block is left
//!   out, counted as `trimmed_frames`. Frames that do not fit at all are the
//!   ring's overrun.
//! - **On and off** ramp over one block, so switching the monitor does not
//!   click; off keeps feeding silence, so the device keeps running.
//!
//! `write_block` does not allocate.

use media_core::{SampleRingStats, SampleWriter};
use serde::Serialize;

use crate::live::{FILL_SMOOTHING, KP, MAX_CORRECTION, TRIM_MARGIN_BLOCKS};
use crate::mixer::GainRamp;
use crate::runner::{BLOCK_FRAMES, CHANNELS};

const CH: usize = CHANNELS as usize;
/// Blocks per window for the recent pull peak (1 s at 10 ms).
const PEAK_WINDOW_BLOCKS: u32 = 100;
/// Output frames buffered before they go into the ring: one block, its
/// largest resampling stretch and some slack.
const OUT_FRAMES: usize = BLOCK_FRAMES + BLOCK_FRAMES / 100 + 8;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlaybackFeedStats {
    pub ring: SampleRingStats,
    pub fill_frames: usize,
    /// The device's largest recent pull, which the target follows.
    pub pull_frames: usize,
    pub target_frames: usize,
    pub drift_ppm: f64,
    pub padded_frames: u64,
    pub trimmed_frames: u64,
}

pub struct PlaybackFeed {
    writer: SampleWriter,
    gain: GainRamp,
    out: [f32; OUT_FRAMES * CH],
    out_len: usize,
    prev: [f32; CH],
    next: [f32; CH],
    /// Output position between `prev` (0) and `next` (1).
    frac: f64,
    /// Mixer frames consumed per device frame.
    ratio: f64,
    fill_ema: f64,
    target: usize,
    /// The target the ring was last topped up to.
    padded_to: usize,
    underruns_seen: u64,
    peak_current: usize,
    peak_previous: usize,
    peak_blocks: u32,
    padded: u64,
    trimmed: u64,
}

impl PlaybackFeed {
    pub fn new(writer: SampleWriter, enabled: bool) -> Self {
        Self {
            writer,
            gain: GainRamp::new(if enabled { 1.0 } else { 0.0 }),
            out: [0.0; OUT_FRAMES * CH],
            out_len: 0,
            prev: [0.0; CH],
            next: [0.0; CH],
            frac: 0.0,
            ratio: 1.0,
            fill_ema: 0.0,
            target: 0,
            padded_to: 0,
            underruns_seen: 0,
            peak_current: 0,
            peak_previous: 0,
            peak_blocks: 0,
            padded: 0,
            trimmed: 0,
        }
    }

    /// Play the following blocks (ramped in), or silence (ramped out).
    pub fn set_enabled(&mut self, enabled: bool) {
        self.gain.set_target(if enabled { 1.0 } else { 0.0 });
    }

    /// Whether the device's end of the ring is gone.
    pub fn is_closed(&self) -> bool {
        self.writer.is_closed()
    }

    pub fn stats(&self) -> PlaybackFeedStats {
        PlaybackFeedStats {
            ring: self.writer.stats(),
            fill_frames: self.writer.fill_frames(),
            pull_frames: self.recent_peak(),
            target_frames: self.target,
            drift_ppm: (1.0 / self.ratio - 1.0) * 1e6,
            padded_frames: self.padded,
            trimmed_frames: self.trimmed,
        }
    }

    /// Resample one block of interleaved stereo onto the device clock and
    /// queue it.
    pub fn write_block(&mut self, block: &[f32]) {
        if self.writer.is_closed() {
            return;
        }
        let frames = block.len() / CH;
        self.update_target(frames);
        let mut fill = self.writer.fill_frames();
        let underruns = self.writer.stats().underrun_frames;
        let wanted = if underruns > self.underruns_seen {
            self.target
        } else if self.target > self.padded_to {
            // Only the growth: what is there already counts.
            fill + (self.target - self.padded_to)
        } else {
            0
        };
        self.underruns_seen = underruns;
        self.padded_to = self.target;
        if wanted > fill {
            self.pad(wanted.min(self.target).saturating_sub(fill));
            fill = self.writer.fill_frames();
            self.fill_ema = fill as f64;
        }

        let trim_above = self.target + TRIM_MARGIN_BLOCKS * frames + self.recent_peak();
        if fill > trim_above {
            // Left out, but the next block goes on from its end, and a
            // ramp due now plays in the next block that is played.
            self.trimmed += frames as u64;
            if let Some(last) = block.as_chunks::<CH>().0.last() {
                let gain = self.gain.current();
                self.next = last.map(|sample| sample * gain);
            }
            return;
        }

        self.fill_ema += FILL_SMOOTHING * (fill as f64 - self.fill_ema);
        let error = self.fill_ema - self.target as f64;
        self.ratio = 1.0 + (KP * error).clamp(-MAX_CORRECTION, MAX_CORRECTION);

        for (index, source) in block.as_chunks::<CH>().0.iter().enumerate() {
            let gain = self.gain.at(index, frames);
            self.prev = self.next;
            self.next = source.map(|sample| sample * gain);
            while self.frac < 1.0 {
                if self.out_len == self.out.len() {
                    self.flush();
                }
                let t = self.frac as f32;
                for channel in 0..CH {
                    let (a, b) = (self.prev[channel], self.next[channel]);
                    self.out[self.out_len + channel] = a + (b - a) * t;
                }
                self.out_len += CH;
                self.frac += self.ratio;
            }
            self.frac -= 1.0;
        }
        self.gain.finish_block();
        self.flush();
    }

    fn flush(&mut self) {
        // What does not fit is the ring's overrun.
        self.writer.write(&self.out[..self.out_len]);
        self.out_len = 0;
    }

    fn pad(&mut self, frames: usize) {
        let mut left = frames;
        while left > 0 {
            let chunk = left.min(OUT_FRAMES);
            self.out[..chunk * CH].fill(0.0);
            let stored = self.writer.write(&self.out[..chunk * CH]);
            self.padded += stored as u64;
            if stored < chunk {
                break;
            }
            left -= chunk;
        }
    }

    fn recent_peak(&self) -> usize {
        self.peak_current.max(self.peak_previous)
    }

    fn update_target(&mut self, block: usize) {
        self.peak_current = self.peak_current.max(self.writer.take_read_peak());
        self.peak_blocks += 1;
        if self.peak_blocks == PEAK_WINDOW_BLOCKS {
            self.peak_previous = self.peak_current;
            self.peak_current = 0;
            self.peak_blocks = 0;
        }
        let wanted = (2 * block).max(self.recent_peak().saturating_add(block));
        let ceiling = self
            .writer
            .capacity_frames()
            .saturating_sub(block)
            .max(block);
        self.target = wanted.min(ceiling);
    }
}

#[cfg(test)]
mod tests {
    use media_core::{SampleReader, sample_ring};

    use super::*;

    /// A device pulling `quantum` frames at a time on its own clock.
    struct Simulation {
        feed: PlaybackFeed,
        reader: SampleReader,
        rate: f64,
        quantum: usize,
        next_pull: f64,
        /// Mixer blocks run so far: the simulation's clock.
        blocks: u64,
        played: Vec<f32>,
        fills: Vec<usize>,
    }

    impl Simulation {
        fn new(drift_ppm: f64, quantum: usize) -> Self {
            let (writer, reader) = sample_ring(2, 24_000);
            Self {
                feed: PlaybackFeed::new(writer, true),
                reader,
                rate: 48_000.0 * (1.0 + drift_ppm * 1e-6),
                quantum,
                next_pull: 0.0,
                blocks: 0,
                played: Vec::new(),
                fills: Vec::new(),
            }
        }

        /// Run `blocks` mixer blocks of `source(block)`; the device skips
        /// its pulls during `device_stall`, the mixer its blocks during
        /// `mixer_stall` (in seconds).
        fn run(
            &mut self,
            blocks: u64,
            source: impl Fn(u64) -> f32,
            device_stall: Option<(f64, f64)>,
            mixer_stall: Option<(f64, f64)>,
        ) {
            let mut pull = vec![0.0; self.quantum * 2];
            let mut block = [0.0; BLOCK_FRAMES * 2];
            let stalled = |stall: Option<(f64, f64)>, at: f64| {
                stall.is_some_and(|(from, to)| (from..to).contains(&at))
            };
            for index in self.blocks..self.blocks + blocks {
                let now = index as f64 * 0.01;
                while self.next_pull <= now {
                    if !stalled(device_stall, self.next_pull) {
                        self.reader.read(&mut pull);
                        self.played.extend_from_slice(&pull);
                    }
                    self.next_pull += self.quantum as f64 / self.rate;
                }
                if !stalled(mixer_stall, now) {
                    for (frame, out) in block.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                        let value = source(index * BLOCK_FRAMES as u64 + frame as u64);
                        *out = [value, -value];
                    }
                    self.fills.push(self.reader.fill_frames());
                    self.feed.write_block(&block);
                }
            }
            self.blocks += blocks;
        }
    }

    #[test]
    fn drifting_devices_are_fed_without_gaps() {
        for drift in [-500.0, -50.0, 0.0, 120.0, 500.0] {
            for quantum in [256, 1024] {
                let mut sim = Simulation::new(drift, quantum);
                // 120 s: well past the controller's settling time.
                sim.run(12_000, |_| 0.5, None, None);
                let stats = sim.feed.stats();
                let ring = stats.ring;
                assert_eq!(ring.underrun_frames, 0, "{drift} ppm, {quantum}: {stats:?}");
                assert_eq!(ring.overrun_frames, 0, "{stats:?}");
                assert_eq!(stats.trimmed_frames, 0, "{stats:?}");
                assert!(
                    (stats.drift_ppm - drift).abs() < 0.1 * drift.abs() + 15.0,
                    "{drift} ppm, {quantum}: estimated {}",
                    stats.drift_ppm
                );
                assert_eq!(stats.pull_frames, quantum);
                assert_eq!(
                    stats.target_frames,
                    (quantum + BLOCK_FRAMES).max(2 * BLOCK_FRAMES)
                );
                // Padding only at the start (the first target and its growth
                // with the first pull).
                assert!(
                    stats.padded_frames <= stats.target_frames as u64,
                    "{stats:?}"
                );
                // A constant passes unchanged once it arrived.
                let settled = &sim.played[sim.played.len() / 2..];
                assert!(settled.as_chunks::<2>().0.iter().all(|f| *f == [0.5, -0.5]));
                // The fill stays near the target, off by the proportional
                // controller's error (drift / KP frames): latency does not
                // creep.
                let late = &sim.fills[sim.fills.len() / 2..];
                let mean = late.iter().sum::<usize>() as f64 / late.len() as f64;
                let error = drift.abs() * 1e-6 / KP;
                assert!(
                    (mean - stats.target_frames as f64).abs() < error + quantum as f64,
                    "{drift} ppm, {quantum}: mean fill {mean} for target {}",
                    stats.target_frames
                );
            }
        }
    }

    #[test]
    fn a_tone_stays_continuous_through_the_resampler() {
        let mut sim = Simulation::new(300.0, 1024);
        let tone =
            |frame: u64| 0.5 * (frame as f64 * 2.0 * std::f64::consts::PI / 48.0).sin() as f32;
        sim.run(3_000, tone, None, None);
        let settled = &sim.played[sim.played.len() / 2..];
        // 1 kHz at 48 kHz moves at most 0.5 * 2π/48 ≈ 0.065 per frame.
        let max_step = settled
            .as_chunks::<2>()
            .0
            .windows(2)
            .map(|pair| (pair[1][0] - pair[0][0]).abs())
            .fold(0.0, f32::max);
        assert!(max_step < 0.07, "a jump of {max_step}");
        let peak = settled.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        assert!((peak - 0.5).abs() < 0.01, "peak {peak}");
    }

    #[test]
    fn a_mixer_stall_is_counted_and_padded_once() {
        let mut sim = Simulation::new(0.0, 1024);
        sim.run(3_000, |_| 0.25, None, Some((10.0, 10.1)));
        let stats = sim.feed.stats();
        assert!(stats.ring.underrun_frames > 0, "{stats:?}");
        // The start, then one top-up after the stall.
        assert!(
            stats.padded_frames > stats.target_frames as u64
                && stats.padded_frames <= 2 * stats.target_frames as u64 + BLOCK_FRAMES as u64,
            "{stats:?}"
        );
        let underruns = stats.ring.underrun_frames;
        sim.run(1_000, |_| 0.25, None, None);
        assert_eq!(sim.feed.stats().ring.underrun_frames, underruns);
    }

    #[test]
    fn a_device_stall_is_trimmed_not_overrun() {
        let mut sim = Simulation::new(0.0, 1024);
        sim.run(2_000, |_| 0.25, Some((5.0, 5.5)), None);
        let stats = sim.feed.stats();
        assert!(stats.trimmed_frames > 0, "{stats:?}");
        assert_eq!(stats.ring.overrun_frames, 0, "{stats:?}");
        let ceiling = stats.target_frames + TRIM_MARGIN_BLOCKS * BLOCK_FRAMES + 1024 + BLOCK_FRAMES;
        assert!(sim.fills.iter().all(|fill| *fill <= ceiling), "{stats:?}");
    }

    #[test]
    fn off_ramps_out_and_keeps_the_device_fed() {
        let mut sim = Simulation::new(0.0, 1024);
        sim.run(500, |_| 0.5, None, None);
        sim.feed.set_enabled(false);
        sim.run(500, |_| 0.5, None, None);
        let tail = &sim.played[sim.played.len() - 2 * 48_000..];
        assert!(tail.iter().all(|sample| *sample == 0.0));
        assert_eq!(sim.feed.stats().ring.underrun_frames, 0);
        // The ramp: no step bigger than one block's share of the level
        // (after the start, where the signal itself steps from silence).
        let max_step = sim.played[2 * 48_000..]
            .as_chunks::<2>()
            .0
            .windows(2)
            .map(|pair| (pair[1][0] - pair[0][0]).abs())
            .fold(0.0, f32::max);
        assert!(max_step < 0.01, "a jump of {max_step}");
    }

    #[test]
    fn a_ramp_due_while_blocks_are_trimmed_still_ramps() {
        let mut sim = Simulation::new(0.0, 1024);
        // The device stalls from 5 s to 5.5 s; the monitor goes off at 5.2 s,
        // while blocks are being left out.
        sim.run(520, |_| 0.5, Some((5.0, 5.5)), None);
        assert!(sim.feed.stats().trimmed_frames > 0);
        sim.feed.set_enabled(false);
        sim.run(500, |_| 0.5, Some((5.0, 5.5)), None);
        let max_step = sim.played[2 * 48_000..]
            .as_chunks::<2>()
            .0
            .windows(2)
            .map(|pair| (pair[1][0] - pair[0][0]).abs())
            .fold(0.0, f32::max);
        assert!(max_step < 0.01, "a jump of {max_step}");
        let tail = &sim.played[sim.played.len() - 48_000..];
        assert!(tail.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn a_closed_ring_is_left_alone() {
        let (writer, reader) = sample_ring(2, 4_800);
        let mut feed = PlaybackFeed::new(writer, true);
        drop(reader);
        assert!(feed.is_closed());
        feed.write_block(&[0.5; BLOCK_FRAMES * 2]);
        let stats = feed.stats();
        assert_eq!((stats.ring.written_frames, stats.padded_frames), (0, 0));
    }
}
