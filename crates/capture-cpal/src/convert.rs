//! Converts between a device's format (any channel count and rate) and the
//! engine's interleaved audio at a fixed rate, inside the audio callback.
//!
//! On Linux PipeWire converts; WASAPI shared mode capture and CoreAudio
//! use the device's own format, so this backend converts itself:
//! - [`Converter`] (capture): mono is duplicated to both channels; more
//!   than two channels keep the first two (front left and right; no
//!   surround downmix);
//! - [`PlaybackConverter`] (playback): the mirror; stereo goes to the first
//!   two channels, a mono device gets their average;
//! - other rates are resampled linearly (cheap; it slightly softens high
//!   frequencies, like the mixer's drift correction, ADR 0004).
//!
//! Neither allocates while converting: capture output goes into a
//! caller-provided scratch buffer that is flushed whenever it fills, and
//! playback reads the feed through a buffer allocated up front.

use media_core::SampleReader;

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

/// Fills a playback device's buffer from the engine's feed (one or two
/// channels at the feed's rate), in the device's channel count and rate.
///
/// Resampling uses an exact integer phase: each second of device audio
/// reads exactly one second of the feed, so no drift builds up here (the
/// feed itself follows the device clock, ADR 0017). It delays by a frame
/// or two of the feed.
#[derive(Debug)]
pub struct PlaybackConverter {
    feed_channels: usize,
    device_channels: usize,
    feed_rate: u64,
    device_rate: u64,
    /// Position of the next device frame past `previous`, in units of
    /// `1 / device_rate` feed frames; below `device_rate`.
    phase: u64,
    previous: [f32; 2],
    next: [f32; 2],
    /// The feed is read through this. A pull it covers is one read, so the
    /// feed sees the pull's true size and buffers enough for it.
    scratch: Vec<f32>,
}

impl PlaybackConverter {
    /// For a feed of `feed_channels` (1 or 2) at `feed_rate`, read through
    /// a buffer of `scratch_frames` feed frames (raised to what one device
    /// frame needs). Allocates; call it before the stream starts.
    pub fn new(
        feed_channels: usize,
        feed_rate: u32,
        device_channels: u16,
        device_rate: u32,
        scratch_frames: usize,
    ) -> Self {
        let feed_channels = feed_channels.clamp(1, 2);
        let feed_rate = u64::from(feed_rate.max(1));
        let device_rate = u64::from(device_rate.max(1));
        let least = usize::try_from(feed_rate / device_rate + 1).unwrap_or(usize::MAX);
        Self {
            feed_channels,
            device_channels: usize::from(device_channels.max(1)),
            feed_rate,
            device_rate,
            phase: 0,
            previous: [0.0; 2],
            next: [0.0; 2],
            scratch: vec![0.0; scratch_frames.max(least) * feed_channels],
        }
    }

    /// Whether the device plays the feed as it is: the feed can be read
    /// straight into the device's buffer.
    pub fn is_passthrough(&self) -> bool {
        self.feed_rate == self.device_rate && self.feed_channels == self.device_channels
    }

    /// Fill `out` (interleaved, the device's channel count; a trailing
    /// partial frame gets silence) from `feed`, which pads silence when it
    /// runs short and counts it. Never waits or allocates.
    pub fn fill(&mut self, out: &mut [f32], feed: &mut SampleReader) {
        if self.is_passthrough() {
            feed.read(out);
            return;
        }
        let channels = self.device_channels;
        let frames = out.len() / channels;
        out[frames * channels..].fill(0.0);
        let capacity = (self.scratch.len() / self.feed_channels) as u64;
        // Device frames whose feed frames fit in the scratch: phase stays
        // below `device_rate`, so `phase + count * feed_rate` stays below
        // `(capacity + 1) * device_rate`.
        let step = usize::try_from(capacity * self.device_rate / self.feed_rate)
            .unwrap_or(usize::MAX)
            .max(1);
        let mut done = 0;
        while done < frames {
            let count = step.min(frames - done);
            let needed = self.feed_frames_for(count);
            let input = &mut self.scratch[..needed * self.feed_channels];
            feed.read(input);
            let mut taken = 0;
            for frame in out[done * channels..(done + count) * channels].chunks_exact_mut(channels)
            {
                let value = if self.feed_rate == self.device_rate {
                    taken += 1;
                    feed_frame(input, taken - 1, self.feed_channels)
                } else {
                    let t = (self.phase as f64 / self.device_rate as f64) as f32;
                    let value = [
                        self.previous[0] + (self.next[0] - self.previous[0]) * t,
                        self.previous[1] + (self.next[1] - self.previous[1]) * t,
                    ];
                    self.phase += self.feed_rate;
                    while self.phase >= self.device_rate {
                        self.phase -= self.device_rate;
                        self.previous = self.next;
                        self.next = feed_frame(input, taken, self.feed_channels);
                        taken += 1;
                    }
                    value
                };
                put_frame(frame, value);
            }
            debug_assert_eq!(taken, needed);
            done += count;
        }
    }

    /// Feed frames that the next `count` device frames consume.
    fn feed_frames_for(&self, count: usize) -> usize {
        if self.feed_rate == self.device_rate {
            return count;
        }
        let consumed = (self.phase + count as u64 * self.feed_rate) / self.device_rate;
        usize::try_from(consumed).unwrap_or(usize::MAX)
    }
}

/// Frame `index` of an interleaved feed buffer, as stereo.
fn feed_frame(input: &[f32], index: usize, channels: usize) -> [f32; 2] {
    match &input[index * channels..(index + 1) * channels] {
        [mono] => [*mono, *mono],
        [left, right, ..] => [*left, *right],
        [] => [0.0; 2],
    }
}

/// A stereo frame in a device's layout: the first two channels, silence on
/// the rest; the average on a mono device.
fn put_frame(frame: &mut [f32], [left, right]: [f32; 2]) {
    match frame {
        [mono] => *mono = (left + right) * 0.5,
        [first, second, rest @ ..] => {
            *first = left;
            *second = right;
            rest.fill(0.0);
        }
        [] => {}
    }
}

#[cfg(test)]
mod tests {
    use media_core::sample_ring;

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

    #[test]
    fn a_matching_device_reads_the_feed_straight() {
        let (mut writer, mut reader) = sample_ring(2, 64);
        writer.write(&[0.5, -0.5, 0.25, -0.25]);
        let mut converter = PlaybackConverter::new(2, 48_000, 2, 48_000, 16);
        assert!(converter.is_passthrough());
        let mut out = [9.0; 6];
        converter.fill(&mut out, &mut reader);
        assert_eq!(out, [0.5, -0.5, 0.25, -0.25, 0.0, 0.0]);
        // One read of the whole pull, so the feed sees its size.
        assert_eq!(reader.stats().max_read_frames, 3);
    }

    #[test]
    fn the_feed_is_mapped_onto_the_device_channels() {
        let samples = [0.5, -0.5, 0.25, 0.75];
        let (mut writer, mut reader) = sample_ring(2, 64);
        // A mono device gets the average.
        writer.write(&samples);
        let mut mono = PlaybackConverter::new(2, 48_000, 1, 48_000, 16);
        let mut out = [9.0; 2];
        mono.fill(&mut out, &mut reader);
        assert_eq!(out, [0.0, 0.5]);
        // Four channels: the first two, silence on the rest and on a
        // trailing partial frame.
        writer.write(&samples);
        let mut quad = PlaybackConverter::new(2, 48_000, 4, 48_000, 16);
        let mut out = [9.0; 9];
        quad.fill(&mut out, &mut reader);
        assert_eq!(out, [0.5, -0.5, 0.0, 0.0, 0.25, 0.75, 0.0, 0.0, 0.0]);
        // A mono feed plays on both channels.
        let (mut writer, mut reader) = sample_ring(1, 64);
        writer.write(&[0.5, 0.25]);
        let mut stereo = PlaybackConverter::new(1, 48_000, 2, 48_000, 16);
        let mut out = [9.0; 4];
        stereo.fill(&mut out, &mut reader);
        assert_eq!(out, [0.5, 0.5, 0.25, 0.25]);
    }

    #[test]
    fn resampling_reads_exactly_the_feed_rate() {
        // One second at 44.1 kHz, in pulls of 10 ms, reads one second at
        // 48 kHz: not a frame more or less.
        let (mut writer, mut reader) = sample_ring(2, 64_000);
        writer.write(&vec![0.25; 2 * 50_000]);
        let mut converter = PlaybackConverter::new(2, 48_000, 2, 44_100, 16_384);
        assert!(!converter.is_passthrough());
        let mut out = vec![0.0; 441 * 2];
        for _ in 0..100 {
            converter.fill(&mut out, &mut reader);
        }
        assert_eq!(reader.stats().read_frames, 48_000);
        assert_eq!(reader.fill_frames(), 2_000);
        // A constant stays exact.
        assert!(out.iter().all(|&sample| sample == 0.25));
    }

    #[test]
    fn resampling_interpolates_between_feed_frames() {
        let ramp: Vec<f32> = (0..10).map(|i| i as f32).collect();
        // Doubling the rate: halfway values, after two frames of delay.
        let (mut writer, mut reader) = sample_ring(1, 64);
        writer.write(&ramp[..8]);
        let mut double = PlaybackConverter::new(1, 48_000, 1, 96_000, 64);
        let mut out = [0.0; 16];
        double.fill(&mut out, &mut reader);
        let expected: Vec<f32> = (0..16).map(|k| (k as f32 - 4.0).max(0.0) / 2.0).collect();
        assert_eq!(out.as_slice(), expected);
        assert_eq!(reader.fill_frames(), 0);
        // Halving it: every second frame.
        writer.write(&ramp);
        let mut half = PlaybackConverter::new(1, 48_000, 1, 24_000, 64);
        let mut out = [0.0; 5];
        half.fill(&mut out, &mut reader);
        assert_eq!(out, [0.0, 0.0, 2.0, 4.0, 6.0]);
        assert_eq!(reader.fill_frames(), 0);
    }

    #[test]
    fn playback_state_carries_across_pulls_and_scratch_steps() {
        // One pull through a large scratch equals several pulls through a
        // scratch of a few frames.
        let samples: Vec<f32> = (0..1_000).map(|i| (i as f32 * 0.37).sin()).collect();
        let (mut writer, mut reader) = sample_ring(2, 1_024);
        writer.write(&samples);
        let mut whole = PlaybackConverter::new(2, 48_000, 2, 44_100, 4_096);
        let mut expected = vec![0.0; 2 * 300];
        whole.fill(&mut expected, &mut reader);
        let (mut writer, mut reader) = sample_ring(2, 1_024);
        writer.write(&samples);
        let mut split = PlaybackConverter::new(2, 48_000, 2, 44_100, 7);
        let mut out = vec![0.0; 2 * 300];
        let (first, second) = out.split_at_mut(2 * 137);
        split.fill(first, &mut reader);
        split.fill(second, &mut reader);
        assert_eq!(out, expected);
        assert!(reader.stats().max_read_frames <= 7);
    }
}
