//! Downscaling for previews: area averaging to any smaller size.
//!
//! Each output pixel is the average of the source pixels it covers, each
//! weighted by how much of it is covered (a box filter). Unlike bilinear
//! sampling this does not skip source pixels, so thin lines and text stay
//! even. Weights are computed once per size pair in fixed point, each
//! axis's summing exactly to one, so a flat colour stays exact and the
//! output does not depend on the platform: 14 bits horizontally, and
//! vertically 8 bits while an output row covers at most five source rows
//! (a ratio up to 4, the preview's sizes), 14 bits beyond. A weight is zero
//! only beyond 16384 source pixels per output pixel. Cost (1920×1080
//! source, release build, see
//! `examples/render_bench.rs`): about 2 ms to 1280×720 on the development
//! machine; equal sizes are a copy.

use media_core::{FrameBuffer, PixelFormat, VideoFrame, VideoFrameError};
use thiserror::Error;

const BPP: usize = PixelFormat::BYTES_PER_PIXEL;
/// The vertical weights sum to this, so a vertical sum (at most
/// `255 * ROW_ONE`) fits in a `u16`: the vertical pass, which runs over
/// whole rows of bytes, vectorizes in 16-bit lanes...
const ROW_ONE: u32 = 1 << 8;
/// ...while an output row covers at most this many source rows; with
/// more, 8-bit weights get too coarse.
const NARROW_ROWS: usize = 5;
/// Beyond that, the vertical weights sum to this, in `u32` sums (at most
/// `255 * WIDE_ROW_ONE`) cut to 16 bits by `WIDE_SHIFT`.
const WIDE_ROW_ONE: u32 = 1 << 14;
const WIDE_SHIFT: u32 = 6;
/// The horizontal weights sum to this; a horizontal sum (at most
/// `65280 * COLUMN_ONE`) fits in a `u32`.
const COLUMN_ONE: u32 = 1 << 14;
/// From the horizontal sum back to a byte.
const OUT_SHIFT: u32 = 8 + 14;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ScaleError {
    #[error(
        "cannot downscale {src_width}x{src_height} to {width}x{height}: the target must be \
         at least 1x1 and no larger than the source"
    )]
    Size {
        src_width: u32,
        src_height: u32,
        width: u32,
        height: u32,
    },
    #[error(
        "the downscaler expects a {expected_width}x{expected_height} frame, got {width}x{height}"
    )]
    Source {
        expected_width: u32,
        expected_height: u32,
        width: u32,
        height: u32,
    },
    #[error(transparent)]
    Frame(#[from] VideoFrameError),
}

/// Scales frames of one size down to another by area averaging. Building
/// one allocates its weight tables; scaling does not allocate.
#[derive(Debug)]
pub struct Downscaler {
    src_width: u32,
    src_height: u32,
    width: u32,
    height: u32,
    columns: Axis,
    rows: Axis,
    /// One source-wide row of vertical sums, four channels per pixel.
    columns_sum: Vec<u16>,
    /// The same in `u32` with 14-bit row weights, for ratios beyond
    /// `NARROW_ROWS`; empty otherwise.
    wide_sums: Vec<u32>,
}

impl Downscaler {
    /// A downscaler from `src_width`×`src_height` to `width`×`height`; each
    /// side of the target is between 1 and the source's.
    pub fn new(
        src_width: u32,
        src_height: u32,
        width: u32,
        height: u32,
    ) -> Result<Self, ScaleError> {
        if width == 0 || height == 0 || width > src_width || height > src_height {
            return Err(ScaleError::Size {
                src_width,
                src_height,
                width,
                height,
            });
        }
        let wide = Axis::taps(src_height, height) > NARROW_ROWS;
        let row_one = if wide { WIDE_ROW_ONE } else { ROW_ONE };
        let row_len = src_width as usize * BPP;
        Ok(Self {
            src_width,
            src_height,
            width,
            height,
            columns: Axis::new(src_width, width, COLUMN_ONE),
            rows: Axis::new(src_height, height, row_one),
            columns_sum: vec![0; row_len],
            wide_sums: if wide { vec![0; row_len] } else { Vec::new() },
        })
    }

    /// The output size.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Bytes of one tightly packed output frame.
    pub fn frame_len(&self) -> usize {
        self.width as usize * self.height as usize * BPP
    }

    /// Scale `src` into `buffer` (at least [`frame_len`](Self::frame_len)
    /// bytes). The result keeps the source's format and timestamp and is
    /// tightly packed.
    pub fn scale(
        &mut self,
        src: &VideoFrame,
        buffer: FrameBuffer,
    ) -> Result<VideoFrame, ScaleError> {
        if (src.width(), src.height()) != (self.src_width, self.src_height) {
            return Err(ScaleError::Source {
                expected_width: self.src_width,
                expected_height: self.src_height,
                width: src.width(),
                height: src.height(),
            });
        }
        let stride = VideoFrame::packed_stride(self.width)?;
        let mut frame = VideoFrame::new(
            self.width,
            self.height,
            stride,
            src.format(),
            src.pts(),
            buffer,
        )?;
        let row_bytes = self.width as usize * BPP;
        let same_size = (self.width, self.height) == (self.src_width, self.src_height);
        for y in 0..self.height {
            let out = &mut frame.data_mut()[y as usize * stride..][..row_bytes];
            if same_size {
                out.copy_from_slice(src.row(y));
                continue;
            }
            // Vertical first: it runs over whole rows of bytes, which
            // vectorizes; the horizontal pass then has one row to reduce.
            let (first, weights) = self.rows.span(y as usize);
            let sums = &mut self.columns_sum;
            let rows = weights
                .iter()
                .enumerate()
                .filter(|(_, wy)| **wy > 0)
                .map(|(k, &wy)| (src.row(first + k as u32), wy));
            if self.wide_sums.is_empty() {
                sums.fill(0);
                for (row, wy) in rows {
                    // Weights sum to ROW_ONE, so the sum stays within a u16.
                    let wy = wy as u16;
                    for (sum, &value) in sums.iter_mut().zip(row) {
                        *sum += wy * u16::from(value);
                    }
                }
            } else {
                let wide = &mut self.wide_sums;
                wide.fill(0);
                for (row, wy) in rows {
                    for (sum, &value) in wide.iter_mut().zip(row) {
                        *sum += wy * u32::from(value);
                    }
                }
                for (sum, &wide) in sums.iter_mut().zip(wide.iter()) {
                    // <= 65280.5, so it fits in a u16.
                    *sum = ((wide + (1 << (WIDE_SHIFT - 1))) >> WIDE_SHIFT) as u16;
                }
            }
            reduce_row(out, &self.columns, sums);
        }
        Ok(frame)
    }
}

/// Reduce one row of vertical sums to the output columns. Spans of up to
/// five pixels (ratios up to 4) get an unrolled loop.
fn reduce_row(out: &mut [u8], columns: &Axis, sums: &[u16]) {
    match columns.taps {
        1 => reduce_row_with::<1>(out, columns, sums),
        2 => reduce_row_with::<2>(out, columns, sums),
        3 => reduce_row_with::<3>(out, columns, sums),
        4 => reduce_row_with::<4>(out, columns, sums),
        5 => reduce_row_with::<5>(out, columns, sums),
        _ => reduce_row_with::<0>(out, columns, sums),
    }
}

/// [`reduce_row`] with `TAPS` weights per pixel, or any number for 0.
fn reduce_row_with<const TAPS: usize>(out: &mut [u8], columns: &Axis, sums: &[u16]) {
    // A constant length lets the compiler unroll the inner loop.
    let taps = if TAPS == 0 { columns.taps } else { TAPS };
    let sums = sums.as_chunks::<BPP>().0;
    for (x, out) in out.as_chunks_mut::<BPP>().0.iter_mut().enumerate() {
        let (first, weights) = columns.span(x);
        let mut total = [0u32; BPP];
        for (sum, &wx) in sums[first as usize..][..taps].iter().zip(&weights[..taps]) {
            for (total, &value) in total.iter_mut().zip(sum) {
                *total += wx * u32::from(value);
            }
        }
        for (out, total) in out.iter_mut().zip(total) {
            // <= 255.5 * 2^OUT_SHIFT, so the result fits in a byte.
            *out = ((total + (1 << (OUT_SHIFT - 1))) >> OUT_SHIFT) as u8;
        }
    }
}

/// The source pixels each output pixel covers along one axis, with their
/// weights. Every pixel has `taps` weights (the longest span), padded with
/// zeros, so the inner loops have a fixed length.
#[derive(Debug)]
struct Axis {
    taps: usize,
    /// Per output pixel: the first of its `taps` source pixels.
    firsts: Vec<u32>,
    /// `taps` per output pixel.
    weights: Vec<u32>,
}

impl Axis {
    /// Output pixel `i` covers source `[i * src / dst, (i + 1) * src / dst)`;
    /// in units of `1 / dst` source pixel, that is `[i * src, (i + 1) * src)`
    /// and source pixel `j` is `[j * dst, (j + 1) * dst)`, so the overlaps
    /// are exact integers summing to `src`. Each weight is the rounded
    /// running total of the overlaps minus the one before it, so the
    /// weights sum exactly to `one` and none is negative, at any ratio
    /// (with more source pixels than `one`, some get no weight).
    fn new(src: u32, dst: u32, one: u32) -> Self {
        let taps = Self::taps(src, dst);
        let (src, dst) = (u64::from(src), u64::from(dst));
        let mut firsts = Vec::with_capacity(dst as usize);
        let mut weights = vec![0; dst as usize * taps];
        for (i, padded) in (0..dst).zip(weights.chunks_exact_mut(taps)) {
            let (lo, hi, first, last) = span(src, dst, i);
            // Near the end, the padding goes in front.
            let start = first.min(src - taps as u64);
            let skip = (first - start) as usize;
            let rounded = |covered: u64| (covered * u64::from(one) + src / 2) / src;
            let mut covered = 0;
            for (j, weight) in (first..=last).zip(&mut padded[skip..]) {
                let overlap = hi.min((j + 1) * dst) - lo.max(j * dst);
                *weight = (rounded(covered + overlap) - rounded(covered)) as u32;
                covered += overlap;
            }
            firsts.push(start as u32);
        }
        Self {
            taps,
            firsts,
            weights,
        }
    }

    /// The most source pixels one output pixel covers.
    fn taps(src: u32, dst: u32) -> usize {
        let (src, dst) = (u64::from(src), u64::from(dst));
        (0..dst)
            .map(|i| {
                let (_, _, first, last) = span(src, dst, i);
                (last - first + 1) as usize
            })
            .max()
            .unwrap_or(1)
    }

    fn span(&self, i: usize) -> (u32, &[u32]) {
        (self.firsts[i], &self.weights[i * self.taps..][..self.taps])
    }
}

/// Output pixel `i`'s cover in units of `1 / dst` source pixel, and its
/// first and last source pixel.
fn span(src: u64, dst: u64, i: u64) -> (u64, u64, u64, u64) {
    let (lo, hi) = (i * src, (i + 1) * src);
    (lo, hi, lo / dst, (hi - 1) / dst)
}

#[cfg(test)]
mod tests {
    use media_core::PixelFormat;

    use super::*;
    use crate::pattern;

    fn scale(src: &VideoFrame, width: u32, height: u32) -> VideoFrame {
        let mut scaler = Downscaler::new(src.width(), src.height(), width, height).expect("scaler");
        let buffer = FrameBuffer::unpooled(vec![0; scaler.frame_len()]);
        scaler.scale(src, buffer).expect("scale")
    }

    /// A one-row frame of grey values.
    fn greys(values: &[u8]) -> VideoFrame {
        let mut frame =
            VideoFrame::zeroed(values.len() as u32, 1, PixelFormat::Bgra8).expect("frame");
        for (pixel, &value) in frame
            .data_mut()
            .as_chunks_mut::<BPP>()
            .0
            .iter_mut()
            .zip(values)
        {
            *pixel = [value, value, value, 255];
        }
        frame
    }

    #[test]
    fn weights_sum_to_one_for_every_ratio() {
        for src in [1, 2, 3, 7, 270, 1080, 1920, 3840] {
            for dst in 1..=src.min(400) {
                for one in [ROW_ONE, COLUMN_ONE] {
                    let axis = Axis::new(src, dst, one);
                    for i in 0..dst as usize {
                        let (first, weights) = axis.span(i);
                        assert_eq!(
                            weights.iter().sum::<u32>(),
                            one,
                            "{src} -> {dst}, pixel {i}"
                        );
                        assert!(
                            first + weights.len() as u32 <= src,
                            "{src} -> {dst}, pixel {i}"
                        );
                    }
                }
            }
        }
    }

    /// The exact box average of `src` at `width`×`height`, in floating
    /// point: an independent reference.
    fn reference(src: &VideoFrame, width: u32, height: u32) -> Vec<[f64; 4]> {
        // How much of source pixel `j` the output pixel `i` covers, of `n`
        // source pixels shown on `m`.
        let cover = |i: u32, j: u32, n: u32, m: u32| {
            let ratio = f64::from(n) / f64::from(m);
            let (lo, hi) = (f64::from(i) * ratio, f64::from(i + 1) * ratio);
            (hi.min(f64::from(j + 1)) - lo.max(f64::from(j))).max(0.0) / ratio
        };
        let mut out = vec![[0.0; 4]; (width * height) as usize];
        for (index, pixel) in out.iter_mut().enumerate() {
            let (x, y) = (index as u32 % width, index as u32 / width);
            for sy in 0..src.height() {
                let wy = cover(y, sy, src.height(), height);
                if wy == 0.0 {
                    continue;
                }
                for sx in 0..src.width() {
                    let w = wy * cover(x, sx, src.width(), width);
                    for (value, &byte) in pixel.iter_mut().zip(&src.pixel(sx, sy)) {
                        *value += w * f64::from(byte);
                    }
                }
            }
        }
        out
    }

    /// Bytes that vary from pixel to pixel and channel to channel.
    fn noise(width: u32, height: u32) -> VideoFrame {
        let mut frame = VideoFrame::zeroed(width, height, PixelFormat::Bgra8).expect("frame");
        let mut state = 0x2545_f491_u32;
        for byte in frame.data_mut() {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *byte = (state >> 24) as u8;
        }
        frame
    }

    #[test]
    fn matches_the_exact_area_average_at_any_ratio() {
        let src = noise(97, 61);
        for (width, height) in [
            (96, 60),
            (65, 41),
            (50, 31),
            (24, 15),
            (13, 7),
            (5, 2),
            (1, 1),
            (97, 1),
            (1, 61),
        ] {
            let small = scale(&src, width, height);
            let exact = reference(&src, width, height);
            for (index, expected) in exact.iter().enumerate() {
                let (x, y) = (index as u32 % width, index as u32 / width);
                for (&got, &want) in small.pixel(x, y).iter().zip(expected) {
                    assert!(
                        (f64::from(got) - want).abs() <= 1.5,
                        "{width}x{height} at ({x}, {y}): {got} vs {want:.2}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_tall_source_on_one_row_averages_every_row() {
        // Two of every three rows white: 1080 rows into one average 170.
        let mut src = VideoFrame::zeroed(2, 1080, PixelFormat::Bgra8).expect("frame");
        for y in 0..1080 {
            let value = if y % 3 == 0 { 0 } else { 255 };
            let at = y * src.stride();
            src.data_mut()[at..at + 8].fill(value);
        }
        let small = scale(&src, 2, 1);
        assert_eq!(small.pixel(0, 0), [170, 170, 170, 170]);
        // Just past the narrow path: six rows per row.
        let small = scale(&src, 2, 180);
        for y in 0..180 {
            assert_eq!(small.pixel(1, y), [170, 170, 170, 170], "row {y}");
        }
    }

    #[test]
    fn keeps_flat_colours_at_any_size() {
        let src = pattern::solid(1920, 1080, [10, 20, 30, 255]).expect("solid");
        for (width, height) in [(1280, 720), (1111, 625), (480, 270), (7, 3), (1, 1)] {
            let small = scale(&src, width, height);
            assert_eq!((small.width(), small.height()), (width, height));
            assert_eq!(small.format(), PixelFormat::Bgra8);
            assert_eq!(small.stride(), width as usize * BPP);
            for (x, y) in [(0, 0), (width - 1, height - 1), (width / 2, height / 2)] {
                assert_eq!(small.pixel(x, y), [10, 20, 30, 255], "{width}x{height}");
            }
        }
    }

    #[test]
    fn the_same_size_is_a_copy() {
        let src = pattern::color_bars(64, 36).expect("bars");
        let copy = scale(&src, 64, 36);
        for y in 0..36 {
            assert_eq!(copy.row(y), src.row(y));
        }
    }

    #[test]
    fn an_integer_ratio_averages_each_block() {
        let small = scale(&greys(&[0, 100, 200, 40]), 2, 1);
        assert_eq!(small.pixel(0, 0), [50, 50, 50, 255]);
        assert_eq!(small.pixel(1, 0), [120, 120, 120, 255]);
    }

    #[test]
    fn a_fractional_ratio_weights_partly_covered_pixels() {
        // Three to two: each output pixel covers one and a half source
        // pixels, the middle one half each.
        let small = scale(&greys(&[0, 255, 0]), 2, 1);
        assert_eq!(small.pixel(0, 0), [85, 85, 85, 255]);
        assert_eq!(small.pixel(1, 0), [85, 85, 85, 255]);
        // Vertically the same.
        let mut column = VideoFrame::zeroed(1, 3, PixelFormat::Bgra8).expect("frame");
        column.data_mut()[4..8].copy_from_slice(&[255, 255, 255, 255]);
        let small = scale(&column, 1, 2);
        assert_eq!(small.pixel(0, 0)[0], 85);
        assert_eq!(small.pixel(0, 1)[0], 85);
    }

    #[test]
    fn a_thin_line_is_never_skipped() {
        // A one-pixel line at any position survives a 1.5x downscale.
        for at in 0..30 {
            let mut values = vec![0; 30];
            values[at] = 255;
            let small = scale(&greys(&values), 20, 1);
            let brightest = (0..20).map(|x| small.pixel(x, 0)[0]).max();
            assert!(brightest >= Some(85), "line at {at}: {brightest:?}");
        }
    }

    #[test]
    fn keeps_the_format_and_timestamp() {
        let mut src = VideoFrame::zeroed(8, 8, PixelFormat::Bgrx8).expect("frame");
        let pts = media_core::MediaTimestamp {
            ticks: 7,
            time_base_num: 1,
            time_base_den: 30,
        };
        src.set_pts(Some(pts));
        let small = scale(&src, 3, 3);
        assert_eq!(small.format(), PixelFormat::Bgrx8);
        assert_eq!(small.pts(), Some(pts));
    }

    #[test]
    fn rejects_bad_sizes_sources_and_buffers() {
        for (width, height) in [(0, 10), (10, 0), (65, 10), (10, 49)] {
            assert!(matches!(
                Downscaler::new(64, 48, width, height),
                Err(ScaleError::Size { .. })
            ));
        }
        let mut scaler = Downscaler::new(64, 48, 16, 12).expect("scaler");
        let other = pattern::solid(32, 48, [0, 0, 0, 255]).expect("solid");
        assert!(matches!(
            scaler.scale(&other, FrameBuffer::unpooled(vec![0; scaler.frame_len()])),
            Err(ScaleError::Source { .. })
        ));
        let src = pattern::solid(64, 48, [0, 0, 0, 255]).expect("solid");
        assert!(matches!(
            scaler.scale(&src, FrameBuffer::unpooled(vec![0; 10])),
            Err(ScaleError::Frame(VideoFrameError::BufferTooSmall { .. }))
        ));
    }
}
