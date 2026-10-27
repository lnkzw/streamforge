//! Render loop on its own thread, driven by a fixed monotonic frame clock.
//!
//! Independent of any UI: it pulls the newest frame of each live source
//! from its [`FrameMailbox`], renders, and records timing. When rendering
//! falls behind by whole frames, those ticks are skipped (not rendered in a
//! burst) and counted.
//!
//! The scene can be replaced while running ([`Compositor::update`]): the
//! update waits in a single "newest wins" slot that the render thread only
//! try-locks before a frame, so editing never blocks rendering and pending
//! edits never queue up.
//!
//! Live statistics ([`Compositor::live`]) work the other way round: every
//! [`LIVE_PUBLISH_MS`] the render thread updates a shared snapshot in place,
//! again only after a successful try-lock, so a reader never delays a frame.
//! A refresh that finds the snapshot busy is retried on the next frame.
//! Publishing reuses the snapshot's allocations; it allocates only after a
//! scene update (more sources, or longer names), which already allocates in
//! `rebuild`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use media_core::{
    FrameMailbox, MailboxStats, PixelFormat, Rational, VideoFrame, VideoFrameError, duration_millis,
};
use serde::Serialize;
use thiserror::Error;

use crate::render::{CpuRenderer, RenderError};
use crate::scene::{Scene, SceneError, SourceId};
use crate::stats::{LatencyHistogram, LatencySummary};

pub const MAX_FPS: u32 = 240;
/// How often the render thread refreshes [`Compositor::live`].
pub const LIVE_PUBLISH_MS: u64 = 250;
/// Sources removed by updates whose stats are kept for the report (the
/// most recent ones; `RenderStats::retired_sources` counts all).
pub const MAX_RETIRED_SOURCES: usize = 64;

pub enum SourceInput {
    /// A frame that never changes, e.g. a test pattern.
    Static(Arc<VideoFrame>),
    /// Frames delivered by a producer such as a capture stream.
    Live(Arc<FrameMailbox>),
}

pub struct SourceConfig {
    pub id: SourceId,
    pub name: String,
    pub input: SourceInput,
}

/// Mutates the scene before frame `tick` is rendered (e.g. moves a layer).
pub type Animation = fn(&mut Scene, u64);

/// Receives every rendered canvas on the compositor thread, with the tick's
/// nominal presentation time (so timestamps are exactly frame-spaced).
/// Must not block: copy what is needed and hand it off.
pub trait FrameSink: Send {
    fn frame(&mut self, frame: &VideoFrame, presented_at: Instant);
}

pub struct CompositorConfig {
    pub scene: Scene,
    pub fps: u32,
    pub sources: Vec<SourceConfig>,
    pub animation: Option<Animation>,
    pub renderer: CpuRenderer,
    /// Optional consumer of rendered frames, e.g. a recorder.
    pub sink: Option<Box<dyn FrameSink>>,
}

/// A replacement scene for a running compositor. Sources whose id and input
/// (the same static frame or mailbox) are unchanged keep their state.
pub struct SceneUpdate {
    pub scene: Scene,
    pub sources: Vec<SourceConfig>,
    pub animation: Option<Animation>,
}

#[derive(Debug, Error)]
pub enum CompositorError {
    #[error(transparent)]
    Scene(#[from] SceneError),
    #[error("frame rate must be between 1 and {MAX_FPS}, got {0}")]
    InvalidFrameRate(u32),
    #[error(transparent)]
    Frame(#[from] VideoFrameError),
    #[error(transparent)]
    Render(#[from] RenderError),
    #[error("cannot start the compositor thread: {0}")]
    Spawn(std::io::Error),
    #[error("the compositor thread panicked")]
    Panicked,
    #[error("an update must keep the canvas size {expected:?}, got {got:?}")]
    CanvasMismatch {
        expected: (u32, u32),
        got: (u32, u32),
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceStats {
    pub id: SourceId,
    pub name: String,
    /// `static` or `live`.
    pub kind: &'static str,
    /// New frames taken from the source.
    pub frames_consumed: u64,
    /// Ticks that reused the previous frame because no new one had arrived.
    pub stale_ticks: u64,
    /// Ticks rendered before the source delivered its first frame.
    pub missing_ticks: u64,
    /// Producer-side counters, including frames replaced before use.
    pub mailbox: Option<MailboxStats>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RenderStats {
    pub renderer: &'static str,
    pub threads: usize,
    pub width: u32,
    pub height: u32,
    /// Nominal frame rate.
    pub target_fps: Rational,
    pub elapsed_ms: u64,
    pub frames_rendered: u64,
    /// Ticks skipped because rendering fell at least one frame behind.
    pub skipped_ticks: u64,
    /// Frames that finished rendering after the next tick was due.
    pub late_frames: u64,
    pub achieved_fps: Option<f64>,
    pub render_time: Option<LatencySummary>,
    /// Scene updates applied while running.
    pub scene_updates: u64,
    /// Sources removed by updates (only the last
    /// [`MAX_RETIRED_SOURCES`] are listed in `sources`).
    pub retired_sources: u64,
    /// Current sources, then the most recently removed ones.
    pub sources: Vec<SourceStats>,
}

/// A running compositor's counters so far (see [`Compositor::live`]); the
/// same meaning as in [`RenderStats`], refreshed every [`LIVE_PUBLISH_MS`].
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LiveRenderStats {
    pub elapsed_ms: u64,
    pub frames_rendered: u64,
    pub skipped_ticks: u64,
    pub late_frames: u64,
    pub render_time: Option<LatencySummary>,
    pub sources: Vec<LiveSourceStats>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LiveSourceStats {
    pub id: SourceId,
    pub name: String,
    /// `static` or `live`.
    pub kind: &'static str,
    pub frames_consumed: u64,
    pub stale_ticks: u64,
    pub missing_ticks: u64,
    /// Time since this live source last delivered a new frame, or since it
    /// was added if it never did (`None` for static sources). A stalled or
    /// ended capture keeps growing here while outputs run; `frames_consumed`
    /// of 0 tells "never delivered" from "stopped delivering".
    pub last_frame_age_ms: Option<u64>,
}

pub struct CompositorRun {
    pub stats: RenderStats,
    /// The most recently rendered canvas.
    pub last_frame: VideoFrame,
}

/// A running render loop; call [`Compositor::stop`] to end it.
///
/// Dropping it without `stop` still stops and joins the render thread.
pub struct Compositor {
    stop: Arc<AtomicBool>,
    pending: Arc<Mutex<Option<SceneUpdate>>>,
    live: Arc<Mutex<LiveRenderStats>>,
    canvas: (u32, u32),
    thread: Option<JoinHandle<Result<CompositorRun, CompositorError>>>,
}

impl Compositor {
    pub fn spawn(config: CompositorConfig) -> Result<Self, CompositorError> {
        config.scene.validate()?;
        if !(1..=MAX_FPS).contains(&config.fps) {
            return Err(CompositorError::InvalidFrameRate(config.fps));
        }
        let target =
            VideoFrame::zeroed(config.scene.width, config.scene.height, PixelFormat::Bgra8)?;
        let canvas = (config.scene.width, config.scene.height);
        let stop = Arc::new(AtomicBool::new(false));
        let pending = Arc::new(Mutex::new(None));
        let live = Arc::new(Mutex::new(LiveRenderStats::default()));
        let thread = thread::Builder::new()
            .name("compositor".to_owned())
            .spawn({
                let stop = Arc::clone(&stop);
                let pending = Arc::clone(&pending);
                let live = Arc::clone(&live);
                move || run(config, target, &stop, &pending, &live)
            })
            .map_err(CompositorError::Spawn)?;
        Ok(Self {
            stop,
            pending,
            live,
            canvas,
            thread: Some(thread),
        })
    }

    /// Replace scene, sources and animation from the next frame on. The
    /// update is validated here; an update not yet picked up is replaced
    /// (newest wins). The render thread never waits for this.
    pub fn update(&self, update: SceneUpdate) -> Result<(), CompositorError> {
        update.scene.validate()?;
        let got = (update.scene.width, update.scene.height);
        if got != self.canvas {
            return Err(CompositorError::CanvasMismatch {
                expected: self.canvas,
                got,
            });
        }
        *self.pending.lock().unwrap_or_else(PoisonError::into_inner) = Some(update);
        Ok(())
    }

    /// Counters so far, at most [`LIVE_PUBLISH_MS`] old. Never delays the
    /// render thread (it skips a refresh while this copies).
    pub fn live(&self) -> LiveRenderStats {
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The render thread ended on its own (it failed or panicked).
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Stop after the frame in progress and return the run's statistics.
    pub fn stop(mut self) -> Result<CompositorRun, CompositorError> {
        self.stop.store(true, Ordering::Release);
        let thread = self.thread.take().ok_or(CompositorError::Panicked)?;
        thread.join().map_err(|_| CompositorError::Panicked)?
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // The run result is not wanted when dropping.
            let _ = thread.join();
        }
    }
}

enum Current {
    Static(Arc<VideoFrame>),
    Live {
        mailbox: Arc<FrameMailbox>,
        frame: Option<VideoFrame>,
    },
}

struct SourceState {
    id: SourceId,
    name: String,
    current: Current,
    consumed: u64,
    stale: u64,
    missing: u64,
    /// Tick at which a live source last delivered a new frame, or was
    /// added (so a source that never delivers ages too).
    last_new_tick: u64,
}

impl SourceState {
    fn new(source: SourceConfig, tick: u64) -> Self {
        Self {
            id: source.id,
            name: source.name,
            current: match source.input {
                SourceInput::Static(frame) => Current::Static(frame),
                SourceInput::Live(mailbox) => Current::Live {
                    mailbox,
                    frame: None,
                },
            },
            consumed: 0,
            stale: 0,
            missing: 0,
            last_new_tick: tick,
        }
    }

    /// Same id and the same underlying frame or mailbox.
    fn matches(&self, source: &SourceConfig) -> bool {
        self.id == source.id
            && match (&self.current, &source.input) {
                (Current::Static(a), SourceInput::Static(b)) => Arc::ptr_eq(a, b),
                (Current::Live { mailbox, .. }, SourceInput::Live(other)) => {
                    Arc::ptr_eq(mailbox, other)
                }
                _ => false,
            }
    }

    fn poll(&mut self, tick: u64) {
        let Current::Live { mailbox, frame } = &mut self.current else {
            return;
        };
        match mailbox.take() {
            Some(new) => {
                // The previous frame's pooled buffer returns to its producer.
                *frame = Some(new);
                self.consumed += 1;
                self.last_new_tick = tick;
            }
            None if frame.is_some() => self.stale += 1,
            None => self.missing += 1,
        }
    }

    fn frame(&self) -> Option<&VideoFrame> {
        match &self.current {
            Current::Static(frame) => Some(frame),
            Current::Live { frame, .. } => frame.as_ref(),
        }
    }

    /// Write this source's live counters into `into`, reusing its name's
    /// allocation.
    fn live_into(&self, into: &mut LiveSourceStats, tick: u64, period_ns: u64) {
        into.id.clone_from(&self.id);
        into.name.clone_from(&self.name);
        into.kind = self.kind();
        into.frames_consumed = self.consumed;
        into.stale_ticks = self.stale;
        into.missing_ticks = self.missing;
        into.last_frame_age_ms = matches!(self.current, Current::Live { .. }).then(|| {
            tick.saturating_sub(self.last_new_tick)
                .saturating_mul(period_ns)
                / 1_000_000
        });
    }

    fn kind(&self) -> &'static str {
        match &self.current {
            Current::Static(_) => "static",
            Current::Live { .. } => "live",
        }
    }

    fn stats(&self) -> SourceStats {
        let (kind, mailbox) = match &self.current {
            Current::Static(_) => ("static", None),
            Current::Live { mailbox, .. } => ("live", Some(mailbox.stats())),
        };
        SourceStats {
            id: self.id.clone(),
            name: self.name.clone(),
            kind,
            frames_consumed: self.consumed,
            stale_ticks: self.stale,
            missing_ticks: self.missing,
            mailbox,
        }
    }
}

/// Take a pending update without waiting for the slot.
fn take_update(pending: &Mutex<Option<SceneUpdate>>) -> Option<SceneUpdate> {
    match pending.try_lock() {
        Ok(mut slot) => slot.take(),
        Err(TryLockError::Poisoned(slot)) => slot.into_inner().take(),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// Keep the state of unchanged sources; retire the rest (for the stats).
/// Runs on the render thread only when an update arrives; the rebuild
/// allocates, which is acceptable outside the per-frame path.
fn rebuild(
    mut current: Vec<SourceState>,
    sources: Vec<SourceConfig>,
    retired: &mut Retired,
    tick: u64,
) -> Vec<SourceState> {
    let mut next = Vec::with_capacity(sources.len());
    for source in sources {
        match current.iter().position(|state| state.matches(&source)) {
            Some(index) => {
                let mut state = current.swap_remove(index);
                state.name = source.name;
                // The update's id, which its layers share: `Arc` equality
                // then compares pointers first.
                state.id = source.id;
                next.push(state);
            }
            None => next.push(SourceState::new(source, tick)),
        }
    }
    for state in &current {
        retired.push(state.stats());
    }
    next
}

/// Stats of removed sources, bounded so a long session of scene switches
/// cannot grow the report without limit.
#[derive(Default)]
struct Retired {
    recent: Vec<SourceStats>,
    total: u64,
}

impl Retired {
    fn push(&mut self, stats: SourceStats) {
        if self.recent.len() == MAX_RETIRED_SOURCES {
            self.recent.remove(0);
        }
        self.recent.push(stats);
        self.total += 1;
    }
}

/// Refresh the shared snapshot in place; `false` if a reader holds it now
/// (the caller retries on the next frame).
fn publish(
    live: &Mutex<LiveRenderStats>,
    start: Instant,
    counts: (u64, u64, u64),
    render_times: &LatencyHistogram,
    sources: &[SourceState],
    tick: u64,
    period_ns: u64,
) -> bool {
    let mut snapshot = match live.try_lock() {
        Ok(snapshot) => snapshot,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => return false,
    };
    let (rendered, skipped, late) = counts;
    snapshot.elapsed_ms = duration_millis(start.elapsed());
    snapshot.frames_rendered = rendered;
    snapshot.skipped_ticks = skipped;
    snapshot.late_frames = late;
    snapshot.render_time = render_times.summary();
    snapshot.sources.truncate(sources.len());
    for (index, source) in sources.iter().enumerate() {
        match snapshot.sources.get_mut(index) {
            Some(entry) => source.live_into(entry, tick, period_ns),
            None => {
                // Only when the source list grows.
                let mut entry = LiveSourceStats {
                    id: source.id.clone(),
                    name: String::new(),
                    kind: "static",
                    frames_consumed: 0,
                    stale_ticks: 0,
                    missing_ticks: 0,
                    last_frame_age_ms: None,
                };
                source.live_into(&mut entry, tick, period_ns);
                snapshot.sources.push(entry);
            }
        }
    }
    true
}

fn run(
    config: CompositorConfig,
    mut target: VideoFrame,
    stop: &AtomicBool,
    pending: &Mutex<Option<SceneUpdate>>,
    live: &Mutex<LiveRenderStats>,
) -> Result<CompositorRun, CompositorError> {
    let CompositorConfig {
        mut scene,
        fps,
        sources,
        mut animation,
        mut renderer,
        mut sink,
    } = config;
    let mut sources: Vec<SourceState> = sources
        .into_iter()
        .map(|source| SourceState::new(source, 0))
        .collect();
    let mut retired = Retired::default();
    let mut updates = 0u64;

    let period_ns = 1_000_000_000 / u64::from(fps);
    let due =
        |start: Instant, tick: u64| start + Duration::from_nanos(period_ns.saturating_mul(tick));
    let mut render_times = LatencyHistogram::new();
    let (mut tick, mut rendered, mut skipped, mut late) = (0u64, 0u64, 0u64, 0u64);
    let publish_every = (u64::from(fps) * LIVE_PUBLISH_MS / 1000).max(1);
    let mut publish_due = false;
    let start = Instant::now();

    while !stop.load(Ordering::Acquire) {
        let deadline = due(start, tick);
        let now = Instant::now();
        if now < deadline {
            thread::sleep(deadline - now);
            if stop.load(Ordering::Acquire) {
                break;
            }
        }
        let behind = u64::try_from((Instant::now() - deadline).as_nanos() / u128::from(period_ns))
            .unwrap_or(u64::MAX);
        if behind > 0 {
            skipped += behind;
            tick += behind;
        }

        if let Some(update) = take_update(pending) {
            scene = update.scene;
            animation = update.animation;
            sources = rebuild(sources, update.sources, &mut retired, tick);
            updates += 1;
        }
        for source in &mut sources {
            source.poll(tick);
        }
        if let Some(animate) = animation {
            animate(&mut scene, tick);
        }
        let render_start = Instant::now();
        renderer.render(
            &scene,
            |id| {
                sources
                    .iter()
                    .find(|source| source.id == *id)
                    .and_then(SourceState::frame)
            },
            &mut target,
        )?;
        if let Some(sink) = &mut sink {
            sink.frame(&target, due(start, tick));
        }
        let finished = Instant::now();
        render_times.record(finished - render_start);
        rendered += 1;
        if finished > due(start, tick + 1) {
            late += 1;
        }
        publish_due |= (rendered - 1) % publish_every == 0;
        if publish_due {
            publish_due = !publish(
                live,
                start,
                (rendered, skipped, late),
                &render_times,
                &sources,
                tick,
                period_ns,
            );
        }
        tick += 1;
    }

    let elapsed = start.elapsed();
    let stats = RenderStats {
        renderer: CpuRenderer::NAME,
        threads: renderer.threads(),
        width: scene.width,
        height: scene.height,
        target_fps: Rational::integer(fps),
        elapsed_ms: duration_millis(elapsed),
        frames_rendered: rendered,
        skipped_ticks: skipped,
        late_frames: late,
        achieved_fps: (elapsed > Duration::ZERO && rendered > 0)
            .then(|| rendered as f64 / elapsed.as_secs_f64()),
        render_time: render_times.summary(),
        scene_updates: updates,
        retired_sources: retired.total,
        sources: sources
            .iter()
            .map(SourceState::stats)
            .chain(retired.recent)
            .collect(),
    };
    Ok(CompositorRun {
        stats,
        last_frame: target,
    })
}

#[cfg(test)]
mod tests {
    use media_core::FramePool;

    use super::*;
    use crate::geometry::Transform;
    use crate::pattern;
    use crate::scene::Layer;

    fn config(sources: Vec<SourceConfig>, layers: Vec<Layer>) -> CompositorConfig {
        CompositorConfig {
            scene: Scene {
                width: 64,
                height: 48,
                background: [0, 0, 0, 255],
                layers,
            },
            fps: 100,
            sources,
            animation: None,
            renderer: CpuRenderer::new(1),
            sink: None,
        }
    }

    #[test]
    fn invalid_configuration_is_rejected_before_spawning() {
        let mut bad = config(vec![], vec![]);
        bad.fps = 0;
        assert!(matches!(
            Compositor::spawn(bad),
            Err(CompositorError::InvalidFrameRate(0))
        ));
        let bad = config(
            vec![],
            vec![Layer::new("l", SourceId::from("0"), Transform::IDENTITY).with_opacity(2.0)],
        );
        assert!(matches!(
            Compositor::spawn(bad),
            Err(CompositorError::Scene(_))
        ));
    }

    struct CountingSink(Arc<std::sync::Mutex<Vec<Instant>>>);

    impl FrameSink for CountingSink {
        fn frame(&mut self, frame: &VideoFrame, presented_at: Instant) {
            assert_eq!(frame.width(), 64);
            self.0.lock().expect("lock").push(presented_at);
        }
    }

    #[test]
    fn sink_receives_frames_at_exact_frame_spacing() {
        let times = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut config = config(vec![], vec![]);
        config.sink = Some(Box::new(CountingSink(Arc::clone(&times))));
        let compositor = Compositor::spawn(config).expect("spawn");
        thread::sleep(Duration::from_millis(100));
        let run = compositor.stop().expect("stop");
        let times = times.lock().expect("lock");
        assert_eq!(times.len() as u64, run.stats.frames_rendered);
        // 100 FPS: nominal times are multiples of 10 ms apart.
        for pair in times.windows(2) {
            let step = pair[1] - pair[0];
            assert_eq!(step.as_nanos() % 10_000_000, 0, "{step:?}");
            assert!(step >= Duration::from_millis(10));
        }
    }

    /// Poll `done` until it holds (bounded), so slow runners pass.
    fn wait_until(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// Records how many frames were rendered and the last top-left pixel.
    struct PixelSink(Arc<std::sync::Mutex<(u64, [u8; 4])>>);

    impl FrameSink for PixelSink {
        fn frame(&mut self, frame: &VideoFrame, _presented_at: Instant) {
            let mut seen = self.0.lock().expect("lock");
            *seen = (seen.0 + 1, frame.pixel(10, 10));
        }
    }

    #[test]
    fn updates_replace_the_scene_while_running_and_keep_unchanged_sources() {
        let red = Arc::new(pattern::solid(64, 48, [0, 0, 255, 255]).expect("red"));
        let blue = Arc::new(pattern::solid(64, 48, [255, 0, 0, 255]).expect("blue"));
        let source = |id: u32, name: &str, frame: &Arc<VideoFrame>| SourceConfig {
            id: SourceId::new(id.to_string()),
            name: name.into(),
            input: SourceInput::Static(Arc::clone(frame)),
        };
        let layer = |name: &str, id: u32| {
            Layer::new(name, SourceId::new(id.to_string()), Transform::IDENTITY)
        };
        let seen = Arc::new(std::sync::Mutex::new((0, [0; 4])));
        let mut red_scene = config(vec![source(1, "red", &red)], vec![layer("red", 1)]);
        red_scene.sink = Some(Box::new(PixelSink(Arc::clone(&seen))));
        let compositor = Compositor::spawn(red_scene).expect("spawn");
        wait_until("a red frame", || {
            seen.lock().expect("lock").1 == [0, 0, 255, 255]
        });
        let scene = |layers| Scene {
            width: 64,
            height: 48,
            background: [0, 0, 0, 255],
            layers,
        };
        // Keep red (same id and frame), add blue on top.
        compositor
            .update(SceneUpdate {
                scene: scene(vec![layer("red", 1), layer("blue", 2)]),
                sources: vec![source(1, "red", &red), source(2, "blue", &blue)],
                animation: None,
            })
            .expect("update");
        // A different canvas size is rejected before it reaches the thread.
        let mut wrong = scene(vec![]);
        wrong.width = 32;
        assert!(matches!(
            compositor.update(SceneUpdate {
                scene: wrong,
                sources: vec![],
                animation: None,
            }),
            Err(CompositorError::CanvasMismatch { .. })
        ));
        wait_until("a blue frame", || {
            seen.lock().expect("lock").1 == [255, 0, 0, 255]
        });
        let run = compositor.stop().expect("stop");
        assert_eq!(run.stats.scene_updates, 1);
        // Blue covers red in the last frame.
        assert_eq!(run.last_frame.pixel(10, 10), [255, 0, 0, 255]);
        let red_stats = &run.stats.sources[0];
        assert_eq!(red_stats.name, "red");
        assert_eq!(red_stats.kind, "static");
        // Red was kept, not retired and re-created (which would list it twice).
        assert_eq!(run.stats.sources.len(), 2);
    }

    #[test]
    fn retired_stats_stay_bounded() {
        let mut retired = Retired::default();
        for index in 0..(MAX_RETIRED_SOURCES as u32 + 10) {
            retired.push(SourceStats {
                id: SourceId::new(index.to_string()),
                name: index.to_string(),
                kind: "static",
                frames_consumed: 0,
                stale_ticks: 0,
                missing_ticks: 0,
                mailbox: None,
            });
        }
        assert_eq!(retired.total, MAX_RETIRED_SOURCES as u64 + 10);
        assert_eq!(retired.recent.len(), MAX_RETIRED_SOURCES);
        // The oldest were dropped.
        assert_eq!(retired.recent[0].id, SourceId::from("10"));
    }

    #[test]
    fn removed_sources_are_reported_as_retired() {
        let mailbox = Arc::new(FrameMailbox::new());
        let seen = Arc::new(std::sync::Mutex::new((0, [0; 4])));
        let mut live = config(
            vec![SourceConfig {
                id: SourceId::from("7"),
                name: "live".into(),
                input: SourceInput::Live(Arc::clone(&mailbox)),
            }],
            vec![Layer::new("live", SourceId::from("7"), Transform::IDENTITY)],
        );
        live.sink = Some(Box::new(PixelSink(Arc::clone(&seen))));
        let compositor = Compositor::spawn(live).expect("spawn");
        wait_until("a rendered frame", || seen.lock().expect("lock").0 > 0);
        compositor
            .update(SceneUpdate {
                scene: Scene::new(64, 48),
                sources: vec![],
                animation: None,
            })
            .expect("update");
        // Applying the update releases the render thread's mailbox handle.
        wait_until("the update", || Arc::strong_count(&mailbox) == 1);
        let run = compositor.stop().expect("stop");
        // The retired live source still shows its counters.
        assert_eq!(run.stats.retired_sources, 1);
        assert_eq!(run.stats.sources.len(), 1);
        assert_eq!(run.stats.sources[0].name, "live");
        assert!(run.stats.sources[0].missing_ticks > 0);
        // The render thread released the mailbox.
        assert_eq!(Arc::strong_count(&mailbox), 1);
    }

    #[test]
    fn dropping_without_stop_ends_the_render_thread() {
        let mailbox = Arc::new(FrameMailbox::new());
        let compositor = Compositor::spawn(config(
            vec![SourceConfig {
                id: SourceId::from("0"),
                name: "live".into(),
                input: SourceInput::Live(Arc::clone(&mailbox)),
            }],
            vec![],
        ))
        .expect("spawn");
        assert!(!compositor.is_finished());
        drop(compositor);
        // The thread has been joined, so it released its mailbox handle.
        assert_eq!(Arc::strong_count(&mailbox), 1);
    }

    #[test]
    fn renders_static_and_live_sources_on_its_own_thread() {
        let bars = Arc::new(pattern::color_bars(64, 48).expect("bars"));
        let mailbox = Arc::new(FrameMailbox::new());
        let pool = FramePool::new(4);
        let compositor = Compositor::spawn(config(
            vec![
                SourceConfig {
                    id: SourceId::from("0"),
                    name: "bars".into(),
                    input: SourceInput::Static(bars),
                },
                SourceConfig {
                    id: SourceId::from("1"),
                    name: "live".into(),
                    input: SourceInput::Live(Arc::clone(&mailbox)),
                },
            ],
            vec![
                Layer::new("bars", SourceId::from("0"), Transform::IDENTITY),
                Layer::new("live", SourceId::from("1"), Transform::at(8.0, 8.0)),
            ],
        ))
        .expect("spawn");

        // Two frames back to back: the first is replaced before use.
        for value in [100u8, 200] {
            let mut buffer = pool.acquire(4 * 4 * 4).expect("buffer");
            buffer.fill(value);
            mailbox.publish(
                VideoFrame::new(4, 4, 16, PixelFormat::Bgrx8, None, buffer).expect("frame"),
            );
        }
        thread::sleep(Duration::from_millis(150));
        let run = compositor.stop().expect("stop");

        let stats = &run.stats;
        assert!(stats.frames_rendered >= 3, "{stats:?}");
        assert!(stats.render_time.is_some());
        let live = &stats.sources[1];
        assert_eq!(live.kind, "live");
        assert_eq!(live.frames_consumed, 1, "{live:?}");
        assert_eq!(live.mailbox.expect("mailbox").replaced, 1);
        assert!(
            live.stale_ticks + live.missing_ticks + live.frames_consumed
                <= stats.frames_rendered + stats.skipped_ticks
        );
        // The newest live frame is drawn over the bars.
        assert_eq!(run.last_frame.pixel(9, 9), [200, 200, 200, 255]);
        assert_eq!(run.last_frame.pixel(0, 0), pattern::BARS[0]);
    }

    #[test]
    fn live_stats_follow_the_run_and_show_a_stalled_source() {
        let mailbox = Arc::new(FrameMailbox::new());
        let pool = FramePool::new(2);
        let compositor = Compositor::spawn(config(
            vec![
                SourceConfig {
                    id: SourceId::from("7"),
                    name: "capture".into(),
                    input: SourceInput::Live(Arc::clone(&mailbox)),
                },
                SourceConfig {
                    id: SourceId::from("8"),
                    name: "never".into(),
                    input: SourceInput::Live(Arc::new(FrameMailbox::new())),
                },
            ],
            vec![Layer::new(
                "capture",
                SourceId::from("7"),
                Transform::IDENTITY,
            )],
        ))
        .expect("spawn");
        // No check for an empty snapshot here: the first frame renders and
        // publishes at once, possibly before this thread reads it.
        let mut buffer = pool.acquire(4 * 4 * 4).expect("buffer");
        buffer.fill(9);
        mailbox
            .publish(VideoFrame::new(4, 4, 16, PixelFormat::Bgrx8, None, buffer).expect("frame"));
        // One frame, then the source stalls: its age grows while rendering goes on.
        wait_until("an aged source", || {
            compositor.live().sources.first().is_some_and(|source| {
                source.frames_consumed == 1
                    && source.last_frame_age_ms.is_some_and(|age| age >= 300)
            })
        });
        let live = compositor.live();
        assert!(
            live.frames_rendered > 0 && live.render_time.is_some(),
            "{live:?}"
        );
        let source = &live.sources[0];
        assert_eq!(
            (source.id.as_str(), source.name.as_str(), source.kind),
            ("7", "capture", "live")
        );
        assert!(source.stale_ticks > 0, "{source:?}");
        // A source that never delivered ages from when it was added.
        let never = &live.sources[1];
        assert_eq!(never.frames_consumed, 0);
        assert!(never.missing_ticks > 0, "{never:?}");
        assert!(
            never.last_frame_age_ms.is_some_and(|age| age >= 300),
            "{never:?}"
        );
        let run = compositor.stop().expect("stop");
        // The snapshot is at most one publish interval behind the final count.
        let behind = run.stats.frames_rendered - live.frames_rendered;
        assert!(behind <= 100, "{behind} frames behind");
    }

    #[test]
    fn a_reader_holding_the_snapshot_does_not_delay_rendering() {
        let times = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut config = config(vec![], vec![]);
        config.sink = Some(Box::new(CountingSink(Arc::clone(&times))));
        let compositor = Compositor::spawn(config).expect("spawn");
        wait_until("a first snapshot", || compositor.live().frames_rendered > 0);
        let held = compositor.live.lock().expect("lock");
        let before = times.lock().expect("lock").len();
        thread::sleep(Duration::from_millis(300));
        let during = times.lock().expect("lock").len() - before;
        drop(held);
        compositor.stop().expect("stop");
        // About 30 frames at 100 FPS; rendering went on with the snapshot locked.
        assert!(
            during >= 10,
            "only {during} frames while the snapshot was held"
        );
    }
}
