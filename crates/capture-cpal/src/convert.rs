//! Converts device audio (any channel count and rate) into the engine's
//! interleaved stereo at a fixed rate, inside the audio callback.
//!
//! On Linux PipeWire converts; WASAPI shared mode and CoreAudio deliver
//! the device's own format, so this backend converts itself:
//! - mono is duplicated to both channels; more than two channels keep the
//!   first two (front left and right; no surround downmix);
//! - other rates are resampled linearly (cheap; it slightly softens high
//!   frequencies, like the mixer's drift correction, ADR 0004).
//!
//! It never allocates: output goes into a caller-provided scratch buffer
//! that is flushed whenever it fills.

#[derive(Debug, Clone)]
pub struct Converter {
    in_channels: usize,
    passthrough: bool,
    /// Input frames per output frame.
    step: f64,
    /// Position of the next output frame between `previous` (0.0) and the
    /// frame being consumed (1.0).
    position: f64,
    previous: Option<[f32; 2]>,
}

impl Converter {
    pub fn new(in_channels: u16, in_rate: u32, out_rate: u32) -> Self {
        Self {
            in_channels: usize::from(in_channels.max(1)),
            passthrough: in_rate == out_rate,
            step: f64::from(in_rate) / f64::from(out_rate.max(1)),
            position: 0.0,
            previous: None,
        }
    }

    /// Convert interleaved `input` (`in_channels` samples per frame) into
    /// stereo frames in `scratch`, calling `flush` with each filled part.
    /// `scratch` must hold at least one stereo frame.
    pub fn convert(
        &mut self,
        input: impl Iterator<Item = f32>,
        scratch: &mut [f32],
        mut flush: impl FnMut(&[f32]),
    ) {
        let capacity = scratch.len() - scratch.len() % 2;
        let mut used = 0;
        let mut emit = |frame: [f32; 2], scratch: &mut [f32]| {
            if used == capacity {
                flush(&scratch[..used]);
                used = 0;
            }
            scratch[used] = frame[0];
            scratch[used + 1] = frame[1];
            used += 2;
        };
        let mut channel = 0;
        let mut frame = [0.0f32; 2];
        for sample in input {
            if channel < 2 {
                frame[channel] = sample;
            }
            channel += 1;
            if channel < self.in_channels {
                continue;
            }
            channel = 0;
            if self.in_channels == 1 {
                frame[1] = frame[0];
            }
            if self.passthrough {
                emit(frame, scratch);
                continue;
            }
            let Some(previous) = self.previous.replace(frame) else {
                continue;
            };
            while self.position < 1.0 {
                let t = self.position as f32;
                emit(
                    [
                        previous[0] + (frame[0] - previous[0]) * t,
                        previous[1] + (frame[1] - previous[1]) * t,
                    ],
                    scratch,
                );
                self.position += self.step;
            }
            self.position -= 1.0;
        }
        if used > 0 {
            flush(&scratch[..used]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(converter: &mut Converter, input: &[f32], scratch: usize) -> Vec<f32> {
        let mut out = Vec::new();
        let mut buffer = vec![0.0; scratch];
        converter.convert(input.iter().copied(), &mut buffer, |part| {
            out.extend_from_slice(part)
        });
        out
    }

    #[test]
    fn stereo_at_the_target_rate_passes_through() {
        let mut converter = Converter::new(2, 48_000, 48_000);
        let input = [0.1, -0.1, 0.2, -0.2, 0.3, -0.3];
        assert_eq!(run(&mut converter, &input, 64), input);
    }

    #[test]
    fn mono_is_duplicated_and_extra_channels_are_dropped() {
        let mut mono = Converter::new(1, 48_000, 48_000);
        assert_eq!(run(&mut mono, &[0.5, 0.25], 64), [0.5, 0.5, 0.25, 0.25]);
        let mut surround = Converter::new(6, 48_000, 48_000);
        let frame = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
        assert_eq!(run(&mut surround, &frame, 64), [0.1, 0.2]);
    }

    #[test]
    fn a_small_scratch_buffer_is_flushed_as_it_fills() {
        let mut converter = Converter::new(2, 48_000, 48_000);
        let input: Vec<f32> = (0..20).map(|i| i as f32).collect();
        // Room for 3 frames (an odd length is rounded down to frames).
        assert_eq!(run(&mut converter, &input, 7), input);
    }

    #[test]
    fn resampling_keeps_the_rate_ratio_and_interpolates() {
        // 44.1 kHz in, 48 kHz out: one second becomes about one second.
        let mut converter = Converter::new(1, 44_100, 48_000);
        let input: Vec<f32> = (0..44_100).map(|i| (i % 100) as f32 / 100.0).collect();
        let out = run(&mut converter, &input, 4096);
        let frames = out.len() / 2;
        assert!((frames as i64 - 48_000).abs() <= 2, "{frames} frames");
        // Halving the rate yields every second input frame (linear: exact).
        let mut half = Converter::new(1, 48_000, 24_000);
        let ramp: Vec<f32> = (0..10).map(|i| i as f32).collect();
        let out = run(&mut half, &ramp, 64);
        let left: Vec<f32> = out.chunks(2).map(|frame| frame[0]).collect();
        assert_eq!(left, [0.0, 2.0, 4.0, 6.0, 8.0]);
        // Upsampling a ramp interpolates between neighbours.
        let mut double = Converter::new(1, 24_000, 48_000);
        let out = run(&mut double, &[0.0, 1.0, 2.0], 64);
        let left: Vec<f32> = out.chunks(2).map(|frame| frame[0]).collect();
        assert_eq!(left, [0.0, 0.5, 1.0, 1.5]);
    }

    #[test]
    fn state_carries_across_callbacks() {
        // Feeding in two parts gives the same output as one part.
        let input: Vec<f32> = (0..100).map(|i| (i as f32 * 0.37).sin()).collect();
        let mut whole = Converter::new(1, 44_100, 48_000);
        let expected = run(&mut whole, &input, 4096);
        let mut split = Converter::new(1, 44_100, 48_000);
        let mut out = run(&mut split, &input[..37], 4096);
        out.extend(run(&mut split, &input[37..], 4096));
        assert_eq!(out, expected);
    }
}
