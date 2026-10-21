//! Mixer thread with a fixed block clock.
//!
//! Every live input is an [`AudioRing`] filled by a producer running on its
//! own clock; [`crate::live`] keeps each input's buffering bounded and
//! compensates clock drift by adaptive resampling. Tones are generated in
//! place. When the thread falls behind, it mixes missed blocks back to back,
//! up to [`MAX_CATCH_UP_BLOCKS`]; anything beyond that is skipped and counted.
//! The loop does not allocate after start-up.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use media_core::{AudioRing, AudioRingStats, LatencyHistogram, LatencySummary, duration_millis};
use serde::Serialize;
use thiserror::Error;

use crate::live::LiveInput;
use crate::mixer::{MeterReading, Mixer, StripConfig};
use crate::tone::Tone;

pub const RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
/// 10 ms at 48 kHz.
pub const BLOCK_FRAMES: usize = 480;
pub const MAX_CATCH_UP_BLOCKS: u64 = 20;
/// Mean fill is recorded per window of this many blocks (10 s).
const WINDOW_BLOCKS: u64 = 1_000;

pub enum InputSource {
    Live(Arc<AudioRing>),
    Tone(Tone),
}

pub struct MixerInput {
    pub name: String,
    pub source: InputSource,
    pub strip: StripConfig,
}

/// Receives every mixed block on the mixer thread, with the block's nominal
/// start time on the mixer clock. Must not block or allocate.
pub trait BlockSink: Send {
    fn block(&mut self, samples: &[f32], at: Instant);
}

pub struct MixerConfig {
    pub inputs: Vec<MixerInput>,
    pub master_gain_db: f32,
    /// Sizes the latency trend buffer up front so the loop never allocates.
    pub expected_duration: Duration,
    /// Optional consumer of the master output, e.g. a recorder.
    pub sink: Option<Box<dyn BlockSink>>,
}

#[derive(Debug, Error)]
pub enum MixerError {
    #[error("live input `{name}` delivers {channels} channel(s), the mixer needs {CHANNELS}")]
    ChannelMismatch { name: String, channels: usize },
    #[error("the mixer accepts at most {MAX_INPUTS} inputs, got {0}")]
    TooManyInputs(usize),
    #[error("cannot start the mixer thread: {0}")]
    Spawn(std::io::Error),
    #[error("the mixer thread panicked")]
    Panicked,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BufferLatency {
    /// Final target fill.
    pub target_ms: f64,
    pub min_ms: f64,
    pub mean_ms: f64,
    pub max_ms: f64,
    /// Mean fill per 10 s window, oldest first; growth would show here.
    pub windows_ms: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InputStats {
    pub name: String,
    /// `live` or `tone`.
    pub kind: &'static str,
    pub gain_db: f32,
    pub muted: bool,
    /// Post-gain level of this input.
    pub meter: MeterReading,
    pub warmup_frames: u64,
    pub underrun_frames: u64,
    pub trimmed_frames: u64,
    /// Producer clock offset against the mixer clock, as tracked by the
    /// adaptive resampler (live inputs only).
    pub drift_ppm: Option<f64>,
    pub ring: Option<AudioRingStats>,
    pub buffer: Option<BufferLatency>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MixStats {
    pub rate: u32,
    pub channels: u32,
    pub block_frames: usize,
    pub elapsed_ms: u64,
    pub blocks_mixed: u64,
    /// Blocks mixed after the next block was already due.
    pub late_blocks: u64,
    /// Blocks not mixed because the thread fell too far behind.
    pub skipped_blocks: u64,
    pub mix_time: Option<LatencySummary>,
    pub master: MeterReading,
    pub clipped_samples: u64,
    pub invalid_samples: u64,
    pub inputs: Vec<InputStats>,
}

/// Peak levels cover the most recent blocks (300 ms).
pub const LEVEL_WINDOW_BLOCKS: usize = 30;
/// The mixer thread refreshes [`MixerControl::live`] every this many blocks
/// (250 ms).
pub const LIVE_PUBLISH_BLOCKS: u64 = 25;

/// A running mixer's counters so far (see [`MixerControl::live`]); the same
/// meaning as in [`MixStats`], at most [`LIVE_PUBLISH_BLOCKS`] blocks old.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LiveMixStats {
    pub elapsed_ms: u64,
    pub blocks_mixed: u64,
    pub late_blocks: u64,
    pub skipped_blocks: u64,
    pub clipped_samples: u64,
    pub invalid_samples: u64,
    pub mix_time: Option<LatencySummary>,
    pub inputs: Vec<LiveInputStats>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LiveInputStats {
    pub name: String,
    /// `live` or `tone`.
    pub kind: &'static str,
    pub warmup_frames: u64,
    pub underrun_frames: u64,
    pub trimmed_frames: u64,
    /// Current buffer fill of a live input; growing underruns with a fill
    /// near zero mean the device stopped delivering.
    pub fill_ms: Option<f64>,
    pub drift_ppm: Option<f64>,
    /// Frames the input's ring overwrote because the mixer fell behind.
    pub overrun_frames: Option<u64>,
}

/// Live control of a running mixer: gain and mute changes, and peak levels
/// for meters. Cloneable and usable from any thread.
///
/// Changes wait in a preallocated slot that the mixer thread only
/// try-locks once per block, so a caller never stalls the mixer and the
/// mixer never allocates. Levels are atomics with the highest peak of the
/// last [`LEVEL_WINDOW_BLOCKS`] blocks, written by the mixer thread; reading
/// them changes nothing, so any number of readers see the same levels.
#[derive(Clone)]
pub struct MixerControl {
    shared: Arc<ControlShared>,
}

struct ControlShared {
    pending: Mutex<PendingChanges>,
    input_peaks: Vec<AtomicU32>,
    master_peak: AtomicU32,
    /// Refreshed in place by the mixer thread after a try-lock.
    live: Mutex<LiveMixStats>,
}

struct PendingChanges {
    strips: Vec<Option<StripConfig>>,
    master_gain_db: Option<f32>,
}

/// Peaks of the recent window, in dBFS (`None`: silence).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LevelReading {
    pub inputs: Vec<Option<f64>>,
    pub master: Option<f64>,
}

impl MixerControl {
    /// `inputs` gives each input's name and kind, so the live snapshot is
    /// allocated here and the mixer thread only overwrites numbers.
    fn new(inputs: &[(String, &'static str)]) -> Self {
        let live = LiveMixStats {
            inputs: inputs
                .iter()
                .map(|(name, kind)| LiveInputStats {
                    name: name.clone(),
                    kind,
                    warmup_frames: 0,
                    underrun_frames: 0,
                    trimmed_frames: 0,
                    fill_ms: None,
                    drift_ppm: None,
                    overrun_frames: None,
                })
                .collect(),
            ..LiveMixStats::default()
        };
        Self {
            shared: Arc::new(ControlShared {
                pending: Mutex::new(PendingChanges {
                    strips: vec![None; inputs.len()],
                    master_gain_db: None,
                }),
                input_peaks: (0..inputs.len()).map(|_| AtomicU32::new(0)).collect(),
                master_peak: AtomicU32::new(0),
                live: Mutex::new(live),
            }),
        }
    }

    /// Counters so far, at most [`LIVE_PUBLISH_BLOCKS`] blocks old. Never
    /// delays the mixer (it skips a refresh while this copies).
    pub fn live(&self) -> LiveMixStats {
        lock(&self.shared.live).clone()
    }

    /// Change input `index` from the next block on (ramped). Returns `false`
    /// for an unknown index.
    pub fn set_input(&self, index: usize, config: StripConfig) -> bool {
        let mut pending = lock(&self.shared.pending);
        match pending.strips.get_mut(index) {
            Some(slot) => {
                *slot = Some(config);
                true
            }
            None => false,
        }
    }

    pub fn set_master_gain(&self, gain_db: f32) {
        lock(&self.shared.pending).master_gain_db = Some(gain_db);
    }

    /// Peaks of the last [`LEVEL_WINDOW_BLOCKS`] blocks.
    pub fn levels(&self) -> LevelReading {
        let to_db = |peak: &AtomicU32| {
            let linear = f64::from(f32::from_bits(peak.load(Ordering::Relaxed)));
            (linear > 0.0).then(|| 20.0 * linear.log10())
        };
        LevelReading {
            inputs: self.shared.input_peaks.iter().map(to_db).collect(),
            master: to_db(&self.shared.master_peak),
        }
    }

    /// Mixer thread: apply pending changes if the slot is free right now.
    fn apply(&self, mixer: &mut Mixer) {
        let mut pending = match self.shared.pending.try_lock() {
            Ok(pending) => pending,
            Err(TryLockError::Poisoned(pending)) => pending.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        for (index, slot) in pending.strips.iter_mut().enumerate() {
            if let Some(config) = slot.take() {
                mixer.set_input(index, config);
            }
        }
        if let Some(gain_db) = pending.master_gain_db.take() {
            mixer.set_master_gain(gain_db);
        }
    }

    /// Mixer thread: record the block's peaks and publish each window's
    /// maximum.
    fn publish(&self, mixer: &Mixer, window: &mut PeakWindow) {
        let slot = window.next;
        window.next = (window.next + 1) % LEVEL_WINDOW_BLOCKS;
        let targets = self
            .shared
            .input_peaks
            .iter()
            .chain(std::iter::once(&self.shared.master_peak));
        for (index, (history, published)) in window.history.iter_mut().zip(targets).enumerate() {
            history[slot] = if index < self.shared.input_peaks.len() {
                mixer.input_block_peak(index).unwrap_or(0.0)
            } else {
                mixer.master_block_peak()
            };
            let peak = history.iter().copied().fold(0.0f32, f32::max);
            published.store(peak.to_bits(), Ordering::Relaxed);
        }
    }
}

/// Recent block peaks per input plus master; owned by the mixer thread and
/// allocated once at start.
struct PeakWindow {
    history: Vec<[f32; LEVEL_WINDOW_BLOCKS]>,
    next: usize,
}

impl PeakWindow {
    fn new(inputs: usize) -> Self {
        Self {
            history: vec![[0.0; LEVEL_WINDOW_BLOCKS]; inputs + 1],
            next: 0,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A running mixer; [`MixerThread::stop`] returns its statistics. Dropping
/// it without `stop` still stops and joins the thread.
pub struct MixerThread {
    stop: Arc<AtomicBool>,
    control: MixerControl,
    thread: Option<JoinHandle<MixStats>>,
}

impl MixerThread {
    pub fn spawn(config: MixerConfig) -> Result<Self, MixerError> {
        if config.inputs.len() > MAX_INPUTS {
            return Err(MixerError::TooManyInputs(config.inputs.len()));
        }
        for input in &config.inputs {
            if let InputSource::Live(ring) = &input.source
                && ring.channels() != CHANNELS as usize
            {
                return Err(MixerError::ChannelMismatch {
                    name: input.name.clone(),
                    channels: ring.channels(),
                });
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let names: Vec<(String, &'static str)> = config
            .inputs
            .iter()
            .map(|input| {
                let kind = match input.source {
                    InputSource::Live(_) => "live",
                    InputSource::Tone(_) => "tone",
                };
                (input.name.clone(), kind)
            })
            .collect();
        let control = MixerControl::new(&names);
        let thread = thread::Builder::new()
            .name("audio-mixer".to_owned())
            .spawn({
                let stop = Arc::clone(&stop);
                let control = control.clone();
                move || run(config, &stop, &control)
            })
            .map_err(MixerError::Spawn)?;
        Ok(Self {
            stop,
            control,
            thread: Some(thread),
        })
    }

    /// Live gain/mute control and levels of this mixer.
    pub fn control(&self) -> MixerControl {
        self.control.clone()
    }

    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub fn stop(mut self) -> Result<MixStats, MixerError> {
        self.stop.store(true, Ordering::Release);
        let thread = self.thread.take().ok_or(MixerError::Panicked)?;
        thread.join().map_err(|_| MixerError::Panicked)
    }
}

impl Drop for MixerThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Buffer fill statistics of one live input.
struct LatencyTrend {
    min_fill: usize,
    max_fill: usize,
    fill_sum: u128,
    fill_samples: u64,
    window_sum: u128,
    window_blocks: u64,
    /// Preallocated; holds at most its initial capacity.
    windows: Vec<f64>,
}

impl LatencyTrend {
    fn new(windows: usize) -> Self {
        Self {
            min_fill: usize::MAX,
            max_fill: 0,
            fill_sum: 0,
            fill_samples: 0,
            window_sum: 0,
            window_blocks: 0,
            windows: Vec::with_capacity(windows),
        }
    }

    fn record(&mut self, fill: usize) {
        self.min_fill = self.min_fill.min(fill);
        self.max_fill = self.max_fill.max(fill);
        self.fill_sum += fill as u128;
        self.fill_samples += 1;
        self.window_sum += fill as u128;
        self.window_blocks += 1;
        if self.window_blocks == WINDOW_BLOCKS {
            if self.windows.len() < self.windows.capacity() {
                self.windows.push(frames_to_ms(
                    self.window_sum as f64 / self.window_blocks as f64,
                ));
            }
            self.window_sum = 0;
            self.window_blocks = 0;
        }
    }

    fn latency(&self, target: usize) -> Option<BufferLatency> {
        (self.fill_samples > 0).then(|| BufferLatency {
            target_ms: frames_to_ms(target as f64),
            min_ms: frames_to_ms(self.min_fill as f64),
            mean_ms: frames_to_ms(self.fill_sum as f64 / self.fill_samples as f64),
            max_ms: frames_to_ms(self.max_fill as f64),
            windows_ms: self.windows.clone(),
        })
    }
}

fn frames_to_ms(frames: f64) -> f64 {
    frames * 1000.0 / f64::from(RATE)
}

enum Source {
    Live(Box<LiveInput>, LatencyTrend),
    Tone(Tone),
}

/// Refresh the shared snapshot in place; `false` if a reader holds it now
/// (the caller retries on the next block). Does not allocate: the input
/// list was created with the control.
fn publish_live(
    live: &Mutex<LiveMixStats>,
    start: Instant,
    (mixed, late, skipped): (u64, u64, u64),
    mix_times: &LatencyHistogram,
    mixer: &Mixer,
    inputs: &[InputState],
) -> bool {
    let mut snapshot = match live.try_lock() {
        Ok(snapshot) => snapshot,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => return false,
    };
    snapshot.elapsed_ms = duration_millis(start.elapsed());
    snapshot.blocks_mixed = mixed;
    snapshot.late_blocks = late;
    snapshot.skipped_blocks = skipped;
    snapshot.clipped_samples = mixer.clipped_samples();
    snapshot.invalid_samples = mixer.invalid_samples();
    snapshot.mix_time = mix_times.summary();
    for (entry, input) in snapshot.inputs.iter_mut().zip(inputs) {
        entry.warmup_frames = input.warmup;
        entry.underrun_frames = input.underrun;
        entry.trimmed_frames = input.trimmed;
        if let Source::Live(live, _) = &input.source {
            entry.fill_ms = Some(frames_to_ms(live.fill() as f64));
            entry.drift_ppm = Some(live.drift_ppm());
            entry.overrun_frames = Some(live.ring().stats().overrun_frames);
        }
    }
    true
}

struct InputState {
    name: String,
    source: Source,
    scratch: Vec<f32>,
    warmup: u64,
    underrun: u64,
    trimmed: u64,
}

fn run(config: MixerConfig, stop: &AtomicBool, control: &MixerControl) -> MixStats {
    let mut sink = config.sink;
    let channels = CHANNELS as usize;
    let windows = (config.expected_duration.as_secs() / 10 + 2) as usize;
    let strips: Vec<StripConfig> = config.inputs.iter().map(|input| input.strip).collect();
    let mut mixer = Mixer::new(channels, &strips, config.master_gain_db);
    let mut inputs: Vec<InputState> = config
        .inputs
        .into_iter()
        .map(|input| InputState {
            name: input.name,
            source: match input.source {
                InputSource::Live(ring) => {
                    Source::Live(Box::new(LiveInput::new(ring)), LatencyTrend::new(windows))
                }
                InputSource::Tone(tone) => Source::Tone(tone),
            },
            scratch: vec![0.0; BLOCK_FRAMES * channels],
            warmup: 0,
            underrun: 0,
            trimmed: 0,
        })
        .collect();
    let mut out = vec![0.0f32; BLOCK_FRAMES * channels];
    let mut mix_times = LatencyHistogram::new();
    let mut peaks = PeakWindow::new(inputs.len());

    let period_ns = BLOCK_FRAMES as u64 * 1_000_000_000 / u64::from(RATE);
    let due =
        |start: Instant, block: u64| start + Duration::from_nanos(period_ns.saturating_mul(block));
    let (mut block, mut mixed, mut late, mut skipped) = (0u64, 0u64, 0u64, 0u64);
    let mut publish_due = false;
    let start = Instant::now();

    while !stop.load(Ordering::Acquire) {
        let deadline = due(start, block);
        let now = Instant::now();
        if now < deadline {
            thread::sleep(deadline - now);
            continue;
        }
        let behind =
            u64::try_from((now - deadline).as_nanos() / u128::from(period_ns)).unwrap_or(u64::MAX);
        if behind > MAX_CATCH_UP_BLOCKS {
            let skip = behind - MAX_CATCH_UP_BLOCKS;
            skipped += skip;
            block += skip;
        }

        let mix_start = Instant::now();
        control.apply(&mut mixer);
        for input in &mut inputs {
            match &mut input.source {
                Source::Live(live, trend) => {
                    let (fill, outcome) = live.read_block(&mut input.scratch);
                    if let Some(fill) = fill {
                        trend.record(fill);
                    }
                    input.warmup += outcome.warmup;
                    input.underrun += outcome.underrun;
                    input.trimmed += outcome.trimmed;
                }
                Source::Tone(tone) => tone.fill(&mut input.scratch, channels),
            }
        }
        {
            // Fixed-size view array: `spawn` rejects more than MAX_INPUTS.
            let mut views: [&[f32]; MAX_INPUTS] = [&[]; MAX_INPUTS];
            for (view, input) in views.iter_mut().zip(&inputs) {
                *view = &input.scratch;
            }
            mixer.mix(&views[..inputs.len().min(MAX_INPUTS)], &mut out);
        }
        control.publish(&mixer, &mut peaks);
        if let Some(sink) = &mut sink {
            sink.block(&out, due(start, block));
        }
        let finished = Instant::now();
        mix_times.record(finished - mix_start);
        mixed += 1;
        if finished > due(start, block + 1) {
            late += 1;
        }
        publish_due |= (mixed - 1) % LIVE_PUBLISH_BLOCKS == 0;
        if publish_due {
            publish_due = !publish_live(
                &control.shared.live,
                start,
                (mixed, late, skipped),
                &mix_times,
                &mixer,
                &inputs,
            );
        }
        block += 1;
    }

    let elapsed = start.elapsed();
    MixStats {
        rate: RATE,
        channels: CHANNELS,
        block_frames: BLOCK_FRAMES,
        elapsed_ms: duration_millis(elapsed),
        blocks_mixed: mixed,
        late_blocks: late,
        skipped_blocks: skipped,
        mix_time: mix_times.summary(),
        master: mixer.master_meter(),
        clipped_samples: mixer.clipped_samples(),
        invalid_samples: mixer.invalid_samples(),
        inputs: inputs
            .iter()
            .enumerate()
            .map(|(index, input)| {
                let (kind, ring, buffer, drift_ppm) = match &input.source {
                    Source::Live(live, trend) => (
                        "live",
                        Some(live.ring().stats()),
                        trend.latency(live.target()),
                        Some(live.drift_ppm()),
                    ),
                    Source::Tone(_) => ("tone", None, None, None),
                };
                let strip = mixer.input_config(index).unwrap_or(StripConfig {
                    gain_db: 0.0,
                    muted: false,
                });
                InputStats {
                    name: input.name.clone(),
                    kind,
                    gain_db: strip.gain_db,
                    muted: strip.muted,
                    meter: mixer.input_meter(index).unwrap_or(MeterReading {
                        peak_dbfs: None,
                        rms_dbfs: None,
                    }),
                    warmup_frames: input.warmup,
                    underrun_frames: input.underrun,
                    trimmed_frames: input.trimmed,
                    drift_ppm,
                    ring,
                    buffer,
                }
            })
            .collect(),
    }
}

/// Maximum number of mixer inputs.
pub const MAX_INPUTS: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;

    fn unity() -> StripConfig {
        StripConfig {
            gain_db: 0.0,
            muted: false,
        }
    }

    #[test]
    fn mixer_thread_mixes_tone_and_live_input() {
        let ring = Arc::new(AudioRing::new(2, 48_000));
        let mixer = MixerThread::spawn(MixerConfig {
            inputs: vec![
                MixerInput {
                    name: "tone".into(),
                    source: InputSource::Tone(Tone::new(1000.0, -6.0, RATE)),
                    strip: unity(),
                },
                MixerInput {
                    name: "live".into(),
                    source: InputSource::Live(Arc::clone(&ring)),
                    strip: StripConfig {
                        gain_db: -6.0,
                        muted: false,
                    },
                },
            ],
            master_gain_db: 0.0,
            expected_duration: Duration::from_secs(1),
            sink: None,
        })
        .expect("spawn");
        for _ in 0..30 {
            ring.push(std::iter::repeat_n(0.25, 480 * 2));
            thread::sleep(Duration::from_millis(10));
        }
        let stats = mixer.stop().expect("stop");
        assert!(stats.blocks_mixed >= 10, "{stats:?}");
        assert_eq!(stats.inputs[0].kind, "tone");
        let tone_peak = stats.inputs[0].meter.peak_dbfs.expect("tone level");
        assert!((tone_peak + 6.0).abs() < 0.01, "{tone_peak}");
        let live = &stats.inputs[1];
        assert_eq!(live.kind, "live");
        assert!(live.ring.expect("ring").pulled_frames > 0);
        let live_peak = live.meter.peak_dbfs.expect("live level");
        // 0.25 at -6 dB.
        assert!(
            (live_peak - 20.0 * (0.25f64 * 0.501_187).log10()).abs() < 0.01,
            "{live_peak}"
        );
        assert!(live.buffer.is_some());
    }

    struct RecordingSink(Arc<std::sync::Mutex<Vec<(usize, Instant)>>>);

    impl BlockSink for RecordingSink {
        fn block(&mut self, samples: &[f32], at: Instant) {
            self.0.lock().expect("lock").push((samples.len(), at));
        }
    }

    /// Poll `levels` until `done` holds (bounded), so slow runners pass.
    fn wait_levels(control: &MixerControl, done: impl Fn(&LevelReading) -> bool) -> LevelReading {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let levels = control.levels();
            if done(&levels) || Instant::now() >= deadline {
                return levels;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn live_control_changes_gain_and_reports_levels() {
        let mixer = MixerThread::spawn(MixerConfig {
            inputs: vec![MixerInput {
                name: "tone".into(),
                source: InputSource::Tone(Tone::new(1000.0, -6.0, RATE)),
                strip: StripConfig {
                    gain_db: 0.0,
                    muted: false,
                },
            }],
            master_gain_db: 0.0,
            expected_duration: Duration::from_secs(1),
            sink: None,
        })
        .expect("spawn");
        let control = mixer.control();
        let before = wait_levels(&control, |levels| levels.inputs[0].is_some());
        let peak = before.inputs[0].expect("tone level");
        assert!((peak + 6.0).abs() < 0.1, "{before:?}");
        assert!(before.master.is_some());
        // Reading does not consume the levels.
        assert!(control.levels().inputs[0].is_some());
        assert!(control.set_input(
            0,
            StripConfig {
                gain_db: 0.0,
                muted: true
            }
        ));
        assert!(!control.set_input(
            5,
            StripConfig {
                gain_db: 0.0,
                muted: false
            }
        ));
        // Silence once the window has passed the mute.
        let muted = wait_levels(&control, |levels| levels.inputs[0].is_none());
        assert_eq!(muted.inputs[0], None, "{muted:?}");
        assert_eq!(muted.master, None);
        let stats = mixer.stop().expect("stop");
        assert!(stats.inputs[0].muted);
    }

    #[test]
    fn sink_receives_every_block_at_exact_spacing() {
        let blocks = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mixer = MixerThread::spawn(MixerConfig {
            inputs: vec![MixerInput {
                name: "tone".into(),
                source: InputSource::Tone(Tone::new(440.0, -6.0, RATE)),
                strip: unity(),
            }],
            master_gain_db: 0.0,
            expected_duration: Duration::from_secs(1),
            sink: Some(Box::new(RecordingSink(Arc::clone(&blocks)))),
        })
        .expect("spawn");
        thread::sleep(Duration::from_millis(120));
        let stats = mixer.stop().expect("stop");
        let blocks = blocks.lock().expect("lock");
        assert_eq!(blocks.len() as u64, stats.blocks_mixed);
        assert!(blocks.iter().all(|(len, _)| *len == BLOCK_FRAMES * 2));
        for pair in blocks.windows(2) {
            assert_eq!(pair[1].1 - pair[0].1, Duration::from_millis(10));
        }
    }

    #[test]
    fn channel_mismatch_is_rejected() {
        let result = MixerThread::spawn(MixerConfig {
            inputs: vec![MixerInput {
                name: "mono".into(),
                source: InputSource::Live(Arc::new(AudioRing::new(1, 100))),
                strip: unity(),
            }],
            master_gain_db: 0.0,
            expected_duration: Duration::from_secs(1),
            sink: None,
        });
        assert!(matches!(result, Err(MixerError::ChannelMismatch { .. })));
    }

    #[test]
    fn live_stats_show_a_silent_live_input_and_never_hold_up_the_mixer() {
        let blocks = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ring = Arc::new(AudioRing::new(2, 48_000));
        let mixer = MixerThread::spawn(MixerConfig {
            inputs: vec![
                MixerInput {
                    name: "tone".into(),
                    source: InputSource::Tone(Tone::new(440.0, -6.0, RATE)),
                    strip: unity(),
                },
                MixerInput {
                    name: "mic".into(),
                    source: InputSource::Live(Arc::clone(&ring)),
                    strip: unity(),
                },
            ],
            master_gain_db: 0.0,
            expected_duration: Duration::from_secs(1),
            sink: Some(Box::new(RecordingSink(Arc::clone(&blocks)))),
        })
        .expect("spawn");
        let control = mixer.control();
        // The live input never delivers: it stays warming up, with no fill.
        let deadline = Instant::now() + Duration::from_secs(5);
        let live = loop {
            let live = control.live();
            if live.blocks_mixed >= 30 || Instant::now() >= deadline {
                break live;
            }
            thread::sleep(Duration::from_millis(5));
        };
        assert!(
            live.blocks_mixed >= 30 && live.mix_time.is_some(),
            "{live:?}"
        );
        assert_eq!(
            live.inputs
                .iter()
                .map(|input| (input.name.as_str(), input.kind))
                .collect::<Vec<_>>(),
            [("tone", "tone"), ("mic", "live")]
        );
        let mic = &live.inputs[1];
        assert!(mic.warmup_frames > 0, "{mic:?}");
        assert_eq!(mic.fill_ms, Some(0.0));
        assert_eq!(live.inputs[0].fill_ms, None);
        // Holding the snapshot does not stall the mixer thread.
        let held = control.shared.live.lock().expect("lock");
        let before = blocks.lock().expect("lock").len();
        thread::sleep(Duration::from_millis(200));
        let during = blocks.lock().expect("lock").len() - before;
        drop(held);
        let stats = mixer.stop().expect("stop");
        assert!(
            during >= 10,
            "only {during} blocks while the snapshot was held"
        );
        assert!(stats.blocks_mixed >= live.blocks_mixed);
    }
}
