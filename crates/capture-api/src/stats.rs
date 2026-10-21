//! Capture diagnostics accumulators.
//!
//! Called from backend real-time callbacks: every method is O(1) per buffer
//! (O(samples) for audio levels), uses fixed-size state and never allocates.

use std::time::Duration;

use media_core::{MediaTimestamp, duration_millis};
use serde::Serialize;

const NANOS_PER_SECOND: u32 = 1_000_000_000;

/// What a received video buffer contained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferKind {
    /// Image data.
    Frame,
    /// No image data, e.g. a cursor-only metadata update.
    Empty,
    /// Flagged as corrupted by the source.
    Corrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameObservation {
    /// Monotonic arrival time in nanoseconds since the probe started.
    pub arrival_ns: u64,
    /// Presentation timestamp from the source, in nanoseconds.
    pub pts_ns: Option<i64>,
    /// Source sequence number; gaps count as dropped frames.
    pub seq: Option<u64>,
    pub kind: BufferKind,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IntervalStats {
    pub min_ms: f64,
    pub mean_ms: f64,
    pub max_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VideoStats {
    /// Length of the capture window.
    pub elapsed_ms: u64,
    /// Buffers carrying image data.
    pub frames: u64,
    /// Buffers without image data, e.g. cursor-only updates.
    pub empty_buffers: u64,
    pub corrupted_buffers: u64,
    /// Frames the source produced but this consumer never received
    /// (gaps in the source sequence number).
    pub dropped_frames: u64,
    /// Process callbacks that found no buffer to dequeue.
    pub buffer_starvation: u64,
    /// Frames received but not delivered to the consumer, e.g. because no
    /// free buffer was available or the pixel format cannot be converted.
    pub frames_discarded: u64,
    /// Frame rate between first and last frame arrival.
    pub measured_fps: Option<f64>,
    /// Spacing between consecutive frame arrivals.
    pub frame_interval: Option<IntervalStats>,
    /// First and last source presentation timestamps (time base 1/1e9).
    pub first_pts: Option<MediaTimestamp>,
    pub last_pts: Option<MediaTimestamp>,
    /// Frames whose PTS was lower than the previous frame's.
    pub non_monotonic_pts: u64,
    /// Frames delivered without a PTS.
    pub frames_without_pts: u64,
}

#[derive(Debug, Clone, Default)]
pub struct VideoStatsAccumulator {
    frames: u64,
    empty_buffers: u64,
    corrupted_buffers: u64,
    dropped_frames: u64,
    buffer_starvation: u64,
    frames_discarded: u64,
    first_arrival_ns: Option<u64>,
    last_arrival_ns: Option<u64>,
    interval_min_ns: u64,
    interval_max_ns: u64,
    interval_sum_ns: u128,
    interval_count: u64,
    first_pts_ns: Option<i64>,
    last_pts_ns: Option<i64>,
    non_monotonic_pts: u64,
    frames_without_pts: u64,
    last_seq: Option<u64>,
}

impl VideoStatsAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(&mut self, observation: FrameObservation) {
        if let Some(seq) = observation.seq {
            if let Some(last) = self.last_seq
                && seq > last
            {
                self.dropped_frames += seq - last - 1;
            }
            self.last_seq = Some(seq);
        }

        match observation.kind {
            BufferKind::Empty => self.empty_buffers += 1,
            BufferKind::Corrupted => self.corrupted_buffers += 1,
            BufferKind::Frame => self.observe_frame(observation),
        }
    }

    fn observe_frame(&mut self, observation: FrameObservation) {
        self.frames += 1;

        let arrival = observation.arrival_ns;
        if let Some(previous) = self.last_arrival_ns {
            let interval = arrival.saturating_sub(previous);
            if self.interval_count == 0 {
                self.interval_min_ns = interval;
                self.interval_max_ns = interval;
            } else {
                self.interval_min_ns = self.interval_min_ns.min(interval);
                self.interval_max_ns = self.interval_max_ns.max(interval);
            }
            self.interval_sum_ns += u128::from(interval);
            self.interval_count += 1;
        }
        self.first_arrival_ns.get_or_insert(arrival);
        self.last_arrival_ns = Some(arrival);

        match observation.pts_ns {
            Some(pts) => {
                if self.last_pts_ns.is_some_and(|last| pts < last) {
                    self.non_monotonic_pts += 1;
                }
                self.first_pts_ns.get_or_insert(pts);
                self.last_pts_ns = Some(pts);
            }
            None => self.frames_without_pts += 1,
        }
    }

    /// The backend asked for data but no buffer was queued.
    pub fn buffer_unavailable(&mut self) {
        self.buffer_starvation += 1;
    }

    /// A received frame could not be handed to the consumer.
    pub fn frame_discarded(&mut self) {
        self.frames_discarded += 1;
    }

    pub fn finish(&self, elapsed: Duration) -> VideoStats {
        let measured_fps = match (self.first_arrival_ns, self.last_arrival_ns) {
            (Some(first), Some(last)) if self.frames >= 2 && last > first => {
                Some((self.frames - 1) as f64 / nanos_to_seconds(last - first))
            }
            _ => None,
        };
        let frame_interval = (self.interval_count > 0).then(|| IntervalStats {
            min_ms: nanos_to_millis(self.interval_min_ns),
            mean_ms: self.interval_sum_ns as f64 / self.interval_count as f64 / 1e6,
            max_ms: nanos_to_millis(self.interval_max_ns),
        });
        VideoStats {
            elapsed_ms: duration_millis(elapsed),
            frames: self.frames,
            empty_buffers: self.empty_buffers,
            corrupted_buffers: self.corrupted_buffers,
            dropped_frames: self.dropped_frames,
            buffer_starvation: self.buffer_starvation,
            frames_discarded: self.frames_discarded,
            measured_fps,
            frame_interval,
            first_pts: self.first_pts_ns.and_then(nanos_timestamp),
            last_pts: self.last_pts_ns.and_then(nanos_timestamp),
            non_monotonic_pts: self.non_monotonic_pts,
            frames_without_pts: self.frames_without_pts,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AudioStats {
    /// Length of the capture window.
    pub elapsed_ms: u64,
    pub buffers: u64,
    /// Sample frames received (one sample per channel).
    pub frames: u64,
    /// Frames the negotiated rate implies for the time between first and last
    /// buffer arrival. Scheduling jitter makes this differ from `frames` by up
    /// to about one buffer.
    pub expected_frames: Option<u64>,
    /// `expected_frames - frames` when positive: audio the consumer did not receive.
    pub frame_deficit: Option<u64>,
    pub empty_buffers: u64,
    /// Buffers with data that could not be decoded, e.g. because no usable
    /// format was negotiated. Their audio is lost.
    pub dropped_buffers: u64,
    /// Process callbacks that found no buffer to dequeue.
    pub buffer_starvation: u64,
    /// Samples that were NaN or infinite; excluded from levels.
    pub invalid_samples: u64,
    /// Samples at or beyond full scale (|x| >= 1.0).
    pub clipped_samples: u64,
    /// Peak level in dBFS; `null` for digital silence.
    pub peak_dbfs: Option<f64>,
    /// RMS level in dBFS; `null` for digital silence.
    pub rms_dbfs: Option<f64>,
    /// First and last stream position reported by the backend, in its own time base.
    pub first_position: Option<MediaTimestamp>,
    pub last_position: Option<MediaTimestamp>,
}

/// Stream position in backend ticks with an explicit time base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPosition {
    pub ticks: i64,
    pub time_base_num: u32,
    pub time_base_den: u32,
}

#[derive(Debug, Clone, Default)]
pub struct AudioStatsAccumulator {
    rate: u32,
    buffers: u64,
    frames: u64,
    first_buffer_frames: u64,
    first_arrival_ns: Option<u64>,
    last_arrival_ns: Option<u64>,
    empty_buffers: u64,
    dropped_buffers: u64,
    buffer_starvation: u64,
    invalid_samples: u64,
    clipped_samples: u64,
    peak: f32,
    sum_squares: f64,
    level_samples: u64,
    first_position: Option<StreamPosition>,
    last_position: Option<StreamPosition>,
}

impl AudioStatsAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the negotiated sample rate; required for `expected_frames`.
    pub fn set_rate(&mut self, rate: u32) {
        self.rate = rate;
    }

    /// Record one buffer of `frames` sample frames; `samples` yields its
    /// interleaved samples so callers can decode in place without copying.
    pub fn observe_buffer<I>(
        &mut self,
        arrival_ns: u64,
        frames: u64,
        samples: I,
        position: Option<StreamPosition>,
    ) where
        I: IntoIterator<Item = f32>,
    {
        if frames == 0 {
            self.empty_buffers += 1;
            return;
        }
        if self.first_arrival_ns.is_none() {
            self.first_arrival_ns = Some(arrival_ns);
            self.first_buffer_frames = frames;
        }
        self.last_arrival_ns = Some(arrival_ns);
        self.buffers += 1;
        self.frames += frames;

        for sample in samples {
            if !sample.is_finite() {
                self.invalid_samples += 1;
                continue;
            }
            let magnitude = sample.abs();
            if magnitude >= 1.0 {
                self.clipped_samples += 1;
            }
            self.peak = self.peak.max(magnitude);
            self.sum_squares += f64::from(sample) * f64::from(sample);
            self.level_samples += 1;
        }

        if let Some(position) = position {
            self.first_position.get_or_insert(position);
            self.last_position = Some(position);
        }
    }

    /// The backend asked for data but no buffer was queued.
    pub fn buffer_unavailable(&mut self) {
        self.buffer_starvation += 1;
    }

    /// A buffer carried data that could not be decoded and was discarded.
    pub fn buffer_dropped(&mut self) {
        self.dropped_buffers += 1;
    }

    pub fn finish(&self, elapsed: Duration) -> AudioStats {
        let expected_frames = match (self.first_arrival_ns, self.last_arrival_ns) {
            (Some(first), Some(last)) if self.rate > 0 => {
                let span = nanos_to_seconds(last.saturating_sub(first));
                Some(self.first_buffer_frames + (span * f64::from(self.rate)).round() as u64)
            }
            _ => None,
        };
        let rms =
            (self.level_samples > 0).then(|| (self.sum_squares / self.level_samples as f64).sqrt());
        AudioStats {
            elapsed_ms: duration_millis(elapsed),
            buffers: self.buffers,
            frames: self.frames,
            expected_frames,
            frame_deficit: expected_frames.map(|expected| expected.saturating_sub(self.frames)),
            empty_buffers: self.empty_buffers,
            dropped_buffers: self.dropped_buffers,
            buffer_starvation: self.buffer_starvation,
            invalid_samples: self.invalid_samples,
            clipped_samples: self.clipped_samples,
            peak_dbfs: to_dbfs(f64::from(self.peak)),
            rms_dbfs: rms.and_then(to_dbfs),
            first_position: self.first_position.and_then(position_timestamp),
            last_position: self.last_position.and_then(position_timestamp),
        }
    }
}

fn to_dbfs(linear: f64) -> Option<f64> {
    (linear > 0.0).then(|| 20.0 * linear.log10())
}

fn nanos_timestamp(nanos: i64) -> Option<MediaTimestamp> {
    MediaTimestamp::new(nanos, 1, NANOS_PER_SECOND).ok()
}

fn position_timestamp(position: StreamPosition) -> Option<MediaTimestamp> {
    MediaTimestamp::new(
        position.ticks,
        position.time_base_num,
        position.time_base_den,
    )
    .ok()
}

fn nanos_to_seconds(nanos: u64) -> f64 {
    nanos as f64 / f64::from(NANOS_PER_SECOND)
}

fn nanos_to_millis(nanos: u64) -> f64 {
    nanos as f64 / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_NS: u64 = 16_666_667;

    fn frame(arrival_ns: u64, pts_ns: Option<i64>, seq: Option<u64>) -> FrameObservation {
        FrameObservation {
            arrival_ns,
            pts_ns,
            seq,
            kind: BufferKind::Frame,
        }
    }

    #[test]
    fn steady_sixty_fps_is_measured() {
        let mut acc = VideoStatsAccumulator::new();
        for i in 0..61u64 {
            let t = i * FRAME_NS;
            acc.observe(frame(t, Some(t as i64), Some(i)));
        }
        let stats = acc.finish(Duration::from_secs(1));
        assert_eq!(stats.frames, 61);
        assert_eq!(stats.dropped_frames, 0);
        let fps = stats.measured_fps.expect("fps");
        assert!((fps - 60.0).abs() < 0.01, "{fps}");
        let interval = stats.frame_interval.expect("interval");
        assert!((interval.mean_ms - 16.666_667).abs() < 0.001);
        assert_eq!(
            stats.last_pts,
            MediaTimestamp::new(60 * FRAME_NS as i64, 1, 1_000_000_000).ok()
        );
    }

    #[test]
    fn sequence_gaps_count_as_dropped_frames() {
        let mut acc = VideoStatsAccumulator::new();
        for (i, seq) in [10u64, 11, 14, 15, 20].into_iter().enumerate() {
            acc.observe(frame(i as u64 * FRAME_NS, None, Some(seq)));
        }
        let stats = acc.finish(Duration::from_secs(1));
        assert_eq!(stats.dropped_frames, 2 + 4);
        assert_eq!(stats.frames_without_pts, 5);
    }

    #[test]
    fn sequence_reset_is_not_a_drop() {
        let mut acc = VideoStatsAccumulator::new();
        acc.observe(frame(0, None, Some(100)));
        acc.observe(frame(FRAME_NS, None, Some(0)));
        acc.observe(frame(2 * FRAME_NS, None, Some(1)));
        assert_eq!(acc.finish(Duration::ZERO).dropped_frames, 0);
    }

    #[test]
    fn empty_corrupted_and_starved_buffers_are_counted_separately() {
        let mut acc = VideoStatsAccumulator::new();
        acc.observe(FrameObservation {
            kind: BufferKind::Empty,
            ..frame(0, None, None)
        });
        acc.observe(FrameObservation {
            kind: BufferKind::Corrupted,
            ..frame(1, None, None)
        });
        acc.buffer_unavailable();
        acc.frame_discarded();
        let stats = acc.finish(Duration::from_millis(1500));
        assert_eq!(stats.frames_discarded, 1);
        assert_eq!(stats.frames, 0);
        assert_eq!(stats.empty_buffers, 1);
        assert_eq!(stats.corrupted_buffers, 1);
        assert_eq!(stats.buffer_starvation, 1);
        assert_eq!(stats.measured_fps, None);
        assert_eq!(stats.frame_interval, None);
        assert_eq!(stats.elapsed_ms, 1500);
    }

    #[test]
    fn non_monotonic_pts_is_counted() {
        let mut acc = VideoStatsAccumulator::new();
        acc.observe(frame(0, Some(1_000), None));
        acc.observe(frame(FRAME_NS, Some(500), None));
        acc.observe(frame(2 * FRAME_NS, Some(2_000), None));
        assert_eq!(acc.finish(Duration::ZERO).non_monotonic_pts, 1);
    }

    #[test]
    fn half_scale_signal_peaks_at_minus_six_dbfs() {
        let mut acc = AudioStatsAccumulator::new();
        acc.set_rate(48_000);
        acc.observe_buffer(0, 4, [0.5, -0.5, 0.5, -0.5, 0.5, -0.5, 0.5, -0.5], None);
        let stats = acc.finish(Duration::from_secs(1));
        let peak = stats.peak_dbfs.expect("peak");
        let rms = stats.rms_dbfs.expect("rms");
        assert!((peak + 6.0206).abs() < 1e-3, "{peak}");
        assert!((rms + 6.0206).abs() < 1e-3, "{rms}");
        assert_eq!(stats.frames, 4);
        assert_eq!(stats.clipped_samples, 0);
    }

    #[test]
    fn silence_has_no_level_and_clipping_and_nan_are_counted() {
        let mut acc = AudioStatsAccumulator::new();
        acc.observe_buffer(0, 2, [0.0, 0.0, 0.0, 0.0], None);
        let silent = acc.finish(Duration::ZERO);
        assert_eq!(silent.peak_dbfs, None);
        assert_eq!(silent.rms_dbfs, None);

        acc.observe_buffer(1, 2, [1.0, -1.5, f32::NAN, 0.0], None);
        let stats = acc.finish(Duration::ZERO);
        assert_eq!(stats.clipped_samples, 2);
        assert_eq!(stats.invalid_samples, 1);
    }

    #[test]
    fn frame_deficit_reflects_missing_audio() {
        let mut acc = AudioStatsAccumulator::new();
        acc.set_rate(48_000);
        // 10 buffers of 1024 frames every 1024/48000 s, but buffer 5 never arrives.
        let period_ns = 1024 * 1_000_000_000 / 48_000;
        for i in (0..10u64).filter(|&i| i != 5) {
            acc.observe_buffer(i * period_ns, 1024, std::iter::empty(), None);
        }
        let stats = acc.finish(Duration::from_secs(1));
        assert_eq!(stats.frames, 9 * 1024);
        assert_eq!(stats.expected_frames, Some(10 * 1024));
        assert_eq!(stats.frame_deficit, Some(1024));
    }

    #[test]
    fn empty_audio_buffers_do_not_start_the_clock() {
        let mut acc = AudioStatsAccumulator::new();
        acc.set_rate(48_000);
        acc.observe_buffer(0, 0, std::iter::empty(), None);
        acc.buffer_unavailable();
        acc.buffer_dropped();
        let stats = acc.finish(Duration::ZERO);
        assert_eq!(stats.empty_buffers, 1);
        assert_eq!(stats.buffer_starvation, 1);
        assert_eq!(stats.dropped_buffers, 1);
        assert_eq!(stats.buffers, 0);
        assert_eq!(stats.expected_frames, None);
    }

    #[test]
    fn positions_keep_their_time_base() {
        let mut acc = AudioStatsAccumulator::new();
        let position = |ticks| {
            Some(StreamPosition {
                ticks,
                time_base_num: 1,
                time_base_den: 48_000,
            })
        };
        acc.observe_buffer(0, 1, [0.1], position(48_000));
        acc.observe_buffer(1, 1, [0.1], position(96_000));
        let stats = acc.finish(Duration::ZERO);
        assert_eq!(
            stats.first_position,
            MediaTimestamp::new(48_000, 1, 48_000).ok()
        );
        assert_eq!(
            stats.last_position.map(|p| p.to_nanoseconds()),
            Some(Ok(2_000_000_000))
        );
    }
}
