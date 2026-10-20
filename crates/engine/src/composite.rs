//! `probe_composite`: run the compositor on the M3 reference scene.
//!
//! Scene (1920x1080 @ 60 FPS):
//! 1. color bars, full canvas
//! 2. optional live screen capture, fitted into a 1152x648 box, 90 % opacity
//! 3. 160x160 half-transparent overlay moving left to right

use std::fs::{self, OpenOptions};
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use capture_api::{CaptureBackend, VideoStream};
use command_api::{
    CommandError, CompositeReport, CompositeRequest, ErrorCode, ErrorSummary, SnapshotResult,
};
use serde_json::{Map, Value};
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
pub(crate) const SCREEN_BOX: Rect = Rect {
    x: 704.0,
    y: 368.0,
    width: 1152.0,
    height: 648.0,
};
/// Frames for one left-to-right pass of the overlay.
const OVERLAY_PERIOD: u64 = 240;
/// How often the waiting engine thread checks whether the compositor died.
const WATCH_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) fn probe(
    capture: &mut dyn CaptureBackend,
    output_dir: &Path,
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
    // On early errors the stream is dropped, which stops capturing and
    // closes the portal session.
    let config = reference_scene(stream.as_deref()).map_err(internal)?;
    let compositor = Compositor::spawn(config).map_err(internal)?;
    let deadline = Instant::now() + request.duration;
    // Wake early if the render thread fails instead of waiting out the duration.
    while !compositor.is_finished() {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep(WATCH_INTERVAL.min(deadline - now));
    }
    let run = compositor.stop();

    // Stop capturing on every path, so its diagnostics are never lost.
    let (screen_capture, screen_capture_error) = match stream.map(|stream| stream.stop()) {
        Some(Ok(report)) => (Some(report), None),
        Some(Err(error)) => (None, Some(ErrorSummary::from(error))),
        None => (None, None),
    };
    let run = match run {
        Ok(run) => run,
        Err(error) => {
            let mut details = Map::new();
            if let Some(report) = &screen_capture {
                details.insert("screen_capture".into(), to_value(report));
            }
            if let Some(error) = &screen_capture_error {
                details.insert("screen_capture_error".into(), to_value(error));
            }
            let mut error = internal(error);
            if !details.is_empty() {
                error = error.with_details(Value::Object(details));
            }
            return Err(error);
        }
    };
    let snapshot = request
        .snapshot_path
        .as_ref()
        .map(|name| write_snapshot(&output_dir.join(name), &run.last_frame));

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

pub(crate) fn reference_scene(
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
        // Fitted per frame: the delivered size may differ from what the
        // portal reported (HiDPI scaling, window resizes).
        scene
            .layers
            .push(Layer::fitted("screen", SCREEN, SCREEN_BOX).with_opacity(0.9));
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
        sink: None,
    })
}

/// Sweep the overlay across the canvas; sub-pixel positions exercise the
/// bilinear path.
pub(crate) fn move_overlay(scene: &mut Scene, tick: u64) {
    let travel = (CANVAS_WIDTH - OVERLAY_SIZE - 128) as f32;
    let phase = (tick % OVERLAY_PERIOD) as f32 / OVERLAY_PERIOD as f32;
    if let Some(layer) = scene.layer_mut("overlay") {
        layer.transform.x = 64.0 + phase * travel;
    }
}

/// Create `path` (never overwriting) and write the frame. A partially
/// written file is removed so a retry is not blocked by it.
fn write_snapshot(path: &Path, frame: &media_core::VideoFrame) -> SnapshotResult {
    let display = std::path::absolute(path)
        .unwrap_or_else(|_| path.to_owned())
        .display()
        .to_string();
    let file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(err) => {
            return SnapshotResult {
                path: display,
                written: false,
                error: Some(err.to_string()),
            };
        }
    };
    let error = match snapshot::write_ppm(frame, &mut BufWriter::new(file)) {
        Ok(()) => None,
        Err(err) => {
            // We created this file ourselves, so removing it is safe.
            let cleanup = fs::remove_file(path)
                .err()
                .map(|e| format!("; removing the partial file failed: {e}"))
                .unwrap_or_default();
            Some(format!("{err}{cleanup}"))
        }
    };
    SnapshotResult {
        path: display,
        written: error.is_none(),
        error,
    }
}

fn to_value(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or_else(|err| Value::String(err.to_string()))
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
