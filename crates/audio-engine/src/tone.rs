//! Deterministic sine test tone.

use std::f64::consts::TAU;

#[derive(Debug, Clone, PartialEq)]
pub struct Tone {
    amplitude: f64,
    /// Phase in cycles, `0.0..1.0`.
    phase: f64,
    /// Phase advance per frame, in cycles.
    step: f64,
}

impl Tone {
    /// A sine at `frequency_hz` with peak level `level_dbfs` (<= 0).
    pub fn new(frequency_hz: f64, level_dbfs: f64, rate: u32) -> Self {
        Self {
            amplitude: 10f64.powf(level_dbfs / 20.0),
            phase: 0.0,
            step: frequency_hz / f64::from(rate),
        }
    }

    /// Write the next `out.len() / channels` frames; every channel gets the
    /// same sample.
    pub fn fill(&mut self, out: &mut [f32], channels: usize) {
        for frame in out.chunks_exact_mut(channels.max(1)) {
            let sample = (self.amplitude * (TAU * self.phase).sin()) as f32;
            frame.fill(sample);
            self.phase = (self.phase + self.step).fract();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tone_is_deterministic_and_has_the_requested_level() {
        let mut a = Tone::new(1000.0, -6.0, 48_000);
        let mut b = Tone::new(1000.0, -6.0, 48_000);
        let mut out_a = vec![0.0; 48_000 * 2];
        let mut out_b = vec![0.0; 48_000 * 2];
        a.fill(&mut out_a, 2);
        b.fill(&mut out_b, 2);
        assert_eq!(out_a, out_b);
        let peak = out_a.iter().fold(0f32, |m, s| m.max(s.abs()));
        let expected = 10f32.powf(-6.0 / 20.0);
        assert!((peak - expected).abs() < 1e-4, "{peak}");
        // Both channels carry the same sample.
        assert!(out_a.as_chunks::<2>().0.iter().all(|f| f[0] == f[1]));
    }

    #[test]
    fn tone_continues_across_blocks() {
        let mut once = Tone::new(440.0, 0.0, 48_000);
        let mut split = once.clone();
        let mut whole = vec![0.0; 960];
        once.fill(&mut whole, 1);
        let mut first = vec![0.0; 480];
        let mut second = vec![0.0; 480];
        split.fill(&mut first, 1);
        split.fill(&mut second, 1);
        assert_eq!(whole[..480], first[..]);
        assert_eq!(whole[480..], second[..]);
    }
}
