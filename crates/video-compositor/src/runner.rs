//! Render loop on its own thread, driven by a fixed monotonic frame clock.
//!
//! Independent of any UI: it pulls the newest frame of each live source
//! from its [`FrameMailbox`], renders, and records timing. When rendering
//! falls behind by whole frames, those ticks are skipped (not rendered in a
//! burst) and counted.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use media_core::{FrameMailbox, MailboxStats, PixelFormat, VideoFrame, VideoFrameError};
use serde::Serialize;
use thiserror::Error;

use crate::render::{CpuRenderer, RenderError};
use crate::scene::{Scene, SceneError, SourceId};
use crate::stats::{LatencyHistogram, LatencySummary};

pub const MAX_FPS: u32 = 240;

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

pub struct CompositorConfig {
    pub scene: Scene,
    pub fps: u32,
    pub sources: Vec<SourceConfig>,
    pub animation: Option<Animation>,
    pub renderer: CpuRenderer,
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
    pub target_fps: u32,
    pub elapsed_ms: u64,
    pub frames_rendered: u64,
    /// Ticks skipped because rendering fell at least one frame behind.
    pub skipped_ticks: u64,
    /// Frames that finished rendering after the next tick was due.
    pub late_frames: u64,
    pub achieved_fps: Option<f64>,
    pub render_time: Option<LatencySummary>,
    pub sources: Vec<SourceStats>,
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
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::Builder::new()
            .name("compositor".to_owned())
            .spawn({
                let stop = Arc::clone(&stop);
                move || run(config, target, &stop)
            })
            .map_err(CompositorError::Spawn)?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
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
}

impl SourceState {
    fn poll(&mut self) {
        let Current::Live { mailbox, frame } = &mut self.current else {
            return;
        };
        match mailbox.take() {
            Some(new) => {
                // The previous frame's pooled buffer returns to its producer.
                *frame = Some(new);
                self.consumed += 1;
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

    fn stats(&self) -> SourceStats {
        let (kind, mailbox) = match &self.current {
            Current::Static(_) => ("static", None),
            Current::Live { mailbox, .. } => ("live", Some(mailbox.stats())),
        };
        SourceStats {
            id: self.id,
            name: self.name.clone(),
            kind,
            frames_consumed: self.consumed,
            stale_ticks: self.stale,
            missing_ticks: self.missing,
            mailbox,
        }
    }
}

fn run(
    config: CompositorConfig,
    mut target: VideoFrame,
    stop: &AtomicBool,
) -> Result<CompositorRun, CompositorError> {
    let CompositorConfig {
        mut scene,
        fps,
        sources,
        animation,
        mut renderer,
    } = config;
    let mut sources: Vec<SourceState> = sources
        .into_iter()
        .map(|source| SourceState {
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
        })
        .collect();

    let period_ns = 1_000_000_000 / u64::from(fps);
    let due =
        |start: Instant, tick: u64| start + Duration::from_nanos(period_ns.saturating_mul(tick));
    let mut render_times = LatencyHistogram::new();
    let (mut tick, mut rendered, mut skipped, mut late) = (0u64, 0u64, 0u64, 0u64);
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

        for source in &mut sources {
            source.poll();
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
                    .find(|source| source.id == id)
                    .and_then(SourceState::frame)
            },
            &mut target,
        )?;
        let finished = Instant::now();
        render_times.record(finished - render_start);
        rendered += 1;
        if finished > due(start, tick + 1) {
            late += 1;
        }
        tick += 1;
    }

    let elapsed = start.elapsed();
    let stats = RenderStats {
        renderer: CpuRenderer::NAME,
        threads: renderer.threads(),
        width: scene.width,
        height: scene.height,
        target_fps: fps,
        elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        frames_rendered: rendered,
        skipped_ticks: skipped,
        late_frames: late,
        achieved_fps: (elapsed > Duration::ZERO && rendered > 0)
            .then(|| rendered as f64 / elapsed.as_secs_f64()),
        render_time: render_times.summary(),
        sources: sources.iter().map(SourceState::stats).collect(),
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
            vec![Layer::new("l", SourceId(0), Transform::IDENTITY).with_opacity(2.0)],
        );
        assert!(matches!(
            Compositor::spawn(bad),
            Err(CompositorError::Scene(_))
        ));
    }

    #[test]
    fn dropping_without_stop_ends_the_render_thread() {
        let mailbox = Arc::new(FrameMailbox::new());
        let compositor = Compositor::spawn(config(
            vec![SourceConfig {
                id: SourceId(0),
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
                    id: SourceId(0),
                    name: "bars".into(),
                    input: SourceInput::Static(bars),
                },
                SourceConfig {
                    id: SourceId(1),
                    name: "live".into(),
                    input: SourceInput::Live(Arc::clone(&mailbox)),
                },
            ],
            vec![
                Layer::new("bars", SourceId(0), Transform::IDENTITY),
                Layer::new("live", SourceId(1), Transform::at(8.0, 8.0)),
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
}
