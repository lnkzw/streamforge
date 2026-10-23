//! Mixing primitives: decibel conversion, click-free gain ramps and
//! meters. The graph built from them is [`crate::graph`].

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
    pub(crate) fn at(&self, index: usize, frames: usize) -> f32 {
        if self.current == self.target {
            self.current
        } else {
            let t = (index + 1) as f32 / frames as f32;
            self.current + (self.target - self.current) * t
        }
    }

    pub(crate) fn finish_block(&mut self) {
        self.current = self.target;
    }

    /// Silent now and staying so: nothing to add for this block.
    pub(crate) fn is_silent(&self) -> bool {
        self.current == 0.0 && self.target == 0.0
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
    /// RMS; the graph silences non-finite input samples and counts them.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decibel_conversion() {
        assert_eq!(db_to_linear(0.0), 1.0);
        assert!((db_to_linear(-6.0206) - 0.5).abs() < 1e-4);
        assert!((db_to_linear(20.0) - 10.0).abs() < 1e-4);
    }

    #[test]
    fn a_ramp_reaches_its_target_on_the_last_frame() {
        let mut ramp = GainRamp::new(1.0);
        ramp.set_target(0.0);
        let gains: Vec<f32> = (0..4).map(|frame| ramp.at(frame, 4)).collect();
        assert_eq!(gains, [0.75, 0.5, 0.25, 0.0]);
        assert!(!ramp.is_silent());
        ramp.finish_block();
        assert!(ramp.is_silent());
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
