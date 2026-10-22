//! Measures CPU render time for the M3 reference scene.
//!
//! ```text
//! cargo run --release -p video-compositor --example render_bench [frames]
//! ```
//!
//! Scene: 1920x1080 color bars, a 2560x1440 BGRx source scaled into
//! 1152x648 at 90 % opacity, and a 160x160 half-transparent overlay.

use std::time::Instant;

use media_core::{FrameBuffer, PixelFormat, VideoFrame};
use video_compositor::stats::LatencyHistogram;
use video_compositor::{CpuRenderer, Layer, Rect, Scene, SourceId, Transform, pattern};

fn main() {
    let frames: u32 = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(300);
    let bars = pattern::color_bars(1920, 1080).expect("bars");
    let screen = synthetic_screen(2560, 1440);
    let overlay = pattern::solid(160, 160, [128, 128, 128, 128]).expect("overlay");
    let mut scene = Scene::new(1920, 1080);
    scene.layers = vec![
        Layer::new("bars", SourceId::from("0"), Transform::IDENTITY),
        Layer::new(
            "screen",
            SourceId::from("1"),
            Transform::fit(
                2560,
                1440,
                Rect {
                    x: 704.0,
                    y: 368.0,
                    width: 1152.0,
                    height: 648.0,
                },
            ),
        )
        .with_opacity(0.9),
        Layer::new("overlay", SourceId::from("2"), Transform::at(100.5, 200.25)),
    ];
    let sources = [
        (SourceId::from("0"), &bars),
        (SourceId::from("1"), &screen),
        (SourceId::from("2"), &overlay),
    ];
    let mut target = VideoFrame::zeroed(1920, 1080, PixelFormat::Bgra8).expect("target");
    let max_threads = CpuRenderer::with_available_parallelism().threads();
    for threads in [1, max_threads] {
        let mut renderer = CpuRenderer::new(threads);
        let mut histogram = LatencyHistogram::new();
        for _ in 0..frames {
            let start = Instant::now();
            renderer
                .render(
                    &scene,
                    |id| sources.iter().find(|(s, _)| s == id).map(|(_, f)| *f),
                    &mut target,
                )
                .expect("render");
            histogram.record(start.elapsed());
        }
        let s = histogram.summary().expect("summary");
        println!(
            "threads={threads:<2} frames={frames} mean={:.2} ms p95={:.2} ms p99={:.2} ms max={:.2} ms  (60 FPS budget 16.67 ms)",
            s.mean_ms, s.p95_ms, s.p99_ms, s.max_ms
        );
    }
}

fn synthetic_screen(width: u32, height: u32) -> VideoFrame {
    let mut data = vec![0u8; (width * height * 4) as usize];
    for (i, px) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let (x, y) = (i as u32 % width, i as u32 / width);
        *px = [(x % 256) as u8, (y % 256) as u8, ((x ^ y) % 256) as u8, 0];
    }
    VideoFrame::new(
        width,
        height,
        width as usize * 4,
        PixelFormat::Bgrx8,
        None,
        FrameBuffer::unpooled(data),
    )
    .expect("screen")
}
