//! Video compositor: scene graph, CPU reference renderer and render loop.
//!
//! Independent of UI, capture and encoding. Sources are plain
//! [`media_core::VideoFrame`]s (static) or [`media_core::FrameMailbox`]es
//! (live); the render loop runs on its own thread at a fixed frame rate.

pub mod geometry;
pub mod pattern;
pub mod render;
pub mod runner;
pub mod scale;
pub mod scene;
pub mod snapshot;
pub mod stats;

pub use geometry::{Rect, Transform};
pub use render::{CpuRenderer, RenderError, RenderOutcome};
pub use runner::{
    Animation, Compositor, CompositorConfig, CompositorError, CompositorRun, FrameSink,
    LIVE_PUBLISH_MS, LiveRenderStats, LiveSourceStats, RenderStats, SceneUpdate, SourceConfig,
    SourceInput, SourceStats,
};
pub use scale::downscale;
pub use scene::{Layer, Scene, SceneError, SourceId};
pub use stats::LatencySummary;
