//! `probe_composite`: run the compositor on the M3 reference scene.
//!
//! Scene (1920x1080 @ 60 FPS):
//! 1. color bars, full canvas
//! 2. optional live screen capture, fitted into a 1152x648 box, 90 % opacity
//! 3. 160x160 half-transparent overlay moving left to right

use std::fs::OpenOptions;
use std::io::BufWriter;
use std::sync::Arc;
use std::thread;

use capture_api::{CaptureBackend, VideoStream};
use command_api::{
    CommandError, CompositeReport, CompositeRequest, ErrorCode, ErrorSummary, SnapshotResult,
};
use video_compositor::{
    Compositor, CompositorConfig, CpuRenderer, Layer, Rect, Scene, SourceConfig, SourceId,
    SourceInput, Transform, pattern, snapshot,
};

use crate::command_error;

pub const CANVAS_WIDTH: u32 = 1920;
pub const CANVAS_HEIGHT: u32 = 1080;
pub const FPS: u32 = 60;

const BARS: SourceId = SourceId(0);
const SCREEN: SourceId = SourceId(1);
const OVERLAY: SourceId = SourceId(2);
const OVERLAY_SIZE: u32 = 160;
const SCREEN_BOX: Rect = Rect {
    x: 704.0,
    y: 368.0,
    width: 1152.0,
    height: 648.0,
};
/// Frames for one left-to-right pass of the overlay.
const OVERLAY_PERIOD: u64 = 240;

pub(crate) fn probe(
    capture: &mut dyn CaptureBackend,
    request: &CompositeRequest,
) -> Result<CompositeReport, CommandError> {
    let stream = match &request.screen {
        Some(selection) => Some(
            capture
                .start_screen_stream(selection)
                .map_err(command_error)?,
        ),
        None => None,
    };
    let config = reference_scene(stream.as_deref()).map_err(internal)?;
    // On error the stream is dropped, which stops capturing and closes the session.
    let compositor = Compositor::spawn(config).map_err(internal)?;
    thread::sleep(request.duration);
    let run = compositor.stop().map_err(internal)?;

    let (screen_capture, screen_capture_error) = match stream.map(|stream| stream.stop()) {
        Some(Ok(report)) => (Some(report), None),
        Some(Err(error)) => (None, Some(ErrorSummary::from(error))),
        None => (None, None),
    };
    let snapshot = request.snapshot_path.as_ref().map(|path| {
        let written = OpenOptions::new()
            .write(true)
            // Never overwrite: the path comes from a possibly remote client.
            .create_new(true)
            .open(path)
            .and_then(|file| snapshot::write_ppm(&run.last_frame, &mut BufWriter::new(file)));
        SnapshotResult {
            path: path.display().to_string(),
            written: written.is_ok(),
            error: written.err().map(|err| err.to_string()),
        }
    });

    let mut warnings = Vec::new();
    let stats = &run.stats;
    if stats.late_frames > 0 || stats.skipped_ticks > 0 {
        warnings.push(format!(
            "rendering missed its deadline: {} late frame(s), {} skipped tick(s); \
             debug builds are much slower than release builds",
            stats.late_frames, stats.skipped_ticks
        ));
    }
    Ok(CompositeReport {
        render: run.stats,
        screen_capture,
        screen_capture_error,
        snapshot,
        warnings,
    })
}

fn reference_scene(
    stream: Option<&dyn VideoStream>,
) -> Result<CompositorConfig, video_compositor::CompositorError> {
    let bars = pattern::color_bars(CANVAS_WIDTH, CANVAS_HEIGHT)?;
    // 60 % gray at 60 % alpha, premultiplied.
    let overlay = pattern::solid(OVERLAY_SIZE, OVERLAY_SIZE, [92, 92, 92, 153])?;
    let mut scene = Scene::new(CANVAS_WIDTH, CANVAS_HEIGHT);
    let mut sources = vec![
        SourceConfig {
            id: BARS,
            name: "color_bars".to_owned(),
            input: SourceInput::Static(Arc::new(bars)),
        },
        SourceConfig {
            id: OVERLAY,
            name: "overlay".to_owned(),
            input: SourceInput::Static(Arc::new(overlay)),
        },
    ];
    scene
        .layers
        .push(Layer::new("color_bars", BARS, Transform::IDENTITY));
    if let Some(stream) = stream {
        let source = stream.source();
        let (width, height) = (source.width.unwrap_or(16), source.height.unwrap_or(9));
        scene.layers.push(
            Layer::new("screen", SCREEN, Transform::fit(width, height, SCREEN_BOX))
                .with_opacity(0.9),
        );
        sources.push(SourceConfig {
            id: SCREEN,
            name: "screen".to_owned(),
            input: SourceInput::Live(stream.frames()),
        });
    }
    scene
        .layers
        .push(Layer::new("overlay", OVERLAY, Transform::at(64.0, 120.0)));
    Ok(CompositorConfig {
        scene,
        fps: FPS,
        sources,
        animation: Some(move_overlay),
        renderer: CpuRenderer::with_available_parallelism(),
    })
}

/// Sweep the overlay across the canvas; sub-pixel positions exercise the
/// bilinear path.
fn move_overlay(scene: &mut Scene, tick: u64) {
    let travel = (CANVAS_WIDTH - OVERLAY_SIZE - 128) as f32;
    let phase = (tick % OVERLAY_PERIOD) as f32 / OVERLAY_PERIOD as f32;
    if let Some(layer) = scene.layer_mut("overlay") {
        layer.transform.x = 64.0 + phase * travel;
    }
}

fn internal(error: impl std::fmt::Display) -> CommandError {
    CommandError::new(ErrorCode::Internal, format!("compositor failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_moves_and_wraps() {
        let mut config = reference_scene(None).expect("scene");
        move_overlay(&mut config.scene, 0);
        let start = config
            .scene
            .layer_mut("overlay")
            .expect("overlay")
            .transform
            .x;
        move_overlay(&mut config.scene, OVERLAY_PERIOD / 2);
        let middle = config
            .scene
            .layer_mut("overlay")
            .expect("overlay")
            .transform
            .x;
        move_overlay(&mut config.scene, OVERLAY_PERIOD);
        let wrapped = config
            .scene
            .layer_mut("overlay")
            .expect("overlay")
            .transform
            .x;
        assert_eq!(start, 64.0);
        assert!(middle > start);
        assert_eq!(wrapped, start);
    }

    #[test]
    fn scene_without_screen_has_two_layers() {
        let config = reference_scene(None).expect("scene");
        let names: Vec<_> = config
            .scene
            .layers
            .iter()
            .map(|l| l.name.as_str())
            .collect();
        assert_eq!(names, vec!["color_bars", "overlay"]);
        assert_eq!(config.scene.validate(), Ok(()));
    }
}
