//! Mixer core: per-input gain/mute strips, a master strip, meters and the
//! clipping policy. Pure and deterministic: identical input gives
//! bit-identical output. `mix` does not allocate.

use serde::Serialize;

/// Lowest accepted gain; anything quieter is effectively silence.
pub const MIN_GAIN_DB: f32 = -96.0;
/// Highest accepted gain.
pub const MAX_GAIN_DB: f32 = 24.0;

pub fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Gain that moves to a new target linearly over one block, so gain and
/// mute changes do not click.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GainRamp {
    current: f32,
    target: f32,
}

impl GainRamp {
    pub fn new(linear: f32) -> Self {
        Self {
            current: linear,
            target: linear,
        }
    }

    pub fn set_target(&mut self, linear: f32) {
        self.target = linear;
    }

    /// Gain for frame `index` of a `frames`-long block; reaches the target
    /// on the last frame.
    #[inline]
    fn at(&self, index: usize, frames: usize) -> f32 {
        if self.current == self.target {
            self.current
        } else {
            let t = (index + 1) as f32 / frames as f32;
            self.current + (self.target - self.current) * t
        }
    }

    fn finish_block(&mut self) {
        self.current = self.target;
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MeterReading {
    /// `null` for digital silence.
    pub peak_dbfs: Option<f64>,
    /// `null` for digital silence.
    pub rms_dbfs: Option<f64>,
}

/// Peak and RMS over everything observed since creation.
#[derive(Debug, Clone, Copy, Default)]
pub struct Meter {
    peak: f32,
    sum_squares: f64,
    samples: u64,
}

impl Meter {
    /// Non-finite samples are skipped so one bad value cannot poison the
    /// RMS; the mixer counts them as `invalid_samples` on the master bus.
    #[inline]
    pub fn observe(&mut self, sample: f32) {
        if !sample.is_finite() {
            return;
        }
        self.peak = self.peak.max(sample.abs());
        self.sum_squares += f64::from(sample) * f64::from(sample);
        self.samples += 1;
    }

    pub fn reading(&self) -> MeterReading {
        let to_db = |linear: f64| (linear > 0.0).then(|| 20.0 * linear.log10());
        MeterReading {
            peak_dbfs: to_db(f64::from(self.peak)),
            rms_dbfs: (self.samples > 0)
                .then(|| (self.sum_squares / self.samples as f64).sqrt())
                .and_then(to_db),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StripConfig {
    pub gain_db: f32,
    pub muted: bool,
}

#[derive(Debug, Clone)]
struct Strip {
    config: StripConfig,
    gain: GainRamp,
    meter: Meter,
}

impl Strip {
    fn new(config: StripConfig) -> Self {
        Self {
            config,
            gain: GainRamp::new(effective_gain(config)),
            meter: Meter::default(),
        }
    }
}

fn effective_gain(config: StripConfig) -> f32 {
    if config.muted {
        0.0
    } else {
        db_to_linear(config.gain_db)
    }
}

/// Sums inputs in `f32` without intermediate clipping, applies the master
/// gain, then hard-limits the master bus to `[-1.0, 1.0]`. Every limited
/// sample is counted; non-finite samples become silence and are counted.
#[derive(Debug, Clone)]
pub struct Mixer {
    channels: usize,
    strips: Vec<Strip>,
    master: Strip,
    clipped_samples: u64,
    invalid_samples: u64,
}

impl Mixer {
    pub fn new(channels: usize, inputs: &[StripConfig], master_gain_db: f32) -> Self {
        Self {
            channels: channels.max(1),
            strips: inputs.iter().copied().map(Strip::new).collect(),
            master: Strip::new(StripConfig {
                gain_db: master_gain_db,
                muted: false,
            }),
            clipped_samples: 0,
            invalid_samples: 0,
        }
    }

    pub fn set_input(&mut self, index: usize, config: StripConfig) {
        if let Some(strip) = self.strips.get_mut(index) {
            strip.config = config;
            strip.gain.set_target(effective_gain(config));
        }
    }

    /// Mix one block. `inputs[i]` and `out` hold the same number of
    /// interleaved samples; missing inputs count as silence.
    pub fn mix(&mut self, inputs: &[&[f32]], out: &mut [f32]) {
        let channels = self.channels;
        let frames = out.len() / channels;
        out.fill(0.0);
        for (strip, input) in self.strips.iter_mut().zip(inputs) {
            for (frame, (dst, src)) in out
                .chunks_exact_mut(channels)
                .zip(input.chunks_exact(channels))
                .enumerate()
            {
                let gain = strip.gain.at(frame, frames);
                for (d, s) in dst.iter_mut().zip(src) {
                    let sample = s * gain;
                    strip.meter.observe(sample);
                    *d += sample;
                }
            }
            strip.gain.finish_block();
        }
        for (frame, samples) in out.chunks_exact_mut(channels).enumerate() {
            let gain = self.master.gain.at(frame, frames);
            for sample in samples {
                let mut value = *sample * gain;
                if !value.is_finite() {
                    self.invalid_samples += 1;
                    value = 0.0;
                } else if value.abs() > 1.0 {
                    self.clipped_samples += 1;
                    value = value.clamp(-1.0, 1.0);
                }
                self.master.meter.observe(value);
                *sample = value;
            }
        }
        self.master.gain.finish_block();
    }

    /// Current gain and mute of input `index`.
    pub fn input_config(&self, index: usize) -> Option<StripConfig> {
        self.strips.get(index).map(|strip| strip.config)
    }

    pub fn input_meter(&self, index: usize) -> Option<MeterReading> {
        self.strips.get(index).map(|strip| strip.meter.reading())
    }

    pub fn master_meter(&self) -> MeterReading {
        self.master.meter.reading()
    }

    pub fn clipped_samples(&self) -> u64 {
        self.clipped_samples
    }

    pub fn invalid_samples(&self) -> u64 {
        self.invalid_samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNITY: StripConfig = StripConfig {
        gain_db: 0.0,
        muted: false,
    };

    #[test]
    fn decibel_conversion() {
        assert_eq!(db_to_linear(0.0), 1.0);
        assert!((db_to_linear(-6.0206) - 0.5).abs() < 1e-4);
        assert!((db_to_linear(20.0) - 10.0).abs() < 1e-4);
    }

    #[test]
    fn unity_mix_is_the_exact_sum() {
        let mut mixer = Mixer::new(2, &[UNITY, UNITY], 0.0);
        let a = [0.25, -0.5, 0.125, 0.0];
        let b = [0.25, 0.25, -0.125, 0.5];
        let mut out = [0.0; 4];
        mixer.mix(&[&a, &b], &mut out);
        assert_eq!(out, [0.5, -0.25, 0.0, 0.5]);
        assert_eq!(mixer.clipped_samples(), 0);
    }

    #[test]
    fn mute_silences_an_input_after_a_one_block_ramp() {
        let mut mixer = Mixer::new(1, &[UNITY], 0.0);
        mixer.set_input(
            0,
            StripConfig {
                gain_db: 0.0,
                muted: true,
            },
        );
        let input = [1.0; 4];
        let mut out = [0.0; 4];
        mixer.mix(&[&input], &mut out);
        // Linear ramp 1 -> 0 over the block, reaching 0 on the last frame.
        assert_eq!(out, [0.75, 0.5, 0.25, 0.0]);
        mixer.mix(&[&input], &mut out);
        assert_eq!(out, [0.0; 4]);
    }

    #[test]
    fn muted_at_start_is_silent_immediately() {
        let mut mixer = Mixer::new(
            1,
            &[StripConfig {
                gain_db: 0.0,
                muted: true,
            }],
            0.0,
        );
        let mut out = [1.0; 3];
        mixer.mix(&[&[0.5, 0.5, 0.5]], &mut out);
        assert_eq!(out, [0.0; 3]);
        assert_eq!(mixer.input_meter(0).expect("meter").peak_dbfs, None);
    }

    #[test]
    fn master_bus_is_hard_limited_and_counted() {
        let mut mixer = Mixer::new(1, &[UNITY, UNITY], 0.0);
        let mut out = [0.0; 3];
        mixer.mix(&[&[0.75, -0.75, 0.25], &[0.5, -0.5, 0.25]], &mut out);
        assert_eq!(out, [1.0, -1.0, 0.5]);
        assert_eq!(mixer.clipped_samples(), 2);
        assert_eq!(mixer.master_meter().peak_dbfs, Some(0.0));
    }

    #[test]
    fn non_finite_input_becomes_silence_and_is_counted() {
        let mut mixer = Mixer::new(1, &[UNITY], 0.0);
        let mut out = [0.0; 2];
        mixer.mix(&[&[f32::NAN, 0.5]], &mut out);
        assert_eq!(out, [0.0, 0.5]);
        assert_eq!(mixer.invalid_samples(), 1);
    }

    #[test]
    fn missing_inputs_count_as_silence() {
        let mut mixer = Mixer::new(1, &[UNITY, UNITY], 0.0);
        let mut out = [0.0; 2];
        mixer.mix(&[&[0.5, 0.5]], &mut out);
        assert_eq!(out, [0.5, 0.5]);
    }

    #[test]
    fn non_finite_samples_do_not_poison_meters() {
        let mut meter = Meter::default();
        for s in [0.5, f32::NAN, -0.5, f32::INFINITY] {
            meter.observe(s);
        }
        let half_db = 20.0 * 0.5f64.log10();
        assert!((meter.reading().rms_dbfs.expect("rms") - half_db).abs() < 1e-9);
    }

    #[test]
    fn meters_report_peak_and_rms() {
        let mut meter = Meter::default();
        for s in [0.5, -0.5, 0.5, -0.5] {
            meter.observe(s);
        }
        let reading = meter.reading();
        let half_db = 20.0 * 0.5f64.log10();
        assert!((reading.peak_dbfs.expect("peak") - half_db).abs() < 1e-9);
        assert!((reading.rms_dbfs.expect("rms") - half_db).abs() < 1e-9);
        assert_eq!(
            Meter::default().reading(),
            MeterReading {
                peak_dbfs: None,
                rms_dbfs: None
            }
        );
    }
}
