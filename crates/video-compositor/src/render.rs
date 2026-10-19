//! CPU reference renderer.
//!
//! - Sampling: bilinear with 8-bit fixed-point weights, edges clamped.
//!   Pixel-aligned layers (scale 1, integer position) are copied 1:1.
//! - Blending: premultiplied alpha "over", in integer math. The output is
//!   identical for any thread count.
//! - Work is split into row bands across scoped threads for large layers;
//!   the calling thread renders one band itself. After warm-up a render does
//!   not allocate (the column table is reused), apart from spawning the
//!   `threads - 1` scoped workers per large layer. Measured cost is well
//!   inside the 60 FPS budget (ADR 0003); a persistent pool is the next
//!   step if profiling shows spawn jitter.

use std::thread;

use media_core::{PixelFormat, VideoFrame};
use thiserror::Error;

use crate::geometry::{PixelBounds, pixel_bounds, source_coordinate};
use crate::scene::{Layer, Scene, SourceId};

const BPP: usize = PixelFormat::BYTES_PER_PIXEL;
/// Layers covering fewer canvas pixels are drawn on the calling thread.
const PARALLEL_THRESHOLD: u64 = 64 * 1024;
const MAX_DEFAULT_THREADS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RenderError {
    #[error(
        "render target must be a {expected_width}x{expected_height} Bgra8 frame, \
         got {width}x{height} {format:?}"
    )]
    TargetMismatch {
        expected_width: u32,
        expected_height: u32,
        width: u32,
        height: u32,
        format: PixelFormat,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderOutcome {
    pub layers_drawn: u32,
    /// Visible layers whose source had no frame.
    pub layers_missing: u32,
}

/// Source sample position along one axis: blend `i0` and `i1`, with `w1`
/// (0..=256) the weight of `i1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tap {
    i0: u32,
    i1: u32,
    w1: u32,
}

#[derive(Debug)]
pub struct CpuRenderer {
    threads: usize,
    columns: Vec<Tap>,
}

impl CpuRenderer {
    pub const NAME: &'static str = "cpu-bilinear";

    pub fn new(threads: usize) -> Self {
        Self {
            threads: threads.max(1),
            columns: Vec::new(),
        }
    }

    /// One thread per available core, capped at 8.
    pub fn with_available_parallelism() -> Self {
        let threads = thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(MAX_DEFAULT_THREADS);
        Self::new(threads)
    }

    pub fn threads(&self) -> usize {
        self.threads
    }

    /// Draw `scene` into `target`, a `scene.width` x `scene.height` Bgra8
    /// frame. `lookup` returns the current frame of a source, if any.
    pub fn render<'f, F>(
        &mut self,
        scene: &Scene,
        lookup: F,
        target: &mut VideoFrame,
    ) -> Result<RenderOutcome, RenderError>
    where
        F: Fn(SourceId) -> Option<&'f VideoFrame>,
    {
        if target.width() != scene.width
            || target.height() != scene.height
            || target.format() != PixelFormat::Bgra8
        {
            return Err(RenderError::TargetMismatch {
                expected_width: scene.width,
                expected_height: scene.height,
                width: target.width(),
                height: target.height(),
                format: target.format(),
            });
        }
        fill_background(target, scene.background);
        let mut outcome = RenderOutcome::default();
        for layer in scene.layers.iter().filter(|layer| layer.visible) {
            match lookup(layer.source) {
                Some(source) => {
                    if self.draw_layer(layer, source, target) {
                        outcome.layers_drawn += 1;
                    }
                }
                None => outcome.layers_missing += 1,
            }
        }
        Ok(outcome)
    }

    fn draw_layer(&mut self, layer: &Layer, source: &VideoFrame, target: &mut VideoFrame) -> bool {
        let opacity = opacity_weight(layer.opacity);
        if opacity == 0 {
            return false;
        }
        let transform = layer.transform_for(source.width(), source.height());
        let rect = transform.rect(source.width(), source.height());
        let Some(bounds) = pixel_bounds(rect, target.width(), target.height()) else {
            return false;
        };
        let threads = if bounds.area() >= PARALLEL_THRESHOLD {
            self.threads
        } else {
            1
        };
        let opaque = source.format().is_opaque();

        if transform.is_pixel_aligned() {
            let (dx, dy) = (transform.x as i64, transform.y as i64);
            for_each_row(target, bounds, threads, |y, row| {
                // Bounds lie inside the source, so these are in range.
                let sy = (i64::from(y) - dy) as u32;
                let sx0 = (i64::from(bounds.x0) - dx) as usize;
                let src = &source.row(sy)[sx0 * BPP..];
                let dst = &mut row[bounds.x0 as usize * BPP..bounds.x1 as usize * BPP];
                if opaque {
                    draw_row_aligned::<true>(dst, src, opacity);
                } else {
                    draw_row_aligned::<false>(dst, src, opacity);
                }
            });
        } else {
            self.columns.clear();
            self.columns.extend((bounds.x0..bounds.x1).map(|x| {
                tap(
                    source_coordinate(x, rect.x, transform.scale_x),
                    source.width(),
                )
            }));
            let columns = &self.columns[..];
            for_each_row(target, bounds, threads, |y, row| {
                let row_tap = tap(
                    source_coordinate(y, rect.y, transform.scale_y),
                    source.height(),
                );
                let dst = &mut row[bounds.x0 as usize * BPP..bounds.x1 as usize * BPP];
                if opaque {
                    draw_row_bilinear::<true>(dst, source, columns, row_tap, opacity);
                } else {
                    draw_row_bilinear::<false>(dst, source, columns, row_tap, opacity);
                }
            });
        }
        true
    }
}

fn opacity_weight(opacity: f32) -> u32 {
    (opacity.clamp(0.0, 1.0) * 256.0).round() as u32
}

fn tap(coordinate: f32, size: u32) -> Tap {
    let last = size - 1;
    let clamped = coordinate.clamp(0.0, last as f32);
    let i0 = (clamped.floor() as u32).min(last);
    Tap {
        i0,
        i1: (i0 + 1).min(last),
        w1: ((clamped - i0 as f32) * 256.0).round() as u32,
    }
}

fn fill_background(target: &mut VideoFrame, background: [u8; 4]) {
    // The canvas is always opaque.
    let pixel = [background[0], background[1], background[2], 255];
    let stride = target.stride();
    let row_bytes = target.width() as usize * BPP;
    let rows = target.height() as usize;
    let data = target.data_mut();
    for px in data[..row_bytes].as_chunks_mut::<BPP>().0 {
        *px = pixel;
    }
    let (first, rest) = data.split_at_mut(stride.min(data.len()));
    // `take` ignores trailing bytes past the last row in oversized buffers.
    for row in rest.chunks_mut(stride).take(rows - 1) {
        row[..row_bytes].copy_from_slice(&first[..row_bytes]);
    }
}

/// Run `work(y, row)` for every canvas row in `bounds`, split into bands
/// across up to `threads` scoped threads. `row` is the full canvas row.
fn for_each_row<F>(target: &mut VideoFrame, bounds: PixelBounds, threads: usize, work: F)
where
    F: Fn(u32, &mut [u8]) + Sync,
{
    let stride = target.stride();
    let row_bytes = target.width() as usize * BPP;
    let (y0, y1) = (bounds.y0 as usize, bounds.y1 as usize);
    let data = target.data_mut();
    let end = ((y1 - 1) * stride + row_bytes).min(data.len());
    let rows = &mut data[y0 * stride..end];
    let row_count = y1 - y0;
    let run_band = |first_row: usize, band: &mut [u8]| {
        for (i, row) in band.chunks_mut(stride).enumerate() {
            work((first_row + i) as u32, &mut row[..row_bytes]);
        }
    };
    if threads <= 1 || row_count < 2 {
        run_band(y0, rows);
        return;
    }
    let band_rows = row_count.div_ceil(threads);
    let mut bands = rows.chunks_mut(band_rows * stride).enumerate();
    // The calling thread renders the first band itself: one spawn fewer.
    let own = bands.next();
    thread::scope(|scope| {
        for (band, chunk) in bands {
            let run_band = &run_band;
            scope.spawn(move || run_band(y0 + band * band_rows, chunk));
        }
        if let Some((_, chunk)) = own {
            run_band(y0, chunk);
        }
    });
}

fn draw_row_aligned<const OPAQUE: bool>(dst: &mut [u8], src: &[u8], opacity: u32) {
    if OPAQUE && opacity == 256 {
        dst.copy_from_slice(&src[..dst.len()]);
        for px in dst.as_chunks_mut::<BPP>().0 {
            px[3] = 255;
        }
        return;
    }
    for (d, s) in dst
        .as_chunks_mut::<BPP>()
        .0
        .iter_mut()
        .zip(src.as_chunks::<BPP>().0)
    {
        let alpha = if OPAQUE { 255 } else { s[3] };
        blend(d, [s[0], s[1], s[2], alpha], opacity);
    }
}

fn draw_row_bilinear<const OPAQUE: bool>(
    dst: &mut [u8],
    source: &VideoFrame,
    columns: &[Tap],
    row_tap: Tap,
    opacity: u32,
) {
    let top = source.row(row_tap.i0);
    let bottom = source.row(row_tap.i1);
    let wy1 = row_tap.w1;
    let wy0 = 256 - wy1;
    let channels = if OPAQUE { 3 } else { 4 };
    for (d, column) in dst.as_chunks_mut::<BPP>().0.iter_mut().zip(columns) {
        let a = column.i0 as usize * BPP;
        let b = column.i1 as usize * BPP;
        let wx1 = column.w1;
        let wx0 = 256 - wx1;
        let mut px = [0, 0, 0, 255];
        for c in 0..channels {
            let upper = u32::from(top[a + c]) * wx0 + u32::from(top[b + c]) * wx1;
            let lower = u32::from(bottom[a + c]) * wx0 + u32::from(bottom[b + c]) * wx1;
            // <= 255 * 256 * 256 + 2^15, so the result fits in a byte.
            px[c] = ((upper * wy0 + lower * wy1 + (1 << 15)) >> 16) as u8;
        }
        blend(d, px, opacity);
    }
}

/// Premultiplied "over": `dst = src * opacity + dst * (1 - src_alpha)`.
#[inline]
fn blend(dst: &mut [u8; 4], src: [u8; 4], opacity: u32) {
    let src = if opacity == 256 {
        src
    } else {
        src.map(|c| ((u32::from(c) * opacity + 128) >> 8) as u8)
    };
    match src[3] {
        255 => *dst = src,
        0 if src == [0; 4] => {}
        alpha => {
            let inverse = 255 - u32::from(alpha);
            for (d, s) in dst.iter_mut().zip(src) {
                *d = (u32::from(s) + div255(u32::from(*d) * inverse)).min(255) as u8;
            }
        }
    }
}

/// Exactly rounded `x / 255` for `x <= 255 * 255`.
#[inline]
fn div255(x: u32) -> u32 {
    let x = x + 128;
    (x + (x >> 8)) >> 8
}

#[cfg(test)]
mod tests {
    use media_core::FrameBuffer;

    use super::*;
    use crate::geometry::Transform;
    use crate::scene::Layer;

    fn frame(width: u32, height: u32, format: PixelFormat, pixels: &[[u8; 4]]) -> VideoFrame {
        assert_eq!(pixels.len(), (width * height) as usize);
        VideoFrame::new(
            width,
            height,
            width as usize * BPP,
            format,
            None,
            FrameBuffer::unpooled(pixels.concat()),
        )
        .expect("frame")
    }

    fn render(scene: &Scene, sources: &[(SourceId, &VideoFrame)], threads: usize) -> VideoFrame {
        let mut target =
            VideoFrame::zeroed(scene.width, scene.height, PixelFormat::Bgra8).expect("target");
        let lookup = |id| {
            sources
                .iter()
                .find(|(source, _)| *source == id)
                .map(|(_, f)| *f)
        };
        CpuRenderer::new(threads)
            .render(scene, lookup, &mut target)
            .expect("render");
        target
    }

    fn scene(width: u32, height: u32, layers: Vec<Layer>) -> Scene {
        Scene {
            width,
            height,
            background: [10, 20, 30, 255],
            layers,
        }
    }

    const RED: [u8; 4] = [0, 0, 255, 255];
    const BLUE: [u8; 4] = [255, 0, 0, 255];

    #[test]
    fn div255_is_exact() {
        for x in 0..=255 * 255 {
            assert_eq!(div255(x), (x as f64 / 255.0).round() as u32, "{x}");
        }
    }

    #[test]
    fn background_fills_the_canvas_opaquely() {
        let mut s = scene(3, 2, vec![]);
        s.background = [1, 2, 3, 0];
        let out = render(&s, &[], 1);
        for y in 0..2 {
            for x in 0..3 {
                assert_eq!(out.pixel(x, y), [1, 2, 3, 255]);
            }
        }
    }

    #[test]
    fn aligned_layer_is_copied_at_its_offset() {
        let src = frame(2, 1, PixelFormat::Bgrx8, &[[1, 2, 3, 0], [4, 5, 6, 7]]);
        let s = scene(
            4,
            2,
            vec![Layer::new("l", SourceId(1), Transform::at(1.0, 1.0))],
        );
        let out = render(&s, &[(SourceId(1), &src)], 1);
        assert_eq!(out.pixel(0, 1), [10, 20, 30, 255]);
        // Bgrx alpha byte is ignored: the layer is opaque.
        assert_eq!(out.pixel(1, 1), [1, 2, 3, 255]);
        assert_eq!(out.pixel(2, 1), [4, 5, 6, 255]);
        assert_eq!(out.pixel(3, 1), [10, 20, 30, 255]);
        assert_eq!(out.pixel(1, 0), [10, 20, 30, 255]);
    }

    #[test]
    fn half_opacity_blends_exactly() {
        let src = frame(1, 1, PixelFormat::Bgrx8, &[[200, 100, 0, 255]]);
        let mut s = scene(
            1,
            1,
            vec![Layer::new("l", SourceId(0), Transform::IDENTITY).with_opacity(0.5)],
        );
        s.background = [0, 0, 0, 255];
        let out = render(&s, &[(SourceId(0), &src)], 1);
        // src * 0.5 + black * (1 - 0.5)
        assert_eq!(out.pixel(0, 0), [100, 50, 0, 255]);
    }

    #[test]
    fn premultiplied_source_alpha_is_respected() {
        // 50 % transparent red, premultiplied.
        let src = frame(1, 1, PixelFormat::Bgra8, &[[0, 0, 128, 128]]);
        let mut s = scene(
            1,
            1,
            vec![Layer::new("l", SourceId(0), Transform::IDENTITY)],
        );
        s.background = [200, 0, 0, 255];
        let out = render(&s, &[(SourceId(0), &src)], 1);
        // 200 * (255 - 128) / 255 = 99.6 -> 100
        assert_eq!(out.pixel(0, 0), [100, 0, 128, 255]);
    }

    #[test]
    fn fully_transparent_layer_changes_nothing() {
        let src = frame(1, 1, PixelFormat::Bgrx8, &[RED]);
        let s = scene(
            1,
            1,
            vec![Layer::new("l", SourceId(0), Transform::IDENTITY).with_opacity(0.0)],
        );
        assert_eq!(
            render(&s, &[(SourceId(0), &src)], 1).pixel(0, 0),
            [10, 20, 30, 255]
        );
    }

    #[test]
    fn later_layers_cover_earlier_ones() {
        let red = frame(2, 1, PixelFormat::Bgrx8, &[RED, RED]);
        let blue = frame(1, 1, PixelFormat::Bgrx8, &[BLUE]);
        let s = scene(
            2,
            1,
            vec![
                Layer::new("red", SourceId(0), Transform::IDENTITY),
                Layer::new("blue", SourceId(1), Transform::at(1.0, 0.0)),
            ],
        );
        let out = render(&s, &[(SourceId(0), &red), (SourceId(1), &blue)], 1);
        assert_eq!(out.pixel(0, 0), RED);
        assert_eq!(out.pixel(1, 0), BLUE);
    }

    #[test]
    fn bilinear_upscale_interpolates_between_pixels() {
        // 2x1 black/white upscaled 2x horizontally: samples at -0.25, 0.25,
        // 0.75, 1.25 -> 0, 0.25, 0.75, 1 (edges clamp).
        let src = frame(
            2,
            1,
            PixelFormat::Bgrx8,
            &[[0, 0, 0, 255], [255, 255, 255, 255]],
        );
        let transform = Transform {
            scale_x: 2.0,
            ..Transform::IDENTITY
        };
        let s = scene(4, 1, vec![Layer::new("l", SourceId(0), transform)]);
        let out = render(&s, &[(SourceId(0), &src)], 1);
        let values: Vec<u8> = (0..4).map(|x| out.pixel(x, 0)[0]).collect();
        assert_eq!(values, vec![0, 64, 191, 255]);
    }

    #[test]
    fn downscale_of_uniform_source_is_uniform() {
        let src = frame(4, 4, PixelFormat::Bgrx8, &[[7, 8, 9, 0]; 16]);
        let transform = Transform {
            scale_x: 0.5,
            scale_y: 0.5,
            ..Transform::IDENTITY
        };
        let s = scene(2, 2, vec![Layer::new("l", SourceId(0), transform)]);
        let out = render(&s, &[(SourceId(0), &src)], 1);
        for (x, y) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            assert_eq!(out.pixel(x, y), [7, 8, 9, 255]);
        }
    }

    #[test]
    fn missing_and_hidden_layers_are_skipped() {
        let src = frame(1, 1, PixelFormat::Bgrx8, &[RED]);
        let mut hidden = Layer::new("hidden", SourceId(0), Transform::IDENTITY);
        hidden.visible = false;
        let s = scene(
            1,
            1,
            vec![
                hidden,
                Layer::new("missing", SourceId(9), Transform::IDENTITY),
            ],
        );
        let mut target = VideoFrame::zeroed(1, 1, PixelFormat::Bgra8).expect("target");
        let outcome = CpuRenderer::new(1)
            .render(&s, |id| (id == SourceId(0)).then_some(&src), &mut target)
            .expect("render");
        assert_eq!(
            outcome,
            RenderOutcome {
                layers_drawn: 0,
                layers_missing: 1
            }
        );
        assert_eq!(target.pixel(0, 0), [10, 20, 30, 255]);
    }

    #[test]
    fn output_is_independent_of_thread_count() {
        let pixels: Vec<[u8; 4]> = (0..(300 * 260))
            .map(|i| {
                let v = (i * 37 % 251) as u8;
                [v, v.wrapping_mul(3), v.wrapping_add(90), 255]
            })
            .collect();
        let src = frame(300, 260, PixelFormat::Bgrx8, &pixels);
        let transform = Transform {
            x: 3.3,
            y: -7.6,
            scale_x: 1.37,
            scale_y: 1.11,
        };
        let s = scene(
            400,
            300,
            vec![Layer::new("l", SourceId(0), transform).with_opacity(0.8)],
        );
        let single = render(&s, &[(SourceId(0), &src)], 1);
        let parallel = render(&s, &[(SourceId(0), &src)], 7);
        assert_eq!(single.data(), parallel.data());
    }

    #[test]
    fn fitted_layer_follows_the_actual_frame_size() {
        let area = crate::geometry::Rect {
            x: 0.0,
            y: 2.0,
            width: 8.0,
            height: 4.0,
        };
        for (w, h) in [(2, 1), (4, 2), (16, 8)] {
            let src = frame(w, h, PixelFormat::Bgrx8, &vec![RED; (w * h) as usize]);
            let s = scene(8, 8, vec![Layer::fitted("fit", SourceId(0), area)]);
            let out = render(&s, &[(SourceId(0), &src)], 1);
            assert_eq!(out.pixel(0, 2), RED, "{w}x{h}");
            assert_eq!(out.pixel(7, 5), RED, "{w}x{h}");
            assert_eq!(out.pixel(0, 1), [10, 20, 30, 255], "{w}x{h}");
            assert_eq!(out.pixel(0, 6), [10, 20, 30, 255], "{w}x{h}");
        }
    }

    #[test]
    fn oversized_target_buffer_does_not_panic() {
        let s = scene(2, 2, vec![]);
        // Two 8-byte rows plus 4 trailing bytes.
        let mut target = VideoFrame::new(
            2,
            2,
            8,
            PixelFormat::Bgra8,
            None,
            FrameBuffer::unpooled(vec![0; 20]),
        )
        .expect("target");
        CpuRenderer::new(1)
            .render(&s, |_| None, &mut target)
            .expect("render");
        assert_eq!(target.pixel(1, 1), [10, 20, 30, 255]);
    }

    #[test]
    fn wrong_target_is_rejected() {
        let s = scene(2, 2, vec![]);
        let mut target = VideoFrame::zeroed(2, 2, PixelFormat::Bgrx8).expect("target");
        let err = CpuRenderer::new(1)
            .render(&s, |_| None, &mut target)
            .unwrap_err();
        assert!(matches!(err, RenderError::TargetMismatch { .. }));
    }
}
