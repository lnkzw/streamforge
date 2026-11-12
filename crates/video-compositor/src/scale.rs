//! Downscaling for previews: area averaging to any smaller size.
//!
//! Each output pixel is the average of the source pixels it covers, each
//! weighted by how much of it is covered (a box filter). Unlike bilinear
//! sampling this does not skip source pixels, so thin lines and text stay
//! even at any ratio. Weights are computed once per size pair in fixed
//! point (8 bits vertically, 14 horizontally, each axis's summing exactly
//! to one), so a flat colour stays exact and the output does not depend on
//! the platform. Cost (1920×1080 source, release build, see
//! `examples/render_bench.rs`): about 2 ms to 1280×720 on the development
//! machine; equal sizes are a copy.

use media_core::{FrameBuffer, PixelFormat, VideoFrame, VideoFrameError};
use thiserror::Error;

const BPP: usize = PixelFormat::BYTES_PER_PIXEL;
/// The vertical weights sum to this, so a vertical sum (at most
/// `255 * ROW_ONE`) fits in a `u16`: the vertical pass, which runs over
/// whole rows of bytes, vectorizes in 16-bit lanes.
const ROW_ONE: u32 = 1 << 8;
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
        Ok(Self {
            src_width,
            src_height,
            width,
            height,
            columns: Axis::new(src_width, width, COLUMN_ONE),
            rows: Axis::new(src_height, height, ROW_ONE),
            columns_sum: vec![0; src_width as usize * BPP],
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
            sums.fill(0);
            for (k, &wy) in weights.iter().enumerate() {
                if wy == 0 {
                    continue;
                }
                // Weights sum to ROW_ONE, so the sum stays within a u16.
                let wy = wy as u16;
                for (sum, &value) in sums.iter_mut().zip(src.row(first + k as u32)) {
                    *sum += wy * u16::from(value);
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
        let (src, dst) = (u64::from(src), u64::from(dst));
        let span = |i: u64| {
            let (lo, hi) = (i * src, (i + 1) * src);
            (lo, hi, lo / dst, (hi - 1) / dst)
        };
        let taps = (0..dst)
            .map(|i| {
                let (_, _, first, last) = span(i);
                (last - first + 1) as usize
            })
            .max()
            .unwrap_or(1);
        let mut firsts = Vec::with_capacity(dst as usize);
        let mut weights = vec![0; dst as usize * taps];
        for (i, padded) in (0..dst).zip(weights.chunks_exact_mut(taps)) {
            let (lo, hi, first, last) = span(i);
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

    fn span(&self, i: usize) -> (u32, &[u32]) {
        (self.firsts[i], &self.weights[i * self.taps..][..self.taps])
    }
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
